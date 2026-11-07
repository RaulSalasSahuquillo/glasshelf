//! Dock model: application catalog, launcher, Hyprland IPC client, event parser,
//! dock entry construction and pinned-app persistence.
//!
//! This module contains no rendering or Wayland code and depends only on `std`,
//! `serde` and `serde_json`.
//!
//! # Main types
//!
//! * [`Catalog`], [`AppInfo`], [`AppAction`]: freedesktop `.desktop` files scanned
//!   from the XDG application directories (plus snap and flatpak exports), with
//!   window-class matching.
//! * [`launch`]: starts an application, or one of its desktop actions, detached from
//!   the dock's session.
//! * [`Hypr`], [`Window`], [`MonitorInfo`]: client for Hyprland's command socket
//!   (socket1) and its non-blocking event socket (socket2).
//! * [`EventParser`]: incremental parser for the `name>>data` lines of the event socket.
//! * [`build_entries`], [`Entry`]: merge pinned apps and open windows into the list
//!   of dock icons.
//! * [`Pinned`]: the user's pinned apps, stored as a JSON array of desktop-file ids.
//!   On first run (or when the file is corrupt) it is seeded from [`SEED_CATEGORIES`].
//!
//! # Robustness, threading and timeouts
//!
//! Nothing here panics on malformed input (`.desktop` files, Hyprland JSON, event
//! bytes, `pinned.json`). All Hyprland requests use short timeouts so that a hung
//! compositor can never freeze the dock: the `connect` happens in a helper thread
//! that is abandoned after 300 ms (at most four stalled attempts at a time), reads and
//! writes time out after 250 ms, and a whole request is capped at 0.9 s and 8 MiB.
//! [`Hypr`] holds only socket paths, so it is cheap to clone and its blocking calls
//! can be made from any thread. [`launch`] never blocks: children are started in their
//! own session and collected by a tiny reaper thread so no zombies are left behind.

// The public API is intentionally wider than what the dock binary uses today (for
// example `Catalog::from_apps`, the `parse_*` helpers and `Hypr::close_window`), so
// the unused items would otherwise trigger `dead_code` warnings in the binary crate.
#![allow(dead_code)]

use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

// Application catalog

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AppAction {
    pub id: String,
    /// Localized name (`Name[de_DE]` / `Name[de]`, when present).
    pub name: String,
    /// The action's `Exec` line verbatim (field codes included); used by `launch`.
    pub exec: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AppInfo {
    /// Desktop file id, e.g. "kitty.desktop" or "firefox_firefox.desktop".
    pub id: String,
    pub name: String,
    /// Icon theme name or absolute path, exactly as written in the file (may be "").
    pub icon: String,
    pub exec: String,
    pub wm_class: Option<String>,
    pub path: PathBuf,
    pub actions: Vec<AppAction>,
    /// `NoDisplay=true`, or not shown on the current desktop (`OnlyShowIn`/`NotShowIn`).
    /// Kept so window classes can still be matched to the app, but `visible()` excludes it.
    pub no_display: bool,
    pub terminal: bool,
}

/// First-run seeding table: each category is a priority list of desktop ids.
///
/// [`seed_pinned`] picks the first *installed* id of each category (never more than
/// one per category), in the order of this table.
pub const SEED_CATEGORIES: &[(&str, &[&str])] = &[
    (
        "terminal",
        &[
            "kitty.desktop",
            "Alacritty.desktop",
            "org.wezfurlong.wezterm.desktop",
            "foot.desktop",
            "org.gnome.Ptyxis.desktop",
            "org.gnome.Terminal.desktop",
            "org.kde.konsole.desktop",
            "xfce4-terminal.desktop",
            "xterm.desktop",
        ],
    ),
    (
        "web browser",
        &[
            "firefox.desktop",
            "firefox_firefox.desktop",
            "org.mozilla.firefox.desktop",
            "google-chrome.desktop",
            "chromium.desktop",
            "chromium_chromium.desktop",
            "brave-browser.desktop",
            "librewolf.desktop",
            "org.mozilla.librewolf.desktop",
        ],
    ),
    (
        "file manager",
        &[
            "org.gnome.Nautilus.desktop",
            "thunar.desktop",
            "org.kde.dolphin.desktop",
            "nemo.desktop",
            "pcmanfm.desktop",
        ],
    ),
    (
        "code editor",
        &[
            "code.desktop",
            "com.visualstudio.code.desktop",
            "com.microsoft.VSCode.desktop",
            "codium.desktop",
            "org.gnome.TextEditor.desktop",
            "org.kde.kate.desktop",
            "gedit.desktop",
        ],
    ),
    (
        "music",
        &[
            "spotify.desktop",
            "spotify_spotify.desktop",
            "com.spotify.Client.desktop",
            "org.gnome.Rhythmbox3.desktop",
        ],
    ),
    (
        "settings",
        &[
            "org.gnome.Settings.desktop",
            "systemsettings.desktop",
            "org.kde.systemsettings.desktop",
        ],
    ),
];

/// Builds the first-run pinned list from [`SEED_CATEGORIES`]: for each category, the
/// first id that is present in `catalog` (at most one per category). Categories with
/// nothing installed are skipped.
pub fn seed_pinned(catalog: &Catalog) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, candidates) in SEED_CATEGORIES {
        if let Some(app) = candidates.iter().find_map(|id| catalog.get(id)) {
            if !out.contains(&app.id) {
                out.push(app.id.clone());
            }
        }
    }
    out
}

/// Context used when interpreting a .desktop file.
#[derive(Clone, Debug, Default)]
pub struct ParseCtx {
    /// Localization keys in priority order, e.g. `["de_DE", "de"]`.
    pub locales: Vec<String>,
    /// `XDG_CURRENT_DESKTOP` split on ':' (for `OnlyShowIn`/`NotShowIn`).
    pub desktops: Vec<String>,
    /// Drop apps whose `TryExec` program does not exist.
    pub check_try_exec: bool,
}

impl ParseCtx {
    pub fn from_env() -> Self {
        let desktops = env_nonempty("XDG_CURRENT_DESKTOP")
            .map(|v| v.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        ParseCtx {
            locales: env_locale_keys(),
            desktops,
            check_try_exec: true,
        }
    }
}

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// `"de_DE.UTF-8@euro"` -> `["de_DE@euro", "de_DE", "de@euro", "de"]`
pub fn locale_keys(locale: &str) -> Vec<String> {
    let l = locale.trim();
    if l.is_empty() || l == "C" || l == "POSIX" || l.starts_with("C.") {
        return Vec::new();
    }
    let (rest, modifier) = match l.split_once('@') {
        Some((a, m)) => (a, Some(m)),
        None => (l, None),
    };
    let rest = rest.split('.').next().unwrap_or(rest);
    let (lang, country) = match rest.split_once('_') {
        Some((a, c)) => (a, Some(c)),
        None => (rest, None),
    };
    if lang.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(c) = country {
        if let Some(m) = modifier {
            out.push(format!("{lang}_{c}@{m}"));
        }
        out.push(format!("{lang}_{c}"));
    }
    if let Some(m) = modifier {
        out.push(format!("{lang}@{m}"));
    }
    out.push(lang.to_string());
    out
}

/// Localization keys from the environment (LANGUAGE, LC_ALL, LC_MESSAGES, LANG).
pub fn env_locale_keys() -> Vec<String> {
    let main = env_nonempty("LC_ALL")
        .or_else(|| env_nonempty("LC_MESSAGES"))
        .or_else(|| env_nonempty("LANG"));
    let mut out: Vec<String> = Vec::new();
    let mut add = |keys: Vec<String>| {
        for k in keys {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    };
    if let Some(m) = &main {
        if !locale_keys(m).is_empty() {
            if let Some(langs) = env_nonempty("LANGUAGE") {
                for p in langs.split(':') {
                    add(locale_keys(p));
                }
            }
        }
        add(locale_keys(m));
    }
    out
}

struct RawGroup {
    name: String,
    kv: Vec<(String, String)>,
}

fn parse_groups(text: &str) -> Vec<RawGroup> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut groups: Vec<RawGroup> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.starts_with('[') {
            if let Some(end) = l.rfind(']') {
                groups.push(RawGroup {
                    name: l[1..end].to_string(),
                    kv: Vec::new(),
                });
            }
            continue;
        }
        if let Some(g) = groups.last_mut() {
            if let Some((k, v)) = l.split_once('=') {
                g.kv.push((k.trim().to_string(), v.trim_start().to_string()));
            }
        }
    }
    groups
}

fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(';') => out.push(';'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn kv_get<'a>(kv: &'a [(String, String)], key: &str) -> Option<&'a str> {
    kv.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn kv_localized(kv: &[(String, String)], key: &str, locales: &[String]) -> Option<String> {
    for loc in locales {
        let lk = format!("{key}[{loc}]");
        if let Some(v) = kv_get(kv, &lk) {
            if !v.is_empty() {
                return Some(unescape(v));
            }
        }
    }
    kv_get(kv, key).filter(|v| !v.is_empty()).map(unescape)
}

fn kv_bool(kv: &[(String, String)], key: &str) -> bool {
    matches!(kv_get(kv, key), Some("true") | Some("1"))
}

fn kv_list(kv: &[(String, String)], key: &str) -> Vec<String> {
    kv_get(kv, key)
        .map(|v| {
            v.split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug)]
pub enum Parsed {
    App(AppInfo),
    /// `Hidden=true`: the entry does not exist, and it also shadows .desktop files with
    /// the same id in later directories.
    Hidden,
    /// Invalid or inapplicable file (not an Application, no Name/Exec, missing TryExec, ...).
    Skip,
}

pub fn parse_desktop_entry(text: &str, path: &Path, id: &str, ctx: &ParseCtx) -> Parsed {
    let groups = parse_groups(text);
    let Some(main) = groups.iter().find(|g| g.name == "Desktop Entry") else {
        return Parsed::Skip;
    };
    let kv = &main.kv;
    if kv_bool(kv, "Hidden") {
        return Parsed::Hidden;
    }
    match kv_get(kv, "Type") {
        Some("Application") | None => {}
        Some(_) => return Parsed::Skip,
    }
    let Some(name) = kv_localized(kv, "Name", &ctx.locales) else {
        return Parsed::Skip;
    };
    let exec = kv_get(kv, "Exec").map(unescape).unwrap_or_default();
    if exec.is_empty() && !kv_bool(kv, "DBusActivatable") {
        return Parsed::Skip;
    }
    if ctx.check_try_exec {
        if let Some(t) = kv_get(kv, "TryExec").map(unescape) {
            if !t.is_empty() && !program_exists(&t) {
                return Parsed::Skip;
            }
        }
    }
    let mut no_display = kv_bool(kv, "NoDisplay");
    let only = kv_list(kv, "OnlyShowIn");
    let not = kv_list(kv, "NotShowIn");
    if !only.is_empty() && !ctx.desktops.iter().any(|d| only.contains(d)) {
        no_display = true;
    }
    if ctx.desktops.iter().any(|d| not.contains(d)) {
        no_display = true;
    }
    let mut actions = Vec::new();
    for aid in kv_list(kv, "Actions") {
        let gname = format!("Desktop Action {aid}");
        let Some(g) = groups.iter().find(|g| g.name == gname) else {
            continue;
        };
        let (Some(aname), Some(aexec)) = (
            kv_localized(&g.kv, "Name", &ctx.locales),
            kv_get(&g.kv, "Exec").map(unescape).filter(|e| !e.is_empty()),
        ) else {
            continue;
        };
        if actions.iter().any(|a: &AppAction| a.id == aid) {
            continue;
        }
        actions.push(AppAction {
            id: aid,
            name: aname,
            exec: aexec,
        });
    }
    Parsed::App(AppInfo {
        id: id.to_string(),
        name,
        icon: kv_get(kv, "Icon").map(unescape).unwrap_or_default(),
        exec,
        wm_class: kv_get(kv, "StartupWMClass").map(unescape).filter(|s| !s.is_empty()),
        path: path.to_path_buf(),
        actions,
        no_display,
        terminal: kv_bool(kv, "Terminal"),
    })
}

fn is_executable(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn program_exists(prog: &str) -> bool {
    if prog.contains('/') {
        return is_executable(Path::new(prog));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| is_executable(&d.join(prog)))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn user_runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let uid = fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(1000);
    PathBuf::from(format!("/run/user/{uid}"))
}

/// Application directories in priority order (user first), without duplicates.
pub fn applications_dirs() -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    let home = home_dir();
    match env_nonempty("XDG_DATA_HOME") {
        Some(d) => bases.push(PathBuf::from(d)),
        None => {
            if let Some(h) = &home {
                bases.push(h.join(".local/share"));
            }
        }
    }
    let data_dirs = env_nonempty("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    bases.extend(data_dirs.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    bases.push(PathBuf::from("/usr/local/share"));
    bases.push(PathBuf::from("/usr/share"));
    bases.push(PathBuf::from("/var/lib/snapd/desktop"));
    if let Some(h) = &home {
        bases.push(h.join(".local/share/flatpak/exports/share"));
    }
    bases.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for b in bases {
        let d: PathBuf = b.join("applications").components().collect(); // normalizes "//" and a trailing "/"
        if seen.insert(d.clone()) {
            out.push(d);
        }
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    apps: Vec<AppInfo>,
    by_id: HashMap<String, usize>,
    by_idkey: HashMap<String, usize>,
    by_wmclass: HashMap<String, usize>,
    idkeys: Vec<String>,
}

fn id_key(id: &str) -> String {
    id.strip_suffix(".desktop").unwrap_or(id).to_lowercase()
}

impl Catalog {
    /// Scans the standard directories (XDG_DATA_DIRS, snap, flatpak).
    pub fn load() -> Catalog {
        Catalog::from_dirs(&applications_dirs(), &ParseCtx::from_env())
    }

    /// Scans `dirs` in priority order: the first file with a given id wins, and
    /// `Hidden=true` also shadows that id in later directories.
    pub fn from_dirs(dirs: &[PathBuf], ctx: &ParseCtx) -> Catalog {
        let mut seen = HashSet::new();
        let mut apps = Vec::new();
        for d in dirs {
            scan_dir(d, "", 0, ctx, &mut seen, &mut apps);
        }
        Catalog::from_apps(apps)
    }

    pub fn from_apps(apps: Vec<AppInfo>) -> Catalog {
        let mut c = Catalog {
            apps,
            ..Catalog::default()
        };
        for (i, a) in c.apps.iter().enumerate() {
            c.by_id.entry(a.id.clone()).or_insert(i);
            let k = id_key(&a.id);
            c.by_idkey.entry(k.clone()).or_insert(i);
            c.idkeys.push(k);
        }
        // StartupWMClass: the first visible app wins; if only NoDisplay apps declare it,
        // the first of those.
        for pass_visible in [true, false] {
            for (i, a) in c.apps.iter().enumerate() {
                if a.no_display == pass_visible {
                    continue;
                }
                if let Some(w) = &a.wm_class {
                    c.by_wmclass.entry(w.to_lowercase()).or_insert(i);
                }
            }
        }
        c
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    pub fn all(&self) -> &[AppInfo] {
        &self.apps
    }

    pub fn get(&self, id: &str) -> Option<&AppInfo> {
        if let Some(&i) = self.by_id.get(id) {
            return self.apps.get(i);
        }
        if !id.is_empty() && !id.ends_with(".desktop") {
            return self.by_id.get(&format!("{id}.desktop")).and_then(|&i| self.apps.get(i));
        }
        None
    }

    /// Finds the app that owns a window class (case-insensitive). Matching rules, in
    /// order of precedence:
    ///
    /// 1. the exact desktop-id key (the id without `.desktop`);
    /// 2. `StartupWMClass`;
    /// 3. a snap-style part of the id (`firefox` in `firefox_firefox`);
    /// 4. the last dotted component of the id (`nautilus` in `org.gnome.Nautilus`).
    ///
    /// In the last two (loose) steps, visible apps are preferred over `NoDisplay` ones.
    pub fn match_class(&self, class: &str) -> Option<&AppInfo> {
        let c = class.to_lowercase();
        if c.is_empty() {
            return None;
        }
        if let Some(&i) = self.by_idkey.get(&c) {
            return self.apps.get(i);
        }
        if let Some(&i) = self.by_wmclass.get(&c) {
            return self.apps.get(i);
        }
        for want_visible in [true, false] {
            for (i, key) in self.idkeys.iter().enumerate() {
                let Some(app) = self.apps.get(i) else { continue };
                if app.no_display == want_visible {
                    continue;
                }
                if key.split('_').any(|p| p == c) || key.rsplit('.').next() == Some(c.as_str()) {
                    return Some(app);
                }
            }
        }
        None
    }

    /// Displayable apps, sorted by name (case-insensitive).
    pub fn visible(&self) -> Vec<&AppInfo> {
        let mut v: Vec<&AppInfo> = self.apps.iter().filter(|a| !a.no_display).collect();
        v.sort_by_cached_key(|a| (a.name.to_lowercase(), a.id.clone()));
        v
    }
}

const MAX_DESKTOP_BYTES: u64 = 1 << 20;
const MAX_SCAN_DEPTH: usize = 6;

fn scan_dir(
    dir: &Path,
    prefix: &str,
    depth: usize,
    ctx: &ParseCtx,
    seen: &mut HashSet<String>,
    out: &mut Vec<AppInfo>,
) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<(OsString, PathBuf)> = rd.filter_map(|e| e.ok()).map(|e| (e.file_name(), e.path())).collect();
    entries.sort();
    for (fname, path) in entries {
        let Some(fname) = fname.to_str() else { continue };
        let Ok(meta) = fs::metadata(&path) else { continue }; // follows symlinks; broken ones are ignored
        if meta.is_dir() {
            if depth < MAX_SCAN_DEPTH {
                scan_dir(&path, &format!("{prefix}{fname}-"), depth + 1, ctx, seen, out);
            }
            continue;
        }
        if !meta.is_file() || !fname.ends_with(".desktop") || meta.len() > MAX_DESKTOP_BYTES {
            continue;
        }
        let id = format!("{prefix}{fname}");
        if seen.contains(&id) {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        match parse_desktop_entry(&text, &path, &id, ctx) {
            Parsed::App(a) => {
                seen.insert(id);
                out.push(a);
            }
            Parsed::Hidden => {
                seen.insert(id);
            }
            Parsed::Skip => {}
        }
    }
}

// Launcher

/// Splits an `Exec` value as the Desktop Entry spec requires (double quotes, backslash escapes).
pub fn split_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quoted = false;
    let mut it = exec.chars();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                quoted = !quoted;
                have = true;
            }
            '\\' => {
                if let Some(n) = it.next() {
                    cur.push(n);
                    have = true;
                }
            }
            c if c.is_whitespace() && !quoted => {
                if have {
                    args.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        args.push(cur);
    }
    args
}

/// Expands field codes: `%f %F %u %U` (and the deprecated `%d %D %n %N %v %m`) are removed,
/// `%i` becomes `--icon <Icon>`, `%c` the app name, `%k` the .desktop path, `%%` a literal `%`.
pub fn expand_exec(exec: &str, app: &AppInfo) -> Vec<String> {
    let mut out = Vec::new();
    for tok in split_exec(exec) {
        if tok == "%i" {
            if !app.icon.is_empty() {
                out.push("--icon".to_string());
                out.push(app.icon.clone());
            }
            continue;
        }
        let mut s = String::with_capacity(tok.len());
        let mut had_code = false;
        let mut it = tok.chars();
        while let Some(c) = it.next() {
            if c != '%' {
                s.push(c);
                continue;
            }
            match it.next() {
                Some('%') => s.push('%'),
                Some('c') => s.push_str(&app.name),
                Some('k') => s.push_str(&app.path.to_string_lossy()),
                Some('i') => {
                    had_code = true;
                    if !app.icon.is_empty() {
                        s.push_str(&app.icon);
                    }
                }
                Some(_) => had_code = true,
                None => s.push('%'),
            }
        }
        if s.is_empty() && had_code {
            continue;
        }
        out.push(s);
    }
    out
}

/// Candidate commands (in order) to launch the app or one of its actions.
pub fn launch_commands(app: &AppInfo, action: Option<&str>) -> Vec<Vec<String>> {
    let gio = vec![
        "gio".to_string(),
        "launch".to_string(),
        app.path.to_string_lossy().into_owned(),
    ];
    match action {
        None => {
            let mut v = vec![gio];
            let direct = expand_exec(&app.exec, app);
            if !direct.is_empty() && !app.terminal {
                v.push(direct); // fallback when `gio` is not available
            }
            v
        }
        Some(a) => {
            let Some(act) = app.actions.iter().find(|x| x.id == a) else {
                return Vec::new();
            };
            if app.terminal {
                return vec![gio]; // no reliable way to wrap an action in a terminal: just open the app
            }
            let argv = expand_exec(&act.exec, app);
            if argv.is_empty() {
                Vec::new()
            } else {
                vec![argv]
            }
        }
    }
}

extern "C" {
    fn setsid() -> i32;
}

fn spawn_detached(argv: &[String]) -> io::Result<()> {
    let Some((prog, args)) = argv.split_first() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(h) = home_dir().filter(|h| h.is_dir()) {
        cmd.current_dir(h);
    }
    // SAFETY: only setsid(2) is called, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // Tiny thread that reaps the child so no zombie is left behind (the child is already
    // a session leader).
    let reaper = std::thread::Builder::new()
        .name("launch-reap".into())
        .stack_size(64 * 1024);
    if reaper
        .spawn(move || {
            let _ = child.wait();
        })
        .is_err()
    {
        // Without a thread we cannot wait; the child stays a zombie until the dock exits.
    }
    Ok(())
}

/// Launches the app (or one of its actions) detached from the dock's session; does not block.
/// Returns true if some command could be started.
pub fn launch(app: &AppInfo, action: Option<&str>) -> bool {
    for argv in launch_commands(app, action) {
        match spawn_detached(&argv) {
            Ok(()) => return true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                eprintln!("failed to launch {}: {e}", app.id);
                return false;
            }
        }
    }
    false
}

// Hyprland IPC

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Window {
    pub address: String,
    pub class: String,
    pub title: String,
    /// Workspace name (e.g. "3" or "special:minimized").
    pub workspace: String,
    pub focus_history: i32,
    pub pid: i32,
    pub monitor: i32,
    pub mapped: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MonitorInfo {
    pub id: i32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    /// Physical pixels.
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    pub focused: bool,
    pub active_workspace_id: i32,
    pub active_workspace_name: String,
}

impl MonitorInfo {
    /// Logical width (pixels / scale).
    pub fn logical_width(&self) -> i32 {
        (self.width as f64 / self.scale.max(0.01)).round() as i32
    }
    pub fn logical_height(&self) -> i32 {
        (self.height as f64 / self.scale.max(0.01)).round() as i32
    }
}

const IO_TIMEOUT: Duration = Duration::from_millis(250);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const REQUEST_DEADLINE: Duration = Duration::from_millis(900);
const MAX_RESPONSE: usize = 8 << 20;
static CONNECTS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// connect(2) on a Unix socket can block when the compositor's backlog is full; it is
/// done in a helper thread that is abandoned after CONNECT_TIMEOUT.
fn connect_timeout(path: &Path) -> io::Result<UnixStream> {
    if CONNECTS_IN_FLIGHT.load(Ordering::Relaxed) >= 4 {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "too many stalled connections"));
    }
    CONNECTS_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    let p = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("hypr-connect".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let r = UnixStream::connect(&p);
            CONNECTS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            let _ = tx.send(r);
        });
    if spawned.is_err() {
        CONNECTS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        return Err(io::Error::other("could not spawn the connect thread"));
    }
    match rx.recv_timeout(CONNECT_TIMEOUT) {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out")),
    }
}

fn is_valid_address(a: &str) -> bool {
    a.strip_prefix("0x")
        .is_some_and(|h| !h.is_empty() && h.len() <= 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn jstr(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn ji64(v: &Value, k: &str) -> Option<i64> {
    let x = v.get(k)?;
    x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))
}

fn ji32(v: &Value, k: &str, default: i32) -> i32 {
    ji64(v, k).and_then(|n| i32::try_from(n).ok()).unwrap_or(default)
}

fn jbool(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// Interprets the output of `j/clients`; elements without an address are ignored.
pub fn parse_clients(v: &Value) -> Vec<Window> {
    let Some(arr) = v.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|c| {
            let address = jstr(c, "address");
            if address.is_empty() {
                return None;
            }
            let ws = c.get("workspace");
            let workspace = ws
                .and_then(|w| w.get("name").and_then(Value::as_str).map(str::to_string))
                .or_else(|| ws.and_then(|w| ji64(w, "id")).map(|i| i.to_string()))
                .unwrap_or_default();
            Some(Window {
                address,
                class: jstr(c, "class"),
                title: jstr(c, "title"),
                workspace,
                focus_history: ji32(c, "focusHistoryID", -1),
                pid: ji32(c, "pid", 0),
                monitor: ji32(c, "monitor", -1),
                mapped: jbool(c, "mapped"),
            })
        })
        .collect()
}

/// Interprets the output of `j/monitors`.
pub fn parse_monitors(v: &Value) -> Vec<MonitorInfo> {
    let Some(arr) = v.as_array() else { return Vec::new() };
    arr.iter()
        .filter(|m| !jbool(m, "disabled"))
        .map(|m| {
            let aw = m.get("activeWorkspace");
            MonitorInfo {
                id: ji32(m, "id", -1),
                name: jstr(m, "name"),
                x: ji32(m, "x", 0),
                y: ji32(m, "y", 0),
                width: ji32(m, "width", 0),
                height: ji32(m, "height", 0),
                scale: m
                    .get("scale")
                    .and_then(Value::as_f64)
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                focused: jbool(m, "focused"),
                active_workspace_id: aw.map_or(0, |w| ji32(w, "id", 0)),
                active_workspace_name: aw.map(|w| jstr(w, "name")).unwrap_or_default(),
            }
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct Hypr {
    cmd_sock: PathBuf,
    evt_sock: PathBuf,
}

impl Hypr {
    /// Locates the instance via HYPRLAND_INSTANCE_SIGNATURE (or, failing that, the most
    /// recent one).
    pub fn new() -> Option<Hypr> {
        let rt = user_runtime_dir();
        if let Some(sig) = env_nonempty("HYPRLAND_INSTANCE_SIGNATURE") {
            if let Some(h) = Hypr::with_dir(rt.join("hypr").join(sig)) {
                return Some(h);
            }
        }
        let mut best: Option<(SystemTime, PathBuf)> = None;
        for e in fs::read_dir(rt.join("hypr")).ok()?.filter_map(|e| e.ok()) {
            let sock = e.path().join(".socket.sock");
            if let Ok(m) = fs::metadata(&sock) {
                let t = m.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
                    best = Some((t, e.path()));
                }
            }
        }
        best.and_then(|(_, d)| Hypr::with_dir(d))
    }

    /// Instance in a specific directory (`$XDG_RUNTIME_DIR/hypr/<signature>`).
    pub fn with_dir(dir: PathBuf) -> Option<Hypr> {
        let cmd_sock = dir.join(".socket.sock");
        if fs::metadata(&cmd_sock).is_err() {
            return None;
        }
        Some(Hypr {
            cmd_sock,
            evt_sock: dir.join(".socket2.sock"),
        })
    }

    /// Raw request on socket1; None on failure or timeout.
    pub fn request(&self, req: &str) -> Option<String> {
        let started = Instant::now();
        let mut s = connect_timeout(&self.cmd_sock).ok()?;
        s.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
        s.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
        s.write_all(req.as_bytes()).ok()?;
        let mut buf = Vec::with_capacity(16 * 1024);
        let mut chunk = [0u8; 16 * 1024];
        loop {
            if started.elapsed() > REQUEST_DEADLINE || buf.len() > MAX_RESPONSE {
                return None;
            }
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        Some(String::from_utf8_lossy(&buf).into_owned())
    }

    fn json_request(&self, req: &str) -> Option<Value> {
        serde_json::from_str(self.request(req)?.trim()).ok()
    }

    pub fn clients(&self) -> Vec<Window> {
        self.json_request("j/clients")
            .map(|v| parse_clients(&v))
            .unwrap_or_default()
    }

    pub fn active_window_address(&self) -> Option<String> {
        let v = self.json_request("j/activewindow")?;
        let a = jstr(&v, "address");
        if a.is_empty() || a == "0x0" {
            None
        } else {
            Some(a)
        }
    }

    pub fn monitors(&self) -> Vec<MonitorInfo> {
        self.json_request("j/monitors")
            .map(|v| parse_monitors(&v))
            .unwrap_or_default()
    }

    /// Number of windows on a workspace. None if Hyprland could not be queried.
    pub fn workspace_windows(&self, workspace_id: i32) -> Option<u32> {
        let v = self.json_request("j/workspaces")?;
        let arr = v.as_array()?;
        Some(
            arr.iter()
                .find(|w| ji32(w, "id", i32::MIN) == workspace_id)
                .map_or(0, |w| ji64(w, "windows").unwrap_or(0).max(0) as u32),
        )
    }

    /// Like `workspace_windows`, but returns 1 ("not empty") when Hyprland cannot be
    /// queried, so a workspace is never mistaken for an empty one.
    pub fn workspace_window_count(&self, workspace_id: i32) -> u32 {
        self.workspace_windows(workspace_id).unwrap_or(1)
    }

    /// Runs `dispatch <args>`; true if Hyprland answers "ok".
    pub fn dispatch(&self, args: &str) -> bool {
        if args.contains('\n') || args.trim().is_empty() {
            return false;
        }
        self.request(&format!("dispatch {args}"))
            .is_some_and(|r| r.trim() == "ok")
    }

    pub fn focus_window(&self, address: &str) -> bool {
        is_valid_address(address) && self.dispatch(&format!("focuswindow address:{address}"))
    }

    pub fn close_window(&self, address: &str) -> bool {
        is_valid_address(address) && self.dispatch(&format!("closewindow address:{address}"))
    }

    pub fn minimize_window(&self, address: &str) -> bool {
        is_valid_address(address)
            && self.dispatch(&format!("movetoworkspacesilent special:minimized,address:{address}"))
    }

    /// Non-blocking event socket (socket2), ready for epoll/calloop.
    pub fn event_stream(&self) -> io::Result<UnixStream> {
        let s = connect_timeout(&self.evt_sock)?;
        s.set_nonblocking(true)?;
        Ok(s)
    }
}

// Events

pub const RELEVANT_EVENTS: [&str; 14] = [
    "openwindow",
    "closewindow",
    "activewindow",
    "activewindowv2",
    "workspace",
    "workspacev2",
    "movewindow",
    "movewindowv2",
    "windowtitle",
    "fullscreen",
    "urgent",
    "monitoradded",
    "monitorremoved",
    "configreloaded",
];

const MAX_PARTIAL_LINE: usize = 256 * 1024;

#[derive(Debug, Default)]
pub struct EventParser {
    buf: Vec<u8>,
}

impl EventParser {
    pub fn new() -> Self {
        EventParser { buf: Vec::new() }
    }

    /// Feeds bytes read from socket2; returns the names of the complete events
    /// ("name>>data\n"). A trailing partial line is kept for the next `push`.
    pub fn push(&mut self, data: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(data);
        let mut names = Vec::new();
        let mut start = 0;
        while let Some(off) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let line = &self.buf[start..start + off];
            start += off + 1;
            if let Some(i) = line.windows(2).position(|w| w == b">>") {
                let name = String::from_utf8_lossy(&line[..i]);
                let name = name.trim();
                if !name.is_empty() {
                    names.push(name.to_string());
                }
            }
        }
        self.buf.drain(..start);
        if self.buf.len() > MAX_PARTIAL_LINE {
            self.buf.clear();
        }
        names
    }

    pub fn is_relevant(name: &str) -> bool {
        RELEVANT_EVENTS.contains(&name)
    }
}

// Dock entries

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Entry {
    /// .desktop id, or `"class:<Class>"` for unknown window classes.
    pub key: String,
    pub app: Option<AppInfo>,
    pub name: String,
    pub icon: String,
    /// Window addresses, most recently focused first.
    pub windows: Vec<String>,
    pub pinned: bool,
    pub active: bool,
}

fn app_icon(app: &AppInfo) -> String {
    if app.icon.is_empty() {
        "application-x-executable".to_string()
    } else {
        app.icon.clone()
    }
}

fn app_entry(app: &AppInfo) -> Entry {
    Entry {
        key: app.id.clone(),
        app: Some(app.clone()),
        name: app.name.clone(),
        icon: app_icon(app),
        ..Entry::default()
    }
}

fn focus_key(h: i32) -> i32 {
    if h < 0 {
        i32::MAX
    } else {
        h
    }
}

/// Pinned apps first (in pin order, even if not running); then running unpinned apps
/// in focus order. Windows without a class, or not mapped, are ignored.
pub fn build_entries(
    catalog: &Catalog,
    pinned: &[String],
    windows: &[Window],
    active_address: Option<&str>,
) -> Vec<Entry> {
    let mut ws: Vec<&Window> = windows.iter().filter(|w| w.mapped && !w.class.is_empty()).collect();
    ws.sort_by_key(|w| focus_key(w.focus_history)); // stable

    let mut running: Vec<Option<Entry>> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for w in ws {
        let app = catalog.match_class(&w.class);
        let key = app.map_or_else(|| format!("class:{}", w.class), |a| a.id.clone());
        let i = match index.get(&key) {
            Some(&i) => i,
            None => {
                let e = match app {
                    Some(a) => app_entry(a),
                    None => Entry {
                        key: key.clone(),
                        app: None,
                        name: w.class.clone(),
                        icon: w.class.to_lowercase(),
                        ..Entry::default()
                    },
                };
                running.push(Some(e));
                index.insert(key, running.len() - 1);
                running.len() - 1
            }
        };
        if let Some(e) = running[i].as_mut() {
            e.windows.push(w.address.clone());
            if active_address.is_some_and(|a| a.eq_ignore_ascii_case(&w.address)) {
                e.active = true;
            }
        }
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for pid in pinned {
        let Some(app) = catalog.get(pid) else { continue };
        if !seen.insert(app.id.clone()) {
            continue;
        }
        let mut e = index
            .get(&app.id)
            .and_then(|&i| running[i].take())
            .unwrap_or_else(|| app_entry(app));
        e.pinned = true;
        out.push(e);
    }
    out.extend(running.into_iter().flatten());
    out
}

// Pinned apps (pinned.json)

type Stamp = (SystemTime, u64);

fn file_stamp(path: &Path) -> Option<Stamp> {
    let m = fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// Reads a JSON array of ids; non-string elements and duplicates are dropped.
fn read_pinned_file(path: &Path) -> Result<Vec<String>, io::Error> {
    let text = fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let Some(arr) = v.as_array() else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "pinned.json is not a list"));
    };
    let mut seen = HashSet::new();
    Ok(arr
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| seen.insert(s.to_string()))
        .map(str::to_string)
        .collect())
}

#[derive(Debug)]
pub struct Pinned {
    path: PathBuf,
    list: Vec<String>,
    stamp: Option<Stamp>,
    /// The file existed but was corrupt: a .bak copy is kept when it is overwritten.
    corrupt: bool,
}

impl Pinned {
    /// Loads `path`. If the file is missing or corrupt, the list is the first-run seed
    /// (see [`seed_pinned`]). Nothing is written: call `save()` to persist the seed.
    pub fn load(path: &Path, catalog: &Catalog) -> Pinned {
        let seed = || seed_pinned(catalog);
        match read_pinned_file(path) {
            Ok(list) => Pinned {
                path: path.to_path_buf(),
                list,
                stamp: file_stamp(path),
                corrupt: false,
            },
            Err(e) => Pinned {
                path: path.to_path_buf(),
                list: seed(),
                stamp: file_stamp(path),
                corrupt: e.kind() == io::ErrorKind::InvalidData,
            },
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> &[String] {
        &self.list
    }

    pub fn contains(&self, id: &str) -> bool {
        self.list.iter().any(|x| x == id)
    }

    /// Appends `id`; false if it is already pinned (or empty).
    pub fn pin(&mut self, id: &str) -> bool {
        if id.is_empty() || self.contains(id) {
            return false;
        }
        self.list.push(id.to_string());
        true
    }

    pub fn unpin(&mut self, id: &str) -> bool {
        match self.list.iter().position(|x| x == id) {
            Some(i) => {
                self.list.remove(i);
                true
            }
            None => false,
        }
    }

    /// Moves `id` by `delta` positions (clamped at the ends); false if nothing changes.
    pub fn move_by(&mut self, id: &str, delta: i32) -> bool {
        let Some(i) = self.list.iter().position(|x| x == id) else {
            return false;
        };
        let last = self.list.len() as i64 - 1;
        let j = (i as i64 + delta as i64).clamp(0, last) as usize;
        if j == i {
            return false;
        }
        let item = self.list.remove(i);
        self.list.insert(j, item);
        true
    }

    /// Atomic write (temp file + rename) as a pretty-printed JSON array; remembers the mtime.
    pub fn save(&mut self) -> io::Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(&self.list).map_err(io::Error::other)?;
        let mut tmp_name = self
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "pinned.json".into());
        tmp_name.push(format!(".tmp{}", std::process::id()));
        let tmp = self.path.with_file_name(tmp_name);
        let res = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.sync_all()?;
            if self.corrupt && self.path.exists() {
                let mut bak = self.path.as_os_str().to_os_string();
                bak.push(".bak");
                let _ = fs::copy(&self.path, PathBuf::from(bak));
            }
            fs::rename(&tmp, &self.path)
        })();
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        } else {
            self.corrupt = false;
            self.stamp = file_stamp(&self.path);
        }
        res
    }

    /// Reloads if another instance changed the file (mtime/size) and the content differs.
    pub fn reload_if_changed(&mut self) -> bool {
        let now = file_stamp(&self.path);
        if now == self.stamp {
            return false;
        }
        self.stamp = now;
        if now.is_none() {
            return false;
        }
        match read_pinned_file(&self.path) {
            Ok(list) if list != self.list => {
                self.list = list;
                self.corrupt = false;
                true
            }
            _ => false,
        }
    }
}

// Unit tests with synthetic data

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static N: AtomicU32 = AtomicU32::new(0);

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "model-test-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Parse context with a German locale: fixtures use tagged values such as
    /// "Firefox Browser [de_DE]" so the tests show which localized key was picked.
    fn ctx_de() -> ParseCtx {
        ParseCtx {
            locales: locale_keys("de_DE.UTF-8"),
            desktops: vec!["Hyprland".into()],
            check_try_exec: false,
        }
    }

    const FIREFOX: &str = "\
[Desktop Entry]
Version=1.0
Name=Firefox Web Browser
Name[de]=Firefox Browser [de]
Name[de_DE]=Firefox Browser [de_DE]
Name[fr]=Firefox Browser [fr]
Exec=/snap/bin/firefox %u
Icon=/snap/firefox/current/default256.png
Type=Application
Actions=new-window;new-private-window;broken;

[Desktop Action new-window]
Exec=/snap/bin/firefox --new-window %u
Name=New Window
Name[de]=New Window [de]

[Desktop Action new-private-window]
Exec=/snap/bin/firefox --private-window %u
Name=New Private Window
Name[de]=New Private Window [de]

[Desktop Action orphan]
Name=Not listed
Exec=x
";

    fn app(id: &str, name: &str, wm: Option<&str>, nd: bool) -> AppInfo {
        AppInfo {
            id: id.into(),
            name: name.into(),
            icon: format!("{id}-icon"),
            exec: "true".into(),
            wm_class: wm.map(str::to_string),
            path: PathBuf::from(format!("/x/{id}")),
            no_display: nd,
            ..AppInfo::default()
        }
    }

    fn win(addr: &str, class: &str, fh: i32) -> Window {
        Window {
            address: addr.into(),
            class: class.into(),
            title: "t".into(),
            workspace: "1".into(),
            focus_history: fh,
            pid: 1,
            monitor: 0,
            mapped: true,
        }
    }

    #[test]
    fn locale_key_expansion() {
        assert_eq!(locale_keys("de_DE.UTF-8"), vec!["de_DE", "de"]);
        assert_eq!(
            locale_keys("sr_RS@latin"),
            vec!["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
        );
        assert_eq!(locale_keys("fr"), vec!["fr"]);
        assert!(locale_keys("C").is_empty());
        assert!(locale_keys("POSIX").is_empty());
        assert!(locale_keys("").is_empty());
    }

    #[test]
    fn desktop_localized_names_and_actions() {
        let Parsed::App(a) = parse_desktop_entry(
            FIREFOX,
            Path::new("/v/firefox_firefox.desktop"),
            "firefox_firefox.desktop",
            &ctx_de(),
        ) else {
            panic!("failed to parse")
        };
        assert_eq!(a.name, "Firefox Browser [de_DE]"); // de_DE wins over de
        assert_eq!(a.icon, "/snap/firefox/current/default256.png");
        assert_eq!(a.actions.len(), 2); // "broken" has no group and "orphan" is not listed: both are ignored
        assert_eq!(a.actions[0].id, "new-window");
        assert_eq!(a.actions[0].name, "New Window [de]");
        assert_eq!(a.actions[1].name, "New Private Window [de]");
        assert!(!a.no_display);
        // language-only fallback ("de")
        let ctx = ParseCtx {
            locales: locale_keys("de_AT"),
            ..ctx_de()
        };
        let Parsed::App(b) = parse_desktop_entry(FIREFOX, Path::new("/p"), "f.desktop", &ctx) else {
            panic!()
        };
        assert_eq!(b.name, "Firefox Browser [de]");
        // no locale
        let ctx = ParseCtx {
            locales: vec![],
            ..ctx_de()
        };
        let Parsed::App(c) = parse_desktop_entry(FIREFOX, Path::new("/p"), "f.desktop", &ctx) else {
            panic!()
        };
        assert_eq!(c.name, "Firefox Web Browser");
        assert_eq!(c.actions[0].name, "New Window");
    }

    #[test]
    fn desktop_flags_and_garbage() {
        let ctx = ctx_de();
        let p = Path::new("/p");
        let nd = "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\nStartupWMClass=Foo\n";
        match parse_desktop_entry(nd, p, "x.desktop", &ctx) {
            Parsed::App(a) => {
                assert!(a.no_display);
                assert_eq!(a.wm_class.as_deref(), Some("Foo"));
            }
            o => panic!("{o:?}"),
        }
        let hid = "[Desktop Entry]\nType=Application\nName=X\nExec=x\nHidden=true\n";
        assert!(matches!(parse_desktop_entry(hid, p, "x.desktop", &ctx), Parsed::Hidden));
        let dir = "[Desktop Entry]\nType=Directory\nName=X\n";
        assert!(matches!(parse_desktop_entry(dir, p, "x.desktop", &ctx), Parsed::Skip));
        for junk in [
            "",
            "\u{0}\u{1}garbage",
            "[Desktop Entry",
            "Name=X\nExec=y",
            "[Desktop Entry]\nName=\nExec=y",
            "[Desktop Entry]\nName=X",
        ] {
            assert!(
                matches!(parse_desktop_entry(junk, p, "j.desktop", &ctx), Parsed::Skip),
                "{junk:?}"
            );
        }
        let only = "[Desktop Entry]\nName=X\nExec=x\nOnlyShowIn=GNOME;KDE;\n";
        match parse_desktop_entry(only, p, "x.desktop", &ctx) {
            Parsed::App(a) => assert!(a.no_display),
            o => panic!("{o:?}"),
        }
        let not = "[Desktop Entry]\nName=X\nExec=x\nNotShowIn=Hyprland;\n";
        match parse_desktop_entry(not, p, "x.desktop", &ctx) {
            Parsed::App(a) => assert!(a.no_display),
            o => panic!("{o:?}"),
        }
        let crlf = "\u{feff}[Desktop Entry]\r\nName = Win\r\nExec=w\r\n";
        match parse_desktop_entry(crlf, p, "w.desktop", &ctx) {
            Parsed::App(a) => assert_eq!(a.name, "Win"),
            o => panic!("{o:?}"),
        }
        let esc = "[Desktop Entry]\nName=A\\sB\nExec=x\n";
        match parse_desktop_entry(esc, p, "e.desktop", &ctx) {
            Parsed::App(a) => assert_eq!(a.name, "A B"),
            o => panic!("{o:?}"),
        }
        let te = ParseCtx {
            check_try_exec: true,
            ..ctx
        };
        let bad = "[Desktop Entry]\nName=X\nExec=x\nTryExec=/definitely/not/here\n";
        assert!(matches!(parse_desktop_entry(bad, p, "t.desktop", &te), Parsed::Skip));
        let good = "[Desktop Entry]\nName=X\nExec=x\nTryExec=sh\n";
        assert!(matches!(parse_desktop_entry(good, p, "t.desktop", &te), Parsed::App(_)));
    }

    #[test]
    fn catalog_priority_hidden_and_matching() {
        let user = tmpdir("user");
        let sys = tmpdir("sys");
        let w = |d: &Path, f: &str, body: &str| {
            if let Some(p) = Path::new(f).parent() {
                fs::create_dir_all(d.join(p)).unwrap();
            }
            fs::write(d.join(f), body).unwrap();
        };
        w(&user, "kitty.desktop", "[Desktop Entry]\nName=Kitty user\nExec=kitty\n");
        w(
            &sys,
            "kitty.desktop",
            "[Desktop Entry]\nName=Kitty system\nExec=kitty\n",
        );
        w(
            &user,
            "gone.desktop",
            "[Desktop Entry]\nName=Gone\nExec=g\nHidden=true\n",
        );
        w(&sys, "gone.desktop", "[Desktop Entry]\nName=Gone system\nExec=g\n");
        w(&sys, "firefox_firefox.desktop", FIREFOX);
        w(
            &sys,
            "org.gnome.Nautilus.desktop",
            "[Desktop Entry]\nName=Files\nExec=nautilus\n",
        );
        w(
            &sys,
            "code-url-handler.desktop",
            "[Desktop Entry]\nName=Code URL\nExec=code\nNoDisplay=true\nStartupWMClass=Code\n",
        );
        w(
            &sys,
            "com.microsoft.VSCode.desktop",
            "[Desktop Entry]\nName=zeta Visual Studio Code\nExec=code\nStartupWMClass=Code\n",
        );
        w(&sys, "sub/inner.desktop", "[Desktop Entry]\nName=Inner\nExec=i\n");
        w(&sys, "readme.txt", "not a desktop file");
        w(&sys, "broken.desktop", "\u{0}\u{0}");
        w(&sys, "Apple.desktop", "[Desktop Entry]\nName=apple\nExec=a\n");
        let cat = Catalog::from_dirs(&[user.clone(), sys.clone()], &ctx_de());
        assert_eq!(cat.get("kitty.desktop").unwrap().name, "Kitty user");
        assert!(
            cat.get("gone.desktop").is_none(),
            "Hidden=true in the user dir shadows the system one"
        );
        assert!(cat.get("sub-inner.desktop").is_some());
        assert!(cat.get("readme.txt").is_none());
        assert_eq!(cat.get("kitty").unwrap().id, "kitty.desktop");
        // match_class rules
        assert_eq!(cat.match_class("kitty").unwrap().id, "kitty.desktop");
        assert_eq!(cat.match_class("KITTY").unwrap().id, "kitty.desktop");
        assert_eq!(
            cat.match_class("firefox_firefox").unwrap().id,
            "firefox_firefox.desktop"
        );
        assert_eq!(cat.match_class("firefox").unwrap().id, "firefox_firefox.desktop"); // snap-style part of the id
        assert_eq!(
            cat.match_class("org.gnome.Nautilus").unwrap().id,
            "org.gnome.Nautilus.desktop"
        );
        assert_eq!(cat.match_class("nautilus").unwrap().id, "org.gnome.Nautilus.desktop"); // last dotted component
        assert_eq!(
            cat.match_class("Code").unwrap().id,
            "com.microsoft.VSCode.desktop",
            "the visible app wins by StartupWMClass"
        );
        assert!(cat.match_class("nonexistent").is_none());
        assert!(cat.match_class("").is_none());
        // visible(): sorted case-insensitively, without NoDisplay apps
        let vis: Vec<&str> = cat.visible().iter().map(|a| a.name.as_str()).collect();
        assert!(!vis.contains(&"Code URL"));
        assert_eq!(
            vis,
            vec![
                "apple",
                "Files",
                "Firefox Browser [de_DE]",
                "Inner",
                "Kitty user",
                "zeta Visual Studio Code"
            ]
        );
        let _ = fs::remove_dir_all(&user);
        let _ = fs::remove_dir_all(&sys);
    }

    #[test]
    fn exec_parsing_and_launch_commands() {
        assert_eq!(
            split_exec(r#"env A=1 "my prog" --x "a b" 'c'"#),
            vec!["env", "A=1", "my prog", "--x", "a b", "'c'"]
        );
        assert_eq!(split_exec(r#"sh -c "echo \"hi\"""#), vec!["sh", "-c", "echo \"hi\""]);
        assert_eq!(split_exec(""), Vec::<String>::new());
        assert_eq!(split_exec(r#"a "" b"#), vec!["a", "", "b"]);
        let mut a = app("t.desktop", "Term", None, false);
        a.icon = "ico".into();
        assert_eq!(
            expand_exec("kitty %U %f --class=%c %i %k 100%%", &a),
            vec!["kitty", "--class=Term", "--icon", "ico", "/x/t.desktop", "100%"]
        );
        assert_eq!(expand_exec("foo --file=%f %F", &a), vec!["foo", "--file="]);
        a.exec = "kitty %U".into();
        a.actions = vec![AppAction {
            id: "nw".into(),
            name: "New".into(),
            exec: "kitty --single-instance %U".into(),
        }];
        assert_eq!(launch_commands(&a, None)[0], vec!["gio", "launch", "/x/t.desktop"]);
        assert_eq!(launch_commands(&a, None)[1], vec!["kitty"]);
        assert_eq!(
            launch_commands(&a, Some("nw")),
            vec![vec!["kitty", "--single-instance"]]
        );
        assert!(launch_commands(&a, Some("nope")).is_empty());
        assert!(!launch(&a, Some("nope")));
    }

    #[test]
    fn event_parser_partial_lines() {
        let mut p = EventParser::new();
        assert!(p.push(b"openwin").is_empty());
        assert_eq!(
            p.push(b"dow>>abc,1,kitty,t\nworkspace>>2\nfocus"),
            vec!["openwindow", "workspace"]
        );
        assert_eq!(p.push(b"edmon>>DP-1,1\n"), vec!["focusedmon"]);
        assert_eq!(p.push(b"configreloaded>>\n"), vec!["configreloaded"]);
        // a line without ">>" (continuation of a title containing a newline) is ignored
        assert_eq!(
            p.push(b"windowtitle>>a\ncontinuation\nurgent>>1\n"),
            vec!["windowtitle", "urgent"]
        );
        // invalid UTF-8 bytes
        assert_eq!(p.push(b"windowtitle>>\xff\xfe\n"), vec!["windowtitle"]);
        // a huge line without a newline must not grow without bound
        for _ in 0..40 {
            p.push(&[b'a'; 16 * 1024]);
        }
        assert!(p.buf.len() <= MAX_PARTIAL_LINE);
        assert_eq!(p.push(b"\nclosewindow>>x\n"), vec!["closewindow"]);
        assert!(EventParser::is_relevant("activewindowv2"));
        assert!(EventParser::is_relevant("monitoradded"));
        assert!(EventParser::is_relevant("configreloaded"));
        assert!(!EventParser::is_relevant("focusedmon"));
        assert!(!EventParser::is_relevant("submap"));
    }

    #[test]
    fn build_entries_semantics() {
        let cat = Catalog::from_apps(vec![
            app("kitty.desktop", "Kitty", None, false),
            app("firefox_firefox.desktop", "Firefox", None, false),
            app("code.desktop", "Code", Some("Code"), false),
            app("spotify_spotify.desktop", "Spotify", None, false),
        ]);
        let pinned: Vec<String> = [
            "kitty.desktop",
            "missing.desktop",
            "spotify_spotify.desktop",
            "kitty.desktop",
            "code.desktop",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut hidden = win("0xh", "kitty", 0);
        hidden.mapped = false;
        let wins = vec![
            win("0x1", "firefox_firefox", 2),
            win("0x2", "kitty", 1),
            win("0x3", "Weird.App", 0),
            win("0x4", "kitty", 3),
            win("0x5", "", 4),
            hidden,
            win("0x6", "firefox_firefox", 5),
            win("0x7", "Code", -1), // no focus history: sorted last
        ];
        let es = build_entries(&cat, &pinned, &wins, Some("0x4"));
        let keys: Vec<&str> = es.iter().map(|e| e.key.as_str()).collect();
        // pinned apps in pin order (kitty, spotify even though it is not running, code),
        // then Weird.App (focus 0), then firefox
        assert_eq!(
            keys,
            vec![
                "kitty.desktop",
                "spotify_spotify.desktop",
                "code.desktop",
                "class:Weird.App",
                "firefox_firefox.desktop"
            ]
        );
        assert_eq!(es[0].windows, vec!["0x2", "0x4"]);
        assert!(es[0].pinned && es[0].active);
        assert!(es[1].pinned && es[1].windows.is_empty() && !es[1].active);
        assert_eq!(es[2].windows, vec!["0x7"]);
        assert_eq!(es[3].icon, "weird.app");
        assert_eq!(es[3].name, "Weird.App");
        assert!(es[3].app.is_none() && !es[3].pinned);
        assert_eq!(es[4].windows, vec!["0x1", "0x6"]);
        assert!(!es[4].pinned && !es[4].active);
        // no windows and no pins
        assert!(build_entries(&cat, &[], &[], None).is_empty());
        let only_pins = build_entries(&cat, &pinned, &[], None);
        assert_eq!(only_pins.len(), 3);
        // empty icon -> generic icon
        let mut noicon = app("n.desktop", "N", None, false);
        noicon.icon.clear();
        let cat2 = Catalog::from_apps(vec![noicon]);
        assert_eq!(
            build_entries(&cat2, &["n.desktop".to_string()], &[], None)[0].icon,
            "application-x-executable"
        );
    }

    #[test]
    fn pinned_roundtrip_and_reload() {
        let dir = tmpdir("pinned");
        let path = dir.join("sub").join("pinned.json");
        let cat = Catalog::from_apps(vec![
            app("firefox_firefox.desktop", "F", None, false),
            app("kitty.desktop", "K", None, false),
        ]);
        let mut p = Pinned::load(&path, &cat);
        assert_eq!(p.list(), &["kitty.desktop", "firefox_firefox.desktop"]); // seed: first installed id per category
        assert!(!path.exists(), "load must not write");
        assert!(p.pin("a.desktop"));
        assert!(!p.pin("a.desktop"));
        assert!(p.pin("b.desktop"));
        assert!(p.move_by("b.desktop", -2));
        assert_eq!(
            p.list(),
            &["kitty.desktop", "b.desktop", "firefox_firefox.desktop", "a.desktop"]
        );
        assert!(!p.move_by("kitty.desktop", -1)); // already at the start
        assert!(p.move_by("kitty.desktop", 99)); // clamps at the end
        assert_eq!(p.list().last().unwrap(), "kitty.desktop");
        assert!(!p.move_by("zzz", 1));
        assert!(p.unpin("a.desktop"));
        assert!(!p.unpin("a.desktop"));
        p.save().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("[\n  \""), "{text}");
        let again = Pinned::load(&path, &cat);
        assert_eq!(again.list(), p.list());
        // our own write does not count as an external change
        assert!(!p.reload_if_changed());
        // external change with different content
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&path, r#"["x.desktop", 5, null, "y.desktop", "x.desktop"]"#).unwrap();
        assert!(p.reload_if_changed());
        assert_eq!(p.list(), &["x.desktop", "y.desktop"]);
        assert!(!p.reload_if_changed());
        // rewrite with the same content: no reload
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&path, "[ \"x.desktop\",\n \"y.desktop\" ]").unwrap();
        assert!(!p.reload_if_changed());
        // corrupt file: the in-memory list is kept
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&path, "{not json").unwrap();
        assert!(!p.reload_if_changed());
        assert_eq!(p.list(), &["x.desktop", "y.desktop"]);
        // load with a corrupt file: seed, and saving leaves a .bak copy
        let mut c = Pinned::load(&path, &cat);
        assert_eq!(c.list().len(), 2);
        c.save().unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("sub").join("pinned.json.bak")).unwrap(),
            "{not json"
        );
        // external deletion: the list is not emptied
        fs::remove_file(&path).unwrap();
        assert!(!c.reload_if_changed());
        assert_eq!(c.list().len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_table_is_well_formed() {
        let categories: Vec<&str> = SEED_CATEGORIES.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            categories,
            [
                "terminal",
                "web browser",
                "file manager",
                "code editor",
                "music",
                "settings"
            ]
        );
        let mut all = HashSet::new();
        for (category, ids) in SEED_CATEGORIES {
            assert!(!ids.is_empty(), "category {category:?} has no candidates");
            for id in *ids {
                assert!(id.ends_with(".desktop"), "{id}");
                assert!(all.insert(*id), "{id} is listed twice");
            }
        }
    }

    #[test]
    fn seed_picks_first_installed_id_per_category() {
        let ids = [
            // terminal: kitty has priority over foot and xterm, whatever the catalog order
            "xterm.desktop",
            "foot.desktop",
            "kitty.desktop",
            // web browser: google-chrome ranks above chromium
            "chromium.desktop",
            "google-chrome.desktop",
            // file manager: thunar ranks above nemo
            "nemo.desktop",
            "thunar.desktop",
            // code editor: nothing installed from the table
            // music: only the last candidate of the category
            "org.gnome.Rhythmbox3.desktop",
            // settings: the GNOME entry ranks above the KDE ones
            "org.kde.systemsettings.desktop",
            "systemsettings.desktop",
            "org.gnome.Settings.desktop",
            // not in the table: never seeded
            "unrelated.desktop",
        ];
        let cat = Catalog::from_apps(ids.iter().map(|id| app(id, id, None, false)).collect());
        assert_eq!(
            seed_pinned(&cat),
            vec![
                "kitty.desktop",
                "google-chrome.desktop",
                "thunar.desktop",
                "org.gnome.Rhythmbox3.desktop",
                "org.gnome.Settings.desktop",
            ]
        );
        // nothing from the table is installed
        let none = Catalog::from_apps(vec![app("unrelated.desktop", "U", None, false)]);
        assert!(seed_pinned(&none).is_empty());
        assert!(seed_pinned(&Catalog::default()).is_empty());
    }

    #[test]
    fn pinned_load_seeds_only_without_a_valid_file() {
        let dir = tmpdir("seed");
        let cat = Catalog::from_apps(vec![
            app("kitty.desktop", "K", None, false),
            app("foot.desktop", "F", None, false),
        ]);
        // missing file: seed (one terminal only)
        let missing = Pinned::load(&dir.join("missing.json"), &cat);
        assert_eq!(missing.list(), &["kitty.desktop"]);
        // valid file: used as is, even when empty
        let valid = dir.join("valid.json");
        fs::write(&valid, "[]").unwrap();
        assert!(Pinned::load(&valid, &cat).list().is_empty());
        // corrupt file: seed again
        let corrupt = dir.join("corrupt.json");
        fs::write(&corrupt, "not json").unwrap();
        assert_eq!(Pinned::load(&corrupt, &cat).list(), &["kitty.desktop"]);
        // JSON that is not a list is treated as corrupt too
        fs::write(&corrupt, r#"{"a": 1}"#).unwrap();
        assert_eq!(Pinned::load(&corrupt, &cat).list(), &["kitty.desktop"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Fake Hyprland servers on Unix sockets in a private temp dir.
    #[test]
    fn hypr_ipc_against_fake_sockets() {
        use std::os::unix::net::UnixListener;
        let dir = tmpdir("ipc");
        let good_dir = dir.join("good");
        let bad_dir = dir.join("bad");
        // `sun_path` holds at most 108 bytes: skip on an unusually long temp dir path.
        if good_dir.join(".socket2.sock").as_os_str().len() > 100 {
            eprintln!("skipping: temp dir path is too long for a Unix socket");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        fs::create_dir_all(&good_dir).unwrap();
        fs::create_dir_all(&bad_dir).unwrap();
        // well-behaved server: answers according to the request
        let good = UnixListener::bind(good_dir.join(".socket.sock")).unwrap();
        std::thread::spawn(move || {
            for conn in good.incoming() {
                let Ok(mut c) = conn else { break };
                let mut buf = [0u8; 256];
                let n = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = match req.as_str() {
                    "j/clients" => {
                        r#"[{"address":"0xa","class":"kitty","mapped":true,"workspace":{"id":1,"name":"1"}}]"#
                    }
                    "j/activewindow" => "{}",
                    "j/workspaces" => r#"[{"id":1,"windows":0},{"id":2,"windows":3}]"#,
                    "dispatch focuswindow address:0xa" => "ok",
                    _ => "unknown request",
                };
                let _ = c.write_all(body.as_bytes());
            }
        });
        let ev = UnixListener::bind(good_dir.join(".socket2.sock")).unwrap();
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = ev.accept() {
                let _ = c.write_all(b"workspace>>1\nopenwin");
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        let h = Hypr::with_dir(good_dir.clone()).unwrap();
        assert_eq!(h.clients().len(), 1);
        assert_eq!(h.active_window_address(), None);
        assert_eq!(h.workspace_windows(2), Some(3));
        assert_eq!(h.workspace_windows(1), Some(0));
        assert_eq!(h.workspace_windows(9), Some(0));
        assert!(h.focus_window("0xa"));
        assert!(!h.focus_window("0xa;evil"));
        assert!(!h.dispatch("nonsense"));
        let mut s = h.event_stream().unwrap();
        let mut p = EventParser::new();
        let mut names = Vec::new();
        let t = Instant::now();
        while names.is_empty() && t.elapsed() < Duration::from_secs(2) {
            let mut b = [0u8; 64];
            match s.read(&mut b) {
                Ok(n) if n > 0 => names.extend(p.push(&b[..n])),
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        assert_eq!(names, vec!["workspace"]);
        // hung compositor: connections are accepted (backlog) but never answered
        let _bad = UnixListener::bind(bad_dir.join(".socket.sock")).unwrap();
        let hb = Hypr::with_dir(bad_dir.clone()).unwrap();
        let t = Instant::now();
        assert!(hb.clients().is_empty());
        assert_eq!(hb.workspace_window_count(1), 1);
        assert!(!hb.dispatch("focuswindow address:0xa"));
        // Each request is bounded by its read timeout (250 ms); the generous budget
        // only guards against a request that never gives up (and slow CI machines).
        assert!(t.elapsed() < Duration::from_millis(3000), "{:?}", t.elapsed());
        // no socket
        assert!(Hypr::with_dir(dir.join("missing")).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_detached_reaps_and_reports_missing() {
        // Needs /proc to look for zombies and a `true` binary in PATH (any Linux system).
        if !Path::new("/proc/self/stat").exists() || !program_exists("true") {
            eprintln!("skipping: /proc or `true` not available");
            return;
        }
        let me = std::process::id();
        let zombies = || {
            fs::read_dir("/proc")
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| fs::read_to_string(e.path().join("stat")).ok())
                        .filter(|st| {
                            // pid (comm) state ppid ...
                            let rest = st.rsplit(')').next().unwrap_or("");
                            let mut f = rest.split_whitespace();
                            f.next() == Some("Z") && f.next().and_then(|p| p.parse::<u32>().ok()) == Some(me)
                        })
                        .count()
                })
                .unwrap_or(0)
        };
        for _ in 0..5 {
            spawn_detached(&["true".to_string()]).unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(zombies(), 0, "unreaped children");
        let e = spawn_detached(&["definitely-not-a-program-xyz".to_string()]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(spawn_detached(&[]).is_err());
    }

    #[test]
    fn hypr_json_parsing_is_tolerant() {
        let clients: Value = serde_json::from_str(
            r#"[{"address":"0xa","mapped":true,"class":"kitty","title":"t","workspace":{"id":3,"name":"3"},"monitor":1,"pid":42,"focusHistoryID":0},
                {"address":"0xb"}, {"class":"no-address"}, 7, null,
                {"address":"0xc","workspace":{"id":-98},"focusHistoryID":"x","pid":1e3}]"#,
        )
        .unwrap();
        let w = parse_clients(&clients);
        assert_eq!(w.len(), 3);
        assert_eq!(
            (
                w[0].workspace.as_str(),
                w[0].pid,
                w[0].monitor,
                w[0].focus_history,
                w[0].mapped
            ),
            ("3", 42, 1, 0, true)
        );
        assert_eq!((w[1].class.as_str(), w[1].mapped, w[1].focus_history), ("", false, -1));
        assert_eq!(
            (w[2].workspace.as_str(), w[2].pid, w[2].focus_history),
            ("-98", 1000, -1)
        );
        assert!(parse_clients(&Value::Null).is_empty());
        assert!(parse_clients(&serde_json::json!({"a": 1})).is_empty());
        let mons: Value = serde_json::from_str(
            r#"[{"id":0,"name":"eDP-1","width":2560,"height":1600,"x":0,"y":0,"scale":1.60,"focused":true,"activeWorkspace":{"id":1,"name":"1"}},
                {"id":1,"name":"off","disabled":true}, {"name":"odd","scale":0}]"#,
        )
        .unwrap();
        let m = parse_monitors(&mons);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].logical_width(), m[0].logical_height()), (1600, 1000));
        assert_eq!(
            (m[0].active_workspace_id, m[0].active_workspace_name.as_str()),
            (1, "1")
        );
        assert_eq!(m[1].scale, 1.0);
        assert!(is_valid_address("0x5af195a78f70"));
        assert!(!is_valid_address("0x"));
        assert!(!is_valid_address("0xzz"));
        assert!(!is_valid_address("5af1"));
        assert!(!is_valid_address("0x1;exec"));
    }
}
