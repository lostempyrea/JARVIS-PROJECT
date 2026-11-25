use crate::components::{
    svg_chat, svg_collapse, svg_comms, svg_music, svg_orb, svg_settings, svg_terminal, svg_voice,
    DashboardTab, JarvisUiContext,
};
use crate::window_manager::WindowMode;
use dioxus::desktop::use_window;
use dioxus::prelude::*;

#[component]
pub fn IconRail() -> Element {
    let mut ctx = use_context::<JarvisUiContext>();
    let desktop = use_window();

    let active_tab = *ctx.active_tab.read();
    let ui = ctx.ui_state.read();
    let pending_approvals = ui.pending_approvals.len();
    let unread_comms = ui.channel_notifications.len();
    drop(ui);

    let mut ctx_popup = ctx.clone();
    let mut ctx_orb = ctx.clone();
    let desktop_popup = desktop.clone();
    let desktop_orb = desktop.clone();

    rsx! {
        nav { class: "icon-rail",
            div { class: "icon-rail-group",
                button {
                    class: if active_tab == DashboardTab::Chat { "rail-btn active" } else { "rail-btn" },
                    "data-tooltip": "Chat HUD",
                    "data-tooltip-pos": "right",
                    onclick: move |_| ctx.active_tab.set(DashboardTab::Chat),
                    {svg_chat()}
                }
                button {
                    class: if active_tab == DashboardTab::Terminal { "rail-btn active" } else { "rail-btn" },
                    "data-tooltip": "Terminal & Tasks",
                    "data-tooltip-pos": "right",
                    onclick: move |_| ctx.active_tab.set(DashboardTab::Terminal),
                    {svg_terminal()}
                    if pending_approvals > 0 {
                        span { class: "rail-badge" }
                    }
                }
                button {
                    class: if active_tab == DashboardTab::Music { "rail-btn active" } else { "rail-btn" },
                    "data-tooltip": "Music Deck",
                    "data-tooltip-pos": "right",
                    onclick: move |_| ctx.active_tab.set(DashboardTab::Music),
                    {svg_music()}
                }
                button {
                    class: if active_tab == DashboardTab::Comms { "rail-btn active" } else { "rail-btn" },
                    "data-tooltip": "Comms Matrix",
                    "data-tooltip-pos": "right",
                    onclick: move |_| ctx.active_tab.set(DashboardTab::Comms),
                    {svg_comms()}
                    if unread_comms > 0 {
                        span { class: "rail-badge" }
                    }
                }
                button {
                    class: if active_tab == DashboardTab::Voice { "rail-btn active" } else { "rail-btn" },
                    "data-tooltip": "Voice Pipeline",
                    "data-tooltip-pos": "right",
                    onclick: move |_| ctx.active_tab.set(DashboardTab::Voice),
                    {svg_voice()}
                }
                button {
                    class: if active_tab == DashboardTab::Settings { "rail-btn active" } else { "rail-btn" },
                    "data-tooltip": "Settings & Vault",
                    "data-tooltip-pos": "right",
                    onclick: move |_| ctx.active_tab.set(DashboardTab::Settings),
                    {svg_settings()}
                }
            }

            div { class: "icon-rail-group",
                button {
                    class: "rail-btn",
                    "data-tooltip": "Quick Popup",
                    "data-tooltip-pos": "right",
                    onclick: move |_| ctx_popup.switch_mode(&desktop_popup, WindowMode::StandbyPopup),
                    {svg_collapse()}
                }
                button {
                    class: "rail-btn",
                    "data-tooltip": "Standby Orb",
                    "data-tooltip-pos": "right",
                    onclick: move |_| ctx_orb.switch_mode(&desktop_orb, WindowMode::StandbyOrb),
                    {svg_orb()}
                }
            }
        }
    }
}
