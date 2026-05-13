// Floor: a single large quad at y=0 with a procedural "notebook grid"
// drawn in the fragment shader. Antialiased via fwidth() so lines stay
// crisp at all distances.

struct Uniforms {
    view_proj: mat4x4<f32>,
}
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsIn {
    @location(0) pos: vec3<f32>,
}

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) world_xz: vec2<f32>,
    @location(1) world_pos: vec3<f32>,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip_pos = u.view_proj * vec4<f32>(in.pos, 1.0);
    out.world_xz = in.pos.xz;
    out.world_pos = in.pos;
    return out;
}

// One-unit-spaced grid lines, antialiased.
// Returns 1.0 on a line, 0.0 elsewhere.
fn grid(uv: vec2<f32>, line_width_px: f32) -> f32 {
    // Distance from the nearest grid line in each axis, in cells.
    let d = abs(fract(uv - 0.5) - 0.5);
    // Convert to pixel-space using the screen-space derivative.
    let w = fwidth(uv) * line_width_px;
    let line = min(d.x / max(w.x, 1e-6), d.y / max(w.y, 1e-6));
    return 1.0 - min(line, 1.0);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Two grid scales: minor (every 1 unit) and major (every 5 units).
    let minor = grid(in.world_xz, 0.6);
    let major = grid(in.world_xz / 5.0, 0.9);

    // Notebook palette: pure black background, vivid green crosses.
    let bg = vec3<f32>(0.0, 0.0, 0.0);
    let minor_color = vec3<f32>(0.0, 0.65, 0.18);
    let major_color = vec3<f32>(0.15, 1.0, 0.35);

    // Distance fog so the grid fades into the sky in the distance.
    let dist = length(in.world_pos.xz);
    let fog = clamp(1.0 - dist / 120.0, 0.0, 1.0);

    var c = bg;
    c = mix(c, minor_color, minor * 0.85);
    c = mix(c, major_color, major);
    c = c * fog;

    return vec4<f32>(c, 1.0);
}
