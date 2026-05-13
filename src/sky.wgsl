// Skybox: fullscreen triangle that samples a cubemap by reconstructing
// the world-space camera ray from screen coordinates.
// Adapted from wgpu's skybox example.

struct SkyData {
    proj_inv: mat4x4<f32>,
    view: mat4x4<f32>,
}
@group(0) @binding(0) var<uniform> r: SkyData;
@group(0) @binding(1) var sky_tex: texture_cube<f32>;
@group(0) @binding(2) var sky_samp: sampler;

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) dir: vec3<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    // One oversized triangle covering the screen — no vertex buffer needed.
    let x = i32(vid) / 2;
    let y = i32(vid) & 1;
    let pos = vec4<f32>(
        f32(x) * 4.0 - 1.0,
        f32(y) * 4.0 - 1.0,
        1.0,  // far plane in NDC (depth = 1)
        1.0
    );
    // Unproject NDC → camera space, then inverse-rotate camera → world.
    let view3 = mat3x3<f32>(r.view[0].xyz, r.view[1].xyz, r.view[2].xyz);
    let inv_view = transpose(view3); // orthonormal => transpose == inverse
    let cam_ray = r.proj_inv * pos;

    var out: VsOut;
    out.clip_pos = pos;
    out.dir = inv_view * cam_ray.xyz;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(sky_tex, sky_samp, normalize(in.dir));
}
