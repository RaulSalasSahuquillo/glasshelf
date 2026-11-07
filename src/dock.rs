//! Layout, parabolic magnification and animations of the dock (pure logic, no Wayland).
//! Units are LOGICAL pixels unless stated otherwise.
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const ICON: f32 = 44.0;
pub const ADD_BASE: f32 = 34.0;
pub const SEP_W: f32 = 14.0;
pub const GAP: f32 = 6.0;
pub const PAD_X: f32 = 16.0;
pub const PAD_TOP: f32 = 10.0;
pub const PAD_BOTTOM: f32 = 14.0;
pub const PANEL_H: f32 = PAD_TOP + ICON + PAD_BOTTOM; // 68
pub const ZOOM: f32 = 1.78;
pub const ZOOM_RANGE: f32 = 2.7 * (ICON + GAP);
pub const BOTTOM_GAP: f32 = 10.0;
pub const SIDE_MARGIN: f32 = 56.0;
/// Room above the icons for the name pill and the context menu (the surface has a fixed, transparent size).
pub const LABEL_SPACE: f32 = 340.0;
pub const HOT_H: f32 = 4.0;
pub const HOT_PAD: f32 = 80.0;

pub const HIDE_DELAY: Duration = Duration::from_millis(450);
pub const K_SCALE: f32 = 24.0;
pub const K_REVEAL: f32 = 13.0;
pub const K_LABEL: f32 = 16.0;
pub const BOUNCE_TIME: f32 = 0.55;
pub const BOUNCE_HEIGHT: f32 = 18.0;
pub const CAPTURE_WAIT: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    App,
    Sep,
    Add,
}

#[derive(Clone, Debug)]
pub struct Slot {
    pub kind: Kind,
    /// Index into the entry list (only meaningful for `Kind::App`).
    pub entry: usize,
    pub key: String,
    pub base: f32,
    pub s: f32,
    pub x: f32,
    pub w: f32,
}

impl Slot {
    pub fn interactive(&self) -> bool {
        self.kind != Kind::Sep
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PanelGeom {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

pub struct Layout {
    pub slots: Vec<Slot>,
    /// Logical size of the surface.
    pub w: f32,
    pub h: f32,
    pub reveal: f32,
    pub label_alpha: f32,
    pub hover: f32,
    pub label_slot: Option<usize>,
    pub pointer: Option<(f32, f32)>,
    pub hold_until: Option<Instant>,
    pub menu_open: bool,
    pub forced_visible: bool,
    pub manual_until: Option<Instant>,
    pub bounce: HashMap<String, Instant>,
    /// Deadline while waiting for the backdrop capture to finish.
    pub capture_wait_until: Option<Instant>,
}

impl Layout {
    pub fn new() -> Layout {
        let mut l = Layout {
            slots: vec![],
            w: 800.0,
            h: 200.0,
            reveal: 0.0,
            label_alpha: 0.0,
            hover: 0.0,
            label_slot: None,
            pointer: None,
            hold_until: None,
            menu_open: false,
            forced_visible: false,
            manual_until: None,
            bounce: HashMap::new(),
            capture_wait_until: None,
        };
        l.resize_for_slots();
        l
    }

    pub fn rest_total(&self) -> f32 {
        let n = self.slots.len();
        if n == 0 {
            return 2.0 * PAD_X + ICON;
        }
        2.0 * PAD_X + self.slots.iter().map(|s| s.base).sum::<f32>() + GAP * (n as f32 - 1.0)
    }

    /// Computes the surface size (even width and height). Returns `true` if it changed.
    pub fn resize_for_slots(&mut self) -> bool {
        let extra = (ZOOM - 1.0) * ICON * (ZOOM_RANGE / (ICON + GAP)) * 1.15;
        let mut w = (self.rest_total() + extra).ceil() as i32 + 2 * SIDE_MARGIN as i32;
        w += w % 2;
        let mut h = (ZOOM * ICON + PAD_TOP + PAD_BOTTOM + BOTTOM_GAP + LABEL_SPACE + 24.0).ceil() as i32;
        h += h % 2;
        let changed = (w as f32 - self.w).abs() > 0.5 || (h as f32 - self.h).abs() > 0.5;
        self.w = w as f32;
        self.h = h as f32;
        changed
    }

    pub fn rest_centers(&self) -> Vec<f32> {
        let total = self.rest_total();
        let mut x = self.w / 2.0 - total / 2.0 + PAD_X;
        let mut out = Vec::with_capacity(self.slots.len());
        for s in &self.slots {
            out.push(x + s.base / 2.0);
            x += s.base + GAP;
        }
        out
    }

    /// Current positions from each element's animated scale. Returns `(left, total_width)`.
    pub fn layout(&mut self) -> (f32, f32) {
        let n = self.slots.len();
        let mut total = 2.0 * PAD_X + GAP * (n.max(1) as f32 - 1.0);
        for s in &mut self.slots {
            s.w = if s.kind == Kind::Sep { s.base } else { s.base * s.s };
            total += s.w;
        }
        let left = self.w / 2.0 - total / 2.0;
        let mut x = left + PAD_X;
        for s in &mut self.slots {
            s.x = x;
            x += s.w + GAP;
        }
        (left, total)
    }

    fn pointer_to_rest(&self, px: f32, rest: &[f32]) -> f32 {
        if self.slots.is_empty() {
            return px;
        }
        let cs: Vec<f32> = self.slots.iter().map(|s| s.x + s.w / 2.0).collect();
        if px <= cs[0] {
            return rest[0] + (px - cs[0]);
        }
        let last = cs.len() - 1;
        if px >= cs[last] {
            return rest[last] + (px - cs[last]);
        }
        for i in 0..last {
            if cs[i] <= px && px <= cs[i + 1] {
                let t = (px - cs[i]) / (cs[i + 1] - cs[i]).max(1e-6);
                return rest[i] + t * (rest[i + 1] - rest[i]);
            }
        }
        px
    }

    pub fn slot_at(&self, x: f32) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.interactive() && s.x - GAP / 2.0 <= x && x <= s.x + s.w + GAP / 2.0)
    }

    pub fn want_visible(&self, now: Instant) -> bool {
        self.pointer.is_some()
            || self.menu_open
            || self.forced_visible
            || self.manual_until.is_some_and(|t| now < t)
            || self.hold_until.is_some_and(|t| now < t)
    }

    /// Current geometry of the panel inside the surface.
    pub fn panel_geom(&mut self) -> PanelGeom {
        let (left, total) = self.layout();
        let slide = (1.0 - self.reveal).max(0.0).powf(1.6) * (PANEL_H + BOTTOM_GAP + 30.0);
        let bottom = self.h - BOTTOM_GAP + slide;
        PanelGeom {
            left,
            top: bottom - PANEL_H,
            width: total,
            height: PANEL_H,
        }
    }

    /// Resting position of the panel's top edge (not slid down).
    pub fn rest_top(&self) -> f32 {
        self.h - BOTTOM_GAP - PANEL_H
    }

    /// Advances the animation by `dt` seconds. `capturing`: a backdrop capture is in flight.
    /// `StepResult::busy` stays true while anything is still animating.
    pub fn step(&mut self, dt: f32, now: Instant, need_capture: bool, capturing: bool) -> StepResult {
        let mut busy = false;
        let mut start_capture = false;
        let want = self.want_visible(now);
        let mut target = if want { 1.0 } else { 0.0 };
        if want && self.reveal < 0.02 {
            if need_capture && !capturing && self.capture_wait_until.is_none() {
                start_capture = true;
                self.capture_wait_until = Some(now + CAPTURE_WAIT);
            }
            if let Some(t) = self.capture_wait_until {
                if (capturing || start_capture) && now < t {
                    target = 0.0;
                    busy = true;
                }
            }
        }
        if !want {
            self.capture_wait_until = None;
        }
        let k = 1.0 - (-dt * K_REVEAL).exp();
        if (target - self.reveal).abs() > 0.002 {
            self.reveal += (target - self.reveal) * k;
            busy = true;
        } else {
            self.reveal = target;
        }
        if self.pointer.is_none() && self.hold_until.is_some_and(|t| now < t) {
            busy = true; // waiting for the hide delay to expire
        }

        // magnification
        let rest = self.rest_centers();
        let zoom_on = self.pointer.is_some() && self.reveal > 0.55;
        let u = match (zoom_on, self.pointer) {
            (true, Some((px, _))) => Some(self.pointer_to_rest(px, &rest)),
            _ => None,
        };
        let ks = 1.0 - (-dt * K_SCALE).exp();
        for (i, s) in self.slots.iter_mut().enumerate() {
            if !s.interactive() {
                continue;
            }
            let mut ts = 1.0;
            if let Some(u) = u {
                let d = (u - rest[i]).abs() / ZOOM_RANGE;
                if d < 1.0 {
                    ts = 1.0 + (ZOOM - 1.0) * 0.5 * (1.0 + (std::f32::consts::PI * d).cos());
                }
            }
            if (ts - s.s).abs() > 0.0015 {
                s.s += (ts - s.s) * ks;
                busy = true;
            } else {
                s.s = ts;
            }
        }

        // name pill of the icon under the pointer
        let hov = match (zoom_on, self.pointer) {
            (true, Some((px, _))) => self.slot_at(px),
            _ => None,
        };
        let mut tl = 0.0;
        if let Some(i) = hov {
            if self.slots[i].kind == Kind::App {
                self.label_slot = Some(i);
                tl = 1.0;
            }
        }
        let kl = 1.0 - (-dt * K_LABEL).exp();
        if (tl - self.label_alpha).abs() > 0.004 {
            self.label_alpha += (tl - self.label_alpha) * kl;
            busy = true;
        } else {
            self.label_alpha = tl;
        }

        // strength of the pointer light on the glass
        let th = if zoom_on { 1.0 } else { 0.0 };
        if (th - self.hover).abs() > 0.004 {
            self.hover += (th - self.hover) * kl;
            busy = true;
        } else {
            self.hover = th;
        }

        // launch bounce
        let t_max = BOUNCE_TIME * 3.2;
        self.bounce
            .retain(|_, t0| now.duration_since(*t0).as_secs_f32() <= t_max);
        if !self.bounce.is_empty() {
            busy = true;
        }

        self.layout();
        StepResult { busy, start_capture }
    }

    /// Input region `(x, y, w, h)` in logical surface px.
    pub fn input_rect(&mut self) -> (i32, i32, i32, i32) {
        let h = self.h;
        let (left, total) = self.layout();
        if self.reveal < 0.02 {
            let hot_w = (self.rest_total() + HOT_PAD) as i32;
            return (
                (self.w / 2.0 - hot_w as f32 / 2.0) as i32,
                (h - HOT_H) as i32,
                hot_w,
                HOT_H as i32,
            );
        }
        let panel_bottom = h - BOTTOM_GAP;
        let top = panel_bottom - PANEL_H;
        let lift = self
            .slots
            .iter()
            .filter(|s| s.interactive())
            .map(|s| s.w - if s.kind == Kind::App { ICON } else { ADD_BASE })
            .fold(0.0f32, f32::max);
        let y0 = (top - lift - 6.0).max(0.0) as i32;
        ((left - 6.0) as i32, y0, (total + 12.0) as i32, h as i32 - y0)
    }

    pub fn bounce_offset(&self, key: &str, now: Instant) -> f32 {
        match self.bounce.get(key) {
            None => 0.0,
            Some(t0) => {
                let tt = now.duration_since(*t0).as_secs_f32() / BOUNCE_TIME;
                (std::f32::consts::PI * tt).sin().abs() * BOUNCE_HEIGHT * (1.0 - tt / 3.2).max(0.0)
            }
        }
    }
}

pub struct StepResult {
    pub busy: bool,
    pub start_capture: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(kind: Kind, key: &str, base: f32) -> Slot {
        Slot {
            kind,
            entry: 0,
            key: key.into(),
            base,
            s: 1.0,
            x: 0.0,
            w: base,
        }
    }

    /// Eight apps, a separator and the "+" button: the shape of a typical dock.
    fn typical_layout() -> Layout {
        let mut l = Layout::new();
        l.slots = (0..8).map(|i| slot(Kind::App, &format!("app{i}"), ICON)).collect();
        l.slots.push(slot(Kind::Sep, "sep", SEP_W));
        l.slots.push(slot(Kind::Add, "add", ADD_BASE));
        l.resize_for_slots();
        l
    }

    fn settle(l: &mut Layout, now: Instant, steps: usize) {
        for _ in 0..steps {
            l.step(1.0 / 60.0, now, false, false);
        }
    }

    #[test]
    fn surface_has_even_size_and_room_for_the_zoomed_dock() {
        let l = typical_layout();
        assert_eq!(l.w as i32 % 2, 0);
        assert_eq!(l.h as i32 % 2, 0);
        // Wide enough for the resting dock plus the growth caused by magnification.
        assert!(l.w > l.rest_total() + 2.0 * SIDE_MARGIN);
        // Tall enough for a fully zoomed icon, the panel padding and the label space.
        assert!(l.h >= ZOOM * ICON + PAD_TOP + PAD_BOTTOM + BOTTOM_GAP + LABEL_SPACE);
    }

    #[test]
    fn resting_dock_is_centred_and_ordered() {
        let mut l = typical_layout();
        let (left, total) = l.layout();
        assert!(
            (left + total / 2.0 - l.w / 2.0).abs() < 0.01,
            "dock must be centred in its surface"
        );
        assert!((total - l.rest_total()).abs() < 0.01);
        for pair in l.slots.windows(2) {
            assert!(pair[1].x > pair[0].x, "slots must be laid out left to right");
        }
    }

    #[test]
    fn magnification_peaks_under_the_pointer_and_falls_off() {
        let mut l = typical_layout();
        let now = Instant::now();
        let rest = l.rest_centers();
        let target = 3;
        l.pointer = Some((rest[target], l.h - BOTTOM_GAP - PAD_BOTTOM - 8.0));
        settle(&mut l, now, 240);

        let scales: Vec<f32> = l.slots.iter().filter(|s| s.kind == Kind::App).map(|s| s.s).collect();
        let (peak_idx, peak) = scales
            .iter()
            .copied()
            .enumerate()
            .fold((0, 0.0), |a, (i, v)| if v > a.1 { (i, v) } else { a });
        assert!(
            (peak_idx as i32 - target as i32).abs() <= 1,
            "peak at {peak_idx}, expected near {target}"
        );
        assert!(
            peak > ZOOM - 0.05 && peak <= ZOOM + 0.001,
            "peak scale {peak} should reach ZOOM ({ZOOM})"
        );
        // Far-away icons are untouched.
        assert!(
            (scales[7] - 1.0).abs() < 0.01,
            "icon far from the pointer must stay at 1.0, got {}",
            scales[7]
        );
        // The dock widened.
        let (_, total) = l.layout();
        assert!(total > l.rest_total() + 20.0);
    }

    #[test]
    fn magnification_relaxes_when_the_pointer_leaves() {
        let mut l = typical_layout();
        let now = Instant::now();
        let rest = l.rest_centers();
        l.pointer = Some((rest[2], l.h - BOTTOM_GAP - PAD_BOTTOM - 8.0));
        settle(&mut l, now, 200);
        assert!(l.slots[2].s > 1.5);
        l.pointer = None;
        settle(&mut l, now, 400);
        assert!(
            l.slots.iter().all(|s| (s.s - 1.0).abs() < 0.01),
            "every icon must return to its resting size"
        );
    }

    #[test]
    fn hidden_dock_only_exposes_a_thin_hot_zone() {
        let mut l = typical_layout();
        assert_eq!(l.reveal, 0.0);
        let (x, y, w, h) = l.input_rect();
        assert_eq!(h, HOT_H as i32);
        assert_eq!(y, (l.h - HOT_H) as i32);
        assert!(
            w as f32 >= l.rest_total(),
            "the hot zone must be at least as wide as the resting dock"
        );
        assert!(x >= 0 && (x + w) as f32 <= l.w);
    }

    #[test]
    fn reveal_follows_the_pointer_and_respects_the_hide_delay() {
        let mut l = typical_layout();
        let t0 = Instant::now();
        // Pointer arrives: the dock slides in.
        l.pointer = Some((l.w / 2.0, l.h - 2.0));
        settle(&mut l, t0, 240);
        assert!(l.reveal > 0.99);

        // Pointer leaves: it stays up during the hold delay...
        l.pointer = None;
        l.hold_until = Some(t0 + HIDE_DELAY);
        l.step(1.0 / 60.0, t0 + HIDE_DELAY / 2, false, false);
        assert!(l.want_visible(t0 + HIDE_DELAY / 2));
        assert!(l.reveal > 0.99);

        // ...and slides away once the delay has passed.
        let later = t0 + HIDE_DELAY + Duration::from_millis(50);
        assert!(!l.want_visible(later));
        settle(&mut l, later, 240);
        assert!(l.reveal < 0.01);
    }

    #[test]
    fn menu_and_empty_workspace_keep_the_dock_visible() {
        let mut l = typical_layout();
        let now = Instant::now();
        assert!(!l.want_visible(now));
        l.menu_open = true;
        assert!(l.want_visible(now));
        l.menu_open = false;
        l.forced_visible = true;
        assert!(l.want_visible(now));
    }

    #[test]
    fn launch_bounce_decays_to_nothing() {
        let mut l = typical_layout();
        let t0 = Instant::now();
        l.bounce.insert("app0".into(), t0);
        let mid = l.bounce_offset("app0", t0 + Duration::from_millis(140));
        assert!(mid > 0.0 && mid <= BOUNCE_HEIGHT, "mid-bounce offset {mid}");
        assert_eq!(l.bounce_offset("app0", t0 + Duration::from_secs(10)), 0.0);
        assert_eq!(l.bounce_offset("missing", t0), 0.0);
    }

    #[test]
    fn slot_hit_testing_ignores_separators() {
        let mut l = typical_layout();
        l.layout();
        let sep = l.slots.iter().find(|s| s.kind == Kind::Sep).unwrap().clone();
        assert!(l
            .slot_at(sep.x + sep.w / 2.0)
            .is_none_or(|i| l.slots[i].kind != Kind::Sep));
        let first = l.slots[0].clone();
        assert_eq!(l.slot_at(first.x + first.w / 2.0), Some(0));
        assert_eq!(l.slot_at(-50.0), None);
    }
}
