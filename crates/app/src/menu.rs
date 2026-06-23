//! Brand menu window controller (S12).
//!
//! Manages the frameless, always-on-top popup that anchors under the brand icon
//! when clicked, offering CHECK FOR UPDATES and EXIT.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use slint::winit_030::WinitWindowAccessor;
use slint::winit_030::winit::window::WindowLevel;
use slint::winit_030::winit::dpi::{PhysicalPosition, PhysicalSize};

use clawdpanel_ui::{BrandMenuWindow, Menu, Theme};
#[cfg(target_os = "linux")]
use clawdpanel_platform_shell::WindowOps;
use crate::settings::UiState;

pub struct MenuState {
    pub window: RefCell<Option<BrandMenuWindow>>,
    pub visible: Cell<bool>,
    pub last_hidden_at: Cell<Instant>,
    pub shown_at: Cell<Instant>,
    pub focus_timer: RefCell<Option<slint::Timer>>,
}

thread_local! {
    /// Thread-local state for the brand menu window, kept on the main UI thread.
    pub static MENU_STATE: Rc<MenuState> = Rc::new(MenuState {
        window: RefCell::new(None),
        visible: Cell::new(false),
        last_hidden_at: Cell::new(Instant::now() - Duration::from_secs(1)),
        shown_at: Cell::new(Instant::now() - Duration::from_secs(1)),
        focus_timer: RefCell::new(None),
    });
}

/// Toggles the brand menu window visibility.
pub fn toggle_brand_menu(ui: &Rc<UiState>) {
    MENU_STATE.with(|state| {
        if state.visible.get() {
            hide_brand_menu(state);
            return;
        }

        // The click that brought us here may have just auto-closed the menu via focus
        // loss; treat that as "toggle off" rather than immediately reopening.
        if state.last_hidden_at.get().elapsed() < Duration::from_millis(250) {
            return;
        }

        if state.window.borrow().is_none() {
            match build_window(ui, state) {
                Ok(w) => *state.window.borrow_mut() = Some(w),
                Err(e) => {
                    eprintln!("[menu] failed to create brand menu window: {e}");
                    return;
                }
            }
        }

        if let Some(w) = state.window.borrow().as_ref() {
            w.global::<Theme>().set_index(ui.theme_idx.get());
            w.global::<Menu>().set_check_state("idle".into());
            w.global::<Menu>().set_hint("".into());
            w.global::<Menu>().set_hint_kind("none".into());

            // Position it anchored under the brand icon at the bar monitor's top-left.
            let mon_idx = ui.cfg.borrow().monitor as usize;
            let monitors = clawdpanel_platform_shell::get_monitors();
            let mon = monitors.get(mon_idx).or_else(|| monitors.first()).cloned().unwrap_or_default();

            let scale = mon.dpi_scale.max(1.0);
            let bar_height = 28;
            let menu_width = 188;
            let menu_height = 64;

            let (x, y) = if mon.dock_edge == "bottom" {
                let mon_bottom = mon.top + mon.height;
                (
                    mon.left + (6.0 * scale) as i32,
                    mon_bottom - (bar_height as f64 * scale) as i32 - (menu_height as f64 * scale) as i32
                )
            } else {
                (
                    mon.left + (6.0 * scale) as i32,
                    mon.top + (mon.work_top_offset as f64 * scale) as i32 + (bar_height as f64 * scale) as i32
                )
            };

            let configure_window = |w: &BrandMenuWindow| -> bool {
                let _xid = w.window().with_winit_window(|win| {
                    crate::x11_window_id(win)
                }).flatten();

                let winit_ok = w.window().with_winit_window(|win| {
                    win.set_decorations(false);
                    win.set_window_level(WindowLevel::AlwaysOnTop);
                    let _ = win.request_inner_size(PhysicalSize::new(
                        (menu_width as f64 * scale) as u32,
                        (menu_height as f64 * scale) as u32,
                    ));
                    win.set_outer_position(PhysicalPosition::new(x, y));
                }).is_some();

                #[cfg(target_os = "linux")]
                if let Some(id) = _xid {
                    if let Ok(xwin) = clawdpanel_platform_shell::X11Window::new(id) {
                        xwin.apply_menu_styles();
                        xwin.move_to(x, y);
                        eprintln!("[menu] X11 menu window type and position (x={}, y={}) applied via x11rb", x, y);
                    }
                }

                winit_ok
            };

            let _ = w.show();

            if !configure_window(w) {
                let w_weak = w.as_weak();
                std::thread::spawn(move || {
                    for _attempt in 1..=10 {
                        std::thread::sleep(Duration::from_millis(30));
                        let (tx, rx) = std::sync::mpsc::channel();
                        let w_weak2 = w_weak.clone();
                        
                        let invoke_res = slint::invoke_from_event_loop(move || {
                            let mut configured = false;
                            if let Some(w) = w_weak2.upgrade() {
                                let _xid = w.window().with_winit_window(|win| {
                                    crate::x11_window_id(win)
                                }).flatten();

                                let winit_ok = w.window().with_winit_window(|win| {
                                    win.set_decorations(false);
                                    win.set_window_level(WindowLevel::AlwaysOnTop);
                                    let _ = win.request_inner_size(PhysicalSize::new(
                                        (menu_width as f64 * scale) as u32,
                                        (menu_height as f64 * scale) as u32,
                                    ));
                                    win.set_outer_position(PhysicalPosition::new(x, y));
                                }).is_some();

                                #[cfg(target_os = "linux")]
                                if let Some(id) = _xid {
                                    if let Ok(xwin) = clawdpanel_platform_shell::X11Window::new(id) {
                                        xwin.apply_menu_styles();
                                        xwin.move_to(x, y);
                                        eprintln!("[menu] Deferred X11 menu window type and position (x={}, y={}) applied via x11rb (attempt {})", x, y, _attempt);
                                    }
                                }
                                configured = winit_ok;
                            }
                            let _ = tx.send(configured);
                        });
                        
                        if invoke_res.is_ok() {
                            if rx.recv().unwrap_or(false) {
                                // Successful configuration!
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                });
            }

            state.shown_at.set(Instant::now());
            state.visible.set(true);

            // Start focus loss timer
            let timer = slint::Timer::default();
            let state_weak = Rc::downgrade(state);
            let _mon_clone = mon.clone();
            timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(100),
                move || {
                    let Some(state) = state_weak.upgrade() else { return };
                    if !state.visible.get() { return; }
                    
                    // Don't auto-close immediately to allow settling
                    if state.shown_at.get().elapsed() < Duration::from_millis(300) {
                        return;
                    }
                    
                    // Query current cursor position to see if it's over the menu or the brand button (Linux/X11 only)
                    #[cfg(target_os = "linux")]
                    let mut cursor_over_menu_or_button = false;
                    #[cfg(target_os = "linux")]
                    if let Some(w) = state.window.borrow().as_ref() {
                        let xid = w.window().with_winit_window(|win| crate::x11_window_id(win)).flatten();
                        if let Some(id) = xid {
                            if let Ok(xwin) = clawdpanel_platform_shell::X11Window::new(id) {
                                let (cx, cy) = xwin.cursor_pos();
                                // Check if cursor is over menu bounds
                                if cx >= x && cx < x + (menu_width as f64 * scale) as i32
                                    && cy >= y && cy < y + (menu_height as f64 * scale) as i32
                                {
                                    cursor_over_menu_or_button = true;
                                }
                                
                                // Check if cursor is over the brand button bounds (anchor)
                                let mon_bottom = _mon_clone.top + _mon_clone.height;
                                let y_bar = if _mon_clone.dock_edge == "bottom" {
                                    mon_bottom - (bar_height as f64 * scale) as i32
                                } else {
                                    _mon_clone.top + (_mon_clone.work_top_offset as f64 * scale) as i32
                                };
                                let btn_x_min = _mon_clone.left;
                                let btn_x_max = _mon_clone.left + (23.0 * scale) as i32;
                                let btn_y_min = y_bar;
                                let btn_y_max = y_bar + (bar_height as f64 * scale) as i32;
                                
                                if cx >= btn_x_min && cx < btn_x_max
                                    && cy >= btn_y_min && cy < btn_y_max
                                {
                                    cursor_over_menu_or_button = true;
                                }
                            }
                        }
                    }
                    
                    #[cfg(not(target_os = "linux"))]
                    let cursor_over_menu_or_button = false;
                    
                    let has_focus = state.window.borrow().as_ref()
                        .and_then(|w| w.window().with_winit_window(|win| win.has_focus()))
                        .unwrap_or(true);
                        
                    if !has_focus && !cursor_over_menu_or_button {
                        hide_brand_menu(&state);
                    }
                }
            );
            *state.focus_timer.borrow_mut() = Some(timer);
        }
    });
}

/// Hides the brand menu window.
pub fn hide_brand_menu(state: &Rc<MenuState>) {
    if !state.visible.get() {
        return;
    }
    state.visible.set(false);
    state.last_hidden_at.set(Instant::now());
    if let Some(timer) = state.focus_timer.borrow_mut().take() {
        timer.stop();
    }
    if let Some(w) = state.window.borrow().as_ref() {
        let _ = w.hide();
    }
}

/// Syncs the theme index to the brand menu window if it is currently instantiated.
pub fn sync_theme(index: i32) {
    MENU_STATE.with(|state| {
        if let Some(w) = state.window.borrow().as_ref() {
            w.global::<Theme>().set_index(index);
        }
    });
}

fn build_window(_ui: &Rc<UiState>, state: &Rc<MenuState>) -> Result<BrandMenuWindow, slint::PlatformError> {
    let w = BrandMenuWindow::new()?;
    let m = w.global::<Menu>();

    // Exit handler
    {
        let state = state.clone();
        m.on_quit(move || {
            hide_brand_menu(&state);
            let _ = slint::quit_event_loop();
        });
    }

    // Escape/Close handler
    {
        let state = state.clone();
        m.on_close(move || {
            hide_brand_menu(&state);
        });
    }

    // Check updates handler
    {
        let w_weak = w.as_weak();
        m.on_check_updates(move || {
            let Some(w) = w_weak.upgrade() else { return };
            let m = w.global::<Menu>();
            m.set_check_state("busy".into());
            m.set_hint("CHECKING…".into());
            m.set_hint_kind("busy".into());

            let w_weak2 = w_weak.clone();
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("[menu] runtime build failed: {e}");
                        return;
                    }
                };

                let version = match option_env!("CLAWDPANEL_VERSION") {
                    Some(v) => v,
                    None => "dev",
                };

                let check = rt.block_on(clawdpanel_platform_shell::updater::check_for_updates(version));

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = w_weak2.upgrade() {
                        let m = w.global::<Menu>();
                        m.set_check_state("idle".into());
                        if !check.error.is_empty() {
                            m.set_hint(check.error.to_uppercase().into());
                            m.set_hint_kind("error".into());
                        } else if check.update_available {
                            m.set_hint(format!("v{} AVAILABLE", check.latest).into());
                            m.set_hint_kind("update".into());
                            crate::updater::show_update_window(check.clone());
                        } else {
                            m.set_check_state("done".into());
                            m.set_hint("".into());
                        }
                    }
                });
            });
        });
    }

    Ok(w)
}
