// Instanced rounded-rect tiles: SDF corners, border ring, and thumbnail
// sampling from a fixed-slot atlas (slot -1 = flat placeholder color).

// Must match ATLAS_* consts in lib.rs.
const ATLAS_COLS: f32 = 8.0;
const ATLAS_ROWS: f32 = 15.0;
const SLOT_W: f32 = 480.0;
const SLOT_H: f32 = 270.0;

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
    @location(6) tex_slot: f32,
    @location(7) tex_mix: f32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border: vec4<f32>,
    @location(4) radius: f32,
    @location(5) border_width: f32,
    @location(6) tex_slot: f32,
    @location(7) tex_mix: f32,
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
    out.tex_slot = inst.tex_slot;
    out.tex_mix = inst.tex_mix;
    return out;
}

fn sd_round_rect(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Atlas UV for this fragment: slot origin + fraction across the tile,
    // inset half a texel so linear filtering never bleeds a neighbor slot.
    let slot = max(in.tex_slot, 0.0);
    let scol = slot % ATLAS_COLS;
    let srow = floor(slot / ATLAS_COLS);
    let frac = clamp(in.local / in.size, vec2<f32>(0.0), vec2<f32>(1.0));
    let atlas_px = vec2<f32>(ATLAS_COLS * SLOT_W, ATLAS_ROWS * SLOT_H);
    let uv0 = vec2<f32>(scol * SLOT_W, srow * SLOT_H) + vec2<f32>(0.5);
    let uv1 = vec2<f32>((scol + 1.0) * SLOT_W, (srow + 1.0) * SLOT_H) - vec2<f32>(0.5);
    // Sampled unconditionally: textureSample requires uniform control flow.
    let tex = textureSample(atlas, samp, mix(uv0, uv1, frac) / atlas_px);
    let use_tex = select(0.0, clamp(in.tex_mix, 0.0, 1.0), in.tex_slot >= 0.0);

    let p = in.local - in.size * 0.5;
    let r = min(in.radius, min(in.size.x, in.size.y) * 0.5);
    let d = sd_round_rect(p, in.size * 0.5, r);
    let aa = 1.0;
    let fill = 1.0 - smoothstep(-aa, aa, d);

    var rgb = mix(in.color.rgb, tex.rgb, use_tex);
    if (in.border_width > 0.0) {
        let ring = 1.0 - smoothstep(-aa, aa, abs(d + in.border_width * 0.5) - in.border_width * 0.5);
        rgb = mix(rgb, in.border.rgb, ring * in.border.a);
    }
    return vec4<f32>(rgb, in.color.a * fill);
}
