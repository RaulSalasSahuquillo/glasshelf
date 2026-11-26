//! The system accent colour used by the "outline follows system colour" option.
//!
//! The source is Hyprland's own active window border (`general:col.active_border`), a generic
//! accent that any theme (and the wallpaper colour sync of the desktop) rewrites. The dock reads
//! it through its Hyprland IPC; this module only holds the pure parts: parsing the gradient text,
//! making the colours fit a thin outline, and the short colour tween.
use std::time::{Duration, Instant};

/// Duration of the colour change when the accent (or the option) changes.
pub const TWEEN: Duration = Duration::from_millis(250);

/// Border gradient: first and last colour (equal when the border is a single colour), linear RGB
/// in 0..1 as written by the user (sRGB values, alpha dropped).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gradient {
    pub a: [f32; 3],
    pub b: [f32; 3],
}

/// Parses the text of `getoption general:col.active_border`, e.g.
/// `custom type: ccfa785a ccfd8d2e 45deg` (also accepts `0xAARRGGBB` and plain `RRGGBB`).
/// Returns `None` when no colour is found (option unset, a Hyprland that answers otherwise).
pub fn parse_gradient(text: &str) -> Option<Gradient> {
    let mut cols: Vec<[f32; 3]> = vec![];
    // JSON answers carry the same text in a "custom" string: look only at that part, if present.
    let body = text.split("custom").nth(1).unwrap_or(text);
    let body = body.lines().next().unwrap_or("");
    for tok in body.split(|c: char| c.is_whitespace() || c == '"' || c == ',') {
        let t = tok.trim_start_matches("0x");
        if t.ends_with("deg") || !(t.len() == 8 || t.len() == 6) || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(v) = u32::from_str_radix(t, 16) else { continue };
        let v = if t.len() == 8 { v & 0x00ff_ffff } else { v };
        cols.push([
            ((v >> 16) & 0xff) as f32 / 255.0,
            ((v >> 8) & 0xff) as f32 / 255.0,
            (v & 0xff) as f32 / 255.0,
        ]);
    }
    let a = *cols.first()?;
    let b = if cols.len() > 1 { cols[1] } else { a };
    Some(Gradient { a, b })
}

/// Makes a colour usable as a thin glowing edge: too dark colours (a near-black border) would
/// vanish, so the brightest channel is raised to at least 0.78 keeping the hue. Bright colours
/// are untouched.
pub fn fit_outline(c: [f32; 3]) -> [f32; 3] {
    let m = c[0].max(c[1]).max(c[2]);
    if m >= 0.78 {
        return c;
    }
    if m < 0.02 {
        return [0.78; 3];
    }
    let k = 0.78 / m;
    [(c[0] * k).min(1.0), (c[1] * k).min(1.0), (c[2] * k).min(1.0)]
}

/// What the shader receives: both colours (already fitted) and the strength 0 (off) .. 1 (full).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub a: [f32; 3],
    pub b: [f32; 3],
    pub amt: f32,
}

impl Look {
    pub const OFF: Look = Look {
        a: [0.48, 0.64, 0.97],
        b: [0.48, 0.64, 0.97],
        amt: 0.0,
    };

    pub fn on(g: Gradient) -> Look {
        Look {
            a: fit_outline(g.a),
            b: fit_outline(g.b),
            amt: 1.0,
        }
    }

    fn lerp(&self, to: &Look, k: f32) -> Look {
        let m = |x: f32, y: f32| x + (y - x) * k;
        Look {
            a: [m(self.a[0], to.a[0]), m(self.a[1], to.a[1]), m(self.a[2], to.a[2])],
            b: [m(self.b[0], to.b[0]), m(self.b[1], to.b[1]), m(self.b[2], to.b[2])],
            amt: m(self.amt, to.amt),
        }
    }
}

/// Colour tween (ease-out cubic over `TWEEN`). Costs nothing at rest: `active` is false once done.
#[derive(Clone, Debug)]
pub struct Tween {
    from: Look,
    to: Look,
    start: Instant,
}

impl Tween {
    pub fn new(now: Instant) -> Tween {
        Tween {
            from: Look::OFF,
            to: Look::OFF,
            start: now,
        }
    }

    fn progress(&self, now: Instant) -> f32 {
        let t = now.saturating_duration_since(self.start).as_secs_f32() / TWEEN.as_secs_f32();
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }

    pub fn current(&self, now: Instant) -> Look {
        self.from.lerp(&self.to, self.progress(now))
    }

    pub fn target(&self) -> Look {
        self.to
    }

    pub fn active(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) < TWEEN
    }

    /// Starts a tween from what is shown right now to `to`; no-op when already heading there.
    /// When the strength is 0 the colours are kept (the outline fades out in its old colour);
    /// when coming from 0 they are taken at once, so only the strength fades in.
    pub fn retarget(&mut self, now: Instant, to: Look) {
        if to == self.to {
            return;
        }
        let mut from = self.current(now);
        if from.amt <= 0.001 {
            from.a = to.a;
            from.b = to.b;
        }
        self.from = from;
        self.to = to;
        self.start = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_hyprland_prints() {
        let g = parse_gradient("custom type: ccfa785a ccfd8d2e 45deg\nset: true\n").unwrap();
        assert!((g.a[0] - 250.0 / 255.0).abs() < 1e-4 && (g.a[1] - 120.0 / 255.0).abs() < 1e-4);
        assert!((g.b[0] - 253.0 / 255.0).abs() < 1e-4 && (g.b[2] - 46.0 / 255.0).abs() < 1e-4);
        let j = parse_gradient(
            r#"{"option": "general:col.active_border", "custom": "ff7aa2f7 4400ff00 45deg", "set": true }"#,
        )
        .unwrap();
        assert!((j.a[2] - 247.0 / 255.0).abs() < 1e-4);
        assert!((j.b[1] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn single_colour_and_odd_forms() {
        let g = parse_gradient("custom type: 0xff336699").unwrap();
        assert_eq!(g.a, g.b);
        assert!((g.a[0] - 0.2).abs() < 1e-4);
        assert!(parse_gradient("custom type: 45deg").is_none());
        assert!(parse_gradient("").is_none());
        assert!(parse_gradient("no such option").is_none());
        // the angle is never mistaken for a colour
        assert!(parse_gradient("custom type: 90deg 123456deg").is_none());
    }

    #[test]
    fn dark_colours_are_lifted_bright_ones_kept() {
        assert_eq!(fit_outline([0.9, 0.2, 0.1]), [0.9, 0.2, 0.1]);
        let f = fit_outline([0.2, 0.1, 0.1]);
        assert!((f[0] - 0.78).abs() < 1e-4);
        assert!((f[1] / f[0] - 0.5).abs() < 1e-3, "hue kept");
        assert_eq!(fit_outline([0.0, 0.0, 0.0]), [0.78; 3]);
        assert!(fit_outline([0.5, 0.5, 0.5]).iter().all(|c| *c <= 1.0));
    }

    #[test]
    fn tween_is_smooth_bounded_and_settles() {
        let t0 = Instant::now();
        let mut tw = Tween::new(t0);
        assert!(!tw.active(t0 + Duration::from_secs(1)));
        let red = Look::on(Gradient {
            a: [1.0, 0.0, 0.0],
            b: [1.0, 0.2, 0.0],
        });
        tw.retarget(t0, red);
        assert_eq!(tw.current(t0).amt, 0.0);
        let mid = tw.current(t0 + TWEEN / 2);
        assert!(mid.amt > 0.5 && mid.amt < 1.0, "ease-out: past half way at half time");
        assert!(tw.active(t0 + TWEEN / 2));
        let end = t0 + TWEEN + Duration::from_millis(1);
        assert_eq!(tw.current(end), red);
        assert!(!tw.active(end));
        // a new accent starts from what is shown, not from the old target
        let blue = Look::on(Gradient {
            a: [0.0, 0.0, 1.0],
            b: [0.0, 0.0, 1.0],
        });
        tw.retarget(end, blue);
        assert_eq!(tw.current(end).a, red.a);
        assert_eq!(tw.current(end + TWEEN).a, blue.a);
        // same target again: nothing restarts
        let s = tw.start;
        tw.retarget(end + Duration::from_millis(50), blue);
        assert_eq!(tw.start, s);
    }

    #[test]
    fn switching_on_fades_only_the_strength_and_off_keeps_the_colour() {
        let t0 = Instant::now();
        let mut tw = Tween::new(t0);
        let g = Look::on(Gradient {
            a: [1.0, 0.0, 0.0],
            b: [1.0, 0.0, 0.0],
        });
        tw.retarget(t0, g);
        assert_eq!(tw.current(t0).a, g.a, "no sweep through the placeholder colour");
        let t1 = t0 + Duration::from_secs(1);
        tw.retarget(t1, Look { amt: 0.0, ..g });
        let c = tw.current(t1 + TWEEN / 2);
        assert_eq!(c.a, g.a);
        assert!(c.amt < 1.0 && c.amt > 0.0);
    }
}
