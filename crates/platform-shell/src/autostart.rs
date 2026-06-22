//! Autostart (start-on-login) per OS.
//!
//! S5 (#52): Linux only — create/remove the freedesktop autostart entry
//! `$XDG_CONFIG_HOME/autostart/clawdpanel.desktop` (or `~/.config/...` when
//! `XDG_CONFIG_HOME` is unset), a verbatim port of `internal/config/
//! startup_linux.go` (same path, same keys). Windows (HKCU `…\Run` value
//! `ClawdPanel`) and macOS (`~/Library/LaunchAgents/com.clawdpanel.app.plist` +
//! `launchctl load -w`) are stubbed for the parity slices (S10 / S11).

use std::io;
use std::path::Path;

/// Enable or disable launching ClawdPanel at login. `exe_path` is the absolute
/// path written into the autostart artifact (the running binary's path).
pub fn set_start_on_login(enabled: bool, exe_path: &Path) -> io::Result<()> {
    imp::set_start_on_login(enabled, exe_path)
}

/// Whether the autostart artifact currently exists.
pub fn is_start_on_login() -> bool {
    imp::is_start_on_login()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    const AUTOSTART_FILE_NAME: &str = "clawdpanel.desktop";

    /// `$XDG_CONFIG_HOME/autostart/clawdpanel.desktop`, falling back to
    /// `~/.config/autostart/clawdpanel.desktop` — matching `startup_linux.go`'s
    /// `autostartPath`.
    fn autostart_path() -> io::Result<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => home_dir()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "home directory not found")
                })?
                .join(".config"),
        };
        Ok(autostart_path_in(&base))
    }

    /// The autostart file path under an explicit config base
    /// (`<base>/autostart/clawdpanel.desktop`). Factored out so tests target a
    /// temp dir instead of the real `~/.config`.
    pub(super) fn autostart_path_in(config_base: &Path) -> PathBuf {
        config_base.join("autostart").join(AUTOSTART_FILE_NAME)
    }

    /// The `.desktop` body, byte-for-byte the entry `startup_linux.go` writes.
    pub(super) fn desktop_entry(exe_path: &str) -> String {
        format!(
            "[Desktop Entry]\n\
Type=Application\n\
Name=ClawdPanel\n\
Exec={exe_path}\n\
X-GNOME-Autostart-enabled=true\n\
NoDisplay=false\n\
Hidden=false\n\
Terminal=false\n"
        )
    }

    /// Create (enable) or remove (disable) the autostart entry at `path`. Split
    /// out of [`set_start_on_login`] so tests exercise write / remove / idempotent
    /// remove without touching the real config dir. A remove of a missing file is
    /// not an error (matches the Go `os.IsNotExist` guard).
    pub(super) fn apply(path: &Path, enabled: bool, exe_path: &str) -> io::Result<()> {
        if !enabled {
            return match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            };
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, desktop_entry(exe_path))
    }

    pub(super) fn set_start_on_login(enabled: bool, exe_path: &Path) -> io::Result<()> {
        let path = autostart_path()?;
        apply(&path, enabled, &exe_path.to_string_lossy())
    }

    pub(super) fn is_start_on_login() -> bool {
        autostart_path().map(|p| p.exists()).unwrap_or(false)
    }

    /// `$HOME` — matches Go's `os.UserHomeDir` on Linux (reads `HOME`).
    fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    // S10 fills Windows (HKCU `…\Run` value `ClawdPanel`); S11 fills macOS
    // (`~/Library/LaunchAgents/com.clawdpanel.app.plist` + `launchctl load -w`).
    pub(super) fn set_start_on_login(_enabled: bool, _exe_path: &Path) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn is_start_on_login() -> bool {
        false
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::imp::{apply, autostart_path_in, desktop_entry};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unique_temp() -> std::path::PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "clawdpanel-autostart-test-{}-{}",
            std::process::id(),
            n
        ))
    }

    #[test]
    fn path_layout_matches_go() {
        let p = autostart_path_in(Path::new("/cfg"));
        assert_eq!(p, Path::new("/cfg/autostart/clawdpanel.desktop"));
    }

    #[test]
    fn desktop_entry_has_exact_keys() {
        let e = desktop_entry("/opt/clawdpanel/clawdpanel");
        assert!(e.starts_with("[Desktop Entry]\n"));
        assert!(e.contains("Type=Application\n"));
        assert!(e.contains("Name=ClawdPanel\n"));
        assert!(e.contains("Exec=/opt/clawdpanel/clawdpanel\n"));
        assert!(e.contains("X-GNOME-Autostart-enabled=true\n"));
        assert!(e.contains("NoDisplay=false\n"));
        assert!(e.contains("Hidden=false\n"));
        assert!(e.contains("Terminal=false\n"));
    }

    #[test]
    fn enable_writes_then_disable_removes_idempotently() {
        let base = unique_temp();
        let path = autostart_path_in(&base);

        // enable → file written with the exact body
        apply(&path, true, "/x/clawdpanel").unwrap();
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, desktop_entry("/x/clawdpanel"));

        // disable → removed
        apply(&path, false, "/x/clawdpanel").unwrap();
        assert!(!path.exists());

        // disable again → no error (idempotent)
        apply(&path, false, "/x/clawdpanel").unwrap();

        let _ = std::fs::remove_dir_all(&base);
    }
}
