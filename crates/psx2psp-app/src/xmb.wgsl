// XMB preview compositor. Virtual PSP screen: 480x272. Textures are Rgba8UnormSrgb (sampled
// as linear, straight alpha); all maths in linear space, encoded at the end if the target is UNORM.

struct U {
    time: f32,
    blend: f32,      // 0 → prev screen, 1 → current screen (eased on the CPU)
    mode: u32,       // 0 game, 1 save, 2 boot
    prev: u32,
    accent: vec4<f32>,   // sRGB wave tint
    fade: vec4<f32>,     // per-layer opacity: pic1, pic0, icon0, boot
    size: vec2<f32>,     // physical pixel size of the widget
    srgb_target: f32,
    radius: f32,         // corner radius, physical px
};

@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var t_pic1: texture_2d<f32>;
@group(0) @binding(3) var t_pic0: texture_2d<f32>;
@group(0) @binding(4) var t_icon0: texture_2d<f32>;
@group(0) @binding(5) var t_boot: texture_2d<f32>;

const SCREEN: vec2<f32> = vec2<f32>(480.0, 272.0);
const ICON0_AT: vec2<f32> = vec2<f32>(20.0, 96.0);
const ICON0_SIZE: vec2<f32> = vec2<f32>(144.0, 80.0);
const PIC0_AT: vec2<f32> = vec2<f32>(160.0, 46.0);
const PIC0_SIZE: vec2<f32> = vec2<f32>(310.0, 180.0);

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Single oversized triangle; iced sets the viewport to the widget bounds.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VOut;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

fn lin(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.2));
}

// Gaussian ribbon following two superposed sines; returns intensity at p.
fn ribbon(p: vec2<f32>, t: f32, freq: f32, speed: f32, amp: f32, phase: f32, y0: f32, width: f32) -> f32 {
    let x = p.x / SCREEN.x;
    let y = y0 + amp * (0.62 * sin(x * freq + t * speed + phase)
                      + 0.38 * sin(x * freq * 1.73 - t * speed * 0.61 + phase * 2.1));
    let d = (p.y / SCREEN.y - y) / width;
    return exp(-d * d);
}

fn xmb_background(p: vec2<f32>, t: f32) -> vec3<f32> {
    let base = lin(u.accent.rgb);
    let v = p.y / SCREEN.y;
    // Top-lit vertical gradient + soft vignette, like the PSP's monthly theme.
    var col = base * mix(1.25, 0.28, smoothstep(0.0, 1.0, v));
    let c = (p / SCREEN - vec2<f32>(0.5, 0.45)) * vec2<f32>(1.0, 1.6);
    col *= 1.0 - 0.45 * dot(c, c);

    // Translucent sheet: filled band between two slow waves.
    let x = p.x / SCREEN.x;
    let top = 0.52 + 0.07 * sin(x * 3.1 + t * 0.21) + 0.03 * sin(x * 7.3 - t * 0.33);
    let bot = 0.66 + 0.05 * sin(x * 2.4 - t * 0.17 + 1.3);
    let sheet = smoothstep(top - 0.01, top + 0.04, v) * (1.0 - smoothstep(bot - 0.06, bot + 0.02, v));
    col += vec3<f32>(0.10) * sheet;

    // Glowing ribbons (one wide haze, three crisp strands).
    var w = 0.0;
    w += 0.22 * ribbon(p, t, 4.2, 0.27, 0.090, 1.7, 0.58, 0.060);
    w += 0.55 * ribbon(p, t, 5.0, 0.35, 0.080, 0.0, 0.55, 0.010);
    w += 0.80 * ribbon(p, t, 6.1, 0.42, 0.070, 3.1, 0.53, 0.004);
    w += 0.45 * ribbon(p, t, 3.6, 0.23, 0.110, 4.6, 0.60, 0.007);
    col += vec3<f32>(w) * mix(vec3<f32>(1.0), base * 2.0 + 0.4, 0.25);
    return col;
}

fn sprite(tex: texture_2d<f32>, p: vec2<f32>, at: vec2<f32>, size: vec2<f32>) -> vec4<f32> {
    let q = (p - at) / size;
    // Sample unconditionally (uniform control flow), mask afterwards.
    let s = textureSampleLevel(tex, smp, clamp(q, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
    let inside = step(0.0, q.x) * step(0.0, q.y) * step(q.x, 1.0) * step(q.y, 1.0);
    return s * inside;
}

fn over(dst: vec3<f32>, src: vec4<f32>, k: f32) -> vec3<f32> {
    return mix(dst, src.rgb, clamp(src.a * k, 0.0, 1.0));
}

fn screen(mode: u32, p: vec2<f32>, t: f32) -> vec3<f32> {
    if mode == 2u {
        // Boot: STARTDAT image on black, as POPS shows it while loading.
        return over(vec3<f32>(0.0), sprite(t_boot, p, vec2<f32>(0.0), SCREEN), u.fade.w);
    }
    var col = xmb_background(p, t);
    let pic1 = sprite(t_pic1, p, vec2<f32>(0.0), SCREEN);
    if mode == 0u {
        col = over(col, pic1, u.fade.x);
        col = over(col, sprite(t_pic0, p, PIC0_AT, PIC0_SIZE), u.fade.y);
    } else {
        // Saved Data Utility: dimmed PIC1, highlighted slot behind the icon.
        col = over(col, vec4<f32>(pic1.rgb * 0.40, pic1.a), u.fade.x);
        let d = abs(p - (ICON0_AT + ICON0_SIZE * 0.5)) - (ICON0_SIZE * 0.5 + vec2<f32>(6.0));
        let box_d = length(max(d, vec2<f32>(0.0))) + min(max(d.x, d.y), 0.0) - 6.0;
        let pulse = 0.75 + 0.25 * sin(t * 3.0);
        col += vec3<f32>(0.10) * (1.0 - smoothstep(-1.0, 1.0, box_d));
        col += vec3<f32>(0.55 * pulse) * exp(-abs(box_d) * 0.9);
    }
    // ICON0 gently "breathes" when selected, like the XMB focus animation.
    let scale = 1.0 + 0.015 * sin(t * 2.2);
    let size = ICON0_SIZE * scale;
    let at = ICON0_AT - (size - ICON0_SIZE) * 0.5;
    col = over(col, sprite(t_icon0, p, at, size), u.fade.z);
    return col;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let p = in.uv * SCREEN;
    let a = screen(u.prev, p, u.time);
    let b = screen(u.mode, p, u.time);
    var col = mix(a, b, u.blend);
    if u.srgb_target < 0.5 {
        col = pow(max(col, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2));
    }
    // Rounded-corner coverage in physical pixels (premultiplied output).
    let px = in.uv * u.size;
    let half = u.size * 0.5;
    let q = abs(px - half) - (half - vec2<f32>(u.radius));
    let dist = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - u.radius;
    let cov = clamp(0.5 - dist, 0.0, 1.0);
    return vec4<f32>(col * cov, cov);
}
