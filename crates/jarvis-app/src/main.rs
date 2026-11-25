#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod components;
mod window_manager;

use crate::components::arc_reactor::{reactor_state_label, ArcReactorOrb, StandbyOrbView};
use crate::components::chat_hud::ChatHudView;
use crate::components::comms_matrix::CommsMatrixView;
use crate::components::icon_rail::IconRail;
use crate::components::music_deck::MusicDeckView;
use crate::components::settings_hud::SettingsHudView;
use crate::components::standby_popup::StandbyPopupView;
use crate::components::terminal_view::TerminalView;
use crate::components::voice_panel::VoicePanelView;
use crate::components::{
    svg_collapse, svg_minimize, svg_orb, DashboardTab, HudUiState, JarvisUiContext,
};
use crate::window_manager::{
    apply_window_mode, show_and_focus, PointerDragTracker, WindowMode, DASHBOARD_HEIGHT,
    DASHBOARD_WIDTH, ORB_HEIGHT, ORB_WIDTH, POPUP_HEIGHT, POPUP_WIDTH,
};
use dioxus::desktop::tao::dpi::{LogicalPosition, LogicalSize};
#[cfg(target_os = "windows")]
use dioxus::desktop::tao::platform::windows::WindowBuilderExtWindows;
use dioxus::desktop::{
    use_tray_icon_event_handler, use_tray_menu_event_handler, use_window, Config, WindowBuilder,
    WindowCloseBehaviour,
};
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use jarvis_core::{AppState, CoreEvent, ReactorState, UiCommand};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon as TrayIconImage, MouseButton as TrayMouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

const HUD_CSS: &str = include_str!("../assets/hud.css");

const TRAY_ID_TOGGLE: &str = "jarvis_tray_toggle";
const TRAY_ID_DASHBOARD: &str = "jarvis_tray_dashboard";
const TRAY_ID_ORB: &str = "jarvis_tray_orb";
const TRAY_ID_QUIT: &str = "jarvis_tray_quit";

#[derive(Clone)]
struct AppLaunchSeed {
    core: Arc<AppState>,
    start_mode: WindowMode,
}

fn parse_env_window_mode(var_name: &str) -> Option<WindowMode> {
    let val = std::env::var(var_name).ok()?;
    match val.trim().to_ascii_lowercase().as_str() {
        "orb" | "standby_orb" => Some(WindowMode::StandbyOrb),
        "popup" | "standby_popup" => Some(WindowMode::StandbyPopup),
        "dashboard" | "expanded_dashboard" => Some(WindowMode::ExpandedDashboard),
        _ => None,
    }
}

fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();

    let core = match AppState::init_default() {
        Ok(state) => Arc::new(state),
        Err(err) => {
            tracing::error!("Failed to initialize JARVIS core state: {err}");
            eprintln!("Failed to initialize JARVIS core state: {err}");
            std::process::exit(1);
        }
    };

    let (orb_x, orb_y) = core
        .config
        .read()
        .map(|c| (c.ui.orb_x, c.ui.orb_y))
        .unwrap_or((None, None));

    let start_mode = parse_env_window_mode("JARVIS_START_MODE").unwrap_or(WindowMode::StandbyOrb);
    let (init_w, init_h) = match start_mode {
        WindowMode::StandbyOrb => (ORB_WIDTH, ORB_HEIGHT),
        WindowMode::StandbyPopup => (POPUP_WIDTH, POPUP_HEIGHT),
        WindowMode::ExpandedDashboard => (DASHBOARD_WIDTH, DASHBOARD_HEIGHT),
    };

    let rgba64 = generate_arc_reactor_rgba(64);
    let window_icon = dioxus::desktop::tao::window::Icon::from_rgba(rgba64, 64, 64).ok();

    let mut win_builder = WindowBuilder::new()
        .with_title("JARVIS-RS")
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top(start_mode != WindowMode::ExpandedDashboard)
        .with_resizable(start_mode == WindowMode::ExpandedDashboard)
        .with_inner_size(LogicalSize::new(init_w, init_h))
        .with_window_icon(window_icon);

    #[cfg(target_os = "windows")]
    {
        win_builder = win_builder.with_undecorated_shadow(false);
    }

    if start_mode == WindowMode::StandbyOrb {
        if let (Some(x), Some(y)) = (orb_x, orb_y) {
            win_builder = win_builder.with_position(LogicalPosition::new(x, y));
        }
    }

    let desktop_cfg = Config::new()
        .with_window(win_builder)
        .with_background_color((0, 0, 0, 0))
        .with_disable_context_menu(true)
        .with_close_behaviour(WindowCloseBehaviour::LastWindowHides);

    let seed = AppLaunchSeed { core, start_mode };

    dioxus::LaunchBuilder::desktop()
        .with_context(seed)
        .with_cfg(desktop_cfg)
        .launch(RootHudApp);
}

#[component]
fn RootHudApp() -> Element {
    let seed = use_context::<AppLaunchSeed>();
    let core = seed.core.clone();
    let start_mode = seed.start_mode;
    let desktop = use_window();

    let initial_session = use_hook(|| {
        let existing = core.db.list_sessions(1).ok().and_then(|mut v| v.pop());
        if let Some(sess) = existing {
            sess.id
        } else {
            let sid = "hud-main".to_string();
            let _ = core.db.ensure_session(&sid, "Primary HUD", "hud");
            sid
        }
    });

    let initial_orb_pos = core
        .config
        .read()
        .ok()
        .and_then(|c| c.ui.orb_x.zip(c.ui.orb_y));

    let window_mode = use_signal(|| start_mode);
    let active_tab = use_signal(DashboardTab::default);
    let active_session_id = use_signal(|| initial_session.clone());
    let orb_pos = use_signal(|| initial_orb_pos);
    let hud_visible = use_signal(|| true);

    let cmd_tx = use_hook(|| {
        let (tx, rx) = mpsc::unbounded_channel::<UiCommand>();
        spawn_core_command_loop(core.clone(), rx);
        spawn_background_services(core.clone(), initial_session.clone());
        tx
    });
    // Sync initial UI state after spawn_background_services so voice_running reflects startup.
    let ui_state = use_signal(|| HudUiState::from_core(&core, &initial_session));

    let ctx = JarvisUiContext {
        core: core.clone(),
        cmd_tx,
        window_mode,
        active_tab,
        active_session_id,
        orb_pos,
        hud_visible,
        ui_state,
    };
    use_context_provider(|| ctx.clone());

    // Sync initial window bounds, bottom-right placement, and WebView2 controller immediately on mount.
    let init_done = use_hook(|| Rc::new(std::cell::Cell::new(false)));
    let mut ctx_init = ctx.clone();
    let desktop_init = desktop.clone();
    use_effect(move || {
        if init_done.get() {
            return;
        }
        init_done.set(true);
        let mut pos = *ctx_init.orb_pos.peek();
        apply_window_mode(&desktop_init, start_mode, start_mode, &mut pos);
        if pos != *ctx_init.orb_pos.peek() {
            ctx_init.orb_pos.set(pos);
        }
    });

    // Optional automated mode transition hook for Win32 empirical testing.
    let test_switch_to = use_hook(|| parse_env_window_mode("JARVIS_TEST_SWITCH_TO"));
    let ctx_test = ctx.clone();
    let desktop_test = desktop.clone();
    use_future(move || {
        let mut ctx_test = ctx_test.clone();
        let desktop_test = desktop_test.clone();
        async move {
            if let Some(target_mode) = test_switch_to {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                ctx_test.switch_mode(&desktop_test, target_mode);
            }
        }
    });

    // Keep system tray icon & global hotkey manager alive on the Win32 UI thread.
    let _tray_guard: Rc<Option<TrayIcon>> = use_hook(|| Rc::new(setup_tray_icon()));
    let hotkey_rx = use_hook(|| {
        let (tx, rx) = mpsc::unbounded_channel::<()>();
        let manager = setup_global_hotkey(tx);
        Rc::new((manager, std::cell::RefCell::new(Some(rx))))
    });

    // Subscribe to CoreEvent broadcast stream.
    let core_events = core.clone();
    let mut ui_sig_events = ui_state;
    let session_sig_events = active_session_id;
    use_future(move || {
        let core = core_events.clone();
        async move {
            let mut rx = core.event_bus.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let sid = session_sig_events.read().clone();
                        ui_sig_events.write().apply_core_event(ev, &sid);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        ui_sig_events.write().reconcile_approvals(&core);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    // Global Hotkey (Alt+Space) handler loop.
    let ctx_hotkey = ctx.clone();
    let desktop_hotkey = desktop.clone();
    use_future(move || {
        let maybe_rx = hotkey_rx.1.borrow_mut().take();
        let desktop = desktop_hotkey.clone();
        let mut ctx_hotkey = ctx_hotkey.clone();
        async move {
            let Some(mut rx) = maybe_rx else {
                return;
            };
            while rx.recv().await.is_some() {
                let visible = *ctx_hotkey.hud_visible.read();
                let mode = *ctx_hotkey.window_mode.read();
                if !visible {
                    ctx_hotkey.hud_visible.set(true);
                    show_and_focus(&desktop);
                    if mode == WindowMode::StandbyOrb {
                        ctx_hotkey.switch_mode(&desktop, WindowMode::StandbyPopup);
                    }
                } else {
                    match mode {
                        WindowMode::StandbyOrb => {
                            show_and_focus(&desktop);
                            ctx_hotkey.switch_mode(&desktop, WindowMode::StandbyPopup);
                        }
                        WindowMode::StandbyPopup => {
                            ctx_hotkey.switch_mode(&desktop, WindowMode::StandbyOrb);
                        }
                        WindowMode::ExpandedDashboard => {
                            show_and_focus(&desktop);
                        }
                    }
                }
            }
        }
    });

    // System Tray Menu Events
    let mut ctx_tray_menu = ctx.clone();
    let desktop_tray_menu = desktop.clone();
    let core_quit = core.clone();
    use_tray_menu_event_handler(move |event| match event.id().0.as_str() {
        TRAY_ID_TOGGLE => {
            let next_vis = !*ctx_tray_menu.hud_visible.read();
            ctx_tray_menu.hud_visible.set(next_vis);
            if next_vis {
                show_and_focus(&desktop_tray_menu);
            } else {
                desktop_tray_menu.window.set_visible(false);
            }
        }
        TRAY_ID_DASHBOARD => {
            ctx_tray_menu.hud_visible.set(true);
            show_and_focus(&desktop_tray_menu);
            ctx_tray_menu.switch_mode(&desktop_tray_menu, WindowMode::ExpandedDashboard);
        }
        TRAY_ID_ORB => {
            ctx_tray_menu.hud_visible.set(true);
            show_and_focus(&desktop_tray_menu);
            ctx_tray_menu.switch_mode(&desktop_tray_menu, WindowMode::StandbyOrb);
        }
        TRAY_ID_QUIT => {
            core_quit.voice.stop();
            core_quit.music.local_player().shutdown();
            let core_stop = core_quit.clone();
            tokio::spawn(async move {
                core_stop.music.local_player().halt_active_stream().await;
                core_stop.channels.stop_all().await;
                let _ = core_stop.ps_console.shutdown().await;
                core_stop.pty.shutdown_all();
                std::process::exit(0);
            });
        }
        _ => {}
    });

    // System Tray Left-Click restores HUD
    let mut ctx_tray_icon = ctx.clone();
    let desktop_tray_icon = desktop.clone();
    use_tray_icon_event_handler(move |event| {
        if let TrayIconEvent::Click {
            button: TrayMouseButton::Left,
            ..
        } = event
        {
            ctx_tray_icon.hud_visible.set(true);
            show_and_focus(&desktop_tray_icon);
            if *ctx_tray_icon.window_mode.read() == WindowMode::StandbyOrb {
                ctx_tray_icon.switch_mode(&desktop_tray_icon, WindowMode::StandbyPopup);
            }
        }
    });

    let mode = *window_mode.read();
    rsx! {
        style { "{HUD_CSS}" }
        match mode {
            WindowMode::StandbyOrb => rsx! { StandbyOrbView {} },
            WindowMode::StandbyPopup => rsx! { StandbyPopupView {} },
            WindowMode::ExpandedDashboard => rsx! { ExpandedDashboardView {} },
        }
    }
}

#[component]
fn ExpandedDashboardView() -> Element {
    let ctx = use_context::<JarvisUiContext>();
    let desktop = use_window();
    let mut drag_tracker = use_signal(PointerDragTracker::default);

    let tab = *ctx.active_tab.read();
    let ui = ctx.ui_state.read();
    let reactor_state = ui.reactor_state;
    let state_label = reactor_state_label(reactor_state);
    let mic_db = Some(ui.mic_db);
    let pending_count = ui.pending_approvals.len();
    drop(ui);

    let desktop_drag = desktop.clone();
    let desktop_popup = desktop.clone();
    let desktop_orb = desktop.clone();
    let desktop_hide = desktop.clone();

    let mut ctx_popup = ctx.clone();
    let mut ctx_orb = ctx.clone();
    let mut ctx_hide = ctx.clone();

    rsx! {
        div { class: "dashboard-shell hud-glass",
            // Custom Borderless HUD Titlebar
            div {
                class: "dash-titlebar",
                onmousedown: move |evt| {
                    if evt.data().trigger_button() == Some(MouseButton::Primary) {
                        let pt = evt.data().screen_coordinates();
                        drag_tracker.write().on_mouse_down(pt.x, pt.y);
                    }
                },
                onmousemove: move |evt| {
                    if evt.data().held_buttons().contains(MouseButton::Primary) {
                        let pt = evt.data().screen_coordinates();
                        if drag_tracker.write().on_mouse_move(pt.x, pt.y) {
                            desktop_drag.drag();
                        }
                    }
                },
                onmouseup: move |_| {
                    drag_tracker.write().on_mouse_up();
                },
                div { class: "dash-brand",
                    ArcReactorOrb { state: reactor_state, size: 26, badge_count: 0, mic_db }
                    span { class: "dash-brand-text", "J.A.R.V.I.S. // COMMAND DECK" }
                    span { class: "dash-status-pill", "{state_label}" }
                    if pending_count > 0 {
                        span { class: "chip", style: "border-color: var(--orange-alert); color: var(--amber);",
                            "{pending_count} Awaiting Approval"
                        }
                    }
                }
                div {
                    class: "row-gap",
                    onmousedown: move |evt| evt.stop_propagation(),
                    button {
                        class: "icon-btn",
                        "data-tooltip": "Compact Popup",
                        "data-tooltip-pos": "bottom-right",
                        onclick: move |_| ctx_popup.switch_mode(&desktop_popup, WindowMode::StandbyPopup),
                        {svg_collapse()}
                    }
                    button {
                        class: "icon-btn",
                        "data-tooltip": "Orb Mode",
                        "data-tooltip-pos": "bottom-right",
                        onclick: move |_| ctx_orb.switch_mode(&desktop_orb, WindowMode::StandbyOrb),
                        {svg_orb()}
                    }
                    button {
                        class: "icon-btn",
                        "data-tooltip": "Hide to Tray",
                        "data-tooltip-pos": "bottom-right",
                        onclick: move |_| {
                            ctx_hide.hud_visible.set(false);
                            desktop_hide.window.set_visible(false);
                        },
                        {svg_minimize()}
                    }
                }
            }

            // Body: Left Icon-Only Rail + Active View
            div { class: "dash-main",
                IconRail {}
                main { class: "dash-content",
                    match tab {
                        DashboardTab::Chat => rsx! { ChatHudView {} },
                        DashboardTab::Terminal => rsx! { TerminalView {} },
                        DashboardTab::Music => rsx! { MusicDeckView {} },
                        DashboardTab::Comms => rsx! { CommsMatrixView {} },
                        DashboardTab::Voice => rsx! { VoicePanelView {} },
                        DashboardTab::Settings => rsx! { SettingsHudView {} },
                    }
                }
            }
        }
    }
}

fn spawn_core_command_loop(core: Arc<AppState>, mut rx: UnboundedReceiver<UiCommand>) {
    tokio::spawn(async move {
        let mut active_turns: HashMap<String, JoinHandle<()>> = HashMap::new();

        while let Some(cmd) = rx.recv().await {
            match cmd {
                UiCommand::SubmitPrompt { session_id, prompt } => {
                    if let Some(prev) = active_turns.remove(&session_id) {
                        prev.abort();
                    }
                    let core_turn = core.clone();
                    let sid = session_id.clone();
                    let handle = tokio::spawn(async move {
                        if let Err(err) = core_turn
                            .agent
                            .handle_user_prompt(&sid, "hud", &prompt)
                            .await
                        {
                            core_turn.event_bus.publish(CoreEvent::Error {
                                session_id: Some(sid),
                                message: err.to_string(),
                            });
                            core_turn.event_bus.publish(CoreEvent::ReactorStateChanged {
                                state: ReactorState::Idle,
                            });
                        }
                    });
                    active_turns.insert(session_id, handle);
                }
                UiCommand::ResolveApproval { id, decision } => {
                    core.approval_broker.resolve(&id, decision);
                }
                UiCommand::CancelSession { session_id } => {
                    if let Some(handle) = active_turns.remove(&session_id) {
                        handle.abort();
                    }
                    core.event_bus.publish(CoreEvent::ReactorStateChanged {
                        state: ReactorState::Idle,
                    });
                }
                UiCommand::SetSecurityMode { mode } => {
                    if let Ok(mut cfg) = core.config.write() {
                        cfg.security_mode = mode;
                        let _ = cfg.save();
                    }
                }
                UiCommand::SetProvider { provider, model } => {
                    if let Ok(mut cfg) = core.config.write() {
                        cfg.active_provider = provider;
                        cfg.active_model = model;
                        let _ = cfg.save();
                    }
                }
            }
        }
    });
}

fn spawn_background_services(core: Arc<AppState>, default_session_id: String) {
    let voice_enabled = core
        .config
        .read()
        .map(|c| c.voice.wake_word_enabled)
        .unwrap_or(false);
    if voice_enabled {
        if let Err(err) = core.voice.start(default_session_id) {
            tracing::warn!("Voice pipeline did not auto-start: {err}");
        }
    }

    tokio::spawn(async move {
        let outcomes = core.channels.start_all().await;
        for (channel, res) in outcomes {
            if let Err(err) = res {
                tracing::warn!("Channel '{channel}' startup warning: {err}");
            }
        }
    });
}

fn setup_tray_icon() -> Option<TrayIcon> {
    let menu = Menu::new();
    let toggle_item = MenuItem::with_id(TRAY_ID_TOGGLE, "Show / Hide HUD", true, None);
    let dash_item = MenuItem::with_id(TRAY_ID_DASHBOARD, "Expanded Dashboard", true, None);
    let orb_item = MenuItem::with_id(TRAY_ID_ORB, "Standby Orb", true, None);
    let quit_item = MenuItem::with_id(TRAY_ID_QUIT, "Quit", true, None);

    if let Err(err) = menu.append_items(&[
        &toggle_item,
        &dash_item,
        &orb_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]) {
        tracing::warn!("Failed to build tray menu: {err}");
        return None;
    }

    let rgba = generate_arc_reactor_rgba(32);
    let icon = match TrayIconImage::from_rgba(rgba, 32, 32) {
        Ok(icon) => icon,
        Err(err) => {
            tracing::warn!("Failed to create tray RGBA icon: {err}");
            return None;
        }
    };

    match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("JARVIS-RS HUD (Alt+Space)")
        .with_icon(icon)
        .build()
    {
        Ok(tray) => Some(tray),
        Err(err) => {
            tracing::warn!("Failed to initialize system tray icon: {err}");
            None
        }
    }
}

fn setup_global_hotkey(tx: UnboundedSender<()>) -> Option<GlobalHotKeyManager> {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!("GlobalHotKeyManager unavailable: {err}");
            return None;
        }
    };

    let hotkey = HotKey::new(Some(Modifiers::ALT), Code::Space);
    let target_id = hotkey.id();
    if let Err(err) = manager.register(hotkey) {
        tracing::warn!("Could not register global hotkey Alt+Space (may already be in use): {err}");
        return Some(manager);
    }

    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.id == target_id && event.state == HotKeyState::Pressed {
            let _ = tx.send(());
        }
    }));

    Some(manager)
}

/// Programmatically render a crisp 32x32 RGBA Sci-Fi Arc Reactor icon in pure Rust
/// so no external `.ico` asset is required.
pub fn generate_arc_reactor_rgba(size: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.46;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let norm = dist / radius;
            let idx = ((y * size + x) * 4) as usize;

            if norm <= 1.0 {
                // Base dark navy glass disc
                let (mut r, mut g, mut b, mut a) = (6u8, 12u8, 22u8, 240u8);

                // Outer cyan ring
                if (0.76..=0.94).contains(&norm) {
                    (r, g, b, a) = (0, 220, 255, 255);
                } else if (0.48..=0.62).contains(&norm) {
                    // Middle ring
                    (r, g, b, a) = (0, 163, 255, 255);
                } else if norm <= 0.28 {
                    // Bright cyan/white core
                    (r, g, b, a) = (185, 252, 255, 255);
                }

                // Soft edge anti-aliasing
                if norm > 0.94 {
                    let fade = ((1.0 - norm) / 0.06).clamp(0.0, 1.0);
                    a = (a as f32 * fade) as u8;
                }

                buf[idx] = r;
                buf[idx + 1] = g;
                buf[idx + 2] = b;
                buf[idx + 3] = a;
            }
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programmatic_arc_reactor_icon_has_valid_rgba_dimensions() {
        let rgba = generate_arc_reactor_rgba(32);
        assert_eq!(rgba.len(), 32 * 32 * 4);
        // Center pixel should be bright core with high alpha
        let center_idx = ((16 * 32 + 16) * 4) as usize;
        assert!(rgba[center_idx + 3] > 200);
        assert!(rgba[center_idx + 2] > 200);
        // Corner pixel should be transparent
        assert_eq!(rgba[3], 0);
    }
}
