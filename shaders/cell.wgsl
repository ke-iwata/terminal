struct Uniforms {
    screen_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct VertexInput {
    @location(0) corner: vec2<f32>,
};

struct InstanceInput {
    @location(1) pos: vec2<f32>,
    @location(2) size: vec2<f32>,
    @location(3) uv_min: vec2<f32>,
    @location(4) uv_max: vec2<f32>,
    @location(5) color: vec4<f32>,
    // >0 rounds this quad's top-left/top-right corners (bottom stays
    // square) -- used for the Chrome-style tab strip; every other quad
    // (cells, glyphs, cursor, flat chrome bars) passes 0.
    @location(6) top_corner_radius: f32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) local_pos: vec2<f32>,
    @location(3) size: vec2<f32>,
    @location(4) top_corner_radius: f32,
};

@vertex
fn vs_main(vert: VertexInput, inst: InstanceInput) -> VertexOutput {
    let px = inst.pos + vert.corner * inst.size;
    let ndc_x = (px.x / uniforms.screen_size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (px.y / uniforms.screen_size.y) * 2.0;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.uv = mix(inst.uv_min, inst.uv_max, vert.corner);
    out.color = inst.color;
    out.local_pos = vert.corner * inst.size;
    out.size = inst.size;
    out.top_corner_radius = inst.top_corner_radius;
    return out;
}

// Antialiased coverage (0..1) of a disc of radius `r` centered at `center`,
// sampled at `p` -- a ~1px soft edge instead of a hard discard.
fn disc_coverage(p: vec2<f32>, center: vec2<f32>, r: f32) -> f32 {
    let d = distance(p, center);
    return 1.0 - smoothstep(r - 1.0, r + 1.0, d);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let coverage = textureSample(atlas_tex, atlas_sampler, in.uv).r;
    var mask = 1.0;
    let r = in.top_corner_radius;
    if (r > 0.0) {
        let p = in.local_pos;
        if (p.x < r && p.y < r) {
            mask = disc_coverage(p, vec2<f32>(r, r), r);
        } else if (p.x > in.size.x - r && p.y < r) {
            mask = disc_coverage(p, vec2<f32>(in.size.x - r, r), r);
        }
    }
    return vec4<f32>(in.color.rgb, in.color.a * coverage * mask);
}
