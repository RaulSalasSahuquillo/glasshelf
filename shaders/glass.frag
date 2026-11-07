#version 300 es
// GlassDock - liquid glass capsule (fragment shader, GLSL ES 3.00)
//
// One quad covers the capsule plus a shadow margin. Inside the capsule the pixel is a displaced
// sample of a screenshot of the backdrop (a thick lensed bevel with a mirror-fold, chromatic
// dispersion, a light frost in the body), lit by a key light (top-left) and a weaker counter
// light (bottom-right); outside it is an analytic drop shadow. Output is PREMULTIPLIED alpha.
// All look constants are authored in LOGICAL pixels and multiplied by u_scale.
//
// Uniform contract (set by src/gfx.rs):
//   u_surface      surface size in device px
//   u_panel        capsule rect (x, y, w, h) in device px, where it is drawn right now
//   u_radius       corner radius in px (= h / 2)
//   u_scale        device px per logical px (1.0, 1.6, 2.0 ...)
//   u_sample_shift added to the backdrop sampling position, so a sliding dock keeps sampling
//                  as if it were at its resting position
//   u_backdrop     RGBA8 snapshot of the area behind the surface, mipmapped, origin top-left
//   u_pointer      (x, y) in device px and a 0..1 strength of the hover light
//   u_lum          mean luminance (0..1) of the backdrop behind the panel
precision highp float;
precision highp sampler2D;

uniform vec2  u_surface;
uniform vec4  u_panel;
uniform float u_radius;
uniform float u_scale;
uniform vec2  u_sample_shift;
uniform sampler2D u_backdrop;
uniform vec3  u_pointer;
uniform float u_lum;
uniform float u_time;

in vec2 v_px;
out vec4 o_color;

// ---- look constants -------------------------------------------------------
const float BEVEL     = 21.0;   // lens rim thickness
const float FOLD      = 1.05;   // peak inward sample displacement / BEVEL
const float FOLD_K    = 2.0;    // falloff exponent of the displacement
const float DISP      = 0.022;   // chromatic spread of the displacement (+-)
const vec2  LIGHT     = vec2(-0.60, -0.80);  // key light direction (towards light, y down)

float sat1(float x) { return clamp(x, 0.0, 1.0); }
float sq(float x) { return x * x; }

float capsuleSd(vec2 p, vec2 c, vec2 hb, float r) {
    vec2 q = abs(p - c) - (hb - r);
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

vec3 saturate3(vec3 c, float s) {
    float l = dot(c, vec3(0.299, 0.587, 0.114));
    return mix(vec3(l), c, s);
}

void main() {
    float S = u_scale;
    vec2 p = v_px;
    vec2 c = u_panel.xy + 0.5 * u_panel.zw;
    vec2 hb = 0.5 * u_panel.zw;
    float r = u_radius;

    // ---- signed distance + outward normal --------------------------------
    vec2 pc = p - c;
    vec2 q = abs(pc) - (hb - r);
    vec2 mq = max(q, 0.0);
    float ml = length(mq);
    float d = ml + min(max(q.x, q.y), 0.0) - r;          // <0 inside
    vec2 g = ml > 1e-4 ? mq / ml : (q.x > q.y ? vec2(1.0, 0.0) : vec2(0.0, 1.0));
    g *= vec2(pc.x < 0.0 ? -1.0 : 1.0, pc.y < 0.0 ? -1.0 : 1.0);   // outward
    vec2 medial = clamp(pc, -(hb - r), hb - r);
    vec2 vmed = pc - medial;                               // from medial axis, |vmed| = r + d

    if (d > 34.0 * S) { o_color = vec4(0.0); return; }
    float cov = sat1(0.5 - d);
    float lum = u_lum;
    float bright = smoothstep(0.45, 0.92, lum);
    float dark = 1.0 - smoothstep(0.04, 0.42, lum);

    // ---- outside: shadow + hairline ---------------------------------------
    float a_out = 0.0;
    if (cov < 1.0) {
        float Rs = mix(26.0, 15.0, bright) * S;
        float dsh = capsuleSd(p - vec2(0.0, mix(6.0, 3.5, bright) * S), c, hb, r);
        float sh = pow(1.0 - sat1(dsh / Rs), 2.4) * mix(0.34, 0.15, bright);
        float hair = (1.0 - smoothstep(0.0, 1.3 * S, d)) * mix(0.16, 0.30, bright);
        a_out = 1.0 - (1.0 - sh) * (1.0 - hair);
    }
    if (cov <= 0.0) {
        float n = fract(52.9829189 * fract(dot(p, vec2(0.06711056, 0.00583715)))) - 0.5;
        o_color = vec4(0.0, 0.0, 0.0, max(a_out + n * (a_out > 0.0 ? 1.2 / 255.0 : 0.0), 0.0));
        return;
    }

    // ---- lens: height-field style refraction -----------------------------
    float bw = BEVEL * S;
    float t = max(-d, 0.0);                    // depth inside, device px
    float x = sat1(t / bw);                    // 0 at rim .. 1 at end of bevel
    float omx = 1.0 - x;
    float capK = smoothstep(hb.x - r - 6.0 * S, hb.x - r + 10.0 * S, abs(pc.x));   // 1 at the capsule ends
    float Dd = bw * FOLD * pow(omx, FOLD_K) * mix(1.0, 0.65, capK) * mix(1.0, 0.8, bright);   // inward displacement
    float dsdt = abs(1.0 - FOLD * FOLD_K * pow(omx, FOLD_K - 1.0));
    float lodB = 0.85 * log2(max(dsdt, 1.0));  // minification inside the fold

    float hmag = clamp(abs(pc.x) - (hb.x - r), 0.0, r) * (pc.x < 0.0 ? -1.0 : 1.0) * 0.06;
    vec2 base = p + u_sample_shift - vec2(hmag, vmed.y * 0.12);
    vec2 invS = 1.0 / u_surface;
    float dispK = DISP * pow(omx, 1.6);
    vec3 Dc = Dd * vec3(1.0 - dispK, 1.0, 1.0 + dispK);
    vec3 col;
    col.r = textureLod(u_backdrop, (base - g * Dc.r) * invS, lodB).r;
    col.g = textureLod(u_backdrop, (base - g * Dc.g) * invS, lodB).g;
    col.b = textureLod(u_backdrop, (base - g * Dc.b) * invS, lodB).b;

    // light frost in the body (tiny blur), rim stays crisp
    float wf = smoothstep(0.35, 1.0, x) * 0.55;
    float lodF = log2(max(3.0 * S, 1.0));
    vec2 uvg = (base - g * Dc.g) * invS;
    vec2 o = vec2(2.2 * S) * invS;
    vec3 fr = textureLod(u_backdrop, uvg + vec2( o.x,  o.y), lodF).rgb
            + textureLod(u_backdrop, uvg + vec2(-o.x,  o.y), lodF).rgb
            + textureLod(u_backdrop, uvg + vec2( o.x, -o.y), lodF).rgb
            + textureLod(u_backdrop, uvg + vec2(-o.x, -o.y), lodF).rgb;
    col = mix(col, fr * 0.25, wf);

    // soft saturated colour spill sampled from a wide blur just inside the rim
    vec3 spill = textureLod(u_backdrop, (base - g * (9.0 * S)) * invS, log2(max(16.0 * S, 1.0))).rgb;
    spill = saturate3(spill, 1.6);

    // ---- grade -------------------------------------------------------------
    float e = pow(omx, 1.6);                   // bevel emphasis
    col = saturate3(col, mix(1.03, 1.20, e));
    col *= 1.0 + e * (0.045 * (1.0 - 0.6 * bright) + 0.05 * dark);
    col = (col - 0.5) * mix(1.0, 1.10, e) + 0.5;
    col *= 1.0 - 0.06 * bright;
    col *= mix(vec3(1.0), vec3(0.985, 0.992, 1.0), bright);
    col += vec3(0.022 + 0.03 * dark) * (1.0 - 0.7 * bright);

    // ---- light ---------------------------------------------------------------
    vec2 L = normalize(LIGHT);
    float kd = dot(g, L);
    float key = max(kd, 0.0);
    float cnt = max(-kd, 0.0);
    float hiScale = (1.0 - 0.45 * bright) * (1.0 + 0.30 * dark);

    float lw = max(0.75 * S, 0.85);
    float t0 = max(0.85 * S, 1.0);
    float ln = exp(-sq((t - t0) / lw));
    float rimKey = ln * (0.30 + 0.95 * pow(key, 1.3));
    float rimCnt = ln * 0.48 * pow(cnt, 1.5);
    float softKey = exp(-t / (6.5 * S)) * pow(key, 2.0) * 0.20;
    float softCnt = exp(-t / (3.0 * S)) * pow(cnt, 2.0) * 0.16;

    // caustic echo of the fold + inner glow
    float echo = exp(-sq((t - 0.56 * bw) / (1.5 * S))) * (0.045 + 0.10 * key + 0.05 * cnt);
    float glow = exp(-t / (8.0 * S)) * (0.02 + 0.05 * (0.5 - 0.5 * g.y));

    // hover spot
    vec2 dp = p - u_pointer.xy;
    float spot = exp(-dot(dp, dp) / (2.0 * sq(52.0 * S))) * u_pointer.z;

    float glint = (pow(key, 38.0) * 0.85 + pow(cnt, 38.0) * 0.32) * exp(-t / (2.2 * S));
    float hi = (rimKey + rimCnt + softKey + softCnt + echo + glow + glint) * hiScale;
    hi += ln * spot * 0.85 + spot * 0.15 * (1.0 - 0.6 * bright) + softKey * spot * 1.2;
    vec3 rimCol = mix(vec3(1.0), spill, 0.30);
    // inner shadow just inside the highlight (thickness) + gentle edge darkening
    float isd = exp(-sq((t - 4.8 * S) / (2.6 * S)));
    col *= 1.0 - (0.09 + 0.10 * bright) * isd * (0.55 + 0.45 * key);
    col *= 1.0 - 0.11 * pow(omx, 1.8) * (0.45 + 0.55 * cnt);
    // fresnel: bevel brightens towards grazing angles, tinted by spill
    float fres = pow(omx, 3.0);
    col = mix(col, rimCol, 0.10 * fres * hiScale);
    col = 1.0 - (1.0 - col) * (1.0 - clamp(hi, 0.0, 1.0) * rimCol);

    // diagonal gloss streaks across the top half
    vec2 w = pc / S;
    float k = dot(w, normalize(vec2(1.0, -0.62)));
    float topm = smoothstep(0.62, 0.0, (p.y - (c.y - hb.y)) / u_panel.w);
    float gloss = (exp(-sq((k + 190.0) / 60.0)) * 0.07 + exp(-sq((k - 70.0) / 24.0)) * 0.04) * topm;
    col += vec3(gloss) * (1.0 - 0.6 * bright);

    // grain / dither
    float n = fract(52.9829189 * fract(dot(p, vec2(0.06711056, 0.00583715)))) - 0.5;
    col += n * (1.6 / 255.0);

    vec4 glass = vec4(clamp(col, 0.0, 1.0), 1.0);
    o_color = glass * cov + vec4(0.0, 0.0, 0.0, a_out) * (1.0 - cov);
}
