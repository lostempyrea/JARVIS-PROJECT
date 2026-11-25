use crate::components::{
    apply_audio_device_change, clear_dom_input, svg_close, svg_play, svg_plus, svg_refresh,
    svg_stop, svg_voice, JarvisUiContext,
};
use dioxus::events::Key;
use dioxus::prelude::*;
use jarvis_core::voice::{
    VoicePipeline, DEFAULT_VIETNAMESE_FEMALE_VOICE, DEFAULT_VIETNAMESE_MALE_VOICE,
};

#[component]
pub fn VoicePanelView() -> Element {
    let ctx = use_context::<JarvisUiContext>();
    let cfg_voice = ctx
        .core
        .config
        .read()
        .map(|c| c.voice.clone())
        .unwrap_or_default();

    let mut wake_enabled = use_signal(|| cfg_voice.wake_word_enabled);
    let mut wake_phrases = use_signal(|| cfg_voice.wake_phrases.clone());
    let mut new_phrase = use_signal(String::new);
    let mut stt_backend = use_signal(|| cfg_voice.stt_backend.clone());
    let mut tts_backend = use_signal(|| cfg_voice.tts_backend.clone());
    let mut tts_voice = use_signal(|| cfg_voice.tts_voice.clone());
    let mut input_device = use_signal(|| cfg_voice.input_device.clone().unwrap_or_default());
    let mut output_device = use_signal(|| cfg_voice.output_device.clone().unwrap_or_default());
    let mut input_devices = use_signal(|| VoicePipeline::list_input_devices().unwrap_or_default());
    let mut output_devices = use_signal(|| VoicePipeline::list_output_devices().unwrap_or_default());
    let mut sample_text = use_signal(|| "Hệ thống JARVIS đã sẵn sàng phục vụ.".to_string());
    let mut status_msg = use_signal(|| Option::<String>::None);

    let ui = ctx.ui_state.read();
    let mic_db = ui.mic_db;
    let voice_running = ui.voice_running;
    let ptt_active = ui.ptt_active;
    let last_transcript = ui.last_transcript.clone();
    drop(ui);

    let db_clamped = mic_db.clamp(-60.0, 0.0);
    let meter_pct = ((db_clamped + 60.0) / 60.0 * 100.0).clamp(0.0, 100.0);

    let save_voice_cfg = {
        let core = ctx.core.clone();
        let mut ui_sig = ctx.ui_state;
        move |enabled: bool,
              phrases: Vec<String>,
              stt: String,
              tts: String,
              voice: String| {
            let save_res = if let Ok(mut cfg) = core.config.write() {
                cfg.voice.wake_word_enabled = enabled;
                cfg.voice.wake_phrases = phrases;
                cfg.voice.stt_backend = stt;
                cfg.voice.tts_backend = tts;
                cfg.voice.tts_voice = voice;
                cfg.save()
            } else {
                return;
            };

            match save_res {
                Ok(()) => match core.voice.apply_updated_config() {
                    Ok(restarted) => {
                        ui_sig.write().voice_running = core.voice.is_running();
                        let msg = if restarted {
                            "Voice configuration saved and pipeline reloaded."
                        } else {
                            "Voice configuration saved."
                        };
                        status_msg.set(Some(msg.to_string()));
                    }
                    Err(err) => {
                        ui_sig.write().voice_running = core.voice.is_running();
                        status_msg.set(Some(format!(
                            "Voice configuration saved, but pipeline reload failed: {err}"
                        )));
                    }
                },
                Err(err) => status_msg.set(Some(format!("Save error: {err}"))),
            }
        }
    };

    rsx! {
        div { class: "col-gap", style: "flex: 1; min-height: 0;",
            div { class: "grid-2",
                // Left: Real-Time Mic dB Meter & Hands-Free Controls
                div { class: "panel-card",
                    div { class: "panel-header",
                        div { class: "panel-title",
                            {svg_voice()}
                            "Hands-Free VAD & Audio Meter"
                        }
                        span { class: "chip", if voice_running { "ACTIVE" } else { "STANDBY" } }
                    }

                    div { class: "row-between",
                        span { class: "muted", "Mic Input Level" }
                        span { style: "font-family: var(--font-mono); color: var(--cyan-neon);", "{mic_db:.1} dBFS" }
                    }
                    div { class: "db-meter-track",
                        div { class: "db-meter-fill", style: "width: {meter_pct:.1}%;" }
                    }

                    // Audio Device Selection (Input Microphone, Output Speaker, Refresh)
                    div { class: "row-gap", style: "align-items: flex-end; margin-top: 2px;",
                        div { class: "col-gap", style: "flex: 1; min-width: 0;",
                            label { class: "muted", "Input Microphone" }
                            select {
                                class: "hud-select",
                                style: "width: 100%;",
                                value: "{input_device}",
                                onchange: {
                                    let mut c = ctx.clone();
                                    move |evt: FormEvent| {
                                        let val = evt.value();
                                        input_device.set(val.clone());
                                        let opt = (!val.trim().is_empty()).then(|| val.clone());
                                        match apply_audio_device_change(&mut c, Some(opt), None) {
                                            Ok(restarted) => {
                                                let label = if val.is_empty() { "System Default" } else { val.as_str() };
                                                let msg = if restarted {
                                                    format!("Input microphone set to '{label}' and pipeline reloaded.")
                                                } else {
                                                    format!("Input microphone set to '{label}'.")
                                                };
                                                status_msg.set(Some(msg));
                                            }
                                            Err(err) => status_msg.set(Some(err)),
                                        }
                                    }
                                },
                                option { value: "", "System Default" }
                                {
                                    let cur = input_device.read().clone();
                                    let missing = !cur.is_empty() && !input_devices.read().iter().any(|d| d.name == cur);
                                    rsx! {
                                        if missing {
                                            option { key: "in-missing-{cur}", value: "{cur}", "{cur} (Unavailable)" }
                                        }
                                    }
                                }
                                for dev in input_devices.read().iter() {
                                    option { key: "in-{dev.name}", value: "{dev.name}", "{dev.name}" }
                                }
                            }
                        }
                        div { class: "col-gap", style: "flex: 1; min-width: 0;",
                            label { class: "muted", "Output Speaker" }
                            select {
                                class: "hud-select",
                                style: "width: 100%;",
                                value: "{output_device}",
                                onchange: {
                                    let mut c = ctx.clone();
                                    move |evt: FormEvent| {
                                        let val = evt.value();
                                        output_device.set(val.clone());
                                        let opt = (!val.trim().is_empty()).then(|| val.clone());
                                        match apply_audio_device_change(&mut c, None, Some(opt)) {
                                            Ok(restarted) => {
                                                let label = if val.is_empty() { "System Default" } else { val.as_str() };
                                                let msg = if restarted {
                                                    format!("Output speaker set to '{label}' and pipeline reloaded.")
                                                } else {
                                                    format!("Output speaker set to '{label}'.")
                                                };
                                                status_msg.set(Some(msg));
                                            }
                                            Err(err) => status_msg.set(Some(err)),
                                        }
                                    }
                                },
                                option { value: "", "System Default" }
                                {
                                    let cur = output_device.read().clone();
                                    let missing = !cur.is_empty() && !output_devices.read().iter().any(|d| d.name == cur);
                                    rsx! {
                                        if missing {
                                            option { key: "out-missing-{cur}", value: "{cur}", "{cur} (Unavailable)" }
                                        }
                                    }
                                }
                                for dev in output_devices.read().iter() {
                                    option { key: "out-{dev.name}", value: "{dev.name}", "{dev.name}" }
                                }
                            }
                        }
                        button {
                            class: "hud-btn",
                            "data-tooltip": "Refresh Audio Devices",
                            onclick: move |_| {
                                let ins = VoicePipeline::list_input_devices().unwrap_or_default();
                                let outs = VoicePipeline::list_output_devices().unwrap_or_default();
                                let msg = format!(
                                    "Detected {} input mic(s) and {} output speaker(s).",
                                    ins.len(),
                                    outs.len()
                                );
                                input_devices.set(ins);
                                output_devices.set(outs);
                                status_msg.set(Some(msg));
                            },
                            {svg_refresh()}
                            "Refresh"
                        }
                    }

                    div { class: "row-gap", style: "margin-top: 4px;",
                        button {
                            class: if voice_running { "hud-btn danger" } else { "hud-btn primary" },
                            onclick: {
                                let mut c = ctx.clone();
                                move |_| {
                                    if c.core.voice.is_running() {
                                        c.core.voice.stop();
                                        c.ui_state.write().voice_running = false;
                                        status_msg.set(Some("Voice pipeline stopped.".to_string()));
                                    } else {
                                        let sid = c.active_session_id.read().clone();
                                        match c.core.voice.start(sid) {
                                            Ok(()) => {
                                                c.ui_state.write().voice_running = true;
                                                status_msg.set(Some("Hands-free voice loop active (1.2s pre-roll VAD).".to_string()));
                                            }
                                            Err(err) => status_msg.set(Some(format!("Start failed: {err}"))),
                                        }
                                    }
                                }
                            },
                            if voice_running { {svg_stop()} "Stop Hands-Free" } else { {svg_play()} "Start Hands-Free" }
                        }

                        button {
                            class: if ptt_active { "hud-btn danger active" } else { "hud-btn" },
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
                                        spawn(async move {
                                            if let Err(err) = core.voice.process_ptt_wav(&session_id, &wav).await {
                                                status_msg.set(Some(format!("PTT failed: {err}")));
                                            }
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
                                        spawn(async move {
                                            if let Err(err) = core.voice.process_ptt_wav(&session_id, &wav).await {
                                                status_msg.set(Some(format!("PTT failed: {err}")));
                                            }
                                        });
                                    }
                                }
                            },
                            "Hold Push-to-Talk"
                        }
                    }

                    if let Some((text, is_cmd)) = last_transcript {
                        div { style: "padding: 8px; border-radius: 7px; background: rgba(4, 10, 20, 0.72); border: 1px solid rgba(0, 240, 255, 0.16);",
                            div { class: "muted", if is_cmd { "Last Dispatched Command:" } else { "Last Wake Detection:" } }
                            div { style: "color: var(--cyan-neon); margin-top: 2px;", "{text}" }
                        }
                    }
                }

                // Right: Wake-Word Toggle & Phrase Chips
                div { class: "panel-card",
                    div { class: "panel-header",
                        span { class: "panel-title", "Wake-Word & Inline Splitter" }
                        button {
                            class: if *wake_enabled.read() { "hud-btn active" } else { "hud-btn" },
                            onclick: {
                                let mut save = save_voice_cfg.clone();
                                move |_| {
                                    let next = !*wake_enabled.read();
                                    wake_enabled.set(next);
                                    save(
                                        next,
                                        wake_phrases.read().clone(),
                                        stt_backend.read().clone(),
                                        tts_backend.read().clone(),
                                        tts_voice.read().clone(),
                                    );
                                }
                            },
                            if *wake_enabled.read() { "Wake-Word: ON" } else { "Wake-Word: OFF" }
                        }
                    }

                    div { class: "row-gap", style: "flex-wrap: wrap;",
                        for (idx, phrase) in wake_phrases.read().iter().enumerate() {
                            span { class: "chip", key: "{idx}-{phrase}",
                                "{phrase}"
                                button {
                                    style: "background: none; border: none; color: var(--cyan-neon); cursor: pointer; display: inline-flex;",
                                    onclick: {
                                        let mut save = save_voice_cfg.clone();
                                        move |_| {
                                            let mut list = wake_phrases.read().clone();
                                            if list.len() > 1 && idx < list.len() {
                                                list.remove(idx);
                                                wake_phrases.set(list.clone());
                                                save(
                                                    *wake_enabled.read(),
                                                    list,
                                                    stt_backend.read().clone(),
                                                    tts_backend.read().clone(),
                                                    tts_voice.read().clone(),
                                                );
                                            }
                                        }
                                    },
                                    {svg_close()}
                                }
                            }
                        }
                    }

                    div { class: "row-gap",
                        input {
                            id: "voice-wake-input",
                            class: "hud-input",
                            style: "flex: 1;",
                            spellcheck: "false",
                            placeholder: "Add wake phrase (e.g. trợ lý jarvis)",
                            oninput: move |evt| new_phrase.set(evt.value()),
                            onkeydown: {
                                let mut save = save_voice_cfg.clone();
                                move |evt: KeyboardEvent| {
                                    if evt.key() == Key::Enter && !evt.data().is_composing() {
                                        let p = new_phrase.read().trim().to_lowercase();
                                        if !p.is_empty() {
                                            let mut list = wake_phrases.read().clone();
                                            if !list.contains(&p) {
                                                list.push(p);
                                                wake_phrases.set(list.clone());
                                                save(
                                                    *wake_enabled.read(),
                                                    list,
                                                    stt_backend.read().clone(),
                                                    tts_backend.read().clone(),
                                                    tts_voice.read().clone(),
                                                );
                                            }
                                            new_phrase.set(String::new());
                                            clear_dom_input("voice-wake-input", true);
                                        }
                                    }
                                }
                            },
                        }
                        button {
                            class: "hud-btn primary",
                            onclick: {
                                let mut save = save_voice_cfg.clone();
                                move |_| {
                                    let p = new_phrase.read().trim().to_lowercase();
                                    if !p.is_empty() {
                                        let mut list = wake_phrases.read().clone();
                                        if !list.contains(&p) {
                                            list.push(p);
                                            wake_phrases.set(list.clone());
                                            save(
                                                *wake_enabled.read(),
                                                list,
                                                stt_backend.read().clone(),
                                                tts_backend.read().clone(),
                                                tts_voice.read().clone(),
                                            );
                                        }
                                        new_phrase.set(String::new());
                                        clear_dom_input("voice-wake-input", true);
                                    }
                                }
                            },
                            {svg_plus()}
                            "Add"
                        }
                    }
                }
            }

            // Bottom: Cloud vs Offline STT/TTS Selector & Test Speak
            div { class: "panel-card",
                div { class: "panel-header",
                    span { class: "panel-title", "Cloud vs Offline STT / TTS Engines" }
                }
                div { class: "grid-3",
                    div { class: "col-gap",
                        label { class: "muted", "STT Backend" }
                        select {
                            class: "hud-select",
                            value: "{stt_backend}",
                            onchange: {
                                let mut save = save_voice_cfg.clone();
                                move |evt: FormEvent| {
                                    let val = evt.value();
                                    stt_backend.set(val.clone());
                                    save(
                                        *wake_enabled.read(),
                                        wake_phrases.read().clone(),
                                        val,
                                        tts_backend.read().clone(),
                                        tts_voice.read().clone(),
                                    );
                                }
                            },
                            option { value: "groq_whisper", "Cloud: Groq Whisper (turbo)" }
                            option { value: "openai_whisper", "Cloud: OpenAI Whisper" }
                            option { value: "local_http", "Offline: Local HTTP Whisper" }
                            option { value: "whisper_cli", "Offline: whisper-cli ONNX/GGUF" }
                        }
                    }

                    div { class: "col-gap",
                        label { class: "muted", "TTS Backend" }
                        select {
                            class: "hud-select",
                            value: "{tts_backend}",
                            onchange: {
                                let mut save = save_voice_cfg.clone();
                                move |evt: FormEvent| {
                                    let val = evt.value();
                                    tts_backend.set(val.clone());
                                    save(
                                        *wake_enabled.read(),
                                        wake_phrases.read().clone(),
                                        stt_backend.read().clone(),
                                        val,
                                        tts_voice.read().clone(),
                                    );
                                }
                            },
                            option { value: "edge_tts", "Cloud: Edge-TTS Neural" }
                            option { value: "openai_tts", "Cloud: OpenAI TTS" }
                            option { value: "windows_sapi", "Offline: Windows SAPI" }
                        }
                    }

                    div { class: "col-gap",
                        label { class: "muted", "TTS Voice" }
                        select {
                            class: "hud-select",
                            value: "{tts_voice}",
                            onchange: {
                                let mut save = save_voice_cfg.clone();
                                move |evt: FormEvent| {
                                    let val = evt.value();
                                    tts_voice.set(val.clone());
                                    save(
                                        *wake_enabled.read(),
                                        wake_phrases.read().clone(),
                                        stt_backend.read().clone(),
                                        tts_backend.read().clone(),
                                        val,
                                    );
                                }
                            },
                            option { value: "{DEFAULT_VIETNAMESE_FEMALE_VOICE}", "vi-VN-HoaiMyNeural (Female)" }
                            option { value: "{DEFAULT_VIETNAMESE_MALE_VOICE}", "vi-VN-NamMinhNeural (Male)" }
                            option { value: "en-US-AndrewNeural", "en-US-AndrewNeural" }
                            option { value: "en-US-AriaNeural", "en-US-AriaNeural" }
                            option { value: "alloy", "OpenAI: alloy" }
                        }
                    }
                }

                div { class: "row-gap",
                    input {
                        id: "voice-sample-input",
                        class: "hud-input",
                        style: "flex: 1;",
                        spellcheck: "false",
                        placeholder: "Test TTS synthesis phrase...",
                        initial_value: "Hệ thống JARVIS đã sẵn sàng phục vụ.",
                        oninput: move |evt| sample_text.set(evt.value()),
                    }
                    button {
                        class: "hud-btn primary",
                        onclick: {
                            let core = ctx.core.clone();
                            move |_| {
                                let core = core.clone();
                                let text = sample_text.read().clone();
                                status_msg.set(Some("Synthesizing speech...".to_string()));
                                spawn(async move {
                                    match core.voice.speak(&text).await {
                                        Ok(()) => status_msg.set(Some("Speaking test utterance.".to_string())),
                                        Err(err) => status_msg.set(Some(format!("TTS failed: {err}"))),
                                    }
                                });
                            }
                        },
                        {svg_play()}
                        "Test Speak"
                    }
                    button {
                        class: "hud-btn danger",
                        onclick: {
                            let core = ctx.core.clone();
                            move |_| {
                                core.voice.stop_speaking();
                                status_msg.set(Some("TTS playback stopped.".to_string()));
                            }
                        },
                        {svg_stop()}
                        "Stop"
                    }
                }

                if let Some(msg) = status_msg.read().as_ref() {
                    div { class: "muted", "{msg}" }
                }
            }
        }
    }
}
