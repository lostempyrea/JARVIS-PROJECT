use crate::components::arc_reactor::{reactor_state_label, ArcReactorOrb};
use crate::components::{
    clear_dom_input, render_chat_item, svg_check, svg_close, svg_expand, svg_headset,
    svg_minimize, svg_orb, svg_pause, svg_play, svg_send, svg_skip, svg_speaker, svg_stop,
    svg_voice, ChatTimelineItem, JarvisUiContext, MicPickerButton,
};
use crate::window_manager::{PointerDragTracker, WindowMode};
use dioxus::desktop::use_window;
use dioxus::events::Key;
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use jarvis_core::{ApprovalDecision, MusicTarget, PlaybackState, UiCommand};

#[component]
pub fn StandbyPopupView() -> Element {
    let ctx = use_context::<JarvisUiContext>();
    let desktop = use_window();
    let mut prompt_text = use_signal(String::new);
    let mut drag_tracker = use_signal(PointerDragTracker::default);

    let ui = ctx.ui_state.read();
    let reactor_state = ui.reactor_state;
    let state_label = reactor_state_label(reactor_state);
    let toast = ui.toast_banner.clone();
    let pending = ui.pending_approvals.clone();
    let messages: Vec<ChatTimelineItem> = ui.messages.iter().rev().take(24).cloned().collect();
    let music_target = ui.music_target;
    let music_snap = match music_target {
        MusicTarget::Local => ui.local_music.clone(),
        MusicTarget::DiscordVoice | MusicTarget::Both => ui.discord_music.clone(),
    };
    let voice_running = ui.voice_running;
    let ptt_active = ui.ptt_active;
    let streaming_active = ui.streaming_active;
    let mic_db = Some(ui.mic_db);
    drop(ui);

    let output_label = match music_target {
        MusicTarget::Local => "Output: Local Speakers",
        MusicTarget::DiscordVoice => "Output: Discord Voice",
        MusicTarget::Both => "Output: Local + Discord",
    };
    let switch_tip = if music_target == MusicTarget::Local {
        "Switch to Discord"
    } else {
        "Switch to Local"
    };
    let play_tip = if music_snap.state == PlaybackState::Playing {
        "Pause"
    } else {
        "Resume"
    };
    let voice_tip = if voice_running {
        "Stop Voice Loop"
    } else {
        "Start Hands-Free Voice"
    };

    let desktop_drag = desktop.clone();
    let desktop_dash = desktop.clone();
    let desktop_orb = desktop.clone();
    let desktop_hide = desktop.clone();

    let mut ctx_dash = ctx.clone();
    let mut ctx_orb = ctx.clone();
    let mut ctx_hide = ctx.clone();

    rsx! {
        div { class: "popup-shell hud-glass",
            // Header (drag via threshold)
            div {
                class: "popup-header",
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
                div { class: "popup-header-left",
                    ArcReactorOrb { state: reactor_state, size: 26, badge_count: 0, mic_db }
                    span { class: "popup-title", "J.A.R.V.I.S." }
                    span { class: "popup-state-tag", "{state_label}" }
                }
                div {
                    class: "popup-actions",
                    onmousedown: move |evt| evt.stop_propagation(),
                    button {
                        class: "icon-btn",
                        "data-tooltip": "Dashboard",
                        "data-tooltip-pos": "bottom-right",
                        onclick: move |_| ctx_dash.switch_mode(&desktop_dash, WindowMode::ExpandedDashboard),
                        {svg_expand()}
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
                        "data-tooltip": "Hide HUD",
                        "data-tooltip-pos": "bottom-right",
                        onclick: move |_| {
                            ctx_hide.hud_visible.set(false);
                            desktop_hide.window.set_visible(false);
                        },
                        {svg_minimize()}
                    }
                }
            }

            // Notification Toast Strip
            if let Some(banner) = toast {
                div { class: "toast-strip",
                    span { style: "overflow: hidden; text-overflow: ellipsis; white-space: nowrap;", "{banner}" }
                    button {
                        class: "icon-btn",
                        style: "width: 22px; height: 22px;",
                        "data-tooltip": "Dismiss",
                        "data-tooltip-pos": "bottom-right",
                        onclick: {
                            let mut c = ctx.clone();
                            move |_| c.ui_state.write().toast_banner = None
                        },
                        {svg_close()}
                    }
                }
            }

            // Mini Music Pill
            div { class: "mini-music-pill",
                div { class: "mini-music-meta",
                    div { class: "mini-music-title",
                        if let Some(track) = &music_snap.current {
                            "{track.title}"
                        } else {
                            "No Track Playing"
                        }
                    }
                    div { class: "mini-music-sub", "{output_label}" }
                }
                div { class: "row-gap",
                    button {
                        class: "icon-btn",
                        style: "width: 28px; height: 28px;",
                        "data-tooltip": "{switch_tip}",
                        "data-tooltip-pos": "bottom-right",
                        onclick: {
                            let mut c = ctx.clone();
                            move |_| {
                                let next = match c.ui_state.read().music_target {
                                    MusicTarget::Local => MusicTarget::DiscordVoice,
                                    MusicTarget::DiscordVoice => MusicTarget::Both,
                                    MusicTarget::Both => MusicTarget::Local,
                                };
                                c.ui_state.write().music_target = next;
                            }
                        },
                        if music_target == MusicTarget::Local { {svg_speaker()} } else { {svg_headset()} }
                    }
                    button {
                        class: "icon-btn",
                        style: "width: 28px; height: 28px;",
                        "data-tooltip": "{play_tip}",
                        "data-tooltip-pos": "bottom-right",
                        onclick: {
                            let c = ctx.clone();
                            move |_| {
                                let target = c.ui_state.read().music_target;
                                if music_snap.state == PlaybackState::Playing {
                                    let _ = c.core.music.pause(target, None);
                                } else {
                                    let _ = c.core.music.resume(target, None);
                                }
                            }
                        },
                        if music_snap.state == PlaybackState::Playing { {svg_pause()} } else { {svg_play()} }
                    }
                    button {
                        class: "icon-btn",
                        style: "width: 28px; height: 28px;",
                        "data-tooltip": "Skip",
                        "data-tooltip-pos": "bottom-right",
                        onclick: {
                            let c = ctx.clone();
                            move |_| {
                                let target = c.ui_state.read().music_target;
                                let _ = c.core.music.skip(target, None);
                            }
                        },
                        {svg_skip()}
                    }
                }
            }

            // Body: Approval Cards + Quick Chat Stream
            div { class: "popup-body",
                for req in pending {
                    div { class: "approval-card", key: "{req.id}",
                        div { class: "approval-head",
                            span { class: "approval-tool", "⚠ {req.tool_name}" }
                            span { class: "muted", {format!("{}", req.created_at.format("%H:%M:%S"))} }
                        }
                        div { class: "approval-reason", "{req.reason}" }
                        div { class: "approval-preview", "{req.command_or_args}" }
                        div { class: "approval-actions",
                            button {
                                class: "icon-btn success",
                                "data-tooltip": "Approve",
                                onclick: {
                                    let id = req.id.clone();
                                    let mut c = ctx.clone();
                                    move |_| c.resolve_approval(&id, ApprovalDecision::Approved)
                                },
                                {svg_check()}
                            }
                            button {
                                class: "icon-btn danger",
                                "data-tooltip": "Reject",
                                onclick: {
                                    let id = req.id.clone();
                                    let mut c = ctx.clone();
                                    move |_| c.resolve_approval(&id, ApprovalDecision::Denied)
                                },
                                {svg_close()}
                            }
                        }
                    }
                }

                if messages.is_empty() {
                    div { class: "muted", style: "text-align: center; margin-top: 40px;",
                        "Arc Reactor Ready. Type a prompt or hold Mic to talk."
                    }
                } else {
                    for item in messages.into_iter().rev() {
                        {render_chat_item(item, ctx.clone(), None, None)}
                    }
                }
            }

            // Input Bar + Mic PTT + Hands-Free Voice Toggle + Mic Picker
            div { class: "popup-input-bar",
                button {
                    class: if voice_running { "icon-btn active" } else { "icon-btn" },
                    "data-tooltip": "{voice_tip}",
                    "data-tooltip-pos": "top-left",
                    onclick: {
                        let mut c = ctx.clone();
                        move |_| {
                            if c.core.voice.is_running() {
                                c.core.voice.stop();
                                c.ui_state.write().voice_running = false;
                            } else {
                                let sid = c.active_session_id.read().clone();
                                match c.core.voice.start(sid) {
                                    Ok(()) => c.ui_state.write().voice_running = true,
                                    Err(err) => {
                                        c.ui_state.write().toast_banner = Some(format!("Voice start failed: {err}"));
                                    }
                                }
                            }
                        }
                    },
                    {svg_voice()}
                }
                MicPickerButton { tooltip_pos: "top-left" }
                button {
                    class: if ptt_active { "icon-btn danger active" } else { "icon-btn" },
                    "data-tooltip": "Hold Push-to-Talk",
                    "data-tooltip-pos": "top-left",
                    onmousedown: {
                        let mut c = ctx.clone();
                        move |_| {
                            c.ui_state.write().ptt_active = true;
                            c.core.voice.ptt_press();
                        }
                    },
                    onmouseup: {
                        let mut c = ctx.clone();
                        move |_| {
                            if !c.ui_state.read().ptt_active {
                                return;
                            }
                            c.ui_state.write().ptt_active = false;
                            if let Ok(Some(wav)) = c.core.voice.ptt_release_wav() {
                                let core = c.core.clone();
                                let session_id = c.active_session_id.read().clone();
                                tokio::spawn(async move {
                                    let _ = core.voice.process_ptt_wav(&session_id, &wav).await;
                                });
                            }
                        }
                    },
                    onmouseleave: {
                        let mut c = ctx.clone();
                        move |_| {
                            if !c.ui_state.read().ptt_active {
                                return;
                            }
                            c.ui_state.write().ptt_active = false;
                            if let Ok(Some(wav)) = c.core.voice.ptt_release_wav() {
                                let core = c.core.clone();
                                let session_id = c.active_session_id.read().clone();
                                tokio::spawn(async move {
                                    let _ = core.voice.process_ptt_wav(&session_id, &wav).await;
                                });
                            }
                        }
                    },
                    "PTT"
                }
                input {
                    id: "popup-prompt-input",
                    class: "hud-input",
                    spellcheck: "false",
                    placeholder: "Command JARVIS...",
                    oninput: move |evt| prompt_text.set(evt.value()),
                    onkeydown: {
                        let mut c = ctx.clone();
                        move |evt: KeyboardEvent| {
                            if evt.key() == Key::Enter && !evt.data().is_composing() {
                                let val = prompt_text.read().clone();
                                if !val.trim().is_empty() {
                                    prompt_text.set(String::new());
                                    clear_dom_input("popup-prompt-input", true);
                                    c.submit_prompt(val);
                                }
                            }
                        }
                    },
                }
                if streaming_active {
                    button {
                        class: "icon-btn danger",
                        "data-tooltip": "Cancel",
                        "data-tooltip-pos": "top-right",
                        onclick: {
                            let c = ctx.clone();
                            move |_| {
                                let sid = c.active_session_id.read().clone();
                                c.send_command(UiCommand::CancelSession { session_id: sid });
                            }
                        },
                        {svg_stop()}
                    }
                } else {
                    button {
                        class: "icon-btn",
                        "data-tooltip": "Send",
                        "data-tooltip-pos": "top-right",
                        onclick: {
                            let mut c = ctx.clone();
                            move |_| {
                                let val = prompt_text.read().clone();
                                if !val.trim().is_empty() {
                                    prompt_text.set(String::new());
                                    clear_dom_input("popup-prompt-input", true);
                                    c.submit_prompt(val);
                                }
                            }
                        },
                        {svg_send()}
                    }
                }
            }
        }
    }
}
