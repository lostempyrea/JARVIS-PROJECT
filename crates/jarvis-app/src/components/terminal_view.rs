use crate::components::{
    clear_dom_input, svg_bolt, svg_power, svg_refresh, svg_shield, svg_stop, svg_trash,
    JarvisUiContext,
};
use dioxus::prelude::*;
use jarvis_core::{ApprovalDecision, StyledSpan, VtScreenSnapshot};

#[component]
pub fn TerminalView() -> Element {
    let ctx = use_context::<JarvisUiContext>();
    let mut drawer_open = use_signal(|| false);
    let mut scroll_offset = use_signal(|| 0usize);
    let mut composing = use_signal(|| false);
    let mut last_comp = use_signal(|| Option::<(String, std::time::Instant)>::None);
    let mut active_tab_id = use_signal(|| Option::<String>::None);

    let _rev = ctx.ui_state.read().pty_rev;
    let pty_manager = ctx.core.pty.clone();

    let tabs = pty_manager.list_tabs();

    // Auto-spawn a PowerShell tab on mount if tabs is empty
    let pm_mount = pty_manager.clone();
    use_effect(move || {
        if pm_mount.list_tabs().is_empty() {
            if let Ok(id) = pm_mount.spawn_powershell_tab(None, None, 100, 28) {
                active_tab_id.set(Some(id));
            }
        }
    });

    let current_active = active_tab_id.read().clone().or_else(|| {
        tabs.first().map(|t| t.tab_id.clone())
    });

    if active_tab_id.read().is_none() {
        if let Some(ref aid) = current_active {
            active_tab_id.set(Some(aid.clone()));
        }
    }

    let active_id = current_active.unwrap_or_default();
    let snap = pty_manager.snapshot_tab(&active_id, *scroll_offset.read()).unwrap_or_else(|| {
        VtScreenSnapshot {
            rows: vec![],
            cols: 100,
            height: 28,
            title: String::new(),
            app_cursor_keys: false,
            bracketed_paste: false,
            scrollback_len: 0,
        }
    });

    let pending_approvals = ctx.ui_state.read().pending_approvals.clone();
    let is_drawer_open = *drawer_open.read();
    let show_drawer = is_drawer_open || !pending_approvals.is_empty();
    let approval_count = pending_approvals.len();

    let show_cursor = *scroll_offset.read() == 0;

    let render_span = move |span: &StyledSpan, key_idx: usize| {
        let fg = &span.fg_css;
        let bg = span.bg_css.as_deref().unwrap_or("transparent");
        let mut style_str = format!("color: {}; background-color: {};", fg, bg);
        if span.bold {
            style_str.push_str(" font-weight: bold;");
        }
        if span.italic {
            style_str.push_str(" font-style: italic;");
        }
        if span.underline {
            style_str.push_str(" text-decoration: underline;");
        }
        if span.dim {
            style_str.push_str(" opacity: 0.6;");
        }

        if show_cursor {
            if let Some(c_idx) = span.has_cursor {
                let text_chars: Vec<char> = span.text.chars().collect();
                let before: String = text_chars.iter().take(c_idx).collect();
                let cursor_ch = text_chars.get(c_idx).copied().unwrap_or(' ');
                let after: String = text_chars.iter().skip(c_idx + 1).collect();
                return rsx! {
                    span { key: "{key_idx}", style: "{style_str}",
                        "{before}"
                        span { class: "pty-cursor", "{cursor_ch}" }
                        "{after}"
                    }
                };
            }
        }

        rsx! {
            span { key: "{key_idx}", style: "{style_str}", "{span.text}" }
        }
    };

    rsx! {
        div {
            class: "pty-shell",
            // Top Tab / Action Bar
            div {
                class: "pty-tabbar",
                div {
                    class: "pty-tabs",
                    for tab in &tabs {
                        {
                            let tid = tab.tab_id.clone();
                            let is_active = tid == active_id;
                            let active_cls = if is_active { "pty-tab active" } else { "pty-tab" };
                            let title = if tab.title.is_empty() { "PowerShell".to_string() } else { tab.title.clone() };
                            let mut act_signal = active_tab_id;
                            let tid_click = tid.clone();
                            let tid_close = tid.clone();
                            let pty_m_close = pty_manager.clone();
                            let mut act_signal_close = active_tab_id;
                            rsx! {
                                div {
                                    key: "{tid}",
                                    class: "{active_cls}",
                                    onclick: move |_| {
                                        act_signal.set(Some(tid_click.clone()));
                                    },
                                    span {
                                        class: if tab.alive { "led online" } else { "led offline" },
                                    },
                                    span { style: "max-width: 120px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;", "{title}" },
                                    button {
                                        class: "icon-btn",
                                        style: "padding: 2px; font-size: 10px; margin-left: 4px;",
                                        onclick: move |e: MouseEvent| {
                                            e.stop_propagation();
                                            let _ = pty_m_close.close_tab(&tid_close);
                                            let remaining = pty_m_close.list_tabs();
                                            act_signal_close.set(remaining.first().map(|t| t.tab_id.clone()));
                                        },
                                        "×"
                                    }
                                }
                            }
                        }
                    }
                }
                div {
                    class: "row-gap",
                    button {
                        class: "icon-btn",
                        "data-tooltip": "New PowerShell Tab",
                        "data-tooltip-pos": "bottom-right",
                        onclick: {
                            let pm = pty_manager.clone();
                            let mut act_signal = active_tab_id;
                            move |_| {
                                if let Ok(id) = pm.spawn_powershell_tab(None, None, 100, 28) {
                                    act_signal.set(Some(id));
                                }
                            }
                        },
                        svg_power {}
                    }
                    button {
                        class: "icon-btn primary",
                        "data-tooltip": "New AGY Agent Tab",
                        "data-tooltip-pos": "bottom-right",
                        onclick: {
                            let pm = pty_manager.clone();
                            let mut act_signal = active_tab_id;
                            move |_| {
                                if let Ok(id) = pm.spawn_agy_tab(None, None, 100, 28) {
                                    act_signal.set(Some(id));
                                }
                            }
                        },
                        svg_bolt {}
                    }
                    button {
                        class: "icon-btn danger",
                        "data-tooltip": "Send Ctrl+C",
                        "data-tooltip-pos": "bottom-right",
                        onclick: {
                            let pm = pty_manager.clone();
                            let aid = active_id.clone();
                            move |_| {
                                let _ = pm.write_input(&aid, &[0x03]);
                            }
                        },
                        svg_stop {}
                    }
                    button {
                        class: "icon-btn",
                        "data-tooltip": "Restart Tab",
                        "data-tooltip-pos": "bottom-right",
                        onclick: {
                            let pm = pty_manager.clone();
                            let aid = active_id.clone();
                            move |_| {
                                let _ = pm.restart_tab(&aid);
                            }
                        },
                        svg_refresh {}
                    }
                    button {
                        class: "icon-btn",
                        "data-tooltip": "Clear Screen",
                        "data-tooltip-pos": "bottom-right",
                        onclick: {
                            let pm = pty_manager.clone();
                            let aid = active_id.clone();
                            move |_| {
                                let _ = pm.write_input(&aid, b"cls\r");
                            }
                        },
                        svg_trash {}
                    }
                    button {
                        class: if show_drawer { "icon-btn primary" } else { "icon-btn" },
                        "data-tooltip": if approval_count > 0 { format!("AGY Approvals ({})", approval_count) } else { "AGY Approvals & Tasks".into() },
                        "data-tooltip-pos": "bottom-right",
                        onclick: move |_| {
                            let current_open = *drawer_open.read();
                            drawer_open.set(!current_open);
                        },
                        svg_shield {}
                    }
                }
            }

            // Terminal Viewport + Hidden IME Input + Drawer
            div {
                style: "display: flex; gap: 8px; flex: 1; min-height: 0; min-width: 0; position: relative;",
                div {
                    class: "pty-viewport",
                    style: "display: flex; flex-direction: column; overflow: hidden; position: relative; cursor: text; flex: 1; min-height: 0;",
                    onclick: move |_| {
                        let _ = document::eval("document.getElementById('pty-ime-input')?.focus();");
                    },
                                    onwheel: move |_evt: WheelEvent| {
                                        let current = *scroll_offset.read();
                                        let next = current.saturating_add(3).min(snap.scrollback_len);
                                        scroll_offset.set(next);
                                    },
                    // Rows rendering
                    div {
                        class: "scroll-y",
                        style: "flex: 1; display: flex; flex-direction: column; font-family: var(--font-mono); font-size: 13px; line-height: 1.35; white-space: pre;",
                        for (r_idx, row) in snap.rows.iter().enumerate() {
                            {
                                rsx! {
                                    div {
                                        key: "{r_idx}",
                                        class: "pty-row",
                                        style: "display: flex; min-height: 18px;",
                                        for (s_idx, span) in row.iter().enumerate() {
                                            {
                                                render_span(span, s_idx)
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if *scroll_offset.read() > 0 {
                        {
                            let offset = *scroll_offset.read();
                            rsx! {
                                button {
                                    class: "hud-btn",
                                    style: "position: absolute; right: 12px; bottom: 10px; z-index: 5;",
                                    onclick: move |e: MouseEvent| {
                                        e.stop_propagation();
                                        scroll_offset.set(0);
                                    },
                                    "↓ Jump to Bottom (-{offset})"
                                }
                            }
                        }
                    }

                    // Hidden uncontrolled textarea for Vietnamese IME support & keyboard events
                    textarea {
                        id: "pty-ime-input",
                        style: "position: absolute; opacity: 0; pointer-events: none; height: 0; width: 0;",
                        spellcheck: "false",
                        oncompositionstart: move |_| {
                            composing.set(true);
                        },
                        oncompositionend: {
                            let pm = pty_manager.clone();
                            let aid = active_id.clone();
                            move |evt: Event<CompositionData>| {
                                composing.set(false);
                                let text = evt.data();
                                let s = text.data().clone();
                                if !s.is_empty() {
                                    let _ = pm.write_input(&aid, s.as_bytes());
                                    scroll_offset.set(0);
                                    last_comp.set(Some((s, std::time::Instant::now())));
                                    clear_dom_input("pty-ime-input", true);
                                }
                            }
                        },
                        oninput: {
                            let pm = pty_manager.clone();
                            let aid = active_id.clone();
                            move |evt: Event<FormData>| {
                                if *composing.read() {
                                    return;
                                }
                                let val = evt.value();
                                let should_clear = {
                                    let guard = last_comp.read();
                                    if let Some((prev, ts)) = guard.as_ref() {
                                        ts.elapsed().as_millis() < 80 && val == *prev
                                    } else {
                                        false
                                    }
                                };
                                if should_clear {
                                    last_comp.set(None);
                                    clear_dom_input("pty-ime-input", true);
                                    return;
                                }
                                if !val.is_empty() {
                                    if snap.bracketed_paste && (val.contains('\n') || val.contains('\r')) {
                                        let pasted = format!("\x1b[200~{}\x1b[201~", val);
                                        let _ = pm.write_input(&aid, pasted.as_bytes());
                                    } else {
                                        let _ = pm.write_input(&aid, val.as_bytes());
                                    }
                                    scroll_offset.set(0);
                                    clear_dom_input("pty-ime-input", true);
                                }
                            }
                        },
                        onkeydown: {
                            let pm = pty_manager.clone();
                            let aid = active_id.clone();
                            let app_cursor_keys = snap.app_cursor_keys;
                            move |evt: KeyboardEvent| {
                                if *composing.read() {
                                    return;
                                }
                                let key = evt.key();
                                let mut bytes: Option<Vec<u8>> = None;

                                match key {
                                    Key::Enter => bytes = Some(b"\r".to_vec()),
                                    Key::Backspace => bytes = Some(b"\x7f".to_vec()),
                                    Key::Tab => bytes = Some(b"\t".to_vec()),
                                    Key::Escape => bytes = Some(b"\x1b".to_vec()),
                                    Key::ArrowUp => {
                                        bytes = Some(if app_cursor_keys { b"\x1bOA".to_vec() } else { b"\x1b[A".to_vec() });
                                    }
                                    Key::ArrowDown => {
                                        bytes = Some(if app_cursor_keys { b"\x1bOB".to_vec() } else { b"\x1b[B".to_vec() });
                                    }
                                    Key::ArrowRight => {
                                        bytes = Some(if app_cursor_keys { b"\x1bOC".to_vec() } else { b"\x1b[C".to_vec() });
                                    }
                                    Key::ArrowLeft => {
                                        bytes = Some(if app_cursor_keys { b"\x1bOD".to_vec() } else { b"\x1b[D".to_vec() });
                                    }
                                    Key::Home => bytes = Some(b"\x1b[H".to_vec()),
                                    Key::End => bytes = Some(b"\x1b[F".to_vec()),
                                    Key::PageUp => bytes = Some(b"\x1b[5~".to_vec()),
                                    Key::PageDown => bytes = Some(b"\x1b[6~".to_vec()),
                                    Key::Delete => bytes = Some(b"\x1b[3~".to_vec()),
                                    _ => {
                                        let modifiers = evt.modifiers();
                                        if modifiers.ctrl() && !modifiers.alt() {
                                            if let Key::Character(ref ch) = key {
                                                match ch.as_str() {
                                                    "c" | "C" => bytes = Some(vec![0x03]),
                                                    "d" | "D" => bytes = Some(vec![0x04]),
                                                    "l" | "L" => bytes = Some(vec![0x0c]),
                                                    "z" | "Z" => bytes = Some(vec![0x1a]),
                                                    _ => {}
                                                }
                                            }
                                        }
                                    }
                                }

                                if let Some(b) = bytes {
                                    evt.prevent_default();
                                    let _ = pm.write_input(&aid, &b);
                                    scroll_offset.set(0);
                                    clear_dom_input("pty-ime-input", true);
                                }
                            }
                        },
                    }
                }

                // Collapsible Approval Drawer
                if show_drawer {
                    div {
                        class: "pty-drawer panel-card",
                        div {
                            class: "panel-header",
                            span { class: "panel-title", "AGY Approvals & Tasks" }
                            button {
                                class: "icon-btn danger",
                                onclick: move |_| drawer_open.set(false),
                                "×"
                            }
                        }
                        div {
                            class: "col-gap scroll-y",
                            style: "flex: 1;",
                            if pending_approvals.is_empty() {
                                div { class: "muted", style: "padding: 8px; text-align: center;",
                                    "No pending approval requests."
                                }
                            } else {
                                for req in pending_approvals {
                                    {
                                        let req_id = req.id.clone();
                                        let broker = ctx.core.approval_broker.clone();
                                        let broker_deny = broker.clone();
                                        rsx! {
                                            div {
                                                key: "{req.id}",
                                                class: "bot-config-card",
                                                div {
                                                    class: "row-between",
                                                    span { class: "id-chip", "{req.tool_name}" }
                                                    span { class: "muted", {format!("{}", req.created_at.with_timezone(&chrono::Local).format("%H:%M:%S"))} }
                                                }
                                                div { style: "font-family: var(--font-mono); font-size: 11px; word-break: break-all;",
                                                    "{req.command_or_args}"
                                                }
                                                div {
                                                    class: "row-gap",
                                                    style: "justify-content: flex-end;",
                                                    button {
                                                        class: "icon-btn danger",
                                                        "data-tooltip": "Reject",
                                                        onclick: {
                                                            let req_id_deny = req_id.clone();
                                                            let bd = broker_deny.clone();
                                                            move |_| {
                                                                let _ = bd.resolve(
                                                                    &req_id_deny,
                                                                    ApprovalDecision::Denied,
                                                                );
                                                            }
                                                        },
                                                        "×"
                                                    }
                                                    button {
                                                        class: "icon-btn primary",
                                                        "data-tooltip": "Approve",
                                                        onclick: {
                                                            let req_id_app = req_id.clone();
                                                            let b = broker.clone();
                                                            move |_| {
                                                                let _ = b.resolve(
                                                                    &req_id_app,
                                                                    ApprovalDecision::Approved,
                                                                );
                                                            }
                                                        },
                                                        "✓"
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
            }
        }
    }
}
