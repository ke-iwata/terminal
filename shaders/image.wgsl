// Draws one RGBA texture into a pixel-space rectangle -- the preview
// overlay's image. Separate from cell.wgsl because the glyph atlas is a
// single-channel coverage texture sampled as `.r`; this samples full
// color, and there is only ever one quad, so the rect comes from a
// uniform instead of an instance buffer.

struct Uniforms {
    screen_size: vec2<f32>,
    // Destination rectangle in physical pixels: top-left, then size.
    rect_pos: vec2<f32>,
    rect_size: vec2<f32>,
    alpha: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var image_tex: texture_2d<f32>;
@group(0) @binding(2) var image_sampler: sampler;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // Two triangles covering the unit square, expanded here rather than
    // uploaded: six constant corners are cheaper to inline than a vertex
    // buffer to bind.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let corner = corners[index];
    let px = uniforms.rect_pos + corner * uniforms.rect_size;
    let ndc_x = (px.x / uniforms.screen_size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (px.y / uniforms.screen_size.y) * 2.0;

    var out: VertexOutput;
    out.clip_position = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(image_tex, image_sampler, in.uv);
    // The image's own alpha is kept (so a transparent PNG shows the
    // backdrop through it) and scaled by the overlay's opacity.
    return vec4<f32>(texel.rgb, texel.a * uniforms.alpha);
}
