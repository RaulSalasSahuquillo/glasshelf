//! CPU rasterisation helpers for the dock: icon-theme lookup, icon loading
//! (SVG / PNG), a small high-quality resampler, text rendering with skrifa +
//! tiny-skia, and a procedural placeholder icon.
//!
//! Everything here produces / consumes [`Bitmap`]: PREMULTIPLIED RGBA8,
//! row-major, top row first (exactly what a GL texture upload with
//! `GL_RGBA / GL_UNSIGNED_BYTE` and `premultiplied alpha` blending expects).
//!
//! The module is self-contained: drop it into `src/raster.rs` and add
//! `png`, `resvg` (no default features), `usvg` (no default features),
//! `tiny-skia` and `skrifa` to `Cargo.toml`.

// The module exposes a deliberately wider API than the dock uses today (extra sizes, theme
// introspection, text metrics); keep it, but do not warn about it.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use skrifa::instance::{Location, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tiny_skia::{
    FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, Rect, SpreadMode,
    Stroke, Transform,
};

// Bitmap

/// Premultiplied RGBA8 image, row-major, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

impl Bitmap {
    /// Fully transparent bitmap.
    pub fn new(w: u32, h: u32) -> Bitmap {
        Bitmap {
            w,
            h,
            rgba: vec![0; (w as usize) * (h as usize) * 4],
        }
    }

    /// Pixel at (x, y) as premultiplied `[r, g, b, a]`; transparent outside.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        if x >= self.w || y >= self.h {
            return [0; 4];
        }
        let i = ((y * self.w + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }

    fn from_pixmap(p: Pixmap) -> Bitmap {
        Bitmap {
            w: p.width(),
            h: p.height(),
            rgba: p.take(),
        }
    }
}

#[inline]
fn premul8(c: u8, a: u8) -> u8 {
    // exact round(c * a / 255)
    let t = c as u32 * a as u32 + 128;
    ((t + (t >> 8)) >> 8) as u8
}

// Icon theme lookup

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DirKind {
    Fixed,
    Scalable,
    Threshold,
}

#[derive(Clone, Debug)]
struct ThemeDir {
    rel: String,
    size: u32,
    scale: u32,
    kind: DirKind,
    min: u32,
    max: u32,
    threshold: u32,
}

impl ThemeDir {
    /// Nominal physical size (size * scale).
    fn px(&self) -> u32 {
        self.size * self.scale
    }

    /// Physical pixel range for which this directory is "suitable".
    fn range(&self) -> (u32, u32) {
        let s = self.scale;
        match self.kind {
            DirKind::Fixed => (self.px(), self.px()),
            DirKind::Scalable => (self.min * s, self.max * s),
            DirKind::Threshold => (
                self.size.saturating_sub(self.threshold) * s,
                (self.size + self.threshold) * s,
            ),
        }
    }
}

#[derive(Clone, Debug)]
struct Theme {
    name: String,
    /// `<base>/<name>` directories that exist on disk.
    roots: Vec<PathBuf>,
    dirs: Vec<ThemeDir>,
    inherits: Vec<String>,
}

/// A resolved XDG icon theme: the chosen theme plus its `Inherits=` chain,
/// followed by the fallbacks (Yaru, Adwaita) and `hicolor`.
#[derive(Clone, Debug)]
pub struct IconTheme {
    chain: Vec<Theme>,
    /// Directories searched for loose (unthemed) icons: the icon base
    /// directories themselves and `/usr/share/pixmaps`.
    loose: Vec<PathBuf>,
}

struct Candidate {
    /// Lower is better.
    cost: u32,
    /// How much a bitmap would be enlarged, in percent (100 = not at all).
    upscale_pct: u32,
    path: PathBuf,
}

/// Maximum enlargement (percent) accepted before looking for a better source.
const MAX_GOOD_UPSCALE_PCT: u32 = 150;

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

fn default_icon_base_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let home = home_dir();
    if let Some(h) = &home {
        v.push(h.join(".local/share/icons"));
        v.push(h.join(".icons"));
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|p| !p.is_empty()) {
        v.push(PathBuf::from(x).join("icons"));
    }
    let xdg = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let xdg = if xdg.is_empty() {
        "/usr/local/share:/usr/share".to_string()
    } else {
        xdg
    };
    for d in xdg.split(':').filter(|d| !d.is_empty()) {
        v.push(PathBuf::from(d).join("icons"));
    }
    v.push(PathBuf::from("/usr/share/icons"));
    v.push(PathBuf::from("/usr/local/share/icons"));
    v.push(PathBuf::from("/var/lib/snapd/desktop/icons"));
    v.push(PathBuf::from("/var/lib/flatpak/exports/share/icons"));
    if let Some(h) = &home {
        v.push(h.join(".local/share/flatpak/exports/share/icons"));
    }
    let mut seen = HashSet::new();
    v.retain(|p| p.is_dir() && seen.insert(p.clone()));
    v
}

/// Runs `gsettings get org.gnome.desktop.interface icon-theme` with a hard
/// timeout; returns the theme name without quotes.
fn gsettings_icon_theme(timeout: Duration) -> Option<String> {
    let mut child = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(3));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.take(4096).read_to_string(&mut out).ok()?;
    let name = out.trim().trim_matches('\'').trim_matches('"').trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// `gtk-icon-theme-name` from GTK 3/4 settings.ini (fallback for gsettings).
fn gtk_settings_icon_theme() -> Option<String> {
    let home = home_dir()?;
    for rel in [".config/gtk-3.0/settings.ini", ".config/gtk-4.0/settings.ini"] {
        let Ok(text) = fs::read_to_string(home.join(rel)) else {
            continue;
        };
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "gtk-icon-theme-name" {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

type IniSections = Vec<(String, HashMap<String, String>)>;

fn parse_ini(text: &str) -> IniSections {
    let mut out: IniSections = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            if let Some(name) = rest.strip_suffix(']') {
                out.push((name.to_string(), HashMap::new()));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if let Some((_, map)) = out.last_mut() {
                map.entry(k.trim().to_string())
                    .or_insert_with(|| v.trim().to_string());
            }
        }
    }
    out
}

fn split_list(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// Parses an `index.theme`: `(inherits, directories)`.
fn parse_index_theme(text: &str) -> (Vec<String>, Vec<ThemeDir>) {
    let sections = parse_ini(text);
    let Some((_, head)) = sections.iter().find(|(n, _)| n == "Icon Theme") else {
        return (Vec::new(), Vec::new());
    };
    let inherits: Vec<String> = head
        .get("Inherits")
        .map(|s| split_list(s).map(str::to_string).collect())
        .unwrap_or_default();
    let mut names: Vec<&str> = Vec::new();
    for key in ["Directories", "ScaledDirectories"] {
        if let Some(v) = head.get(key) {
            names.extend(split_list(v));
        }
    }
    let mut dirs = Vec::new();
    for name in names {
        let Some((_, sec)) = sections.iter().find(|(n, _)| n == name) else {
            continue;
        };
        let num = |k: &str| sec.get(k).and_then(|v| v.parse::<u32>().ok());
        let Some(size) = num("Size").filter(|s| *s > 0) else {
            continue;
        };
        let scale = num("Scale").unwrap_or(1).max(1);
        let kind = match sec.get("Type").map(|s| s.as_str()) {
            Some("Fixed") => DirKind::Fixed,
            Some("Scalable") => DirKind::Scalable,
            _ => DirKind::Threshold,
        };
        dirs.push(ThemeDir {
            rel: name.to_string(),
            size,
            scale,
            kind,
            min: num("MinSize").unwrap_or(size),
            max: num("MaxSize").unwrap_or(size),
            threshold: num("Threshold").unwrap_or(2),
        });
    }
    (inherits, dirs)
}

fn load_theme(name: &str, base_dirs: &[PathBuf]) -> Option<Theme> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return None;
    }
    let mut roots = Vec::new();
    let mut index: Option<(Vec<String>, Vec<ThemeDir>)> = None;
    for b in base_dirs {
        let root = b.join(name);
        if !root.is_dir() {
            continue;
        }
        if index.is_none() {
            if let Ok(text) = fs::read_to_string(root.join("index.theme")) {
                let parsed = parse_index_theme(&text);
                if !parsed.1.is_empty() {
                    index = Some(parsed);
                }
            }
        }
        roots.push(root);
    }
    if roots.is_empty() {
        return None;
    }
    let (inherits, mut dirs) = index.unwrap_or_default();
    // Keep only directories that actually exist in some root: far fewer stat
    // calls during lookups.
    dirs.retain(|d| roots.iter().any(|r| r.join(&d.rel).is_dir()));
    Some(Theme {
        name: name.to_string(),
        roots,
        dirs,
        inherits,
    })
}

fn png_dims(path: &Path) -> Option<(u32, u32)> {
    let mut f = fs::File::open(path).ok()?;
    let mut b = [0u8; 24];
    f.read_exact(&mut b).ok()?;
    if b[..8] != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] || &b[12..16] != b"IHDR" {
        return None;
    }
    Some((
        u32::from_be_bytes([b[16], b[17], b[18], b[19]]),
        u32::from_be_bytes([b[20], b[21], b[22], b[23]]),
    ))
}

/// Cost of using a bitmap of `px` physical pixels when `want` is wanted:
/// exact is best, larger is mildly penalised, smaller strongly.
fn bitmap_cost(px: u32, want: u32) -> (u32, u32) {
    if px >= want {
        (px - want, 100)
    } else {
        ((want - px) * 4, (want * 100) / px.max(1))
    }
}

fn svg_cost(range: (u32, u32), want: u32) -> u32 {
    if want >= range.0 && want <= range.1 {
        0
    } else {
        let d = if want < range.0 { range.0 - want } else { want - range.1 };
        d / 4
    }
}

impl Theme {
    fn lookup(&self, name: &str, want: u32) -> Option<Candidate> {
        // (cost, rank (svg first), Reverse(px), dir index, is_svg)
        let mut cands: Vec<(u32, u8, u32, usize, bool)> = Vec::with_capacity(self.dirs.len() * 2);
        for (i, d) in self.dirs.iter().enumerate() {
            cands.push((svg_cost(d.range(), want), 0, u32::MAX - d.px(), i, true));
            cands.push((bitmap_cost(d.px(), want).0, 1, u32::MAX - d.px(), i, false));
        }
        cands.sort();
        for (cost, _, _, i, svg) in cands {
            let d = &self.dirs[i];
            let file = format!("{}.{}", name, if svg { "svg" } else { "png" });
            for root in &self.roots {
                let p = root.join(&d.rel).join(&file);
                if p.is_file() {
                    let upscale_pct = if svg { 100 } else { bitmap_cost(d.px(), want).1 };
                    return Some(Candidate {
                        cost,
                        upscale_pct,
                        path: p,
                    });
                }
            }
        }
        None
    }
}

impl IconTheme {
    /// Detects the user's icon theme through
    /// `gsettings get org.gnome.desktop.interface icon-theme` (300 ms timeout),
    /// then GTK `settings.ini`, then falls back to Yaru, Adwaita and hicolor.
    pub fn detect() -> IconTheme {
        let name = gsettings_icon_theme(Duration::from_millis(300)).or_else(gtk_settings_icon_theme);
        IconTheme::new(name.as_deref())
    }

    /// Builds the theme chain for `name` (or only the fallbacks when `None`)
    /// using the standard icon search directories.
    pub fn new(name: Option<&str>) -> IconTheme {
        IconTheme::with_base_dirs(name, default_icon_base_dirs())
    }

    /// Like [`IconTheme::new`] with explicit icon base directories (the
    /// directories that contain theme folders). Used by tests.
    pub fn with_base_dirs(name: Option<&str>, base_dirs: Vec<PathBuf>) -> IconTheme {
        let mut chain: Vec<Theme> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert("hicolor".to_string());
        let mut roots_todo: Vec<String> = Vec::new();
        if let Some(n) = name {
            roots_todo.push(n.to_string());
        }
        roots_todo.push("Yaru".to_string());
        roots_todo.push("Adwaita".to_string());

        // Depth-first, in listed order; hicolor is implicit and always last.
        fn visit(
            name: &str,
            base_dirs: &[PathBuf],
            visited: &mut HashSet<String>,
            chain: &mut Vec<Theme>,
            depth: u32,
        ) {
            if depth > 16 || !visited.insert(name.to_string()) {
                return;
            }
            let Some(t) = load_theme(name, base_dirs) else {
                return;
            };
            let inherits = t.inherits.clone();
            chain.push(t);
            for i in inherits {
                visit(&i, base_dirs, visited, chain, depth + 1);
            }
        }
        for n in &roots_todo {
            visit(n, &base_dirs, &mut visited, &mut chain, 0);
        }
        if let Some(h) = load_theme("hicolor", &base_dirs) {
            chain.push(h);
        }
        let mut loose: Vec<PathBuf> = base_dirs.clone();
        let pix = PathBuf::from("/usr/share/pixmaps");
        if pix.is_dir() && !loose.contains(&pix) {
            loose.push(pix);
        }
        IconTheme { chain, loose }
    }

    /// Names of the themes consulted, in lookup order (debugging aid).
    pub fn chain_names(&self) -> Vec<String> {
        self.chain.iter().map(|t| t.name.clone()).collect()
    }

    /// Finds the best file for `name` for a physical size of `want_px`.
    ///
    /// Follows the XDG icon theme specification (theme first, then inherited
    /// themes, then hicolor, then loose files) with a size policy tuned for a
    /// dock: SVG whenever a directory offers one, otherwise the closest
    /// *larger* PNG; a smaller bitmap is only used if nothing better exists.
    /// An absolute path is returned as-is when it exists.
    pub fn find(&self, name: &str, want_px: u32) -> Option<PathBuf> {
        let want = want_px.max(1);
        if name.starts_with('/') {
            let p = PathBuf::from(name);
            return p.is_file().then_some(p);
        }
        if name.is_empty() || name.contains('/') || name.contains("..") {
            return None;
        }
        let mut best: Option<Candidate> = None;
        for t in &self.chain {
            if let Some(c) = t.lookup(name, want) {
                if c.upscale_pct <= MAX_GOOD_UPSCALE_PCT {
                    return Some(c.path);
                }
                if best.as_ref().map_or(true, |b| c.cost < b.cost) {
                    best = Some(c);
                }
            }
        }
        // Loose files (icon base dirs, /usr/share/pixmaps).
        for dir in &self.loose {
            for ext in ["svg", "png"] {
                let p = dir.join(format!("{name}.{ext}"));
                if !p.is_file() {
                    continue;
                }
                let (cost, upscale_pct) = if ext == "svg" {
                    (0, 100)
                } else {
                    match png_dims(&p) {
                        Some((w, h)) => bitmap_cost(w.max(h), want),
                        None => continue,
                    }
                };
                let c = Candidate {
                    cost,
                    upscale_pct,
                    path: p,
                };
                if best.is_none() {
                    // Nothing themed: anything beats nothing.
                    if c.upscale_pct <= MAX_GOOD_UPSCALE_PCT {
                        return Some(c.path);
                    }
                    best = Some(c);
                } else if best.as_ref().map_or(false, |b| b.upscale_pct > MAX_GOOD_UPSCALE_PCT)
                    && c.cost < best.as_ref().map_or(u32::MAX, |b| b.cost)
                {
                    best = Some(c);
                }
            }
        }
        best.map(|c| c.path)
    }
}

// Icon loading

const MAX_ICON_PX: u32 = 4096;
const MAX_FILE_BYTES: u64 = 96 * 1024 * 1024;

/// A decoded icon source, independent of the output size.
enum Source {
    /// Premultiplied RGBA8.
    Raster { w: u32, h: u32, rgba: Vec<u8> },
    Svg(Box<usvg::Tree>),
}

/// Loads an icon (theme name OR absolute path) as a `px` x `px` premultiplied
/// bitmap (aspect preserved, centred). Falls back to
/// `application-x-executable` and finally to a procedural placeholder, so the
/// result is `None` only for `px == 0`.
pub fn load_icon(theme: &IconTheme, icon: &str, px: u32) -> Option<Bitmap> {
    load_icon_colored(theme, icon, px, [255, 255, 255])
}

/// Like [`load_icon`], with the colour used for SVG `currentColor`
/// (symbolic icons). `load_icon` uses white.
pub fn load_icon_colored(
    theme: &IconTheme,
    icon: &str,
    px: u32,
    current_color: [u8; 3],
) -> Option<Bitmap> {
    if px == 0 || px > MAX_ICON_PX {
        return None;
    }
    resolve_source(theme, icon, px, current_color)
        .and_then(|s| render_source(&s, px))
        .or_else(|| Some(rounded_square_placeholder(px, PLACEHOLDER_RGB)))
}

/// Loads the same icon at several sizes while decoding the file only once
/// (the source is chosen for the largest size). One bitmap per entry of
/// `sizes`, in order; entries that are 0 or too large yield 1x1 transparent.
pub fn load_icon_sizes(theme: &IconTheme, icon: &str, sizes: &[u32]) -> Vec<Bitmap> {
    let max = sizes
        .iter()
        .copied()
        .filter(|s| *s > 0 && *s <= MAX_ICON_PX)
        .max();
    let src = max.and_then(|m| resolve_source(theme, icon, m, [255, 255, 255]));
    sizes
        .iter()
        .map(|&px| {
            if px == 0 || px > MAX_ICON_PX {
                return Bitmap::new(1, 1);
            }
            src.as_ref()
                .and_then(|s| render_source(s, px))
                .unwrap_or_else(|| rounded_square_placeholder(px, PLACEHOLDER_RGB))
        })
        .collect()
}

const PLACEHOLDER_RGB: [u8; 3] = [120, 126, 140];

/// Finds and decodes the first usable file for `icon`; falls back to the
/// generic executable icon. `None` means "use the placeholder".
fn resolve_source(theme: &IconTheme, icon: &str, want: u32, color: [u8; 3]) -> Option<Source> {
    let icon = icon.trim();
    let mut tried: Vec<PathBuf> = Vec::new();
    let mut attempt = |p: Option<PathBuf>| -> Option<Source> {
        let p = p?;
        if tried.contains(&p) {
            return None;
        }
        let r = decode_file(&p, color);
        tried.push(p);
        r
    };

    if !icon.is_empty() {
        let mut names: Vec<String> = vec![icon.to_string()];
        if !icon.starts_with('/') {
            for ext in [".png", ".svg", ".svgz", ".xpm", ".desktop"] {
                if let Some(s) = icon.strip_suffix(ext) {
                    names.push(s.to_string());
                }
            }
            names.push(icon.to_lowercase());
        }
        for n in &names {
            if let Some(s) = attempt(theme.find(n, want)) {
                return Some(s);
            }
        }
        if icon.starts_with('/') {
            // Absolute path whose extension is wrong / missing: try siblings.
            let p = Path::new(icon);
            for ext in ["svg", "png"] {
                if let Some(s) = attempt(Some(p.with_extension(ext)).filter(|q| q.is_file())) {
                    return Some(s);
                }
            }
        }
    }
    attempt(theme.find("application-x-executable", want))
}

fn decode_file(path: &Path, color: [u8; 3]) -> Option<Source> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let data = fs::read(path).ok()?;
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        let (w, h, mut rgba) = decode_png(&data)?;
        for p in rgba.chunks_exact_mut(4) {
            let a = p[3];
            if a != 255 {
                p[0] = premul8(p[0], a);
                p[1] = premul8(p[1], a);
                p[2] = premul8(p[2], a);
            }
        }
        return Some(Source::Raster { w, h, rgba });
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let head = &data[..data.len().min(4096)];
    let looks_svg = data.starts_with(&[0x1f, 0x8b])
        || ext == "svg"
        || ext == "svgz"
        || head.windows(4).any(|w| w == b"<svg");
    if looks_svg {
        return parse_svg(&data, path.parent(), color).map(|t| Source::Svg(Box::new(t)));
    }
    None
}

fn render_source(src: &Source, px: u32) -> Option<Bitmap> {
    match src {
        Source::Raster { w, h, rgba } => Some(fit_square(rgba, *w, *h, px)),
        Source::Svg(tree) => render_tree(tree, px),
    }
}

fn parse_svg(data: &[u8], dir: Option<&Path>, color: [u8; 3]) -> Option<usvg::Tree> {
    let opt = usvg::Options {
        resources_dir: dir.map(Path::to_path_buf),
        ..usvg::Options::default()
    };
    if data.starts_with(&[0x1f, 0x8b]) {
        return usvg::Tree::from_data(data, &opt).ok();
    }
    let text = String::from_utf8_lossy(data);
    let text = if text.contains("currentColor") || text.contains("currentcolor") {
        let hex = format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2]);
        text.replace("currentColor", &hex).replace("currentcolor", &hex)
    } else {
        text.into_owned()
    };
    usvg::Tree::from_str(&text, &opt).ok()
}

fn render_tree(tree: &usvg::Tree, px: u32) -> Option<Bitmap> {
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return None;
    }
    let s = px as f32 / w.max(h);
    let tx = (px as f32 - w * s) * 0.5;
    let ty = (px as f32 - h * s) * 0.5;
    let mut pm = Pixmap::new(px, px)?;
    resvg::render(tree, Transform::from_row(s, 0.0, 0.0, s, tx, ty), &mut pm.as_mut());
    Some(Bitmap::from_pixmap(pm))
}

/// Decodes any PNG into straight RGBA8.
fn decode_png(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let limits = png::Limits { bytes: 512 * 1024 * 1024 };
    let mut dec = png::Decoder::new_with_limits(Cursor::new(data), limits);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let (w, h) = {
        let i = reader.info();
        (i.width, i.height)
    };
    if w == 0 || h == 0 || (w as u64) * (h as u64) > 64 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let src = &buf[..info.buffer_size()];
    let mut out = vec![0u8; w * h * 4];
    match info.color_type {
        png::ColorType::Rgba => {
            if src.len() < w * h * 4 {
                return None;
            }
            out.copy_from_slice(&src[..w * h * 4]);
        }
        png::ColorType::Rgb => {
            for (d, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
                d.copy_from_slice(&[s[0], s[1], s[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for (d, s) in out.chunks_exact_mut(4).zip(src.iter()) {
                d.copy_from_slice(&[*s, *s, *s, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for (d, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(2)) {
                d.copy_from_slice(&[s[0], s[0], s[0], s[1]]);
            }
        }
        png::ColorType::Indexed => {
            // EXPAND should have removed this; handle defensively via palette.
            let info = reader.info();
            let pal = info.palette.as_ref()?;
            let trns = info.trns.as_ref();
            for (d, s) in out.chunks_exact_mut(4).zip(src.iter()) {
                let i = *s as usize;
                let (r, g, b) = (
                    *pal.get(i * 3)?,
                    *pal.get(i * 3 + 1)?,
                    *pal.get(i * 3 + 2)?,
                );
                let a = trns.and_then(|t| t.get(i).copied()).unwrap_or(255);
                d.copy_from_slice(&[r, g, b, a]);
            }
        }
    }
    Some((w as u32, h as u32, out))
}

/// Scales premultiplied RGBA (aspect preserved) to fit a `px` x `px` square
/// and centres it on a transparent canvas.
pub fn fit_square(rgba: &[u8], sw: u32, sh: u32, px: u32) -> Bitmap {
    let mut out = Bitmap::new(px, px);
    if sw == 0 || sh == 0 || px == 0 || rgba.len() < (sw as usize) * (sh as usize) * 4 {
        return out;
    }
    let k = px as f64 / sw.max(sh) as f64;
    let nw = ((sw as f64 * k).round() as u32).clamp(1, px);
    let nh = ((sh as f64 * k).round() as u32).clamp(1, px);
    let scaled;
    let data: &[u8] = if nw == sw && nh == sh {
        rgba
    } else {
        scaled = resample(rgba, sw as usize, sh as usize, nw as usize, nh as usize);
        &scaled
    };
    let ox = ((px - nw) / 2) as usize;
    let oy = ((px - nh) / 2) as usize;
    for y in 0..nh as usize {
        let d = ((oy + y) * px as usize + ox) * 4;
        let s = y * nw as usize * 4;
        out.rgba[d..d + nw as usize * 4].copy_from_slice(&data[s..s + nw as usize * 4]);
    }
    out
}

// Resampler: separable Lanczos-3 (support scaled when shrinking), edge clamp,
// premultiplied data, f32 accumulation.

struct Weights {
    first: Vec<u32>,
    count: Vec<u32>,
    offset: Vec<u32>,
    w: Vec<f32>,
}

fn lanczos(x: f32, a: f32) -> f32 {
    let x = x.abs();
    if x < 1e-6 {
        1.0
    } else if x >= a {
        0.0
    } else {
        let px = std::f32::consts::PI * x;
        a * px.sin() * (px / a).sin() / (px * px)
    }
}

fn build_weights(src: usize, dst: usize) -> Weights {
    let scale = src as f32 / dst as f32;
    let (support_scale, a) = if scale > 1.0 { (scale, 3.0) } else { (1.0, 3.0) };
    let radius = a * support_scale;
    let mut first = Vec::with_capacity(dst);
    let mut count = Vec::with_capacity(dst);
    let mut offset = Vec::with_capacity(dst);
    let mut w: Vec<f32> = Vec::new();
    for i in 0..dst {
        let center = (i as f32 + 0.5) * scale - 0.5;
        let lo = (center - radius).floor() as i64;
        let hi = (center + radius).ceil() as i64;
        let lo_c = lo.max(0);
        let hi_c = hi.min(src as i64 - 1);
        offset.push(w.len() as u32);
        let start = w.len();
        let n = (hi_c - lo_c + 1).max(1) as usize;
        w.resize(start + n, 0.0);
        // Edge clamping: taps outside [0, src) fold into the border pixel.
        for j in lo..=hi {
            let k = lanczos((j as f32 - center) / support_scale, a);
            let jc = j.clamp(lo_c, hi_c.max(lo_c));
            w[start + (jc - lo_c) as usize] += k;
        }
        let sum: f32 = w[start..start + n].iter().sum();
        if sum.abs() > 1e-8 {
            for v in &mut w[start..start + n] {
                *v /= sum;
            }
        } else {
            w[start] = 1.0;
        }
        first.push(lo_c as u32);
        count.push(n as u32);
    }
    Weights {
        first,
        count,
        offset,
        w,
    }
}

/// Integer box-average reduction of premultiplied RGBA8 (cheap pre-pass for
/// large sources; partial blocks at the right/bottom edge average what exists).
fn box_reduce(src: &[u8], sw: usize, sh: usize, f: usize) -> (Vec<u8>, usize, usize) {
    let nw = sw.div_ceil(f);
    let nh = sh.div_ceil(f);
    let mut out = vec![0u8; nw * nh * 4];
    for by in 0..nh {
        let y0 = by * f;
        let y1 = (y0 + f).min(sh);
        for bx in 0..nw {
            let x0 = bx * f;
            let x1 = (x0 + f).min(sw);
            let mut acc = [0u32; 4];
            for y in y0..y1 {
                let row = &src[(y * sw + x0) * 4..(y * sw + x1) * 4];
                for p in row.chunks_exact(4) {
                    acc[0] += p[0] as u32;
                    acc[1] += p[1] as u32;
                    acc[2] += p[2] as u32;
                    acc[3] += p[3] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (by * nw + bx) * 4;
            for c in 0..4 {
                out[o + c] = ((acc[c] + n / 2) / n) as u8;
            }
        }
    }
    (out, nw, nh)
}

/// Resizes premultiplied RGBA8 (`sw` x `sh`) to `dw` x `dh`: Lanczos-3 with a
/// support scaled to the shrink factor (a proper low-pass), preceded by an
/// exact box pre-reduction when shrinking by more than 3x.
pub fn resample(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < sw * sh * 4 {
        return vec![0; dw * dh * 4];
    }
    let ratio = (sw as f32 / dw as f32).min(sh as f32 / dh as f32);
    if ratio >= 3.0 {
        let f = (ratio / 1.5).floor() as usize;
        if f >= 2 {
            let (reduced, rw, rh) = box_reduce(src, sw, sh, f);
            return resample_lanczos(&reduced, rw, rh, dw, dh);
        }
    }
    resample_lanczos(src, sw, sh, dw, dh)
}

fn resample_lanczos(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let wx = build_weights(sw, dw);
    let wy = build_weights(sh, dh);
    // Horizontal pass -> f32 (dw x sh).
    let mut tmp = vec![0f32; dw * sh * 4];
    for y in 0..sh {
        let row = &src[y * sw * 4..(y + 1) * sw * 4];
        let trow = &mut tmp[y * dw * 4..(y + 1) * dw * 4];
        for x in 0..dw {
            let f = wx.first[x] as usize;
            let n = wx.count[x] as usize;
            let ws = &wx.w[wx.offset[x] as usize..wx.offset[x] as usize + n];
            let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
            for (k, wt) in ws.iter().enumerate() {
                let p = &row[(f + k) * 4..(f + k) * 4 + 4];
                r += p[0] as f32 * wt;
                g += p[1] as f32 * wt;
                b += p[2] as f32 * wt;
                a += p[3] as f32 * wt;
            }
            trow[x * 4..x * 4 + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    // Vertical pass -> u8.
    let mut out = vec![0u8; dw * dh * 4];
    let mut acc = vec![0f32; dw * 4];
    for y in 0..dh {
        let f = wy.first[y] as usize;
        let n = wy.count[y] as usize;
        let ws = &wy.w[wy.offset[y] as usize..wy.offset[y] as usize + n];
        acc.iter_mut().for_each(|v| *v = 0.0);
        for (k, wt) in ws.iter().enumerate() {
            let trow = &tmp[(f + k) * dw * 4..(f + k + 1) * dw * 4];
            for (a, t) in acc.iter_mut().zip(trow.iter()) {
                *a += t * wt;
            }
        }
        let orow = &mut out[y * dw * 4..(y + 1) * dw * 4];
        for x in 0..dw {
            let a = acc[x * 4 + 3].round().clamp(0.0, 255.0);
            let af = a;
            for c in 0..3 {
                // Premultiplied invariant: colour never exceeds alpha.
                orow[x * 4 + c] = acc[x * 4 + c].round().clamp(0.0, af) as u8;
            }
            orow[x * 4 + 3] = a as u8;
        }
    }
    out
}

// Placeholder icon

/// Procedurally drawn rounded-square (squircle-ish) icon with a vertical
/// gradient and a soft top highlight; premultiplied.
pub fn rounded_square_placeholder(px: u32, rgb: [u8; 3]) -> Bitmap {
    let px = px.max(1);
    let Some(mut pm) = Pixmap::new(px, px) else {
        return Bitmap::new(px, px);
    };
    let s = px as f32;
    let inset = s * 0.06;
    let rect = match Rect::from_xywh(inset, inset, s - 2.0 * inset, s - 2.0 * inset) {
        Some(r) => r,
        None => return Bitmap::from_pixmap(pm),
    };
    let radius = rect.width() * 0.225;
    let path = rounded_rect_path(rect, radius);
    let Some(path) = path else {
        return Bitmap::from_pixmap(pm);
    };
    let mix = |k: f32| -> tiny_skia::Color {
        // k > 0 lightens, k < 0 darkens
        let f = |c: u8| -> u8 {
            let c = c as f32;
            if k >= 0.0 {
                (c + (255.0 - c) * k).round() as u8
            } else {
                (c * (1.0 + k)).round() as u8
            }
        };
        tiny_skia::Color::from_rgba8(f(rgb[0]), f(rgb[1]), f(rgb[2]), 255)
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.shader = LinearGradient::new(
        Point::from_xy(0.0, rect.top()),
        Point::from_xy(0.0, rect.bottom()),
        vec![
            GradientStop::new(0.0, mix(0.22)),
            GradientStop::new(1.0, mix(-0.28)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or(tiny_skia::Shader::SolidColor(mix(0.0)));
    pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);

    // Thin inner highlight rim.
    let mut rim = Paint::default();
    rim.anti_alias = true;
    rim.set_color_rgba8(255, 255, 255, 46);
    let stroke = Stroke {
        width: (s * 0.012).max(1.0),
        ..Stroke::default()
    };
    pm.stroke_path(&path, &rim, &stroke, Transform::identity(), None);

    // A simple window glyph so it still reads as "an application".
    let gw = rect.width() * 0.46;
    let gh = rect.height() * 0.36;
    let gx = rect.left() + (rect.width() - gw) * 0.5;
    let gy = rect.top() + (rect.height() - gh) * 0.5;
    if let Some(gr) = Rect::from_xywh(gx, gy, gw, gh) {
        if let Some(gp) = rounded_rect_path(gr, gw * 0.12) {
            let mut gpnt = Paint::default();
            gpnt.anti_alias = true;
            gpnt.set_color_rgba8(255, 255, 255, 200);
            let gs = Stroke {
                width: (s * 0.035).max(1.0),
                ..Stroke::default()
            };
            pm.stroke_path(&gp, &gpnt, &gs, Transform::identity(), None);
            if let Some(bar) = Rect::from_xywh(gx, gy + gh * 0.28, gw, (s * 0.03).max(1.0)) {
                pm.fill_rect(bar, &gpnt, Transform::identity(), None);
            }
        }
    }
    Bitmap::from_pixmap(pm)
}

fn rounded_rect_path(r: Rect, rad: f32) -> Option<tiny_skia::Path> {
    // Cubic approximation of a circular corner.
    let k = 0.552_284_75 * rad;
    let (l, t, rt, b) = (r.left(), r.top(), r.right(), r.bottom());
    let mut pb = PathBuilder::new();
    pb.move_to(l + rad, t);
    pb.line_to(rt - rad, t);
    pb.cubic_to(rt - rad + k, t, rt, t + rad - k, rt, t + rad);
    pb.line_to(rt, b - rad);
    pb.cubic_to(rt, b - rad + k, rt - rad + k, b, rt - rad, b);
    pb.line_to(l + rad, b);
    pb.cubic_to(l + rad - k, b, l, b - rad + k, l, b - rad);
    pb.line_to(l, t + rad);
    pb.cubic_to(l, t + rad - k, l + rad - k, t, l + rad, t);
    pb.close();
    pb.finish()
}

// Text rendering

/// Result of [`TextRenderer::render_ex`].
#[derive(Clone, Debug)]
pub struct TextImage {
    /// White text, coverage in alpha (premultiplied white): tight ink bounds
    /// plus 1 px padding on every side.
    pub bitmap: Bitmap,
    /// Rows from the top of `bitmap` down to the baseline.
    pub baseline: i32,
    /// Columns from the left of `bitmap` to the pen origin (may be negative
    /// when the first glyph has a positive left bearing).
    pub origin_x: i32,
    /// Total advance width of the string in pixels (as `measure`).
    pub advance: f32,
}

struct Face {
    data: Arc<Vec<u8>>,
    index: u32,
    location: Location,
    synthetic_bold: bool,
}

impl Face {
    fn font(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(&self.data, self.index).ok()
    }
}

struct GlyphBmp {
    /// Left edge relative to the (integer part of the) pen position.
    left: i32,
    /// Top edge relative to the baseline, y down (negative = above).
    top: i32,
    w: u32,
    h: u32,
    alpha: Vec<u8>,
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct GlyphKey {
    face: u8,
    gid: u32,
    px_bits: u32,
    sub: u8,
}

/// Cached, anti-aliased vector text renderer.
pub struct TextRenderer {
    faces: [Face; 2],
    family: String,
    glyphs: HashMap<GlyphKey, Option<Arc<GlyphBmp>>>,
    kerning: HashMap<(u8, u32, u32), i32>,
}

struct PathPen {
    pb: PathBuilder,
    dx: f32,
}

impl OutlinePen for PathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb.move_to(x + self.dx, -y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb.line_to(x + self.dx, -y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.pb.quad_to(cx0 + self.dx, -cy0, x + self.dx, -y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.pb
            .cubic_to(cx0 + self.dx, -cy0, cx1 + self.dx, -cy1, x + self.dx, -y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// Subpixel positions per pixel for glyph positioning.
const SUBPIXELS: u32 = 4;
const GLYPH_CACHE_MAX: usize = 6000;

struct FontFiles {
    family: &'static str,
    /// Variable font (regular + bold through the `wght` axis).
    variable: Option<PathBuf>,
    regular: Option<PathBuf>,
    bold: Option<PathBuf>,
}

fn collect_fonts(dir: &Path, depth: u32, out: &mut Vec<(String, PathBuf)>) {
    if depth > 6 || out.len() > 40_000 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else {
            continue;
        };
        if ft.is_dir() {
            collect_fonts(&p, depth + 1, out);
        } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_lowercase();
            if lower.ends_with(".ttf") || lower.ends_with(".otf") {
                let stem = lower[..lower.len() - 4].to_string();
                out.push((stem, p));
            }
        }
    }
}

fn pick_family_files(all: &[(String, PathBuf)]) -> Vec<FontFiles> {
    let find = |pred: &dyn Fn(&str) -> bool| -> Option<PathBuf> {
        all.iter().find(|(s, _)| pred(s)).map(|(_, p)| p.clone())
    };
    vec![
        FontFiles {
            family: "Ubuntu Sans",
            variable: find(&|s| {
                s.starts_with("ubuntusans[") && !s.contains("italic") && !s.contains("mono")
            }),
            regular: find(&|s| s == "ubuntusans-regular"),
            bold: find(&|s| s == "ubuntusans-bold"),
        },
        FontFiles {
            family: "Inter",
            variable: find(&|s| {
                s == "intervariable" || s.starts_with("inter[") || s == "inter-variable"
            }),
            regular: find(&|s| s == "inter-regular" || s == "inter"),
            bold: find(&|s| s == "inter-bold"),
        },
        FontFiles {
            family: "DejaVu Sans",
            variable: None,
            regular: find(&|s| s == "dejavusans"),
            bold: find(&|s| s == "dejavusans-bold"),
        },
    ]
}

fn fc_match_file(pattern: &str) -> Option<PathBuf> {
    let mut child = Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => {
                if start.elapsed() > Duration::from_millis(1500) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(3));
            }
        }
    }
    let mut s = String::new();
    child.stdout.take()?.take(4096).read_to_string(&mut s).ok()?;
    let p = PathBuf::from(s.trim());
    p.is_file().then_some(p)
}

fn load_font_bytes(p: &Path) -> Option<Arc<Vec<u8>>> {
    let data = fs::read(p).ok()?;
    let f = FontRef::from_index(&data, 0).ok()?;
    f.charmap().map('A')?;
    Some(Arc::new(data))
}

fn make_location(data: &[u8], weight: Option<f32>) -> Location {
    match FontRef::from_index(data, 0) {
        Ok(f) => {
            let axes = f.axes();
            match weight {
                Some(w) => axes.location([("wght", w)]),
                None => axes.location(std::iter::empty::<(&str, f32)>()),
            }
        }
        Err(_) => Location::default(),
    }
}

impl TextRenderer {
    /// Finds a system font (Ubuntu Sans, Inter, DejaVu Sans, in that order;
    /// then whatever `fc-match sans` says) and builds a renderer.
    pub fn new() -> Option<TextRenderer> {
        let mut dirs: Vec<PathBuf> = vec![
            PathBuf::from("/usr/share/fonts"),
            PathBuf::from("/usr/local/share/fonts"),
        ];
        if let Some(h) = home_dir() {
            dirs.push(h.join(".local/share/fonts"));
            dirs.push(h.join(".fonts"));
        }
        let mut all = Vec::new();
        for d in &dirs {
            collect_fonts(d, 0, &mut all);
        }
        for ff in pick_family_files(&all) {
            if let Some(v) = &ff.variable {
                if let Some(data) = load_font_bytes(v) {
                    let reg = make_location(&data, Some(400.0));
                    let bold = make_location(&data, Some(700.0));
                    return Some(TextRenderer::from_faces(
                        Face { data: data.clone(), index: 0, location: reg, synthetic_bold: false },
                        Face { data, index: 0, location: bold, synthetic_bold: false },
                        ff.family,
                    ));
                }
            }
            if let Some(r) = &ff.regular {
                if let Some(rd) = load_font_bytes(r) {
                    let rl = make_location(&rd, None);
                    let bold_face = ff
                        .bold
                        .as_ref()
                        .and_then(|b| load_font_bytes(b))
                        .map(|bd| {
                            let l = make_location(&bd, None);
                            Face { data: bd, index: 0, location: l, synthetic_bold: false }
                        })
                        .unwrap_or_else(|| Face {
                            data: rd.clone(),
                            index: 0,
                            location: make_location(&rd, Some(700.0)),
                            synthetic_bold: true,
                        });
                    return Some(TextRenderer::from_faces(
                        Face { data: rd, index: 0, location: rl, synthetic_bold: false },
                        bold_face,
                        ff.family,
                    ));
                }
            }
        }
        // Last resort: ask fontconfig.
        let reg = fc_match_file("sans")?;
        let rd = load_font_bytes(&reg)?;
        let bold_face = fc_match_file("sans:bold")
            .filter(|p| *p != reg)
            .and_then(|p| load_font_bytes(&p))
            .map(|bd| {
                let l = make_location(&bd, None);
                Face { data: bd, index: 0, location: l, synthetic_bold: false }
            })
            .unwrap_or_else(|| Face {
                data: rd.clone(),
                index: 0,
                location: make_location(&rd, Some(700.0)),
                synthetic_bold: true,
            });
        let rl = make_location(&rd, Some(400.0));
        Some(TextRenderer::from_faces(
            Face { data: rd, index: 0, location: rl, synthetic_bold: false },
            bold_face,
            "sans (fontconfig)",
        ))
    }

    fn from_faces(regular: Face, bold: Face, family: &str) -> TextRenderer {
        TextRenderer {
            faces: [regular, bold],
            family: family.to_string(),
            glyphs: HashMap::new(),
            kerning: HashMap::new(),
        }
    }

    /// Family name actually in use (for logs / tests).
    pub fn family(&self) -> &str {
        &self.family
    }

    /// `(ascent, descent)` in pixels, both positive.
    pub fn vertical_metrics(&self, px: f32, bold: bool) -> (f32, f32) {
        let face = &self.faces[bold as usize];
        let Some(font) = face.font() else {
            return (px * 0.9, px * 0.25);
        };
        let m = font.metrics(Size::new(px), &face.location);
        (m.ascent.abs(), m.descent.abs())
    }

    /// `(advance width, line height)` in pixels. Width is the sum of the
    /// (kerned) advances; height is ascent + descent.
    pub fn measure(&mut self, text: &str, px: f32, bold: bool) -> (f32, f32) {
        let px = sanitize_px(px);
        let (asc, desc) = self.vertical_metrics(px, bold);
        let (_, adv) = self.layout(text, px, bold);
        (adv, asc + desc)
    }

    /// Renders `text` as white coverage (premultiplied white): tight ink
    /// bounds + 1 px padding. See [`TextRenderer::render_ex`] for baseline
    /// information.
    pub fn render(&mut self, text: &str, px: f32, bold: bool) -> Bitmap {
        self.render_ex(text, px, bold).bitmap
    }

    /// Like [`render`](TextRenderer::render) but also returns where the
    /// baseline and the pen origin are inside the bitmap.
    pub fn render_ex(&mut self, text: &str, px: f32, bold: bool) -> TextImage {
        let px = sanitize_px(px);
        let (placed, advance) = self.layout(text, px, bold);
        let face_idx = bold as u8;

        // Rasterise (cached) glyphs at quarter-pixel subpixel offsets.
        let mut items: Vec<(i32, Arc<GlyphBmp>)> = Vec::with_capacity(placed.len());
        for (gid, x) in placed {
            let q = (x * SUBPIXELS as f32).round() as i32;
            let xi = q.div_euclid(SUBPIXELS as i32);
            let sub = q.rem_euclid(SUBPIXELS as i32) as u8;
            if let Some(g) = self.glyph(face_idx, gid, px, sub) {
                items.push((xi, g));
            }
        }
        if items.is_empty() {
            return TextImage {
                bitmap: Bitmap::new(2, 2),
                baseline: 1,
                origin_x: 1,
                advance,
            };
        }
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for (xi, g) in &items {
            x0 = x0.min(xi + g.left);
            y0 = y0.min(g.top);
            x1 = x1.max(xi + g.left + g.w as i32);
            y1 = y1.max(g.top + g.h as i32);
        }
        let cw = (x1 - x0) as usize;
        let ch = (y1 - y0) as usize;
        let mut cov = vec![0u8; cw * ch];
        for (xi, g) in &items {
            let gx = (xi + g.left - x0) as usize;
            let gy = (g.top - y0) as usize;
            for row in 0..g.h as usize {
                let d = &mut cov[(gy + row) * cw + gx..(gy + row) * cw + gx + g.w as usize];
                let s = &g.alpha[row * g.w as usize..(row + 1) * g.w as usize];
                for (dv, sv) in d.iter_mut().zip(s) {
                    if *sv != 0 {
                        let (a, b) = (*dv as u32, *sv as u32);
                        *dv = (a + b - (a * b + 127) / 255) as u8;
                    }
                }
            }
        }
        // Tight crop to non-zero coverage.
        let (mut tx0, mut ty0, mut tx1, mut ty1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for y in 0..ch {
            for x in 0..cw {
                if cov[y * cw + x] != 0 {
                    tx0 = tx0.min(x);
                    tx1 = tx1.max(x + 1);
                    ty0 = ty0.min(y);
                    ty1 = ty1.max(y + 1);
                }
            }
        }
        if tx0 == usize::MAX {
            return TextImage {
                bitmap: Bitmap::new(2, 2),
                baseline: 1,
                origin_x: 1,
                advance,
            };
        }
        let ow = tx1 - tx0 + 2;
        let oh = ty1 - ty0 + 2;
        let mut bmp = Bitmap::new(ow as u32, oh as u32);
        for y in ty0..ty1 {
            for x in tx0..tx1 {
                let a = cov[y * cw + x];
                if a != 0 {
                    let i = ((y - ty0 + 1) * ow + (x - tx0 + 1)) * 4;
                    bmp.rgba[i..i + 4].copy_from_slice(&[a, a, a, a]);
                }
            }
        }
        // Pen origin / baseline in output coordinates.
        let origin_x = -(x0 + tx0 as i32) + 1;
        let baseline = -(y0 + ty0 as i32) + 1;
        TextImage {
            bitmap: bmp,
            baseline,
            origin_x,
            advance,
        }
    }

    /// Maps text to glyph ids with pen positions (px) and the total advance.
    fn layout(&mut self, text: &str, px: f32, bold: bool) -> (Vec<(u32, f32)>, f32) {
        let fi = bold as usize;
        let mut out: Vec<(u32, f32)> = Vec::with_capacity(text.len());
        let mut pen = 0.0f32;
        let (data, index, location, synth) = {
            let f = &self.faces[fi];
            (f.data.clone(), f.index, f.location.clone(), f.synthetic_bold)
        };
        let Ok(font) = FontRef::from_index(&data, index) else {
            return (out, 0.0);
        };
        let charmap = font.charmap();
        let gm = font.glyph_metrics(Size::unscaled(), &location);
        let upem = font
            .head()
            .map(|h| h.units_per_em() as f32)
            .unwrap_or(1000.0)
            .max(1.0);
        let k = px / upem;
        let extra = if synth { px * 0.035 } else { 0.0 };
        let gpos = font.gpos().ok();
        let mut prev: Option<u32> = None;
        for ch in text.chars() {
            let ch = match ch {
                '\t' | '\u{a0}' | '\u{2009}' | '\u{202f}' => ' ',
                '\n' | '\r' | '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{ad}' => {
                    continue
                }
                c => c,
            };
            let gid = charmap
                .map(ch)
                .or_else(|| charmap.map('?'))
                .unwrap_or(GlyphId::NOTDEF)
                .to_u32();
            if let (Some(p), Some(g)) = (prev, gpos.as_ref()) {
                let key = (fi as u8, p, gid);
                let kern = match self.kerning.get(&key) {
                    Some(v) => *v,
                    None => {
                        let v = gpos_kern(g, p, gid);
                        self.kerning.insert(key, v);
                        v
                    }
                };
                pen += kern as f32 * k;
            }
            out.push((gid, pen));
            let adv = gm.advance_width(GlyphId::new(gid)).unwrap_or(upem * 0.5);
            pen += adv * k + extra;
            prev = Some(gid);
        }
        (out, pen)
    }

    fn glyph(&mut self, face: u8, gid: u32, px: f32, sub: u8) -> Option<Arc<GlyphBmp>> {
        let key = GlyphKey {
            face,
            gid,
            px_bits: px.to_bits(),
            sub,
        };
        if let Some(g) = self.glyphs.get(&key) {
            return g.clone();
        }
        if self.glyphs.len() >= GLYPH_CACHE_MAX {
            self.glyphs.clear();
        }
        let g = self.rasterize(face, gid, px, sub).map(Arc::new);
        self.glyphs.insert(key, g.clone());
        g
    }

    fn rasterize(&self, face: u8, gid: u32, px: f32, sub: u8) -> Option<GlyphBmp> {
        let f = &self.faces[face as usize];
        let font = f.font()?;
        let glyph = font.outline_glyphs().get(GlyphId::new(gid))?;
        let mut pen = PathPen {
            pb: PathBuilder::new(),
            dx: sub as f32 / SUBPIXELS as f32,
        };
        glyph
            .draw(DrawSettings::unhinted(Size::new(px), &f.location), &mut pen)
            .ok()?;
        let path = pen.pb.finish()?;
        let b = path.bounds();
        let stroke_w = if f.synthetic_bold { px * 0.035 } else { 0.0 };
        let x0 = (b.left() - stroke_w).floor() as i32 - 1;
        let y0 = (b.top() - stroke_w).floor() as i32 - 1;
        let x1 = (b.right() + stroke_w).ceil() as i32 + 1;
        let y1 = (b.bottom() + stroke_w).ceil() as i32 + 1;
        let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
        if w == 0 || h == 0 || w > 2048 || h > 2048 {
            return None;
        }
        let mut pm = Pixmap::new(w, h)?;
        let mut paint = Paint::default();
        paint.set_color_rgba8(255, 255, 255, 255);
        paint.anti_alias = true;
        let t = Transform::from_translate(-x0 as f32, -y0 as f32);
        pm.fill_path(&path, &paint, FillRule::Winding, t, None);
        if stroke_w > 0.0 {
            let stroke = Stroke {
                width: stroke_w,
                ..Stroke::default()
            };
            pm.stroke_path(&path, &paint, &stroke, t, None);
        }
        let alpha: Vec<u8> = pm.data().chunks_exact(4).map(|p| p[3]).collect();
        Some(GlyphBmp {
            left: x0,
            top: y0,
            w,
            h,
            alpha,
        })
    }
}

fn sanitize_px(px: f32) -> f32 {
    if px.is_finite() {
        px.clamp(1.0, 512.0)
    } else {
        13.0
    }
}

/// Pair-kerning (design units) from the GPOS `kern` feature of the default
/// script / language system. Returns 0 when nothing applies.
fn gpos_kern(gpos: &skrifa::raw::tables::gpos::Gpos<'_>, left: u32, right: u32) -> i32 {
    use skrifa::raw::tables::gpos::{PairPos, PositionSubtables};
    use skrifa::raw::types::Tag;
    let (Ok(scripts), Ok(features), Ok(lookups)) =
        (gpos.script_list(), gpos.feature_list(), gpos.lookup_list())
    else {
        return 0;
    };
    let Some(sel) = scripts.select(&[Tag::new(b"latn")]) else {
        return 0;
    };
    let Ok(script) = scripts.get(sel.index).map(|s| s.element) else {
        return 0;
    };
    let Some(Ok(langsys)) = script.default_lang_sys() else {
        return 0;
    };
    let kern_tag = Tag::new(b"kern");
    let mut indices: Vec<u16> = Vec::new();
    for fi in langsys.feature_indices() {
        let Some(rec) = features.feature_records().get(fi.get() as usize) else {
            continue;
        };
        if rec.feature_tag() != kern_tag {
            continue;
        }
        if let Ok(feat) = rec.feature(features.offset_data()) {
            indices.extend(feat.lookup_list_indices().iter().map(|i| i.get()));
        }
    }
    indices.sort_unstable();
    indices.dedup();
    let (Ok(l16), Ok(r16)) = (
        skrifa::raw::types::GlyphId16::try_from(GlyphId::new(left)),
        skrifa::raw::types::GlyphId16::try_from(GlyphId::new(right)),
    ) else {
        return 0;
    };
    let lookup_arr = lookups.lookups();
    for li in indices {
        let Ok(lookup) = lookup_arr.get(li as usize) else {
            continue;
        };
        let Ok(PositionSubtables::Pair(subs)) = lookup.subtables() else {
            continue;
        };
        for sub in subs.iter() {
            let Ok(sub) = sub else {
                continue;
            };
            match sub {
                PairPos::Format1(f1) => {
                    let Ok(cov) = f1.coverage() else { continue };
                    let Some(ci) = cov.get(left) else { continue };
                    let Ok(set) = f1.pair_sets().get(ci as usize) else {
                        continue;
                    };
                    for rec in set.pair_value_records().iter().flatten() {
                        if rec.second_glyph() == r16 {
                            return rec.value_record1.x_advance().unwrap_or(0) as i32;
                        }
                    }
                }
                PairPos::Format2(f2) => {
                    let Ok(cov) = f2.coverage() else { continue };
                    if cov.get(left).is_none() {
                        continue;
                    }
                    let (Ok(c1), Ok(c2)) = (f2.class_def1(), f2.class_def2()) else {
                        continue;
                    };
                    let (cl1, cl2) = (c1.get(l16), c2.get(r16));
                    let Ok(rec1) = f2.class1_records().get(cl1 as usize) else {
                        continue;
                    };
                    let Ok(rec2) = rec1.class2_records().get(cl2 as usize) else {
                        continue;
                    };
                    return rec2.value_record1().x_advance().unwrap_or(0) as i32;
                }
            }
        }
    }
    0
}

// Unit tests

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        // Under the system temp dir (never inside the repository).
        let d = std::env::temp_dir().join("glassdock-tests").join(format!("{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let _ = fs::create_dir_all(&d);
        d
    }

    fn write_png(path: &Path, w: u32, h: u32, ct: png::ColorType, depth: png::BitDepth, data: &[u8]) {
        if let Some(p) = path.parent() {
            let _ = fs::create_dir_all(p);
        }
        let f = fs::File::create(path).expect("create png");
        let mut enc = png::Encoder::new(f, w, h);
        enc.set_color(ct);
        enc.set_depth(depth);
        let mut wr = enc.write_header().expect("header");
        wr.write_image_data(data).expect("data");
    }

    fn solid_rgba(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        (0..w * h).flat_map(|_| px).collect()
    }

    #[test]
    fn premultiplication_straight_png() {
        let dir = tmp_dir("premul");
        let p = dir.join("a.png");
        // straight (200, 100, 50) at alpha 128
        write_png(&p, 4, 4, png::ColorType::Rgba, png::BitDepth::Eight, &solid_rgba(4, 4, [200, 100, 50, 128]));
        let theme = IconTheme::with_base_dirs(None, vec![]);
        let b = load_icon(&theme, p.to_str().expect("utf8"), 4).expect("icon");
        assert_eq!(b.pixel(1, 1), [100, 50, 25, 128]);
        // invariant: colour <= alpha everywhere
        assert!(b.rgba.chunks_exact(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
    }

    #[test]
    fn png_color_types_and_depths() {
        let dir = tmp_dir("ctypes");
        let theme = IconTheme::with_base_dirs(None, vec![]);
        let load = |name: &str| load_icon(&theme, dir.join(name).to_str().expect("utf8"), 8).expect("icon");
        write_png(&dir.join("g8.png"), 2, 2, png::ColorType::Grayscale, png::BitDepth::Eight, &[10, 10, 10, 10]);
        assert_eq!(load("g8.png").pixel(4, 4), [10, 10, 10, 255]);
        write_png(&dir.join("ga8.png"), 2, 2, png::ColorType::GrayscaleAlpha, png::BitDepth::Eight, &[200, 100, 200, 100, 200, 100, 200, 100]);
        let p = load("ga8.png").pixel(4, 4);
        assert_eq!(p[3], 100);
        assert_eq!(p[0], premul8(200, 100));
        write_png(&dir.join("rgb16.png"), 1, 1, png::ColorType::Rgb, png::BitDepth::Sixteen, &[0xff, 0xff, 0x80, 0x00, 0x00, 0x00]);
        let p = load("rgb16.png").pixel(4, 4);
        assert_eq!([p[0], p[3]], [255, 255]);
        assert!(p[1] >= 127 && p[1] <= 128 && p[2] == 0);
        write_png(&dir.join("g1.png"), 8, 1, png::ColorType::Grayscale, png::BitDepth::One, &[0b1010_1010]);
        assert_eq!(load("g1.png").pixel(0, 3)[3], 255); // 8x1 centred on row 3
    }

    #[test]
    fn indexed_with_trns_is_transparent() {
        let dir = tmp_dir("indexed");
        let p = dir.join("i.png");
        let f = fs::File::create(&p).expect("create");
        let mut enc = png::Encoder::new(f, 2, 1);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette(vec![255, 0, 0, 0, 255, 0]);
        enc.set_trns(vec![0, 255]);
        let mut w = enc.write_header().expect("hdr");
        w.write_image_data(&[0, 1]).expect("data");
        drop(w);
        let theme = IconTheme::with_base_dirs(None, vec![]);
        let b = load_icon(&theme, p.to_str().expect("utf8"), 2).expect("icon");
        assert_eq!(b.pixel(0, 0), [0, 0, 0, 0]);
        assert_eq!(b.pixel(1, 0), [0, 255, 0, 255]);
    }

    #[test]
    fn resize_preserves_flat_colour_and_alpha_invariant() {
        let src = solid_rgba(37, 23, [90, 60, 30, 120]);
        for (dw, dh) in [(10, 6), (100, 62), (37, 23), (1, 1), (200, 5)] {
            let out = resample(&src, 37, 23, dw, dh);
            assert_eq!(out.len(), dw * dh * 4);
            for p in out.chunks_exact(4) {
                assert!((p[0] as i32 - 90).abs() <= 1, "{p:?}");
                assert!((p[3] as i32 - 120).abs() <= 1, "{p:?}");
                assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
            }
        }
    }

    #[test]
    fn resize_downscale_averages_checker() {
        let mut src = vec![0u8; 16 * 16 * 4];
        for y in 0..16 {
            for x in 0..16 {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                let i = (y * 16 + x) * 4;
                src[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let out = resample(&src, 16, 16, 4, 4);
        for p in out.chunks_exact(4).skip(4).take(8) {
            assert!((p[0] as i32 - 127).abs() <= 24, "{p:?}");
        }
    }

    #[test]
    fn fit_square_centres_and_keeps_aspect() {
        let src = solid_rgba(20, 10, [255, 0, 0, 255]);
        let b = fit_square(&src, 20, 10, 40);
        assert_eq!((b.w, b.h), (40, 40));
        assert_eq!(b.pixel(20, 20)[3], 255);
        assert_eq!(b.pixel(20, 2)[3], 0); // padding above
        assert_eq!(b.pixel(20, 37)[3], 0); // padding below
        assert_eq!(b.pixel(0, 20)[3], 255); // full width
        assert_eq!(b.pixel(39, 20)[3], 255);
    }

    #[test]
    fn index_theme_parsing() {
        let (inh, dirs) = parse_index_theme(
            "[Icon Theme]\nName=T\nInherits=A, B\nDirectories=16x16/apps,scalable/apps,48x48@2x/apps\n\
             [16x16/apps]\nSize=16\nType=Fixed\n[scalable/apps]\nSize=128\nMinSize=8\nMaxSize=512\nType=Scalable\n\
             [48x48@2x/apps]\nSize=48\nScale=2\n",
        );
        assert_eq!(inh, vec!["A", "B"]);
        assert_eq!(dirs.len(), 3);
        assert_eq!(dirs[1].range(), (8, 512));
        assert_eq!(dirs[2].px(), 96);
        assert_eq!(dirs[2].kind, DirKind::Threshold);
        assert_eq!(dirs[2].range(), (92, 100));
    }

    fn mk_theme(base: &Path) {
        let t = base.join("T");
        fs::create_dir_all(t.join("48x48/apps")).expect("mk");
        fs::create_dir_all(t.join("128x128/apps")).expect("mk");
        fs::create_dir_all(t.join("scalable/apps")).expect("mk");
        fs::create_dir_all(t.join("32x32@2x/apps")).expect("mk");
        fs::write(
            t.join("index.theme"),
            "[Icon Theme]\nName=T\nInherits=P\nDirectories=48x48/apps,128x128/apps,scalable/apps,32x32@2x/apps\n\
             [48x48/apps]\nSize=48\nType=Fixed\n[128x128/apps]\nSize=128\nType=Fixed\n\
             [scalable/apps]\nSize=64\nMinSize=16\nMaxSize=256\nType=Scalable\n\
             [32x32@2x/apps]\nSize=32\nScale=2\nType=Fixed\n",
        )
        .expect("write");
        let p = base.join("P");
        fs::create_dir_all(p.join("256x256/apps")).expect("mk");
        fs::write(
            p.join("index.theme"),
            "[Icon Theme]\nName=P\nDirectories=256x256/apps\n[256x256/apps]\nSize=256\nType=Fixed\n",
        )
        .expect("write");
        let d = solid_rgba(2, 2, [1, 2, 3, 255]);
        let rgba = |path: PathBuf| write_png(&path, 2, 2, png::ColorType::Rgba, png::BitDepth::Eight, &d);
        rgba(t.join("48x48/apps/only48.png"));
        rgba(t.join("48x48/apps/both.png"));
        rgba(t.join("128x128/apps/both.png"));
        rgba(t.join("32x32@2x/apps/hidpi.png"));
        rgba(t.join("48x48/apps/hidpi.png"));
        fs::write(t.join("scalable/apps/vec.svg"), "<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'/>").expect("write");
        rgba(t.join("48x48/apps/vec.png"));
        rgba(p.join("256x256/apps/inherited.png"));
        rgba(p.join("256x256/apps/only48.png"));
    }

    #[test]
    fn find_follows_sizes_svg_scale_and_inherits() {
        let base = tmp_dir("find");
        mk_theme(&base);
        let th = IconTheme::with_base_dirs(Some("T"), vec![base.clone()]);
        assert_eq!(th.chain_names().first().map(String::as_str), Some("T"));
        assert!(th.chain_names().contains(&"P".to_string()));
        // closest larger PNG
        assert!(th.find("both", 100).expect("both").ends_with("128x128/apps/both.png"));
        assert!(th.find("both", 40).expect("both").ends_with("48x48/apps/both.png"));
        // SVG preferred over PNG when a scalable dir has it
        assert!(th.find("vec", 160).expect("vec").ends_with("scalable/apps/vec.svg"));
        // Scale=2 directory (physical 64) preferred for 60..64
        assert!(th.find("hidpi", 64).expect("hidpi").ends_with("32x32@2x/apps/hidpi.png"));
        // inherited theme
        assert!(th.find("inherited", 160).expect("inh").ends_with("P/256x256/apps/inherited.png"));
        // 48px icon in T would be a 3.3x upscale for 160: inherited 256 one is preferred
        assert!(th.find("only48", 160).expect("only48").to_string_lossy().contains("/P/256x256"));
        assert!(th.find("missing-icon", 64).is_none());
        // absolute paths pass through
        let abs = base.join("T/48x48/apps/both.png");
        assert_eq!(th.find(abs.to_str().expect("utf8"), 64), Some(abs.clone()));
    }

    #[test]
    fn load_icon_never_fails_and_is_square() {
        let theme = IconTheme::with_base_dirs(None, vec![]);
        let b = load_icon(&theme, "definitely-not-an-icon", 88).expect("fallback");
        assert_eq!((b.w, b.h), (88, 88));
        assert!(b.rgba.chunks_exact(4).any(|p| p[3] == 255));
        assert!(b.rgba.chunks_exact(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
        assert!(load_icon(&theme, "x", 0).is_none());
        let bad = tmp_dir("bad").join("broken.png");
        fs::write(&bad, b"\x89PNG not really").expect("write");
        let b = load_icon(&theme, bad.to_str().expect("utf8"), 32).expect("fallback");
        assert_eq!((b.w, b.h), (32, 32));
    }

    #[test]
    fn svg_current_color_and_missing_viewbox() {
        let dir = tmp_dir("svg");
        let p = dir.join("a.svg");
        fs::write(&p, "<svg xmlns='http://www.w3.org/2000/svg' width='20' height='10'><rect width='20' height='10' fill='currentColor'/></svg>").expect("write");
        let theme = IconTheme::with_base_dirs(None, vec![]);
        let b = load_icon_colored(&theme, p.to_str().expect("utf8"), 40, [255, 0, 0]).expect("svg");
        assert_eq!((b.w, b.h), (40, 40));
        assert_eq!(b.pixel(20, 20), [255, 0, 0, 255]);
        assert_eq!(b.pixel(20, 2)[3], 0); // centred: transparent band on top
        let b = load_icon(&theme, p.to_str().expect("utf8"), 40).expect("svg");
        assert_eq!(b.pixel(20, 20), [255, 255, 255, 255]);
    }

    #[test]
    fn load_icon_sizes_decodes_once_and_matches_sizes() {
        let dir = tmp_dir("sizes");
        let p = dir.join("big.png");
        write_png(&p, 64, 64, png::ColorType::Rgba, png::BitDepth::Eight, &solid_rgba(64, 64, [10, 200, 30, 255]));
        let theme = IconTheme::with_base_dirs(None, vec![]);
        let v = load_icon_sizes(&theme, p.to_str().expect("utf8"), &[16, 0, 44, 160]);
        assert_eq!(v.len(), 4);
        assert_eq!((v[0].w, v[1].w, v[2].w, v[3].w), (16, 1, 44, 160));
        assert_eq!(v[3].pixel(80, 80), [10, 200, 30, 255]);
        let v = load_icon_sizes(&theme, "nope", &[20]);
        assert_eq!(v[0].w, 20); // placeholder
    }

    #[test]
    fn box_reduce_averages_blocks() {
        let mut src = vec![0u8; 4 * 4 * 4];
        for (i, p) in src.chunks_exact_mut(4).enumerate() {
            let v = if i % 4 < 2 { 200 } else { 0 };
            p.copy_from_slice(&[v, v, v, v]);
        }
        let (o, w, h) = box_reduce(&src, 4, 4, 2);
        assert_eq!((w, h), (2, 2));
        assert_eq!(&o[0..4], &[200; 4]);
        assert_eq!(&o[4..8], &[0; 4]);
        // odd size: partial edge blocks average what exists
        let (o, w, h) = box_reduce(&solid_rgba(5, 5, [9, 9, 9, 9]), 5, 5, 2);
        assert_eq!((w, h), (3, 3));
        assert!(o.iter().all(|v| *v == 9));
    }

    #[test]
    fn placeholder_is_premultiplied_and_rounded() {
        let b = rounded_square_placeholder(64, [200, 40, 40]);
        assert_eq!(b.pixel(0, 0)[3], 0); // corner transparent
        assert_eq!(b.pixel(32, 12)[3], 255);
        assert!(b.rgba.chunks_exact(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
    }

    #[test]
    fn text_renders_white_coverage() {
        let Some(mut tr) = TextRenderer::new() else {
            return; // no system font on this machine
        };
        let (w, h) = tr.measure("Google Chrome", 13.0, false);
        assert!(w > 50.0 && w < 120.0 && h > 10.0 && h < 25.0, "{w} {h}");
        let img = tr.render_ex("Añadir al dock · 2", 26.0, true);
        let b = &img.bitmap;
        assert!(b.w > 100 && b.h > 15);
        assert!(b.rgba.chunks_exact(4).all(|p| p[0] == p[3] && p[1] == p[3] && p[2] == p[3]));
        assert!(b.rgba.chunks_exact(4).any(|p| p[3] > 200));
        // 1px transparent padding on every side
        for x in 0..b.w {
            assert_eq!(b.pixel(x, 0)[3], 0);
            assert_eq!(b.pixel(x, b.h - 1)[3], 0);
        }
        assert!(img.baseline > 0 && (img.baseline as u32) < b.h);
        // bold is wider than regular
        let (wr, _) = tr.measure("Cerrar todas las ventanas", 13.0, false);
        let (wb, _) = tr.measure("Cerrar todas las ventanas", 13.0, true);
        assert!(wb >= wr);
        // empty / weird input never panics
        assert!(tr.render("", 13.0, false).w >= 1);
        assert!(tr.render("\u{10ffff}\n\t", f32::NAN, false).w >= 1);
    }
}
