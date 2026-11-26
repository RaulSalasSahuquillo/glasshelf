//! The settings window (`glassdock settings`): a client-side decorated `xdg_toplevel` drawn with
//! the same EGL + GLES 3 stack as the dock, in the same liquid-glass style.
//!
//! The file is split in three layers so that most of it is testable without Wayland:
//!
//! 1. pure geometry and value maths (`Rect`, `Layout`, `hit_test`, slider/stepper mapping,
//!    `approach`, `ReloadGate`, `fit_ellipsis`, ...),
//! 2. `Ui`, the interaction state machine (hover, press, drag) that turns pointer events into
//!    `Effects` (save, reload, clear history, close, move the window),
//! 3. the Wayland/EGL shell (`SettingsApp`) and the `Renderer` that draws a `Ui` snapshot.
//!
//! Like the dock it redraws only while something animates or changed (frame-callback driven), so
//! an untouched window costs no CPU.
use crate::config::{self, Settings, RECOMMENDED_MAX, RECOMMENDED_MIN, ZOOM_MAX, ZOOM_MIN};
use crate::gfx::{self, Gfx, Tex};
use crate::i18n::{tr, Msg};
use crate::raster::TextRenderer;
use crate::usage::Usage;
use anyhow::{anyhow, Result};
use glow::HasContext;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_pointer, delegate_registry, delegate_seat, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop, LoopHandle,
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
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::time::{Duration, Instant};
use wayland_client::{
    delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};

const APP_ID: &str = "glassdock-settings";
const BTN_LEFT: u32 = 0x110;
const KEY_ESC: u32 = 1;
const KEY_W: u32 = 17;
/// Control is the third real modifier of every xkb keymap.
const XKB_MOD_CONTROL: u32 = 1 << 2;

// ---------------------------------------------------------------------------------------------
// Geometry (logical px). Everything below is pure and unit tested.
// ---------------------------------------------------------------------------------------------

const WIN_W: f32 = 560.0;
const WIN_H: f32 = 608.0;
/// Matches Hyprland's `decoration:rounding` on this setup: the compositor blurs (and clips) the
/// window with that radius, a larger one would leave blurred corners poking out of the glass.
const WIN_RADIUS: f32 = 12.0;
const TITLE_H: f32 = 52.0;
const CARD_MAX_W: f32 = 520.0;
const SIDE_MIN: f32 = 20.0;
const CARD_RADIUS: f32 = 14.0;
const PAD: f32 = 16.0;
const SEC_HEAD_H: f32 = 34.0;
const SEC_GAP: f32 = 10.0;
const ROW_HINT_H: f32 = 64.0;
const ROW_PLAIN_H: f32 = 52.0;
const ROW_BTN_H: f32 = 62.0;

const TOGGLE_W: f32 = 52.0;
const TOGGLE_H: f32 = 31.0;
const TOGGLE_KNOB_W: f32 = 36.0;
const TOGGLE_KNOB_H: f32 = 25.0;
const SLIDER_W: f32 = 170.0;
const SLIDER_TRACK_H: f32 = 6.0;
const SLIDER_KNOB_W: f32 = 34.0;
const SLIDER_KNOB_H: f32 = 22.0;
/// How much a knob grows (width) while it is pressed.
const STRETCH: f32 = 7.0;
const STEP_BTN_W: f32 = 38.0;
const STEP_BTN_H: f32 = 28.0;
const STEP_VALUE_W: f32 = 34.0;
const CLEAR_W: f32 = 240.0;
const CLEAR_H: f32 = 34.0;
const CLOSE_D: f32 = 14.0;
const ZOOM_STEP: f32 = 0.05;
const RELOAD_INTERVAL: Duration = Duration::from_millis(80);
const CLEARED_FOR: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    fn right(&self) -> f32 {
        self.x + self.w
    }
    fn bottom(&self) -> f32 {
        self.y + self.h
    }
    fn cx(&self) -> f32 {
        self.x + self.w * 0.5
    }
    fn cy(&self) -> f32 {
        self.y + self.h * 0.5
    }
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
    fn inflate(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x - dx, self.y - dy, self.w + 2.0 * dx, self.h + 2.0 * dy)
    }
    /// Scales the rectangle around its centre (the 0.98 "pressed" look).
    fn scaled(&self, k: f32) -> Rect {
        Rect::new(
            self.cx() - self.w * k * 0.5,
            self.cy() - self.h * k * 0.5,
            self.w * k,
            self.h * k,
        )
    }
}

/// Everything the pointer can interact with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ctl {
    Close,
    Magnification,
    Zoom,
    Recommended,
    Minus,
    Plus,
    Clear,
    AutoHide,
    LiveGlass,
}

const CTL_COUNT: usize = 9;
const ALL_CTLS: [Ctl; CTL_COUNT] = [
    Ctl::Close,
    Ctl::Magnification,
    Ctl::Zoom,
    Ctl::Recommended,
    Ctl::Minus,
    Ctl::Plus,
    Ctl::Clear,
    Ctl::AutoHide,
    Ctl::LiveGlass,
];

impl Ctl {
    fn idx(self) -> usize {
        self as usize
    }
}

// Row indices into `Layout::rows`.
const ROW_HIDE: usize = 0;
const ROW_LIVE: usize = 1;
const ROW_MAG: usize = 2;
const ROW_ZOOM: usize = 3;
const ROW_REC: usize = 4;
const ROW_COUNT: usize = 5;
const ROW_CLEAR: usize = 6;
const ROW_N: usize = 7;
const ROW_HEIGHTS: [f32; ROW_N] = [
    ROW_HINT_H,
    ROW_HINT_H,
    ROW_HINT_H,
    ROW_PLAIN_H,
    ROW_HINT_H,
    ROW_PLAIN_H,
    ROW_BTN_H,
];
/// (first row, number of rows) of each card.
const SECTIONS: [(usize, usize); 2] = [(ROW_HIDE, 4), (ROW_REC, 3)];

#[derive(Clone, Debug, PartialEq)]
struct Layout {
    w: f32,
    h: f32,
    close: Rect,
    heads: [Rect; 2],
    cards: [Rect; 2],
    rows: [Rect; ROW_N],
    footer_y: f32,
}

impl Layout {
    /// Lays the page out for a window of `w` x `h`. The cards are centred and capped in width, so a
    /// compositor that hands us a different size (tiling) still gets a sane page.
    fn new(w: f32, h: f32) -> Layout {
        let cw = (w - 2.0 * SIDE_MIN).clamp(0.0, CARD_MAX_W);
        let cx = ((w - cw) * 0.5).max(0.0);
        let mut y = TITLE_H;
        let mut heads = [Rect::default(); 2];
        let mut cards = [Rect::default(); 2];
        let mut rows = [Rect::default(); ROW_N];
        for (si, &(first, n)) in SECTIONS.iter().enumerate() {
            heads[si] = Rect::new(cx, y, cw, SEC_HEAD_H);
            y += SEC_HEAD_H;
            let top = y;
            for r in first..first + n {
                rows[r] = Rect::new(cx, y, cw, ROW_HEIGHTS[r]);
                y += ROW_HEIGHTS[r];
            }
            cards[si] = Rect::new(cx, top, cw, y - top);
            y += SEC_GAP;
        }
        Layout {
            w,
            h,
            close: Rect::new(17.0, (TITLE_H - CLOSE_D) * 0.5, CLOSE_D, CLOSE_D),
            heads,
            cards,
            rows,
            footer_y: h - 26.0,
        }
    }

    /// Rows that are followed by a hairline (every row but the last of its card).
    fn separators(&self) -> Vec<Rect> {
        let mut v = vec![];
        for &(first, n) in &SECTIONS {
            for r in first..first + n - 1 {
                let row = &self.rows[r];
                v.push(Rect::new(row.x + PAD, row.bottom(), row.w - 2.0 * PAD, 1.0));
            }
        }
        v
    }
}

fn toggle_track(row: &Rect) -> Rect {
    Rect::new(
        row.right() - PAD - TOGGLE_W,
        row.cy() - TOGGLE_H * 0.5,
        TOGGLE_W,
        TOGGLE_H,
    )
}

/// The knob inside a toggle: `on` is the slide position (0..1), `press` the stretch (0..1). The
/// edge opposite to the travel direction stays put while it stretches, like on iOS.
fn toggle_knob(track: &Rect, on: f32, press: f32) -> Rect {
    let l0 = track.x + 3.0;
    let l1 = track.right() - 3.0 - TOGGLE_KNOB_W;
    let stretch = STRETCH * press;
    let left = l0 + (l1 - l0) * on - stretch * on;
    let h = TOGGLE_KNOB_H + 2.0 * press;
    Rect::new(left, track.cy() - h * 0.5, TOGGLE_KNOB_W + stretch, h)
}

fn slider_track(row: &Rect) -> Rect {
    Rect::new(
        row.right() - PAD - SLIDER_W,
        row.cy() - SLIDER_TRACK_H * 0.5,
        SLIDER_W,
        SLIDER_TRACK_H,
    )
}

fn slider_knob(track: &Rect, frac: f32, press: f32) -> Rect {
    let kw = SLIDER_KNOB_W + STRETCH * press;
    let kh = SLIDER_KNOB_H + 2.0 * press;
    let cx = track.x + SLIDER_KNOB_W * 0.5 + frac.clamp(0.0, 1.0) * (track.w - SLIDER_KNOB_W);
    Rect::new(cx - kw * 0.5, track.cy() - kh * 0.5, kw, kh)
}

fn slider_zone(track: &Rect) -> Rect {
    track.inflate(SLIDER_KNOB_W * 0.5, 12.0)
}

/// Pointer x -> slider position 0..1 (the knob centre travels inside the track).
fn slider_frac(x: f32, track: &Rect) -> f32 {
    let span = track.w - SLIDER_KNOB_W;
    if span <= 0.0 || !x.is_finite() {
        return 0.0;
    }
    ((x - track.x - SLIDER_KNOB_W * 0.5) / span).clamp(0.0, 1.0)
}

fn zoom_from_frac(f: f32) -> f32 {
    ZOOM_MIN + f.clamp(0.0, 1.0) * (ZOOM_MAX - ZOOM_MIN)
}

fn frac_from_zoom(z: f32) -> f32 {
    ((z - ZOOM_MIN) / (ZOOM_MAX - ZOOM_MIN)).clamp(0.0, 1.0)
}

/// Snaps to the 0.05 grid the slider offers.
fn snap_zoom(z: f32) -> f32 {
    let n = ((z - ZOOM_MIN) / ZOOM_STEP).round();
    (ZOOM_MIN + n * ZOOM_STEP).clamp(ZOOM_MIN, ZOOM_MAX)
}

/// "1.8x", "1.25x", "2.0x": two decimals, one trailing zero trimmed.
fn fmt_zoom(z: f32) -> String {
    let mut s = format!("{z:.2}");
    if s.ends_with('0') {
        s.pop();
    }
    s + "x"
}

fn step_count(n: u32, delta: i32) -> u32 {
    (n as i64 + delta as i64).clamp(RECOMMENDED_MIN as i64, RECOMMENDED_MAX as i64) as u32
}

/// `(minus, value, plus)` rectangles of the stepper.
fn stepper(row: &Rect) -> (Rect, Rect, Rect) {
    let y = row.cy() - STEP_BTN_H * 0.5;
    let plus = Rect::new(row.right() - PAD - STEP_BTN_W, y, STEP_BTN_W, STEP_BTN_H);
    let value = Rect::new(plus.x - STEP_VALUE_W, y, STEP_VALUE_W, STEP_BTN_H);
    let minus = Rect::new(value.x - STEP_BTN_W, y, STEP_BTN_W, STEP_BTN_H);
    (minus, value, plus)
}

fn clear_button(row: &Rect) -> Rect {
    Rect::new(row.cx() - CLEAR_W * 0.5, row.cy() - CLEAR_H * 0.5, CLEAR_W, CLEAR_H)
}

/// Which controls currently react (disabled ones are dimmed and ignored by the pointer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Enabled {
    zoom: bool,
    minus: bool,
    plus: bool,
}

fn enabled(s: &Settings) -> Enabled {
    Enabled {
        zoom: s.magnification,
        minus: s.recommended && s.recommended_count > RECOMMENDED_MIN,
        plus: s.recommended && s.recommended_count < RECOMMENDED_MAX,
    }
}

fn hit_test(l: &Layout, en: &Enabled, x: f32, y: f32) -> Option<Ctl> {
    if l.close.inflate(6.0, 6.0).contains(x, y) {
        return Some(Ctl::Close);
    }
    // A toggle is clickable on its whole row, which is a much bigger target than the capsule.
    if l.rows[ROW_HIDE].contains(x, y) {
        return Some(Ctl::AutoHide);
    }
    if l.rows[ROW_LIVE].contains(x, y) {
        return Some(Ctl::LiveGlass);
    }
    if l.rows[ROW_MAG].contains(x, y) {
        return Some(Ctl::Magnification);
    }
    if l.rows[ROW_REC].contains(x, y) {
        return Some(Ctl::Recommended);
    }
    if en.zoom && slider_zone(&slider_track(&l.rows[ROW_ZOOM])).contains(x, y) {
        return Some(Ctl::Zoom);
    }
    let (minus, _, plus) = stepper(&l.rows[ROW_COUNT]);
    if en.minus && minus.contains(x, y) {
        return Some(Ctl::Minus);
    }
    if en.plus && plus.contains(x, y) {
        return Some(Ctl::Plus);
    }
    if clear_button(&l.rows[ROW_CLEAR]).contains(x, y) {
        return Some(Ctl::Clear);
    }
    None
}

/// Applies a click on `c` to the settings; returns whether something changed.
fn activate(s: &mut Settings, c: Ctl) -> bool {
    let before = s.clone();
    match c {
        Ctl::AutoHide => s.auto_hide = !s.auto_hide,
        Ctl::LiveGlass => s.live_glass = !s.live_glass,
        Ctl::Magnification => s.magnification = !s.magnification,
        Ctl::Recommended => s.recommended = !s.recommended,
        Ctl::Minus => s.recommended_count = step_count(s.recommended_count, -1),
        Ctl::Plus => s.recommended_count = step_count(s.recommended_count, 1),
        Ctl::Close | Ctl::Zoom | Ctl::Clear => {}
    }
    *s != before
}

/// Sets the zoom from a pointer x while dragging; returns whether the value changed.
fn drag_zoom(s: &mut Settings, x: f32, track: &Rect) -> bool {
    let z = snap_zoom(zoom_from_frac(slider_frac(x, track)));
    if (z - s.zoom).abs() > 1e-4 {
        s.zoom = z;
        true
    } else {
        false
    }
}

/// One step of an exponential approach (`tau` seconds), snapping when close enough to settle.
fn approach(cur: f32, target: f32, dt: f32, tau: f32) -> f32 {
    let k = 1.0 - (-dt / tau.max(1e-4)).exp();
    let v = cur + (target - cur) * k;
    if (v - target).abs() < 0.002 {
        target
    } else {
        v
    }
}

/// Buffer size in device px: with a viewport the buffer is `round(logical * scale)` and the
/// viewport maps it back; without, only whole scales are possible.
fn buffer_size(w: u32, h: u32, scale: f32, has_viewport: bool) -> (i32, i32) {
    let s = if has_viewport { scale } else { scale.round() };
    let f = |v: u32| ((v.max(1) as f32 * s).round() as i32).max(1);
    (f(w), f(h))
}

/// Scale to render with: the fractional one when it can be used (it needs the viewport),
/// otherwise the integer scale of the output.
fn effective_scale(frac: Option<f32>, has_viewport: bool, int_scale: i32) -> f32 {
    let s = match frac {
        Some(f) if has_viewport => f,
        _ if has_viewport => int_scale as f32,
        _ => int_scale as f32,
    };
    if s.is_finite() {
        s.clamp(0.5, 8.0)
    } else {
        1.0
    }
}

/// The size to use for a `configure` (0 means "your choice"); never degenerate.
fn pick_size(new: (Option<u32>, Option<u32>)) -> (u32, u32) {
    let w = new.0.unwrap_or(WIN_W as u32).clamp(120, 8192);
    let h = new.1.unwrap_or(WIN_H as u32).clamp(120, 8192);
    (w, h)
}

/// Shortens `text` with an ellipsis until `measure` says it fits in `max_w`.
fn fit_ellipsis(text: &str, max_w: f32, mut measure: impl FnMut(&str) -> f32) -> String {
    if measure(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cand: String = chars[..mid].iter().collect::<String>().trim_end().to_string() + "…";
        if measure(&cand) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
}

/// Rate limiter for `reload` messages while a slider is dragged: at most one per interval, and
/// the last value is never lost (a timer sends the pending one).
#[derive(Default)]
struct ReloadGate {
    last: Option<Instant>,
    pending: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Gate {
    SendNow,
    /// Send later: arm a timer for this long.
    Schedule(Duration),
    /// A timer is already armed.
    Nothing,
}

impl ReloadGate {
    fn change(&mut self, now: Instant) -> Gate {
        match self.last {
            Some(t) if now.duration_since(t) < RELOAD_INTERVAL => {
                if self.pending {
                    Gate::Nothing
                } else {
                    self.pending = true;
                    Gate::Schedule(RELOAD_INTERVAL - now.duration_since(t))
                }
            }
            _ => {
                self.last = Some(now);
                self.pending = false;
                Gate::SendNow
            }
        }
    }
    /// The armed timer fired: true if a reload should be sent now.
    fn timer_fired(&mut self, now: Instant) -> bool {
        if self.pending {
            self.pending = false;
            self.last = Some(now);
            true
        } else {
            false
        }
    }
    /// A reload is sent unconditionally (end of a drag, a click): resets the window.
    fn force(&mut self, now: Instant) {
        self.pending = false;
        self.last = Some(now);
    }
}

// ---------------------------------------------------------------------------------------------
// Interaction state machine (no Wayland).
// ---------------------------------------------------------------------------------------------

/// What the shell has to do after an input event.
#[derive(Default, Debug, PartialEq, Eq)]
struct Effects {
    save: bool,
    /// Send `reload` right now.
    reload_now: bool,
    /// Send `reload`, rate limited (slider drag).
    reload_soon: bool,
    clear_history: bool,
    close: bool,
    move_window: bool,
    redraw: bool,
}

struct Ui {
    s: Settings,
    lay: Layout,
    hover: Option<Ctl>,
    pressed: Option<Ctl>,
    /// "History cleared" is showing instead of the button label.
    cleared: bool,
}

impl Ui {
    fn new(s: Settings, lay: Layout) -> Ui {
        Ui {
            s,
            lay,
            hover: None,
            pressed: None,
            cleared: false,
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<Ctl> {
        hit_test(&self.lay, &enabled(&self.s), x, y)
    }

    fn zoom_track(&self) -> Rect {
        slider_track(&self.lay.rows[ROW_ZOOM])
    }

    fn press(&mut self, x: f32, y: f32) -> Effects {
        let mut fx = Effects {
            redraw: true,
            ..Effects::default()
        };
        if self.pressed.is_some() {
            return fx;
        }
        let hit = self.hit(x, y);
        self.hover = hit;
        match hit {
            None if y < TITLE_H => fx.move_window = true,
            None => {}
            Some(Ctl::Zoom) => {
                self.pressed = hit;
                let track = self.zoom_track();
                if drag_zoom(&mut self.s, x, &track) {
                    fx.save = true;
                    fx.reload_soon = true;
                }
            }
            Some(_) => self.pressed = hit,
        }
        fx
    }

    fn motion(&mut self, x: f32, y: f32) -> Effects {
        let mut fx = Effects::default();
        // While dragging the slider the pointer may be anywhere (it is grabbed), keep following it.
        if self.pressed == Some(Ctl::Zoom) {
            let track = self.zoom_track();
            if drag_zoom(&mut self.s, x, &track) {
                fx.save = true;
                fx.reload_soon = true;
                fx.redraw = true;
            }
            return fx;
        }
        let hit = self.hit(x, y);
        if hit != self.hover {
            self.hover = hit;
            fx.redraw = true;
        }
        fx
    }

    fn release(&mut self, x: f32, y: f32) -> Effects {
        let mut fx = Effects {
            redraw: true,
            ..Effects::default()
        };
        let hit = self.hit(x, y);
        self.hover = hit;
        match self.pressed.take() {
            Some(Ctl::Zoom) => fx.reload_now = true,
            // A press acts on release, and only if the pointer is still on the control.
            Some(c) if hit == Some(c) => match c {
                Ctl::Close => fx.close = true,
                Ctl::Clear => fx.clear_history = true,
                _ => {
                    if activate(&mut self.s, c) {
                        fx.save = true;
                        fx.reload_now = true;
                    }
                }
            },
            _ => {}
        }
        fx
    }

    fn leave(&mut self) -> Effects {
        let mut fx = Effects {
            redraw: self.hover.is_some() || self.pressed.is_some(),
            ..Effects::default()
        };
        self.hover = None;
        if self.pressed.take() == Some(Ctl::Zoom) {
            fx.reload_now = true;
        }
        fx
    }
}

// ---------------------------------------------------------------------------------------------
// Animation state.
// ---------------------------------------------------------------------------------------------

/// Smoothed values the renderer reads. Every one follows a target derived from `Ui`.
struct Anims {
    hover: [f32; CTL_COUNT],
    press: [f32; CTL_COUNT],
    /// Slide position of the toggles: magnification, recommended, auto-hide, live glass.
    on: [f32; 4],
    /// 1 = enabled: zoom row, count row, minus, plus.
    en: [f32; 4],
    cleared: f32,
}

struct Targets {
    hover: [f32; CTL_COUNT],
    press: [f32; CTL_COUNT],
    on: [f32; 4],
    en: [f32; 4],
    cleared: f32,
}

fn targets(ui: &Ui) -> Targets {
    let f = |b: bool| b as u32 as f32;
    let mut hover = [0.0; CTL_COUNT];
    let mut press = [0.0; CTL_COUNT];
    for c in ALL_CTLS {
        hover[c.idx()] = f(ui.hover == Some(c));
        // The slider keeps its pressed look while dragged outside of the control; a button only
        // while the pointer is still over it (releasing there would cancel the click).
        press[c.idx()] = f(ui.pressed == Some(c) && (c == Ctl::Zoom || ui.hover == Some(c)));
    }
    let en = enabled(&ui.s);
    Targets {
        hover,
        press,
        on: [
            f(ui.s.magnification),
            f(ui.s.recommended),
            f(ui.s.auto_hide),
            f(ui.s.live_glass),
        ],
        en: [
            f(en.zoom),
            f(ui.s.recommended),
            f(en.minus || !ui.s.recommended),
            f(en.plus || !ui.s.recommended),
        ],
        cleared: f(ui.cleared),
    }
}

impl Anims {
    /// Starts settled on the targets (no animation when the window opens).
    fn new(ui: &Ui) -> Anims {
        let t = targets(ui);
        Anims {
            hover: t.hover,
            press: t.press,
            on: t.on,
            en: t.en,
            cleared: t.cleared,
        }
    }

    fn snap(&mut self, ui: &Ui) {
        *self = Anims::new(ui);
    }

    /// Advances by `dt` seconds; returns true while anything still moves.
    fn step(&mut self, ui: &Ui, dt: f32) -> bool {
        let t = targets(ui);
        let mut busy = false;
        let mut go = |cur: &mut f32, target: f32, tau: f32| {
            *cur = approach(*cur, target, dt, tau);
            busy |= *cur != target;
        };
        for i in 0..CTL_COUNT {
            go(&mut self.hover[i], t.hover[i], 0.060);
            // Faster in than out would feel jumpy; a single time constant is enough.
            go(&mut self.press[i], t.press[i], 0.045);
        }
        for i in 0..4 {
            // About 4 time constants = the 150-200 ms slide of the brief.
            go(&mut self.on[i], t.on[i], 0.045);
        }
        for i in 0..4 {
            go(&mut self.en[i], t.en[i], 0.080);
        }
        go(&mut self.cleared, t.cleared, 0.070);
        busy
    }
}

// ---------------------------------------------------------------------------------------------
// Debug hook
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum DebugCmd {
    Toggle(Ctl),
    Slider(f32),
    Count(u32),
    Hover(Ctl),
    Press(Ctl),
    Cleared,
    Scale(f32),
}

fn parse_ctl(s: &str) -> Option<Ctl> {
    match s.trim().to_ascii_lowercase().as_str() {
        "0" | "close" => Some(Ctl::Close),
        "1" | "mag" | "magnification" => Some(Ctl::Magnification),
        "2" | "zoom" | "slider" => Some(Ctl::Zoom),
        "3" | "rec" | "recommended" => Some(Ctl::Recommended),
        "4" | "minus" => Some(Ctl::Minus),
        "5" | "plus" => Some(Ctl::Plus),
        "6" | "clear" => Some(Ctl::Clear),
        "7" | "hide" | "autohide" => Some(Ctl::AutoHide),
        "8" | "live" => Some(Ctl::LiveGlass),
        _ => None,
    }
}

/// Parses `GLASSDOCK_SETTINGS_DEBUG`, e.g. `toggle:mag,slider:0.6,hover:clear,press:plus,cleared,@800:toggle:rec`.
/// Only used to put the window into a state without a mouse (screenshots); unknown items are ignored.
fn parse_debug(spec: &str) -> Vec<(u64, DebugCmd)> {
    let mut out = vec![];
    for item in spec.split(',') {
        // `@800:toggle:mag` applies the command 800 ms after the start (to exercise the animations).
        let (delay, item) = match item.trim().strip_prefix('@').and_then(|r| r.split_once(':')) {
            Some((ms, rest)) => (ms.parse::<u64>().unwrap_or(0).min(60_000), rest),
            None => (0, item),
        };
        let (k, v) = item.split_once(':').unwrap_or((item, ""));
        let cmd = match k.trim() {
            "toggle" => parse_ctl(v).map(DebugCmd::Toggle),
            "slider" => v
                .trim()
                .parse::<f32>()
                .ok()
                .filter(|f| f.is_finite())
                .map(DebugCmd::Slider),
            "count" => v.trim().parse::<u32>().ok().map(DebugCmd::Count),
            "hover" => parse_ctl(v).map(DebugCmd::Hover),
            "press" => parse_ctl(v).map(DebugCmd::Press),
            "cleared" => Some(DebugCmd::Cleared),
            "scale" => v
                .trim()
                .parse::<f32>()
                .ok()
                .filter(|f| f.is_finite())
                .map(DebugCmd::Scale),
            _ => None,
        };
        out.extend(cmd.map(|c| (delay, c)));
    }
    out
}

/// Applies debug commands to the in-memory state only (nothing is saved, nothing is reloaded).
/// Returns a forced scale, if any.
fn apply_debug(ui: &mut Ui, cmds: &[DebugCmd]) -> Option<f32> {
    let mut scale = None;
    for c in cmds {
        match c {
            DebugCmd::Toggle(ctl) => {
                activate(&mut ui.s, *ctl);
            }
            DebugCmd::Slider(f) => ui.s.zoom = snap_zoom(zoom_from_frac(*f)),
            DebugCmd::Count(n) => ui.s.recommended_count = (*n).clamp(RECOMMENDED_MIN, RECOMMENDED_MAX),
            DebugCmd::Hover(ctl) => ui.hover = Some(*ctl),
            DebugCmd::Press(ctl) => {
                ui.hover = Some(*ctl);
                ui.pressed = Some(*ctl);
            }
            DebugCmd::Cleared => ui.cleared = true,
            DebugCmd::Scale(s) => scale = Some(*s),
        }
    }
    scale
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

/// One shader that draws every box of the page: a rounded rectangle with a vertical gradient fill,
/// a rim that is brighter towards the top-left light, a specular line on the lit edges, an inner
/// glow, an inner shade on the shadowed edges and a soft drop shadow. Rotation is supported so
/// the "x" of the close button can be two pills. Output is premultiplied alpha.
const UI_FRAG: &str = r#"#version 300 es
precision highp float;
uniform vec4 u_rect;       // x, y, w, h (device px)
uniform float u_radius;
uniform float u_rot;       // radians, around the centre
uniform vec4 u_top;        // premultiplied fill at the top edge
uniform vec4 u_bot;        // premultiplied fill at the bottom edge
uniform vec4 u_stroke_a;   // rim, top-left (premultiplied)
uniform vec4 u_stroke_b;   // rim, bottom-right (premultiplied)
uniform float u_stroke_w;  // device px
uniform float u_spec;      // specular line on the edges facing the light
uniform float u_glow;      // soft inner glow, strongest at the top
uniform float u_shade;     // inner shade on the edges facing away from the light
uniform vec4 u_shadow;     // alpha, softness (px), y offset (px), unused
uniform float u_scale;
in vec2 v_px;
out vec4 o_color;
float sd_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
void main() {
    vec2 hb = 0.5 * u_rect.zw;
    vec2 pc = v_px - (u_rect.xy + hb);
    float cs = cos(u_rot), sn = sin(u_rot);
    pc = vec2(cs * pc.x + sn * pc.y, -sn * pc.x + cs * pc.y);
    float r = min(u_radius, min(hb.x, hb.y));
    float d = sd_box(pc, hb, r);                       // < 0 inside
    float cov = clamp(0.5 - d, 0.0, 1.0);
    float inner = clamp(0.5 - (d + u_stroke_w), 0.0, 1.0);

    float t = clamp((pc.y + hb.y) / max(2.0 * hb.y, 1.0), 0.0, 1.0);
    vec4 fill = mix(u_top, u_bot, t);
    float k = clamp((pc.x / hb.x + pc.y / hb.y) * 0.25 + 0.5, 0.0, 1.0);
    vec4 stroke = mix(u_stroke_a, u_stroke_b, k);
    float ring = cov - inner;
    vec4 col = fill * cov;
    col = stroke * ring + col * (1.0 - stroke.a * ring);

    // outward normal of the box
    vec2 q = abs(pc) - (hb - r);
    vec2 mq = max(q, 0.0);
    float ml = length(mq);
    vec2 g = ml > 1e-4 ? mq / ml : (q.x > q.y ? vec2(1.0, 0.0) : vec2(0.0, 1.0));
    g *= vec2(pc.x < 0.0 ? -1.0 : 1.0, pc.y < 0.0 ? -1.0 : 1.0);
    float depth = max(-d, 0.0);
    vec2 L = normalize(vec2(-0.40, -0.92));
    float kd = dot(g, L);
    float key = max(kd, 0.0);
    float cnt = max(-kd, 0.0);

    float spec = u_spec * (pow(key, 1.5) * exp(-depth / (1.15 * u_scale)) * 0.85
                         + pow(key, 2.0) * exp(-depth / (5.0 * u_scale)) * 0.16);
    float glow = u_glow * exp(-depth / (9.0 * u_scale)) * (0.35 + 0.65 * (0.5 - 0.5 * g.y)) * 0.20;
    float a = clamp(spec + glow, 0.0, 1.0) * cov;
    col = col * (1.0 - a) + vec4(a);
    float sh = u_shade * pow(cnt, 1.3) * exp(-depth / (4.5 * u_scale)) * 0.30 * cov;
    col = col * (1.0 - sh) + vec4(0.0, 0.0, 0.0, sh);

    float shadow = 0.0;
    if (u_shadow.x > 0.0) {
        float ds = sd_box(pc - vec2(0.0, u_shadow.z), hb, r);
        shadow = u_shadow.x * (1.0 - smoothstep(-u_shadow.y, u_shadow.y, ds)) * (1.0 - cov);
    }
    o_color = col + vec4(0.0, 0.0, 0.0, shadow) * (1.0 - col.a);
}
"#;

/// Straight-alpha RGBA.
type Color = [f32; 4];

fn white(a: f32) -> Color {
    [1.0, 1.0, 1.0, a]
}

fn with_alpha(c: Color, k: f32) -> Color {
    [c[0], c[1], c[2], (c[3] * k).clamp(0.0, 1.0)]
}

fn lerp4(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    [0, 1, 2, 3].map(|i| a[i] + (b[i] - a[i]) * t)
}

fn premul(c: Color) -> [f32; 4] {
    [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]]
}

/// Look of one box (logical units; the renderer scales them).
#[derive(Clone, Copy)]
struct Style {
    top: Color,
    bot: Color,
    stroke_a: Color,
    stroke_b: Color,
    stroke_w: f32,
    spec: f32,
    glow: f32,
    shade: f32,
    /// alpha, softness, y offset
    shadow: [f32; 3],
    rot: f32,
}

impl Style {
    const NONE: Style = Style {
        top: [0.0; 4],
        bot: [0.0; 4],
        stroke_a: [0.0; 4],
        stroke_b: [0.0; 4],
        stroke_w: 0.0,
        spec: 0.0,
        glow: 0.0,
        shade: 0.0,
        shadow: [0.0; 3],
        rot: 0.0,
    };
    fn flat(c: Color) -> Style {
        Style {
            top: c,
            bot: c,
            ..Style::NONE
        }
    }
    fn fade(&self, k: f32) -> Style {
        Style {
            top: with_alpha(self.top, k),
            bot: with_alpha(self.bot, k),
            stroke_a: with_alpha(self.stroke_a, k),
            stroke_b: with_alpha(self.stroke_b, k),
            spec: self.spec * k,
            glow: self.glow * k,
            shade: self.shade * k,
            shadow: [self.shadow[0] * k, self.shadow[1], self.shadow[2]],
            ..*self
        }
    }
}

const GREEN: [f32; 3] = [0.30, 0.82, 0.45];
const TEXT: [f32; 3] = [0.96, 0.97, 1.0];

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

struct TextTex {
    tex: Tex,
    /// Everything in device px.
    w: f32,
    h: f32,
    baseline: f32,
    origin_x: f32,
    adv: f32,
}

struct UiLocs {
    rect: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    rot: Option<glow::UniformLocation>,
    top: Option<glow::UniformLocation>,
    bot: Option<glow::UniformLocation>,
    stroke_a: Option<glow::UniformLocation>,
    stroke_b: Option<glow::UniformLocation>,
    stroke_w: Option<glow::UniformLocation>,
    spec: Option<glow::UniformLocation>,
    glow: Option<glow::UniformLocation>,
    shade: Option<glow::UniformLocation>,
    shadow: Option<glow::UniformLocation>,
    scale: Option<glow::UniformLocation>,
    surface: Option<glow::UniformLocation>,
}

/// Draws a `Ui` snapshot. Owns the GL objects (created with the context current).
struct Renderer {
    gfx: Gfx,
    prog: glow::Program,
    locs: UiLocs,
    text: Option<TextRenderer>,
    labels: HashMap<(String, u32, bool), TextTex>,
    fitted: HashMap<(String, u32, u32), String>,
}

/// Per-frame drawing context: scale and surface size.
#[derive(Clone, Copy)]
struct Canvas {
    surf: (f32, f32),
    s: f32,
}

impl Renderer {
    fn new(gl: glow::Context) -> Result<Renderer> {
        // The glass shader is not used by this window, so the cheap fallback program is enough.
        let gfx = Gfx::new(gl, None, None)?;
        // SAFETY: plain GL calls; the context created by `ensure_egl` is current on this thread.
        let (prog, locs) = unsafe {
            let gl = &gfx.gl;
            let p = gl.create_program().map_err(|e| anyhow!(e))?;
            let mut shaders = vec![];
            for (kind, src) in [(glow::VERTEX_SHADER, gfx::VERT), (glow::FRAGMENT_SHADER, UI_FRAG)] {
                let sh = gl.create_shader(kind).map_err(|e| anyhow!(e))?;
                gl.shader_source(sh, src);
                gl.compile_shader(sh);
                if !gl.get_shader_compile_status(sh) {
                    return Err(anyhow!("ui shader: {}", gl.get_shader_info_log(sh)));
                }
                gl.attach_shader(p, sh);
                shaders.push(sh);
            }
            // Same attribute layout as the shared quad buffer of `Gfx`.
            gl.bind_attrib_location(p, 0, "a_pos");
            gl.bind_attrib_location(p, 1, "a_uv");
            gl.link_program(p);
            if !gl.get_program_link_status(p) {
                return Err(anyhow!("ui shader link: {}", gl.get_program_info_log(p)));
            }
            for sh in shaders {
                gl.detach_shader(p, sh);
                gl.delete_shader(sh);
            }
            let u = |n: &str| gl.get_uniform_location(p, n);
            let locs = UiLocs {
                rect: u("u_rect"),
                radius: u("u_radius"),
                rot: u("u_rot"),
                top: u("u_top"),
                bot: u("u_bot"),
                stroke_a: u("u_stroke_a"),
                stroke_b: u("u_stroke_b"),
                stroke_w: u("u_stroke_w"),
                spec: u("u_spec"),
                glow: u("u_glow"),
                shade: u("u_shade"),
                shadow: u("u_shadow"),
                scale: u("u_scale"),
                surface: u("u_surface"),
            };
            (p, locs)
        };
        let text = TextRenderer::new();
        if text.is_none() {
            eprintln!("glassdock settings: no usable font found, text will be missing");
        }
        Ok(Renderer {
            gfx,
            prog,
            locs,
            text,
            labels: HashMap::new(),
            fitted: HashMap::new(),
        })
    }

    /// Forgets cached text (its pixel size depends on the scale).
    fn clear_text_cache(&mut self) {
        for (_, t) in self.labels.drain() {
            self.gfx.delete_tex(t.tex);
        }
        self.fitted.clear();
    }

    /// One styled box at a logical rectangle.
    fn boxed(&self, cv: Canvas, r: Rect, radius: f32, st: &Style) {
        let s = cv.s;
        let gl = &self.gfx.gl;
        let (x, y, w, h) = (r.x * s, r.y * s, r.w * s, r.h * s);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let l = &self.locs;
        // SAFETY: plain GL calls on the current context; the quad buffer bound by `begin_frame` has room
        // for the 6 vertices written here.
        unsafe {
            gl.use_program(Some(self.prog));
            gl.uniform_2_f32(l.surface.as_ref(), cv.surf.0, cv.surf.1);
            gl.uniform_4_f32(l.rect.as_ref(), x, y, w, h);
            gl.uniform_1_f32(l.radius.as_ref(), radius * s);
            gl.uniform_1_f32(l.rot.as_ref(), st.rot);
            let t = premul(st.top);
            gl.uniform_4_f32(l.top.as_ref(), t[0], t[1], t[2], t[3]);
            let b = premul(st.bot);
            gl.uniform_4_f32(l.bot.as_ref(), b[0], b[1], b[2], b[3]);
            let a = premul(st.stroke_a);
            gl.uniform_4_f32(l.stroke_a.as_ref(), a[0], a[1], a[2], a[3]);
            let c = premul(st.stroke_b);
            gl.uniform_4_f32(l.stroke_b.as_ref(), c[0], c[1], c[2], c[3]);
            gl.uniform_1_f32(l.stroke_w.as_ref(), st.stroke_w * s);
            gl.uniform_1_f32(l.spec.as_ref(), st.spec);
            gl.uniform_1_f32(l.glow.as_ref(), st.glow);
            gl.uniform_1_f32(l.shade.as_ref(), st.shade);
            gl.uniform_4_f32(l.shadow.as_ref(), st.shadow[0], st.shadow[1] * s, st.shadow[2] * s, 0.0);
            gl.uniform_1_f32(l.scale.as_ref(), s);
            // The quad covers the box, its shadow and (when rotated) its bounding circle.
            let m = if st.shadow[0] > 0.0 {
                (st.shadow[1] * 1.2 + st.shadow[2].abs()) * s
            } else {
                0.0
            } + 2.0;
            let (cx, cy) = (x + w * 0.5, y + h * 0.5);
            let half = if st.rot != 0.0 {
                (w * w + h * h).sqrt() * 0.5 + m
            } else {
                f32::max(w, h) * 0.5 + m
            };
            let (hx, hy) = if st.rot != 0.0 {
                (half, half)
            } else {
                (w * 0.5 + m, h * 0.5 + m)
            };
            let (x0, y0, x1, y1) = (cx - hx, cy - hy, cx + hx, cy + hy);
            let v: [f32; 24] = [
                x0, y0, 0.0, 0.0, x1, y0, 0.0, 0.0, x0, y1, 0.0, 0.0, x1, y0, 0.0, 0.0, x1, y1, 0.0, 0.0, x0, y1, 0.0,
                0.0,
            ];
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytemuck::cast_slice(&v));
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
        }
    }

    /// Text with its baseline at logical `y`; `x` is the left, centre or right edge depending on `align`.
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, cv: Canvas, txt: &str, px: f32, bold: bool, align: Align, x: f32, y: f32, color: Color) {
        if color[3] <= 0.003 || txt.is_empty() {
            return;
        }
        let key = (txt.to_string(), (px * cv.s * 10.0) as u32, bold);
        if !self.labels.contains_key(&key) {
            let Some(tr) = self.text.as_mut() else { return };
            let img = tr.render_ex(txt, px * cv.s, bold);
            let Ok(tex) = self
                .gfx
                .upload_rgba(img.bitmap.w as i32, img.bitmap.h as i32, &img.bitmap.rgba, false)
            else {
                return;
            };
            self.labels.insert(
                key.clone(),
                TextTex {
                    tex,
                    w: img.bitmap.w as f32,
                    h: img.bitmap.h as f32,
                    baseline: img.baseline as f32,
                    origin_x: img.origin_x as f32,
                    adv: img.advance,
                },
            );
        }
        let t = &self.labels[&key];
        let s = cv.s;
        let left = match align {
            Align::Left => x * s,
            Align::Center => x * s - t.adv * 0.5,
            Align::Right => x * s - t.adv,
        };
        // Snap to whole device pixels: crisp stems.
        let px0 = (left - t.origin_x).round();
        let py0 = (y * s - t.baseline).round();
        let p = premul(color);
        self.gfx.texture(cv.surf, &t.tex, [px0, py0, t.w, t.h], p, true);
    }

    /// Width in logical px.
    fn measure(&mut self, txt: &str, px: f32, bold: bool) -> f32 {
        self.text.as_mut().map_or(0.0, |t| t.measure(txt, px, bold).0)
    }

    /// `txt` shortened to `max_w` logical px (memoised).
    fn fit(&mut self, txt: &str, px: f32, bold: bool, max_w: f32) -> String {
        let key = (txt.to_string(), (px * 10.0) as u32, (max_w * 10.0) as u32);
        if let Some(s) = self.fitted.get(&key) {
            return s.clone();
        }
        let out = fit_ellipsis(txt, max_w, |t| self.measure(t, px, bold));
        self.fitted.insert(key, out.clone());
        out
    }

    fn frame(&mut self, bw: i32, bh: i32, scale: f32, ui: &Ui, an: &Anims, active: bool) {
        self.gfx.begin_frame(bw, bh);
        let cv = Canvas {
            surf: (bw as f32, bh as f32),
            s: scale,
        };
        let l = &ui.lay;
        self.draw_window(cv, l, active);
        self.draw_page(cv, ui, an);
        self.draw_titlebar(cv, ui, an, active);
    }

    fn draw_window(&self, cv: Canvas, l: &Layout, active: bool) {
        let full = Rect::new(0.0, 0.0, l.w, l.h);
        // Dark tinted base (the compositor blurs what is behind), keeps the text readable on any wallpaper.
        let tint_k = if active { 1.0 } else { 0.95 };
        let base = Style {
            top: [0.085, 0.095, 0.135, 0.68 * tint_k],
            bot: [0.045, 0.052, 0.080, 0.78 * tint_k],
            stroke_a: white(0.60),
            stroke_b: white(0.15),
            stroke_w: 1.0,
            spec: 0.55,
            glow: 0.9,
            ..Style::NONE
        };
        self.boxed(cv, full, WIN_RADIUS, &base);
        // Soft vertical highlight on the upper part.
        let hi = Style {
            top: white(0.11),
            bot: white(0.0),
            ..Style::NONE
        };
        self.boxed(cv, Rect::new(0.0, 0.0, l.w, l.h * 0.55), WIN_RADIUS, &hi);
    }

    fn draw_titlebar(&mut self, cv: Canvas, ui: &Ui, an: &Anims, active: bool) {
        let l = &ui.lay;
        let title = tr(Msg::SettingsTitle);
        let a = if active { 0.92 } else { 0.55 };
        self.text(
            cv,
            title,
            13.0,
            true,
            Align::Center,
            l.w * 0.5,
            TITLE_H * 0.5 + 4.6,
            with_alpha(rgb(TEXT), a),
        );

        // Close button: a red dot, an "x" appears on hover.
        let c = l.close;
        let hv = an.hover[Ctl::Close.idx()];
        let pr = an.press[Ctl::Close.idx()];
        let (top, bot) = if active {
            (
                lerp4([1.0, 0.45, 0.41, 1.0], [1.0, 0.36, 0.32, 1.0], pr),
                lerp4([0.96, 0.30, 0.27, 1.0], [0.88, 0.22, 0.20, 1.0], pr),
            )
        } else {
            ([0.58, 0.60, 0.66, 0.75], [0.46, 0.48, 0.54, 0.75])
        };
        let dot = Style {
            top,
            bot,
            stroke_a: white(0.55),
            stroke_b: [0.3, 0.02, 0.02, 0.35],
            stroke_w: 1.0,
            spec: 0.9,
            shadow: [0.25, 3.0, 1.0],
            ..Style::NONE
        };
        self.boxed(cv, c, CLOSE_D * 0.5, &dot);
        if hv > 0.01 {
            let ink = Style {
                rot: std::f32::consts::FRAC_PI_4,
                ..Style::flat([0.38, 0.04, 0.03, 0.85 * hv])
            };
            let ink2 = Style {
                rot: -std::f32::consts::FRAC_PI_4,
                ..ink
            };
            let arm = Rect::new(c.cx() - 3.6, c.cy() - 0.8, 7.2, 1.6);
            self.boxed(cv, arm, 0.8, &ink);
            self.boxed(cv, arm, 0.8, &ink2);
        }
    }

    fn draw_page(&mut self, cv: Canvas, ui: &Ui, an: &Anims) {
        let l = &ui.lay;
        let s = &ui.s;

        // Cards.
        let card = Style {
            top: white(0.090),
            bot: white(0.045),
            stroke_a: white(0.22),
            stroke_b: white(0.07),
            stroke_w: 1.0,
            spec: 0.55,
            glow: 0.5,
            ..Style::NONE
        };
        for c in &l.cards {
            self.boxed(cv, *c, CARD_RADIUS, &card);
        }
        for sep in l.separators() {
            self.boxed(cv, sep, 0.0, &Style::flat(white(0.085)));
        }

        // Section headers.
        let head_col = with_alpha(rgb(TEXT), 0.62);
        for (i, msg) in [Msg::SecAppearance, Msg::SecRecommended].into_iter().enumerate() {
            let h = &l.heads[i];
            self.text(
                cv,
                tr(msg),
                12.0,
                true,
                Align::Left,
                h.x + 8.0,
                h.bottom() - 9.0,
                head_col,
            );
        }

        let dim = |e: f32| 1.0 - 0.58 * (1.0 - e);
        // Rows: (row, label, hint, dimming)
        let rows = [
            (ROW_HIDE, Msg::AutoHide, Some(Msg::AutoHideHint), 1.0),
            (ROW_LIVE, Msg::LiveGlass, Some(Msg::LiveGlassHint), 1.0),
            (ROW_MAG, Msg::Magnification, Some(Msg::MagnificationHint), 1.0),
            (ROW_ZOOM, Msg::Zoom, None, dim(an.en[0])),
            (ROW_REC, Msg::ShowRecommended, Some(Msg::ShowRecommendedHint), 1.0),
            (ROW_COUNT, Msg::RecommendedCount, None, dim(an.en[1])),
        ];
        for (ri, label, hint, dm) in rows {
            let row = l.rows[ri];
            let x = row.x + PAD;
            // The widest control of every row is 170 px (slider + value) or the toggle.
            let ctl_w = match ri {
                ROW_ZOOM => SLIDER_W + 70.0,
                ROW_COUNT => 2.0 * STEP_BTN_W + STEP_VALUE_W + 12.0,
                _ => TOGGLE_W + 12.0,
            };
            let max_w = row.w - 2.0 * PAD - ctl_w;
            let lab = self.fit(tr(label), 14.0, false, max_w);
            match hint {
                Some(h) => {
                    self.text(
                        cv,
                        &lab,
                        14.0,
                        false,
                        Align::Left,
                        x,
                        row.y + 27.0,
                        with_alpha(rgb(TEXT), 0.95 * dm),
                    );
                    let hs = self.fit(tr(h), 11.5, false, max_w);
                    self.text(
                        cv,
                        &hs,
                        11.5,
                        false,
                        Align::Left,
                        x,
                        row.y + 46.0,
                        with_alpha(rgb(TEXT), 0.60 * dm),
                    );
                }
                None => {
                    self.text(
                        cv,
                        &lab,
                        14.0,
                        false,
                        Align::Left,
                        x,
                        row.cy() + 5.0,
                        with_alpha(rgb(TEXT), 0.95 * dm),
                    );
                }
            }
        }

        // Toggles.
        self.draw_toggle(cv, &l.rows[ROW_HIDE], an.on[2], Ctl::AutoHide, an, 1.0);
        self.draw_toggle(cv, &l.rows[ROW_LIVE], an.on[3], Ctl::LiveGlass, an, 1.0);
        self.draw_toggle(cv, &l.rows[ROW_MAG], an.on[0], Ctl::Magnification, an, 1.0);
        self.draw_toggle(cv, &l.rows[ROW_REC], an.on[1], Ctl::Recommended, an, 1.0);

        // Zoom slider and its value.
        let zr = l.rows[ROW_ZOOM];
        let dz = dim(an.en[0]);
        let track = slider_track(&zr);
        self.text(
            cv,
            &fmt_zoom(s.zoom),
            13.0,
            true,
            Align::Right,
            track.x - SLIDER_KNOB_W * 0.5 - 8.0,
            zr.cy() + 4.6,
            with_alpha(rgb(TEXT), 0.80 * dz),
        );
        self.draw_slider(cv, &track, frac_from_zoom(s.zoom), an, dz, an.en[0]);

        // Stepper.
        let cr = l.rows[ROW_COUNT];
        let dc = dim(an.en[1]);
        let (minus, value, plus) = stepper(&cr);
        self.draw_pill(cv, minus, Ctl::Minus, an, dc * (0.45 + 0.55 * an.en[2]));
        self.draw_pill(cv, plus, Ctl::Plus, an, dc * (0.45 + 0.55 * an.en[3]));
        let ink = with_alpha(rgb(TEXT), 0.95 * dc);
        let minus_ink = with_alpha(ink, 0.45 + 0.55 * an.en[2]);
        let plus_ink = with_alpha(ink, 0.45 + 0.55 * an.en[3]);
        let (mr, pr) = (
            minus.scaled(1.0 - 0.02 * an.press[Ctl::Minus.idx()]),
            plus.scaled(1.0 - 0.02 * an.press[Ctl::Plus.idx()]),
        );
        self.boxed(
            cv,
            Rect::new(mr.cx() - 5.0, mr.cy() - 1.0, 10.0, 2.0),
            1.0,
            &Style::flat(minus_ink),
        );
        self.boxed(
            cv,
            Rect::new(pr.cx() - 5.0, pr.cy() - 1.0, 10.0, 2.0),
            1.0,
            &Style::flat(plus_ink),
        );
        self.boxed(
            cv,
            Rect::new(pr.cx() - 1.0, pr.cy() - 5.0, 2.0, 10.0),
            1.0,
            &Style::flat(plus_ink),
        );
        self.text(
            cv,
            &s.recommended_count.to_string(),
            15.0,
            true,
            Align::Center,
            value.cx(),
            value.cy() + 5.4,
            ink,
        );

        // Clear history button.
        let br = clear_button(&l.rows[ROW_CLEAR]);
        self.draw_pill(cv, br, Ctl::Clear, an, 1.0);
        let k = an.press[Ctl::Clear.idx()];
        let r = br.scaled(1.0 - 0.02 * k);
        self.text(
            cv,
            tr(Msg::ClearHistory),
            13.0,
            true,
            Align::Center,
            r.cx(),
            r.cy() + 4.6,
            [1.0, 0.60, 0.57, 0.97 * (1.0 - an.cleared)],
        );
        self.text(
            cv,
            tr(Msg::HistoryCleared),
            13.0,
            true,
            Align::Center,
            r.cx(),
            r.cy() + 4.6,
            [0.56, 0.93, 0.66, 0.95 * an.cleared],
        );

        // Footer.
        let foot = format!("GlassDock  ·  {}", env!("CARGO_PKG_VERSION"));
        self.text(
            cv,
            &foot,
            11.0,
            false,
            Align::Center,
            l.w * 0.5,
            l.footer_y,
            with_alpha(rgb(TEXT), 0.36),
        );
    }

    /// The glass knob shared by toggle and slider. `press` 0..1 makes it clearer (see-through).
    fn knob(&self, cv: Canvas, r: Rect, hover: f32, press: f32, fade: f32) {
        let clear = 0.42 * press;
        let st = Style {
            top: white((0.99 - clear) * fade),
            bot: [0.88, 0.91, 0.97, (0.93 - clear) * fade],
            stroke_a: white((1.0 - 0.1 * press) * fade),
            stroke_b: [0.75, 0.82, 1.0, 0.55 * fade],
            stroke_w: 1.0,
            spec: (0.95 + 0.25 * hover) * fade,
            glow: 0.0,
            shade: 1.0 * fade,
            shadow: [(0.40 - 0.14 * press) * fade, 4.5, 2.5],
            rot: 0.0,
        };
        self.boxed(cv, r, r.h * 0.5, &st);
    }

    fn draw_toggle(&self, cv: Canvas, row: &Rect, on: f32, ctl: Ctl, an: &Anims, fade: f32) {
        let hv = an.hover[ctl.idx()];
        let pr = an.press[ctl.idx()];
        let tr_ = toggle_track(row);
        let off_top = [0.80, 0.82, 0.90, 0.26 + 0.05 * hv];
        let off_bot = [0.72, 0.75, 0.84, 0.34 + 0.05 * hv];
        let on_top = [GREEN[0] + 0.05, GREEN[1] + 0.04, GREEN[2] + 0.05, 1.0];
        let on_bot = [GREEN[0] - 0.04, GREEN[1] - 0.06, GREEN[2] - 0.04, 1.0];
        let track = Style {
            top: lerp4(off_top, on_top, on),
            bot: lerp4(off_bot, on_bot, on),
            stroke_a: white(0.28),
            stroke_b: white(0.08),
            stroke_w: 1.0,
            spec: 0.35,
            glow: 0.0,
            shade: 0.5,
            ..Style::NONE
        }
        .fade(fade);
        self.boxed(cv, tr_, tr_.h * 0.5, &track);
        self.knob(cv, toggle_knob(&tr_, on, pr), hv, pr, fade);
    }

    fn draw_slider(&self, cv: Canvas, track: &Rect, frac: f32, an: &Anims, fade: f32, en: f32) {
        let hv = an.hover[Ctl::Zoom.idx()];
        let pr = an.press[Ctl::Zoom.idx()];
        let empty = Style {
            top: [0.8, 0.82, 0.9, 0.20],
            bot: [0.8, 0.82, 0.9, 0.28],
            stroke_a: white(0.16),
            stroke_b: white(0.05),
            stroke_w: 1.0,
            ..Style::NONE
        }
        .fade(fade);
        self.boxed(cv, *track, track.h * 0.5, &empty);
        let k = slider_knob(track, frac, pr);
        let filled = Rect::new(track.x, track.y, (k.cx() - track.x).max(0.0), track.h);
        if filled.w > 0.5 {
            // A disabled slider loses its colour as well.
            let grey = [0.78, 0.80, 0.86, 0.55];
            let st = Style {
                top: lerp4(grey, [GREEN[0] + 0.05, GREEN[1] + 0.04, GREEN[2] + 0.05, 1.0], en),
                bot: lerp4(grey, [GREEN[0] - 0.04, GREEN[1] - 0.06, GREEN[2] - 0.04, 1.0], en),
                ..Style::NONE
            }
            .fade(fade);
            self.boxed(cv, filled, track.h * 0.5, &st);
        }
        self.knob(cv, k, hv, pr, fade);
    }

    /// Glass pill used by the stepper buttons and the clear button: brighter on hover, darker
    /// and 0.98 scaled when pressed.
    fn draw_pill(&self, cv: Canvas, r: Rect, ctl: Ctl, an: &Anims, fade: f32) {
        let hv = an.hover[ctl.idx()];
        let pr = an.press[ctl.idx()];
        let r = r.scaled(1.0 - 0.02 * pr);
        let st = Style {
            top: white(0.20 + 0.10 * hv - 0.09 * pr),
            bot: white(0.12 + 0.08 * hv - 0.06 * pr),
            stroke_a: white(0.42 - 0.10 * pr),
            stroke_b: white(0.11),
            stroke_w: 1.0,
            spec: 0.75 - 0.35 * pr,
            glow: 0.5,
            shade: 0.6 + 0.6 * pr,
            shadow: [0.22 - 0.10 * pr, 4.0, 2.0],
            rot: 0.0,
        }
        .fade(fade);
        self.boxed(cv, r, r.h * 0.5, &st);
    }
}

fn rgb(c: [f32; 3]) -> Color {
    [c[0], c[1], c[2], 1.0]
}

// ---------------------------------------------------------------------------------------------
// Wayland shell
// ---------------------------------------------------------------------------------------------

struct SettingsApp {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    qh: QueueHandle<SettingsApp>,
    loop_handle: LoopHandle<'static, SettingsApp>,
    window: Window,
    viewport: Option<WpViewport>,
    _frac: Option<WpFractionalScaleV1>,
    frac_scale: Option<f32>,
    int_scale: i32,
    egl: gfx::EglCtx,
    egl_win: *mut c_void,
    egl_surf: Option<khronos_egl::Surface>,
    renderer: Option<Renderer>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    cursor_mgr: Option<WpCursorShapeManagerV1>,
    cursor_dev: Option<WpCursorShapeDeviceV1>,
    enter_serial: u32,
    cursor_is_pointer: bool,
    ctrl_down: bool,
    active: bool,
    size: (u32, u32),
    scale: f32,
    buf: (i32, i32),
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    last_draw: Instant,
    ui: Ui,
    an: Anims,
    gate: ReloadGate,
    cleared_until: Option<Instant>,
    reload_threads: Vec<std::thread::JoinHandle<()>>,
    forced_scale: Option<f32>,
    /// The debug hook is active: pointer input is ignored so the forced state stays put.
    debug: bool,
    draws: u64,
    quit: bool,
}

/// State of the single-instance lock.
enum Lock {
    /// We hold it (keep the file open until the process ends).
    Acquired(#[allow(dead_code)] std::fs::File),
    /// Another settings window holds it.
    Busy,
    /// The lock file cannot be used: go ahead (a second window is harmless).
    Unavailable,
}

fn single_instance() -> Lock {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("glassdock-settings.lock"))
    else {
        return Lock::Unavailable;
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Lock::Acquired(file),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Lock::Busy,
        Err(_) => Lock::Unavailable,
    }
}

pub fn run() -> Result<()> {
    // One settings window at a time (the dock's menu and a launcher can both start one): a second
    // run just brings the open window to the front.
    let _lock = single_instance();
    if matches!(_lock, Lock::Busy) {
        let _ = std::process::Command::new("hyprctl")
            .args(["dispatch", "focuswindow", "class:^glassdock-settings$"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        return Ok(());
    }
    // Same as the dock: keep a discrete NVIDIA GPU asleep, use Mesa for EGL unless overridden.
    crate::model::force_mesa_egl();
    let conn = Connection::connect_to_env().map_err(|e| anyhow!("no Wayland connection: {e}"))?;
    let (globals, event_queue) = registry_queue_init::<SettingsApp>(&conn).map_err(|e| anyhow!("registry: {e}"))?;
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<SettingsApp> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| anyhow!("wl_compositor: {e}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh).map_err(|e| anyhow!("xdg_wm_base: {e}"))?;
    let frac_mgr = globals.bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ()).ok();
    let viewporter = globals.bind::<WpViewporter, _, _>(&qh, 1..=1, ()).ok();
    let cursor_mgr = globals.bind::<WpCursorShapeManagerV1, _, _>(&qh, 1..=2, ()).ok();

    let wl_display = conn.backend().display_ptr() as *mut c_void;
    // SAFETY: the display pointer stays valid as long as `conn`, which outlives the EGL context
    // (both live until the end of this function).
    let egl = unsafe { gfx::EglCtx::new(wl_display)? };

    let surface = compositor.create_surface(&qh);
    let viewport = viewporter.as_ref().map(|v| v.get_viewport(&surface, &qh, ()));
    let frac = frac_mgr.as_ref().map(|m| m.get_fractional_scale(&surface, &qh, ()));
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestClient, &qh);
    window.set_title(tr(Msg::SettingsTitle));
    window.set_app_id(APP_ID);
    // min == max makes Hyprland float the window and keeps the layout fixed.
    window.set_min_size(Some((WIN_W as u32, WIN_H as u32)));
    window.set_max_size(Some((WIN_W as u32, WIN_H as u32)));
    window.commit();

    let settings = Settings::load();
    let lay = Layout::new(WIN_W, WIN_H);
    let mut ui = Ui::new(settings, lay);
    let mut forced_scale = None;
    let mut debug = false;
    let mut timed_debug = vec![];
    if let Ok(spec) = std::env::var("GLASSDOCK_SETTINGS_DEBUG") {
        let cmds = parse_debug(&spec);
        // An empty or all-invalid value must not switch the window to debug mode.
        if !cmds.is_empty() {
            let (now_cmds, later): (Vec<_>, Vec<_>) = cmds.into_iter().partition(|(ms, _)| *ms == 0);
            forced_scale = apply_debug(&mut ui, &now_cmds.into_iter().map(|(_, c)| c).collect::<Vec<_>>());
            timed_debug = later;
            debug = true;
        }
    }
    let an = Anims::new(&ui);

    let mut app = SettingsApp {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        qh: qh.clone(),
        loop_handle: loop_handle.clone(),
        window,
        viewport,
        _frac: frac,
        frac_scale: None,
        int_scale: 1,
        egl,
        egl_win: std::ptr::null_mut(),
        egl_surf: None,
        renderer: None,
        seat: None,
        pointer: None,
        keyboard: None,
        cursor_mgr,
        cursor_dev: None,
        enter_serial: 0,
        cursor_is_pointer: false,
        ctrl_down: false,
        active: true,
        size: (WIN_W as u32, WIN_H as u32),
        scale: 1.0,
        buf: (0, 0),
        configured: false,
        frame_pending: false,
        dirty: false,
        last_draw: Instant::now(),
        ui,
        an,
        gate: ReloadGate::default(),
        cleared_until: None,
        reload_threads: vec![],
        forced_scale,
        debug,
        draws: 0,
        quit: false,
    };

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .map_err(|e| anyhow!("calloop wayland: {e}"))?;
    for (ms, cmd) in timed_debug {
        let _ = app.loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(ms)),
            move |_, _, app: &mut SettingsApp| {
                apply_debug(&mut app.ui, std::slice::from_ref(&cmd));
                app.update_cursor();
                app.request_redraw();
                TimeoutAction::Drop
            },
        );
    }
    let signal = event_loop.get_signal();
    event_loop.run(None, &mut app, |app| {
        if app.quit {
            signal.stop();
        }
    })?;
    app.teardown();
    Ok(())
}

impl SettingsApp {
    fn make_current(&self) -> bool {
        match self.egl_surf {
            Some(s) => self.egl.make_current(s).is_ok(),
            None => false,
        }
    }

    /// (Re)creates the EGL window for the current size and scale.
    fn ensure_egl(&mut self) -> Result<()> {
        let (w, h) = self.size;
        let has_vp = self.viewport.is_some();
        self.scale = self
            .forced_scale
            .unwrap_or_else(|| effective_scale(self.frac_scale, has_vp, self.int_scale));
        let (bw, bh) = buffer_size(w, h, self.scale, has_vp);
        match &self.viewport {
            Some(vp) => vp.set_destination(w as i32, h as i32),
            None => self
                .window
                .wl_surface()
                .set_buffer_scale(self.scale.round().max(1.0) as i32),
        }
        if self.egl_win.is_null() {
            let proxy = self.window.wl_surface().id().as_ptr();
            // SAFETY: `proxy` is the live wl_surface of our window; the wl_egl_window is destroyed in
            // `teardown` before the surface goes away.
            let win = unsafe {
                wayland_sys::ffi_dispatch!(
                    wayland_sys::egl::wayland_egl_handle(),
                    wl_egl_window_create,
                    proxy,
                    bw,
                    bh
                )
            };
            if win.is_null() {
                return Err(anyhow!("wl_egl_window_create failed"));
            }
            // SAFETY: `win` is the valid wl_egl_window created just above.
            let surf = unsafe { self.egl.create_surface(win as *mut c_void)? };
            self.egl_win = win as *mut c_void;
            self.egl_surf = Some(surf);
            self.buf = (bw, bh);
            self.egl.make_current(surf)?;
            self.egl.set_swap_interval0();
            let egl = &self.egl;
            // SAFETY: the loader returns addresses of GL entry points of the current EGL context.
            let gl = unsafe { glow::Context::from_loader_function(|n| egl.proc_address(n)) };
            self.renderer = Some(Renderer::new(gl)?);
        } else if self.buf != (bw, bh) {
            // SAFETY: `egl_win` is the live wl_egl_window created above.
            unsafe {
                wayland_sys::ffi_dispatch!(
                    wayland_sys::egl::wayland_egl_handle(),
                    wl_egl_window_resize,
                    self.egl_win as *mut _,
                    bw,
                    bh,
                    0,
                    0
                );
            }
            self.buf = (bw, bh);
        }
        if self.make_current() {
            if let Some(r) = self.renderer.as_mut() {
                r.clear_text_cache();
            }
        }
        Ok(())
    }

    fn teardown(&mut self) {
        if let Some(s) = self.egl_surf.take() {
            self.egl.destroy_surface(s);
        }
        if !self.egl_win.is_null() {
            // SAFETY: the window was created by `ensure_egl` and its EGL surface is gone.
            unsafe {
                wayland_sys::ffi_dispatch!(
                    wayland_sys::egl::wayland_egl_handle(),
                    wl_egl_window_destroy,
                    self.egl_win as *mut _
                );
            }
            self.egl_win = std::ptr::null_mut();
        }
        // A reload still in flight gets a moment to finish, but a dock that does not answer must
        // not keep the process (and the user waiting) for the socket timeout: the rest is left to
        // the process exit.
        let deadline = Instant::now() + Duration::from_millis(300);
        for t in self.reload_threads.drain(..) {
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }

    fn request_redraw(&mut self) {
        if !self.configured {
            return;
        }
        if self.frame_pending {
            self.dirty = true;
        } else {
            self.draw();
        }
    }

    fn draw(&mut self) {
        if !self.configured || self.renderer.is_none() || !self.make_current() {
            return;
        }
        let now = Instant::now();
        let dt = now.duration_since(self.last_draw).as_secs_f32().clamp(0.0005, 0.05);
        self.last_draw = now;
        let busy = self.an.step(&self.ui, dt);
        let (bw, bh) = self.buf;
        if let Some(r) = self.renderer.as_mut() {
            r.frame(bw, bh, self.scale, &self.ui, &self.an, self.active);
        }
        self.draws += 1;
        if self.debug {
            eprintln!("draw #{} busy={busy}", self.draws);
        }
        let surface = self.window.wl_surface().clone();
        if busy || self.dirty {
            self.dirty = false;
            self.frame_pending = true;
            surface.frame(&self.qh, surface.clone());
        } else {
            self.frame_pending = false;
        }
        if let Some(s) = self.egl_surf {
            let _ = self.egl.swap(s);
        }
    }

    fn update_cursor(&mut self) {
        let want = self.ui.hover.is_some();
        if want == self.cursor_is_pointer {
            return;
        }
        self.cursor_is_pointer = want;
        self.set_cursor_shape();
    }

    fn set_cursor_shape(&self) {
        if let Some(dev) = &self.cursor_dev {
            let shape = if self.cursor_is_pointer {
                wp_cursor_shape_device_v1::Shape::Pointer
            } else {
                wp_cursor_shape_device_v1::Shape::Default
            };
            dev.set_shape(self.enter_serial, shape);
        }
    }

    /// Tells the running dock to reload, off the UI thread (best effort).
    fn spawn_reload(&mut self) {
        self.reload_threads.retain(|t| !t.is_finished());
        let t = std::thread::Builder::new().name("glassdock-reload".into()).spawn(|| {
            let _ = config::ctl("reload");
        });
        if let Ok(t) = t {
            self.reload_threads.push(t);
        }
    }

    fn apply(&mut self, fx: Effects, serial: u32) {
        let now = Instant::now();
        if fx.save {
            if let Err(e) = self.ui.s.save() {
                eprintln!("glassdock settings: could not save {}: {e}", Settings::path().display());
            }
        }
        if fx.reload_now {
            self.gate.force(now);
            self.spawn_reload();
        } else if fx.reload_soon {
            match self.gate.change(now) {
                Gate::SendNow => self.spawn_reload(),
                Gate::Schedule(d) => {
                    let _ = self
                        .loop_handle
                        .insert_source(Timer::from_duration(d), |_, _, app: &mut SettingsApp| {
                            if app.gate.timer_fired(Instant::now()) {
                                app.spawn_reload();
                            }
                            TimeoutAction::Drop
                        });
                }
                Gate::Nothing => {}
            }
        }
        if fx.clear_history {
            if let Err(e) = Usage::clear_file(&Usage::default_path()) {
                eprintln!("could not clear the usage history: {e}");
            }
            self.gate.force(now);
            self.spawn_reload();
            self.ui.cleared = true;
            self.cleared_until = Some(now + CLEARED_FOR);
            let _ = self.loop_handle.insert_source(
                Timer::from_duration(CLEARED_FOR + Duration::from_millis(20)),
                |_, _, app: &mut SettingsApp| {
                    if app.cleared_until.is_some_and(|t| Instant::now() >= t) {
                        app.cleared_until = None;
                        app.ui.cleared = false;
                        app.request_redraw();
                    }
                    TimeoutAction::Drop
                },
            );
        }
        if fx.close {
            self.quit = true;
        }
        if fx.move_window {
            if let Some(seat) = &self.seat {
                self.window.move_(seat, serial);
            }
        }
        self.update_cursor();
        if fx.redraw {
            self.request_redraw();
        }
    }
}

impl CompositorHandler for SettingsApp {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, new: i32) {
        self.int_scale = new.max(1);
        if self.configured {
            let _ = self.ensure_egl();
            self.request_redraw();
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _time: u32) {
        self.frame_pending = false;
        if self.an_busy() || self.dirty {
            self.draw();
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl SettingsApp {
    /// True while a smoothed value has not reached its target.
    fn an_busy(&self) -> bool {
        let t = targets(&self.ui);
        let a = &self.an;
        a.hover != t.hover || a.press != t.press || a.on != t.on || a.en != t.en || a.cleared != t.cleared
    }
}

impl OutputHandler for SettingsApp {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl WindowHandler for SettingsApp {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.quit = true;
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window, configure: WindowConfigure, _: u32) {
        let size = pick_size((
            configure.new_size.0.map(|v| v.get()),
            configure.new_size.1.map(|v| v.get()),
        ));
        let was_configured = self.configured;
        let active = configure.is_activated();
        let changed = !was_configured || size != self.size || active != self.active;
        self.size = size;
        self.active = active;
        self.configured = true;
        if size.0 as f32 != self.ui.lay.w || size.1 as f32 != self.ui.lay.h {
            self.ui.lay = Layout::new(size.0 as f32, size.1 as f32);
        }
        if let Err(e) = self.ensure_egl() {
            eprintln!("glassdock settings: {e}");
            self.quit = true;
            return;
        }
        if changed {
            self.dirty = false;
            self.frame_pending = false;
            if !was_configured {
                // First frame: no animation from a default state.
                self.an.snap(&self.ui);
            }
            self.draw();
        }
    }
}

impl SeatHandler for SettingsApp {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.is_none() {
            self.seat = Some(seat);
        }
    }
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        match cap {
            Capability::Pointer if self.pointer.is_none() => {
                if let Ok(p) = self.seat_state.get_pointer(qh, &seat) {
                    if let Some(m) = &self.cursor_mgr {
                        self.cursor_dev = Some(m.get_pointer(&p, qh, ()));
                    }
                    self.pointer = Some(p);
                }
            }
            // Raw wl_keyboard: there is no xkbcommon here, Esc and Ctrl+W are plain evdev codes.
            Capability::Keyboard if self.keyboard.is_none() => {
                self.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            _ => {}
        }
        if self.seat.is_none() {
            self.seat = Some(seat);
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        match cap {
            Capability::Pointer => {
                if let Some(p) = self.pointer.take() {
                    p.release();
                }
                self.cursor_dev = None;
                self.cursor_is_pointer = false;
                // The pointer is gone: end a slider drag or a press the way a Leave does.
                let fx = self.ui.leave();
                self.apply(fx, 0);
            }
            Capability::Keyboard => {
                if let Some(k) = self.keyboard.take() {
                    k.release();
                }
                self.ctrl_down = false;
            }
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for SettingsApp {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            if self.debug || &ev.surface != self.window.wl_surface() {
                continue;
            }
            let (x, y) = (ev.position.0 as f32, ev.position.1 as f32);
            match ev.kind {
                PointerEventKind::Enter { serial } => {
                    self.enter_serial = serial;
                    // The cursor is ours again: say which shape it should have.
                    self.cursor_is_pointer = false;
                    let fx = self.ui.motion(x, y);
                    self.cursor_is_pointer = self.ui.hover.is_some();
                    self.set_cursor_shape();
                    self.apply(fx, serial);
                }
                PointerEventKind::Leave { .. } => {
                    let fx = self.ui.leave();
                    self.apply(fx, 0);
                }
                PointerEventKind::Motion { .. } => {
                    let fx = self.ui.motion(x, y);
                    self.apply(fx, 0);
                }
                PointerEventKind::Press { button, serial, .. } if button == BTN_LEFT => {
                    let fx = self.ui.press(x, y);
                    self.apply(fx, serial);
                }
                PointerEventKind::Release { button, serial, .. } if button == BTN_LEFT => {
                    let fx = self.ui.release(x, y);
                    self.apply(fx, serial);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for SettingsApp {
    fn event(
        app: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // Without a keymap the modifier bits cannot be named, but the xkb protocol fixes the first
            // eight (Shift, Lock, Control, Mod1..Mod5), so Control is always bit 2.
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                app.ctrl_down = mods_depressed & XKB_MOD_CONTROL != 0
            }
            wl_keyboard::Event::Leave { .. } => app.ctrl_down = false,
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => match key {
                KEY_ESC => app.quit = true,
                KEY_W if app.ctrl_down => app.quit = true,
                _ => {}
            },
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for SettingsApp {
    fn event(
        app: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            app.frac_scale = Some(scale as f32 / 120.0);
            if app.configured {
                let _ = app.ensure_egl();
                app.request_redraw();
            }
        }
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for SettingsApp {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeDeviceV1,
        _: wp_cursor_shape_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(SettingsApp: ignore WpFractionalScaleManagerV1);
delegate_noop!(SettingsApp: ignore WpViewporter);
delegate_noop!(SettingsApp: ignore WpViewport);
delegate_noop!(SettingsApp: ignore WpCursorShapeManagerV1);

delegate_compositor!(SettingsApp);
delegate_output!(SettingsApp);
delegate_seat!(SettingsApp);
delegate_pointer!(SettingsApp);
delegate_xdg_shell!(SettingsApp);
delegate_xdg_window!(SettingsApp);
delegate_registry!(SettingsApp);

impl ProvidesRegistryState for SettingsApp {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lay() -> Layout {
        Layout::new(WIN_W, WIN_H)
    }

    fn ui() -> Ui {
        Ui::new(Settings::default(), lay())
    }

    #[test]
    fn layout_is_inside_the_window_and_ordered() {
        let l = lay();
        let mut prev = TITLE_H;
        for (i, &(first, n)) in SECTIONS.iter().enumerate() {
            assert!(l.heads[i].y >= prev);
            assert_eq!(l.cards[i].y, l.heads[i].bottom());
            for r in first..first + n {
                let row = l.rows[r];
                assert!(row.x >= l.cards[i].x && row.right() <= l.cards[i].right() + 0.01);
                assert!(row.y >= l.cards[i].y && row.bottom() <= l.cards[i].bottom() + 0.01);
            }
            prev = l.cards[i].bottom();
        }
        assert!(prev < l.footer_y - 14.0, "the footer must clear the last card");
        assert!(l.cards[1].right() <= WIN_W && l.cards[1].x >= 0.0);
    }

    #[test]
    fn layout_survives_odd_sizes() {
        for (w, h) in [(0.0, 0.0), (10.0, 10.0), (1920.0, 1080.0), (560.0, 100.0)] {
            let l = Layout::new(w, h);
            assert!(l.cards[0].w >= 0.0 && l.cards[0].w <= CARD_MAX_W);
            assert!(l.cards[0].x >= 0.0);
        }
        // Wide window: the cards stay centred.
        let l = Layout::new(1000.0, 700.0);
        assert!((l.cards[0].cx() - 500.0).abs() < 0.01);
    }

    #[test]
    fn controls_fit_in_their_rows() {
        let l = lay();
        let t = toggle_track(&l.rows[ROW_MAG]);
        assert!(t.x > l.rows[ROW_MAG].x && t.right() < l.rows[ROW_MAG].right());
        assert!((t.cy() - l.rows[ROW_MAG].cy()).abs() < 0.01);
        let (m, v, p) = stepper(&l.rows[ROW_COUNT]);
        assert!(m.right() <= v.x + 0.01 && v.right() <= p.x + 0.01);
        assert!(p.right() < l.rows[ROW_COUNT].right());
        let b = clear_button(&l.rows[ROW_CLEAR]);
        assert!((b.cx() - l.rows[ROW_CLEAR].cx()).abs() < 0.01);
    }

    #[test]
    fn slider_maps_position_to_value_and_back() {
        let track = slider_track(&lay().rows[ROW_ZOOM]);
        assert_eq!(slider_frac(track.x - 500.0, &track), 0.0);
        assert_eq!(slider_frac(track.right() + 500.0, &track), 1.0);
        assert_eq!(slider_frac(f32::NAN, &track), 0.0);
        let mid = slider_frac(track.cx(), &track);
        assert!((mid - 0.5).abs() < 1e-4);
        assert_eq!(zoom_from_frac(0.0), ZOOM_MIN);
        assert_eq!(zoom_from_frac(1.0), ZOOM_MAX);
        assert_eq!(zoom_from_frac(7.0), ZOOM_MAX);
        for z in [ZOOM_MIN, 1.5, 1.78, ZOOM_MAX] {
            assert!((zoom_from_zoom_roundtrip(z) - z).abs() < 1e-4);
        }
        // The knob centre is where the value says.
        let k = slider_knob(&track, 0.0, 0.0);
        assert!((k.cx() - (track.x + SLIDER_KNOB_W * 0.5)).abs() < 1e-3);
        let k = slider_knob(&track, 1.0, 0.0);
        assert!((k.cx() - (track.right() - SLIDER_KNOB_W * 0.5)).abs() < 1e-3);
    }

    fn zoom_from_zoom_roundtrip(z: f32) -> f32 {
        zoom_from_frac(frac_from_zoom(z))
    }

    #[test]
    fn zoom_snaps_to_the_grid_and_formats() {
        assert_eq!(snap_zoom(1.78), 1.8);
        assert_eq!(snap_zoom(0.0), ZOOM_MIN);
        assert_eq!(snap_zoom(9.0), ZOOM_MAX);
        assert_eq!(fmt_zoom(1.8), "1.8x");
        assert_eq!(fmt_zoom(1.25), "1.25x");
        assert_eq!(fmt_zoom(2.0), "2.0x");
        assert_eq!(fmt_zoom(1.78), "1.78x");
    }

    #[test]
    fn stepper_is_clamped() {
        assert_eq!(step_count(RECOMMENDED_MIN, -1), RECOMMENDED_MIN);
        assert_eq!(step_count(RECOMMENDED_MAX, 1), RECOMMENDED_MAX);
        assert_eq!(step_count(3, 1), 4);
        assert_eq!(step_count(3, -1), 2);
        let mut s = Settings {
            recommended_count: RECOMMENDED_MAX,
            ..Settings::default()
        };
        assert!(!activate(&mut s, Ctl::Plus));
        assert!(activate(&mut s, Ctl::Minus));
        assert_eq!(s.recommended_count, RECOMMENDED_MAX - 1);
    }

    #[test]
    fn toggles_flip() {
        let mut s = Settings::default();
        assert!(activate(&mut s, Ctl::Magnification));
        assert!(!s.magnification);
        assert!(activate(&mut s, Ctl::Recommended));
        assert!(!s.recommended);
        assert!(!activate(&mut s, Ctl::Clear));
        assert!(!activate(&mut s, Ctl::Close));
    }

    #[test]
    fn hit_testing_and_disabled_controls() {
        let l = lay();
        let mut s = Settings::default();
        let en = enabled(&s);
        let zr = &l.rows[ROW_ZOOM];
        let track = slider_track(zr);
        assert_eq!(hit_test(&l, &en, track.cx(), track.cy()), Some(Ctl::Zoom));
        assert_eq!(
            hit_test(&l, &en, l.rows[ROW_MAG].x + 30.0, l.rows[ROW_MAG].cy()),
            Some(Ctl::Magnification)
        );
        assert_eq!(hit_test(&l, &en, l.close.cx(), l.close.cy()), Some(Ctl::Close));
        assert_eq!(hit_test(&l, &en, 280.0, 20.0), None, "the title area is not a control");
        let (m, _, p) = stepper(&l.rows[ROW_COUNT]);
        assert_eq!(hit_test(&l, &en, m.cx(), m.cy()), Some(Ctl::Minus));
        assert_eq!(hit_test(&l, &en, p.cx(), p.cy()), Some(Ctl::Plus));
        let b = clear_button(&l.rows[ROW_CLEAR]);
        assert_eq!(hit_test(&l, &en, b.cx(), b.cy()), Some(Ctl::Clear));
        // Magnification off: the slider is dead. Recommended off: the stepper is dead.
        s.magnification = false;
        s.recommended = false;
        let en = enabled(&s);
        assert_eq!(hit_test(&l, &en, track.cx(), track.cy()), None);
        assert_eq!(hit_test(&l, &en, m.cx(), m.cy()), None);
        assert_eq!(hit_test(&l, &en, p.cx(), p.cy()), None);
        // At the limits only the useful button works.
        let s = Settings {
            recommended_count: RECOMMENDED_MAX,
            ..Settings::default()
        };
        let en = enabled(&s);
        assert!(en.minus && !en.plus);
        assert_eq!(hit_test(&l, &en, -5.0, -5.0), None);
        assert_eq!(hit_test(&l, &en, f32::NAN, 3.0), None);
    }

    #[test]
    fn click_acts_on_release_only_over_the_control() {
        let mut u = ui();
        let row = u.lay.rows[ROW_MAG];
        let (x, y) = (row.x + 40.0, row.cy());
        let fx = u.press(x, y);
        assert!(!fx.save && u.s.magnification, "nothing happens on press");
        let fx = u.release(x, y);
        assert!(fx.save && fx.reload_now && !u.s.magnification);
        // Press, move away, release: cancelled.
        let fx = u.press(x, y);
        assert!(fx.redraw);
        u.motion(5.0, 200.0);
        let fx = u.release(5.0, 200.0);
        assert!(!fx.save && !u.s.magnification);
        // Close and clear report their effect.
        let c = u.lay.close;
        u.press(c.cx(), c.cy());
        assert!(u.release(c.cx(), c.cy()).close);
        let b = clear_button(&u.lay.rows[ROW_CLEAR]);
        u.press(b.cx(), b.cy());
        assert!(u.release(b.cx(), b.cy()).clear_history);
    }

    #[test]
    fn slider_acts_on_press_and_follows_outside_the_window() {
        let mut u = ui();
        let track = u.zoom_track();
        let fx = u.press(track.x + 2.0, track.cy());
        assert!(fx.save && fx.reload_soon);
        assert_eq!(u.s.zoom, ZOOM_MIN);
        let fx = u.motion(track.right() + 400.0, track.cy() + 900.0);
        assert!(fx.save && fx.reload_soon);
        assert_eq!(u.s.zoom, ZOOM_MAX);
        let fx = u.release(track.right() + 400.0, track.cy());
        assert!(fx.reload_now && u.pressed.is_none());
        // Leaving while dragging also finishes the drag.
        u.press(track.x, track.cy());
        assert!(u.leave().reload_now);
    }

    #[test]
    fn title_press_moves_the_window_but_not_the_cards() {
        let mut u = ui();
        assert!(u.press(300.0, 20.0).move_window);
        u.release(300.0, 20.0);
        let mut u = ui();
        let card = u.lay.cards[0];
        assert!(!u.press(card.x + 4.0, card.y + 1.0).move_window);
        let mut u = ui();
        assert!(!u.press(10.0, WIN_H - 5.0).move_window);
    }

    #[test]
    fn approach_settles() {
        let mut v = 0.0;
        let mut t = 0.0;
        while v != 1.0 {
            v = approach(v, 1.0, 1.0 / 60.0, 0.045);
            t += 1.0 / 60.0;
            assert!(t < 1.0, "must settle");
        }
        assert!(t > 0.1 && t < 0.4, "the slide takes about 150-250 ms, got {t}");
        assert_eq!(approach(0.5, 0.5, 0.016, 0.1), 0.5);
        assert!(approach(0.0, 1.0, 0.0, 0.1) == 0.0);
    }

    #[test]
    fn animations_report_busy_only_while_moving() {
        let mut u = ui();
        let mut an = Anims::new(&u);
        assert!(!an.step(&u, 0.016), "idle means no redraws");
        u.hover = Some(Ctl::Clear);
        assert!(an.step(&u, 0.016));
        let mut n = 0;
        while an.step(&u, 0.016) {
            n += 1;
            assert!(n < 200);
        }
        assert_eq!(an.hover[Ctl::Clear.idx()], 1.0);
    }

    #[test]
    fn knob_stretches_towards_the_free_side() {
        let t = toggle_track(&lay().rows[ROW_MAG]);
        let off = toggle_knob(&t, 0.0, 1.0);
        let on = toggle_knob(&t, 1.0, 1.0);
        assert!((off.x - (t.x + 3.0)).abs() < 1e-3, "off: the left edge stays");
        assert!(
            (on.right() - (t.right() - 3.0)).abs() < 1e-3,
            "on: the right edge stays"
        );
        assert!(off.w > TOGGLE_KNOB_W && on.w > TOGGLE_KNOB_W);
        assert!(off.y >= t.y && off.bottom() <= t.bottom());
    }

    #[test]
    fn reload_gate_limits_the_rate_and_keeps_the_last_value() {
        let t0 = Instant::now();
        let mut g = ReloadGate::default();
        assert_eq!(g.change(t0), Gate::SendNow);
        let t1 = t0 + Duration::from_millis(10);
        assert_eq!(g.change(t1), Gate::Schedule(Duration::from_millis(70)));
        assert_eq!(g.change(t1 + Duration::from_millis(5)), Gate::Nothing);
        assert!(g.timer_fired(t0 + RELOAD_INTERVAL));
        assert!(!g.timer_fired(t0 + RELOAD_INTERVAL), "only once");
        // After the interval a change is sent immediately again.
        assert_eq!(g.change(t0 + RELOAD_INTERVAL * 3), Gate::SendNow);
        // A forced reload (release) clears the pending one.
        let t = t0 + RELOAD_INTERVAL * 3 + Duration::from_millis(1);
        assert!(matches!(g.change(t), Gate::Schedule(_)));
        g.force(t);
        assert!(!g.timer_fired(t));
    }

    #[test]
    fn scale_and_buffer_size() {
        assert_eq!(buffer_size(560, 480, 1.6, true), (896, 768));
        assert_eq!(
            buffer_size(560, 480, 1.6, false),
            (1120, 960),
            "no viewport: whole scales only"
        );
        assert_eq!(buffer_size(0, 0, 2.0, true), (2, 2), "never a zero-sized buffer");
        assert_eq!(effective_scale(Some(1.6), true, 2), 1.6);
        assert_eq!(effective_scale(Some(1.6), false, 2), 2.0);
        assert_eq!(effective_scale(None, true, 2), 2.0);
        assert_eq!(effective_scale(Some(f32::NAN), true, 1), 1.0);
        assert_eq!(effective_scale(Some(0.0), true, 1), 0.5);
    }

    #[test]
    fn configure_size_zero_means_preferred() {
        assert_eq!(pick_size((None, None)), (WIN_W as u32, WIN_H as u32));
        assert_eq!(pick_size((Some(600), None)), (600, WIN_H as u32));
        assert_eq!(pick_size((Some(1), Some(1))), (120, 120));
    }

    #[test]
    fn ellipsis_fits_and_is_stable() {
        let m = |s: &str| s.chars().count() as f32 * 10.0;
        assert_eq!(fit_ellipsis("hello", 100.0, m), "hello");
        let s = fit_ellipsis("hello world again", 100.0, m);
        assert!(s.ends_with('…') && m(&s) <= 100.0, "{s}");
        assert_eq!(fit_ellipsis("hello world", 0.0, m), "…");
        assert_eq!(fit_ellipsis("", 0.0, m), "");
    }

    #[test]
    fn debug_spec_parsing() {
        let c = parse_debug(
            "toggle:mag, slider:0.6,hover:clear,press:5,cleared,count:4,bogus:1,slider:x,scale:1.5,@250:toggle:rec",
        );
        assert_eq!(
            c,
            vec![
                (0, DebugCmd::Toggle(Ctl::Magnification)),
                (0, DebugCmd::Slider(0.6)),
                (0, DebugCmd::Hover(Ctl::Clear)),
                (0, DebugCmd::Press(Ctl::Plus)),
                (0, DebugCmd::Cleared),
                (0, DebugCmd::Count(4)),
                (0, DebugCmd::Scale(1.5)),
                (250, DebugCmd::Toggle(Ctl::Recommended)),
            ]
        );
        assert!(parse_debug("").is_empty());
        let mut u = ui();
        let c: Vec<DebugCmd> = c.into_iter().map(|(_, c)| c).collect();
        let sc = apply_debug(&mut u, &c);
        assert_eq!(sc, Some(1.5));
        assert!(!u.s.magnification && u.cleared && u.s.recommended_count == 4);
        assert_eq!(u.pressed, Some(Ctl::Plus));
    }

    #[test]
    fn separators_sit_between_rows_of_one_card() {
        let l = lay();
        let seps = l.separators();
        assert_eq!(seps.len(), 5);
        for s in seps {
            assert!(l.cards.iter().any(|c| s.y > c.y && s.bottom() < c.bottom()));
        }
    }
}
