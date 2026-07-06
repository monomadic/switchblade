// Instanced rounded-rect tiles. One quad per tile, SDF corners + border ring.

struct Uniforms {
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct Inst {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border: vec4<f32>,
    @location(4) radius: f32,
    @location(5) border_width: f32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border: vec4<f32>,
    @location(4) radius: f32,
    @location(5) border_width: f32,
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
    return out;
}

fn sd_round_rect(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let p = in.local - in.size * 0.5;
    let r = min(in.radius, min(in.size.x, in.size.y) * 0.5);
    let d = sd_round_rect(p, in.size * 0.5, r);
    let aa = 1.0;
    let fill = 1.0 - smoothstep(-aa, aa, d);
    var col = in.color;
    if (in.border_width > 0.0) {
        let ring = 1.0 - smoothstep(-aa, aa, abs(d + in.border_width * 0.5) - in.border_width * 0.5);
        col = vec4<f32>(mix(col.rgb, in.border.rgb, ring * in.border.a), col.a);
    }
    return vec4<f32>(col.rgb, col.a * fill);
}
