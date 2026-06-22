//! Monitor enumeration + dock-edge / strut-reservability logic. 1:1 port of the
//! Go `internal/platform/monitor_linux.go`. Parses `xrandr --listmonitors`
//! (RandR HiDPI scaling on Linux isn't derivable from xrandr alone, so DPI is
//! fixed at 1.0 — same as Go).

use std::process::Command;

use crate::{width_px, MonitorInfo};

/// Height of GNOME Shell's top panel for the primary monitor (where it lives).
/// The panel is compositor chrome: it draws over X11 docks regardless of
/// stacking, and as a Wayland-native surface it can't be measured via X tooling
/// — so we use its default 1×-scale height. `CLAWDPANEL_TOP_OFFSET` overrides.
fn gnome_panel_offset(is_primary: bool) -> i32 {
    if let Ok(v) = std::env::var("CLAWDPANEL_TOP_OFFSET") {
        if let Ok(n) = v.trim().parse::<i32>() {
            return n;
        }
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if !is_primary || !desktop.to_lowercase().contains("gnome") {
        return 0;
    }
    32
}

/// Parses `xrandr --listmonitors` output. Example line:
///
/// ` 0: +*HDMI-0 1920/598x1080/336+0+0  HDMI-0`
///
/// Fields: index, primary marker, name, geometry (logical px/physical mm),
/// X+Y origin, output name. Falls back to a single 1920×1080 monitor when xrandr
/// is unavailable or yields nothing.
pub fn get_monitors() -> Vec<MonitorInfo> {
    let out = match Command::new("xrandr").arg("--listmonitors").output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return vec![fallback_monitor()],
    };
    let text = String::from_utf8_lossy(&out);
    let mut monitors: Vec<MonitorInfo> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("Monitors:") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        // fields[0] = "0:" — index
        let idx = match fields[0].trim_end_matches(':').parse::<i32>() {
            Ok(i) => i,
            Err(_) => continue,
        };
        let name_marker = fields[1];
        let is_primary = name_marker.starts_with("+*") || name_marker.starts_with("*+");
        let name = name_marker.trim_start_matches(['+', '*']).to_string();
        let geom = fields[2]; // e.g. "1920/598x1080/336+0+0"
        let (w, h, x, y) = match parse_xrandr_geometry(geom) {
            Some(g) => g,
            None => continue,
        };
        monitors.push(MonitorInfo {
            index: idx,
            left: x,
            top: y,
            width: w,
            height: h,
            phys_width: w,
            dpi_scale: 1.0,
            is_primary,
            name,
            work_top_offset: gnome_panel_offset(is_primary),
            dock_edge: String::new(),
        });
    }
    if monitors.is_empty() {
        return vec![fallback_monitor()];
    }
    let all = monitors.clone();
    for m in monitors.iter_mut() {
        m.dock_edge = pick_dock_edge(m, &all);
    }
    monitors
}

fn fallback_monitor() -> MonitorInfo {
    MonitorInfo {
        index: 0,
        left: 0,
        top: 0,
        width: 1920,
        height: 1080,
        phys_width: 1920,
        dpi_scale: 1.0,
        is_primary: true,
        name: "default".to_string(),
        work_top_offset: 0,
        dock_edge: String::new(),
    }
}

fn x_overlap(a: &MonitorInfo, b: &MonitorInfo) -> bool {
    a.left < b.left + width_px(b) && b.left < a.left + width_px(a)
}

/// Reports whether a strut along the given edge of `mon` would reserve only
/// `mon`'s own space. Struts are measured from the ROOT screen edge, so any other
/// monitor occupying the band between that root edge and `mon` (within `mon`'s
/// x-range) would be carved up instead.
pub(crate) fn edge_reservable(mon: &MonitorInfo, all: &[MonitorInfo], edge: &str) -> bool {
    for o in all {
        if o.index == mon.index || !x_overlap(mon, o) {
            continue;
        }
        if edge == "bottom" {
            if o.top + o.height > mon.top + mon.height {
                return false;
            }
        } else if o.top < mon.top {
            return false;
        }
    }
    true
}

/// Prefers the top edge, falling back to the bottom when only the bottom can
/// actually reserve space (e.g. a center monitor with another above it). If
/// neither edge is reservable the bar still docks top — visible and above the
/// stack, just without a strut.
fn pick_dock_edge(mon: &MonitorInfo, all: &[MonitorInfo]) -> String {
    if edge_reservable(mon, all, "top") {
        return "top".to_string();
    }
    if edge_reservable(mon, all, "bottom") {
        return "bottom".to_string();
    }
    "top".to_string()
}

/// The root screen size as the bounding box of the given monitor layout.
/// (`xdotool getdisplaygeometry` is unreliable under XWayland — it can report a
/// single monitor's logical size instead of the root extent.)
#[allow(dead_code)]
pub(crate) fn root_geometry(all: &[MonitorInfo]) -> (i32, i32) {
    let mut w = 0;
    let mut h = 0;
    for m in all {
        let r = m.left + width_px(m);
        if r > w {
            w = r;
        }
        let b = m.top + m.height;
        if b > h {
            h = b;
        }
    }
    (w, h)
}

/// Parses `"1920/598x1080/336+0+0"` → `(1920, 1080, 0, 0)`. We discard the
/// physical-mm size; DPI scale is fixed at 1.0 by the caller.
fn parse_xrandr_geometry(s: &str) -> Option<(i32, i32, i32, i32)> {
    // Split on 'x' first: "1920/598" and "1080/336+0+0"
    let (wpart, rest) = s.split_once('x')?;
    let w: i32 = wpart.split('/').next()?.parse().ok()?;

    // Find the first '+' or '-' after the height in `rest` ("1080/336+0+0").
    let h_end = rest.char_indices().find(|&(_, c)| c == '+' || c == '-')?.0;
    if h_end == 0 {
        return None;
    }
    let h: i32 = rest[..h_end].split('/').next()?.parse().ok()?;

    // rest[h_end..] is "+0+0" or similar — two signed ints.
    let off = &rest[h_end..];
    // Find the second '+' or '-' (start scanning at byte 1 to skip the leading sign).
    let split_idx = off
        .char_indices()
        .skip(1)
        .find(|&(_, c)| c == '+' || c == '-')?
        .0;
    let x: i32 = off[..split_idx].parse().ok()?;
    let y: i32 = off[split_idx..].parse().ok()?;
    Some((w, h, x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(index: i32, left: i32, top: i32, width: i32, height: i32) -> MonitorInfo {
        MonitorInfo {
            index,
            left,
            top,
            width,
            height,
            phys_width: width,
            dpi_scale: 1.0,
            ..Default::default()
        }
    }

    #[test]
    fn parse_geometry_basic() {
        assert_eq!(parse_xrandr_geometry("1920/598x1080/336+0+0"), Some((1920, 1080, 0, 0)));
    }

    #[test]
    fn parse_geometry_offset_and_negative() {
        assert_eq!(parse_xrandr_geometry("2560/600x1440/340+1920+0"), Some((2560, 1440, 1920, 0)));
        // xrandr writes a left-of-origin monitor with a leading '-', not "+-".
        assert_eq!(parse_xrandr_geometry("1920/500x1080/300-1920+0"), Some((1920, 1080, -1920, 0)));
    }

    #[test]
    fn parse_geometry_rejects_garbage() {
        assert_eq!(parse_xrandr_geometry("nonsense"), None);
        assert_eq!(parse_xrandr_geometry("1920x1080"), None); // no offset section
    }

    #[test]
    fn edge_reservable_single_monitor() {
        let a = mon(0, 0, 0, 1920, 1080);
        assert!(edge_reservable(&a, &[a.clone()], "top"));
        assert!(edge_reservable(&a, &[a.clone()], "bottom"));
    }

    #[test]
    fn stacked_center_monitor_only_reserves_bottom() {
        // `mid` has another monitor directly above it (same x-range): a top strut
        // would carve up `above`, so only the bottom edge is reservable.
        let above = mon(0, 0, 0, 1920, 1080);
        let mid = mon(1, 0, 1080, 1920, 1080);
        let all = vec![above, mid.clone()];
        assert!(!edge_reservable(&mid, &all, "top"));
        assert!(edge_reservable(&mid, &all, "bottom"));
        assert_eq!(pick_dock_edge(&mid, &all), "bottom");
    }

    #[test]
    fn side_by_side_monitors_both_reserve_top() {
        let left = mon(0, 0, 0, 1920, 1080);
        let right = mon(1, 1920, 0, 1920, 1080);
        let all = vec![left.clone(), right];
        assert_eq!(pick_dock_edge(&left, &all), "top");
    }

    #[test]
    fn root_geometry_bounding_box() {
        let all = vec![mon(0, 0, 0, 1920, 1080), mon(1, 1920, 0, 2560, 1440)];
        assert_eq!(root_geometry(&all), (4480, 1440));
    }
}
