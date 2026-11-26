//! User settings (`settings.json`), shared by the dock and the settings window.
//!
//! The settings window writes the file and tells the running dock to reload it through the
//! control socket. The dock also notices a changed file by itself (it polls the modification
//! time), so a hand edit works too.
use serde::Serialize;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const ZOOM_MIN: f32 = 1.25;
pub const ZOOM_MAX: f32 = 5.0;
pub const ZOOM_DEFAULT: f32 = 1.78;
pub const RECOMMENDED_MIN: u32 = 1;
pub const RECOMMENDED_MAX: u32 = 5;
pub const RECOMMENDED_DEFAULT: u32 = 3;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Settings {
    /// The dock hides and slides in when the pointer touches the bottom edge. When off, it stays
    /// visible and reserves its space on the screen (windows are not placed under it).
    pub auto_hide: bool,
    /// The glass shows the backdrop live (Hyprland blurs it every frame) instead of refracting a
    /// snapshot taken when the dock appears. Needs the `glassdock` blur layer rule in Hyprland.
    pub live_glass: bool,
    /// Icons grow under the pointer.
    pub magnification: bool,
    /// Peak icon scale of the magnification (`ZOOM_MIN..=ZOOM_MAX`).
    pub zoom: f32,
    /// The outline (edge ring) of the glass takes the desktop accent colour: Hyprland's active
    /// window border (`general:col.active_border`), so it follows themes and wallpaper sync.
    pub outline_system: bool,
    /// Show recently and frequently used apps that are closed and not pinned.
    pub recommended: bool,
    /// How many recommended apps at most (`RECOMMENDED_MIN..=RECOMMENDED_MAX`).
    pub recommended_count: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            auto_hide: true,
            live_glass: true,
            magnification: true,
            zoom: ZOOM_DEFAULT,
            outline_system: false,
            recommended: true,
            recommended_count: RECOMMENDED_DEFAULT,
        }
    }
}

impl Settings {
    /// `$GLASSDOCK_SETTINGS`, otherwise `$XDG_CONFIG_HOME/glassdock/settings.json`.
    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("GLASSDOCK_SETTINGS").filter(|p| !p.is_empty()) {
            return PathBuf::from(p);
        }
        config_dir().join("settings.json")
    }

    /// Forces every value into its valid range (NaN and infinities fall back to the default).
    pub fn clamp(&mut self) {
        if !self.zoom.is_finite() {
            self.zoom = ZOOM_DEFAULT;
        }
        self.zoom = self.zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        self.recommended_count = self.recommended_count.clamp(RECOMMENDED_MIN, RECOMMENDED_MAX);
    }

    /// Loads the settings; a missing or corrupt file gives the defaults. Never fails.
    pub fn load() -> Settings {
        Settings::load_from(&Settings::path())
    }

    /// Reads each key on its own: a value of the wrong type (or a missing key) only affects
    /// that setting, which keeps its default.
    pub fn load_from(path: &Path) -> Settings {
        let mut s = Settings::default();
        let value = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        if let Some(o) = value.as_ref().and_then(|v| v.as_object()) {
            if let Some(b) = o.get("auto_hide").and_then(|v| v.as_bool()) {
                s.auto_hide = b;
            }
            if let Some(b) = o.get("live_glass").and_then(|v| v.as_bool()) {
                s.live_glass = b;
            }
            if let Some(b) = o.get("magnification").and_then(|v| v.as_bool()) {
                s.magnification = b;
            }
            if let Some(z) = o.get("zoom").and_then(|v| v.as_f64()) {
                s.zoom = z as f32;
            }
            if let Some(b) = o.get("outline_system").and_then(|v| v.as_bool()) {
                s.outline_system = b;
            }
            if let Some(b) = o.get("recommended").and_then(|v| v.as_bool()) {
                s.recommended = b;
            }
            if let Some(n) = o.get("recommended_count").and_then(|v| v.as_u64()) {
                s.recommended_count = n.min(u32::MAX as u64) as u32;
            }
        }
        s.clamp();
        s
    }

    /// Writes the file atomically (temporary file + rename).
    pub fn save(&self) -> io::Result<()> {
        self.save_to(&Settings::path())
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        let mut s = self.clone();
        s.clamp();
        // Write through a symlink (dotfile managers) instead of replacing it with a plain file.
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let path = resolved.as_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(&s).map_err(io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json + "\n")?;
        std::fs::rename(&tmp, path)
    }

    /// Modification time of the file, to detect external changes cheaply.
    pub fn stamp() -> Option<SystemTime> {
        std::fs::metadata(Settings::path()).and_then(|m| m.modified()).ok()
    }
}

/// `$XDG_CONFIG_HOME/glassdock` (default `~/.config/glassdock`).
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into())).join(".config"));
    base.join("glassdock")
}

/// `$XDG_STATE_HOME/glassdock` (default `~/.local/state/glassdock`): data the dock writes itself.
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into())).join(".local/state"));
    base.join("glassdock")
}

/// Control socket of the running dock: `$GLASSDOCK_SOCKET`, otherwise
/// `$XDG_RUNTIME_DIR/glassdock.sock`.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GLASSDOCK_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let rt = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    rt.join("glassdock.sock")
}

/// Sends one command line (e.g. `"reload"`) to the running dock. Best effort: returns the reply,
/// or `None` if no dock is listening.
pub fn ctl(command: &str) -> Option<String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let mut s = UnixStream::connect(socket_path()).ok()?;
    s.set_read_timeout(Some(Duration::from_millis(800))).ok()?;
    s.set_write_timeout(Some(Duration::from_millis(800))).ok()?;
    s.write_all(command.as_bytes()).ok()?;
    s.shutdown(std::net::Shutdown::Write).ok()?;
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join("glassdock-tests")
            .join(format!("config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn defaults_when_missing_or_corrupt() {
        let d = tmp("defaults");
        assert_eq!(Settings::load_from(&d.join("none.json")), Settings::default());
        std::fs::write(d.join("bad.json"), "{ nope").unwrap();
        assert_eq!(Settings::load_from(&d.join("bad.json")), Settings::default());
    }

    #[test]
    fn a_wrong_type_only_affects_its_own_key() {
        let d = tmp("types");
        std::fs::write(
            d.join("s.json"),
            r#"{ "zoom": "big", "recommended": false, "recommended_count": 4, "magnification": 1 }"#,
        )
        .unwrap();
        let s = Settings::load_from(&d.join("s.json"));
        assert_eq!(s.zoom, ZOOM_DEFAULT, "wrong type: default");
        assert!(s.magnification, "wrong type: default");
        assert!(!s.recommended, "valid keys still apply");
        assert_eq!(s.recommended_count, 4);
        std::fs::write(d.join("arr.json"), "[1,2]").unwrap();
        assert_eq!(Settings::load_from(&d.join("arr.json")), Settings::default());
    }

    #[test]
    fn saving_through_a_symlink_keeps_the_link() {
        let d = tmp("symlink");
        let real = d.join("real.json");
        std::fs::write(&real, "{}").unwrap();
        let link = d.join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        Settings {
            zoom: 1.5,
            ..Settings::default()
        }
        .save_to(&link)
        .unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(Settings::load_from(&real).zoom, 1.5);
    }

    #[test]
    fn outline_option_defaults_off_and_reads_the_key() {
        let d = tmp("outline");
        assert!(!Settings::default().outline_system);
        std::fs::write(d.join("a.json"), r#"{ "zoom": 2.0 }"#).unwrap();
        assert!(!Settings::load_from(&d.join("a.json")).outline_system, "absent key: off");
        std::fs::write(d.join("b.json"), r#"{ "outline_system": true }"#).unwrap();
        assert!(Settings::load_from(&d.join("b.json")).outline_system);
        std::fs::write(d.join("c.json"), r#"{ "outline_system": "yes" }"#).unwrap();
        assert!(!Settings::load_from(&d.join("c.json")).outline_system, "wrong type: default");
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let d = tmp("partial");
        std::fs::write(d.join("s.json"), r#"{ "recommended": false }"#).unwrap();
        let s = Settings::load_from(&d.join("s.json"));
        assert!(!s.recommended);
        assert!(s.magnification);
        assert!(s.auto_hide, "auto-hide stays on when the file does not mention it");
        assert_eq!(s.recommended_count, RECOMMENDED_DEFAULT);
    }

    #[test]
    fn values_are_clamped() {
        let d = tmp("clamp");
        std::fs::write(d.join("s.json"), r#"{ "zoom": 9.0, "recommended_count": 99 }"#).unwrap();
        let s = Settings::load_from(&d.join("s.json"));
        assert_eq!(s.zoom, ZOOM_MAX);
        assert_eq!(s.recommended_count, RECOMMENDED_MAX);
        let mut s = Settings {
            zoom: f32::NAN,
            recommended_count: 0,
            ..Settings::default()
        };
        s.clamp();
        assert_eq!(s.zoom, ZOOM_DEFAULT);
        assert_eq!(s.recommended_count, RECOMMENDED_MIN);
    }

    #[test]
    fn save_and_load_roundtrip_is_atomic() {
        let d = tmp("roundtrip");
        let p = d.join("sub").join("s.json");
        let s = Settings {
            auto_hide: false,
            live_glass: false,
            magnification: false,
            zoom: 1.5,
            outline_system: true,
            recommended: false,
            recommended_count: 2,
        };
        s.save_to(&p).unwrap();
        assert_eq!(Settings::load_from(&p), s);
        assert!(!p.with_extension("json.tmp").exists());
    }
}
