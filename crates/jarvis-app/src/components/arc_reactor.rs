use crate::components::JarvisUiContext;
use crate::window_manager::{PointerDragTracker, WindowMode};
use dioxus::desktop::use_window;
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use jarvis_core::ReactorState;

fn reactor_state_class(state: ReactorState) -> &'static str {
    match state {
        ReactorState::Idle => "reactor-idle",
        ReactorState::Listening => "reactor-listening",
        ReactorState::Executing => "reactor-executing",
        ReactorState::AwaitingApproval => "reactor-approval",
        ReactorState::Notification => "reactor-notification",
    }
}

pub fn reactor_state_label(state: ReactorState) -> &'static str {
    match state {
        ReactorState::Idle => "IDLE",
        ReactorState::Listening => "LISTENING",
        ReactorState::Executing => "EXECUTING",
        ReactorState::AwaitingApproval => "APPROVAL",
        ReactorState::Notification => "NOTICE",
    }
}

/// 3-ring SVG Arc Reactor component supporting all 5 `ReactorState` animations.
#[component]
pub fn ArcReactorOrb(
    state: ReactorState,
    #[props(default = 76)] size: u32,
    #[props(default = 0)] badge_count: usize,
    #[props(default = None)] mic_db: Option<f32>,
) -> Element {
    let state_class = reactor_state_class(state);
    let size_px = format!("{size}px");
    let core_scale = if state == ReactorState::Listening {
        let db = mic_db.unwrap_or(-60.0).clamp(-60.0, 0.0);
        let norm = (db + 60.0) / 60.0;
        format!("transform: scale({:.2});", 1.0 + norm * 0.25)
    } else {
        String::new()
    };

    rsx! {
        div { class: "arc-reactor-wrap {state_class}", style: "width: {size_px}; height: {size_px};",
            svg {
                class: "arc-reactor-svg",
                width: "{size}",
                height: "{size}",
                view_box: "0 0 100 100",
                // Background dark core disc
                circle {
                    cx: "50",
                    cy: "50",
                    r: "44",
                    fill: "rgba(6, 12, 24, 0.94)",
                    stroke: "rgba(0, 240, 255, 0.25)",
                    stroke_width: "1.5",
                }
                // Outer segmented Arc Reactor ring
                circle {
                    class: "outer-ring",
                    cx: "50",
                    cy: "50",
                    r: "40",
                    fill: "none",
                    stroke_width: "3",
                    stroke_dasharray: "18 7 8 7",
                    stroke_linecap: "round",
                }
                // Middle counter-rotating ring
                circle {
                    class: "mid-ring",
                    cx: "50",
                    cy: "50",
                    r: "29",
                    fill: "none",
                    stroke_width: "2.4",
                    stroke_dasharray: "10 6 22 6",
                    stroke_linecap: "round",
                }
                // Inner triangular/ring detail
                circle {
                    cx: "50",
                    cy: "50",
                    r: "18",
                    fill: "none",
                    stroke: "rgba(0, 240, 255, 0.45)",
                    stroke_width: "1.2",
                }
                // Central core glow
                circle {
                    class: "core-glow",
                    cx: "50",
                    cy: "50",
                    r: "11",
                    style: "{core_scale}",
                }
                circle {
                    cx: "50",
                    cy: "50",
                    r: "5",
                    fill: "#E8FDFF",
                }
            }
            if badge_count > 0 {
                div { class: "orb-badge", "{badge_count}" }
            }
        }
    }
}

/// Floating 120x120 transparent Orb mode with drag-threshold vs click/double-click handling.
#[component]
pub fn StandbyOrbView() -> Element {
    let ctx = use_context::<JarvisUiContext>();
    let desktop = use_window();
    let mut drag_tracker = use_signal(PointerDragTracker::default);
    let mut pending_click_gen = use_signal(|| 0u64);

    let ui = ctx.ui_state.read();
    let state = ui.reactor_state;
    let badge_count = ui.pending_approvals.len() + ui.unread_notifications;
    let mic_db = Some(ui.mic_db);
    drop(ui);

    let desktop_move = desktop.clone();
    let desktop_click = desktop.clone();
    let desktop_dbl = desktop.clone();
    let ctx_click = ctx.clone();
    let mut ctx_dbl = ctx.clone();

    rsx! {
        div {
            class: "orb-stage",
            title: "Click: Quick HUD | Double-Click: Dashboard | Drag: Move",
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
                        desktop_move.drag();
                    }
                }
            },
            onmouseup: move |_| {
                drag_tracker.write().on_mouse_up();
            },
            onclick: move |_| {
                if !drag_tracker.write().take_suppress_click() {
                    let next_gen = pending_click_gen.read().wrapping_add(1);
                    pending_click_gen.set(next_gen);
                    let mut ctx_deferred = ctx_click.clone();
                    let desktop_deferred = desktop_click.clone();
                    let gen_sig = pending_click_gen;
                    spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(220)).await;
                        if *gen_sig.read() == next_gen
                            && *ctx_deferred.window_mode.read() == WindowMode::StandbyOrb
                        {
                            ctx_deferred.switch_mode(&desktop_deferred, WindowMode::StandbyPopup);
                        }
                    });
                }
            },
            ondoubleclick: move |_| {
                let next_gen = pending_click_gen.read().wrapping_add(1);
                pending_click_gen.set(next_gen);
                drag_tracker.write().take_suppress_click();
                ctx_dbl.switch_mode(&desktop_dbl, WindowMode::ExpandedDashboard);
            },
            ArcReactorOrb {
                state,
                size: 96,
                badge_count,
                mic_db,
            }
        }
    }
}
