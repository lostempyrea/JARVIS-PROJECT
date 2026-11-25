use dioxus::desktop::tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
#[cfg(target_os = "windows")]
use dioxus::desktop::tao::platform::windows::WindowExtWindows;
use dioxus::desktop::DesktopService;
use jarvis_core::AppState;

pub const ORB_WIDTH: f64 = 120.0;
pub const ORB_HEIGHT: f64 = 120.0;
pub const POPUP_WIDTH: f64 = 420.0;
pub const POPUP_HEIGHT: f64 = 580.0;
pub const DASHBOARD_WIDTH: f64 = 1180.0;
pub const DASHBOARD_HEIGHT: f64 = 760.0;

pub const POPUP_GAP: f64 = 10.0;
pub const SCREEN_MARGIN: f64 = 12.0;
pub const DRAG_THRESHOLD_PX: f64 = 4.0;

/// Active desktop window layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowMode {
    #[default]
    StandbyOrb,
    StandbyPopup,
    ExpandedDashboard,
}

/// Logical monitor bounding box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectF {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl RectF {
    pub fn right(self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(self) -> f64 {
        self.y + self.h
    }
}

/// Tracks pointer-down coordinates to distinguish clicks/double-clicks from drags.
///
/// Calling `window.drag()` inside `onmousedown` swallows `onclick`/`ondblclick` on
/// Windows `wry`. Instead, we only initiate `desktop.drag()` once pointer movement
/// exceeds [`DRAG_THRESHOLD_PX`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PointerDragTracker {
    down_pos: Option<(f64, f64)>,
    drag_active: bool,
    suppress_next_click: bool,
}

impl PointerDragTracker {
    pub fn on_mouse_down(&mut self, x: f64, y: f64) {
        self.down_pos = Some((x, y));
        self.drag_active = false;
        self.suppress_next_click = false;
    }

    /// Returns `true` once when the pointer moves beyond [`DRAG_THRESHOLD_PX`].
    pub fn on_mouse_move(&mut self, x: f64, y: f64) -> bool {
        let Some((sx, sy)) = self.down_pos else {
            return false;
        };
        if self.drag_active {
            return false;
        }
        let dx = x - sx;
        let dy = y - sy;
        if (dx * dx + dy * dy).sqrt() > DRAG_THRESHOLD_PX {
            self.drag_active = true;
            self.suppress_next_click = true;
            return true;
        }
        false
    }

    pub fn on_mouse_up(&mut self) {
        self.down_pos = None;
        self.drag_active = false;
    }

    /// Consumes whether the last interaction was a drag (and should ignore `onclick`).
    pub fn take_suppress_click(&mut self) -> bool {
        let suppress = self.suppress_next_click;
        self.suppress_next_click = false;
        self.down_pos = None;
        self.drag_active = false;
        suppress
    }
}

/// Effective logical `(width, height)` for [`WindowMode::StandbyPopup`], clamped to monitor bounds.
pub fn effective_popup_size(bounds: RectF) -> (f64, f64) {
    let w = (bounds.w - 2.0 * SCREEN_MARGIN).clamp(320.0, POPUP_WIDTH);
    let h = (bounds.h - 2.0 * SCREEN_MARGIN).clamp(380.0, POPUP_HEIGHT);
    (w, h)
}

/// Effective logical `(width, height)` for [`WindowMode::ExpandedDashboard`], clamped so HiDPI
/// screens never overflow.
pub fn effective_dashboard_size(bounds: RectF) -> (f64, f64) {
    let w = (bounds.w - 40.0).clamp(640.0, DASHBOARD_WIDTH);
    let h = (bounds.h - 56.0).clamp(480.0, DASHBOARD_HEIGHT);
    (w, h)
}

/// Compute a screen-safe `(x, y)` logical coordinate for [`WindowMode::StandbyPopup`]
/// anchored next to the Orb at `(orb_x, orb_y)`, flipping directions so it never
/// overflows `monitor_bounds`.
pub fn compute_popup_position(orb_x: f64, orb_y: f64, monitor_bounds: RectF) -> (f64, f64) {
    let (popup_w, popup_h) = effective_popup_size(monitor_bounds);
    let min_x = monitor_bounds.x + SCREEN_MARGIN;
    let max_x = (monitor_bounds.right() - popup_w - SCREEN_MARGIN).max(min_x);
    let min_y = monitor_bounds.y + SCREEN_MARGIN;
    let max_y = (monitor_bounds.bottom() - popup_h - SCREEN_MARGIN).max(min_y);

    // Prefer opening above the orb (typical bottom-right placement), flip below if needed.
    let above_y = orb_y - popup_h - POPUP_GAP;
    let below_y = orb_y + ORB_HEIGHT + POPUP_GAP;
    let y = if above_y >= min_y {
        above_y
    } else if below_y + popup_h <= monitor_bounds.bottom() - SCREEN_MARGIN {
        below_y
    } else {
        orb_y.clamp(min_y, max_y)
    };

    // Align right edge with orb if there is room to the left; otherwise anchor left edge to orb.
    let align_right_x = orb_x + ORB_WIDTH - popup_w;
    let align_left_x = orb_x;
    let x = if align_right_x >= min_x {
        align_right_x.min(max_x)
    } else if align_left_x + popup_w <= monitor_bounds.right() - SCREEN_MARGIN {
        align_left_x.max(min_x)
    } else {
        align_left_x.clamp(min_x, max_x)
    };

    (x, y)
}

/// Compute centered logical coordinates for [`WindowMode::ExpandedDashboard`].
pub fn compute_dashboard_position(monitor_bounds: RectF) -> (f64, f64) {
    let (dash_w, dash_h) = effective_dashboard_size(monitor_bounds);
    let min_x = monitor_bounds.x + SCREEN_MARGIN;
    let min_y = monitor_bounds.y + SCREEN_MARGIN;
    let x = (monitor_bounds.x + (monitor_bounds.w - dash_w) / 2.0).max(min_x);
    let y = (monitor_bounds.y + (monitor_bounds.h - dash_h) / 2.0).max(min_y);
    (x, y)
}

/// Compute default bottom-right Orb position within `monitor_bounds`.
pub fn default_orb_position(monitor_bounds: RectF) -> (f64, f64) {
    let x = (monitor_bounds.right() - ORB_WIDTH - 28.0).max(monitor_bounds.x + SCREEN_MARGIN);
    let y = (monitor_bounds.bottom() - ORB_HEIGHT - 64.0).max(monitor_bounds.y + SCREEN_MARGIN);
    (x, y)
}

/// Read the monitor's Win32 WorkingArea (`rcWork`, excluding the taskbar) in physical pixels.
#[cfg(target_os = "windows")]
fn win32_monitor_work_area(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct MonitorInfo {
        cb_size: u32,
        rc_monitor: Rect,
        rc_work: Rect,
        dw_flags: u32,
    }

    const MONITOR_DEFAULTTONEAREST: u32 = 2;

    #[link(name = "user32")]
    extern "system" {
        fn MonitorFromWindow(hwnd: isize, dw_flags: u32) -> isize;
        fn GetMonitorInfoW(hmonitor: isize, lpmi: *mut MonitorInfo) -> i32;
    }

    if hwnd == 0 {
        return None;
    }

    // SAFETY: Standard Win32 FFI with initialized MONITORINFO struct and valid cb_size.
    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if hmon == 0 {
            return None;
        }
        let mut info = MonitorInfo {
            cb_size: std::mem::size_of::<MonitorInfo>() as u32,
            rc_monitor: Rect::default(),
            rc_work: Rect::default(),
            dw_flags: 0,
        };
        if GetMonitorInfoW(hmon, &mut info) == 0 {
            return None;
        }
        let w = info.rc_work.right - info.rc_work.left;
        let h = info.rc_work.bottom - info.rc_work.top;
        if w <= 0 || h <= 0 {
            return None;
        }
        Some((info.rc_work.left, info.rc_work.top, w, h))
    }
}

/// Query current monitor logical bounds (work area excluding OS taskbar) from a Dioxus desktop window.
pub fn current_monitor_bounds(desktop: &DesktopService) -> RectF {
    let monitor = desktop
        .window
        .current_monitor()
        .or_else(|| desktop.window.primary_monitor());
    let scale = monitor
        .as_ref()
        .map(|m| m.scale_factor().max(1.0))
        .unwrap_or_else(|| desktop.window.scale_factor().max(1.0));

    #[cfg(target_os = "windows")]
    if let Some((wx, wy, ww, wh)) = win32_monitor_work_area(desktop.window.hwnd()) {
        return RectF {
            x: wx as f64 / scale,
            y: wy as f64 / scale,
            w: ww as f64 / scale,
            h: wh as f64 / scale,
        };
    }

    if let Some(m) = monitor {
        let pos = m.position().to_logical::<f64>(scale);
        let size = m.size().to_logical::<f64>(scale);
        #[cfg(target_os = "windows")]
        let h = (size.height - 48.0).max(400.0);
        #[cfg(not(target_os = "windows"))]
        let h = size.height;
        RectF {
            x: pos.x,
            y: pos.y,
            w: size.width,
            h,
        }
    } else {
        #[cfg(target_os = "windows")]
        let h = 1080.0 - 48.0;
        #[cfg(not(target_os = "windows"))]
        let h = 1080.0;
        RectF {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h,
        }
    }
}

/// Read the window's current logical outer position.
pub fn current_logical_position(desktop: &DesktopService) -> Option<(f64, f64)> {
    let scale = desktop.window.scale_factor().max(1.0);
    desktop
        .window
        .outer_position()
        .ok()
        .map(|pos| {
            let logical = pos.to_logical::<f64>(scale);
            (logical.x, logical.y)
        })
}

/// Immediately synchronize the WebView2/wry child controller bounds to match `(logical_w, logical_h)`.
pub fn sync_webview_to_window(desktop: &DesktopService, logical_w: f64, logical_h: f64) {
    let scale = desktop.window.scale_factor().max(1.0);
    let phys_w = (logical_w * scale).round().max(1.0) as u32;
    let phys_h = (logical_h * scale).round().max(1.0) as u32;
    let _ = desktop.webview.set_bounds(dioxus::desktop::wry::Rect {
        position: dioxus::desktop::wry::dpi::Position::Physical(
            dioxus::desktop::wry::dpi::PhysicalPosition::new(0, 0),
        ),
        size: dioxus::desktop::wry::dpi::Size::Physical(
            dioxus::desktop::wry::dpi::PhysicalSize::new(phys_w, phys_h),
        ),
    });
}

/// Synchronize the WebView2/wry bounds to the effective size of `mode`.
pub fn sync_webview_to_mode(desktop: &DesktopService, mode: WindowMode) {
    let bounds = current_monitor_bounds(desktop);
    let (w, h) = match mode {
        WindowMode::StandbyOrb => (ORB_WIDTH, ORB_HEIGHT),
        WindowMode::StandbyPopup => effective_popup_size(bounds),
        WindowMode::ExpandedDashboard => effective_dashboard_size(bounds),
    };
    sync_webview_to_window(desktop, w, h);
}

/// Resize and reposition the Win32/Tao window and immediately sync the WebView2 bounds.
pub fn resize_window_and_webview(
    desktop: &DesktopService,
    logical_w: f64,
    logical_h: f64,
    logical_x: f64,
    logical_y: f64,
) {
    let win = &desktop.window;
    #[cfg(target_os = "windows")]
    {
        win.set_undecorated_shadow(false);
    }

    // Clear min/max constraints before resizing so WM_GETMINMAXINFO never blocks shrinking or growing.
    win.set_min_inner_size(None::<LogicalSize<f64>>);
    win.set_max_inner_size(None::<LogicalSize<f64>>);

    let scale = win.scale_factor().max(1.0);
    let phys_w = (logical_w * scale).round().max(1.0) as u32;
    let phys_h = (logical_h * scale).round().max(1.0) as u32;
    let phys_x = (logical_x * scale).round() as i32;
    let phys_y = (logical_y * scale).round() as i32;

    win.set_inner_size(PhysicalSize::new(phys_w, phys_h));
    win.set_outer_position(PhysicalPosition::new(phys_x, phys_y));

    let _ = desktop.webview.set_bounds(dioxus::desktop::wry::Rect {
        position: dioxus::desktop::wry::dpi::Position::Physical(
            dioxus::desktop::wry::dpi::PhysicalPosition::new(0, 0),
        ),
        size: dioxus::desktop::wry::dpi::Size::Physical(
            dioxus::desktop::wry::dpi::PhysicalSize::new(phys_w, phys_h),
        ),
    });
}

/// Transition the Dioxus desktop window between `StandbyOrb`, `StandbyPopup`,
/// and `ExpandedDashboard`, preserving and restoring the Orb's dragged position.
pub fn apply_window_mode(
    desktop: &DesktopService,
    previous_mode: WindowMode,
    next_mode: WindowMode,
    orb_pos: &mut Option<(f64, f64)>,
) {
    if previous_mode == WindowMode::StandbyOrb && next_mode != WindowMode::StandbyOrb {
        if let Some(pos) = current_logical_position(desktop) {
            *orb_pos = Some(pos);
        }
    }

    let bounds = current_monitor_bounds(desktop);
    let anchor = orb_pos.unwrap_or_else(|| default_orb_position(bounds));
    if orb_pos.is_none() {
        *orb_pos = Some(anchor);
    }

    let win = &desktop.window;
    match next_mode {
        WindowMode::StandbyOrb => {
            win.set_decorations(false);
            win.set_resizable(false);
            win.set_always_on_top(true);
            let (ox, oy) = (
                anchor.0.clamp(
                    bounds.x + SCREEN_MARGIN,
                    (bounds.right() - ORB_WIDTH - SCREEN_MARGIN).max(bounds.x + SCREEN_MARGIN),
                ),
                anchor.1.clamp(
                    bounds.y + SCREEN_MARGIN,
                    (bounds.bottom() - ORB_HEIGHT - SCREEN_MARGIN).max(bounds.y + SCREEN_MARGIN),
                ),
            );
            resize_window_and_webview(desktop, ORB_WIDTH, ORB_HEIGHT, ox, oy);
        }
        WindowMode::StandbyPopup => {
            win.set_decorations(false);
            win.set_resizable(false);
            win.set_always_on_top(true);
            let (pw, ph) = effective_popup_size(bounds);
            let (px, py) = compute_popup_position(anchor.0, anchor.1, bounds);
            resize_window_and_webview(desktop, pw, ph, px, py);
            win.set_focus();
        }
        WindowMode::ExpandedDashboard => {
            win.set_decorations(false);
            win.set_always_on_top(false);
            win.set_resizable(true);
            let (dw, dh) = effective_dashboard_size(bounds);
            let (dx, dy) = compute_dashboard_position(bounds);
            resize_window_and_webview(desktop, dw, dh, dx, dy);
            win.set_min_inner_size(Some(LogicalSize::new(
                760.0_f64.min(dw),
                500.0_f64.min(dh),
            )));
            win.set_focus();
        }
    }
}

/// Show and focus the HUD window.
pub fn show_and_focus(desktop: &DesktopService) {
    desktop.window.set_visible(true);
    desktop.window.set_minimized(false);
    desktop.window.set_focus();
}

/// Persist the Orb position into `AppConfig` (`ui.orb_x`, `ui.orb_y`).
pub fn persist_orb_position(pos: (f64, f64), state: &AppState) {
    if let Ok(mut cfg) = state.config.write() {
        cfg.ui.orb_x = Some(pos.0);
        cfg.ui.orb_y = Some(pos.1);
        if let Err(err) = cfg.save() {
            tracing::warn!("Failed to persist orb position: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: RectF = RectF {
        x: 0.0,
        y: 0.0,
        w: 1920.0,
        h: 1080.0,
    };

    fn assert_inside_screen(px: f64, py: f64, bounds: RectF) {
        let (pw, ph) = effective_popup_size(bounds);
        assert!(px >= bounds.x + SCREEN_MARGIN - 0.01, "x={px} overflowed left");
        assert!(
            px + pw <= bounds.right() - SCREEN_MARGIN + 0.01,
            "x={px} overflowed right"
        );
        assert!(py >= bounds.y + SCREEN_MARGIN - 0.01, "y={py} overflowed top");
        assert!(
            py + ph <= bounds.bottom() - SCREEN_MARGIN + 0.01,
            "y={py} overflowed bottom"
        );
    }

    #[test]
    fn popup_flips_cleanly_in_all_four_corners() {
        let right_orb_x = SCREEN.right() - SCREEN_MARGIN - ORB_WIDTH;
        let bottom_orb_y = SCREEN.bottom() - SCREEN_MARGIN - ORB_HEIGHT;

        // Top-left
        let (x, y) = compute_popup_position(12.0, 12.0, SCREEN);
        assert_inside_screen(x, y, SCREEN);
        assert!(y >= 12.0 + ORB_HEIGHT); // Opens below

        // Top-right
        let (x, y) = compute_popup_position(right_orb_x, 12.0, SCREEN);
        assert_inside_screen(x, y, SCREEN);
        assert!(y >= 12.0 + ORB_HEIGHT);

        // Bottom-left
        let (x, y) = compute_popup_position(12.0, bottom_orb_y, SCREEN);
        assert_inside_screen(x, y, SCREEN);
        assert!(y + POPUP_HEIGHT <= bottom_orb_y); // Opens above

        // Bottom-right
        let (x, y) = compute_popup_position(right_orb_x, bottom_orb_y, SCREEN);
        assert_inside_screen(x, y, SCREEN);
        assert!(y + POPUP_HEIGHT <= bottom_orb_y);
    }

    #[test]
    fn hidpi_1280x720_clamps_dashboard_and_popup_sizes() {
        let hidpi = RectF {
            x: 0.0,
            y: 0.0,
            w: 1280.0,
            h: 720.0,
        };
        let (dw, dh) = effective_dashboard_size(hidpi);
        assert_eq!(dw, DASHBOARD_WIDTH);
        assert_eq!(dh, 720.0 - 56.0);
        assert!(dw <= hidpi.w - 40.0);
        assert!(dh <= hidpi.h - 56.0);

        let (pw, ph) = effective_popup_size(hidpi);
        assert_eq!(pw, POPUP_WIDTH);
        assert_eq!(ph, POPUP_HEIGHT);

        let compact = RectF {
            x: 0.0,
            y: 0.0,
            w: 960.0,
            h: 540.0,
        };
        assert_eq!(effective_dashboard_size(compact), (920.0, 484.0));
        assert_eq!(effective_popup_size(compact), (420.0, 516.0));
    }

    #[test]
    fn popup_stays_inside_work_area_excluding_taskbar() {
        let work_area = RectF {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1032.0,
        };
        let (px, py) = compute_popup_position(1608.0, 437.0, work_area);
        let (pw, ph) = effective_popup_size(work_area);
        assert!(px >= 12.0 && px + pw <= 1908.0, "x={px}, right={}", px + pw);
        assert!(py >= 12.0 && py + ph <= 1020.0, "y={py}, bottom={}", py + ph);
        assert_inside_screen(px, py, work_area);
    }

    #[test]
    fn pointer_drag_tracker_separates_click_from_drag() {
        let mut tracker = PointerDragTracker::default();

        // Sub-threshold jitter -> treated as click
        tracker.on_mouse_down(40.0, 40.0);
        assert!(!tracker.on_mouse_move(42.0, 42.0));
        tracker.on_mouse_up();
        assert!(!tracker.take_suppress_click());

        // Above-threshold movement -> triggers drag and suppresses click
        tracker.on_mouse_down(40.0, 40.0);
        assert!(tracker.on_mouse_move(46.0, 41.0));
        // Only fires drag trigger once per press
        assert!(!tracker.on_mouse_move(60.0, 60.0));
        tracker.on_mouse_up();
        assert!(tracker.take_suppress_click());
    }
}
