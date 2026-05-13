// FFOIE prototype shader: instanced blocks with per-instance scale.

struct Uniforms {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) instance_pos: vec3<f32>,
    @location(3) instance_scale: vec3<f32>,
    @location(4) instance_color: vec3<f32>,
}

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) normal: vec3<f32>,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    // The base cube mesh is unit-sized (±0.5). Scale gives the block its full
    // world-space size; instance_pos places the centre.
    let world_pos = in.position * in.instance_scale + in.instance_pos;
    out.clip_pos = u.view_proj * vec4<f32>(world_pos, 1.0);
    out.color = in.instance_color;
    out.normal = in.normal;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let light_dir = normalize(vec3<f32>(0.4, 0.8, 0.3));
    let diffuse = max(dot(normalize(in.normal), light_dir), 0.0);
    let ambient = 0.18;
    let lit = in.color * (ambient + diffuse * 0.85);
    return vec4<f32>(lit, 1.0);
}
