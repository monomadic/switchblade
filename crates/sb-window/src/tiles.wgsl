// Instanced rounded-rect tiles: SDF corners, border ring, and thumbnail
// sampling via per-instance atlas UV rects (zero-size uv = flat
// placeholder color).

struct Uniforms {
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct Inst {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border: vec4<f32>,
    @location(4) radius: f32,
    @location(5) border_width: f32,
    @location(6) tex_mix: f32,
    @location(7) frame_fade: f32,
    @location(8) uv: vec4<f32>,
    @location(9) uv2: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border: vec4<f32>,
    @location(4) radius: f32,
    @location(5) border_width: f32,
    @location(6) tex_mix: f32,
    @location(7) frame_fade: f32,
    @location(8) uv: vec4<f32>,
    @location(9) uv2: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Inst) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let c = corners[vi];
    let px = inst.pos + c * inst.size;
    let ndc = vec2<f32>(
        px.x / u.viewport.x * 2.0 - 1.0,
        1.0 - px.y / u.viewport.y * 2.0,
    );

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.local = c * inst.size;
    out.size = inst.size;
    out.color = inst.color;
    out.border = inst.border;
    out.radius = inst.radius;
    out.border_width = inst.border_width;
    out.tex_mix = inst.tex_mix;
    out.frame_fade = inst.frame_fade;
    out.uv = inst.uv;
    out.uv2 = inst.uv2;
    return out;
}

fn sd_round_rect(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let frac = clamp(in.local / in.size, vec2<f32>(0.0), vec2<f32>(1.0));
    // Sampled unconditionally: textureSample requires uniform control flow.
    // Two taps let anim-sheet frames crossfade (frame_fade blends uv→uv2).
    let tex_a = textureSample(atlas, samp, in.uv.xy + frac * in.uv.zw);
    let tex_b = textureSample(atlas, samp, in.uv2.xy + frac * in.uv2.zw);
    let tex = mix(tex_a, tex_b, clamp(in.frame_fade, 0.0, 1.0));
    let use_tex = select(0.0, clamp(in.tex_mix, 0.0, 1.0), in.uv.z > 0.0);

    let p = in.local - in.size * 0.5;
    let r = min(in.radius, min(in.size.x, in.size.y) * 0.5);
    let d = sd_round_rect(p, in.size * 0.5, r);
    let aa = 1.0;
    let fill = 1.0 - smoothstep(-aa, aa, d);

    var ring = 0.0;
    if (in.border_width > 0.0) {
        ring = (1.0 - smoothstep(-aa, aa, abs(d + in.border_width * 0.5) - in.border_width * 0.5))
            * in.border.a;
    }
    let rgb = mix(mix(in.color.rgb, tex.rgb, use_tex), in.border.rgb, ring);

    // Border alpha is independent of fill alpha, so a transparent tile can
    // still draw just its outline.
    return vec4<f32>(rgb, max(in.color.a, ring) * fill);
}
