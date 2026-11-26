use crate::components::{
    clear_dom_input, render_chat_item, svg_check, svg_clear, svg_close, svg_edit, svg_headset, svg_mute_speaker, svg_pause, svg_play,
    svg_plus, svg_send, svg_skip, svg_speaker, svg_stop, svg_trash, svg_voice, JarvisUiContext,
    MicPickerButton,
};
use dioxus::events::Key;
use dioxus::prelude::*;
use jarvis_core::{ApprovalDecision, MusicTarget, PlaybackState, UiCommand};

#[component]
pub fn ChatHudView() -> Element {
    let ctx = use_context::<JarvisUiContext>();
    let mut prompt_text = use_signal(String::new);
    let mut editing_session = use_signal(|| false);
    let mut edit_title_buf = use_signal(String::new);

    let cfg_info = ctx
        .core
        .config
        .read()
        .map(|c| (format!("{:?}", c.active_provider), c.active_model.clone()))
        .unwrap_or_else(|_| ("OpenAiCompat".to_string(), "gpt-4o-mini".to_string()));

    let ui = ctx.ui_state.read();
    let sessions = ui.sessions.clone();
    let messages = ui.messages.clone();
    let pending = ui.pending_approvals.clone();
    let streaming_active = ui.streaming_active;
    let ptt_active = ui.ptt_active;
    let voice_running = ui.voice_running;
    let music_target = ui.music_target;
    let music_snap = match music_target {
        MusicTarget::Local => ui.local_music.clone(),
        MusicTarget::DiscordVoice | MusicTarget::Both => ui.discord_music.clone(),
    };
    drop(ui);

    let active_session = ctx.active_session_id.read().clone();
    let active_channel = sessions
        .iter()
        .find(|s| s.id == active_session)
        .map(|s| s.channel.as_str());
    let is_protected = jarvis_core::is_protected_session_id(&active_session, active_channel);

    let play_tip = if music_snap.state == PlaybackState::Playing {
        "Pause"
    } else {
        "Resume"
    };
    let voice_tip = if voice_running {
        "Stop Voice Pipeline"
    } else {
        "Start Hands-Free Voice"
    };

    rsx! {
        div { class: "col-gap", style: "flex: 1; min-height: 0;",
            // Session Switcher & Mini Music Bar
            div { class: "panel-card", style: "padding: 8px 12px; position: relative; z-index: 40; overflow: visible;",
                div { class: "session-bar-row",
                    div { class: "session-controls-group",
                        if *editing_session.read() && !is_protected {
                            div { class: "session-edit-inline",
                                input {
                                    id: "session-rename-input",
                                    class: "hud-input",
                                    spellcheck: "false",
                                    value: "{edit_title_buf}",
                                    oninput: move |evt| edit_title_buf.set(evt.value()),
                                    onkeydown: {
                                        let mut c = ctx.clone();
                                        let active_session = active_session.clone();
                                        move |evt: KeyboardEvent| {
                                            if evt.key() == Key::Enter && !evt.data().is_composing() {
                                                let title = edit_title_buf.read().clone();
                                                c.rename_session(&active_session, &title);
                                                editing_session.set(false);
                                            } else if evt.key() == Key::Escape {
                                                editing_session.set(false);
                                            }
                                        }
                                    }
                                }
                                button {
                                    class: "icon-btn",
                                    "data-tooltip": "Save Session Title",
                                    onclick: {
                                        let mut c = ctx.clone();
                                        let active_session = active_session.clone();
                                        move |_| {
                                            let title = edit_title_buf.read().clone();
                                            c.rename_session(&active_session, &title);
                                            editing_session.set(false);
                                        }
                                    },
                                    {svg_check()}
                                }
                                button {
                                    class: "icon-btn danger",
                                    "data-tooltip": "Cancel",
                                    onclick: move |_| editing_session.set(false),
                                    {svg_close()}
                                }
                            }
                        } else {
                            select {
                                class: "hud-select",
                                value: "{active_session}",
                                onchange: {
                                    let mut c = ctx.clone();
                                    move |evt: FormEvent| {
                                        c.switch_session(evt.value());
                                        editing_session.set(false);
                                    }
                                },
                                if !sessions.iter().any(|s| s.id == active_session) {
                                    option { value: "{active_session}", "{active_session}" }
                                }
                                for sess in sessions.clone() {
                                    option {
                                        key: "{sess.id}",
                                        value: "{sess.id}",
                                        "{sess.title} ({sess.channel})"
                                    }
                                }
                            }
                            button {
                                class: "icon-btn",
                                "data-tooltip": "New Session",
                                "data-tooltip-pos": "bottom-left",
                                onclick: {
                                    let mut c = ctx.clone();
                                    move |_| c.create_new_session()
                                },
                                {svg_plus()}
                            }
                            button {
                                class: "icon-btn",
                                "data-tooltip": "Clear Chat History",
                                "data-tooltip-pos": "bottom-left",
                                onclick: {
                                    let mut c = ctx.clone();
                                    let active_session = active_session.clone();
                                    move |_| c.clear_session_history(&active_session)
                                },
                                {svg_clear()}
                            }
                            if !is_protected {
                                button {
                                    class: "icon-btn",
                                    "data-tooltip": "Rename Session",
                                    "data-tooltip-pos": "bottom-left",
                                    onclick: {
                                        let active_session = active_session.clone();
                                        let sessions = sessions.clone();
                                        move |_| {
                                            let cur_title = sessions.iter().find(|s| s.id == active_session).map(|s| s.title.clone()).unwrap_or_else(|| active_session.clone());
                                            edit_title_buf.set(cur_title);
                                            editing_session.set(true);
                                        }
                                    },
                                    {svg_edit()}
                                }
                                button {
                                    class: "icon-btn danger",
                                    "data-tooltip": "Delete Session",
                                    "data-tooltip-pos": "bottom-left",
                                    onclick: {
                                        let mut c = ctx.clone();
                                        let active_session = active_session.clone();
                                        move |_| c.delete_session(&active_session)
                                    },
                                    {svg_trash()}
                                }
                                span { class: "chip", style: "max-width: 100%; overflow: hidden; text-overflow: ellipsis;", "{cfg_info.0} · {cfg_info.1}" }
                            } else {
                                span { class: "chip", "data-tooltip": "Core session — Clear History only", "LOCKED" }
                                span { class: "chip", style: "max-width: 100%; overflow: hidden; text-overflow: ellipsis;", "{cfg_info.0} · {cfg_info.1}" }
                            }
                        }
                    }

                    // Quick Mini Music Strip
                    div { class: "session-music-group",
                        span { class: "muted", style: "max-width: 190px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                            if let Some(track) = &music_snap.current {
                                "♫ {track.title}"
                            } else {
                                "♫ Idle"
                            }
                        }
                        button {
                            class: "icon-btn",
                            style: "width: 28px; height: 28px;",
                            "data-tooltip": "Toggle Output",
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
            }

            // Inline Approval Cards
            if !pending.is_empty() {
                div { class: "col-gap", style: "max-height: 36%; overflow-y: auto;",
                    for req in pending {
                        div { class: "approval-card", key: "{req.id}",
                            div { class: "approval-head",
                                span { class: "approval-tool", "⚠ APPROVAL REQUIRED: {req.tool_name}" }
                                span { class: "muted", {format!("{}", req.created_at.with_timezone(&chrono::Local).format("%H:%M:%S"))} }
                            }
                            div { class: "approval-reason", "{req.reason}" }
                            div { class: "approval-preview", "{req.command_or_args}" }
                            div { class: "approval-actions",
                                button {
                                    class: "hud-btn success",
                                    onclick: {
                                        let id = req.id.clone();
                                        let mut c = ctx.clone();
                                        move |_| c.resolve_approval(&id, ApprovalDecision::Approved)
                                    },
                                    {svg_check()}
                                    "Approve"
                                }
                                button {
                                    class: "hud-btn danger",
                                    onclick: {
                                        let id = req.id.clone();
                                        let mut c = ctx.clone();
                                        move |_| c.resolve_approval(&id, ApprovalDecision::Denied)
                                    },
                                    {svg_close()}
                                    "Deny"
                                }
                            }
                        }
                    }
                }
            }

            // Streaming Chat Timeline
            div { class: "panel-card scroll-y", style: "flex: 1; min-height: 0; gap: 10px;",
                if messages.is_empty() {
                    div { class: "muted", style: "margin: auto; text-align: center;",
                        "JARVIS Neural Core Online. Enter a command or coding task below."
                    }
                } else {
                    for item in messages {
                        {render_chat_item(item, ctx.clone(), Some("YOU"), Some("JARVIS"))}
                    }
                }
            }

            // Prompt Input Bar
            div { class: "row-gap", style: "position: relative; z-index: 55; overflow: visible;",
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
                                    Err(err) => c.ui_state.write().toast_banner = Some(format!("Voice error: {err}")),
                                }
                            }
                        }
                    },
                    {svg_voice()}
                }
                MicPickerButton { tooltip_pos: "top-left" }
                button {
                    class: "icon-btn danger",
                    "data-tooltip": "Mute / Stop Speaking",
                    "data-tooltip-pos": "top-left",
                    onclick: {
                        let mut c = ctx.clone();
                        move |_| {
                            c.core.voice.stop_speaking();
                            c.ui_state.write().toast_banner = Some("🔇 JARVIS muted.".to_string());
                        }
                    },
                    {svg_mute_speaker()}
                }
                button {
                    class: if ptt_active { "hud-btn danger active" } else { "hud-btn" },
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
                    id: "chat-prompt-input",
                    class: "hud-input",
                    style: "flex: 1;",
                    spellcheck: "false",
                    placeholder: "Ask JARVIS, run tools, search Gmail, or delegate to AGY...",
                    oninput: move |evt| prompt_text.set(evt.value()),
                    onkeydown: {
                        let mut c = ctx.clone();
                        move |evt: KeyboardEvent| {
                            if evt.key() == Key::Enter && !evt.data().is_composing() {
                                let val = prompt_text.read().clone();
                                if !val.trim().is_empty() {
                                    prompt_text.set(String::new());
                                    clear_dom_input("chat-prompt-input", true);
                                    c.submit_prompt(val);
                                }
                            }
                        }
                    },
                }
                if streaming_active {
                    button {
                        class: "hud-btn danger",
                        onclick: {
                            let c = ctx.clone();
                            move |_| {
                                let sid = c.active_session_id.read().clone();
                                c.send_command(UiCommand::CancelSession { session_id: sid });
                            }
                        },
                        {svg_stop()}
                        "Stop"
                    }
                } else {
                    button {
                        class: "hud-btn primary",
                        onclick: {
                            let mut c = ctx.clone();
                            move |_| {
                                let val = prompt_text.read().clone();
                                if !val.trim().is_empty() {
                                    prompt_text.set(String::new());
                                    clear_dom_input("chat-prompt-input", true);
                                    c.submit_prompt(val);
                                }
                            }
                        },
                        {svg_send()}
                        "Send"
                    }
                }
            }
        }
    }
}
