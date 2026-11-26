use crate::components::{
    add_allowed_discord_user, add_allowed_telegram_chat, clear_dom_input,
    parse_discord_user_ids_input, parse_telegram_chat_ids_input, remove_allowed_discord_user,
    remove_allowed_telegram_chat, set_dom_input_value, svg_check, svg_discord, svg_eye,
    svg_eye_off, svg_key, svg_link, svg_plus, svg_refresh, svg_send, svg_settings, svg_shield,
    svg_telegram, svg_trash, svg_bolt, JarvisUiContext,
};
use dioxus::events::Key;
use dioxus::prelude::*;
use jarvis_core::providers::agy_cli::AgyCliBridge;
use jarvis_core::voice::VoicePipeline;
use jarvis_core::{
    secrets::keys, ChannelState, ProviderKind, SecurityMode, StreamEvent, UiCommand,
};
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
pub fn SettingsHudView() -> Element {
    let ctx = use_context::<JarvisUiContext>();
    let initial = ctx.core.config.read().map(|c| c.clone()).unwrap_or_default();

    let init_model = initial.active_model.clone();
    let init_agy_model = initial.agy_model.clone().unwrap_or_default();
    let init_openai_url = initial.openai_base_url.clone();
    let init_ollama_url = initial.ollama_base_url.clone();
    let init_workspace_root = initial.workspace_root.display().to_string();
    let init_stt_lang = initial.voice.stt_language.clone().unwrap_or_default();
    let init_local_stt_url = initial.voice.local_stt_url.clone();
    let init_discord_guild = initial
        .channels
        .discord_guild_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    let init_gmail_addr = initial.channels.gmail_address.clone().unwrap_or_default();
    let init_gmail_oauth_client_id = initial
        .channels
        .gmail_oauth_client_id
        .clone()
        .unwrap_or_default();

    let mut provider = use_signal(|| initial.active_provider);
    let mut preset_hint = use_signal(|| {
        if initial.active_provider == ProviderKind::AgyCli {
            "agy_cli"
        } else if initial.active_provider == ProviderKind::Ollama {
            "ollama"
        } else if initial.openai_base_url.contains("groq.com") {
            "groq"
        } else if initial.openai_base_url.contains("openai.com") {
            "openai"
        } else {
            ""
        }
    });
    let mut model = use_signal(|| init_model.clone());
    let mut agy_model = use_signal(|| init_agy_model.clone());
    let mut openai_url = use_signal(|| init_openai_url.clone());
    let mut ollama_url = use_signal(|| init_ollama_url.clone());
    let mut agy_effort = use_signal(|| initial.agy_effort.clone());
    let mut workspace_root = use_signal(|| init_workspace_root.clone());
    let mut security_mode = use_signal(|| initial.security_mode);

    // Voice device & STT extra settings
    let mut input_device = use_signal(|| initial.voice.input_device.clone().unwrap_or_default());
    let mut output_device = use_signal(|| initial.voice.output_device.clone().unwrap_or_default());
    let mut stt_lang = use_signal(|| init_stt_lang.clone());
    let mut local_stt_url = use_signal(|| init_local_stt_url.clone());

    // Channels config
    let mut discord_guild = use_signal(|| init_discord_guild.clone());
    let mut discord_auto_join_voice = use_signal(|| initial.channels.discord_auto_join_voice);
    let mut discord_add_input = use_signal(String::new);
    let mut telegram_add_input = use_signal(String::new);
    let mut gmail_addr = use_signal(|| init_gmail_addr.clone());
    let mut gmail_auth_mode = use_signal(|| initial.channels.gmail_auth_mode.clone());
    let mut gmail_oauth_client_id = use_signal(|| init_gmail_oauth_client_id.clone());

    // Masked Credential Vault inputs (Windows Credential Manager)
    let mut openai_key = use_signal(String::new);
    let mut groq_key = use_signal(String::new);
    let mut telegram_token = use_signal(String::new);
    let mut discord_token = use_signal(String::new);
    let mut gmail_app_pw = use_signal(String::new);
    let mut gmail_oauth_rt = use_signal(String::new);
    let mut gmail_oauth_secret = use_signal(String::new);

    let mut show_openai_key = use_signal(|| false);
    let mut show_groq_key = use_signal(|| false);
    let mut show_discord_token = use_signal(|| false);
    let mut show_telegram_token = use_signal(|| false);
    let mut show_gmail_secrets = use_signal(|| false);

    let mut secret_rev = use_signal(|| 0u64);
    let mut whitelist_rev = use_signal(|| 0u64);
    let mut pairing_tick = use_signal(|| 0u64);

    let mut save_banner = use_signal(|| Option::<String>::None);
    let mut testing_agy = use_signal(|| false);

    let mut input_devices = use_signal(|| VoicePipeline::list_input_devices().unwrap_or_default());
    let mut output_devices = use_signal(|| VoicePipeline::list_output_devices().unwrap_or_default());

    let _sec_rev = *secret_rev.read();
    let _wl_rev = *whitelist_rev.read();
    let _pair_tick = *pairing_tick.read();

    let secrets_check = ctx.core.secrets.clone();
    let has_secret = move |k: &str| secrets_check.has(k).unwrap_or(false);

    let save_secret_key = {
        let secrets_save = ctx.core.secrets.clone();
        move |key: &'static str, val: String, label: &'static str| -> bool {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                return false;
            }
            match secrets_save.set(key, trimmed) {
                Ok(()) => {
                    secret_rev.with_mut(|v| *v = v.wrapping_add(1));
                    save_banner.set(Some(format!(
                        "{label} saved securely to Windows Credential Manager."
                    )));
                    true
                }
                Err(err) => {
                    save_banner.set(Some(format!("Failed to save credential ({label}): {err}")));
                    false
                }
            }
        }
    };

    let delete_secret_key = {
        let secrets_del = ctx.core.secrets.clone();
        move |key: &'static str, label: &'static str| {
            match secrets_del.delete(key) {
                Ok(()) => {
                    secret_rev.with_mut(|v| *v = v.wrapping_add(1));
                    save_banner.set(Some(format!(
                        "Removed {label} from Windows Credential Manager."
                    )));
                }
                Err(err) => save_banner.set(Some(format!("Failed to delete credential ({label}): {err}"))),
            }
        }
    };

    let ui_snap = ctx.ui_state.read();
    let discord_status = ui_snap
        .channel_statuses
        .iter()
        .find(|s| s.channel_id == "discord")
        .cloned()
        .unwrap_or_else(|| jarvis_core::ChannelStatusInfo::offline("discord"));
    let telegram_status = ui_snap
        .channel_statuses
        .iter()
        .find(|s| s.channel_id == "telegram")
        .cloned()
        .unwrap_or_else(|| jarvis_core::ChannelStatusInfo::offline("telegram"));
    let discord_candidates: Vec<_> = ui_snap
        .pairing_candidates
        .iter()
        .filter(|c| c.channel == "discord")
        .cloned()
        .collect();
    let telegram_candidates: Vec<_> = ui_snap
        .pairing_candidates
        .iter()
        .filter(|c| c.channel == "telegram")
        .cloned()
        .collect();
    drop(ui_snap);

    let (allowed_discord_users, allowed_telegram_chats) = ctx
        .core
        .config
        .read()
        .map(|c| {
            (
                c.channels.discord_allowed_user_ids.clone(),
                c.channels.telegram_allowed_chat_ids.clone(),
            )
        })
        .unwrap_or_default();

    let tg_pairing_rem = ctx.core.channels.telegram().pairing_remaining_secs();

    let trigger_discord_save_connect = {
        let core = ctx.core.clone();
        let mut save_sec = save_secret_key.clone();
        let mut ui_sig = ctx.ui_state;
        move || {
            let val = discord_token.read().clone();
            let has_new = !val.trim().is_empty();
            if has_new {
                discord_token.set(String::new());
                clear_dom_input("vault-discord-token", false);
                if !save_sec(keys::DISCORD_BOT_TOKEN, val, "Discord Bot Token") {
                    return;
                }
            }
            let core_async = core.clone();
            spawn(async move {
                if has_new {
                    let _ = core_async.channels.stop_channel("discord").await;
                }
                match core_async.channels.start_channel("discord").await {
                    Ok(()) => {
                        save_banner.set(Some("Discord Bot connected successfully.".to_string()));
                    }
                    Err(err) => {
                        let msg = format!("Discord connection failed: {err}");
                        save_banner.set(Some(msg.clone()));
                        ui_sig.write().toast_banner = Some(msg);
                    }
                }
            });
        }
    };

    let trigger_telegram_save_connect = {
        let core = ctx.core.clone();
        let mut save_sec = save_secret_key.clone();
        let mut ui_sig = ctx.ui_state;
        move || {
            let val = telegram_token.read().clone();
            let has_new = !val.trim().is_empty();
            if has_new {
                telegram_token.set(String::new());
                clear_dom_input("vault-telegram-token", false);
                if !save_sec(keys::TELEGRAM_BOT_TOKEN, val, "Telegram Bot Token") {
                    return;
                }
            }
            let core_async = core.clone();
            spawn(async move {
                if has_new {
                    let _ = core_async.channels.stop_channel("telegram").await;
                }
                match core_async.channels.start_channel("telegram").await {
                    Ok(()) => {
                        save_banner.set(Some("Telegram Bot connected successfully.".to_string()));
                    }
                    Err(err) => {
                        let msg = format!("Telegram connection failed: {err}");
                        save_banner.set(Some(msg.clone()));
                        ui_sig.write().toast_banner = Some(msg);
                    }
                }
            });
        }
    };

    let add_discord_users_from_input = {
        let mut c = ctx.clone();
        move || {
            let raw = discord_add_input.read().clone();
            let ids = parse_discord_user_ids_input(&raw);
            if ids.is_empty() {
                save_banner.set(Some(
                    "Please enter a valid Discord User ID (e.g. 362150827776933888 or @mention)."
                        .to_string(),
                ));
                return;
            }
            let mut count = 0usize;
            for uid in ids {
                if add_allowed_discord_user(&mut c, uid).is_ok() {
                    count += 1;
                }
            }
            discord_add_input.set(String::new());
            clear_dom_input("discord-add-user-input", true);
            whitelist_rev.with_mut(|v| *v = v.wrapping_add(1));
            save_banner.set(Some(format!(
                "Added {count} Discord User ID(s) to whitelist."
            )));
        }
    };

    let add_telegram_chats_from_input = {
        let mut c = ctx.clone();
        move || {
            let raw = telegram_add_input.read().clone();
            let ids = parse_telegram_chat_ids_input(&raw);
            if ids.is_empty() {
                save_banner.set(Some(
                    "Please enter a valid Telegram Chat ID (e.g. 123456789 or -100...)."
                        .to_string(),
                ));
                return;
            }
            let mut count = 0usize;
            for cid in ids {
                if add_allowed_telegram_chat(&mut c, cid).is_ok() {
                    count += 1;
                }
            }
            telegram_add_input.set(String::new());
            clear_dom_input("telegram-add-chat-input", true);
            whitelist_rev.with_mut(|v| *v = v.wrapping_add(1));
            save_banner.set(Some(format!(
                "Added {count} Telegram Chat ID(s) to whitelist."
            )));
        }
    };

    let save_discord_guild_only = {
        let core = ctx.core.clone();
        move || {
            let trimmed = discord_guild.read().trim().to_string();
            let parsed = if trimmed.is_empty() {
                None
            } else {
                match trimmed.parse::<u64>() {
                    Ok(id) if id > 0 => Some(id),
                    _ => {
                        save_banner.set(Some("Invalid Discord Guild ID.".to_string()));
                        return;
                    }
                }
            };
            if let Ok(mut cfg) = core.config.write() {
                cfg.channels.discord_guild_id = parsed;
                match cfg.save() {
                    Ok(()) => save_banner.set(Some("Saved Discord Guild ID.".to_string())),
                    Err(e) => save_banner.set(Some(format!("Config save error: {e}"))),
                }
            }
        }
    };

    rsx! {
        div { class: "tab-scroll-page",
            if let Some(msg) = save_banner.read().as_ref() {
                div { class: "toast-strip", style: "border-radius: 8px;",
                    span { "{msg}" }
                }
            }

            // ================================================================
            // TOP ROW: AI Provider & API Keys | Security & Audio Devices
            // ================================================================
            div { class: "grid-2",
                // Left: AI Provider & Runtime + LLM & STT API Keys
                div { class: "panel-card",
                    div { class: "panel-header",
                        div { class: "panel-title",
                            {svg_settings()}
                            "AI Provider & Runtime"
                        }
                        button {
                            class: "icon-btn",
                            "data-tooltip": "Test AGY CLI",
                            "data-tooltip-pos": "bottom-right",
                            disabled: *testing_agy.read(),
                            onclick: move |_| {
                                if *testing_agy.read() {
                                    return;
                                }
                                let ws_raw = workspace_root.read().trim().to_string();
                                let ws_path = if ws_raw.is_empty() {
                                    std::path::PathBuf::from(".")
                                } else {
                                    std::path::PathBuf::from(&ws_raw)
                                };
                                let effort = agy_effort.read().clone();
                                let mdl = agy_model.read().trim().to_string();
                                let full_auto = *security_mode.read() == SecurityMode::FullAuto;
                                testing_agy.set(true);
                                save_banner.set(Some("Testing AGY CLI (running prompt \"Reply with READY\")...".to_string()));
                                spawn(async move {
                                    let started = std::time::Instant::now();
                                    let bridge = AgyCliBridge::new(ws_path, effort, full_auto);
                                    match bridge.run_prompt("Reply with READY", &mdl).await {
                                        Ok(mut rx) => {
                                            let mut text = String::new();
                                            let mut err_msg = None;
                                            while let Some(ev) = rx.recv().await {
                                                match ev {
                                                    StreamEvent::TextDelta(delta) => text.push_str(&delta),
                                                    StreamEvent::Error(err) => err_msg = Some(err),
                                                    StreamEvent::Done => break,
                                                    _ => {}
                                                }
                                            }
                                            let elapsed_ms = started.elapsed().as_millis();
                                            if let Some(err) = err_msg {
                                                save_banner.set(Some(format!(
                                                    "AGY CLI error ({elapsed_ms} ms): {err}"
                                                )));
                                            } else {
                                                let trimmed = text.trim();
                                                let preview = if trimmed.is_empty() {
                                                    "(empty response)".to_string()
                                                } else {
                                                    trimmed.chars().take(160).collect()
                                                };
                                                save_banner.set(Some(format!(
                                                    "AGY CLI OK ({elapsed_ms} ms): {preview}"
                                                )));
                                            }
                                        }
                                        Err(err) => {
                                            let elapsed_ms = started.elapsed().as_millis();
                                            save_banner.set(Some(format!(
                                                "AGY CLI launch failed ({elapsed_ms} ms): {err}"
                                            )));
                                        }
                                    }
                                    testing_agy.set(false);
                                });
                            },
                            {svg_bolt()}
                        }
                    }
                    div { class: "col-gap",
                        label { class: "muted", "Quick Provider Presets" }
                        div { class: "row-gap", style: "flex-wrap: wrap;",
                            button {
                                class: if *provider.read() == ProviderKind::OpenAiCompat && *preset_hint.read() == "groq" {
                                    "hud-btn active"
                                } else {
                                    "hud-btn"
                                },
                                onclick: move |_| {
                                    provider.set(ProviderKind::OpenAiCompat);
                                    openai_url.set("https://api.groq.com/openai/v1".to_string());
                                    model.set("openai/gpt-oss-20b".to_string());
                                    preset_hint.set("groq");
                                    set_dom_input_value("settings-openai-url-input", "https://api.groq.com/openai/v1");
                                    set_dom_input_value("settings-model-input", "openai/gpt-oss-20b");
                                },
                                "Groq Cloud"
                            }
                            button {
                                class: if *provider.read() == ProviderKind::OpenAiCompat && *preset_hint.read() == "openai" {
                                    "hud-btn active"
                                } else {
                                    "hud-btn"
                                },
                                onclick: move |_| {
                                    provider.set(ProviderKind::OpenAiCompat);
                                    openai_url.set("https://api.openai.com/v1".to_string());
                                    model.set("gpt-4o-mini".to_string());
                                    preset_hint.set("openai");
                                    set_dom_input_value("settings-openai-url-input", "https://api.openai.com/v1");
                                    set_dom_input_value("settings-model-input", "gpt-4o-mini");
                                },
                                "OpenAI"
                            }
                            button {
                                class: if *provider.read() == ProviderKind::AgyCli {
                                    "hud-btn active"
                                } else {
                                    "hud-btn"
                                },
                                onclick: move |_| {
                                    provider.set(ProviderKind::AgyCli);
                                    preset_hint.set("agy_cli");
                                },
                                "AGY CLI"
                            }
                            button {
                                class: if *provider.read() == ProviderKind::Ollama {
                                    "hud-btn active"
                                } else {
                                    "hud-btn"
                                },
                                onclick: move |_| {
                                    provider.set(ProviderKind::Ollama);
                                    ollama_url.set("http://127.0.0.1:11434".to_string());
                                    model.set("qwen2.5:14b".to_string());
                                    preset_hint.set("ollama");
                                    set_dom_input_value("settings-ollama-url-input", "http://127.0.0.1:11434");
                                    set_dom_input_value("settings-model-input", "qwen2.5:14b");
                                },
                                "Local Ollama"
                            }
                        }

                        label { class: "muted", "Active Provider" }
                        select {
                            class: "hud-select",
                            value: match *provider.read() {
                                ProviderKind::OpenAiCompat => "openai_compat",
                                ProviderKind::Ollama => "ollama",
                                ProviderKind::AgyCli => "agy_cli",
                            },
                            onchange: move |evt| {
                                let kind = match evt.value().as_str() {
                                    "ollama" => {
                                        preset_hint.set("ollama");
                                        ProviderKind::Ollama
                                    }
                                    "agy_cli" => {
                                        preset_hint.set("agy_cli");
                                        ProviderKind::AgyCli
                                    }
                                    _ => {
                                        preset_hint.set("");
                                        ProviderKind::OpenAiCompat
                                    }
                                };
                                provider.set(kind);
                            },
                            option { value: "openai_compat", "OpenAI-Compatible (OpenAI / Groq / DeepSeek)" }
                            option { value: "ollama", "Local Ollama (NDJSON)" }
                            option { value: "agy_cli", "AGY CLI (Antigravity Agent)" }
                        }

                        label { class: "muted", "Model" }
                        input {
                            id: "settings-model-input",
                            class: "hud-input",
                            spellcheck: "false",
                            initial_value: "{init_model}",
                            oninput: move |evt| model.set(evt.value()),
                        }

                        div { class: "grid-2",
                            div { class: "col-gap",
                                label { class: "muted", "OpenAI-Compat Base URL" }
                                input {
                                    id: "settings-openai-url-input",
                                    class: "hud-input",
                                    spellcheck: "false",
                                    initial_value: "{init_openai_url}",
                                    oninput: move |evt| openai_url.set(evt.value()),
                                }
                            }
                            div { class: "col-gap",
                                label { class: "muted", "Ollama Base URL" }
                                input {
                                    id: "settings-ollama-url-input",
                                    class: "hud-input",
                                    spellcheck: "false",
                                    initial_value: "{init_ollama_url}",
                                    oninput: move |evt| ollama_url.set(evt.value()),
                                }
                            }
                        }

                        div { class: "grid-2",
                            div { class: "col-gap",
                                label { class: "muted", "AGY Model Override (optional)" }
                                input {
                                    id: "settings-agy-model-input",
                                    class: "hud-input",
                                    spellcheck: "false",
                                    placeholder: "auto / leave empty for default",
                                    initial_value: "{init_agy_model}",
                                    oninput: move |evt| agy_model.set(evt.value()),
                                }
                            }
                            div { class: "col-gap",
                                label { class: "muted", "AGY Effort" }
                                select {
                                    class: "hud-select",
                                    value: "{agy_effort}",
                                    onchange: move |evt| agy_effort.set(evt.value()),
                                    option { value: "low", "low" }
                                    option { value: "medium", "medium" }
                                    option { value: "high", "high" }
                                }
                            }
                        }

                        div { class: "col-gap",
                            label { class: "muted", "Workspace Root" }
                            input {
                                id: "settings-workspace-input",
                                class: "hud-input",
                                spellcheck: "false",
                                initial_value: "{init_workspace_root}",
                                oninput: move |evt| workspace_root.set(evt.value()),
                            }
                        }

                        // Compact LLM & STT API Keys
                        div { class: "col-gap", style: "margin-top: 4px; padding-top: 8px; border-top: 1px solid rgba(0, 240, 255, 0.14);",
                            label { class: "muted", "LLM & STT API Keys (Windows Credential Manager)" }
                            // OpenAI-Compat Key
                            div { class: "row-gap",
                                {svg_key()}
                                span {
                                    class: if has_secret(keys::OPENAI_API_KEY) { "vault-status-pill saved" } else { "vault-status-pill missing" },
                                    if has_secret(keys::OPENAI_API_KEY) { "● Saved" } else { "○ Missing" }
                                }
                                input {
                                    id: "vault-openai-key",
                                    class: "hud-input",
                                    style: "flex: 1; min-width: 0;",
                                    spellcheck: "false",
                                    r#type: if *show_openai_key.read() { "text" } else { "password" },
                                    placeholder: "OpenAI-Compat API Key",
                                    oninput: move |evt| openai_key.set(evt.value()),
                                    onkeydown: {
                                        let mut save = save_secret_key.clone();
                                        move |evt: KeyboardEvent| {
                                            if evt.key() == Key::Enter && !evt.data().is_composing() {
                                                let val = openai_key.read().clone();
                                                openai_key.set(String::new());
                                                clear_dom_input("vault-openai-key", false);
                                                save(keys::OPENAI_API_KEY, val, "OpenAI API Key");
                                            }
                                        }
                                    },
                                }
                                button {
                                    class: "icon-btn",
                                    "data-tooltip": if *show_openai_key.read() { "Hide" } else { "Reveal" },
                                    onclick: move |_| {
                                        let next = !*show_openai_key.read();
                                        show_openai_key.set(next);
                                    },
                                    if *show_openai_key.read() { {svg_eye_off()} } else { {svg_eye()} }
                                }
                                button {
                                    class: "hud-btn primary",
                                    onclick: {
                                        let mut save = save_secret_key.clone();
                                        move |_| {
                                            let val = openai_key.read().clone();
                                            openai_key.set(String::new());
                                            clear_dom_input("vault-openai-key", false);
                                            save(keys::OPENAI_API_KEY, val, "OpenAI API Key");
                                        }
                                    },
                                    "Store"
                                }
                                button {
                                    class: "icon-btn danger",
                                    "data-tooltip": "Delete OpenAI Key",
                                    onclick: {
                                        let mut del = delete_secret_key.clone();
                                        move |_| del(keys::OPENAI_API_KEY, "OpenAI API Key")
                                    },
                                    {svg_trash()}
                                }
                            }

                            // Groq Whisper Key
                            div { class: "row-gap",
                                {svg_key()}
                                span {
                                    class: if has_secret(keys::GROQ_API_KEY) { "vault-status-pill saved" } else { "vault-status-pill missing" },
                                    if has_secret(keys::GROQ_API_KEY) { "● Saved" } else { "○ Missing" }
                                }
                                input {
                                    id: "vault-groq-key",
                                    class: "hud-input",
                                    style: "flex: 1; min-width: 0;",
                                    spellcheck: "false",
                                    r#type: if *show_groq_key.read() { "text" } else { "password" },
                                    placeholder: "Groq Whisper API Key",
                                    oninput: move |evt| groq_key.set(evt.value()),
                                    onkeydown: {
                                        let mut save = save_secret_key.clone();
                                        move |evt: KeyboardEvent| {
                                            if evt.key() == Key::Enter && !evt.data().is_composing() {
                                                let val = groq_key.read().clone();
                                                groq_key.set(String::new());
                                                clear_dom_input("vault-groq-key", false);
                                                save(keys::GROQ_API_KEY, val, "Groq API Key");
                                            }
                                        }
                                    },
                                }
                                button {
                                    class: "icon-btn",
                                    "data-tooltip": if *show_groq_key.read() { "Hide" } else { "Reveal" },
                                    onclick: move |_| {
                                        let next = !*show_groq_key.read();
                                        show_groq_key.set(next);
                                    },
                                    if *show_groq_key.read() { {svg_eye_off()} } else { {svg_eye()} }
                                }
                                button {
                                    class: "hud-btn primary",
                                    onclick: {
                                        let mut save = save_secret_key.clone();
                                        move |_| {
                                            let val = groq_key.read().clone();
                                            groq_key.set(String::new());
                                            clear_dom_input("vault-groq-key", false);
                                            save(keys::GROQ_API_KEY, val, "Groq API Key");
                                        }
                                    },
                                    "Store"
                                }
                                button {
                                    class: "icon-btn danger",
                                    "data-tooltip": "Delete Groq Key",
                                    onclick: {
                                        let mut del = delete_secret_key.clone();
                                        move |_| del(keys::GROQ_API_KEY, "Groq API Key")
                                    },
                                    {svg_trash()}
                                }
                            }
                        }
                    }
                }

                // Right: Security Mode & Audio Devices
                div { class: "col-gap",
                    div { class: "panel-card",
                        div { class: "panel-header",
                            div { class: "panel-title",
                                {svg_shield()}
                                "Security & Risk Policy"
                            }
                        }
                        div { class: "row-gap",
                            button {
                                class: if *security_mode.read() == SecurityMode::SmartTiered { "hud-btn active" } else { "hud-btn" },
                                onclick: {
                                    let c = ctx.clone();
                                    move |_| {
                                        security_mode.set(SecurityMode::SmartTiered);
                                        c.send_command(UiCommand::SetSecurityMode { mode: SecurityMode::SmartTiered });
                                    }
                                },
                                "SmartTiered (Recommended)"
                            }
                            button {
                                class: if *security_mode.read() == SecurityMode::FullAuto { "hud-btn danger active" } else { "hud-btn" },
                                onclick: {
                                    let c = ctx.clone();
                                    move |_| {
                                        security_mode.set(SecurityMode::FullAuto);
                                        c.send_command(UiCommand::SetSecurityMode { mode: SecurityMode::FullAuto });
                                    }
                                },
                                "FullAuto (Skip Shell Prompts)"
                            }
                        }
                        div { class: "muted",
                            "Note: gmail_send_confirmed ALWAYS requires explicit approval even in FullAuto mode."
                        }
                    }

                    div { class: "panel-card",
                        div { class: "panel-header",
                            span { class: "panel-title", "Audio Devices & Local STT" }
                            button {
                                class: "icon-btn",
                                "data-tooltip": "Refresh Audio Devices",
                                "data-tooltip-pos": "bottom-right",
                                onclick: move |_| {
                                    let ins = VoicePipeline::list_input_devices().unwrap_or_default();
                                    let outs = VoicePipeline::list_output_devices().unwrap_or_default();
                                    let msg = format!(
                                        "Audio devices refreshed: {} input(s), {} output(s).",
                                        ins.len(),
                                        outs.len()
                                    );
                                    input_devices.set(ins);
                                    output_devices.set(outs);
                                    save_banner.set(Some(msg));
                                },
                                {svg_refresh()}
                            }
                        }
                        div { class: "grid-2",
                            div { class: "col-gap",
                                label { class: "muted", "Input Microphone" }
                                select {
                                    class: "hud-select",
                                    value: "{input_device}",
                                    onchange: move |evt| input_device.set(evt.value()),
                                    option { value: "", "System Default" }
                                    for dev in input_devices.read().iter() {
                                        option { key: "in-{dev.name}", value: "{dev.name}", "{dev.name}" }
                                    }
                                }
                            }
                            div { class: "col-gap",
                                label { class: "muted", "Output Speaker" }
                                select {
                                    class: "hud-select",
                                    value: "{output_device}",
                                    onchange: move |evt| output_device.set(evt.value()),
                                    option { value: "", "System Default" }
                                    for dev in output_devices.read().iter() {
                                        option { key: "out-{dev.name}", value: "{dev.name}", "{dev.name}" }
                                    }
                                }
                            }
                        }
                        div { class: "grid-2",
                            div { class: "col-gap",
                                label { class: "muted", "STT Language (e.g. vi, en)" }
                                input {
                                    id: "settings-stt-lang-input",
                                    class: "hud-input",
                                    spellcheck: "false",
                                    placeholder: "auto",
                                    initial_value: "{init_stt_lang}",
                                    oninput: move |evt| stt_lang.set(evt.value()),
                                }
                            }
                            div { class: "col-gap",
                                label { class: "muted", "Local Whisper HTTP URL" }
                                input {
                                    id: "settings-local-stt-url-input",
                                    class: "hud-input",
                                    spellcheck: "false",
                                    initial_value: "{init_local_stt_url}",
                                    oninput: move |evt| local_stt_url.set(evt.value()),
                                }
                            }
                        }
                    }
                }
            }

            // ================================================================
            // MIDDLE ROW: Dedicated DISCORD BOT & TELEGRAM BOT Setup Cards
            // ================================================================
            div { class: "grid-2",
                // Left Card: DISCORD BOT
                div { class: "bot-config-card",
                    div { class: "panel-header", style: "flex-wrap: wrap;",
                        div { class: "row-gap",
                            span { class: led_class(discord_status.state) }
                            div { class: "panel-title",
                                {svg_discord()}
                                "DISCORD BOT"
                            }
                            if let Some(identity) = &discord_status.connected_as {
                                span { class: "chip", "@{identity}" }
                            }
                        }
                        div { class: "row-gap",
                            button {
                                class: if *discord_auto_join_voice.read() { "hud-btn active" } else { "hud-btn" },
                                onclick: {
                                    let core = ctx.core.clone();
                                    move |_| {
                                        let next = !*discord_auto_join_voice.read();
                                        discord_auto_join_voice.set(next);
                                        if let Ok(mut cfg) = core.config.write() {
                                            cfg.channels.discord_auto_join_voice = next;
                                            let _ = cfg.save();
                                        }
                                        save_banner.set(Some(format!(
                                            "Discord Auto-Follow Voice: {}",
                                            if next { "ON" } else { "OFF" }
                                        )));
                                    }
                                },
                                if *discord_auto_join_voice.read() { "Auto-Follow Voice: ON" } else { "Auto-Follow Voice: OFF" }
                            }
                            if discord_status.state == ChannelState::Online || discord_status.state == ChannelState::Connecting {
                                button {
                                    class: "hud-btn danger",
                                    onclick: {
                                        let core = ctx.core.clone();
                                        move |_| {
                                            let core = core.clone();
                                            spawn(async move {
                                                let _ = core.channels.stop_channel("discord").await;
                                            });
                                        }
                                    },
                                    "Disconnect"
                                }
                            } else {
                                button {
                                    class: "hud-btn primary",
                                    onclick: {
                                        let core = ctx.core.clone();
                                        let mut ui_sig = ctx.ui_state;
                                        move |_| {
                                            let core = core.clone();
                                            spawn(async move {
                                                if let Err(err) = core.channels.start_channel("discord").await {
                                                    let msg = format!("Discord start failed: {err}");
                                                    save_banner.set(Some(msg.clone()));
                                                    ui_sig.write().toast_banner = Some(msg);
                                                }
                                            });
                                        }
                                    },
                                    "Connect"
                                }
                            }
                        }
                    }

                    if let Some(err) = &discord_status.last_error {
                        div { style: "color: var(--red-danger); font-size: 11px;", "{err}" }
                    }

                    // Discord Bot Token Row
                    div { class: "col-gap",
                        label { class: "muted", "Discord Bot Token (Windows Credential Manager)" }
                        div { class: "row-gap",
                            {svg_key()}
                            span {
                                class: if has_secret(keys::DISCORD_BOT_TOKEN) { "vault-status-pill saved" } else { "vault-status-pill missing" },
                                if has_secret(keys::DISCORD_BOT_TOKEN) { "● Saved" } else { "○ Missing" }
                            }
                            input {
                                id: "vault-discord-token",
                                class: "hud-input",
                                style: "flex: 1; min-width: 0;",
                                spellcheck: "false",
                                r#type: if *show_discord_token.read() { "text" } else { "password" },
                                placeholder: "Paste Discord Bot Token...",
                                oninput: move |evt| discord_token.set(evt.value()),
                                onkeydown: {
                                    let mut act = trigger_discord_save_connect.clone();
                                    move |evt: KeyboardEvent| {
                                        if evt.key() == Key::Enter && !evt.data().is_composing() {
                                            act();
                                        }
                                    }
                                },
                            }
                            button {
                                class: "icon-btn",
                                "data-tooltip": if *show_discord_token.read() { "Hide Token" } else { "Reveal Token" },
                                onclick: move |_| {
                                    let next = !*show_discord_token.read();
                                    show_discord_token.set(next);
                                },
                                if *show_discord_token.read() { {svg_eye_off()} } else { {svg_eye()} }
                            }
                            button {
                                class: "hud-btn primary",
                                onclick: {
                                    let mut act = trigger_discord_save_connect.clone();
                                    move |_| act()
                                },
                                {svg_link()}
                                "Save & Connect"
                            }
                            button {
                                class: "icon-btn danger",
                                "data-tooltip": "Delete Discord Token",
                                onclick: {
                                    let mut del = delete_secret_key.clone();
                                    let core = ctx.core.clone();
                                    move |_| {
                                        del(keys::DISCORD_BOT_TOKEN, "Discord Bot Token");
                                        let core = core.clone();
                                        spawn(async move {
                                            let _ = core.channels.stop_channel("discord").await;
                                        });
                                    }
                                },
                                {svg_trash()}
                            }
                        }
                    }

                    // Discord Allowed User IDs
                    div { class: "col-gap",
                        label { class: "muted", "Allowed User IDs (Whitelist)" }
                        div { class: "id-chip-list",
                            if allowed_discord_users.is_empty() {
                                span { class: "muted", "No User IDs added yet — add an ID below or message the bot to pair." }
                            } else {
                                for uid in allowed_discord_users {
                                    div { class: "id-chip", key: "dc-uid-{uid}",
                                        span { "{uid}" }
                                        button {
                                            class: "id-chip-remove",
                                            "data-tooltip": "Remove User ID",
                                            onclick: {
                                                let mut c = ctx.clone();
                                                move |_| {
                                                    if let Err(err) = remove_allowed_discord_user(&mut c, uid) {
                                                        save_banner.set(Some(err));
                                                    } else {
                                                        whitelist_rev.with_mut(|v| *v = v.wrapping_add(1));
                                                    }
                                                }
                                            },
                                            "×"
                                        }
                                    }
                                }
                            }
                        }

                        if !discord_candidates.is_empty() {
                            div { class: "id-chip-list",
                                for cand in discord_candidates {
                                    {
                                        let sid = cand.sender_id.clone();
                                        let dname = cand.display_name.clone();
                                        let preview = cand.preview.clone();
                                        rsx! {
                                            button {
                                                key: "dc-cand-{sid}",
                                                class: "pairing-chip",
                                                "data-tooltip": "{preview}",
                                                onclick: {
                                                    let mut c = ctx.clone();
                                                    let sid_click = sid.clone();
                                                    move |_| {
                                                        if let Ok(uid) = sid_click.parse::<u64>() {
                                                            match add_allowed_discord_user(&mut c, uid) {
                                                                Ok(()) => {
                                                                    whitelist_rev.with_mut(|v| *v = v.wrapping_add(1));
                                                                    save_banner.set(Some(format!("Allowed Discord User {uid}")));
                                                                }
                                                                Err(err) => save_banner.set(Some(err)),
                                                            }
                                                        }
                                                    }
                                                },
                                                "+ Allow {sid} ({dname})"
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        div { class: "row-gap",
                            input {
                                id: "discord-add-user-input",
                                class: "hud-input",
                                style: "flex: 1;",
                                spellcheck: "false",
                                placeholder: "Add User ID (e.g. 362150827776933888 or @mention)...",
                                oninput: move |evt| discord_add_input.set(evt.value()),
                                onkeydown: {
                                    let mut add_fn = add_discord_users_from_input.clone();
                                    move |evt: KeyboardEvent| {
                                        if evt.key() == Key::Enter && !evt.data().is_composing() {
                                            add_fn();
                                        }
                                    }
                                },
                            }
                            button {
                                class: "hud-btn primary",
                                onclick: {
                                    let mut add_fn = add_discord_users_from_input.clone();
                                    move |_| add_fn()
                                },
                                {svg_plus()}
                            }
                        }
                    }

                    // Optional Default Server (Guild ID)
                    div { class: "col-gap",
                        label { class: "muted", "Default Server (Guild ID - optional)" }
                        input {
                            id: "settings-discord-guild-input",
                            class: "hud-input",
                            spellcheck: "false",
                            placeholder: "Auto-detect any server (enter Guild ID to pin)",
                            initial_value: "{init_discord_guild}",
                            oninput: move |evt| discord_guild.set(evt.value()),
                            onkeydown: {
                                let mut save_guild = save_discord_guild_only.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter && !evt.data().is_composing() {
                                        save_guild();
                                    }
                                }
                            },
                        }
                    }
                }

                // Right Card: TELEGRAM BOT
                div { class: "bot-config-card",
                    div { class: "panel-header", style: "flex-wrap: wrap;",
                        div { class: "row-gap",
                            span { class: led_class(telegram_status.state) }
                            div { class: "panel-title",
                                {svg_telegram()}
                                "TELEGRAM BOT"
                            }
                            if let Some(identity) = &telegram_status.connected_as {
                                span { class: "chip", "@{identity}" }
                            }
                        }
                        div { class: "row-gap",
                            button {
                                class: if tg_pairing_rem.is_some() { "hud-btn active" } else { "hud-btn" },
                                onclick: {
                                    let core = ctx.core.clone();
                                    let mut ui_sig = ctx.ui_state;
                                    move |_| {
                                        core.channels.telegram().enable_pairing_window(Duration::from_secs(60));
                                        pairing_tick.with_mut(|v| *v = v.wrapping_add(1));
                                        save_banner.set(Some(
                                            "Enabled /start pairing for 60s — send /start to your Telegram bot now."
                                                .to_string(),
                                        ));
                                        let core_async = core.clone();
                                        spawn(async move {
                                            if core_async.channels.telegram().status().state == ChannelState::Offline {
                                                if let Err(err) = core_async.channels.start_channel("telegram").await {
                                                    ui_sig.write().toast_banner = Some(format!("Telegram start failed: {err}"));
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
                                    "Waiting for /start ({rem}s)"
                                } else {
                                    "Pair /start (60s)"
                                }
                            }
                            button {
                                class: "icon-btn",
                                "data-tooltip": "Send Test Ping",
                                onclick: {
                                    let core = ctx.core.clone();
                                    let mut ui_sig = ctx.ui_state;
                                    move |_| {
                                        let core = core.clone();
                                        spawn(async move {
                                            match core.channels.telegram().send_test_ping().await {
                                                Ok(cnt) => {
                                                    let msg = format!("Sent test ping to {cnt} Telegram chat(s).");
                                                    save_banner.set(Some(msg.clone()));
                                                    ui_sig.write().toast_banner = Some(msg);
                                                }
                                                Err(err) => {
                                                    let msg = format!("Test Ping failed: {err}");
                                                    save_banner.set(Some(msg.clone()));
                                                    ui_sig.write().toast_banner = Some(msg);
                                                }
                                            }
                                        });
                                    }
                                },
                                {svg_send()}
                            }
                            if telegram_status.state == ChannelState::Online || telegram_status.state == ChannelState::Connecting {
                                button {
                                    class: "hud-btn danger",
                                    onclick: {
                                        let core = ctx.core.clone();
                                        move |_| {
                                            let core = core.clone();
                                            spawn(async move {
                                                let _ = core.channels.stop_channel("telegram").await;
                                            });
                                        }
                                    },
                                    "Disconnect"
                                }
                            } else {
                                button {
                                    class: "hud-btn primary",
                                    onclick: {
                                        let core = ctx.core.clone();
                                        let mut ui_sig = ctx.ui_state;
                                        move |_| {
                                            let core = core.clone();
                                            spawn(async move {
                                                if let Err(err) = core.channels.start_channel("telegram").await {
                                                    let msg = format!("Telegram start failed: {err}");
                                                    save_banner.set(Some(msg.clone()));
                                                    ui_sig.write().toast_banner = Some(msg);
                                                }
                                            });
                                        }
                                    },
                                    "Connect"
                                }
                            }
                        }
                    }

                    if let Some(err) = &telegram_status.last_error {
                        div { style: "color: var(--red-danger); font-size: 11px;", "{err}" }
                    }

                    // Telegram Bot Token Row
                    div { class: "col-gap",
                        label { class: "muted", "Telegram Bot Token (Windows Credential Manager)" }
                        div { class: "row-gap",
                            {svg_key()}
                            span {
                                class: if has_secret(keys::TELEGRAM_BOT_TOKEN) { "vault-status-pill saved" } else { "vault-status-pill missing" },
                                if has_secret(keys::TELEGRAM_BOT_TOKEN) { "● Saved" } else { "○ Missing" }
                            }
                            input {
                                id: "vault-telegram-token",
                                class: "hud-input",
                                style: "flex: 1; min-width: 0;",
                                spellcheck: "false",
                                r#type: if *show_telegram_token.read() { "text" } else { "password" },
                                placeholder: "Paste Telegram Bot Token...",
                                oninput: move |evt| telegram_token.set(evt.value()),
                                onkeydown: {
                                    let mut act = trigger_telegram_save_connect.clone();
                                    move |evt: KeyboardEvent| {
                                        if evt.key() == Key::Enter && !evt.data().is_composing() {
                                            act();
                                        }
                                    }
                                },
                            }
                            button {
                                class: "icon-btn",
                                "data-tooltip": if *show_telegram_token.read() { "Hide Token" } else { "Reveal Token" },
                                onclick: move |_| {
                                    let next = !*show_telegram_token.read();
                                    show_telegram_token.set(next);
                                },
                                if *show_telegram_token.read() { {svg_eye_off()} } else { {svg_eye()} }
                            }
                            button {
                                class: "hud-btn primary",
                                onclick: {
                                    let mut act = trigger_telegram_save_connect.clone();
                                    move |_| act()
                                },
                                {svg_link()}
                                "Save & Connect"
                            }
                            button {
                                class: "icon-btn danger",
                                "data-tooltip": "Delete Telegram Token",
                                onclick: {
                                    let mut del = delete_secret_key.clone();
                                    let core = ctx.core.clone();
                                    move |_| {
                                        del(keys::TELEGRAM_BOT_TOKEN, "Telegram Bot Token");
                                        let core = core.clone();
                                        spawn(async move {
                                            let _ = core.channels.stop_channel("telegram").await;
                                        });
                                    }
                                },
                                {svg_trash()}
                            }
                        }
                    }

                    // Telegram Allowed Chat IDs
                    div { class: "col-gap",
                        label { class: "muted", "Allowed Chat IDs (Whitelist)" }
                        div { class: "id-chip-list",
                            if allowed_telegram_chats.is_empty() {
                                span { class: "muted", "No Chat IDs added yet — click 'Pair /start (60s)' and send /start to your Telegram bot." }
                            } else {
                                for cid in allowed_telegram_chats {
                                    div { class: "id-chip", key: "tg-cid-{cid}",
                                        span { "{cid}" }
                                        button {
                                            class: "id-chip-remove",
                                            "data-tooltip": "Remove Chat ID",
                                            onclick: {
                                                let mut c = ctx.clone();
                                                move |_| {
                                                    if let Err(err) = remove_allowed_telegram_chat(&mut c, cid) {
                                                        save_banner.set(Some(err));
                                                    } else {
                                                        whitelist_rev.with_mut(|v| *v = v.wrapping_add(1));
                                                    }
                                                }
                                            },
                                            "×"
                                        }
                                    }
                                }
                            }
                        }

                        if !telegram_candidates.is_empty() {
                            div { class: "id-chip-list",
                                for cand in telegram_candidates {
                                    {
                                        let sid = cand.sender_id.clone();
                                        let dname = cand.display_name.clone();
                                        let preview = cand.preview.clone();
                                        rsx! {
                                            button {
                                                key: "tg-cand-{sid}",
                                                class: "pairing-chip",
                                                "data-tooltip": "{preview}",
                                                onclick: {
                                                    let mut c = ctx.clone();
                                                    let sid_click = sid.clone();
                                                    move |_| {
                                                        if let Ok(cid) = sid_click.parse::<i64>() {
                                                            match add_allowed_telegram_chat(&mut c, cid) {
                                                                Ok(()) => {
                                                                    whitelist_rev.with_mut(|v| *v = v.wrapping_add(1));
                                                                    save_banner.set(Some(format!("Allowed Telegram Chat {cid}")));
                                                                }
                                                                Err(err) => save_banner.set(Some(err)),
                                                            }
                                                        }
                                                    }
                                                },
                                                "+ Allow {sid} ({dname})"
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        div { class: "row-gap",
                            input {
                                id: "telegram-add-chat-input",
                                class: "hud-input",
                                style: "flex: 1;",
                                spellcheck: "false",
                                placeholder: "Add Chat ID (e.g. 123456789 or -100...)...",
                                oninput: move |evt| telegram_add_input.set(evt.value()),
                                onkeydown: {
                                    let mut add_fn = add_telegram_chats_from_input.clone();
                                    move |evt: KeyboardEvent| {
                                        if evt.key() == Key::Enter && !evt.data().is_composing() {
                                            add_fn();
                                        }
                                    }
                                },
                            }
                            button {
                                class: "hud-btn primary",
                                onclick: {
                                    let mut add_fn = add_telegram_chats_from_input.clone();
                                    move |_| add_fn()
                                },
                                {svg_plus()}
                            }
                        }
                    }
                }
            }

            // ================================================================
            // BOTTOM CARD: Gmail & General Save
            // ================================================================
            div { class: "panel-card",
                div { class: "panel-header",
                    span { class: "panel-title", "Gmail & General Save" }
                    div { class: "row-gap",
                        button {
                            class: "hud-btn",
                            onclick: move |_| {
                                let next = !*show_gmail_secrets.read();
                                show_gmail_secrets.set(next);
                            },
                            if *show_gmail_secrets.read() { {svg_eye_off()} } else { {svg_eye()} }
                            if *show_gmail_secrets.read() { "Mask Gmail Secrets" } else { "Reveal Gmail Secrets" }
                        }
                        button {
                            class: "hud-btn primary",
                            onclick: {
                                let mut c = ctx.clone();
                                move |_| {
                                    let ws_raw = workspace_root.read().trim().to_string();
                                    if ws_raw.is_empty() {
                                        save_banner.set(Some(
                                            "Validation error: Workspace Root must not be empty.".to_string(),
                                        ));
                                        return;
                                    }
                                    let ws_path = std::path::PathBuf::from(&ws_raw);
                                    if !ws_path.exists() || !ws_path.is_dir() {
                                        save_banner.set(Some(format!(
                                            "Validation error: Workspace Root does not exist or is not a directory: {}",
                                            ws_path.display()
                                        )));
                                        return;
                                    }

                                    let prov = *provider.read();
                                    let mdl = model.read().trim().to_string();
                                    let save_res = if let Ok(mut cfg) = c.core.config.write() {
                                        cfg.active_provider = prov;
                                        cfg.active_model = mdl;
                                        cfg.agy_model = (!agy_model.read().trim().is_empty())
                                            .then(|| agy_model.read().trim().to_string());
                                        cfg.openai_base_url = openai_url.read().trim().to_string();
                                        cfg.ollama_base_url = ollama_url.read().trim().to_string();
                                        cfg.agy_effort = agy_effort.read().clone();
                                        cfg.workspace_root = ws_path;
                                        cfg.security_mode = *security_mode.read();

                                        cfg.voice.input_device = (!input_device.read().trim().is_empty())
                                            .then(|| input_device.read().trim().to_string());
                                        cfg.voice.output_device = (!output_device.read().trim().is_empty())
                                            .then(|| output_device.read().trim().to_string());
                                        cfg.voice.stt_language = (!stt_lang.read().trim().is_empty())
                                            .then(|| stt_lang.read().trim().to_string());
                                        cfg.voice.local_stt_url = local_stt_url.read().trim().to_string();

                                        // Keep live telegram_allowed_chat_ids & discord_allowed_user_ids untouched!
                                        cfg.channels.discord_guild_id = discord_guild
                                            .read()
                                            .trim()
                                            .parse::<u64>()
                                            .ok()
                                            .filter(|&id| id > 0);
                                        cfg.channels.discord_auto_join_voice = *discord_auto_join_voice.read();
                                        cfg.channels.gmail_address = (!gmail_addr.read().trim().is_empty())
                                            .then(|| gmail_addr.read().trim().to_string());
                                        cfg.channels.gmail_auth_mode = gmail_auth_mode.read().clone();
                                        cfg.channels.gmail_oauth_client_id =
                                            (!gmail_oauth_client_id.read().trim().is_empty())
                                                .then(|| gmail_oauth_client_id.read().trim().to_string());

                                        cfg.save()
                                    } else {
                                        save_banner.set(Some("Save error: config lock poisoned.".to_string()));
                                        return;
                                    };

                                    match save_res {
                                        Ok(()) => {
                                            let reload_res = c.core.voice.apply_updated_config();
                                            c.ui_state.write().voice_running = c.core.voice.is_running();
                                            match reload_res {
                                                Ok(_) => save_banner.set(Some(
                                                    "Configuration saved to config.toml.".to_string(),
                                                )),
                                                Err(err) => save_banner.set(Some(format!(
                                                    "Saved config.toml, but voice reload failed: {err}"
                                                ))),
                                            }
                                        }
                                        Err(err) => save_banner.set(Some(format!("Save error: {err}"))),
                                    }
                                }
                            },
                            {svg_check()}
                            "Save Config"
                        }
                    }
                }

                div { class: "grid-3",
                    div { class: "col-gap",
                        label { class: "muted", "Gmail Address" }
                        input {
                            id: "settings-gmail-addr-input",
                            class: "hud-input",
                            spellcheck: "false",
                            placeholder: "you@gmail.com",
                            initial_value: "{init_gmail_addr}",
                            oninput: move |evt| gmail_addr.set(evt.value()),
                        }
                    }
                    div { class: "col-gap",
                        label { class: "muted", "Gmail Auth Mode" }
                        select {
                            class: "hud-select",
                            value: "{gmail_auth_mode}",
                            onchange: move |evt| gmail_auth_mode.set(evt.value()),
                            option { value: "app_password", "App Password (TLS)" }
                            option { value: "oauth2", "OAuth2 (XOAUTH2)" }
                        }
                    }
                    div { class: "col-gap",
                        label { class: "muted", "Gmail OAuth2 Client ID" }
                        input {
                            id: "settings-gmail-oauth-client-input",
                            class: "hud-input",
                            spellcheck: "false",
                            placeholder: "123456-abc.apps.googleusercontent.com",
                            initial_value: "{init_gmail_oauth_client_id}",
                            oninput: move |evt| gmail_oauth_client_id.set(evt.value()),
                        }
                    }
                }

                div { class: "grid-3",
                    // Gmail App Password
                    div { class: "row-gap",
                        span {
                            class: if has_secret(keys::GMAIL_APP_PASSWORD) { "vault-status-pill saved" } else { "vault-status-pill missing" },
                            if has_secret(keys::GMAIL_APP_PASSWORD) { "● Saved" } else { "○ Missing" }
                        }
                        input {
                            id: "vault-gmail-app-pw",
                            class: "hud-input",
                            style: "flex: 1; min-width: 0;",
                            spellcheck: "false",
                            r#type: if *show_gmail_secrets.read() { "text" } else { "password" },
                            placeholder: "Gmail App Password",
                            oninput: move |evt| gmail_app_pw.set(evt.value()),
                            onkeydown: {
                                let mut save = save_secret_key.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter && !evt.data().is_composing() {
                                        let val = gmail_app_pw.read().clone();
                                        gmail_app_pw.set(String::new());
                                        clear_dom_input("vault-gmail-app-pw", false);
                                        save(keys::GMAIL_APP_PASSWORD, val, "Gmail App Password");
                                    }
                                }
                            },
                        }
                        button {
                            class: "hud-btn primary",
                            onclick: {
                                let mut save = save_secret_key.clone();
                                move |_| {
                                    let val = gmail_app_pw.read().clone();
                                    gmail_app_pw.set(String::new());
                                    clear_dom_input("vault-gmail-app-pw", false);
                                    save(keys::GMAIL_APP_PASSWORD, val, "Gmail App Password");
                                }
                            },
                            "Store"
                        }
                        button {
                            class: "icon-btn danger",
                            "data-tooltip": "Delete App Password",
                            onclick: {
                                let mut del = delete_secret_key.clone();
                                move |_| del(keys::GMAIL_APP_PASSWORD, "Gmail App Password")
                            },
                            {svg_trash()}
                        }
                    }

                    // Gmail OAuth2 Refresh Token
                    div { class: "row-gap",
                        span {
                            class: if has_secret(keys::GMAIL_OAUTH_REFRESH_TOKEN) { "vault-status-pill saved" } else { "vault-status-pill missing" },
                            if has_secret(keys::GMAIL_OAUTH_REFRESH_TOKEN) { "● Saved" } else { "○ Missing" }
                        }
                        input {
                            id: "vault-gmail-oauth-rt",
                            class: "hud-input",
                            style: "flex: 1; min-width: 0;",
                            spellcheck: "false",
                            r#type: if *show_gmail_secrets.read() { "text" } else { "password" },
                            placeholder: "Gmail OAuth2 Refresh Token",
                            oninput: move |evt| gmail_oauth_rt.set(evt.value()),
                            onkeydown: {
                                let mut save = save_secret_key.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter && !evt.data().is_composing() {
                                        let val = gmail_oauth_rt.read().clone();
                                        gmail_oauth_rt.set(String::new());
                                        clear_dom_input("vault-gmail-oauth-rt", false);
                                        save(keys::GMAIL_OAUTH_REFRESH_TOKEN, val, "Gmail OAuth2 Token");
                                    }
                                }
                            },
                        }
                        button {
                            class: "hud-btn primary",
                            onclick: {
                                let mut save = save_secret_key.clone();
                                move |_| {
                                    let val = gmail_oauth_rt.read().clone();
                                    gmail_oauth_rt.set(String::new());
                                    clear_dom_input("vault-gmail-oauth-rt", false);
                                    save(keys::GMAIL_OAUTH_REFRESH_TOKEN, val, "Gmail OAuth2 Token");
                                }
                            },
                            "Store"
                        }
                        button {
                            class: "icon-btn danger",
                            "data-tooltip": "Delete OAuth2 Token",
                            onclick: {
                                let mut del = delete_secret_key.clone();
                                move |_| del(keys::GMAIL_OAUTH_REFRESH_TOKEN, "Gmail OAuth2 Token")
                            },
                            {svg_trash()}
                        }
                    }

                    // Gmail OAuth2 Client Secret
                    div { class: "row-gap",
                        span {
                            class: if has_secret(keys::GMAIL_OAUTH_CLIENT_SECRET) { "vault-status-pill saved" } else { "vault-status-pill missing" },
                            if has_secret(keys::GMAIL_OAUTH_CLIENT_SECRET) { "● Saved" } else { "○ Missing" }
                        }
                        input {
                            id: "vault-gmail-oauth-secret",
                            class: "hud-input",
                            style: "flex: 1; min-width: 0;",
                            spellcheck: "false",
                            r#type: if *show_gmail_secrets.read() { "text" } else { "password" },
                            placeholder: "Gmail OAuth2 Client Secret",
                            oninput: move |evt| gmail_oauth_secret.set(evt.value()),
                            onkeydown: {
                                let mut save = save_secret_key.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter && !evt.data().is_composing() {
                                        let val = gmail_oauth_secret.read().clone();
                                        gmail_oauth_secret.set(String::new());
                                        clear_dom_input("vault-gmail-oauth-secret", false);
                                        save(keys::GMAIL_OAUTH_CLIENT_SECRET, val, "Gmail OAuth2 Client Secret");
                                    }
                                }
                            },
                        }
                        button {
                            class: "hud-btn primary",
                            onclick: {
                                let mut save = save_secret_key.clone();
                                move |_| {
                                    let val = gmail_oauth_secret.read().clone();
                                    gmail_oauth_secret.set(String::new());
                                    clear_dom_input("vault-gmail-oauth-secret", false);
                                    save(keys::GMAIL_OAUTH_CLIENT_SECRET, val, "Gmail OAuth2 Client Secret");
                                }
                            },
                            "Store"
                        }
                        button {
                            class: "icon-btn danger",
                            "data-tooltip": "Delete Client Secret",
                            onclick: {
                                let mut del = delete_secret_key.clone();
                                move |_| del(keys::GMAIL_OAUTH_CLIENT_SECRET, "Gmail OAuth2 Client Secret")
                            },
                            {svg_trash()}
                        }
                    }
                }
            }
        }
    }
}
