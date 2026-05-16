//! FFOIE prototype — Quake/Defrag-style movement on wgpu.
//!
//! This build adds:
//!   • Quake-style strafe-jump physics (PM_Accelerate / PM_AirAccelerate / friction).
//!     Hold strafe + look perpendicular to velocity while air-borne = gain speed.
//!   • Proper egui UI overlay: top-left FPS readout + centered pause menu.
//!   • Floor culling fix (was facing the wrong way — invisible from above).

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
// `web_time` re-exports `std::time` on native and provides browser-backed
// (`performance.now()`-based) implementations on wasm32 — where the real
// `std::time::Instant` panics.
use std::time::Duration;
use web_time::Instant;

use bytemuck::{Pod, Zeroable};
use glam::{EulerRot, Mat4, Quat, Vec3};
use wgpu::util::DeviceExt;
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    keyboard::{Key, KeyCode, NamedKey, PhysicalKey},
    window::{CursorGrabMode, Window, WindowId},
};

// ───────────────────────────── tunables ─────────────────────────────

const TICK_RATE_HZ: f32 = 120.0;
const TICK_DT: f32 = 1.0 / TICK_RATE_HZ;
const MAX_TICKS_PER_FRAME: u32 = 8;

const MOUSE_SENSITIVITY: f32 = 0.0022;

// ── Quake-style movement physics ─────────────────────────────────────
// Constants are in SI (meters / seconds), converted from Quake's 32-units-per-meter
// defaults. Values are tuned to feel close to VQ3 (Vanilla Quake 3) defaults:
//   sv_friction = 6, sv_accelerate = 10, sv_airaccelerate = 1,
//   sv_maxspeed = 320 u/s ≈ 10 m/s, sv_gravity = 800 u/s² ≈ 25 m/s².
//
// To get Defrag/CPM "easy strafe" feel, bump AIR_ACCEL to 5+ and add an
// air-control term — that's a later iteration.
const GROUND_ACCEL: f32 = 10.0;
const AIR_ACCEL: f32 = 1.0;     // VQ3 = 1.0; CPM = 2.0+; arcade = 5–10
const MAX_SPEED: f32 = 10.0;    // ~320 u/s
const FRICTION: f32 = 6.0;
const STOP_SPEED: f32 = 3.0;    // ~100 u/s
const JUMP_VELOCITY: f32 = 8.5; // ~270 u/s → ~1.45 m apex
const GRAVITY: f32 = -25.0;     // 800 u/s²
const EYE_HEIGHT: f32 = 1.7;
const SPRINT_MULT: f32 = 1.8;

// Player AABB: feet at pos, extends up by PLAYER_HEIGHT. Half-width keeps
// the player narrow enough to fit between closely-placed blocks but wide
// enough to stand stably on a 1×1 platform top.
const PLAYER_HALF_X: f32 = 0.3;
const PLAYER_HALF_Z: f32 = 0.3;
const PLAYER_HEIGHT: f32 = 1.7;
/// Small offset added after a collision snap so we don't sit *exactly* on
/// the surface (float rounding can re-trigger the overlap next tick).
const COLLISION_EPS: f32 = 0.001;
/// How far below the feet we look for ground each tick. Bigger = more
/// forgiving "on ground" detection for slope changes / float drift.
const GROUND_PROBE: f32 = 0.05;

const FOV_DEG: f32 = 90.0;
const NEAR_PLANE: f32 = 0.05;
const FAR_PLANE: f32 = 500.0;
const PITCH_LIMIT: f32 = 89.0_f32 * std::f32::consts::PI / 180.0;

const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

static START_TIME: OnceLock<Instant> = OnceLock::new();

// ───────────────────────────── adapter info formatting ─────────────────────────────

fn fmt_backend(b: wgpu::Backend) -> &'static str {
    match b {
        wgpu::Backend::Vulkan => "Vulkan",
        wgpu::Backend::Metal => "Metal",
        wgpu::Backend::Dx12 => "DirectX 12",
        wgpu::Backend::Gl => "OpenGL",
        wgpu::Backend::BrowserWebGpu => "WebGPU",
        _ => "Unknown",
    }
}

fn fmt_device_type(t: wgpu::DeviceType) -> &'static str {
    match t {
        wgpu::DeviceType::IntegratedGpu => "Integrated GPU",
        wgpu::DeviceType::DiscreteGpu => "Discrete GPU",
        wgpu::DeviceType::VirtualGpu => "Virtual GPU",
        wgpu::DeviceType::Cpu => "CPU",
        wgpu::DeviceType::Other => "Other",
    }
}

/// Speed-bar colour: lerps green → yellow → red as speed climbs.
/// 0 m/s = green (slow / stopped), ~15 m/s = yellow (running/sprinting),
/// 25+ m/s = red (strafe-jumping into Defrag territory).
fn speed_color(speed: f32) -> egui::Color32 {
    let t = (speed / 25.0).clamp(0.0, 1.0);
    let (r, g, b) = if t < 0.5 {
        let u = t * 2.0;
        (
            60.0 + (220.0 - 60.0) * u,
            220.0 + (200.0 - 220.0) * u,
            100.0 + (60.0 - 100.0) * u,
        )
    } else {
        let u = (t - 0.5) * 2.0;
        (
            220.0 + (240.0 - 220.0) * u,
            200.0 + (90.0 - 200.0) * u,
            60.0 + (60.0 - 60.0) * u,
        )
    };
    egui::Color32::from_rgb(r as u8, g as u8, b as u8)
}

fn fmt_present_mode(p: wgpu::PresentMode) -> &'static str {
    match p {
        wgpu::PresentMode::Immediate => "Immediate (no vsync)",
        wgpu::PresentMode::Mailbox => "Mailbox (display-rate)",
        wgpu::PresentMode::Fifo => "Fifo (vsync)",
        wgpu::PresentMode::FifoRelaxed => "Fifo Relaxed",
        wgpu::PresentMode::AutoNoVsync => "Auto (no vsync)",
        wgpu::PresentMode::AutoVsync => "Auto (vsync)",
    }
}

// ───────────────────────────── GPU data ─────────────────────────────

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Instance {
    pos: [f32; 3],
    scale: [f32; 3],
    color: [f32; 3],
}

/// One collidable axis-aligned block in the world. Used for both rendering
/// (becomes an `Instance`) and physics (becomes an AABB).
#[derive(Copy, Clone)]
struct Block {
    center: Vec3,
    half_size: Vec3,
    color: [f32; 3],
}

impl Block {
    fn min(&self) -> Vec3 { self.center - self.half_size }
    fn max(&self) -> Vec3 { self.center + self.half_size }

    fn to_instance(&self) -> Instance {
        Instance {
            pos: self.center.to_array(),
            scale: (self.half_size * 2.0).to_array(),
            color: self.color,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct FloorVertex {
    pos: [f32; 3],
}

/// Vertex format for textured meshes (glTF). Adds UVs over plain `Vertex`.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct TexturedVertex {
    pos: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
}

/// What `load_glb` returns. Geometry + (optionally) the raw PNG/JPEG bytes
/// of the first material's baseColor texture.
struct GlbAsset {
    vertices: Vec<TexturedVertex>,
    indices: Vec<u32>,
    base_color_image: Option<Vec<u8>>,
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct SkyUniforms {
    proj_inv: [[f32; 4]; 4],
    view: [[f32; 4]; 4],
}

// ───────────────────────────── cube geometry ─────────────────────────────

const CUBE_VERTS: &[Vertex] = &[
    // +X
    Vertex { pos: [ 0.5, -0.5, -0.5], normal: [ 1.0,  0.0,  0.0] },
    Vertex { pos: [ 0.5,  0.5, -0.5], normal: [ 1.0,  0.0,  0.0] },
    Vertex { pos: [ 0.5,  0.5,  0.5], normal: [ 1.0,  0.0,  0.0] },
    Vertex { pos: [ 0.5, -0.5,  0.5], normal: [ 1.0,  0.0,  0.0] },
    // -X
    Vertex { pos: [-0.5, -0.5,  0.5], normal: [-1.0,  0.0,  0.0] },
    Vertex { pos: [-0.5,  0.5,  0.5], normal: [-1.0,  0.0,  0.0] },
    Vertex { pos: [-0.5,  0.5, -0.5], normal: [-1.0,  0.0,  0.0] },
    Vertex { pos: [-0.5, -0.5, -0.5], normal: [-1.0,  0.0,  0.0] },
    // +Y
    Vertex { pos: [-0.5,  0.5, -0.5], normal: [ 0.0,  1.0,  0.0] },
    Vertex { pos: [-0.5,  0.5,  0.5], normal: [ 0.0,  1.0,  0.0] },
    Vertex { pos: [ 0.5,  0.5,  0.5], normal: [ 0.0,  1.0,  0.0] },
    Vertex { pos: [ 0.5,  0.5, -0.5], normal: [ 0.0,  1.0,  0.0] },
    // -Y
    Vertex { pos: [-0.5, -0.5,  0.5], normal: [ 0.0, -1.0,  0.0] },
    Vertex { pos: [-0.5, -0.5, -0.5], normal: [ 0.0, -1.0,  0.0] },
    Vertex { pos: [ 0.5, -0.5, -0.5], normal: [ 0.0, -1.0,  0.0] },
    Vertex { pos: [ 0.5, -0.5,  0.5], normal: [ 0.0, -1.0,  0.0] },
    // +Z
    Vertex { pos: [ 0.5, -0.5,  0.5], normal: [ 0.0,  0.0,  1.0] },
    Vertex { pos: [ 0.5,  0.5,  0.5], normal: [ 0.0,  0.0,  1.0] },
    Vertex { pos: [-0.5,  0.5,  0.5], normal: [ 0.0,  0.0,  1.0] },
    Vertex { pos: [-0.5, -0.5,  0.5], normal: [ 0.0,  0.0,  1.0] },
    // -Z
    Vertex { pos: [-0.5, -0.5, -0.5], normal: [ 0.0,  0.0, -1.0] },
    Vertex { pos: [-0.5,  0.5, -0.5], normal: [ 0.0,  0.0, -1.0] },
    Vertex { pos: [ 0.5,  0.5, -0.5], normal: [ 0.0,  0.0, -1.0] },
    Vertex { pos: [ 0.5, -0.5, -0.5], normal: [ 0.0,  0.0, -1.0] },
];

#[rustfmt::skip]
const CUBE_INDICES: &[u16] = &[
    0, 1, 2,  0, 2, 3,
    4, 5, 6,  4, 6, 7,
    8, 9,10,  8,10,11,
   12,13,14, 12,14,15,
   16,17,18, 16,18,19,
   20,21,22, 20,22,23,
];

/// Strafe-jump course: a hand-arranged set of collidable blocks the player
/// can run on, jump on, and use to build speed.
///
/// Layout (top-down, X→ forward, Z↓):
///                            tall finish ▓▓▓
///         side    side                ▓▓
///   start ── ─── ─── ─── ─── ─── ─── ─── ── ─── (long jumps growing)
///         side    side                ▓▓
///                            stairs going up at +Z
fn build_course() -> Vec<Block> {
    let mut b = Vec::new();
    let easy   = [0.35, 0.55, 0.85]; // blue
    let mid    = [0.80, 0.70, 0.25]; // amber
    let hard   = [0.85, 0.30, 0.30]; // red
    let side   = [0.40, 0.75, 0.45]; // green
    let finish = [0.85, 0.55, 0.90]; // violet
    let stone  = [0.55, 0.55, 0.62]; // gray

    let mk = |cx: f32, h: f32, cz: f32, hx: f32, hz: f32, color: [f32; 3]| Block {
        center: Vec3::new(cx, h * 0.5, cz),
        half_size: Vec3::new(hx, h * 0.5, hz),
        color,
    };

    // ── Main line: low platforms, gaps growing for speed-building ──
    b.push(mk( 7.0,  0.6,  0.0,  1.6, 1.6, easy));
    b.push(mk(13.0,  0.6,  0.0,  1.4, 1.4, easy));
    b.push(mk(20.0,  0.6,  0.0,  1.4, 1.4, easy));
    b.push(mk(28.0,  0.6,  0.0,  1.4, 1.4, mid));
    b.push(mk(37.0,  0.6,  0.0,  1.4, 1.4, mid));
    b.push(mk(47.0,  0.6,  0.0,  1.4, 1.4, mid));
    b.push(mk(58.0,  0.6,  0.0,  1.6, 1.6, hard));

    // ── Finish platform: big, raised, rewards a long strafe approach ──
    b.push(mk(72.0,  1.2,  0.0,  3.5, 3.5, finish));

    // ── Side strafe targets (off-axis pads) ──
    b.push(mk(15.0,  0.5,  6.5,  1.0, 1.0, side));
    b.push(mk(25.0,  0.5, -6.5,  1.0, 1.0, side));
    b.push(mk(35.0,  0.5,  6.5,  1.0, 1.0, side));
    b.push(mk(45.0,  0.5, -6.5,  1.0, 1.0, side));

    // ── Stair-step climb (back-right of spawn) ──
    for i in 0..6 {
        let fi = i as f32;
        b.push(mk(
            -4.0 - fi * 2.4,
            0.5 + fi * 0.4,
            -8.0,
            1.1, 1.1, stone,
        ));
    }

    // ── A few orientation markers near spawn ──
    b.push(mk(-6.0, 0.4,  3.5,  0.5, 0.5, stone));
    b.push(mk(-6.0, 0.4, -3.5,  0.5, 0.5, stone));

    b
}

// ───────────────────────────── floor geometry ─────────────────────────────

const FLOOR_HALF: f32 = 120.0;

#[rustfmt::skip]
const FLOOR_VERTS: &[FloorVertex] = &[
    FloorVertex { pos: [-FLOOR_HALF, 0.0, -FLOOR_HALF] },
    FloorVertex { pos: [ FLOOR_HALF, 0.0, -FLOOR_HALF] },
    FloorVertex { pos: [ FLOOR_HALF, 0.0,  FLOOR_HALF] },
    FloorVertex { pos: [-FLOOR_HALF, 0.0, -FLOOR_HALF] },
    FloorVertex { pos: [ FLOOR_HALF, 0.0,  FLOOR_HALF] },
    FloorVertex { pos: [-FLOOR_HALF, 0.0,  FLOOR_HALF] },
];

// ───────────────────────────── camera ─────────────────────────────

struct Camera {
    position: Vec3,
    yaw: f32,
    pitch: f32,
    aspect: f32,
}

impl Camera {
    fn new(aspect: f32) -> Self {
        Self {
            position: Vec3::new(0.0, EYE_HEIGHT, 25.0),
            yaw: 0.0,
            pitch: -0.05,
            aspect,
        }
    }

    fn rotation(&self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0)
    }

    fn forward(&self) -> Vec3 {
        self.rotation() * Vec3::NEG_Z
    }

    fn proj(&self) -> Mat4 {
        Mat4::perspective_rh(FOV_DEG.to_radians(), self.aspect, NEAR_PLANE, FAR_PLANE)
    }

    fn view(&self) -> Mat4 {
        Mat4::look_to_rh(self.position, self.forward(), Vec3::Y)
    }
}

// ───────────────────────────── player & physics ─────────────────────────────

struct Player {
    pos: Vec3, // feet position
    vel: Vec3,
    on_ground: bool,
}

impl Player {
    fn new() -> Self {
        Self {
            pos: Vec3::new(0.0, 0.0, 25.0),
            vel: Vec3::ZERO,
            on_ground: true,
        }
    }
}

/// Quake's PM_Accelerate.
///
/// Adds velocity toward `wish_dir`, but only up to `wish_speed`. The current
/// component of velocity along `wish_dir` is subtracted from the budget — so
/// once you're moving at full speed in that direction, no more is added.
///
/// The strafe-jump trick falls out of this: when `wish_dir` is perpendicular
/// to your current velocity, `dot(vel, wish_dir) ≈ 0`, so the whole budget
/// gets added — increasing your total speed even though the *forward* speed
/// is capped.
fn accelerate(vel: &mut Vec3, wish_dir: Vec3, wish_speed: f32, accel: f32, dt: f32) {
    let current = vel.dot(wish_dir);
    let add = wish_speed - current;
    if add <= 0.0 {
        return;
    }
    let mut amount = accel * wish_speed * dt;
    if amount > add {
        amount = add;
    }
    *vel += wish_dir * amount;
}

/// Quake's PM_Friction — only applied on ground.
fn apply_friction(vel: &mut Vec3, dt: f32) {
    let h = Vec3::new(vel.x, 0.0, vel.z);
    let speed = h.length();
    if speed < 0.05 {
        vel.x = 0.0;
        vel.z = 0.0;
        return;
    }
    let control = speed.max(STOP_SPEED);
    let drop = control * FRICTION * dt;
    let new_speed = (speed - drop).max(0.0);
    let factor = new_speed / speed;
    vel.x *= factor;
    vel.z *= factor;
}

// ───────────────────────────── collision ─────────────────────────────

/// Build a player AABB given feet position.
fn player_aabb(pos: Vec3) -> (Vec3, Vec3) {
    (
        Vec3::new(pos.x - PLAYER_HALF_X, pos.y, pos.z - PLAYER_HALF_Z),
        Vec3::new(pos.x + PLAYER_HALF_X, pos.y + PLAYER_HEIGHT, pos.z + PLAYER_HALF_Z),
    )
}

#[inline]
fn aabb_overlap(amin: Vec3, amax: Vec3, bmin: Vec3, bmax: Vec3) -> bool {
    amax.x > bmin.x && amin.x < bmax.x
        && amax.y > bmin.y && amin.y < bmax.y
        && amax.z > bmin.z && amin.z < bmax.z
}

/// Integrate the player position with axis-separated swept AABB collision.
///
/// We do Y first so a falling player lands on top of a block (and gets
/// `on_ground = true`) before any horizontal collision tries to clip them
/// into the block's side. X and Z are then resolved independently — this
/// is the "Quake-style slide" where one axis being blocked doesn't stop
/// movement on the others.
fn move_and_collide(player: &mut Player, blocks: &[Block], dt: f32) -> bool {
    let mut landed = false;

    // ── Y ──
    let dy = player.vel.y * dt;
    if dy != 0.0 {
        let new_y = player.pos.y + dy;
        let (pmin, pmax) = player_aabb(Vec3::new(player.pos.x, new_y, player.pos.z));
        let mut snap_to: Option<f32> = None;
        let mut going_down = dy < 0.0;
        for blk in blocks {
            if aabb_overlap(pmin, pmax, blk.min(), blk.max()) {
                if going_down {
                    let candidate = blk.max().y + COLLISION_EPS;
                    if snap_to.map_or(true, |s| candidate > s) {
                        snap_to = Some(candidate);
                    }
                } else {
                    // Bumped head on a ceiling.
                    let candidate = blk.min().y - PLAYER_HEIGHT - COLLISION_EPS;
                    if snap_to.map_or(true, |s| candidate < s) {
                        snap_to = Some(candidate);
                    }
                    going_down = false;
                }
            }
        }
        if let Some(y) = snap_to {
            player.pos.y = y;
            if player.vel.y < 0.0 {
                landed = true;
            }
            player.vel.y = 0.0;
        } else {
            player.pos.y = new_y;
        }
    }

    // Floor clamp at y=0 (catches falling through the world)
    if player.pos.y < 0.0 {
        player.pos.y = 0.0;
        if player.vel.y < 0.0 {
            player.vel.y = 0.0;
        }
        landed = true;
    }

    // ── X ──
    let dx = player.vel.x * dt;
    if dx != 0.0 {
        let new_x = player.pos.x + dx;
        let (pmin, pmax) = player_aabb(Vec3::new(new_x, player.pos.y, player.pos.z));
        let mut snap_to: Option<f32> = None;
        for blk in blocks {
            if aabb_overlap(pmin, pmax, blk.min(), blk.max()) {
                let candidate = if dx > 0.0 {
                    blk.min().x - PLAYER_HALF_X - COLLISION_EPS
                } else {
                    blk.max().x + PLAYER_HALF_X + COLLISION_EPS
                };
                if let Some(s) = snap_to {
                    snap_to = Some(if dx > 0.0 { s.min(candidate) } else { s.max(candidate) });
                } else {
                    snap_to = Some(candidate);
                }
            }
        }
        if let Some(x) = snap_to {
            player.pos.x = x;
            player.vel.x = 0.0;
        } else {
            player.pos.x = new_x;
        }
    }

    // ── Z ──
    let dz = player.vel.z * dt;
    if dz != 0.0 {
        let new_z = player.pos.z + dz;
        let (pmin, pmax) = player_aabb(Vec3::new(player.pos.x, player.pos.y, new_z));
        let mut snap_to: Option<f32> = None;
        for blk in blocks {
            if aabb_overlap(pmin, pmax, blk.min(), blk.max()) {
                let candidate = if dz > 0.0 {
                    blk.min().z - PLAYER_HALF_Z - COLLISION_EPS
                } else {
                    blk.max().z + PLAYER_HALF_Z + COLLISION_EPS
                };
                if let Some(s) = snap_to {
                    snap_to = Some(if dz > 0.0 { s.min(candidate) } else { s.max(candidate) });
                } else {
                    snap_to = Some(candidate);
                }
            }
        }
        if let Some(z) = snap_to {
            player.pos.z = z;
            player.vel.z = 0.0;
        } else {
            player.pos.z = new_z;
        }
    }

    landed
}

/// Probe slightly below the player's feet — used to keep `on_ground` true
/// when standing still on top of a block (no vertical motion happening,
/// so movement-based collision wouldn't notice).
fn standing_on_ground(player: &Player, blocks: &[Block]) -> bool {
    if player.pos.y <= 0.001 {
        return true;
    }
    let probe_min = Vec3::new(player.pos.x - PLAYER_HALF_X, player.pos.y - GROUND_PROBE, player.pos.z - PLAYER_HALF_Z);
    let probe_max = Vec3::new(player.pos.x + PLAYER_HALF_X, player.pos.y + 0.001,        player.pos.z + PLAYER_HALF_Z);
    for blk in blocks {
        if aabb_overlap(probe_min, probe_max, blk.min(), blk.max()) {
            return true;
        }
    }
    false
}

// ───────────────────────────── input ─────────────────────────────

#[derive(Default)]
struct Input {
    keys: HashSet<KeyCode>,
    mouse_dx: f32,
    mouse_dy: f32,
}

impl Input {
    fn is_down(&self, code: KeyCode) -> bool {
        self.keys.contains(&code)
    }

    fn take_mouse_delta(&mut self) -> (f32, f32) {
        let d = (self.mouse_dx, self.mouse_dy);
        self.mouse_dx = 0.0;
        self.mouse_dy = 0.0;
        d
    }
}

// ───────────────────────────── skybox ─────────────────────────────

// ───────────────────────────── glTF model loading ─────────────────────────────

/// Load all mesh primitives + the first material's baseColor texture from a
/// `.glb` (binary glTF). We intentionally skip animations, skins, multiple
/// materials, normal maps, MR/occlusion maps — enough to put a recognizable
/// textured mesh on screen with one draw call.
fn load_glb(bytes: &[u8]) -> GlbAsset {
    let gltf = gltf::Gltf::from_slice(bytes).expect("invalid .glb data");
    let blob = gltf
        .blob
        .as_deref()
        .expect("expected .glb (binary glTF) with embedded buffer");

    let mut vertices: Vec<TexturedVertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut base_color_image: Option<Vec<u8>> = None;

    for mesh in gltf.document.meshes() {
        for primitive in mesh.primitives() {
            let reader = primitive.reader(|_buf| Some(blob));
            let base = vertices.len() as u32;

            let positions: Vec<[f32; 3]> = reader
                .read_positions()
                .map(|it| it.collect())
                .unwrap_or_default();
            if positions.is_empty() {
                continue;
            }
            let normals: Vec<[f32; 3]> = reader
                .read_normals()
                .map(|it| it.collect())
                .unwrap_or_else(|| vec![[0.0, 1.0, 0.0]; positions.len()]);
            let uvs: Vec<[f32; 2]> = reader
                .read_tex_coords(0)
                .map(|it| it.into_f32().collect())
                .unwrap_or_else(|| vec![[0.0, 0.0]; positions.len()]);

            for i in 0..positions.len() {
                vertices.push(TexturedVertex {
                    pos: positions[i],
                    normal: *normals.get(i).unwrap_or(&[0.0, 1.0, 0.0]),
                    uv: *uvs.get(i).unwrap_or(&[0.0, 0.0]),
                });
            }

            if let Some(iter) = reader.read_indices() {
                indices.extend(iter.into_u32().map(|i| i + base));
            } else {
                indices.extend(base..(base + positions.len() as u32));
            }

            // Capture the baseColor texture of the first primitive we see.
            if base_color_image.is_none() {
                if let Some(info) = primitive
                    .material()
                    .pbr_metallic_roughness()
                    .base_color_texture()
                {
                    if let gltf::image::Source::View { view, .. } = info.texture().source().source()
                    {
                        let start = view.offset();
                        let end = start + view.length();
                        base_color_image = Some(blob[start..end].to_vec());
                    }
                }
            }
        }
    }

    log::info!(
        "loaded glb: {} verts, {} indices, base_color: {}",
        vertices.len(),
        indices.len(),
        if base_color_image.is_some() { "yes" } else { "no" },
    );

    GlbAsset {
        vertices,
        indices,
        base_color_image,
    }
}

/// Decode a PNG/JPEG byte slice and upload it as an sRGB Rgba8 texture.
fn create_texture_from_image_bytes(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bytes: &[u8],
    label: &str,
) -> wgpu::TextureView {
    let img = image::load_from_memory(bytes)
        .expect("failed to decode embedded image");
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let pixels = rgba.into_raw();

    let texture = device.create_texture_with_data(
        queue,
        &wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // glTF baseColor textures are spec'd as sRGB — let the GPU do
            // the inverse-gamma conversion automatically on sample.
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::default(),
        &pixels,
    );

    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

fn load_skybox(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::TextureView {
    let bytes: &[u8] = include_bytes!("assets/skybox.ktx2");
    let reader = ktx2::Reader::new(bytes).expect("invalid ktx2");
    let header = reader.header();

    let mut image_data = Vec::with_capacity(bytes.len());
    for level in reader.levels() {
        image_data.extend_from_slice(level.data);
    }

    let size = wgpu::Extent3d {
        width: header.pixel_width,
        height: header.pixel_height,
        depth_or_array_layers: 6,
    };

    let texture = device.create_texture_with_data(
        queue,
        &wgpu::TextureDescriptor {
            label: Some("skybox"),
            size,
            mip_level_count: header.level_count,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::MipMajor,
        &image_data,
    );

    texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("skybox view"),
        dimension: Some(wgpu::TextureViewDimension::Cube),
        ..Default::default()
    })
}

// ───────────────────────────── state ─────────────────────────────

struct State {
    instance: wgpu::Instance,
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,

    depth_view: wgpu::TextureView,

    scene_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    instance_count: u32,
    index_count: u32,

    floor_pipeline: wgpu::RenderPipeline,
    floor_buffer: wgpu::Buffer,

    scene_uniform_buffer: wgpu::Buffer,
    scene_bind_group: wgpu::BindGroup,

    sky_pipeline: wgpu::RenderPipeline,
    sky_uniform_buffer: wgpu::Buffer,
    sky_bind_group: wgpu::BindGroup,

    // ── Fox glTF model (decorative, in a far corner) ──
    fox_pipeline: wgpu::RenderPipeline,
    fox_bind_group: wgpu::BindGroup,
    fox_vertex_buffer: wgpu::Buffer,
    fox_index_buffer: wgpu::Buffer,
    fox_instance_buffer: wgpu::Buffer,
    fox_index_count: u32,

    /// World blocks: source of both the GPU instance buffer above and the
    /// per-tick AABB collision list.
    blocks: Vec<Block>,

    /// Chosen present mode (cached so resize/reconfig keep it).
    present_mode: wgpu::PresentMode,

    // ── GPU / API info, shown in HUD (set once at startup) ──
    gpu_name: String,
    gpu_backend: &'static str,
    gpu_device_type: &'static str,
    gpu_driver: String,

    // egui
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,

    // Game state
    camera: Camera,
    player: Player,
    input: Input,
    captured: bool,
    paused: bool,
    exit_requested: bool,

    // Timing
    last_frame: Instant,
    sim_accumulator: f32,
    fps_counter: u32,
    fps_timer: Instant,
    /// Last FPS sampled (refreshed once per second). Shown in egui HUD.
    fps_display: f32,
    frame_ms_display: f32,
    first_frame_done: bool,
}

impl State {
    async fn new(display: OwnedDisplayHandle, window: Arc<Window>) -> State {
        // `*_from_env` so env vars (WGPU_BACKEND, WGPU_POWER_PREF,
        // WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER, …) are honoured. The
        // plain `new_with_display_handle` ignores them and would silently
        // pick whatever wgpu's auto-selection wants — on Mali/Linux that's
        // GL even when PanVK is available.
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(display)),
        );
        // Create the surface *first* so we can pass it as `compatible_surface`
        // when asking for an adapter. On the web's WebGL2 backend this is
        // required — wgpu needs the canvas's WebGL context to construct an
        // adapter at all. On native it's free hint that makes sure the
        // adapter we get supports our window's surface.
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("no compatible GPU adapter");
        let adapter_info = adapter.get_info();
        log::info!("adapter: {adapter_info:?}");
        let gpu_name = adapter_info.name.clone();
        let gpu_backend = fmt_backend(adapter_info.backend);
        let gpu_device_type = fmt_device_type(adapter_info.device_type);
        let gpu_driver = match (
            adapter_info.driver.is_empty(),
            adapter_info.driver_info.is_empty(),
        ) {
            (true, true) => String::new(),
            (false, true) => adapter_info.driver.clone(),
            (true, false) => adapter_info.driver_info.clone(),
            (false, false) => format!("{} ({})", adapter_info.driver, adapter_info.driver_info),
        };
        println!("[ffoie] GPU: {gpu_name}  backend: {gpu_backend}  type: {gpu_device_type}");
        if !gpu_driver.is_empty() {
            println!("[ffoie] driver: {gpu_driver}");
        }

        // WebGPU's *default* `max_texture_dimension_2d` is only 8192, which is
        // smaller than the physical pixel size of large HiDPI displays.
        // Request the adapter's actual maximum so we can create a depth
        // attachment that matches the canvas. Native Metal/Vulkan don't have
        // this restriction but the explicit limit is harmless there.
        let adapter_limits = adapter.limits();
        // `downlevel_defaults` (rather than the WebGPU-spec `default`) is the
        // explicitly-mobile-safe baseline — it doesn't ask for things like
        // `max_texture_dimension_3d = 2048` that PanVK on Mali-G52 can't
        // satisfy (the GPU caps that at 512). We still bump 2D up to the
        // adapter's own max so HiDPI displays get a depth attachment that
        // matches the canvas.
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_limits: wgpu::Limits {
                    max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
                    ..wgpu::Limits::downlevel_defaults()
                },
                ..Default::default()
            })
            .await
            .expect("device request failed");

        let size = clamp_render_size(window.inner_size());
        let cap = surface.get_capabilities(&adapter);
        let surface_format = cap.formats[0];
        let surface_format_srgb = surface_format.add_srgb_suffix();
        let depth_view = create_depth_view(&device, size);

        // Default to vsync'd presentation. Rendering uncapped above the
        // display refresh rate puts the GPU at full power for zero
        // perceptual benefit — and on Apple Silicon with a shared power
        // budget, that can heat the machine to the point of thermal stall.
        //
        // Mailbox (display-rate, no stalls) is the best feel-vs-load
        // trade-off; fall back to Fifo if it's unavailable.
        let present_mode = if cap.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if cap.present_modes.contains(&wgpu::PresentMode::Fifo) {
            wgpu::PresentMode::Fifo
        } else {
            wgpu::PresentMode::AutoVsync
        };
        log::info!("present mode: {present_mode:?}, available: {:?}", cap.present_modes);
        println!("[ffoie] present mode: {present_mode:?}");

        // ── Buffers: cubes + floor ──
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cube verts"),
            contents: bytemuck::cast_slice(CUBE_VERTS),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cube indices"),
            contents: bytemuck::cast_slice(CUBE_INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });
        let blocks = build_course();
        let instances: Vec<Instance> = blocks.iter().map(Block::to_instance).collect();
        let instance_count = instances.len() as u32;
        let instance_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("instances"),
            contents: bytemuck::cast_slice(&instances),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let floor_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("floor"),
            contents: bytemuck::cast_slice(FLOOR_VERTS),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // ── Scene uniforms (view_proj) shared by cubes, floor, and fox. ──
        let scene_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scene uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // ── Fox glTF model (Khronos sample asset, CC0) ──
        // The base mesh is in cm; scale ~0.03 lands it at roughly player height.
        let fox_asset = load_glb(include_bytes!("assets/fox.glb"));
        let fox_index_count = fox_asset.indices.len() as u32;
        let fox_vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fox verts"),
            contents: bytemuck::cast_slice(&fox_asset.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let fox_index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fox indices"),
            contents: bytemuck::cast_slice(&fox_asset.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        let fox_instances = [Instance {
            pos: [40.0, 0.0, -28.0],
            scale: [0.03, 0.03, 0.03],
            color: [1.0, 1.0, 1.0], // unused by fox shader (texture provides color)
        }];
        let fox_instance_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fox instance"),
            contents: bytemuck::cast_slice(&fox_instances),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // Texture + sampler + bind group + pipeline for the fox.
        let fox_texture_view = create_texture_from_image_bytes(
            &device,
            &queue,
            fox_asset
                .base_color_image
                .as_deref()
                .expect("fox.glb must contain a baseColor texture"),
            "fox baseColor",
        );
        let fox_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("fox sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let fox_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fox bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let fox_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fox bg"),
            layout: &fox_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: scene_uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&fox_texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&fox_sampler),
                },
            ],
        });

        let fox_shader = device.create_shader_module(wgpu::include_wgsl!("fox.wgsl"));
        // TexturedVertex layout: pos(0), normal(1), uv(2). Instance same as cubes
        // (pos(3), scale(4), color(5)) — color is declared in shader but unused.
        let fox_vertex_attrs = wgpu::vertex_attr_array![
            0 => Float32x3,
            1 => Float32x3,
            2 => Float32x2,
        ];
        let fox_instance_attrs = wgpu::vertex_attr_array![
            3 => Float32x3,
            4 => Float32x3,
            5 => Float32x3,
        ];
        let fox_buffers = [
            wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<TexturedVertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &fox_vertex_attrs,
            },
            wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Instance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &fox_instance_attrs,
            },
        ];
        let fox_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fox pl"),
            bind_group_layouts: &[Some(&fox_bgl)],
            immediate_size: 0,
        });
        let fox_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fox pipeline"),
            layout: Some(&fox_pl),
            vertex: wgpu::VertexState {
                module: &fox_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &fox_buffers,
            },
            fragment: Some(wgpu::FragmentState {
                module: &fox_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format_srgb,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // glTF's winding can vary by exporter; rendering both sides
                // is cheap insurance against an invisible fox.
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let scene_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let scene_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene bg"),
            layout: &scene_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: scene_uniform_buffer.as_entire_binding(),
            }],
        });
        let scene_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene pl"),
            bind_group_layouts: &[Some(&scene_bgl)],
            immediate_size: 0,
        });

        // ── Cubes pipeline ──
        let scene_shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let cube_vertex_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
        // Per-instance: pos, scale, color
        let instance_attrs =
            wgpu::vertex_attr_array![2 => Float32x3, 3 => Float32x3, 4 => Float32x3];
        let cube_buffers = [
            wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &cube_vertex_attrs,
            },
            wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Instance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &instance_attrs,
            },
        ];
        let scene_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cubes"),
            layout: Some(&scene_pl),
            vertex: wgpu::VertexState {
                module: &scene_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &cube_buffers,
            },
            fragment: Some(wgpu::FragmentState {
                module: &scene_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format_srgb,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // ── Floor pipeline (NOTE: cull_mode = None so the floor renders
        //    regardless of which side the triangle winding faces. Cheaper
        //    than fixing the winding manually, and makes "from below" debug
        //    views work too.) ──
        let floor_shader = device.create_shader_module(wgpu::include_wgsl!("floor.wgsl"));
        let floor_attrs = wgpu::vertex_attr_array![0 => Float32x3];
        let floor_layout = [wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<FloorVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &floor_attrs,
        }];
        let floor_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("floor"),
            layout: Some(&scene_pl),
            vertex: wgpu::VertexState {
                module: &floor_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &floor_layout,
            },
            fragment: Some(wgpu::FragmentState {
                module: &floor_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format_srgb,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None, // ← was Some(Face::Back); fix for invisible-floor bug
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // ── Sky pipeline ──
        let sky_view = load_skybox(&device, &queue);
        let sky_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sky sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let sky_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sky uniforms"),
            size: std::mem::size_of::<SkyUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sky_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sky bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::Cube,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sky_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sky bg"),
            layout: &sky_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: sky_uniform_buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&sky_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sky_sampler) },
            ],
        });
        let sky_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sky pl"),
            bind_group_layouts: &[Some(&sky_bgl)],
            immediate_size: 0,
        });
        let sky_shader = device.create_shader_module(wgpu::include_wgsl!("sky.wgsl"));
        let sky_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sky"),
            layout: Some(&sky_pl),
            vertex: wgpu::VertexState {
                module: &sky_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &sky_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format_srgb,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // ── egui ──
        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window, // Window implements HasDisplayHandle
            Some(window.scale_factor() as f32),
            None,
            Some(device.limits().max_texture_dimension_2d as usize),
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            surface_format_srgb,
            egui_wgpu::RendererOptions {
                depth_stencil_format: Some(DEPTH_FORMAT),
                ..Default::default()
            },
        );

        let camera = Camera::new(size.width as f32 / size.height.max(1) as f32);

        let s = State {
            instance,
            window,
            device,
            queue,
            size,
            surface,
            surface_format,
            depth_view,
            scene_pipeline,
            vertex_buffer,
            index_buffer,
            instance_buffer,
            instance_count,
            index_count: CUBE_INDICES.len() as u32,
            floor_pipeline,
            floor_buffer,
            scene_uniform_buffer,
            scene_bind_group,
            sky_pipeline,
            sky_uniform_buffer,
            sky_bind_group,
            fox_pipeline,
            fox_bind_group,
            fox_vertex_buffer,
            fox_index_buffer,
            fox_instance_buffer,
            fox_index_count,
            blocks,
            present_mode,
            gpu_name,
            gpu_backend,
            gpu_device_type,
            gpu_driver,
            egui_ctx,
            egui_state,
            egui_renderer,
            camera,
            player: Player::new(),
            input: Input::default(),
            captured: false,
            paused: false,
            exit_requested: false,
            last_frame: Instant::now(),
            sim_accumulator: 0.0,
            fps_counter: 0,
            fps_timer: Instant::now(),
            fps_display: 0.0,
            frame_ms_display: 0.0,
            first_frame_done: false,
        };
        s.configure_surface();
        s
    }

    fn configure_surface(&self) {
        let cfg = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            view_formats: vec![self.surface_format.add_srgb_suffix()],
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            width: self.size.width,
            height: self.size.height,
            // Default 2: lowest input latency without starving the GPU.
            // Bumping to 3+ helps if you're CPU-bound, but with vsync'd
            // presentation and a 100Hz display you have plenty of headroom.
            desired_maximum_frame_latency: 2,
            present_mode: self.present_mode,
        };
        self.surface.configure(&self.device, &cfg);
    }

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        let new_size = clamp_render_size(new_size);
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.size = new_size;
        self.depth_view = create_depth_view(&self.device, new_size);
        self.camera.aspect = new_size.width as f32 / new_size.height as f32;
        self.configure_surface();
    }

    fn apply_mouse_look(&mut self) {
        let (dx, dy) = self.input.take_mouse_delta();
        if !self.captured || self.paused {
            return;
        }
        self.camera.yaw -= dx * MOUSE_SENSITIVITY;
        self.camera.pitch -= dy * MOUSE_SENSITIVITY;
        self.camera.pitch = self.camera.pitch.clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Fixed-timestep tick. Implements Quake-style movement physics.
    fn tick(&mut self) {
        if !self.captured || self.paused {
            return;
        }

        // ── Compute wish direction from input ──
        let yaw_rot = Quat::from_axis_angle(Vec3::Y, self.camera.yaw);
        let forward_h = (yaw_rot * Vec3::NEG_Z).normalize_or_zero();
        let right_h = (yaw_rot * Vec3::X).normalize_or_zero();

        let mut wish = Vec3::ZERO;
        if self.input.is_down(KeyCode::KeyW) { wish += forward_h; }
        if self.input.is_down(KeyCode::KeyS) { wish -= forward_h; }
        if self.input.is_down(KeyCode::KeyD) { wish += right_h; }
        if self.input.is_down(KeyCode::KeyA) { wish -= right_h; }
        let wish_len = wish.length();
        let (wish_dir, wish_speed) = if wish_len > 0.0 {
            let s = if self.input.is_down(KeyCode::ShiftLeft) {
                MAX_SPEED * SPRINT_MULT
            } else {
                MAX_SPEED
            };
            (wish / wish_len, s)
        } else {
            (Vec3::ZERO, 0.0)
        };

        // ── Apply friction on ground, then accelerate ──
        // (Friction first, then accel — Quake order. This is why an instant
        //  jump preserves the just-added-this-tick velocity.)
        if self.player.on_ground {
            apply_friction(&mut self.player.vel, TICK_DT);
            accelerate(&mut self.player.vel, wish_dir, wish_speed, GROUND_ACCEL, TICK_DT);

            // Auto-hop: Space held while grounded → jump.
            if self.input.is_down(KeyCode::Space) {
                self.player.vel.y = JUMP_VELOCITY;
                self.player.on_ground = false;
            }
        } else {
            // PM_AirAccelerate: same function, lower accel constant.
            // This is the magic that makes strafe-jumping work.
            accelerate(&mut self.player.vel, wish_dir, wish_speed, AIR_ACCEL, TICK_DT);
        }

        // ── Gravity ──
        self.player.vel.y += GRAVITY * TICK_DT;

        // ── Integrate with collision (Y first, then X, then Z) ──
        let landed = move_and_collide(&mut self.player, &self.blocks, TICK_DT);
        if landed {
            self.player.on_ground = true;
        } else {
            // No "landing" event this tick, but we may still be resting on
            // something — probe just below the feet.
            self.player.on_ground = standing_on_ground(&self.player, &self.blocks);
        }

        self.camera.position = self.player.pos + Vec3::new(0.0, EYE_HEIGHT, 0.0);
    }

    fn render(&mut self) {
        // ── Timing ──
        let now = Instant::now();
        let frame_dt = now.duration_since(self.last_frame).as_secs_f32().min(0.25);
        self.last_frame = now;

        // ── Sim ticks ──
        self.sim_accumulator += frame_dt;
        let mut ticks = 0;
        while self.sim_accumulator >= TICK_DT && ticks < MAX_TICKS_PER_FRAME {
            self.tick();
            self.sim_accumulator -= TICK_DT;
            ticks += 1;
        }

        self.apply_mouse_look();

        // ── GPU uniforms ──
        let view = self.camera.view();
        let proj = self.camera.proj();
        let view_proj = proj * view;
        self.queue.write_buffer(
            &self.scene_uniform_buffer, 0,
            bytemuck::bytes_of(&Uniforms { view_proj: view_proj.to_cols_array_2d() }),
        );
        self.queue.write_buffer(
            &self.sky_uniform_buffer, 0,
            bytemuck::bytes_of(&SkyUniforms {
                proj_inv: proj.inverse().to_cols_array_2d(),
                view: view.to_cols_array_2d(),
            }),
        );

        // ── Build egui UI ──
        let raw_input = self.egui_state.take_egui_input(&self.window);
        let mut resume_clicked = false;
        let mut exit_clicked = false;
        let fps = self.fps_display;
        let frame_ms = self.frame_ms_display;
        let paused = self.paused;
        let speed_h = {
            let v = self.player.vel;
            (v.x * v.x + v.z * v.z).sqrt()
        };
        let gpu_name = self.gpu_name.clone();
        let gpu_backend = self.gpu_backend;
        let gpu_device_type = self.gpu_device_type;
        let gpu_driver = self.gpu_driver.clone();
        let present_mode_str = fmt_present_mode(self.present_mode);
        let viewport_w = self.size.width;
        let viewport_h = self.size.height;

        let full_output = self.egui_ctx.run_ui(raw_input, |root_ui| {
            let ctx = root_ui.ctx().clone();
            let style = ctx.global_style();

            // ─ Crosshair + speed meter (both painted directly on the
            //   foreground layer — no widget frames, no backgrounds).
            //   Hidden when paused so the cursor reaches menu buttons clean.
            if !paused {
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("reticle"),
                ));
                // Compute the screen centre from the framebuffer dimensions
                // we *actually* configured the surface with (`viewport_w/h`)
                // and egui's current scale factor. Using `ctx.content_rect()`
                // can lag by a frame after a window resize on the web, leaving
                // the crosshair off-centre until the next redraw.
                let ppp = ctx.pixels_per_point().max(0.001);
                let c = egui::pos2(
                    viewport_w as f32 / ppp * 0.5,
                    viewport_h as f32 / ppp * 0.5,
                );

                // ── Crosshair ──
                let len = 7.0;
                let outer = egui::Stroke::new(
                    3.0,
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 160),
                );
                let inner = egui::Stroke::new(
                    1.5,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 230),
                );
                for stroke in [outer, inner] {
                    painter.line_segment(
                        [egui::pos2(c.x - len, c.y), egui::pos2(c.x + len, c.y)],
                        stroke,
                    );
                    painter.line_segment(
                        [egui::pos2(c.x, c.y - len), egui::pos2(c.x, c.y + len)],
                        stroke,
                    );
                }

                // ── Thin speed bar, just below the crosshair ──
                let bar_w = 140.0;
                let bar_h = 2.0;
                let bar_y = c.y + 26.0;
                let bar_x = c.x - bar_w / 2.0;
                let progress = (speed_h / 25.0).clamp(0.0, 1.0);

                // Faint full-width track so the bar is locatable at 0 m/s.
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(bar_x, bar_y),
                        egui::vec2(bar_w, bar_h),
                    ),
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 55),
                );
                // Filled portion in speed-coloured fill.
                if progress > 0.0 {
                    painter.rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(bar_x, bar_y),
                            egui::vec2(bar_w * progress, bar_h),
                        ),
                        0.0,
                        speed_color(speed_h),
                    );
                }

                // Speed number just below the bar.
                painter.text(
                    egui::pos2(c.x, bar_y + bar_h + 12.0),
                    egui::Align2::CENTER_CENTER,
                    format!("{speed_h:.1} m/s"),
                    egui::FontId::monospace(11.0),
                    egui::Color32::from_rgba_unmultiplied(230, 230, 230, 220),
                );
            }

            // ─ FPS / engine HUD, top-left ─
            egui::Area::new(egui::Id::new("hud"))
                .anchor(egui::Align2::LEFT_TOP, [10.0, 10.0])
                .show(&ctx, |ui| {
                    egui::Frame::popup(&style)
                        .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180))
                        .show(ui, |ui| {
                            ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
                            // Realtime perf
                            ui.colored_label(
                                egui::Color32::from_rgb(140, 255, 160),
                                format!("{fps:>5.0} fps   {frame_ms:>5.2} ms"),
                            );

                            // ── Engine / GPU info ──
                            // Lets you confirm which backend wgpu picked when
                            // running this same binary on Win/Linux/macOS with
                            // different GPUs.
                            ui.separator();
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 220, 130),
                                format!("API   {gpu_backend}"),
                            );
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 220, 220),
                                format!("GPU   {gpu_name}"),
                            );
                            ui.colored_label(
                                egui::Color32::from_rgb(170, 170, 170),
                                format!("Type  {gpu_device_type}"),
                            );
                            ui.colored_label(
                                egui::Color32::from_rgb(170, 170, 170),
                                format!("Pres. {present_mode_str}"),
                            );
                            ui.colored_label(
                                egui::Color32::from_rgb(170, 170, 170),
                                format!("Res   {viewport_w} x {viewport_h}"),
                            );
                            if !gpu_driver.is_empty() {
                                ui.colored_label(
                                    egui::Color32::from_rgb(130, 130, 130),
                                    format!("Drv   {gpu_driver}"),
                                );
                            }
                            ui.colored_label(
                                egui::Color32::from_rgb(120, 130, 150),
                                "wgpu 29  ·  egui 0.34",
                            );
                        });
                });

            // ─ Pause menu ─
            if paused {
                egui::Area::new(egui::Id::new("pause"))
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(&ctx, |ui| {
                        egui::Frame::popup(&style)
                            .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 24, 230))
                            .inner_margin(egui::Margin::same(24))
                            .show(ui, |ui| {
                                ui.set_min_width(220.0);
                                ui.vertical_centered(|ui| {
                                    ui.heading("Paused");
                                    ui.add_space(10.0);
                                    let btn = |label: &str| -> egui::Button<'_> {
                                        egui::Button::new(
                                            egui::RichText::new(label).size(18.0),
                                        )
                                        .min_size(egui::vec2(180.0, 36.0))
                                    };
                                    if ui.add(btn("Resume")).clicked() {
                                        resume_clicked = true;
                                    }
                                    ui.add_space(6.0);
                                    if ui.add(btn("Exit")).clicked() {
                                        exit_clicked = true;
                                    }
                                    ui.add_space(4.0);
                                    ui.colored_label(
                                        egui::Color32::from_gray(180),
                                        "Esc to resume",
                                    );
                                });
                            });
                    });
            }
        });

        // Egui platform output: clipboard, cursor icon, etc.
        self.egui_state
            .handle_platform_output(&self.window, full_output.platform_output);

        let pixels_per_point = full_output.pixels_per_point;
        let tris = self
            .egui_ctx
            .tessellate(full_output.shapes, pixels_per_point);
        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.size.width, self.size.height],
            pixels_per_point,
        };
        for (id, image_delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, image_delta);
        }

        // ── Acquire surface ──
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(tex) => tex,
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => return,
            wgpu::CurrentSurfaceTexture::Suboptimal(tex) => {
                drop(tex);
                self.configure_surface();
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.configure_surface();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => unreachable!(),
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone()).unwrap();
                self.configure_surface();
                return;
            }
        };
        let color_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.surface_format.add_srgb_suffix()),
                ..Default::default()
            });

        // ── Encode ──
        let mut encoder = self.device.create_command_encoder(&Default::default());

        // egui_wgpu may need to copy texture data; it can request extra command
        // buffers via update_buffers — we submit them with our own.
        let egui_cmds =
            self.egui_renderer
                .update_buffers(&self.device, &self.queue, &mut encoder, &tris, &screen_desc);

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // 1. Cubes
            rpass.set_pipeline(&self.scene_pipeline);
            rpass.set_bind_group(0, &self.scene_bind_group, &[]);
            rpass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            rpass.set_vertex_buffer(1, self.instance_buffer.slice(..));
            rpass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
            rpass.draw_indexed(0..self.index_count, 0, 0..self.instance_count);

            // 2. Fox (textured pipeline, samples baseColor from the glTF).
            rpass.set_pipeline(&self.fox_pipeline);
            rpass.set_bind_group(0, &self.fox_bind_group, &[]);
            rpass.set_vertex_buffer(0, self.fox_vertex_buffer.slice(..));
            rpass.set_vertex_buffer(1, self.fox_instance_buffer.slice(..));
            rpass.set_index_buffer(self.fox_index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            rpass.draw_indexed(0..self.fox_index_count, 0, 0..1);

            // 3. Floor
            rpass.set_pipeline(&self.floor_pipeline);
            rpass.set_bind_group(0, &self.scene_bind_group, &[]);
            rpass.set_vertex_buffer(0, self.floor_buffer.slice(..));
            rpass.draw(0..FLOOR_VERTS.len() as u32, 0..1);

            // 3. Sky (only fills uncovered pixels)
            rpass.set_pipeline(&self.sky_pipeline);
            rpass.set_bind_group(0, &self.sky_bind_group, &[]);
            rpass.draw(0..3, 0..1);

            // 4. egui (HUD + optional pause menu)
            self.egui_renderer.render(&mut rpass.forget_lifetime(), &tris, &screen_desc);
        }

        self.queue
            .submit(egui_cmds.into_iter().chain([encoder.finish()]));
        self.window.pre_present_notify();
        surface_texture.present();

        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        // ── Apply menu actions ──
        if resume_clicked {
            close_menu_and_play(self);
        }
        if exit_clicked {
            self.exit_requested = true;
        }

        // ── Telemetry ──
        if !self.first_frame_done {
            self.first_frame_done = true;
            if let Some(t) = START_TIME.get() {
                let ms = t.elapsed().as_secs_f32() * 1000.0;
                println!("[ffoie] cold start: {ms:.0} ms");
                println!("[ffoie] click window to play · Esc opens menu · WASD move · Space jump · LeftShift sprint");
                println!("[ffoie] strafe-jump: hold W + A or D, look diagonal, jump → gain speed");
            }
        }
        self.fps_counter += 1;
        if self.fps_timer.elapsed() >= Duration::from_secs(1) {
            self.fps_display = self.fps_counter as f32;
            self.frame_ms_display = 1000.0 / self.fps_display.max(1.0);
            self.fps_counter = 0;
            self.fps_timer = Instant::now();
        }
    }
}

/// Maximum render-target dimension along either axis. WebGPU's default
/// `max_texture_dimension_2d` is 8192; some hardware tops out at 16384. With
/// HiDPI canvases on huge displays we can blow past either. Capping the
/// drawing buffer at 4K per side keeps allocations sane while the browser
/// upscales the framebuffer to the canvas's CSS size — visually
/// indistinguishable from a 1:1 render on any realistic display.
const MAX_RENDER_DIM: u32 = 4096;

/// Clamp the physical render size to `MAX_RENDER_DIM` while preserving
/// aspect ratio.
fn clamp_render_size(s: winit::dpi::PhysicalSize<u32>) -> winit::dpi::PhysicalSize<u32> {
    let w = s.width.max(1);
    let h = s.height.max(1);
    if w <= MAX_RENDER_DIM && h <= MAX_RENDER_DIM {
        return winit::dpi::PhysicalSize::new(w, h);
    }
    let longest = w.max(h) as u64;
    let scale = MAX_RENDER_DIM as u64;
    winit::dpi::PhysicalSize::new(
        ((w as u64 * scale / longest) as u32).max(1),
        ((h as u64 * scale / longest) as u32).max(1),
    )
}

fn create_depth_view(
    device: &wgpu::Device,
    size: winit::dpi::PhysicalSize<u32>,
) -> wgpu::TextureView {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("depth"),
        size: wgpu::Extent3d {
            width: size.width.max(1),
            height: size.height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    tex.create_view(&wgpu::TextureViewDescriptor::default())
}

// ───────────────────────────── cursor helpers ─────────────────────────────

fn grab_cursor(window: &Window) -> bool {
    let res = window
        .set_cursor_grab(CursorGrabMode::Locked)
        .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined));
    if res.is_ok() {
        window.set_cursor_visible(false);
        true
    } else {
        false
    }
}

fn release_cursor(window: &Window) {
    let _ = window.set_cursor_grab(CursorGrabMode::None);
    window.set_cursor_visible(true);
}

fn open_menu(s: &mut State) {
    if s.paused {
        return;
    }
    s.paused = true;
    if s.captured {
        release_cursor(&s.window);
        s.captured = false;
    }
}

fn close_menu_and_play(s: &mut State) {
    s.paused = false;
    if !s.captured && grab_cursor(&s.window) {
        s.captured = true;
    }
}

// ───────────────────────────── app ─────────────────────────────

#[derive(Default)]
struct App {
    state: Option<State>,
    /// On wasm we can't `pollster::block_on` the async `State::new` (it'd
    /// hang the JS event loop). Instead `resumed` kicks the init off via
    /// `spawn_local` and stashes the result here; `window_event` adopts it
    /// once it's ready.
    #[cfg(target_arch = "wasm32")]
    pending_state: std::rc::Rc<std::cell::RefCell<Option<State>>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        if self.pending_state.borrow().is_some() {
            return; // async init already in flight
        }

        let attrs = Window::default_attributes()
            .with_title("FFOIE — wgpu prototype")
            .with_inner_size(winit::dpi::LogicalSize::new(1280, 720));
        let window = Arc::new(event_loop.create_window(attrs).unwrap());

        // On the web, winit creates a fresh <canvas> but does NOT attach it
        // to the DOM. Append it to <body> so it's actually visible.
        #[cfg(target_arch = "wasm32")]
        {
            use winit::platform::web::WindowExtWebSys;
            if let (Some(doc_win), Some(canvas)) = (web_sys::window(), window.canvas()) {
                if let Some(body) = doc_win.document().and_then(|d| d.body()) {
                    let _ = body.append_child(&canvas);
                }
            }
        }

        let display = event_loop.owned_display_handle();

        #[cfg(not(target_arch = "wasm32"))]
        {
            let state = pollster::block_on(State::new(display, window.clone()));
            self.state = Some(state);
            if let Some(s) = self.state.as_ref() {
                s.window.request_redraw();
            }
        }

        #[cfg(target_arch = "wasm32")]
        {
            let cell = self.pending_state.clone();
            let win_clone = window.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let state = State::new(display, win_clone).await;
                let w = state.window.clone();
                *cell.borrow_mut() = Some(state);
                w.request_redraw();
            });
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // On the web, the async `State::new` may have just finished — adopt it.
        #[cfg(target_arch = "wasm32")]
        if self.state.is_none() {
            if let Some(state) = self.pending_state.borrow_mut().take() {
                self.state = Some(state);
            } else {
                return;
            }
        }

        let Some(state) = self.state.as_mut() else { return };

        // Forward to egui so it can drive its UI (mouse-over, button clicks, etc.).
        let egui_response = state.egui_state.on_window_event(&state.window, &event);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => state.resize(size),
            WindowEvent::Focused(false) => {
                if state.captured {
                    release_cursor(&state.window);
                    state.captured = false;
                    state.paused = true;
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                // If egui consumed the click (e.g. pressed Resume / Exit), do nothing —
                // the result is picked up after egui's UI builder runs.
                // Otherwise, an un-captured click means "start playing".
                if !egui_response.consumed && !state.captured && !state.paused {
                    close_menu_and_play(state);
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state: key_state,
                        logical_key,
                        ..
                    },
                ..
            } => {
                // Esc toggles the menu — always handled by us, regardless of egui.
                if let Key::Named(NamedKey::Escape) = logical_key {
                    if key_state == ElementState::Pressed {
                        if state.paused {
                            close_menu_and_play(state);
                        } else if state.captured {
                            open_menu(state);
                        }
                    }
                }
                match key_state {
                    ElementState::Pressed => { state.input.keys.insert(code); }
                    ElementState::Released => { state.input.keys.remove(&code); }
                }
            }
            WindowEvent::RedrawRequested => {
                state.render();
                if state.exit_requested {
                    event_loop.exit();
                    return;
                }
                state.window.request_redraw();
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        let Some(state) = self.state.as_mut() else { return };
        if let DeviceEvent::MouseMotion { delta } = event {
            state.input.mouse_dx += delta.0 as f32;
            state.input.mouse_dy += delta.1 as f32;
        }
    }
}

// ───────────────────────────── entry points ─────────────────────────────
//
// The event loop is the same on native and web — only the *bootstrap* differs:
//   • Native binary: `fn main` is called by the OS loader.
//   • Web (wasm32):  `run_wasm` is called by JavaScript when the page loads,
//                    via the `#[wasm_bindgen(start)]` attribute.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    let _ = START_TIME.set(Instant::now());
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    run_event_loop();
}

// Cargo requires `main` on every binary; on wasm the real entry is `run_wasm`,
// but we keep a no-op `main` to satisfy the binary-crate requirement.
#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn run_wasm() {
    let _ = START_TIME.set(Instant::now());
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Info);
    log::info!("[ffoie] booting on wasm32 / WebGPU");
    run_event_loop();
}

fn run_event_loop() {
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);

    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut app = App::default();
        event_loop.run_app(&mut app).unwrap();
    }
    #[cfg(target_arch = "wasm32")]
    {
        // On the web `run_app` would block the JS event loop forever.
        // `spawn_app` returns to JS and hooks our handler into requestAnimationFrame.
        use winit::platform::web::EventLoopExtWebSys;
        let app = App::default();
        event_loop.spawn_app(app);
    }
}
