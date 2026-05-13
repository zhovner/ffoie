// Textured static mesh shader, used for glTF models (Fox).
// Vertex layout matches `TexturedVertex` + `Instance` (which has pos/scale/color;
// we only read pos and scale here, color is ignored).

struct Uniforms {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var base_color_tex: texture_2d<f32>;
@group(0) @binding(2) var base_color_sampler: sampler;

struct VsIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) instance_pos: vec3<f32>,
    @location(4) instance_scale: vec3<f32>,
    @location(5) instance_color: vec3<f32>, // declared so the buffer layout matches, unused
}

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) normal: vec3<f32>,
}

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    let world_pos = in.position * in.instance_scale + in.instance_pos;
    out.clip_pos = u.view_proj * vec4<f32>(world_pos, 1.0);
    out.uv = in.uv;
    out.normal = in.normal;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let light_dir = normalize(vec3<f32>(0.4, 0.8, 0.3));
    let n = normalize(in.normal);
    let diffuse = max(dot(n, light_dir), 0.0);
    let ambient = 0.28;
    let albedo = textureSample(base_color_tex, base_color_sampler, in.uv).rgb;
    let lit = albedo * (ambient + diffuse * 0.85);
    return vec4<f32>(lit, 1.0);
}
