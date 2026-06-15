//! Auto-updater — S9 (#56). Ports the Go updater: the GitHub `releases/latest`
//! check (`app.go: CheckForUpdates` + `isNewerVersion`), the per-OS/flavor asset
//! pick (`selectUpdateAsset` / `installFlavor`), the streamed installer download
//! with progress (`InstallUpdate` + `progressWriter`), and the detached install +
//! relaunch (`runSilentInstaller` / `updateAppImage` / `spawnPackageInstall` /
//! `spawnDetached`).
//!
//! This module is UI-free: the check/download are plain async fns over `reqwest`,
//! and progress is pushed through a caller-supplied `FnMut` callback (the app
//! routes it onto a Slint property, replacing Go's `update:progress` event). The
//! `is_newer_version` / `select_update_asset_for` logic is pure + unit-tested
//! (the acceptance gate), with no network.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// GitHub API + human releases page (verbatim from `app.go`).
const RELEASES_API_URL: &str = "https://api.github.com/repos/Eyalm321/clawdpanel/releases/latest";
const RELEASES_PAGE_URL: &str = "https://github.com/Eyalm321/clawdpanel/releases/latest";

/// One downloadable file of a GitHub release (Go `releaseAsset`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub browser_download_url: String,
}

/// The check outcome handed to the update window (Go `UpdateCheckResult`). On
/// failure `error` carries a short message; otherwise the UI compares
/// `current`/`latest` and opens `url` when `update_available`.
#[derive(Debug, Clone, Default)]
pub struct UpdateCheckResult {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
    pub url: String,
    pub changelog: String,
    pub download_url: String,
    pub error: String,
}

/// The GitHub `releases/latest` payload subset we read.
#[derive(serde::Deserialize)]
struct ReleasePayload {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

/// Trim surrounding whitespace + a leading `v` (Go's
/// `strings.TrimPrefix(strings.TrimSpace(v), "v")`).
fn trim_version(v: &str) -> String {
    v.trim().strip_prefix('v').unwrap_or(v.trim()).to_string()
}

/// Leading signed-integer scan, matching Go's `fmt.Sscanf(part, "%d", &n)`:
/// reads an optional sign then a digit run from the start, ignoring any trailing
/// text; `None` when no integer is present.
fn leading_int(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    s[..i].parse::<i64>().ok()
}

/// Verbatim port of `app.go: isNewerVersion` — the custom dotted comparator with
/// an `rc` sub-comparator and the `dev` "always newer" rule. `latest`/`current`
/// are bare version strings (the leading `v` is tolerated). The acceptance gate.
pub fn is_newer_version(latest: &str, current: &str) -> bool {
    if current == "dev" {
        return true;
    }
    if latest.is_empty() {
        return false;
    }
    if latest == current {
        return false;
    }

    // `v` strip → `-`→`.` → split on `.` (so `2.0.0-rc1` → ["2","0","0","rc1"]).
    let parse = |v: &str| -> Vec<String> {
        let v = trim_version(v);
        v.replace('-', ".").split('.').map(str::to_string).collect()
    };
    let latest_parts = parse(latest);
    let current_parts = parse(current);

    let n = latest_parts.len().min(current_parts.len());
    for i in 0..n {
        let l = &latest_parts[i];
        let c = &current_parts[i];
        if l == c {
            continue;
        }
        match (leading_int(l), leading_int(c)) {
            (Some(ln), Some(cn)) => {
                if ln != cn {
                    return ln > cn;
                }
                // equal numbers, differing strings (e.g. "01" vs "1") → keep going
            }
            _ => {
                // at least one non-numeric component: try the `rc<N>` comparator,
                // else fall back to a lexicographic string compare.
                let lrc = leading_int(l.strip_prefix("rc").unwrap_or(l));
                let crc = leading_int(c.strip_prefix("rc").unwrap_or(c));
                match (lrc, crc) {
                    (Some(lr), Some(cr)) => {
                        if lr != cr {
                            return lr > cr;
                        }
                    }
                    _ => return l > c,
                }
            }
        }
    }

    latest_parts.len() > current_parts.len()
}

/// Linux asset pick keyed on the install flavor (Go `selectUpdateAssetFor`).
/// Pure + testable; `select_update_asset` supplies the live flavor.
pub fn select_update_asset_for(flavor: &str, assets: &[ReleaseAsset]) -> String {
    let suffix = match flavor {
        "appimage" => ".appimage",
        "rpm" => ".rpm",
        "deb" => ".deb",
        _ => return String::new(),
    };
    for a in assets {
        if a.name.to_lowercase().ends_with(suffix) {
            return a.browser_download_url.clone();
        }
    }
    String::new()
}

// ── Linux install flavors (port of updater_linux.go) ─────────────────────────

/// Reports how this binary was installed — decides the asset and the install
/// mechanism: `"appimage"` ($APPIMAGE set), `"rpm"`/`"deb"` (package-owned exe),
/// or `""` (manual copy / dev build → no in-place update). Port of `installFlavor`.
#[cfg(target_os = "linux")]
pub fn install_flavor() -> String {
    if std::env::var_os("APPIMAGE").is_some_and(|v| !v.is_empty()) {
        return "appimage".into();
    }
    let Ok(exe) = std::env::current_exe() else {
        return String::new();
    };
    if command_succeeds("rpm", &["-qf", &exe.to_string_lossy()]) {
        return "rpm".into();
    }
    if command_succeeds("dpkg", &["-S", &exe.to_string_lossy()]) {
        return "deb".into();
    }
    String::new()
}

/// Runs `cmd args...` discarding output, reporting whether it both spawned and
/// exited 0 (the `exec.Command(...).Run() == nil` check; a missing binary, the
/// `exec.LookPath` failure, counts as false).
#[cfg(target_os = "linux")]
fn command_succeeds(cmd: &str, args: &[&str]) -> bool {
    use std::process::{Command, Stdio};
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
pub fn select_update_asset(assets: &[ReleaseAsset]) -> String {
    select_update_asset_for(&install_flavor(), assets)
}

/// For an AppImage the running exe is the mounted squashfs payload; the
/// relaunchable file is the `.AppImage` itself ($APPIMAGE). Port of
/// `resolveRelaunchPath`.
#[cfg(target_os = "linux")]
pub fn resolve_relaunch_path(current: &Path) -> PathBuf {
    if let Some(ai) = std::env::var_os("APPIMAGE") {
        if !ai.is_empty() {
            return PathBuf::from(ai);
        }
    }
    current.to_path_buf()
}

/// Detached, in-place install + relaunch for the live flavor. Port of
/// `runSilentInstaller`.
#[cfg(target_os = "linux")]
pub fn run_silent_installer(installer_path: &Path, app_path: &Path) -> Result<(), String> {
    match install_flavor().as_str() {
        "appimage" => update_app_image(installer_path, app_path),
        "rpm" => spawn_package_install("pkexec rpm -U --replacepkgs", installer_path, app_path),
        "deb" => spawn_package_install("pkexec dpkg -i", installer_path, app_path),
        _ => Err("self-update is not supported for this install (use the releases page)".into()),
    }
}

/// Swaps the new image over the running one and relaunches it. Write-to-sibling +
/// rename keeps the replacement atomic; the squashfs mount of the running
/// instance stays valid until exit. Port of `updateAppImage`.
#[cfg(target_os = "linux")]
fn update_app_image(installer_path: &Path, app_image_path: &Path) -> Result<(), String> {
    let new_path = {
        let mut p = app_image_path.as_os_str().to_os_string();
        p.push(".new");
        PathBuf::from(p)
    };
    std::fs::copy(installer_path, &new_path).map_err(|e| format!("write replacement image: {e}"))?;
    // 0o755 so the swapped-in image stays executable.
    set_executable(&new_path)?;
    std::fs::rename(&new_path, app_image_path).map_err(|e| {
        let _ = std::fs::remove_file(&new_path);
        format!("replace AppImage: {e}")
    })?;
    let _ = std::fs::remove_file(installer_path);
    spawn_detached(&format!("sleep 1; exec {}", shell_quote(app_image_path)))
}

#[cfg(target_os = "linux")]
fn set_executable(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(p, perms).map_err(|e| format!("chmod replacement image: {e}"))
}

/// Runs the package manager (behind a polkit auth dialog) after the app exits,
/// then relaunches. If the user cancels auth the old version simply relaunches.
/// Port of `spawnPackageInstall`.
#[cfg(target_os = "linux")]
fn spawn_package_install(install_cmd: &str, installer_path: &Path, app_path: &Path) -> Result<(), String> {
    let installer = shell_quote(installer_path);
    let script = format!(
        "sleep 1; {install_cmd} {installer}; rm -f {installer}; exec {}",
        shell_quote(app_path)
    );
    spawn_detached(&script)
}

/// Starts `script` in its own session so it survives the app's imminent exit
/// (the Rust analogue of Go's `SysProcAttr{Setsid: true}` — `setsid` runs in the
/// child via `pre_exec` before `exec`). Port of `spawnDetached`.
#[cfg(target_os = "linux")]
fn spawn_detached(script: &str) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    // SAFETY: setsid is async-signal-safe and the only thing we do in the child
    // before exec; it detaches the installer into a new session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("spawn updater: {e}"))
}

/// Single-quote a path for `sh -c`, escaping embedded single quotes (Go relied on
/// `%q`; this is the POSIX-shell equivalent for the install scripts).
#[cfg(target_os = "linux")]
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

// ── Windows / macOS flavor stubs (S10 / S11) ─────────────────────────────────

#[cfg(target_os = "windows")]
pub fn select_update_asset(assets: &[ReleaseAsset]) -> String {
    // Windows NSIS installer (Go updater_windows.go); full silent `/S` install
    // lands with S10.
    for a in assets {
        let n = a.name.to_lowercase();
        if n.contains("windows") && n.ends_with(".exe") {
            return a.browser_download_url.clone();
        }
    }
    for a in assets {
        if a.name.to_lowercase().ends_with(".exe") {
            return a.browser_download_url.clone();
        }
    }
    String::new()
}

#[cfg(target_os = "windows")]
pub fn resolve_relaunch_path(current: &Path) -> PathBuf {
    current.to_path_buf()
}

#[cfg(target_os = "windows")]
pub fn run_silent_installer(_installer_path: &Path, _app_path: &Path) -> Result<(), String> {
    Err("self-update is not supported on this platform yet".into())
}

#[cfg(target_os = "macos")]
pub fn select_update_asset(_assets: &[ReleaseAsset]) -> String {
    // macOS: no silent .pkg install yet (needs admin elevation), so the update
    // window offers the releases page. Port of updater_darwin.go.
    String::new()
}

#[cfg(target_os = "macos")]
pub fn resolve_relaunch_path(current: &Path) -> PathBuf {
    current.to_path_buf()
}

#[cfg(target_os = "macos")]
pub fn run_silent_installer(_installer_path: &Path, _app_path: &Path) -> Result<(), String> {
    Err("self-update is not supported on this platform".into())
}

// ── Network: check + streamed download ───────────────────────────────────────

/// Queries the latest GitHub release and compares its tag to `current`. Network /
/// parse failures land in `error` (not an `Err`) so the UI can always show a
/// friendly line. Port of `app.go: CheckForUpdates` (minus the auto-open, which
/// the app does on the result).
pub async fn check_for_updates(current: &str) -> UpdateCheckResult {
    let mut res = UpdateCheckResult {
        current: trim_version(current),
        url: RELEASES_PAGE_URL.to_string(),
        ..Default::default()
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            res.error = "could not build request".into();
            return res;
        }
    };

    let resp = match client
        .get(RELEASES_API_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "clawdpanel")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            res.error = "network unavailable".into();
            return res;
        }
    };

    if !resp.status().is_success() {
        res.error = format!("server returned status: {}", resp.status().as_u16());
        return res;
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => {
            res.error = "failed to read server response".into();
            return res;
        }
    };
    let payload: ReleasePayload = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(_) => {
            res.error = "failed to parse server response".into();
            return res;
        }
    };

    res.changelog = payload.body;
    res.download_url = select_update_asset(&payload.assets);
    res.latest = trim_version(&payload.tag_name);
    res.update_available = is_newer_version(&res.latest, &res.current);
    res
}

/// Streams the installer to a temp file (pushing percent + downloaded/total MB to
/// `on_progress`, replacing Go's `update:progress` event), then runs the detached
/// installer + relaunch and exits the process. Returns only on failure; on
/// success it never returns (`std::process::exit(0)`). Port of `InstallUpdate`.
pub async fn install_update<F>(download_url: &str, mut on_progress: F) -> Result<(), String>
where
    F: FnMut(f64, f64, f64),
{
    use std::io::Write;

    // Keep the asset's own name (the installer dispatches on the extension).
    let base = download_url.split('?').next().unwrap_or(download_url);
    let asset_name = base.rsplit('/').next().unwrap_or("");
    if asset_name.is_empty() || asset_name == "." {
        return Err("cannot derive installer name from URL".into());
    }
    let tmp = std::env::temp_dir().join(format!("clawdpanel-update-{asset_name}"));

    let mut resp = reqwest::get(download_url)
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("server returned status: {}", resp.status().as_u16()));
    }

    let total = resp.content_length().unwrap_or(0);
    let mut out = std::fs::File::create(&tmp).map_err(|e| format!("failed to create temp file: {e}"))?;
    let mut downloaded: u64 = 0;
    const MB: f64 = 1024.0 * 1024.0;

    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("failed to read download: {e}"))?
    {
        out.write_all(&chunk).map_err(|e| format!("failed to save download: {e}"))?;
        downloaded += chunk.len() as u64;
        let pct = if total > 0 {
            downloaded as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        on_progress(pct, downloaded as f64 / MB, total as f64 / MB);
    }
    drop(out);

    let app_path = std::env::current_exe().map_err(|e| format!("failed to get executable path: {e}"))?;
    let app_path = resolve_relaunch_path(&app_path);

    run_silent_installer(&tmp, &app_path).map_err(|e| format!("failed to run silent installer: {e}"))?;

    std::process::exit(0);
}

/// Opens `url` in the user's browser (the releases-page handoff for installs with
/// no in-place flavor). Replaces Go's `Browser.OpenURL`.
pub fn open_url(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str, url: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.into(),
            browser_download_url: url.into(),
        }
    }

    // v2.2.1's actual asset list (ported from updater_linux_test.go).
    fn release_assets() -> Vec<ReleaseAsset> {
        vec![
            asset("clawdpanel-2.2.1-1.x86_64.rpm", "https://example/rpm"),
            asset("ClawdPanel-2.2.1-macos-universal.pkg", "https://example/pkg"),
            asset("ClawdPanel-2.2.1-windows-amd64-setup.exe", "https://example/exe"),
            asset("ClawdPanel-x86_64.AppImage", "https://example/appimage"),
            asset("clawdpanel_2.2.1_amd64.deb", "https://example/deb"),
        ]
    }

    // Port of TestSelectUpdateAssetFor.
    #[test]
    fn select_update_asset_for_each_flavor() {
        let assets = release_assets();
        let cases = [
            ("appimage", "https://example/appimage"),
            ("rpm", "https://example/rpm"),
            ("deb", "https://example/deb"),
            ("", ""), // manual install / dev build: no in-place update
        ];
        for (flavor, want) in cases {
            assert_eq!(
                select_update_asset_for(flavor, &assets),
                want,
                "flavor {flavor:?}"
            );
        }
    }

    // Port of TestInstallFlavorManualBinary + TestInstallFlavorAppImage. Kept in
    // one test (env vars are process-global in Rust) so the two cases run
    // sequentially without a cross-test race on $APPIMAGE.
    #[cfg(target_os = "linux")]
    #[test]
    fn install_flavor_manual_and_appimage() {
        // A test binary is neither an AppImage nor package-owned → "" (the
        // regression behind "self-update is not supported" on hand-installed
        // binaries).
        std::env::remove_var("APPIMAGE");
        assert_eq!(install_flavor(), "", "non-packaged binary must be manual");

        std::env::set_var("APPIMAGE", "/home/user/Apps/ClawdPanel.AppImage");
        assert_eq!(install_flavor(), "appimage");
        std::env::remove_var("APPIMAGE");
    }

    // The acceptance gate: isNewerVersion parity, incl. the dev + rc cases the
    // issue calls out. Cases trace the Go algorithm verbatim.
    #[test]
    fn is_newer_version_parity() {
        // dev is always newer (even with an empty latest — the dev check is first).
        assert!(is_newer_version("2.0.0", "dev"));
        assert!(is_newer_version("", "dev"));

        // empty latest (non-dev current) is never newer; equal is never newer.
        assert!(!is_newer_version("", "1.0.0"));
        assert!(!is_newer_version("1.2.3", "1.2.3"));

        // leading `v` is tolerated on both sides.
        assert!(is_newer_version("v1.2.4", "v1.2.3"));
        assert!(!is_newer_version("v1.2.3", "v1.2.4"));

        // dotted major/minor/patch.
        assert!(is_newer_version("1.3.0", "1.2.9"));
        assert!(is_newer_version("2.0.0", "1.9.9"));
        assert!(!is_newer_version("1.2.9", "1.3.0"));

        // numeric (not lexicographic) component compare: 10 > 9.
        assert!(is_newer_version("1.10.0", "1.9.0"));
        assert!(!is_newer_version("1.9.0", "1.10.0"));

        // rc sub-comparator: rc2 > rc1, numeric so rc10 > rc9.
        assert!(is_newer_version("2.0.0-rc2", "2.0.0-rc1"));
        assert!(!is_newer_version("2.0.0-rc1", "2.0.0-rc2"));
        assert!(is_newer_version("2.0.0-rc10", "2.0.0-rc9"));

        // length tie-breaker (verbatim quirk): a 4-part rc tag outranks the bare
        // 3-part release and vice-versa.
        assert!(is_newer_version("2.0.0-rc1", "2.0.0"));
        assert!(!is_newer_version("2.0.0", "2.0.0-rc1"));
        assert!(is_newer_version("1.2.3.1", "1.2.3"));
    }
}
