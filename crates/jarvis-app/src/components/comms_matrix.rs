use crate::components::{
    add_allowed_discord_user, add_allowed_telegram_chat, clear_dom_input, svg_comms, svg_mail,
    svg_refresh, svg_send, svg_settings, DashboardTab, JarvisUiContext,
};
use dioxus::events::Key;
use dioxus::prelude::*;
use jarvis_core::{ChannelState, RiskTier, SecurityMode};
use std::time::Duration;

fn led_class(state: ChannelState) -> &'static str {
    match state {
        ChannelState::Online => "led online",
        ChannelState::Connecting => "led connecting",
        ChannelState::Offline => "led offline",
        ChannelState::Error => "led error",
    }
}

#[component]
pub fn CommsMatrixView() -> Element {
    let mut ctx = use_context::<JarvisUiContext>();
    let mut gmail_query = use_signal(String::new);
    let mut send_to = use_signal(String::new);
    let mut send_subject = use_signal(String::new);
    let mut send_body = use_signal(String::new);
    let mut pairing_tick = use_signal(|| 0u64);
    let _pair_tick = *pairing_tick.read();

    let ui = ctx.ui_state.read();
    let statuses = ui.channel_statuses.clone();
    let notifications = ui.channel_notifications.clone();
    let pairing_candidates = ui.pairing_candidates.clone();
    let emails = ui.gmail_emails.clone();
    let gmail_loading = ui.gmail_loading;
    let gmail_status_msg = ui.gmail_status_msg.clone();
    drop(ui);

    let (discord_whitelist_count, telegram_whitelist_count) = ctx
        .core
        .config
        .read()
        .map(|c| {
            (
                c.channels.discord_allowed_user_ids.len(),
                c.channels.telegram_allowed_chat_ids.len(),
            )
        })
        .unwrap_or((0, 0));
    let tg_pairing_rem = ctx.core.channels.telegram().pairing_remaining_secs();

    let run_gmail_fetch = {
        let core = ctx.core.clone();
        let mut ui_sig = ctx.ui_state;
        move |query: Option<String>| {
            let core = core.clone();
            ui_sig.write().gmail_loading = true;
            ui_sig.write().gmail_status_msg = Some("Querying Gmail IMAP...".to_string());
            spawn(async move {
                let res = match query {
                    Some(q) if !q.trim().is_empty() => {
                        core.channels.gmail().search_inbox(&q, 10).await
                    }
                    _ => core.channels.gmail().fetch_unread(10).await,
                };
                let mut ui = ui_sig.write();
                ui.gmail_loading = false;
                match res {
                    Ok(list) => {
                        ui.gmail_status_msg = Some(format!("Loaded {} message(s).", list.len()));
                        ui.gmail_emails = list;
                    }
                    Err(err) => {
                        ui.gmail_status_msg = Some(format!("Gmail error: {err}"));
                    }
                }
            });
        }
    };

    rsx! {
        div { class: "col-gap", style: "flex: 1; min-height: 0;",
            // Top 3-Channel Status Matrix
            div { class: "grid-3",
                for ch in statuses {
                    {
                        let ch_candidates: Vec<_> = pairing_candidates
                            .iter()
                            .filter(|c| c.channel == ch.channel_id)
                            .cloned()
                            .collect();
                        let whitelist_badge = match ch.channel_id.as_str() {
                            "discord" => Some(format!("{discord_whitelist_count} User ID")),
                            "telegram" => Some(format!("{telegram_whitelist_count} Chat ID")),
                            _ => None,
                        };
                        rsx! {
                            div { class: "panel-card", key: "{ch.channel_id}",
                                div { class: "row-between",
                                    div { class: "row-gap",
                                        span { class: led_class(ch.state) }
                                        span { style: "font-weight: 700; text-transform: uppercase; letter-spacing: 0.08em;", "{ch.channel_id}" }
                                        if let Some(identity) = &ch.connected_as {
                                            span { class: "chip", "@{identity}" }
                                        }
                                    }
                                    div { class: "row-gap",
                                        span { class: "chip", "{ch.state:?}" }
                                        button {
                                            class: "icon-btn",
                                            style: "width: 26px; height: 26px;",
                                            "data-tooltip": "Cài đặt Token & ID",
                                            "data-tooltip-pos": "bottom-right",
                                            onclick: move |_| ctx.active_tab.set(DashboardTab::Settings),
                                            {svg_settings()}
                                        }
                                    }
                                }
                                div { class: "row-between",
                                    div { class: "muted",
                                        if let Some(identity) = &ch.connected_as {
                                            "Identity: @{identity}"
                                        } else {
                                            "Not connected"
                                        }
                                    }
                                    if let Some(wl) = whitelist_badge {
                                        span { class: "chip", "{wl}" }
                                    }
                                }
                                if let Some(err) = &ch.last_error {
                                    div { style: "color: var(--red-danger); font-size: 11px;", "{err}" }
                                }
                                if !ch_candidates.is_empty() {
                                    div { class: "id-chip-list",
                                        for cand in ch_candidates {
                                            {
                                                let sid = cand.sender_id.clone();
                                                let dname = cand.display_name.clone();
                                                let ch_id = ch.channel_id.clone();
                                                rsx! {
                                                    button {
                                                        key: "cm-cand-{ch_id}-{sid}",
                                                        class: "pairing-chip",
                                                        "data-tooltip": "{cand.preview}",
                                                        onclick: {
                                                            let mut c = ctx.clone();
                                                            let sid_click = sid.clone();
                                                            let ch_click = ch_id.clone();
                                                            move |_| {
                                                                if ch_click == "discord" {
                                                                    if let Ok(uid) = sid_click.parse::<u64>() {
                                                                        if let Err(err) = add_allowed_discord_user(&mut c, uid) {
                                                                            c.ui_state.write().toast_banner = Some(err);
                                                                        } else {
                                                                            c.ui_state.write().toast_banner = Some(format!("Đã cho phép Discord User {uid}"));
                                                                        }
                                                                    }
                                                                } else if ch_click == "telegram" {
                                                                    if let Ok(cid) = sid_click.parse::<i64>() {
                                                                        if let Err(err) = add_allowed_telegram_chat(&mut c, cid) {
                                                                            c.ui_state.write().toast_banner = Some(err);
                                                                        } else {
                                                                            c.ui_state.write().toast_banner = Some(format!("Đã cho phép Telegram Chat {cid}"));
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        },
                                                        "+ Cho phép {sid} ({dname})"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if ch.channel_id == "discord" || ch.channel_id == "telegram" {
                                    div { class: "row-gap", style: "flex-wrap: wrap;",
                                        if ch.state == ChannelState::Online || ch.state == ChannelState::Connecting {
                                            button {
                                                class: "hud-btn danger",
                                                onclick: {
                                                    let cid = ch.channel_id.clone();
                                                    let core = ctx.core.clone();
                                                    move |_| {
                                                        let cid = cid.clone();
                                                        let core = core.clone();
                                                        tokio::spawn(async move {
                                                            let _ = core.channels.stop_channel(&cid).await;
                                                        });
                                                    }
                                                },
                                                "Disconnect"
                                            }
                                        } else {
                                            button {
                                                class: "hud-btn primary",
                                                onclick: {
                                                    let cid = ch.channel_id.clone();
                                                    let core = ctx.core.clone();
                                                    let mut ui_sig = ctx.ui_state;
                                                    move |_| {
                                                        let cid = cid.clone();
                                                        let core = core.clone();
                                                        spawn(async move {
                                                            if let Err(err) = core.channels.start_channel(&cid).await {
                                                                ui_sig.write().toast_banner = Some(format!("{cid} start failed: {err}"));
                                                            }
                                                        });
                                                    }
                                                },
                                                "Connect"
                                            }
                                        }
                                        if ch.channel_id == "telegram" && telegram_whitelist_count == 0 {
                                            button {
                                                class: if tg_pairing_rem.is_some() { "hud-btn active" } else { "hud-btn success" },
                                                onclick: {
                                                    let core = ctx.core.clone();
                                                    let mut ui_sig = ctx.ui_state;
                                                    move |_| {
                                                        core.channels.telegram().enable_pairing_window(Duration::from_secs(60));
                                                        pairing_tick.with_mut(|v| *v = v.wrapping_add(1));
                                                        ui_sig.write().toast_banner = Some(
                                                            "Đã bật ghép đôi /start (60s) — nhắn /start cho bot trên Telegram."
                                                                .to_string(),
                                                        );
                                                        let core_async = core.clone();
                                                        spawn(async move {
                                                            if core_async.channels.telegram().status().state == ChannelState::Offline {
                                                                if let Err(err) = core_async.channels.start_channel("telegram").await {
                                                                    ui_sig.write().toast_banner = Some(format!("telegram start failed: {err}"));
                                                                }
                                                            }
                                                            loop {
                                                                tokio::time::sleep(Duration::from_secs(1)).await;
                                                                let rem = core_async.channels.telegram().pairing_remaining_secs();
                                                                pairing_tick.with_mut(|v| *v = v.wrapping_add(1));
                                                                if rem.is_none() {
                                                                    break;
                                                                }
                                                            }
                                                        });
                                                    }
                                                },
                                                if let Some(rem) = tg_pairing_rem {
                                                    "Đang chờ /start ({rem}s)"
                                                } else {
                                                    "Ghép đôi /start"
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

            // Bottom Split: Gmail Inbox & Compose + Channel Notification Feed
            div { class: "grid-2", style: "flex: 1; min-height: 0;",
                // Gmail Quick Panel
                div { class: "panel-card", style: "min-height: 0;",
                    div { class: "panel-header",
                        div { class: "panel-title",
                            {svg_mail()}
                            "Gmail Inbox & Confirmed Send"
                        }
                        button {
                            class: "hud-btn primary",
                            disabled: gmail_loading,
                            onclick: {
                                let mut fetch = run_gmail_fetch.clone();
                                move |_| fetch(None)
                            },
                            {svg_refresh()}
                            "Unread"
                        }
                    }

                    div { class: "row-gap",
                        input {
                            id: "gmail-search-input",
                            class: "hud-input",
                            style: "flex: 1;",
                            spellcheck: "false",
                            placeholder: "IMAP search (e.g. UNSEEN, FROM \"alice@...\")",
                            oninput: move |evt| gmail_query.set(evt.value()),
                            onkeydown: {
                                let mut fetch = run_gmail_fetch.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter && !evt.data().is_composing() {
                                        fetch(Some(gmail_query.read().clone()));
                                    }
                                }
                            },
                        }
                        button {
                            class: "hud-btn",
                            disabled: gmail_loading,
                            onclick: {
                                let mut fetch = run_gmail_fetch.clone();
                                move |_| fetch(Some(gmail_query.read().clone()))
                            },
                            "Search"
                        }
                    }

                    if let Some(msg) = gmail_status_msg {
                        div { class: "muted", "{msg}" }
                    }

                    div { class: "scroll-y col-gap", style: "flex: 1;",
                        if emails.is_empty() {
                            div { class: "muted", "Click Unread or Search to fetch messages from Gmail." }
                        } else {
                            for mail in emails {
                                div {
                                    key: "{mail.uid}",
                                    style: "padding: 7px 9px; border-radius: 7px; background: rgba(4, 10, 20, 0.72); border: 1px solid rgba(0, 240, 255, 0.14);",
                                    div { style: "font-weight: 700; color: var(--cyan-neon);", "{mail.subject}" }
                                    div { class: "muted", "From: {mail.from}" }
                                    div { style: "font-size: 11.5px; margin-top: 3px;", "{mail.snippet}" }
                                }
                            }
                        }
                    }

                    // Quick Send (Always gated via ApprovalBroker)
                    div { class: "col-gap", style: "border-top: 1px solid rgba(0, 240, 255, 0.15); padding-top: 8px;",
                        div { class: "row-gap",
                            input {
                                id: "gmail-to-input",
                                class: "hud-input",
                                style: "flex: 1;",
                                spellcheck: "false",
                                placeholder: "To (email)",
                                oninput: move |evt| send_to.set(evt.value()),
                            }
                            input {
                                id: "gmail-subject-input",
                                class: "hud-input",
                                style: "flex: 1;",
                                spellcheck: "false",
                                placeholder: "Subject",
                                oninput: move |evt| send_subject.set(evt.value()),
                            }
                        }
                        div { class: "row-gap",
                            input {
                                id: "gmail-body-input",
                                class: "hud-input",
                                style: "flex: 1;",
                                spellcheck: "false",
                                placeholder: "Message body...",
                                oninput: move |evt| send_body.set(evt.value()),
                            }
                            button {
                                class: "hud-btn primary",
                                onclick: move |_| {
                                    let to = send_to.read().trim().to_string();
                                    let subj = send_subject.read().trim().to_string();
                                    let body = send_body.read().trim().to_string();
                                    if to.is_empty() || subj.is_empty() {
                                        return;
                                    }
                                    let core = ctx.core.clone();
                                    let session_id = ctx.active_session_id.read().clone();
                                    let mut ui_sig = ctx.ui_state;
                                    send_to.set(String::new());
                                    send_subject.set(String::new());
                                    send_body.set(String::new());
                                    clear_dom_input("gmail-to-input", false);
                                    clear_dom_input("gmail-subject-input", false);
                                    clear_dom_input("gmail-body-input", false);

                                    spawn(async move {
                                        let full_auto = core.config.read().map(|c| c.security_mode == SecurityMode::FullAuto).unwrap_or(false);
                                        let args = serde_json::json!({ "to": to, "subject": subj, "body": body });
                                        let decision = core.approval_broker.request_approval(
                                            &session_id,
                                            "gmail_send_confirmed",
                                            &args,
                                            "Confirm sending email via Gmail SMTP",
                                            RiskTier::RequiresApproval,
                                            full_auto,
                                        ).await;
                                        if decision.is_approved() {
                                            match core.channels.gmail().send_email(&to, &subj, &body).await {
                                                Ok(info) => ui_sig.write().gmail_status_msg = Some(info),
                                                Err(err) => ui_sig.write().gmail_status_msg = Some(format!("Send failed: {err}")),
                                            }
                                        } else {
                                            ui_sig.write().gmail_status_msg = Some("Email send cancelled.".to_string());
                                        }
                                    });
                                },
                                {svg_send()}
                                "Send"
                            }
                        }
                    }
                }

                // Channel Activity & Notifications Feed
                div { class: "panel-card", style: "min-height: 0;",
                    div { class: "panel-header",
                        div { class: "panel-title",
                            {svg_comms()}
                            "Telegram / Discord / Gmail Activity"
                        }
                        button {
                            class: "hud-btn",
                            onclick: move |_| ctx.ui_state.write().channel_notifications.clear(),
                            "Clear"
                        }
                    }
                    div { class: "scroll-y col-gap", style: "flex: 1;",
                        if notifications.is_empty() {
                            div { class: "muted", "No recent channel events." }
                        } else {
                            for toast in notifications {
                                div {
                                    key: "{toast.id}",
                                    style: "padding: 7px 9px; border-radius: 7px; background: rgba(4, 10, 20, 0.72); border: 1px solid rgba(0, 240, 255, 0.14);",
                                    div { class: "row-between",
                                        span { class: "chip", "{toast.channel}" }
                                        span { class: "muted", "{toast.timestamp}" }
                                    }
                                    div { style: "font-weight: 600; margin-top: 4px;", "{toast.title}" }
                                    div { class: "muted", "{toast.body}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
