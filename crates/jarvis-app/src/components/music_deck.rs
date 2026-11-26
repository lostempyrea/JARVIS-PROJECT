use crate::components::{
    clear_dom_input, svg_download, svg_headset, svg_music, svg_pause, svg_play, svg_refresh,
    svg_skip, svg_speaker, svg_stop, JarvisUiContext,
};
use dioxus::events::Key;
use dioxus::prelude::*;
use jarvis_core::{MusicTarget, PlaybackState};

#[component]
pub fn MusicDeckView() -> Element {
    let mut ctx = use_context::<JarvisUiContext>();
    let mut search_query = use_signal(String::new);
    let mut discord_channel_id = use_signal(String::new);
    let mut status_msg = use_signal(|| Option::<String>::None);
    let mut auto_follow = use_signal(|| {
        ctx.core
            .config
            .read()
            .map(|c| c.channels.discord_auto_join_voice)
            .unwrap_or(false)
    });

    let ui = ctx.ui_state.read();
    let target = ui.music_target;
    let snap = match target {
        MusicTarget::Local => ui.local_music.clone(),
        MusicTarget::DiscordVoice | MusicTarget::Both => ui.discord_music.clone(),
    };
    let bin_status = ui.binary_status.clone();
    let ffmpeg_progress = ui.ffmpeg_install_status.clone();
    drop(ui);

    let volume_pct = (snap.volume * 100.0).round() as i32;

    let trigger_play = {
        let core = ctx.core.clone();
        let ui_sig = ctx.ui_state;
        move |query: String| {
            let trimmed = query.trim().to_string();
            if trimmed.is_empty() {
                return;
            }
            let core = core.clone();
            let target = ui_sig.read().music_target;
            status_msg.set(Some(format!("Resolving '{trimmed}'...")));
            spawn(async move {
                match core.music.play(target, &trimmed, "hud", None).await {
                    Ok(_) => status_msg.set(Some(format!("Queued: {trimmed}"))),
                    Err(err) => status_msg.set(Some(format!("Play failed: {err}"))),
                }
            });
        }
    };

    let trigger_join = {
        let core = ctx.core.clone();
        move || {
            let raw = discord_channel_id.read().trim().to_string();
            let parsed_cid = if raw.is_empty() {
                None
            } else {
                match raw.parse::<u64>() {
                    Ok(id) if id > 0 => Some(id),
                    _ => {
                        status_msg.set(Some(
                            "Invalid Voice Channel ID (must be a positive number).".to_string(),
                        ));
                        return;
                    }
                }
            };
            let core = core.clone();
            status_msg.set(Some("Joining Discord voice channel...".to_string()));
            spawn(async move {
                match core.music.auto_join_discord_voice(None, parsed_cid).await {
                    Ok((gid, cid, _)) => {
                        let msg = match core.music.voice_manager().guild_name(gid) {
                            Some(name) => format!("Joined voice channel {cid} in {name}"),
                            None => format!("Joined voice channel {cid} (server {gid})"),
                        };
                        status_msg.set(Some(msg));
                    }
                    Err(err) => status_msg.set(Some(format!("Join failed: {err}"))),
                }
            });
        }
    };

    rsx! {
        div { class: "tab-scroll-page",
            // Top Controls: Search + Target Output Selector
            div { class: "panel-card",
                div { class: "panel-header",
                    div { class: "panel-title",
                        {svg_music()}
                        "Dual Music Engine (Local rodio + Discord Voice E2EE)"
                    }
                    div { class: "row-gap",
                        button {
                            class: if target == MusicTarget::Local { "hud-btn active" } else { "hud-btn" },
                            onclick: move |_| ctx.ui_state.write().music_target = MusicTarget::Local,
                            {svg_speaker()}
                            "Local"
                        }
                        button {
                            class: if target == MusicTarget::DiscordVoice { "hud-btn active" } else { "hud-btn" },
                            onclick: move |_| ctx.ui_state.write().music_target = MusicTarget::DiscordVoice,
                            {svg_headset()}
                            "DiscordVoice"
                        }
                        button {
                            class: if target == MusicTarget::Both { "hud-btn active" } else { "hud-btn" },
                            onclick: move |_| ctx.ui_state.write().music_target = MusicTarget::Both,
                            "Both"
                        }
                    }
                }

                div { class: "row-gap",
                    input {
                        id: "music-search-input",
                        class: "hud-input",
                        style: "flex: 1;",
                        spellcheck: "false",
                        placeholder: "Song name, YouTube URL, or direct stream URL...",
                        oninput: move |evt| search_query.set(evt.value()),
                        onkeydown: {
                            let mut play = trigger_play.clone();
                            move |evt: KeyboardEvent| {
                                if evt.key() == Key::Enter && !evt.data().is_composing() {
                                    let q = search_query.read().clone();
                                    search_query.set(String::new());
                                    clear_dom_input("music-search-input", true);
                                    play(q);
                                }
                            }
                        },
                    }
                    button {
                        class: "hud-btn primary",
                        onclick: {
                            let mut play = trigger_play.clone();
                            move |_| {
                                let q = search_query.read().clone();
                                search_query.set(String::new());
                                clear_dom_input("music-search-input", true);
                                play(q);
                            }
                        },
                        {svg_play()}
                        "Play / Queue"
                    }
                }

                if let Some(msg) = status_msg.read().as_ref() {
                    div { class: "muted", "{msg}" }
                }
            }

            div { class: "grid-2", style: "flex: 1; min-height: 0;",
                // Left: Now Playing & Transport + Volume + Discord Voice Channel
                div { class: "col-gap",
                    div { class: "panel-card",
                        div { class: "panel-header",
                            span { class: "panel-title", "Now Playing" }
                            span { class: "chip", "{snap.state:?}" }
                        }
                        if let Some(track) = &snap.current {
                            div { class: "col-gap",
                                div { style: "font-size: 15px; font-weight: 700; color: var(--cyan-neon);", "{track.title}" }
                                div { class: "muted", "Requested by: {track.requested_by} · Duration: {track.duration_seconds}s" }
                                div { class: "muted", style: "word-break: break-all;", "{track.webpage_url}" }
                            }
                        } else {
                            div { class: "muted", "Queue is empty. Search or paste a track URL above." }
                        }

                        div { class: "row-gap", style: "margin-top: 6px;",
                            button {
                                class: "hud-btn primary",
                                onclick: {
                                    let core = ctx.core.clone();
                                    move |_| {
                                        let res = if snap.state == PlaybackState::Playing {
                                            core.music.pause(target, None)
                                        } else {
                                            core.music.resume(target, None)
                                        };
                                        if let Err(err) = res {
                                            status_msg.set(Some(err.to_string()));
                                        }
                                    }
                                },
                                if snap.state == PlaybackState::Playing { {svg_pause()} "Pause" } else { {svg_play()} "Resume" }
                            }
                            button {
                                class: "hud-btn",
                                onclick: {
                                    let core = ctx.core.clone();
                                    move |_| {
                                        if let Err(err) = core.music.skip(target, None) {
                                            status_msg.set(Some(err.to_string()));
                                        }
                                    }
                                },
                                {svg_skip()}
                                "Skip"
                            }
                            button {
                                class: "hud-btn danger",
                                onclick: {
                                    let core = ctx.core.clone();
                                    move |_| {
                                        core.music.stop(target, None);
                                    }
                                },
                                {svg_stop()}
                                "Stop"
                            }
                        }

                        div { class: "row-gap", style: "margin-top: 6px;",
                            span { class: "muted", style: "min-width: 78px;", "Volume: {volume_pct}%" }
                            input {
                                r#type: "range",
                                min: "0",
                                max: "200",
                                value: "{volume_pct}",
                                style: "flex: 1; accent-color: #00F0FF;",
                                oninput: {
                                    let core = ctx.core.clone();
                                    move |evt: FormEvent| {
                                        if let Ok(v) = evt.value().parse::<f32>() {
                                            let _ = core.music.set_volume(target, v / 100.0, None);
                                        }
                                    }
                                },
                            }
                        }
                    }

                    // Discord Voice Connection + FFmpeg / Binaries
                    div { class: "panel-card",
                        div { class: "panel-header",
                            span { class: "panel-title", "Discord Voice & Media Binaries" }
                            div { class: "row-gap",
                                button {
                                    class: if *auto_follow.read() { "hud-btn active" } else { "hud-btn" },
                                    onclick: {
                                        let core = ctx.core.clone();
                                        move |_| {
                                            let next = !*auto_follow.read();
                                            if let Ok(mut cfg) = core.config.write() {
                                                cfg.channels.discord_auto_join_voice = next;
                                                match cfg.save() {
                                                    Ok(()) => {
                                                        auto_follow.set(next);
                                                        status_msg.set(Some(if next {
                                                            "Auto-Follow Voice enabled and saved.".to_string()
                                                        } else {
                                                            "Auto-Follow Voice disabled and saved.".to_string()
                                                        }));
                                                    }
                                                    Err(err) => status_msg.set(Some(format!("Save error: {err}"))),
                                                }
                                            }
                                        }
                                    },
                                    if *auto_follow.read() { "Auto-Follow Voice: ON" } else { "Auto-Follow Voice: OFF" }
                                }
                                button {
                                    class: "icon-btn",
                                    "data-tooltip": "Refresh Binary Status",
                                    onclick: {
                                        let mut c = ctx.clone();
                                        move |_| {
                                            let status = c.core.binary_manager.check_status();
                                            c.ui_state.write().binary_status = status;
                                        }
                                    },
                                    {svg_refresh()}
                                }
                            }
                        }
                        div { class: "row-gap",
                            input {
                                id: "music-channel-id-input",
                                class: "hud-input",
                                style: "flex: 1;",
                                spellcheck: "false",
                                placeholder: "Voice Channel ID (leave empty to auto-join yours)",
                                oninput: move |evt| discord_channel_id.set(evt.value()),
                                onkeydown: {
                                    let mut join = trigger_join.clone();
                                    move |evt: KeyboardEvent| {
                                        if evt.key() == Key::Enter && !evt.data().is_composing() {
                                            join();
                                        }
                                    }
                                },
                            }
                            button {
                                class: "hud-btn primary",
                                onclick: {
                                    let mut join = trigger_join.clone();
                                    move |_| join()
                                },
                                "Join"
                            }
                        button {
                            class: "hud-btn danger",
                            onclick: {
                                let core = ctx.core.clone();
                                move |_| {
                                    let core = core.clone();
                                    spawn(async move {
                                        let _ = core.music.leave_discord_voice(None).await;
                                        status_msg.set(Some("Left Discord voice channel".to_string()));
                                    });
                                }
                            },
                            "Leave"
                        }
                        }

                        div { class: "row-gap", style: "flex-wrap: wrap;",
                            span { class: "chip",
                                span { class: if bin_status.ffmpeg_installed { "led online" } else { "led error" } }
                                "ffmpeg"
                            }
                            span { class: "chip",
                                span { class: if bin_status.yt_dlp_installed { "led online" } else { "led error" } }
                                "yt-dlp"
                            }
                            span { class: "chip",
                                span { class: if bin_status.agy_installed { "led online" } else { "led offline" } }
                                "agy"
                            }
                            if !bin_status.ffmpeg_installed {
                                button {
                                    class: "hud-btn primary",
                                    onclick: {
                                        let core = ctx.core.clone();
                                        move |_| {
                                            let core = core.clone();
                                            spawn(async move {
                                                match core.binary_manager.ensure_ffmpeg().await {
                                                    Ok(path) => status_msg.set(Some(format!("FFmpeg ready: {}", path.display()))),
                                                    Err(err) => status_msg.set(Some(format!("FFmpeg install failed: {err}"))),
                                                }
                                            });
                                        }
                                    },
                                    {svg_download()}
                                    "Install FFmpeg"
                                }
                            }
                        }
                        if let Some(prog) = ffmpeg_progress {
                            div { class: "muted", "{prog}" }
                        }
                    }
                }

                // Right: Upcoming Queue List
                div { class: "panel-card", style: "min-height: 0;",
                    div { class: "panel-header",
                        span { class: "panel-title", "Up Next Queue" }
                        span { class: "chip", "{snap.upcoming.len()} Tracks" }
                    }
                    div { class: "scroll-y col-gap", style: "flex: 1;",
                        if snap.upcoming.is_empty() {
                            div { class: "muted", "No upcoming tracks queued." }
                        } else {
                            for (idx, track) in snap.upcoming.iter().enumerate() {
                                div {
                                    key: "{idx}-{track.title}",
                                    class: "row-between",
                                    style: "padding: 7px 9px; border-radius: 7px; background: rgba(4, 10, 20, 0.72); border: 1px solid rgba(0, 240, 255, 0.14);",
                                    div { style: "min-width: 0; flex: 1;",
                                        div { style: "font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                                            "{idx + 1}. {track.title}"
                                        }
                                        div { class: "muted", "by {track.requested_by} · {track.duration_seconds}s" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
