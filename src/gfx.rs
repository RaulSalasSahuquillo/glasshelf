//! EGL + GLES 3.0: shared context, shader programs and drawing primitives.
use anyhow::{anyhow, Result};
use glow::HasContext;
use khronos_egl as egl;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

// EGL
pub struct EglCtx {
    pub inst: egl::DynamicInstance<egl::EGL1_4>,
    pub display: egl::Display,
    pub config: egl::Config,
    pub context: egl::Context,
}

impl EglCtx {
    /// # Safety
    /// `wl_display` must be a valid `wl_display*` for the whole lifetime of the context.
    pub unsafe fn new(wl_display: *mut c_void) -> Result<Self> {
        let inst = egl::DynamicInstance::<egl::EGL1_4>::load_required()
            .map_err(|e| anyhow!("could not load libEGL: {e:?}"))?;
        let display = inst
            .get_display(wl_display)
            .ok_or_else(|| anyhow!("eglGetDisplay failed"))?;
        inst.initialize(display)?;
        inst.bind_api(egl::OPENGL_ES_API)?;
        let attribs = [
            egl::SURFACE_TYPE, egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE, egl::OPENGL_ES3_BIT,
            egl::RED_SIZE, 8, egl::GREEN_SIZE, 8, egl::BLUE_SIZE, 8, egl::ALPHA_SIZE, 8,
            egl::NONE,
        ];
        let config = inst
            .choose_first_config(display, &attribs)?
            .ok_or_else(|| anyhow!("no RGBA8 + GLES3 EGL config available"))?;
        let ctx_attribs = [egl::CONTEXT_MAJOR_VERSION, 3, egl::CONTEXT_MINOR_VERSION, 0, egl::NONE];
        let context = inst.create_context(display, config, None, &ctx_attribs)?;
        Ok(EglCtx { inst, display, config, context })
    }

    /// # Safety
    /// `native_window` must be a valid `wl_egl_window*`.
    pub unsafe fn create_surface(&self, native_window: *mut c_void) -> Result<egl::Surface> {
        let s = self.inst.create_window_surface(self.display, self.config, native_window, None)?;
        Ok(s)
    }

    pub fn make_current(&self, s: egl::Surface) -> Result<()> {
        self.inst.make_current(self.display, Some(s), Some(s), Some(self.context))?;
        Ok(())
    }

    pub fn swap(&self, s: egl::Surface) -> Result<()> {
        self.inst.swap_buffers(self.display, s)?;
        Ok(())
    }

    pub fn set_swap_interval0(&self) {
        let _ = self.inst.swap_interval(self.display, 0);
    }

    pub fn destroy_surface(&self, s: egl::Surface) {
        let _ = self.inst.destroy_surface(self.display, s);
    }

    pub fn proc_address(&self, name: &str) -> *const c_void {
        self.inst.get_proc_address(name).map_or(std::ptr::null(), |p| p as *const c_void)
    }
}

// shaders
pub const VERT: &str = r#"#version 300 es
precision highp float;
uniform vec2 u_surface;
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
out vec2 v_px;
out vec2 v_uv;
void main() {
    v_px = a_pos;
    v_uv = a_uv;
    vec2 c = a_pos / u_surface * 2.0 - 1.0;
    gl_Position = vec4(c.x, -c.y, 0.0, 1.0);
}
"#;

pub const TEX_FRAG: &str = r#"#version 300 es
precision highp float;
precision highp sampler2D;
uniform sampler2D u_tex;
uniform vec4 u_color;   // premultiplied
uniform float u_mask;   // 1 = use only the texture alpha as a mask
in vec2 v_uv;
out vec4 o_color;
void main() {
    vec4 t = texture(u_tex, v_uv);
    o_color = (u_mask > 0.5) ? (u_color * t.a) : (t * u_color);
}
"#;

pub const SHAPE_FRAG: &str = r#"#version 300 es
precision highp float;
uniform vec4 u_rect;       // x, y, w, h (px)
uniform float u_radius;
uniform vec4 u_fill;       // premultiplied
uniform vec4 u_stroke;     // premultiplied
uniform float u_stroke_w;
in vec2 v_px;
out vec4 o_color;
float sd_round_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
void main() {
    vec2 c = u_rect.xy + u_rect.zw * 0.5;
    float d = sd_round_box(v_px - c, u_rect.zw * 0.5, u_radius);
    float outer = clamp(0.5 - d, 0.0, 1.0);
    float inner = clamp(0.5 - (d + u_stroke_w), 0.0, 1.0);
    o_color = u_fill * inner + u_stroke * (outer - inner);
}
"#;

/// Fallback glass (same uniform contract as the real shader).
pub const GLASS_FALLBACK_FRAG: &str = r#"#version 300 es
precision highp float;
precision highp sampler2D;
uniform vec2 u_surface;
uniform vec4 u_panel;
uniform float u_radius;
uniform float u_scale;
uniform vec2 u_sample_shift;
uniform sampler2D u_backdrop;
uniform vec3 u_pointer;
uniform float u_lum;
uniform float u_time;
in vec2 v_px;
out vec4 o_color;
float sd_round_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
void main() {
    vec2 c = u_panel.xy + u_panel.zw * 0.5;
    float d = sd_round_box(v_px - c, u_panel.zw * 0.5, u_radius);   // < 0 inside
    float cov = clamp(0.5 - d, 0.0, 1.0);
    float depth = -d;                                               // distance to the edge, inwards
    float bev = 18.0 * u_scale;
    float t = clamp(1.0 - depth / bev, 0.0, 1.0);
    vec2 e = vec2(1.0, 0.0);
    vec2 g = vec2(
        sd_round_box(v_px - c + vec2(1.0, 0.0), u_panel.zw * 0.5, u_radius) - sd_round_box(v_px - c - vec2(1.0, 0.0), u_panel.zw * 0.5, u_radius),
        sd_round_box(v_px - c + vec2(0.0, 1.0), u_panel.zw * 0.5, u_radius) - sd_round_box(v_px - c - vec2(0.0, 1.0), u_panel.zw * 0.5, u_radius));
    vec2 n = normalize(g + 1e-5);
    vec2 uv = (v_px + u_sample_shift - n * (t * t) * 22.0 * u_scale) / u_surface;
    vec3 col = textureLod(u_backdrop, uv, 1.0).rgb;
    col = mix(col, vec3(0.1, 0.11, 0.16), 0.20);
    float rim = smoothstep(2.0 * u_scale, 0.0, depth);
    col += rim * 0.35;
    float a = 0.90 * cov;
    // shadow
    float sh = exp(-pow(max(d, 0.0) / (10.0 * u_scale), 2.0)) * 0.30 * (1.0 - cov);
    o_color = vec4(col * a, a) + vec4(0.0, 0.0, 0.0, sh);
}
"#;

pub struct Prog {
    pub p: glow::Program,
    locs: RefCell<HashMap<String, Option<glow::UniformLocation>>>,
}

impl Prog {
    pub fn loc(&self, gl: &glow::Context, name: &str) -> Option<glow::UniformLocation> {
        let mut m = self.locs.borrow_mut();
        if let Some(l) = m.get(name) {
            return l.clone();
        }
        let l = unsafe { gl.get_uniform_location(self.p, name) };
        m.insert(name.to_string(), l.clone());
        l
    }
}

pub struct Tex {
    pub id: glow::Texture,
    pub w: i32,
    pub h: i32,
}

pub struct Gfx {
    pub gl: glow::Context,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    pub glass: Prog,
    pub shape: Prog,
    pub tex: Prog,
    pub glass_is_fallback: bool,
}

fn compile(gl: &glow::Context, vs: &str, fs: &str) -> Result<Prog> {
    unsafe {
        let p = gl.create_program().map_err(|e| anyhow!(e))?;
        let mut shaders = vec![];
        for (kind, src) in [(glow::VERTEX_SHADER, vs), (glow::FRAGMENT_SHADER, fs)] {
            let s = gl.create_shader(kind).map_err(|e| anyhow!(e))?;
            gl.shader_source(s, src);
            gl.compile_shader(s);
            if !gl.get_shader_compile_status(s) {
                let log = gl.get_shader_info_log(s);
                gl.delete_shader(s);
                return Err(anyhow!("shader: {log}"));
            }
            gl.attach_shader(p, s);
            shaders.push(s);
        }
        gl.bind_attrib_location(p, 0, "a_pos");
        gl.bind_attrib_location(p, 1, "a_uv");
        gl.link_program(p);
        if !gl.get_program_link_status(p) {
            return Err(anyhow!("link: {}", gl.get_program_info_log(p)));
        }
        for s in shaders {
            gl.detach_shader(p, s);
            gl.delete_shader(s);
        }
        Ok(Prog { p, locs: RefCell::new(HashMap::new()) })
    }
}

impl Gfx {
    pub fn new(gl: glow::Context, glass_vs: Option<&str>, glass_fs: Option<&str>) -> Result<Gfx> {
        let (vao, vbo) = unsafe {
            let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            let vbo = gl.create_buffer().map_err(|e| anyhow!(e))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_size(glow::ARRAY_BUFFER, 6 * 4 * 4, glow::DYNAMIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
            gl.enable(glow::BLEND);
            gl.blend_func_separate(glow::ONE, glow::ONE_MINUS_SRC_ALPHA, glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            (vao, vbo)
        };
        let shape = compile(&gl, VERT, SHAPE_FRAG)?;
        let tex = compile(&gl, VERT, TEX_FRAG)?;
        let mut glass_is_fallback = false;
        let glass = match (glass_vs, glass_fs) {
            (Some(vs), Some(fs)) => match compile(&gl, vs, fs) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("glass shader failed to compile ({e}); using the fallback");
                    glass_is_fallback = true;
                    compile(&gl, VERT, GLASS_FALLBACK_FRAG)?
                }
            },
            _ => {
                glass_is_fallback = true;
                compile(&gl, VERT, GLASS_FALLBACK_FRAG)?
            }
        };
        Ok(Gfx { gl, vao, vbo, glass, shape, tex, glass_is_fallback })
    }

    /// Hot-swaps the glass shader (handy for iterating without a restart). Returns the compile error, if any.
    pub fn set_glass(&mut self, vs: &str, fs: &str) -> Result<()> {
        let p = compile(&self.gl, vs, fs)?;
        unsafe { self.gl.delete_program(self.glass.p) };
        self.glass = p;
        self.glass_is_fallback = false;
        Ok(())
    }

    pub fn begin_frame(&self, w: i32, h: i32) {
        unsafe {
            self.gl.viewport(0, 0, w, h);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
        }
    }

    fn quad(&self, x0: f32, y0: f32, x1: f32, y1: f32, uv: [f32; 4]) {
        let [u0, v0, u1, v1] = uv;
        let v: [f32; 24] = [
            x0, y0, u0, v0, x1, y0, u1, v0, x0, y1, u0, v1,
            x1, y0, u1, v0, x1, y1, u1, v1, x0, y1, u0, v1,
        ];
        unsafe {
            self.gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytemuck::cast_slice(&v));
            self.gl.draw_arrays(glow::TRIANGLES, 0, 6);
        }
    }

    pub fn use_prog(&self, p: &Prog, surf: (f32, f32)) {
        unsafe {
            self.gl.use_program(Some(p.p));
            if let Some(l) = p.loc(&self.gl, "u_surface") {
                self.gl.uniform_2_f32(Some(&l), surf.0, surf.1);
            }
        }
    }

    pub fn u1(&self, p: &Prog, n: &str, v: f32) {
        if let Some(l) = p.loc(&self.gl, n) {
            unsafe { self.gl.uniform_1_f32(Some(&l), v) }
        }
    }
    pub fn u2(&self, p: &Prog, n: &str, a: f32, b: f32) {
        if let Some(l) = p.loc(&self.gl, n) {
            unsafe { self.gl.uniform_2_f32(Some(&l), a, b) }
        }
    }
    pub fn u3(&self, p: &Prog, n: &str, a: f32, b: f32, c: f32) {
        if let Some(l) = p.loc(&self.gl, n) {
            unsafe { self.gl.uniform_3_f32(Some(&l), a, b, c) }
        }
    }
    pub fn u4(&self, p: &Prog, n: &str, a: f32, b: f32, c: f32, d: f32) {
        if let Some(l) = p.loc(&self.gl, n) {
            unsafe { self.gl.uniform_4_f32(Some(&l), a, b, c, d) }
        }
    }

    // primitives
    /// Rounded rectangle (premultiplied fill and stroke).
    pub fn shape(&self, surf: (f32, f32), r: [f32; 4], radius: f32, fill: [f32; 4], stroke: [f32; 4], sw: f32) {
        self.use_prog(&self.shape, surf);
        self.u4(&self.shape, "u_rect", r[0], r[1], r[2], r[3]);
        self.u1(&self.shape, "u_radius", radius);
        self.u4(&self.shape, "u_fill", fill[0], fill[1], fill[2], fill[3]);
        self.u4(&self.shape, "u_stroke", stroke[0], stroke[1], stroke[2], stroke[3]);
        self.u1(&self.shape, "u_stroke_w", sw);
        self.quad(r[0] - 2.0, r[1] - 2.0, r[0] + r[2] + 2.0, r[1] + r[3] + 2.0, [0.0; 4]);
    }

    /// Textured quad at `(x, y, w, h)`; with `mask`, only the alpha is used and tinted with `color`.
    pub fn texture(&self, surf: (f32, f32), t: &Tex, rect: [f32; 4], color: [f32; 4], mask: bool) {
        self.use_prog(&self.tex, surf);
        unsafe {
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(t.id));
        }
        if let Some(l) = self.tex.loc(&self.gl, "u_tex") {
            unsafe { self.gl.uniform_1_i32(Some(&l), 0) }
        }
        self.u4(&self.tex, "u_color", color[0], color[1], color[2], color[3]);
        self.u1(&self.tex, "u_mask", if mask { 1.0 } else { 0.0 });
        self.quad(rect[0], rect[1], rect[0] + rect[2], rect[1] + rect[3], [0.0, 0.0, 1.0, 1.0]);
    }

    /// Glass capsule (with its drop shadow) drawn over `quad_rect`.
    #[allow(clippy::too_many_arguments)]
    pub fn glass(
        &self, surf: (f32, f32), backdrop: &Tex, panel: [f32; 4], quad_rect: [f32; 4], scale: f32,
        sample_shift: (f32, f32), pointer: (f32, f32, f32), lum: f32, time: f32,
    ) {
        let g = &self.glass;
        self.use_prog(g, surf);
        unsafe {
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(backdrop.id));
        }
        if let Some(l) = g.loc(&self.gl, "u_backdrop") {
            unsafe { self.gl.uniform_1_i32(Some(&l), 0) }
        }
        self.u4(g, "u_panel", panel[0], panel[1], panel[2], panel[3]);
        self.u1(g, "u_radius", panel[3] * 0.5);
        self.u1(g, "u_scale", scale);
        self.u2(g, "u_sample_shift", sample_shift.0, sample_shift.1);
        self.u3(g, "u_pointer", pointer.0, pointer.1, pointer.2);
        self.u1(g, "u_lum", lum);
        self.u1(g, "u_time", time);
        self.quad(quad_rect[0], quad_rect[1], quad_rect[0] + quad_rect[2], quad_rect[1] + quad_rect[3], [0.0; 4]);
    }

    // ---- textures
    pub fn upload_rgba(&self, w: i32, h: i32, data: &[u8], mipmaps: bool) -> Result<Tex> {
        unsafe {
            let id = self.gl.create_texture().map_err(|e| anyhow!(e))?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(id));
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            self.gl.tex_image_2d(
                glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(data)),
            );
            self.set_filters(mipmaps);
            if mipmaps {
                self.gl.generate_mipmap(glow::TEXTURE_2D);
            }
            Ok(Tex { id, w, h })
        }
    }

    /// Reuses the texture if the size matches, otherwise recreates it.
    pub fn update_rgba(&self, t: &mut Option<Tex>, w: i32, h: i32, data: &[u8], mipmaps: bool) -> Result<()> {
        unsafe {
            if let Some(old) = t.as_ref() {
                if old.w == w && old.h == h {
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(old.id));
                    self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                    self.gl.tex_sub_image_2d(
                        glow::TEXTURE_2D, 0, 0, 0, w, h, glow::RGBA, glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(data)),
                    );
                    if mipmaps {
                        self.gl.generate_mipmap(glow::TEXTURE_2D);
                    }
                    return Ok(());
                }
            }
            if let Some(old) = t.take() {
                self.gl.delete_texture(old.id);
            }
        }
        *t = Some(self.upload_rgba(w, h, data, mipmaps)?);
        Ok(())
    }

    unsafe fn set_filters(&self, mipmaps: bool) {
        let min = if mipmaps { glow::LINEAR_MIPMAP_LINEAR } else { glow::LINEAR };
        self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, min as i32);
        self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
    }

    pub fn delete_tex(&self, t: Tex) {
        unsafe { self.gl.delete_texture(t.id) }
    }
}
