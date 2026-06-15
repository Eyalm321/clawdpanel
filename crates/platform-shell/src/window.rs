//! Linux (X11 / XWayland) window integration via `x11rb`. Replaces the Go
//! `internal/platform/window_linux.go` wmctrl/xprop/xdotool subprocess zoo with
//! direct X protocol requests: `_NET_WM_WINDOW_TYPE=_DOCK`, always-on-top
//! (`_NET_WM_STATE_ABOVE`), `_NET_WM_STRUT_PARTIAL` (12 cardinals), opacity,
//! geometry/move, map/unmap, cursor (`QueryPointer`), and fullscreen detection
//! (`_NET_ACTIVE_WINDOW` + `_NET_WM_STATE_FULLSCREEN`).
//!
//! Click-through is a no-op (matches v1 — it needs XShape/XFixes input regions),
//! and clip-top is a no-op (Windows-only `SetWindowRgn` concept).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask, PropMode, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::monitor::{edge_reservable, root_geometry};
use crate::{width_px, MonitorInfo, WindowOps};

const NET_WM_STATE_ADD: u32 = 1;

struct Atoms {
    window_type: u32,
    window_type_dock: u32,
    state: u32,
    state_above: u32,
    state_fullscreen: u32,
    strut_partial: u32,
    opacity: u32,
    active_window: u32,
}

struct FsCache {
    at: Option<Instant>,
    last_mon: i32,
    last: bool,
}

struct X11Inner {
    conn: RustConnection,
    root: Window,
    window: Window,
    atoms: Atoms,
    /// Last successful cursor read. The reveal machine polls at 12.5Hz; a
    /// transient failure must not read as "cursor gone" (it would flicker hover).
    /// `(-1, -1)` before the first successful read.
    cursor: Mutex<(i32, i32)>,
    fs_cache: Mutex<FsCache>,
}

/// A bound X11 window: the dock setup surface plus the production [`WindowOps`]
/// for the reveal machine. Cheap to [`Clone`] (it shares one connection), so App
/// keeps one for docking and hands a clone to the reveal `Controller`.
#[derive(Clone)]
pub struct X11Window {
    inner: Arc<X11Inner>,
}

fn intern(conn: &RustConnection, name: &[u8]) -> Result<u32, Box<dyn std::error::Error>> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

impl X11Window {
    /// Binds to an existing X window id (resolved from the Slint/winit window's
    /// raw handle by the caller) on the display named by `$DISPLAY`, interning
    /// the EWMH atoms it needs.
    pub fn new(window_id: u32) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        let atoms = Atoms {
            window_type: intern(&conn, b"_NET_WM_WINDOW_TYPE")?,
            window_type_dock: intern(&conn, b"_NET_WM_WINDOW_TYPE_DOCK")?,
            state: intern(&conn, b"_NET_WM_STATE")?,
            state_above: intern(&conn, b"_NET_WM_STATE_ABOVE")?,
            state_fullscreen: intern(&conn, b"_NET_WM_STATE_FULLSCREEN")?,
            strut_partial: intern(&conn, b"_NET_WM_STRUT_PARTIAL")?,
            opacity: intern(&conn, b"_NET_WM_WINDOW_OPACITY")?,
            active_window: intern(&conn, b"_NET_ACTIVE_WINDOW")?,
        };
        Ok(X11Window {
            inner: Arc::new(X11Inner {
                conn,
                root,
                window: window_id,
                atoms,
                cursor: Mutex::new((-1, -1)),
                fs_cache: Mutex::new(FsCache { at: None, last_mon: -1, last: false }),
            }),
        })
    }

    /// Boxes a clone as the production [`WindowOps`] for a reveal `Controller`.
    pub fn ops(&self) -> Box<dyn WindowOps> {
        Box::new(self.clone())
    }

    /// Marks the window as an EWMH dock (compliant compositors keep it above
    /// others) and requests always-on-top via a `_NET_WM_STATE_ABOVE` message.
    pub fn apply_bar_styles(&self) {
        let i = &self.inner;
        let _ = i.conn.change_property32(
            PropMode::REPLACE,
            i.window,
            i.atoms.window_type,
            AtomEnum::ATOM,
            &[i.atoms.window_type_dock],
        );
        // _NET_WM_STATE add ABOVE (the window is mapped at this point, so EWMH
        // wants a client message to the root window).
        let ev = ClientMessageEvent::new(
            32,
            i.window,
            i.atoms.state,
            [NET_WM_STATE_ADD, i.atoms.state_above, 0, 0, 0],
        );
        let _ = i.conn.send_event(
            false,
            i.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            ev,
        );
        let _ = i.conn.flush();
    }

    /// Positions the bar on `mon`'s configured edge at full monitor width and
    /// reserves the strip via `_NET_WM_STRUT_PARTIAL` when the edge is reservable
    /// (struts measure from the ROOT screen edge → multi-monitor aware via
    /// `dock_edge`). Ported verbatim from `window_linux.go` `DockToMonitor`.
    pub fn dock_to_monitor(
        &self,
        mon: &MonitorInfo,
        bar_height: i32,
        app_bar_mode: bool,
        all: &[MonitorInfo],
    ) {
        let i = &self.inner;
        let width = width_px(mon);
        // Resting position: top edge sits below any compositor chrome (GNOME
        // panel), bottom edge hugs the monitor's bottom (taskbar-style).
        let y = if mon.dock_edge == "bottom" {
            mon.top + mon.height - bar_height
        } else {
            mon.top + mon.work_top_offset
        };
        let _ = i.conn.configure_window(
            i.window,
            &ConfigureWindowAux::new()
                .x(mon.left)
                .y(y)
                .width(width as u32)
                .height(bar_height as u32),
        );

        // _NET_WM_STRUT_PARTIAL reserves space measured from the ROOT screen
        // edges. Only set it when the band between the root edge and the bar is
        // free of other monitors (`edge_reservable`); otherwise the strut would
        // carve up the monitor above/below instead.
        let strut_ok = app_bar_mode && edge_reservable(mon, all, &mon.dock_edge);
        if strut_ok && mon.dock_edge == "bottom" {
            let (_, root_h) = root_geometry(all);
            if root_h > 0 {
                let depth = root_h - (mon.top + mon.height) + bar_height;
                let strut: [u32; 12] = [
                    0, 0, 0, depth as u32, 0, 0, 0, 0, 0, 0, mon.left as u32, (mon.left + width) as u32,
                ];
                self.set_strut(&strut);
                let _ = i.conn.flush();
                return;
            }
        } else if strut_ok {
            let depth = y + bar_height;
            let strut: [u32; 12] = [
                0, 0, depth as u32, 0, 0, 0, 0, 0, mon.left as u32, (mon.left + width) as u32, 0, 0,
            ];
            self.set_strut(&strut);
            let _ = i.conn.flush();
            return;
        }
        self.remove_app_bar();
    }

    fn set_strut(&self, strut: &[u32; 12]) {
        let i = &self.inner;
        let _ = i.conn.change_property32(
            PropMode::REPLACE,
            i.window,
            i.atoms.strut_partial,
            AtomEnum::CARDINAL,
            strut,
        );
    }

    /// Drops the reserved screen strip.
    pub fn remove_app_bar(&self) {
        let i = &self.inner;
        let _ = i.conn.delete_property(i.window, i.atoms.strut_partial);
        let _ = i.conn.flush();
    }

    /// Sets `_NET_WM_WINDOW_OPACITY` (0xFFFFFFFF = opaque).
    pub fn set_opacity(&self, opacity: f64) {
        let o = opacity.clamp(0.0, 1.0);
        let alpha = (o * (0xFFFF_FFFFu32 as f64)) as u32;
        let i = &self.inner;
        let _ = i.conn.change_property32(
            PropMode::REPLACE,
            i.window,
            i.atoms.opacity,
            AtomEnum::CARDINAL,
            &[alpha],
        );
        let _ = i.conn.flush();
    }

    /// Root-relative top-left of the window via `TranslateCoordinates` (handles
    /// reparenting WMs) + its size via `GetGeometry`. `(0,0,0,0)` on failure.
    fn rect(&self) -> (i32, i32, i32, i32) {
        let i = &self.inner;
        let geo = match i.conn.get_geometry(i.window).map(|c| c.reply()) {
            Ok(Ok(g)) => g,
            _ => return (0, 0, 0, 0),
        };
        let (x, y) = match i
            .conn
            .translate_coordinates(i.window, i.root, 0, 0)
            .map(|c| c.reply())
        {
            Ok(Ok(t)) => (t.dst_x as i32, t.dst_y as i32),
            _ => (geo.x as i32, geo.y as i32),
        };
        (x, y, geo.width as i32, geo.height as i32)
    }

    fn full_screen_active_uncached(&self, mon: &MonitorInfo) -> bool {
        let i = &self.inner;
        // _NET_ACTIVE_WINDOW on root → the focused window id.
        let active = match i
            .conn
            .get_property(false, i.root, i.atoms.active_window, AtomEnum::WINDOW, 0, 1)
            .map(|c| c.reply())
        {
            Ok(Ok(r)) => r.value32().and_then(|mut v| v.next()).unwrap_or(0),
            _ => 0,
        };
        if active == 0 {
            return false;
        }
        // _NET_WM_STATE on it → must contain _NET_WM_STATE_FULLSCREEN.
        let is_fs = match i
            .conn
            .get_property(false, active, i.atoms.state, AtomEnum::ATOM, 0, 32)
            .map(|c| c.reply())
        {
            Ok(Ok(r)) => r
                .value32()
                .map(|mut v| v.any(|a| a == i.atoms.state_fullscreen))
                .unwrap_or(false),
            _ => false,
        };
        if !is_fs {
            return false;
        }
        // Scope to the bar's monitor: the fullscreen window's center must be on it.
        let geo = match i.conn.get_geometry(active).map(|c| c.reply()) {
            Ok(Ok(g)) => g,
            _ => return false,
        };
        let (lx, ly) = match i
            .conn
            .translate_coordinates(active, i.root, 0, 0)
            .map(|c| c.reply())
        {
            Ok(Ok(t)) => (t.dst_x as i32, t.dst_y as i32),
            _ => (geo.x as i32, geo.y as i32),
        };
        let (w, h) = (geo.width as i32, geo.height as i32);
        if w == 0 && h == 0 {
            return false;
        }
        let (cx, cy) = (lx + w / 2, ly + h / 2);
        let mw = width_px(mon);
        cx >= mon.left && cx < mon.left + mw && cy >= mon.top && cy < mon.top + mon.height
    }
}

impl WindowOps for X11Window {
    fn window_rect(&self) -> (i32, i32, i32, i32) {
        self.rect()
    }

    fn move_to(&self, x: i32, y: i32) {
        let i = &self.inner;
        let _ = i
            .conn
            .configure_window(i.window, &ConfigureWindowAux::new().x(x).y(y));
        let _ = i.conn.flush();
    }

    fn clip_top(&self, _width: i32, _height: i32, _top_clip: i32) {
        // No-op on Linux (Windows `SetWindowRgn` concept); the compositor hides
        // the above-monitor spill.
    }

    fn show(&self) {
        let i = &self.inner;
        let _ = i.conn.map_window(i.window);
        let _ = i.conn.flush();
    }

    fn hide(&self) {
        let i = &self.inner;
        let _ = i.conn.unmap_window(i.window);
        let _ = i.conn.flush();
    }

    fn set_click_through(&self, _enabled: bool) {
        // No-op (needs XShape/XFixes input region; deferred — matches v1).
    }

    fn cursor_pos(&self) -> (i32, i32) {
        let i = &self.inner;
        let mut last = i.cursor.lock().unwrap();
        match i.conn.query_pointer(i.root).map(|c| c.reply()) {
            Ok(Ok(p)) => {
                *last = (p.root_x as i32, p.root_y as i32);
                *last
            }
            // Transient failure: return the last known position (the reveal
            // machine's "no cursor source" sentinel is the initial (-1, -1)).
            _ => *last,
        }
    }

    fn full_screen_active(&self, mon: &MonitorInfo) -> bool {
        // Throttle the X round-trips: the reveal machine calls this every 80ms
        // tick, but fullscreen state changes on a human timescale.
        let i = &self.inner;
        {
            let c = i.fs_cache.lock().unwrap();
            if c.last_mon == mon.index {
                if let Some(at) = c.at {
                    if at.elapsed() < Duration::from_secs(1) {
                        return c.last;
                    }
                }
            }
        }
        let v = self.full_screen_active_uncached(mon);
        let mut c = i.fs_cache.lock().unwrap();
        c.at = Some(Instant::now());
        c.last_mon = mon.index;
        c.last = v;
        v
    }

    fn auto_hide_supported(&self) -> bool {
        true
    }
}
