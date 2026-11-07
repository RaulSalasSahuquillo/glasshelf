//! GlassDock: a liquid-glass dock for Hyprland with macOS-style magnification (GLES, layer-shell).
//! A single process, one surface per monitor, redraws only on demand (0 % CPU when idle).
mod capture;
mod dock;
mod gfx;
mod i18n;
mod model;
mod raster;

use anyhow::{anyhow, Result};
use i18n::{tr, Msg};
use dock::{Kind, Layout, Slot, ADD_BASE, BOTTOM_GAP, HIDE_DELAY, ICON, PAD_BOTTOM, PANEL_H};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            self,
            generic::Generic,
            timer::{TimeoutAction, Timer},
            EventLoop, Interest, LoopHandle, Mode, PostAction,
        },
        calloop_wayland_source::WaylandSource,
        protocols::wp::{
            cursor_shape::v1::client::{
                wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
                wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
            },
            fractional_scale::v1::client::{
                wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
                wp_fractional_scale_v1::{self, WpFractionalScaleV1},
            },
            viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
        },
        protocols_wlr::screencopy::v1::client::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        WaylandSurface,
    },
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wayland_client::{
    delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_output, wl_pointer, wl_seat, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};

const GLASS_VS: &str = include_str!("../shaders/glass.vert");
const GLASS_FS: &str = include_str!("../shaders/glass.frag");

const C_FG: [f32; 3] = [0.753, 0.792, 0.961];
const C_BLUE: [f32; 3] = [0.478, 0.635, 0.969];
const BACKDROP_MAX_AGE: Duration = Duration::from_millis(1200);
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

// context menu
#[derive(Clone, Debug)]
enum MenuAction {
    NewWindow,
    DesktopAction(String),
    Minimize,
    Close,
    Pin,
    Unpin,
    MoveLeft,
    MoveRight,
}

#[derive(Clone, Debug)]
enum ItemKind {
    Title,
    Sep,
    Item(MenuAction),
}

#[derive(Clone, Debug)]
struct MenuItem {
    label: String,
    kind: ItemKind,
}

struct Menu {
    entry: usize,
    items: Vec<MenuItem>,
    /// x, y, w, h in logical px
    rect: [f32; 4],
    hover: Option<usize>,
}

const MENU_ROW: f32 = 30.0;
const MENU_SEP: f32 = 9.0;
const MENU_PAD: f32 = 8.0;

impl Menu {
    fn row_geom(&self, i: usize) -> (f32, f32) {
        let mut y = self.rect[1] + MENU_PAD;
        for (j, it) in self.items.iter().enumerate() {
            let h = if matches!(it.kind, ItemKind::Sep) { MENU_SEP } else { MENU_ROW };
            if j == i {
                return (y, h);
            }
            y += h;
        }
        (y, 0.0)
    }
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.rect[0] && x <= self.rect[0] + self.rect[2] && y >= self.rect[1] && y <= self.rect[1] + self.rect[3]
    }
    fn item_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.contains(x, y) {
            return None;
        }
        (0..self.items.len()).find(|&i| {
            let (ry, rh) = self.row_geom(i);
            matches!(self.items[i].kind, ItemKind::Item(_)) && y >= ry && y < ry + rh
        })
    }
}

// views (one per monitor)
struct View {
    output: wl_output::WlOutput,
    name: String,
    out_size: (i32, i32), // logical
    layer: LayerSurface,
    viewport: Option<WpViewport>,
    _frac: Option<WpFractionalScaleV1>,
    egl_win: *mut c_void,
    egl_surf: Option<khronos_egl::Surface>,
    scale: f32,
    int_scale: i32,
    buf: (i32, i32),
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    busy: bool,
    last_draw: Instant,
    backdrop: Option<gfx::Tex>,
    lum: f32,
    backdrop_time: Option<Instant>,
    backdrop_stale: bool,
    capturing: bool,
    layout: Layout,
    menu: Option<Menu>,
    menu_close_at: Option<Instant>,
    region_sig: Option<(i32, i32, i32, i32)>,
    draws: u64,
    fake_until: Option<Instant>,
}

#[derive(Clone, Copy)]
struct ViewId(usize);

// screen capture
struct CapState {
    fmt: Option<wl_shm::Format>,
    w: u32,
    h: u32,
    stride: u32,
    y_invert: bool,
    buf: Option<(capture::ShmBuf, wl_buffer::WlBuffer, wl_shm_pool::WlShmPool)>,
}
struct CaptureData {
    view: usize,
    st: Mutex<CapState>,
}

// application
struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    qh: QueueHandle<App>,
    loop_handle: LoopHandle<'static, App>,
    egl: gfx::EglCtx,
    gfx: Option<gfx::Gfx>,
    shm: wl_shm::WlShm,
    screencopy: Option<ZwlrScreencopyManagerV1>,
    frac_mgr: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    cursor_mgr: Option<WpCursorShapeManagerV1>,
    cursor_dev: Option<WpCursorShapeDeviceV1>,
    pointer: Option<wl_pointer::WlPointer>,
    views: Vec<View>,
    view_serial: usize,
    filter: Option<Vec<String>>,
    // model
    catalog: model::Catalog,
    hypr: Option<model::Hypr>,
    parser: model::EventParser,
    pinned: model::Pinned,
    entries: Vec<model::Entry>,
    theme: raster::IconTheme,
    text: Option<raster::TextRenderer>,
    icons: HashMap<String, gfx::Tex>,
    labels: HashMap<(String, u32, bool), (gfx::Tex, i32, i32)>,
    refresh_scheduled: bool,
    add_tx: calloop::channel::Sender<Option<String>>,
    start: Instant,
    quit: bool,
    total_draws: u64,
}

fn dbg_log(msg: &str) {
    if std::env::var_os("GLASSDOCK_DEBUG").is_some() {
        eprintln!("[glassdock] {msg}");
    }
}

/// Where the pinned apps are stored: `$GLASSDOCK_PINNED`, otherwise
/// `$XDG_CONFIG_HOME/glassdock/pinned.json` (default `~/.config/glassdock/pinned.json`).
fn pinned_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("GLASSDOCK_PINNED").filter(|p| !p.is_empty()) {
        return std::path::PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into())).join(".config")
        });
    base.join("glassdock").join("pinned.json")
}

const HELP: &str = "\
GlassDock - a liquid-glass dock for Hyprland

USAGE:
    glassdock [--help | --version]

The dock takes no other arguments; it is configured through environment variables:
    GLASSDOCK_OUTPUTS   comma-separated monitor names to show the dock on (default: all)
    GLASSDOCK_PINNED    path of the pinned-apps JSON (default: ~/.config/glassdock/pinned.json)
    GLASSDOCK_LANG      language of the dock's menus: en or es (default: from your locale)
    GLASSDOCK_DEBUG     log input and capture events to stderr

Run it through `glassdock-launch` to keep it alive, and control it with `glassdockctl`.
";

fn main() -> Result<()> {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("glassdock {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            other => {
                eprintln!("glassdock: unknown argument '{other}' (try --help)");
                std::process::exit(2);
            }
        }
    }
    // Keep a discrete NVIDIA GPU asleep: force Mesa (the iGPU) for EGL unless the user overrides it.
    if std::env::var_os("__EGL_VENDOR_LIBRARY_FILENAMES").is_none() {
        let mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
        if std::path::Path::new(mesa).exists() {
            std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", mesa);
        }
    }
    let filter = std::env::var("GLASSDOCK_OUTPUTS")
        .ok()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>());

    let conn = Connection::connect_to_env().map_err(|e| anyhow!("no Wayland connection: {e}"))?;
    let (globals, event_queue) = registry_queue_init::<App>(&conn).map_err(|e| anyhow!("registry: {e}"))?;
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| anyhow!("wl_compositor: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh).map_err(|e| anyhow!("wlr-layer-shell: {e}"))?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|e| anyhow!("wl_shm: {e}"))?;
    let screencopy = globals.bind::<ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ()).ok();
    let frac_mgr = globals.bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ()).ok();
    let viewporter = globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok();
    let cursor_mgr = globals.bind::<WpCursorShapeManagerV1, _, _>(&qh, 1..=2, ()).ok();
    if screencopy.is_none() {
        eprintln!("warning: wlr-screencopy is not available; the glass cannot refract the backdrop");
    }

    let wl_display = conn.backend().display_ptr() as *mut c_void;
    let egl = unsafe { gfx::EglCtx::new(wl_display)? };

    let catalog = model::Catalog::load();
    let pinned_file = pinned_path();
    if let Some(dir) = pinned_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let pinned = model::Pinned::load(&pinned_file, &catalog);
    let hypr = model::Hypr::new();
    let (add_tx, add_rx) = calloop::channel::channel::<Option<String>>();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        qh: qh.clone(),
        loop_handle: loop_handle.clone(),
        egl,
        gfx: None,
        shm,
        screencopy,
        frac_mgr,
        viewporter,
        cursor_mgr,
        cursor_dev: None,
        pointer: None,
        views: vec![],
        view_serial: 0,
        filter,
        catalog,
        hypr,
        parser: model::EventParser::new(),
        pinned,
        entries: vec![],
        theme: raster::IconTheme::detect(),
        text: raster::TextRenderer::new(),
        icons: HashMap::new(),
        labels: HashMap::new(),
        refresh_scheduled: false,
        add_tx,
        start: Instant::now(),
        quit: false,
        total_draws: 0,
    };

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow!("calloop wayland: {e}"))?;

    // Hyprland events (windows, workspaces, monitors)
    if let Some(h) = app.hypr.as_ref() {
        match h.event_stream() {
            Ok(stream) => {
                let src = Generic::new(stream, Interest::READ, Mode::Level);
                loop_handle
                    .insert_source(src, |_, stream, app: &mut App| {
                        let mut buf = [0u8; 8192];
                        loop {
                            match unsafe { stream.get_mut() }.read(&mut buf) {
                                Ok(0) => {
                                    app.quit = true;
                                    return Ok(PostAction::Remove);
                                }
                                Ok(n) => app.on_hypr_data(&buf[..n]),
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(_) => break,
                            }
                        }
                        Ok(PostAction::Continue)
                    })
                    .map_err(|e| anyhow!("event source: {e}"))?;
            }
            Err(e) => eprintln!("no Hyprland event stream: {e}"),
        }
    }

    // Reload pinned.json if another process changed it
    loop_handle
        .insert_source(Timer::from_duration(Duration::from_secs(2)), |_, _, app: &mut App| {
            if app.pinned.reload_if_changed() {
                app.refresh();
            }
            TimeoutAction::ToDuration(Duration::from_secs(2))
        })
        .map_err(|e| anyhow!("timer: {e}"))?;

    // Result of the "add app" picker (rofi)
    loop_handle
        .insert_source(add_rx, |ev, _, app: &mut App| {
            if let calloop::channel::Event::Msg(res) = ev {
                if let Some(id) = res {
                    if app.pinned.pin(&id) {
                        let _ = app.pinned.save();
                        app.refresh();
                    }
                }
                app.menu_released();
            }
        })
        .map_err(|e| anyhow!("channel: {e}"))?;

    // Control socket (tests and debugging)
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = std::path::Path::new(&rt).join("glassdock.sock");
        let _ = std::fs::remove_file(&path);
        if let Ok(listener) = UnixListener::bind(&path) {
            let _ = listener.set_nonblocking(true);
            let src = Generic::new(listener, Interest::READ, Mode::Level);
            loop_handle
                .insert_source(src, |_, l, app: &mut App| {
                    while let Ok((mut s, _)) = unsafe { l.get_mut() }.accept() {
                        let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                        let mut line = String::new();
                        let mut b = [0u8; 256];
                        if let Ok(n) = s.read(&mut b) {
                            line.push_str(&String::from_utf8_lossy(&b[..n]));
                        }
                        let reply = app.on_ctl(line.trim());
                        let _ = s.write_all(reply.as_bytes());
                    }
                    Ok(PostAction::Continue)
                })
                .map_err(|e| anyhow!("control socket: {e}"))?;
        }
    }

    app.refresh();
    event_loop.run(None, &mut app, |app| {
        if app.quit {
            std::process::exit(0);
        }
    })?;
    Ok(())
}

// app logic
impl App {
    fn view_idx_by_surface(&self, s: &wl_surface::WlSurface) -> Option<usize> {
        self.views.iter().position(|v| v.layer.wl_surface() == s)
    }

    fn make_current(&self, vi: usize) -> bool {
        match self.views[vi].egl_surf {
            Some(s) => self.egl.make_current(s).is_ok(),
            None => false,
        }
    }

    fn ensure_view_for(&mut self, output: &wl_output::WlOutput) {
        if self.views.iter().any(|v| &v.output == output) {
            return;
        }
        let Some(info) = self.output_state.info(output) else {
            dbg_log("output without info yet");
            return;
        };
        dbg_log(&format!("output id={} name={:?} logical_size={:?} scale={}", info.id, info.name, info.logical_size, info.scale_factor));
        let Some(name) = info.name.clone() else { return };
        if let Some(f) = &self.filter {
            if !f.iter().any(|x| x == &name) {
                return;
            }
        }
        let out_size = info.logical_size.unwrap_or((1600, 1000));
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(&self.qh, surface.clone(), Layer::Top, Some("glassdock"), Some(output));
        layer.set_anchor(Anchor::BOTTOM);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        let mut layout = Layout::new();
        self.rebuild_slots_for(&mut layout);
        layout.resize_for_slots();
        layer.set_size(layout.w as u32, layout.h as u32);
        let id = self.view_serial;
        self.view_serial += 1;
        let viewport = self.viewporter.as_ref().map(|v| v.get_viewport(&surface, &self.qh, ()));
        let frac = self.frac_mgr.as_ref().map(|m| m.get_fractional_scale(&surface, &self.qh, ViewId(self.views.len())));
        layer.commit();
        let _ = id;
        self.views.push(View {
            output: output.clone(),
            name,
            out_size,
            layer,
            viewport,
            _frac: frac,
            egl_win: std::ptr::null_mut(),
            egl_surf: None,
            scale: info.scale_factor.max(1) as f32,
            int_scale: info.scale_factor.max(1),
            buf: (0, 0),
            configured: false,
            frame_pending: false,
            dirty: true,
            busy: false,
            last_draw: Instant::now(),
            backdrop: None,
            lum: 0.3,
            backdrop_time: None,
            backdrop_stale: true,
            capturing: false,
            layout,
            menu: None,
            menu_close_at: None,
            region_sig: None,
            draws: 0,
            fake_until: None,
        });
    }

    fn remove_view_for(&mut self, output: &wl_output::WlOutput) {
        if let Some(i) = self.views.iter().position(|v| &v.output == output) {
            let v = self.views.remove(i);
            self.destroy_view(v);
        }
    }

    fn destroy_view(&mut self, v: View) {
        if let Some(s) = v.egl_surf {
            self.egl.destroy_surface(s);
        }
        if !v.egl_win.is_null() {
            unsafe {
                wayland_sys::ffi_dispatch!(wayland_sys::egl::wayland_egl_handle(), wl_egl_window_destroy, v.egl_win as *mut _);
            }
        }
        if let (Some(g), Some(t)) = (self.gfx.as_ref(), v.backdrop) {
            g.delete_tex(t);
        }
    }

    /// Computes the buffer size and (re)creates the EGL window.
    fn ensure_egl(&mut self, vi: usize) -> Result<()> {
        let (w, h, scale, int_scale, has_vp) = {
            let v = &self.views[vi];
            (v.layout.w, v.layout.h, v.scale, v.int_scale, v.viewport.is_some())
        };
        let (bw, bh) = if has_vp {
            ((w * scale).round() as i32, (h * scale).round() as i32)
        } else {
            (w as i32 * int_scale, h as i32 * int_scale)
        };
        {
            let v = &self.views[vi];
            if let Some(vp) = &v.viewport {
                vp.set_destination(w as i32, h as i32);
            } else {
                v.layer.wl_surface().set_buffer_scale(int_scale);
            }
        }
        if self.views[vi].egl_win.is_null() {
            let proxy = self.views[vi].layer.wl_surface().id().as_ptr();
            let win = unsafe { wayland_sys::ffi_dispatch!(wayland_sys::egl::wayland_egl_handle(), wl_egl_window_create, proxy, bw, bh) };
            if win.is_null() {
                return Err(anyhow!("wl_egl_window_create failed"));
            }
            let surf = unsafe { self.egl.create_surface(win as *mut c_void)? };
            let v = &mut self.views[vi];
            v.egl_win = win as *mut c_void;
            v.egl_surf = Some(surf);
            v.buf = (bw, bh);
            self.egl.make_current(surf)?;
            self.egl.set_swap_interval0();
            if self.gfx.is_none() {
                let egl = &self.egl;
                let gl = unsafe { glow::Context::from_loader_function(|n| egl.proc_address(n)) };
                let g = gfx::Gfx::new(gl, Some(GLASS_VS), Some(GLASS_FS))?;
                eprintln!("GL ready (glass: {})", if g.glass_is_fallback { "fallback" } else { "shader" });
                self.gfx = Some(g);
                self.sync_icons();
            }
        } else if self.views[vi].buf != (bw, bh) {
            unsafe {
                wayland_sys::ffi_dispatch!(wayland_sys::egl::wayland_egl_handle(), wl_egl_window_resize, self.views[vi].egl_win as *mut _, bw, bh, 0, 0);
            }
            self.views[vi].buf = (bw, bh);
            self.views[vi].backdrop_stale = true;
        }
        Ok(())
    }

    fn rebuild_slots_for(&self, layout: &mut Layout) {
        let old: HashMap<String, f32> = layout.slots.iter().map(|s| (s.key.clone(), s.s)).collect();
        let pinned: Vec<usize> = (0..self.entries.len()).filter(|&i| self.entries[i].pinned).collect();
        let extra: Vec<usize> = (0..self.entries.len()).filter(|&i| !self.entries[i].pinned).collect();
        let mut slots: Vec<Slot> = vec![];
        let mk = |kind: Kind, entry: usize, key: String, base: f32| Slot {
            kind,
            entry,
            s: old.get(&key).copied().unwrap_or(1.0),
            key,
            base,
            x: 0.0,
            w: base,
        };
        for &i in &pinned {
            slots.push(mk(Kind::App, i, self.entries[i].key.clone(), ICON));
        }
        if !pinned.is_empty() && !extra.is_empty() {
            slots.push(mk(Kind::Sep, 0, "sep1".into(), dock::SEP_W));
        }
        for &i in &extra {
            slots.push(mk(Kind::App, i, self.entries[i].key.clone(), ICON));
        }
        slots.push(mk(Kind::Sep, 0, "sep2".into(), dock::SEP_W));
        slots.push(mk(Kind::Add, 0, "add".into(), ADD_BASE));
        layout.slots = slots;
    }

    /// Re-reads windows and apps from Hyprland and rebuilds the entries.
    fn refresh(&mut self) {
        dbg_log("refresh");
        self.refresh_scheduled = false;
        let (windows, active) = match &self.hypr {
            Some(h) => (h.clients(), h.active_window_address()),
            None => (vec![], None),
        };
        self.entries = model::build_entries(&self.catalog, self.pinned.list(), &windows, active.as_deref());
        let mons = self.hypr.as_ref().map(|h| h.monitors()).unwrap_or_default();
        for vi in 0..self.views.len() {
            let mut layout = std::mem::replace(&mut self.views[vi].layout, Layout::new());
            self.rebuild_slots_for(&mut layout);
            let name = self.views[vi].name.clone();
            let empty = match (&self.hypr, mons.iter().find(|m| m.name == name)) {
                (Some(h), Some(m)) => h.workspace_window_count(m.active_workspace_id) == 0,
                _ => false,
            };
            layout.forced_visible = empty;
            let changed = layout.resize_for_slots();
            let v = &mut self.views[vi];
            v.layout = layout;
            v.backdrop_stale = true;
            if changed {
                v.layer.set_size(v.layout.w as u32, v.layout.h as u32);
                v.layer.commit();
            }
        }
        self.sync_icons();
        for vi in 0..self.views.len() {
            self.request_redraw(vi);
        }
    }

    fn sync_icons(&mut self) {
        if self.gfx.is_none() {
            return;
        }
        let Some(vi) = (0..self.views.len()).find(|&i| self.views[i].egl_surf.is_some()) else { return };
        if !self.make_current(vi) {
            return;
        }
        let gfx = self.gfx.as_ref().unwrap();
        for e in &self.entries {
            if self.icons.contains_key(&e.key) {
                continue;
            }
            if let Some(b) = raster::load_icon(&self.theme, &e.icon, 256) {
                if let Ok(t) = gfx.upload_rgba(b.w as i32, b.h as i32, &b.rgba, true) {
                    self.icons.insert(e.key.clone(), t);
                }
            }
        }
    }

    fn on_hypr_data(&mut self, data: &[u8]) {
        let names = self.parser.push(data);
        if names.iter().any(|n| model::EventParser::is_relevant(n)) && !self.refresh_scheduled {
            self.refresh_scheduled = true;
            let _ = self.loop_handle.insert_source(Timer::from_duration(Duration::from_millis(80)), |_, _, app: &mut App| {
                app.refresh();
                TimeoutAction::Drop
            });
        }
    }

    fn request_redraw(&mut self, vi: usize) {
        if vi >= self.views.len() || !self.views[vi].configured {
            return;
        }
        if self.views[vi].frame_pending {
            self.views[vi].dirty = true;
        } else {
            self.draw(vi);
        }
    }

    fn schedule_wake(&mut self, d: Duration) {
        let _ = self.loop_handle.insert_source(Timer::from_duration(d), |_, _, app: &mut App| {
            for vi in 0..app.views.len() {
                app.request_redraw(vi);
            }
            TimeoutAction::Drop
        });
    }

    fn menu_released(&mut self) {
        let now = Instant::now();
        for v in &mut self.views {
            v.layout.menu_open = v.menu.is_some();
            if v.menu.is_none() {
                v.layout.hold_until = Some(now + HIDE_DELAY);
            }
        }
        self.schedule_wake(HIDE_DELAY + Duration::from_millis(30));
        for vi in 0..self.views.len() {
            self.request_redraw(vi);
        }
    }

    // backdrop capture
    fn start_capture(&mut self, vi: usize) {
        let Some(mgr) = self.screencopy.clone() else {
            self.views[vi].backdrop_stale = false;
            return;
        };
        let v = &mut self.views[vi];
        if v.capturing {
            return;
        }
        v.capturing = true;
        dbg_log(&format!("capture started on {}", v.name));
        let (ow, oh) = v.out_size;
        let (w, h) = (v.layout.w as i32, v.layout.h as i32);
        let x = (ow - w) / 2;
        let y = oh - h;
        let data = CaptureData {
            view: vi,
            st: Mutex::new(CapState { fmt: None, w: 0, h: 0, stride: 0, y_invert: false, buf: None }),
        };
        mgr.capture_output_region(0, &v.output, x, y, w, h, &self.qh, data);
    }

    fn capture_done(&mut self, vi: usize, rgba: Option<(Vec<u8>, i32, i32)>) {
        if vi >= self.views.len() {
            return;
        }
        self.views[vi].capturing = false;
        dbg_log(&format!("capture finished (ok={})", rgba.is_some()));
        if let Some((px, w, h)) = rgba {
            if self.make_current(vi) {
                if let Some(g) = self.gfx.as_ref() {
                    let lum = capture::mean_luma(&px, w as usize, h as usize);
                    let v = &mut self.views[vi];
                    if g.update_rgba(&mut v.backdrop, w, h, &px, true).is_ok() {
                        v.lum = lum;
                        v.backdrop_time = Some(Instant::now());
                        v.backdrop_stale = false;
                    }
                }
            }
        }
        self.request_redraw(vi);
    }

    // drawing
    fn text_tex(&mut self, text: &str, px: f32, bold: bool) -> Option<(i32, i32)> {
        let key = (text.to_string(), (px * 10.0) as u32, bold);
        if let Some((_, w, h)) = self.labels.get(&key) {
            return Some((*w, *h));
        }
        let tr = self.text.as_mut()?;
        let b = tr.render(text, px, bold);
        let g = self.gfx.as_ref()?;
        let t = g.upload_rgba(b.w as i32, b.h as i32, &b.rgba, false).ok()?;
        let (w, h) = (b.w as i32, b.h as i32);
        if self.labels.len() > 200 {
            let old: Vec<_> = self.labels.drain().collect();
            for (_, (t, _, _)) in old {
                g.delete_tex(t);
            }
        }
        self.labels.insert(key, (t, w, h));
        Some((w, h))
    }

    fn draw(&mut self, vi: usize) {
        if vi >= self.views.len() || !self.views[vi].configured || self.gfx.is_none() {
            return;
        }
        if !self.make_current(vi) {
            return;
        }
        let now = Instant::now();
        let (busy, start_cap) = {
            let have_screencopy = self.screencopy.is_some();
            let v = &mut self.views[vi];
            let dt = now.duration_since(v.last_draw).as_secs_f32().clamp(0.0005, 0.08);
            v.last_draw = now;
            if let Some(t) = v.fake_until {
                if now >= t {
                    v.fake_until = None;
                    if v.menu.is_none() {
                        v.layout.pointer = None;
                        v.layout.hold_until = Some(now + HIDE_DELAY);
                    }
                }
            }
            let need = v.backdrop_stale || v.backdrop_time.map_or(true, |t| now.duration_since(t) > BACKDROP_MAX_AGE);
            let r = v.layout.step(dt, now, need && have_screencopy, v.capturing);
            (r.busy, r.start_capture)
        };
        if start_cap {
            self.start_capture(vi);
        }
        self.update_region(vi);
        self.render(vi, now);
        let v = &mut self.views[vi];
        v.draws += 1;
        self.total_draws += 1;
        v.busy = busy;
        let surface = v.layer.wl_surface().clone();
        if busy || v.dirty {
            v.dirty = false;
            v.frame_pending = true;
            surface.frame(&self.qh, surface.clone());
        } else {
            v.frame_pending = false;
        }
        if let Some(s) = v.egl_surf {
            let _ = self.egl.swap(s);
        }
        if !busy {
            if let Some(t) = self.views[vi].layout.hold_until {
                let n = Instant::now();
                if t > n {
                    self.schedule_wake(t - n + Duration::from_millis(30));
                }
            }
        }
    }

    fn update_region(&mut self, vi: usize) {
        let menu_rect = self.views[vi].menu.as_ref().map(|m| m.rect);
        let v = &mut self.views[vi];
        let (mut x, mut y, mut w, mut h) = v.layout.input_rect();
        if let Some(m) = menu_rect {
            let x0 = x.min(m[0] as i32 - 4);
            let y0 = y.min(m[1] as i32 - 4);
            let x1 = (x + w).max((m[0] + m[2]) as i32 + 4);
            let y1 = (y + h).max((m[1] + m[3]) as i32 + 4);
            x = x0;
            y = y0;
            w = x1 - x0;
            h = y1 - y0;
        }
        let sig = (x, y, w, h);
        if v.region_sig == Some(sig) {
            return;
        }
        dbg_log(&format!("input region {:?} -> {:?} (reveal {:.2})", v.region_sig, sig, v.layout.reveal));
        v.region_sig = Some(sig);
        if let Ok(r) = Region::new(&self.compositor) {
            r.add(x, y, w.max(1), h.max(1));
            v.layer.wl_surface().set_input_region(Some(r.wl_region()));
        }
    }

    fn render(&mut self, vi: usize, now: Instant) {
        let (bw, bh, scale) = {
            let v = &self.views[vi];
            (v.buf.0, v.buf.1, v.buf.0 as f32 / v.layout.w)
        };
        let surf = (bw as f32, bh as f32);
        self.gfx.as_ref().unwrap().begin_frame(bw, bh);
        if self.views[vi].layout.reveal < 0.004 || self.views[vi].layout.slots.is_empty() {
            return;
        }
        let pg = self.views[vi].layout.panel_geom();
        let rest_top = self.views[vi].layout.rest_top();
        let bright = self.views[vi].lum > 0.62;
        let lum = self.views[vi].lum;
        let hover = self.views[vi].layout.hover;
        let pointer = self.views[vi].layout.pointer;
        let panel_px = [pg.left * scale, pg.top * scale, pg.width * scale, pg.height * scale];
        let margin = 48.0 * scale;
        let qx = (panel_px[0] - margin).max(0.0);
        let qy = (panel_px[1] - margin).max(0.0);
        let quad = [qx, qy, (panel_px[2] + 2.0 * margin).min(surf.0 - qx), (panel_px[3] + 2.0 * margin + 20.0 * scale).min(surf.1 - qy)];
        let t = now.duration_since(self.start).as_secs_f32();
        {
            let g = self.gfx.as_ref().unwrap();
            let v = &self.views[vi];
            if let Some(bd) = v.backdrop.as_ref() {
                let ptr = match pointer {
                    Some((px, py)) => (px * scale, py * scale, hover),
                    None => (-1000.0, -1000.0, 0.0),
                };
                g.glass(surf, bd, panel_px, quad, scale, (0.0, (rest_top - pg.top) * scale), ptr, lum, t);
            } else {
                g.shape(surf, panel_px, panel_px[3] * 0.5, [0.05, 0.055, 0.08, 0.80], [0.04, 0.04, 0.05, 0.25], 1.0 * scale);
            }
        }

        // icons and decorations
        let icon_bottom = (pg.top + PANEL_H - PAD_BOTTOM) * scale;
        let panel_bottom = (pg.top + PANEL_H) * scale;
        let slots = self.views[vi].layout.slots.clone();
        let label_slot = self.views[vi].layout.label_slot;
        let label_alpha = self.views[vi].layout.label_alpha;
        let mut lifted: Vec<(usize, [f32; 4])> = vec![];
        {
            let g = self.gfx.as_ref().unwrap();
            let v = &self.views[vi];
            for (si, s) in slots.iter().enumerate() {
                match s.kind {
                    Kind::Sep => {
                        let c = if bright { [0.10, 0.11, 0.18, 0.28] } else { [1.0, 1.0, 1.0, 0.22] };
                        let a = c[3];
                        g.shape(
                            surf,
                            [(s.x + s.w / 2.0) * scale - 0.5 * scale, (pg.top + 14.0) * scale, 1.0 * scale, (PANEL_H - 28.0) * scale],
                            0.0,
                            [c[0] * a, c[1] * a, c[2] * a, a],
                            [0.0; 4],
                            0.0,
                        );
                    }
                    Kind::Add => {
                        let size = s.w * scale;
                        let x = s.x * scale;
                        let y = icon_bottom - size;
                        let (fill, stroke, plus) = if bright {
                            ([0.0, 0.0, 0.0, 0.07], [0.04, 0.044, 0.072, 0.40], [0.09, 0.10, 0.16, 0.9])
                        } else {
                            ([0.10, 0.10, 0.10, 0.10], [0.28, 0.28, 0.28, 0.28], [C_FG[0] * 0.9, C_FG[1] * 0.9, C_FG[2] * 0.9, 0.9])
                        };
                        g.shape(surf, [x, y, size, size], size / 2.0, fill, stroke, 1.0 * scale);
                        let k = size * 0.22;
                        let th = (size * 0.055).max(1.5 * scale);
                        let (cx, cy) = (x + size / 2.0, y + size / 2.0);
                        g.shape(surf, [cx - k, cy - th / 2.0, 2.0 * k, th], th / 2.0, plus, [0.0; 4], 0.0);
                        g.shape(surf, [cx - th / 2.0, cy - k, th, 2.0 * k], th / 2.0, plus, [0.0; 4], 0.0);
                    }
                    Kind::App => {
                        let bounce = v.layout.bounce_offset(&s.key, now);
                        let size = s.w * scale;
                        let rect = [s.x * scale, icon_bottom - size - bounce * scale, size, size];
                        if s.s > 1.02 {
                            lifted.push((si, rect));
                        } else if let Some(t) = self.icons.get(&s.key) {
                            g.texture(surf, t, rect, [1.0; 4], false);
                        }
                        if let Some(e) = self.entries.get(s.entry) {
                            if !e.windows.is_empty() {
                                let cx = (s.x + s.w / 2.0) * scale;
                                let cy = panel_bottom - (PAD_BOTTOM / 2.0 - 1.0) * scale;
                                if e.active {
                                    let (w, h) = (14.0 * scale, 4.0 * scale);
                                    g.shape(surf, [cx - w / 2.0, cy - h / 2.0, w, h], h / 2.0, [C_BLUE[0] * 0.95, C_BLUE[1] * 0.95, C_BLUE[2] * 0.95, 0.95], [0.0; 4], 0.0);
                                } else {
                                    let r = 2.2 * scale;
                                    let c = if bright { [0.065, 0.07, 0.117, 0.65] } else { [0.55, 0.55, 0.55, 0.55] };
                                    g.shape(surf, [cx - r, cy - r, 2.0 * r, 2.0 * r], r, c, [0.0; 4], 0.0);
                                }
                            }
                        }
                    }
                }
            }
            for (si, rect) in &lifted {
                if let Some(t) = self.icons.get(&slots[*si].key) {
                    g.texture(surf, t, *rect, [1.0; 4], false);
                }
            }
        }

        // name pill of the icon under the pointer
        if label_alpha > 0.02 {
            if let Some(si) = label_slot {
                if let Some(s) = slots.get(si) {
                    if let Some(e) = self.entries.get(s.entry).cloned() {
                        let mut text = e.name.clone();
                        if e.windows.len() > 1 {
                            text.push_str(&format!("  ·  {}", e.windows.len()));
                        }
                        if let Some((tw, th)) = self.text_tex(&text, 13.0 * scale, true) {
                            let g = self.gfx.as_ref().unwrap();
                            let (pw, ph) = (tw as f32 + 26.0 * scale, th as f32 + 14.0 * scale);
                            let cx = (s.x + s.w / 2.0) * scale;
                            let x = (cx - pw / 2.0).clamp(4.0 * scale, (surf.0 - pw - 4.0 * scale).max(4.0 * scale));
                            let y = icon_bottom - s.w * scale - 16.0 * scale - ph;
                            let a = label_alpha;
                            g.shape(surf, [x, y, pw, ph], ph / 2.0, [0.055 * a, 0.062 * a, 0.094 * a, 0.78 * a], [0.16 * a, 0.16 * a, 0.16 * a, 0.16 * a], 1.0 * scale);
                            let key = (text.clone(), (13.0 * scale * 10.0) as u32, true);
                            if let Some((t, _, _)) = self.labels.get(&key) {
                                g.texture(surf, t, [x + 13.0 * scale, y + 7.0 * scale, tw as f32, th as f32], [C_FG[0] * a, C_FG[1] * a, C_FG[2] * a, a], true);
                            }
                        }
                    }
                }
            }
        }

        self.render_menu(vi, surf, scale);
    }

    fn render_menu(&mut self, vi: usize, surf: (f32, f32), scale: f32) {
        let Some(menu) = self.views[vi].menu.as_ref() else { return };
        let items = menu.items.clone();
        let rect = menu.rect;
        let hover = menu.hover;
        let rows: Vec<(f32, f32)> = (0..items.len()).map(|i| menu.row_geom(i)).collect();
        for it in &items {
            if !matches!(it.kind, ItemKind::Sep) {
                self.text_tex(&it.label, 13.0 * scale, matches!(it.kind, ItemKind::Title));
            }
        }
        let g = self.gfx.as_ref().unwrap();
        let r = [rect[0] * scale, rect[1] * scale, rect[2] * scale, rect[3] * scale];
        g.shape(surf, r, 14.0 * scale, [0.067, 0.071, 0.105, 0.95], [0.18, 0.18, 0.20, 0.20], 1.0 * scale);
        for (i, it) in items.iter().enumerate() {
            let (ry, rh) = rows[i];
            match &it.kind {
                ItemKind::Sep => {
                    g.shape(surf, [(rect[0] + 12.0) * scale, (ry + rh / 2.0) * scale, (rect[2] - 24.0) * scale, 1.0 * scale], 0.0, [0.16, 0.16, 0.18, 0.16], [0.0; 4], 0.0);
                }
                ItemKind::Title | ItemKind::Item(_) => {
                    let is_title = matches!(it.kind, ItemKind::Title);
                    if hover == Some(i) {
                        g.shape(surf, [(rect[0] + 6.0) * scale, ry * scale, (rect[2] - 12.0) * scale, rh * scale], 8.0 * scale, [0.105, 0.14, 0.213, 0.22], [0.0; 4], 0.0);
                    }
                    let key = (it.label.clone(), (13.0 * scale * 10.0) as u32, is_title);
                    if let Some((t, tw, th)) = self.labels.get(&key) {
                        let col = if is_title {
                            [0.34, 0.37, 0.54, 1.0]
                        } else if hover == Some(i) {
                            [C_BLUE[0], C_BLUE[1], C_BLUE[2], 1.0]
                        } else {
                            [C_FG[0], C_FG[1], C_FG[2], 1.0]
                        };
                        g.texture(surf, t, [(rect[0] + 16.0) * scale, (ry + rh / 2.0) * scale - *th as f32 / 2.0, *tw as f32, *th as f32], col, true);
                    }
                }
            }
        }
    }

    // input handling
    fn pointer_enter(&mut self, vi: usize, pos: (f32, f32), serial: u32) {
        if let Some(dev) = &self.cursor_dev {
            dev.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
        }
        let v = &mut self.views[vi];
        v.layout.pointer = Some(pos);
        v.menu_close_at = None;
        self.request_redraw(vi);
    }

    fn pointer_motion(&mut self, vi: usize, pos: (f32, f32)) {
        let v = &mut self.views[vi];
        v.layout.pointer = Some(pos);
        if let Some(m) = v.menu.as_mut() {
            m.hover = m.item_at(pos.0, pos.1);
        }
        self.request_redraw(vi);
    }

    fn pointer_leave(&mut self, vi: usize) {
        dbg_log(&format!("pointer_leave (reveal {:.2})", self.views[vi].layout.reveal));
        let now = Instant::now();
        let v = &mut self.views[vi];
        v.layout.pointer = None;
        v.layout.hold_until = Some(now + HIDE_DELAY);
        if v.menu.is_some() {
            v.menu_close_at = Some(now + Duration::from_millis(900));
        }
        let has_menu = v.menu.is_some();
        self.request_redraw(vi);
        self.schedule_wake(HIDE_DELAY + Duration::from_millis(30));
        if has_menu {
            let _ = self.loop_handle.insert_source(Timer::from_duration(Duration::from_millis(950)), move |_, _, app: &mut App| {
                if let Some(v) = app.views.get_mut(vi) {
                    if v.menu_close_at.map_or(false, |t| Instant::now() >= t) && v.layout.pointer.is_none() {
                        v.menu = None;
                        v.menu_close_at = None;
                    }
                }
                app.menu_released();
                TimeoutAction::Drop
            });
        }
    }

    fn pointer_press(&mut self, vi: usize, pos: (f32, f32), button: u32) {
        if let Some(menu) = self.views[vi].menu.as_ref() {
            let hit = menu.item_at(pos.0, pos.1);
            let inside = menu.contains(pos.0, pos.1);
            let entry = menu.entry;
            let action = hit.and_then(|i| match &menu.items[i].kind {
                ItemKind::Item(a) => Some(a.clone()),
                _ => None,
            });
            if inside && action.is_none() {
                return;
            }
            self.views[vi].menu = None;
            self.menu_released();
            if let Some(a) = action {
                self.run_menu_action(entry, a);
            }
            return;
        }
        let Some(si) = self.views[vi].layout.slot_at(pos.0) else { return };
        let slot = self.views[vi].layout.slots[si].clone();
        match slot.kind {
            Kind::Add => {
                if button == BTN_LEFT {
                    self.add_app_dialog();
                }
            }
            Kind::App => {
                let Some(e) = self.entries.get(slot.entry).cloned() else { return };
                match button {
                    BTN_LEFT => self.activate_entry(vi, &e),
                    BTN_MIDDLE => self.launch_entry(vi, &e),
                    BTN_RIGHT => self.open_menu(vi, si, &e),
                    _ => {}
                }
            }
            Kind::Sep => {}
        }
    }

    fn launch_entry(&mut self, vi: usize, e: &model::Entry) {
        if let Some(app) = &e.app {
            model::launch(app, None);
            if e.windows.is_empty() {
                self.views[vi].layout.bounce.insert(e.key.clone(), Instant::now());
                self.request_redraw(vi);
            }
        }
    }

    fn activate_entry(&mut self, vi: usize, e: &model::Entry) {
        if e.windows.is_empty() {
            self.launch_entry(vi, e);
            return;
        }
        let mut target = e.windows[0].clone();
        if e.active && e.windows.len() > 1 {
            if let Some(act) = self.hypr.as_ref().and_then(|h| h.active_window_address()) {
                if let Some(i) = e.windows.iter().position(|w| *w == act) {
                    target = e.windows[(i + 1) % e.windows.len()].clone();
                }
            }
        }
        if let Some(h) = &self.hypr {
            h.dispatch(&format!("focuswindow address:{target}"));
        }
    }

    fn open_menu(&mut self, vi: usize, si: usize, e: &model::Entry) {
        let mut items: Vec<MenuItem> = vec![];
        let push = |items: &mut Vec<MenuItem>, label: &str, kind: ItemKind| items.push(MenuItem { label: label.to_string(), kind });
        push(&mut items, &e.name, ItemKind::Title);
        push(&mut items, "", ItemKind::Sep);
        push(&mut items, tr(Msg::NewWindow), ItemKind::Item(MenuAction::NewWindow));
        if let Some(app) = &e.app {
            for a in &app.actions {
                push(&mut items, &a.name, ItemKind::Item(MenuAction::DesktopAction(a.id.clone())));
            }
        }
        if !e.windows.is_empty() {
            push(&mut items, "", ItemKind::Sep);
            let many = e.windows.len() > 1;
            push(&mut items, tr(if many { Msg::MinimizeAll } else { Msg::MinimizeOne }), ItemKind::Item(MenuAction::Minimize));
            push(&mut items, tr(if many { Msg::CloseAll } else { Msg::CloseOne }), ItemKind::Item(MenuAction::Close));
        }
        push(&mut items, "", ItemKind::Sep);
        if e.pinned {
            let list = self.pinned.list();
            let idx = e.app.as_ref().and_then(|a| list.iter().position(|p| *p == a.id));
            if let Some(i) = idx {
                if i > 0 {
                    push(&mut items, tr(Msg::MoveLeft), ItemKind::Item(MenuAction::MoveLeft));
                }
                if i + 1 < list.len() {
                    push(&mut items, tr(Msg::MoveRight), ItemKind::Item(MenuAction::MoveRight));
                }
            }
            push(&mut items, tr(Msg::Unpin), ItemKind::Item(MenuAction::Unpin));
        } else if e.app.is_some() {
            push(&mut items, tr(Msg::Pin), ItemKind::Item(MenuAction::Pin));
        }
        let mut maxw = 120.0f32;
        for it in &items {
            if !matches!(it.kind, ItemKind::Sep) {
                if let Some(tr) = self.text.as_mut() {
                    maxw = maxw.max(tr.measure(&it.label, 13.0, matches!(it.kind, ItemKind::Title)).0);
                }
            }
        }
        let w = maxw + 40.0;
        let h = 2.0 * MENU_PAD + items.iter().map(|i| if matches!(i.kind, ItemKind::Sep) { MENU_SEP } else { MENU_ROW }).sum::<f32>();
        let v = &mut self.views[vi];
        let s = &v.layout.slots[si];
        let icon_top = v.layout.h - BOTTOM_GAP - PAD_BOTTOM - s.w;
        let x = (s.x + s.w / 2.0 - w / 2.0).clamp(4.0, (v.layout.w - w - 4.0).max(4.0));
        let y = (icon_top - 14.0 - h).max(4.0);
        v.menu = Some(Menu { entry: s.entry, items, rect: [x, y, w, h], hover: None });
        v.layout.menu_open = true;
        self.request_redraw(vi);
    }

    fn run_menu_action(&mut self, entry: usize, a: MenuAction) {
        let Some(e) = self.entries.get(entry).cloned() else { return };
        match a {
            MenuAction::NewWindow => {
                if let Some(app) = &e.app {
                    model::launch(app, None);
                }
            }
            MenuAction::DesktopAction(id) => {
                if let Some(app) = &e.app {
                    model::launch(app, Some(&id));
                }
            }
            MenuAction::Minimize => {
                if let Some(h) = &self.hypr {
                    for w in &e.windows {
                        h.dispatch(&format!("movetoworkspacesilent special:minimized,address:{w}"));
                    }
                }
            }
            MenuAction::Close => {
                if let Some(h) = &self.hypr {
                    for w in &e.windows {
                        h.dispatch(&format!("closewindow address:{w}"));
                    }
                }
            }
            MenuAction::Pin | MenuAction::Unpin | MenuAction::MoveLeft | MenuAction::MoveRight => {
                if let Some(app) = &e.app {
                    let id = app.id.clone();
                    let ok = match a {
                        MenuAction::Pin => self.pinned.pin(&id),
                        MenuAction::Unpin => self.pinned.unpin(&id),
                        MenuAction::MoveLeft => self.pinned.move_by(&id, -1),
                        _ => self.pinned.move_by(&id, 1),
                    };
                    if ok {
                        let _ = self.pinned.save();
                        self.refresh();
                    }
                }
            }
        }
    }

    fn add_app_dialog(&mut self) {
        let pinned: Vec<String> = self.pinned.list().to_vec();
        let apps: Vec<(String, String, String)> = self
            .catalog
            .visible()
            .into_iter()
            .filter(|a| !pinned.contains(&a.id))
            .map(|a| (a.id.clone(), a.name.replace('\n', " "), a.icon.clone()))
            .collect();
        let mut data: Vec<u8> = vec![];
        for (i, (_, name, icon)) in apps.iter().enumerate() {
            data.extend_from_slice(name.as_bytes());
            if !icon.is_empty() {
                data.push(0);
                data.extend_from_slice(b"icon\x1f");
                data.extend_from_slice(icon.as_bytes());
            }
            if i + 1 < apps.len() {
                data.push(b'\n');
            }
        }
        for v in &mut self.views {
            v.layout.menu_open = true;
        }
        let tx = self.add_tx.clone();
        std::thread::spawn(move || {
            use std::process::{Command, Stdio};
            let child = Command::new("rofi")
                .args(["-dmenu", "-i", "-format", "i", "-show-icons", "-p", tr(Msg::AddPrompt)])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let mut result = None;
            if let Ok(mut c) = child {
                if let Some(mut si) = c.stdin.take() {
                    let _ = si.write_all(&data);
                }
                if let Ok(out) = c.wait_with_output() {
                    let s = String::from_utf8_lossy(&out.stdout);
                    if let Ok(i) = s.trim().parse::<usize>() {
                        result = apps.get(i).map(|a| a.0.clone());
                    }
                }
            }
            let _ = tx.send(result);
        });
    }

    // ---------------------------------------------------------------- control socket
    fn on_ctl(&mut self, line: &str) -> String {
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("");
        let a1 = it.next();
        let a2 = it.next();
        let now = Instant::now();
        let pick = |app: &App, name: Option<&str>| -> Vec<usize> {
            match name {
                Some(n) if n != "all" => (0..app.views.len()).filter(|&i| app.views[i].name == n).collect(),
                _ => (0..app.views.len()).collect(),
            }
        };
        match cmd {
            "reveal" => {
                for vi in pick(self, a1) {
                    self.views[vi].layout.manual_until = Some(now + Duration::from_secs(4));
                    self.request_redraw(vi);
                }
                self.schedule_wake(Duration::from_millis(4100));
                "ok\n".into()
            }
            "hide" => {
                for vi in pick(self, a1) {
                    let v = &mut self.views[vi];
                    v.layout.manual_until = None;
                    v.layout.pointer = None;
                    v.layout.hold_until = None;
                    v.menu = None;
                    v.layout.menu_open = false;
                    self.request_redraw(vi);
                }
                "ok\n".into()
            }
            "hover" => {
                for vi in pick(self, a1) {
                    let v = &mut self.views[vi];
                    match a2 {
                        Some("off") | None => {
                            v.layout.pointer = None;
                            v.fake_until = None;
                            v.layout.hold_until = Some(now + HIDE_DELAY);
                        }
                        Some(x) => {
                            if let Ok(x) = x.parse::<f32>() {
                                v.layout.pointer = Some((x, v.layout.h - BOTTOM_GAP - PAD_BOTTOM - 8.0));
                                v.fake_until = Some(now + Duration::from_secs(4));
                            }
                        }
                    }
                    self.request_redraw(vi);
                }
                self.schedule_wake(Duration::from_millis(4100));
                "ok\n".into()
            }
            "menu" => {
                let key = a2.unwrap_or("");
                let mut ok = false;
                for vi in pick(self, a1) {
                    if let Some(si) = self.views[vi].layout.slots.iter().position(|s| s.key == key) {
                        let e = self.entries.get(self.views[vi].layout.slots[si].entry).cloned();
                        if let Some(e) = e {
                            self.views[vi].layout.manual_until = Some(now + Duration::from_secs(8));
                            self.open_menu(vi, si, &e);
                            ok = true;
                        }
                    }
                }
                if ok { "ok\n".into() } else { "not-found\n".into() }
            }
            "refresh" => {
                self.refresh();
                "ok\n".into()
            }
            "shader" => {
                // shader <directory containing glass.vert and glass.frag>
                let Some(dir) = a1 else { return "usage: shader <directory>\n".into() };
                let vs = std::fs::read_to_string(std::path::Path::new(dir).join("glass.vert"));
                let fs = std::fs::read_to_string(std::path::Path::new(dir).join("glass.frag"));
                match (vs, fs) {
                    (Ok(vs), Ok(fs)) => {
                        if let Some(vi) = (0..self.views.len()).find(|&i| self.views[i].egl_surf.is_some()) {
                            self.make_current(vi);
                        }
                        let res = match self.gfx.as_mut() {
                            Some(g) => g.set_glass(&vs, &fs),
                            None => Err(anyhow!("GL not ready")),
                        };
                        match res {
                            Ok(()) => {
                                for vi in 0..self.views.len() {
                                    self.request_redraw(vi);
                                }
                                "ok\n".into()
                            }
                            Err(e) => format!("shader error: {e}\n"),
                        }
                    }
                    _ => "could not read glass.vert/glass.frag\n".into(),
                }
            }
            "add" => {
                self.add_app_dialog();
                "ok\n".into()
            }
            "dump" => {
                let mut s = String::from("{\n");
                s.push_str(&format!(
                    "  \"total_draws\": {},\n  \"gl\": \"{}\",\n  \"entries\": {},\n  \"views\": [\n",
                    self.total_draws,
                    self.gfx.as_ref().map(|g| if g.glass_is_fallback { "fallback" } else { "shader" }).unwrap_or("none"),
                    self.entries.len()
                ));
                for (i, v) in self.views.iter().enumerate() {
                    let ptr = v.layout.pointer.map(|p| format!("[{:.1},{:.1}]", p.0, p.1)).unwrap_or_else(|| "null".into());
                    let reg = v.region_sig.map(|r| format!("[{},{},{},{}]", r.0, r.1, r.2, r.3)).unwrap_or_else(|| "null".into());
                    s.push_str(&format!(
                        "    {{\"name\":\"{}\",\"reveal\":{:.3},\"scale\":{:.3},\"buf\":[{},{}],\"surface\":[{},{}],\"draws\":{},\"busy\":{},\"frame_pending\":{},\"lum\":{:.3},\"backdrop\":{},\"capturing\":{},\"pointer\":{},\"menu\":{},\"region\":{},\"slots\":[{}]}}{}\n",
                        v.name, v.layout.reveal, v.buf.0 as f32 / v.layout.w, v.buf.0, v.buf.1, v.layout.w, v.layout.h, v.draws, v.busy, v.frame_pending,
                        v.lum, v.backdrop.is_some(), v.capturing, ptr, v.menu.is_some(), reg,
                        v.layout.slots.iter().map(|s| format!("[\"{}\",{:.3},{:.1},{:.1}]", s.key, s.s, s.x, s.w)).collect::<Vec<_>>().join(","),
                        if i + 1 < self.views.len() { "," } else { "" }
                    ));
                }
                s.push_str("  ]\n}\n");
                s
            }
            "quit" => {
                self.quit = true;
                "ok\n".into()
            }
            _ => "unknown command: reveal|hide|hover|menu|refresh|add|shader|dump|quit\n".into(),
        }
    }
}

// Wayland handlers
impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, new_factor: i32) {
        if let Some(vi) = self.view_idx_by_surface(surface) {
            if self.views[vi].viewport.is_none() {
                self.views[vi].int_scale = new_factor.max(1);
                self.views[vi].scale = new_factor.max(1) as f32;
                if self.views[vi].configured {
                    let _ = self.ensure_egl(vi);
                    self.request_redraw(vi);
                }
            }
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _time: u32) {
        if let Some(vi) = self.view_idx_by_surface(surface) {
            self.views[vi].frame_pending = false;
            if self.views[vi].busy || self.views[vi].dirty {
                self.draw(vi);
            }
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.ensure_view_for(&output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.ensure_view_for(&output);
        if let Some(info) = self.output_state.info(&output) {
            if let Some(v) = self.views.iter_mut().find(|v| v.output == output) {
                if let Some(ls) = info.logical_size {
                    v.out_size = ls;
                }
            }
        }
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.remove_view_for(&output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(i) = self.views.iter().position(|v| v.layer.wl_surface() == layer.wl_surface()) {
            let v = self.views.remove(i);
            self.destroy_view(v);
        }
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, _c: LayerSurfaceConfigure, _serial: u32) {
        let Some(vi) = self.views.iter().position(|v| v.layer.wl_surface() == layer.wl_surface()) else { return };
        self.views[vi].configured = true;
        if let Err(e) = self.ensure_egl(vi) {
            eprintln!("EGL: {e}");
            return;
        }
        self.views[vi].dirty = true;
        self.draw(vi);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        dbg_log(&format!("new_capability {cap:?}"));
        if cap == Capability::Pointer && self.pointer.is_none() {
            if let Ok(p) = self.seat_state.get_pointer(qh, &seat) {
                if let Some(m) = &self.cursor_mgr {
                    self.cursor_dev = Some(m.get_pointer(&p, qh, ()));
                }
                self.pointer = Some(p);
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
            self.cursor_dev = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for ev in events {
            dbg_log(&format!("pointer {:?} @ {:?} (views: {})", ev.kind, ev.position, self.views.len()));
            let Some(vi) = self.view_idx_by_surface(&ev.surface) else { continue };
            let pos = (ev.position.0 as f32, ev.position.1 as f32);
            match ev.kind {
                PointerEventKind::Enter { serial } => self.pointer_enter(vi, pos, serial),
                PointerEventKind::Leave { .. } => self.pointer_leave(vi),
                PointerEventKind::Motion { .. } => self.pointer_motion(vi, pos),
                PointerEventKind::Press { button, .. } => self.pointer_press(vi, pos, button),
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, CaptureData> for App {
    fn event(app: &mut Self, frame: &ZwlrScreencopyFrameV1, event: zwlr_screencopy_frame_v1::Event, data: &CaptureData, _: &Connection, qh: &QueueHandle<Self>) {
        use zwlr_screencopy_frame_v1::Event;
        let mut st = data.st.lock().unwrap();
        match event {
            Event::Buffer { format, width, height, stride } => {
                if let WEnum::Value(f) = format {
                    if st.fmt.is_none() || f == wl_shm::Format::Xrgb8888 || f == wl_shm::Format::Argb8888 {
                        st.fmt = Some(f);
                        st.w = width;
                        st.h = height;
                        st.stride = stride;
                    }
                }
            }
            Event::Flags { flags } => {
                if let WEnum::Value(f) = flags {
                    st.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            Event::BufferDone => {
                let size = (st.stride * st.h) as usize;
                match (st.fmt, capture::alloc(size)) {
                    (Some(fmt), Ok(shm)) => {
                        use std::os::fd::AsFd;
                        let pool = app.shm.create_pool(shm.fd.as_fd(), size as i32, qh, ());
                        let buf = pool.create_buffer(0, st.w as i32, st.h as i32, st.stride as i32, fmt, qh, ());
                        frame.copy(&buf);
                        st.buf = Some((shm, buf, pool));
                    }
                    _ => {
                        frame.destroy();
                        drop(st);
                        app.capture_done(data.view, None);
                    }
                }
            }
            Event::Ready { .. } => {
                let result = st.buf.as_ref().and_then(|(shm, _, _)| {
                    st.fmt
                        .and_then(|f| capture::to_rgba(f, st.w as usize, st.h as usize, st.stride as usize, &shm.map[..], st.y_invert))
                        .map(|px| (px, st.w as i32, st.h as i32))
                });
                if let Some((_, b, p)) = st.buf.take() {
                    b.destroy();
                    p.destroy();
                }
                frame.destroy();
                drop(st);
                app.capture_done(data.view, result);
            }
            Event::Failed => {
                if let Some((_, b, p)) = st.buf.take() {
                    b.destroy();
                    p.destroy();
                }
                frame.destroy();
                drop(st);
                app.capture_done(data.view, None);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ViewId> for App {
    fn event(app: &mut Self, _: &WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, data: &ViewId, _: &Connection, _: &QueueHandle<Self>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let vi = data.0;
            if let Some(v) = app.views.get_mut(vi) {
                v.scale = scale as f32 / 120.0;
                if v.configured {
                    let _ = app.ensure_egl(vi);
                    app.views[vi].backdrop_stale = true;
                    app.request_redraw(vi);
                }
            }
        }
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for App {
    fn event(_: &mut Self, _: &WpCursorShapeDeviceV1, _: wp_cursor_shape_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore ZwlrScreencopyManagerV1);
delegate_noop!(App: ignore WpFractionalScaleManagerV1);
delegate_noop!(App: ignore WpViewporter);
delegate_noop!(App: ignore WpViewport);
delegate_noop!(App: ignore WpCursorShapeManagerV1);

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_pointer!(App);
delegate_layer!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
