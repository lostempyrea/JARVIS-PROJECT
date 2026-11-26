pub mod arc_reactor;
pub mod chat_hud;
pub mod comms_matrix;
pub mod icon_rail;
pub mod music_deck;
pub mod settings_hud;
pub mod standby_popup;
pub mod terminal_view;
pub mod voice_panel;

use crate::window_manager::{
    apply_window_mode, persist_orb_position, sync_webview_to_mode, WindowMode,
};
use chrono::{DateTime, Local};
use dioxus::desktop::DesktopContext;
use dioxus::prelude::*;
use jarvis_core::db::Database;
use jarvis_core::voice::VoicePipeline;
use jarvis_core::{
    AppState, ApprovalDecision, ApprovalRequest, BinaryStatus, ChannelStatusInfo, CoreEvent,
    InstallMethod, MusicQueueSnapshot, MusicTarget, ParsedEmail, ReactorState, SessionRecord,
    UiCommand, UpdateReleaseInfo,
};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

pub fn clear_dom_input(element_id: &str, refocus: bool) {
    if let Ok(id_json) = serde_json::to_string(element_id) {
        let focus_js = if refocus { "el.focus();" } else { "" };
        let js = format!(
            "const el = document.getElementById({id_json}); if (el) {{ el.value = ''; {focus_js} }}"
        );
        let _ = dioxus::document::eval(&js);
    }
}

pub fn set_dom_input_value(element_id: &str, value: &str) {
    if let (Ok(id_json), Ok(val_json)) =
        (serde_json::to_string(element_id), serde_json::to_string(value))
    {
        let js = format!(
            "const el = document.getElementById({id_json}); if (el) {{ el.value = {val_json}; }}"
        );
        let _ = dioxus::document::eval(&js);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardTab {
    #[default]
    Chat,
    Terminal,
    Music,
    Comms,
    Voice,
    Settings,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatTimelineItem {
    User {
        id: String,
        content: String,
        timestamp: String,
    },
    Assistant {
        id: String,
        content: String,
        timestamp: String,
    },
    ToolChip {
        id: String,
        tool_call_id: String,
        tool_name: String,
        arguments: String,
        output_preview: String,
        completed: bool,
        success: bool,
        expanded: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLogLine {
    pub id: String,
    pub session_id: String,
    pub stream: String,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalLogItem {
    pub id: String,
    pub tool_name: String,
    pub command_or_args: String,
    pub decision: ApprovalDecision,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTrackerItem {
    pub task_id: String,
    pub status: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelToast {
    pub id: String,
    pub channel: String,
    pub title: String,
    pub body: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCandidate {
    pub channel: String,
    pub sender_id: String,
    pub display_name: String,
    pub preview: String,
}

#[derive(Debug, Clone)]
pub struct HudUiState {
    pub reactor_state: ReactorState,
    pub sessions: Vec<SessionRecord>,
    pub messages: Vec<ChatTimelineItem>,
    pub pending_approvals: Vec<ApprovalRequest>,
    pub approval_log: Vec<ApprovalLogItem>,
    pub terminal_lines: Vec<TerminalLogLine>,
    pub tasks: Vec<TaskTrackerItem>,
    pub local_music: MusicQueueSnapshot,
    pub discord_music: MusicQueueSnapshot,
    pub music_target: MusicTarget,
    pub binary_status: BinaryStatus,
    pub ffmpeg_install_status: Option<String>,
    pub channel_statuses: Vec<ChannelStatusInfo>,
    pub channel_notifications: Vec<ChannelToast>,
    pub pairing_candidates: Vec<PairingCandidate>,
    pub gmail_emails: Vec<ParsedEmail>,
    pub gmail_loading: bool,
    pub gmail_status_msg: Option<String>,
    pub mic_db: f32,
    pub voice_running: bool,
    pub ptt_active: bool,
    pub last_transcript: Option<(String, bool)>,
    pub toast_banner: Option<String>,
    pub available_update: Option<UpdateReleaseInfo>,
    pub update_installing: bool,
    pub update_progress_text: Option<String>,
    pub streaming_active: bool,
    pub unread_notifications: usize,
    pub pty_rev: u64,
}

impl HudUiState {
    pub fn from_core(core: &AppState, session_id: &str) -> Self {
        let local_music = core.music.local_queue().snapshot();
        let discord_music = core
            .music
            .guild_queue()
            .snapshot(core.music.active_or_default_guild_id());
        let binary_status = core.binary_manager.check_status();
        let channel_statuses = core.channels.statuses();
        let voice_running = core.voice.is_running();
        let pending_approvals = core.approval_broker.list_pending_requests();
        let initial_reactor = if !pending_approvals.is_empty() {
            ReactorState::AwaitingApproval
        } else {
            ReactorState::Idle
        };

        let mut state = Self {
            reactor_state: initial_reactor,
            sessions: Vec::new(),
            messages: Vec::new(),
            pending_approvals,
            approval_log: Vec::new(),
            terminal_lines: Vec::new(),
            tasks: Vec::new(),
            local_music,
            discord_music,
            music_target: MusicTarget::Local,
            binary_status,
            ffmpeg_install_status: None,
            channel_statuses,
            channel_notifications: Vec::new(),
            pairing_candidates: Vec::new(),
            gmail_emails: Vec::new(),
            gmail_loading: false,
            gmail_status_msg: None,
            mic_db: -60.0,
            voice_running,
            ptt_active: false,
            last_transcript: None,
            toast_banner: None,
            available_update: None,
            update_installing: false,
            update_progress_text: None,
            streaming_active: false,
            unread_notifications: 0,
            pty_rev: 0,
        };
        state.reload_from_db(&core.db, session_id);
        state
    }

    pub fn reconcile_approvals(&mut self, core: &AppState) {
        let core_pending = core.approval_broker.list_pending_requests();
        self.pending_approvals = core_pending;
        if !self.pending_approvals.is_empty() {
            self.reactor_state = ReactorState::AwaitingApproval;
        } else if self.reactor_state == ReactorState::AwaitingApproval {
            self.reactor_state = ReactorState::Idle;
        }
    }

    pub fn format_local_hms_from_rfc3339(ts: &str) -> String {
        if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
            return dt.with_timezone(&Local).format("%H:%M:%S").to_string();
        }
        ts.get(11..19).unwrap_or(ts).to_string()
    }

    pub fn reload_from_db(&mut self, db: &Database, session_id: &str) {
        if let Ok(sessions) = db.list_sessions(25) {
            self.sessions = sessions;
        }
        if let Ok(records) = db.list_messages(session_id, 120) {
            self.messages.clear();
            let start_idx = records
                .iter()
                .position(|rec| rec.role != "tool")
                .unwrap_or(records.len());
            for rec in records.into_iter().skip(start_idx) {
                let ts = Self::format_local_hms_from_rfc3339(&rec.created_at);
                match rec.role.as_str() {
                    "user" => {
                        if !rec.content.trim().is_empty() {
                            self.messages.push(ChatTimelineItem::User {
                                id: rec.id,
                                content: rec.content,
                                timestamp: ts,
                            });
                        }
                    }
                    "assistant" => {
                        if !rec.content.trim().is_empty() {
                            self.messages.push(ChatTimelineItem::Assistant {
                                id: rec.id,
                                content: rec.content,
                                timestamp: ts,
                            });
                        }
                    }
                    "tool" => {
                        self.messages.push(ChatTimelineItem::ToolChip {
                            id: rec.id.clone(),
                            tool_call_id: rec.tool_call_id.unwrap_or(rec.id),
                            tool_name: "tool".to_string(),
                            arguments: String::new(),
                            output_preview: rec.content,
                            completed: true,
                            success: true,
                            expanded: false,
                        });
                    }
                    _ => {}
                }
            }
        }
        if let Ok(tasks) = db.list_tasks(None) {
            self.tasks = tasks
                .into_iter()
                .take(30)
                .map(|t| TaskTrackerItem {
                    task_id: t.id,
                    status: t.status,
                    title: t.title,
                })
                .collect();
        }
    }

    pub fn push_user_message(&mut self, content: String) {
        self.messages.push(ChatTimelineItem::User {
            id: Uuid::new_v4().to_string(),
            content,
            timestamp: Local::now().format("%H:%M:%S").to_string(),
        });
        self.streaming_active = true;
    }

    pub fn toggle_tool_chip(&mut self, id: &str) {
        for item in &mut self.messages {
            if let ChatTimelineItem::ToolChip {
                id: item_id,
                expanded,
                ..
            } = item
            {
                if item_id == id {
                    *expanded = !*expanded;
                    break;
                }
            }
        }
    }

    pub fn apply_core_event(&mut self, event: CoreEvent, active_session_id: &str) {
        match event {
            CoreEvent::PtyUpdated { .. } => {
                self.pty_rev = self.pty_rev.wrapping_add(1);
            }
            CoreEvent::TurnComplete { .. } => {}
            CoreEvent::ReactorStateChanged { state } => {
                self.reactor_state = state;
                if matches!(state, ReactorState::Idle) {
                    self.streaming_active = false;
                }
            }
            CoreEvent::TextDelta { session_id, delta } => {
                if session_id == active_session_id {
                    self.streaming_active = true;
                    if let Some(ChatTimelineItem::Assistant { content, .. }) =
                        self.messages.last_mut()
                    {
                        content.push_str(&delta);
                    } else {
                        self.messages.push(ChatTimelineItem::Assistant {
                            id: Uuid::new_v4().to_string(),
                            content: delta,
                            timestamp: Local::now().format("%H:%M:%S").to_string(),
                        });
                    }
                }
            }
            CoreEvent::ToolCallStarted {
                session_id,
                tool_call_id,
                tool_name,
                arguments,
            } => {
                if session_id == active_session_id {
                    let args_pretty = serde_json::to_string_pretty(&arguments)
                        .unwrap_or_else(|_| arguments.to_string());
                    self.messages.push(ChatTimelineItem::ToolChip {
                        id: Uuid::new_v4().to_string(),
                        tool_call_id,
                        tool_name,
                        arguments: args_pretty,
                        output_preview: String::new(),
                        completed: false,
                        success: true,
                        expanded: false,
                    });
                }
            }
            CoreEvent::ToolCallCompleted {
                session_id,
                tool_call_id,
                tool_name,
                success,
                output_preview,
            } => {
                if session_id == active_session_id {
                    let mut updated = false;
                    for item in self.messages.iter_mut().rev() {
                        if let ChatTimelineItem::ToolChip {
                            tool_call_id: tc_id,
                            output_preview: prev,
                            completed,
                            success: ok,
                            ..
                        } = item
                        {
                            if tc_id == &tool_call_id {
                                *prev = output_preview.clone();
                                *completed = true;
                                *ok = success;
                                updated = true;
                                break;
                            }
                        }
                    }
                    if !updated {
                        self.messages.push(ChatTimelineItem::ToolChip {
                            id: Uuid::new_v4().to_string(),
                            tool_call_id,
                            tool_name,
                            arguments: String::new(),
                            output_preview,
                            completed: true,
                            success,
                            expanded: false,
                        });
                    }
                }
            }
            CoreEvent::TerminalLine {
                session_id,
                stream,
                line,
            } => {
                self.terminal_lines.push(TerminalLogLine {
                    id: Uuid::new_v4().to_string(),
                    session_id,
                    stream,
                    line,
                });
                if self.terminal_lines.len() > 500 {
                    let overflow = self.terminal_lines.len() - 500;
                    self.terminal_lines.drain(0..overflow);
                }
            }
            CoreEvent::ApprovalRequested(req) => {
                if !self.pending_approvals.iter().any(|p| p.id == req.id) {
                    self.pending_approvals.push(req);
                }
                self.reactor_state = ReactorState::AwaitingApproval;
            }
            CoreEvent::ApprovalResolved { id, decision } => {
                if let Some(idx) = self.pending_approvals.iter().position(|p| p.id == id) {
                    let req = self.pending_approvals.remove(idx);
                    self.approval_log.insert(
                        0,
                        ApprovalLogItem {
                            id: req.id,
                            tool_name: req.tool_name,
                            command_or_args: req.command_or_args,
                            decision,
                            timestamp: Local::now().format("%H:%M:%S").to_string(),
                        },
                    );
                    if self.approval_log.len() > 60 {
                        self.approval_log.truncate(60);
                    }
                }
                self.toast_banner = Some(match decision {
                    ApprovalDecision::Approved => "✓ Approved task (synced App ↔ Telegram)".into(),
                    ApprovalDecision::Denied => "✕ Denied task (synced App ↔ Telegram)".into(),
                    ApprovalDecision::TimedOut => "⏱ Approval request timed out".into(),
                });
                if self.pending_approvals.is_empty()
                    && self.reactor_state == ReactorState::AwaitingApproval
                {
                    self.reactor_state = ReactorState::Idle;
                }
            }
            CoreEvent::TaskUpdated {
                task_id,
                status,
                title,
            } => {
                if let Some(existing) = self.tasks.iter_mut().find(|t| t.task_id == task_id) {
                    existing.status = status;
                    existing.title = title;
                } else {
                    self.tasks.insert(
                        0,
                        TaskTrackerItem {
                            task_id,
                            status,
                            title,
                        },
                    );
                    if self.tasks.len() > 40 {
                        self.tasks.truncate(40);
                    }
                }
            }
            CoreEvent::Notification { title, body } => {
                self.toast_banner = Some(format!("{title}: {body}"));
                self.unread_notifications = self.unread_notifications.saturating_add(1);
                if self.pending_approvals.is_empty() && !self.streaming_active {
                    self.reactor_state = ReactorState::Notification;
                }
            }
            CoreEvent::VoiceTranscript {
                session_id,
                text,
                is_command,
            } => {
                self.last_transcript = Some((text.clone(), is_command));
                if session_id == active_session_id {
                    self.push_user_message(format!("🎙 {text}"));
                    if !is_command {
                        self.messages.push(ChatTimelineItem::Assistant {
                            id: Uuid::new_v4().to_string(),
                            content: "Yes, I'm listening! Speak your request.".to_string(),
                            timestamp: Local::now().format("%H:%M:%S").to_string(),
                        });
                    }
                }
            }
            CoreEvent::MicLevel { db } => {
                self.mic_db = db;
            }
            CoreEvent::BinaryInstallProgress {
                binary,
                status,
                bytes_downloaded,
                total_bytes,
            } => {
                let detail = match total_bytes {
                    Some(total) if total > 0 => {
                        let pct = (bytes_downloaded * 100 / total).min(100);
                        format!("{binary}: {status} ({pct}%)")
                    }
                    _ if bytes_downloaded > 0 => {
                        format!("{binary}: {status} ({} KB)", bytes_downloaded / 1024)
                    }
                    _ => format!("{binary}: {status}"),
                };
                self.ffmpeg_install_status = Some(detail);
            }
            CoreEvent::UpdateAvailable { release, .. } => {
                self.toast_banner = Some(format!("Update available: v{}", release.version));
                self.available_update = Some(release);
                if !self.update_installing {
                    self.update_installing = false;
                    self.update_progress_text = None;
                }
            }
            CoreEvent::UpdateDownloadProgress {
                version,
                status,
                bytes_downloaded,
                total_bytes,
            } => {
                let detail = match total_bytes {
                    Some(total) if total > 0 => {
                        let pct = (bytes_downloaded.saturating_mul(100) / total).min(100);
                        format!("Downloading v{version}: {status} ({pct}%)")
                    }
                    _ if bytes_downloaded > 0 => {
                        format!("Downloading v{version}: {status} ({} KB)", bytes_downloaded / 1024)
                    }
                    _ => format!("Downloading v{version}: {status}"),
                };
                self.update_progress_text = Some(detail);
            }
            CoreEvent::MusicStateUpdated(snap) => match snap.target {
                MusicTarget::Local => self.local_music = snap,
                MusicTarget::DiscordVoice | MusicTarget::Both => self.discord_music = snap,
            },
            CoreEvent::ChannelStatusChanged {
                channel,
                state,
                detail,
            } => {
                let parsed_state = match state.as_str() {
                    "online" => jarvis_core::ChannelState::Online,
                    "connecting" => jarvis_core::ChannelState::Connecting,
                    "error" => jarvis_core::ChannelState::Error,
                    _ => jarvis_core::ChannelState::Offline,
                };
                if let Some(entry) = self
                    .channel_statuses
                    .iter_mut()
                    .find(|c| c.channel_id == channel)
                {
                    entry.state = parsed_state;
                    if parsed_state == jarvis_core::ChannelState::Error {
                        entry.last_error = detail;
                    } else {
                        entry.last_error = None;
                        if detail.is_some() {
                            entry.connected_as = detail;
                        }
                    }
                } else {
                    self.channel_statuses.push(ChannelStatusInfo {
                        channel_id: channel,
                        state: parsed_state,
                        connected_as: detail,
                        active_target: None,
                        last_error: None,
                    });
                }
            }
            CoreEvent::ChannelNotification {
                channel,
                title,
                body,
            } => {
                self.toast_banner = Some(format!("[{channel}] {title}: {body}"));
                self.unread_notifications = self.unread_notifications.saturating_add(1);
                self.channel_notifications.insert(
                    0,
                    ChannelToast {
                        id: Uuid::new_v4().to_string(),
                        channel,
                        title,
                        body,
                        timestamp: Local::now().format("%H:%M:%S").to_string(),
                    },
                );
                if self.channel_notifications.len() > 30 {
                    self.channel_notifications.truncate(30);
                }
            }
            CoreEvent::ChannelPairingCandidate {
                channel,
                sender_id,
                display_name,
                preview,
            } => {
                self.toast_banner = Some(format!(
                    "[{channel}] Pairing request from {display_name} (ID: {sender_id})"
                ));
                self.pairing_candidates
                    .retain(|c| !(c.channel == channel && c.sender_id == sender_id));
                self.pairing_candidates.insert(
                    0,
                    PairingCandidate {
                        channel,
                        sender_id,
                        display_name,
                        preview,
                    },
                );
                if self.pairing_candidates.len() > 8 {
                    self.pairing_candidates.truncate(8);
                }
            }
            CoreEvent::Error {
                session_id,
                message,
            } => {
                self.streaming_active = false;
                self.toast_banner = Some(format!("Error: {message}"));
                if session_id.as_deref() == Some(active_session_id) || session_id.is_none() {
                    self.messages.push(ChatTimelineItem::Assistant {
                        id: Uuid::new_v4().to_string(),
                        content: format!("⚠ {message}"),
                        timestamp: Local::now().format("%H:%M:%S").to_string(),
                    });
                }
            }
        }
    }
}

/// Shared Dioxus context linking UI components to `jarvis-core` and window management.
#[derive(Clone)]
pub struct JarvisUiContext {
    pub core: Arc<AppState>,
    pub cmd_tx: UnboundedSender<UiCommand>,
    pub window_mode: Signal<WindowMode>,
    pub active_tab: Signal<DashboardTab>,
    pub active_session_id: Signal<String>,
    pub orb_pos: Signal<Option<(f64, f64)>>,
    pub hud_visible: Signal<bool>,
    pub ui_state: Signal<HudUiState>,
}

impl JarvisUiContext {
    pub fn switch_mode(&mut self, desktop: &DesktopContext, next_mode: WindowMode) {
        let prev = *self.window_mode.read();
        if prev == next_mode {
            return;
        }
        let mut pos = *self.orb_pos.read();
        apply_window_mode(desktop, prev, next_mode, &mut pos);
        if pos != *self.orb_pos.read() {
            self.orb_pos.set(pos);
            if let Some(saved) = pos {
                persist_orb_position(saved, &self.core);
            }
        }
        self.window_mode.set(next_mode);
        if next_mode != WindowMode::StandbyOrb {
            self.ui_state.write().unread_notifications = 0;
        }
        let desktop_sync = desktop.clone();
        dioxus::prelude::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(35)).await;
            sync_webview_to_mode(&desktop_sync, next_mode);
        });
    }

    pub fn send_command(&self, cmd: UiCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn submit_prompt(&mut self, prompt: String) {
        let trimmed = prompt.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let session_id = self.active_session_id.read().clone();
        self.ui_state.write().push_user_message(trimmed.clone());
        self.send_command(UiCommand::SubmitPrompt {
            session_id,
            prompt: trimmed,
        });
    }

    pub fn resolve_approval(&mut self, id: &str, decision: ApprovalDecision) {
        if !self.core.approval_broker.resolve(id, decision) {
            let mut ui = self.ui_state.write();
            ui.pending_approvals.retain(|p| p.id != id);
            ui.reconcile_approvals(&self.core);
        }
    }

    pub fn switch_session(&mut self, session_id: String) {
        self.active_session_id.set(session_id.clone());
        self.core.voice.set_active_session(session_id.clone());
        self.ui_state
            .write()
            .reload_from_db(&self.core.db, &session_id);
    }

    pub fn create_new_session(&mut self) {
        let id = format!("hud-{}", Uuid::new_v4());
        let title = format!("Session {}", Local::now().format("%H:%M"));
        if let Err(err) = self.core.db.ensure_session(&id, &title, "hud") {
            tracing::warn!("Failed to create new session: {err}");
        }
        self.switch_session(id);
    }

    pub fn rename_session(&mut self, session_id: &str, new_title: &str) {
        let trimmed = new_title.trim();
        if trimmed.is_empty() {
            return;
        }
        let _ = self.core.db.ensure_session(session_id, trimmed, "hud");
        let _ = self.core.db.update_session_title(session_id, trimmed);
        let mut ui = self.ui_state.write();
        ui.reload_from_db(&self.core.db, session_id);
        ui.toast_banner = Some(format!("Renamed session to '{trimmed}'"));
    }

    pub fn clear_session_history(&mut self, session_id: &str) {
        let _ = self.core.db.clear_session_messages(session_id);
        let mut ui = self.ui_state.write();
        ui.messages.clear();
        ui.reload_from_db(&self.core.db, session_id);
        ui.toast_banner = Some("Cleared session message history.".to_string());
    }

    pub fn delete_session(&mut self, session_id: &str) {
        let _ = self.core.db.delete_session(session_id);
        match self.core.db.list_sessions(25) {
            Ok(list) if !list.is_empty() => {
                self.switch_session(list[0].id.clone());
            }
            _ => {
                self.create_new_session();
            }
        }
        self.ui_state.write().toast_banner = Some("Session deleted.".to_string());
    }
}

/// Persist audio device selection(s) to `config.toml` and hot-reload the voice pipeline.
pub fn apply_audio_device_change(
    ctx: &mut JarvisUiContext,
    input_device: Option<Option<String>>,
    output_device: Option<Option<String>>,
) -> Result<bool, String> {
    if let Ok(mut cfg) = ctx.core.config.write() {
        if let Some(inp) = input_device {
            cfg.voice.input_device = inp;
        }
        if let Some(out) = output_device {
            cfg.voice.output_device = out;
        }
        if let Err(err) = cfg.save() {
            return Err(format!("Save error: {err}"));
        }
    } else {
        return Err("Config lock poisoned".to_string());
    }

    match ctx.core.voice.apply_updated_config() {
        Ok(restarted) => {
            ctx.ui_state.write().voice_running = ctx.core.voice.is_running();
            Ok(restarted)
        }
        Err(err) => {
            ctx.ui_state.write().voice_running = ctx.core.voice.is_running();
            Err(format!("Saved config, but voice reload failed: {err}"))
        }
    }
}

/// Parse Discord User IDs from comma- or whitespace-separated text (supporting `<@id>` / `<@!id>`).
pub fn parse_discord_user_ids_input(raw: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    for token in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        let cleaned = token
            .trim()
            .trim_start_matches("<@!")
            .trim_start_matches("<@")
            .trim_end_matches('>');
        if let Ok(uid) = cleaned.parse::<u64>() {
            if uid > 0 && !ids.contains(&uid) {
                ids.push(uid);
            }
        }
    }
    ids
}

/// Parse Telegram Chat IDs from comma- or whitespace-separated text (including negative group IDs).
pub fn parse_telegram_chat_ids_input(raw: &str) -> Vec<i64> {
    let mut ids = Vec::new();
    for token in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        let cleaned = token.trim();
        if let Ok(cid) = cleaned.parse::<i64>() {
            if cid != 0 && !ids.contains(&cid) {
                ids.push(cid);
            }
        }
    }
    ids
}

pub fn add_allowed_discord_user(ctx: &mut JarvisUiContext, uid: u64) -> Result<(), String> {
    if uid == 0 {
        return Err("Invalid Discord User ID.".to_string());
    }
    if let Ok(mut cfg) = ctx.core.config.write() {
        if !cfg.channels.discord_allowed_user_ids.contains(&uid) {
            cfg.channels.discord_allowed_user_ids.push(uid);
        }
        cfg.save().map_err(|e| format!("Save error: {e}"))?;
    } else {
        return Err("Config lock poisoned".to_string());
    }
    let uid_str = uid.to_string();
    ctx.ui_state
        .write()
        .pairing_candidates
        .retain(|c| !(c.channel == "discord" && c.sender_id == uid_str));
    Ok(())
}

pub fn remove_allowed_discord_user(ctx: &mut JarvisUiContext, uid: u64) -> Result<(), String> {
    if let Ok(mut cfg) = ctx.core.config.write() {
        cfg.channels
            .discord_allowed_user_ids
            .retain(|&id| id != uid);
        cfg.save().map_err(|e| format!("Save error: {e}"))?;
    } else {
        return Err("Config lock poisoned".to_string());
    }
    let uid_str = uid.to_string();
    ctx.ui_state
        .write()
        .pairing_candidates
        .retain(|c| !(c.channel == "discord" && c.sender_id == uid_str));
    Ok(())
}

pub fn add_allowed_telegram_chat(ctx: &mut JarvisUiContext, chat_id: i64) -> Result<(), String> {
    if chat_id == 0 {
        return Err("Invalid Telegram Chat ID.".to_string());
    }
    if let Ok(mut cfg) = ctx.core.config.write() {
        if !cfg.channels.telegram_allowed_chat_ids.contains(&chat_id) {
            cfg.channels.telegram_allowed_chat_ids.push(chat_id);
        }
        cfg.save().map_err(|e| format!("Save error: {e}"))?;
    } else {
        return Err("Config lock poisoned".to_string());
    }
    let cid_str = chat_id.to_string();
    ctx.ui_state
        .write()
        .pairing_candidates
        .retain(|c| !(c.channel == "telegram" && c.sender_id == cid_str));
    Ok(())
}

pub fn remove_allowed_telegram_chat(ctx: &mut JarvisUiContext, chat_id: i64) -> Result<(), String> {
    if let Ok(mut cfg) = ctx.core.config.write() {
        cfg.channels
            .telegram_allowed_chat_ids
            .retain(|&id| id != chat_id);
        cfg.save().map_err(|e| format!("Save error: {e}"))?;
    } else {
        return Err("Config lock poisoned".to_string());
    }
    let cid_str = chat_id.to_string();
    ctx.ui_state
        .write()
        .pairing_candidates
        .retain(|c| !(c.channel == "telegram" && c.sender_id == cid_str));
    Ok(())
}

/// Compact Mic Picker button with an upward-anchored popover listing detected input microphones.
#[component]
pub fn MicPickerButton(#[props(default = "top-left")] tooltip_pos: &'static str) -> Element {
    let ctx = use_context::<JarvisUiContext>();
    let mut open = use_signal(|| false);
    let mut devices = use_signal(|| VoicePipeline::list_input_devices().unwrap_or_default());
    let mut active_device = use_signal(|| {
        ctx.core
            .config
            .read()
            .ok()
            .and_then(|c| c.voice.input_device.clone())
            .unwrap_or_default()
    });

    let active_label = {
        let cur = active_device.read();
        if cur.is_empty() {
            "Mic: System Default".to_string()
        } else {
            format!("Mic: {cur}")
        }
    };

    let select_mic = {
        let ctx_sel = ctx.clone();
        move |next_val: String| {
            let mut c = ctx_sel.clone();
            let opt = (!next_val.trim().is_empty()).then(|| next_val.clone());
            match apply_audio_device_change(&mut c, Some(opt), None) {
                Ok(_) => {
                    active_device.set(next_val.clone());
                    let label = if next_val.is_empty() {
                        "System Default"
                    } else {
                        next_val.as_str()
                    };
                    c.ui_state.write().toast_banner = Some(format!("Input microphone: {label}"));
                }
                Err(err) => {
                    c.ui_state.write().toast_banner = Some(err);
                }
            }
            open.set(false);
        }
    };

    rsx! {
        div { class: "mic-picker-wrap",
            button {
                class: if *open.read() { "icon-btn active" } else { "icon-btn" },
                style: "width: 28px;",
                "data-tooltip": "{active_label}",
                "data-tooltip-pos": "{tooltip_pos}",
                onclick: {
                    let core = ctx.core.clone();
                    move |_| {
                        let next_open = !*open.read();
                        if next_open {
                            devices.set(VoicePipeline::list_input_devices().unwrap_or_default());
                            if let Ok(cfg) = core.config.read() {
                                active_device.set(cfg.voice.input_device.clone().unwrap_or_default());
                            }
                        }
                        open.set(next_open);
                    }
                },
                {svg_mic_picker()}
            }
            if *open.read() {
                div { class: "mic-popover",
                    div { class: "mic-popover-head",
                        span { "Input Microphone" }
                        button {
                            class: "icon-btn",
                            style: "width: 22px; height: 22px;",
                            "data-tooltip": "Refresh",
                            "data-tooltip-pos": "bottom-right",
                            onclick: move |_| {
                                devices.set(VoicePipeline::list_input_devices().unwrap_or_default());
                            },
                            {svg_refresh()}
                        }
                    }
                    button {
                        class: if active_device.read().is_empty() { "mic-popover-item active" } else { "mic-popover-item" },
                        onclick: {
                            let mut sel = select_mic.clone();
                            move |_| sel(String::new())
                        },
                        span { class: "mic-popover-label", "System Default" }
                        if active_device.read().is_empty() {
                            {svg_check()}
                        }
                    }
                    for dev in devices.read().iter() {
                        {
                            let dev_name = dev.name.clone();
                            let dev_name_click = dev_name.clone();
                            let is_active = *active_device.read() == dev_name;
                            let suffix = if dev.is_default { " (Default)" } else { "" };
                            let mut sel = select_mic.clone();
                            rsx! {
                                button {
                                    key: "mic-{dev_name}",
                                    class: if is_active { "mic-popover-item active" } else { "mic-popover-item" },
                                    title: "{dev_name}",
                                    onclick: move |_| sel(dev_name_click.clone()),
                                    span { class: "mic-popover-label", "{dev.name}{suffix}" }
                                    if is_active {
                                        {svg_check()}
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

async fn install_update_inner(
    ctx: &mut JarvisUiContext,
    release: &UpdateReleaseInfo,
) -> Result<(), String> {
    let staged_path = ctx
        .core
        .updater
        .download_and_verify(release)
        .await
        .map_err(|e| e.to_string())?;

    ctx.ui_state.write().update_progress_text =
        Some("Verifying SHA-256 & applying update...".to_string());

    let install_method = jarvis_core::updater::apply_staged_to_current_exe(&staged_path)
        .map_err(|e| e.to_string())?;

    ctx.core.voice.stop();
    ctx.core.music.local_player().shutdown();
    ctx.core.music.local_player().halt_active_stream().await;
    ctx.core.channels.stop_all().await;
    let _ = ctx.core.ps_console.shutdown().await;
    ctx.core.pty.shutdown_all();

    let start_mode_env = match *ctx.window_mode.read() {
        WindowMode::ExpandedDashboard => "dashboard",
        _ => "popup",
    };

    match install_method {
        InstallMethod::InPlaceSwapped => {
            let _ = std::fs::remove_file(&staged_path);
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            std::process::Command::new(exe)
                .env("JARVIS_START_MODE", start_mode_env)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
        InstallMethod::DeferredScript(ps1) => {
            jarvis_core::binary_manager::hidden_std_command(std::path::Path::new(
                "powershell.exe",
            ))
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-WindowStyle",
                "Hidden",
                "-File",
            ])
            .arg(&ps1)
            .env("JARVIS_START_MODE", start_mode_env)
            .spawn()
            .map_err(|e| e.to_string())?;
        }
    }

    std::process::exit(0);
}

pub fn trigger_install_update(mut ctx: JarvisUiContext, release: UpdateReleaseInfo) {
    {
        let mut ui = ctx.ui_state.write();
        ui.update_installing = true;
        ui.update_progress_text = Some(format!("Downloading v{}...", release.version));
    }

    dioxus::prelude::spawn(async move {
        if let Err(err) = install_update_inner(&mut ctx, &release).await {
            let mut ui = ctx.ui_state.write();
            ui.update_installing = false;
            ui.update_progress_text = None;
            ui.toast_banner = Some(format!("Update failed: {err}"));
        }
    });
}

#[component]
pub fn UpdatePromptCard() -> Element {
    let ctx = use_context::<JarvisUiContext>();
    let (available_update, update_installing, update_progress_text) = {
        let ui = ctx.ui_state.read();
        (
            ui.available_update.clone(),
            ui.update_installing,
            ui.update_progress_text.clone(),
        )
    };

    let Some(rel) = available_update else {
        return rsx! {};
    };

    let current_ver = jarvis_core::updater::current_version();
    let notes_text = if rel.notes.trim().is_empty() {
        "New release available on GitHub.".to_string()
    } else {
        rel.notes.clone()
    };
    let progress_display =
        update_progress_text.unwrap_or_else(|| "Installing update...".to_string());

    rsx! {
        div { class: "update-card",
            div { class: "update-card-head",
                span {
                    style: "font-size: 12.5px; font-weight: 700; color: var(--cyan-neon);",
                    "⬆ Update v{rel.version} Available"
                }
                span { class: "chip", "Current: v{current_ver}" }
            }
            div { class: "update-card-notes", "{notes_text}" }
            if update_installing {
                div {
                    class: "muted",
                    style: "color: var(--emerald); font-weight: 600;",
                    "{progress_display}"
                }
            } else {
                div { class: "update-card-actions",
                    button {
                        class: "hud-btn primary",
                        onclick: {
                            let c = ctx.clone();
                            let r = rel.clone();
                            move |_| trigger_install_update(c.clone(), r.clone())
                        },
                        "Update Now"
                    }
                    button {
                        class: "hud-btn",
                        onclick: {
                            let mut c = ctx.clone();
                            move |_| c.ui_state.write().available_update = None
                        },
                        "Later"
                    }
                    button {
                        class: "hud-btn",
                        onclick: {
                            let mut c = ctx.clone();
                            let ver = rel.version.clone();
                            move |_| {
                                if let Ok(mut cfg) = c.core.config.write() {
                                    cfg.updater.skipped_version = Some(ver.clone());
                                    let _ = cfg.save();
                                }
                                let mut ui = c.ui_state.write();
                                ui.available_update = None;
                                ui.toast_banner = Some(format!("Skipped update v{ver}"));
                            }
                        },
                        "Skip Version"
                    }
                }
            }
        }
    }
}

pub fn render_chat_item(
    item: ChatTimelineItem,
    mut ctx: JarvisUiContext,
    user_label: Option<&'static str>,
    assistant_label: Option<&'static str>,
) -> Element {
    match item {
        ChatTimelineItem::User {
            id,
            content,
            timestamp,
        } => {
            let meta = match user_label {
                Some(lbl) => format!("{lbl} · {timestamp}"),
                None => timestamp,
            };
            rsx! {
                div { class: "msg-row user", key: "{id}",
                    div { class: "msg-bubble", "{content}" }
                    span { class: "msg-meta", "{meta}" }
                }
            }
        }
        ChatTimelineItem::Assistant {
            id,
            content,
            timestamp,
        } => {
            let meta = match assistant_label {
                Some(lbl) => format!("{lbl} · {timestamp}"),
                None => timestamp,
            };
            rsx! {
                div { class: "msg-row assistant", key: "{id}",
                    div { class: "msg-bubble", "{content}" }
                    span { class: "msg-meta", "{meta}" }
                }
            }
        }
        ChatTimelineItem::ToolChip {
            id,
            tool_name,
            arguments,
            output_preview,
            completed,
            success,
            expanded,
            ..
        } => {
            let status_txt = if !completed {
                "⟳ Running"
            } else if success {
                "✓ Done"
            } else {
                "✕ Failed"
            };
            let caret = if expanded { "▾" } else { "▸" };
            let chip_key = id.clone();
            rsx! {
                div { class: "tool-chip", key: "{chip_key}",
                    div {
                        class: "tool-chip-head",
                        onclick: move |_| ctx.ui_state.write().toggle_tool_chip(&id),
                        span { "{status_txt}" }
                        span { style: "font-weight: 700;", "{tool_name}" }
                        span { class: "muted", "{caret}" }
                    }
                    if expanded {
                        div { class: "tool-chip-body",
                            if !arguments.is_empty() {
                                div { style: "margin-bottom: 4px;", "ARGS:\n{arguments}" }
                            }
                            if !output_preview.is_empty() {
                                div { "OUTPUT:\n{output_preview}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// Pure-SVG Icon Helpers (100% Vector, Zero External Files)
// ============================================================================

pub fn svg_chat() -> Element {
    rsx! {
        svg { width: "18", height: "18", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.8", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" }
        }
    }
}

pub fn svg_terminal() -> Element {
    rsx! {
        svg { width: "18", height: "18", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.8", stroke_linecap: "round", stroke_linejoin: "round",
            polyline { points: "4 17 10 11 4 5" }
            line { x1: "12", y1: "19", x2: "20", y2: "19" }
        }
    }
}

pub fn svg_music() -> Element {
    rsx! {
        svg { width: "18", height: "18", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.8", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M9 18V5l12-2v13" }
            circle { cx: "6", cy: "18", r: "3" }
            circle { cx: "18", cy: "16", r: "3" }
        }
    }
}

pub fn svg_comms() -> Element {
    rsx! {
        svg { width: "18", height: "18", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.8", stroke_linecap: "round", stroke_linejoin: "round",
            circle { cx: "12", cy: "12", r: "2" }
            path { d: "M16.24 7.76a6 6 0 0 1 0 8.49" }
            path { d: "M7.76 16.24a6 6 0 0 1 0-8.49" }
            path { d: "M19.07 4.93a10 10 0 0 1 0 14.14" }
            path { d: "M4.93 19.07a10 10 0 0 1 0-14.14" }
        }
    }
}

pub fn svg_voice() -> Element {
    rsx! {
        svg { width: "18", height: "18", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.8", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z" }
            path { d: "M19 10v2a7 7 0 0 1-14 0v-2" }
            line { x1: "12", y1: "19", x2: "12", y2: "23" }
            line { x1: "8", y1: "23", x2: "16", y2: "23" }
        }
    }
}

pub fn svg_settings() -> Element {
    rsx! {
        svg { width: "18", height: "18", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.8", stroke_linecap: "round", stroke_linejoin: "round",
            circle { cx: "12", cy: "12", r: "3" }
            path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" }
        }
    }
}

pub fn svg_send() -> Element {
    rsx! {
        svg { width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            line { x1: "22", y1: "2", x2: "11", y2: "13" }
            polygon { points: "22 2 15 22 11 13 2 9 22 2" }
        }
    }
}

pub fn svg_expand() -> Element {
    rsx! {
        svg { width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            polyline { points: "15 3 21 3 21 9" }
            polyline { points: "9 21 3 21 3 15" }
            line { x1: "21", y1: "3", x2: "14", y2: "10" }
            line { x1: "3", y1: "21", x2: "10", y2: "14" }
        }
    }
}

pub fn svg_collapse() -> Element {
    rsx! {
        svg { width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            polyline { points: "4 14 10 14 10 20" }
            polyline { points: "20 10 14 10 14 4" }
            line { x1: "14", y1: "10", x2: "21", y2: "3" }
            line { x1: "3", y1: "21", x2: "10", y2: "14" }
        }
    }
}

pub fn svg_orb() -> Element {
    rsx! {
        svg { width: "17", height: "17", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2",
            circle { cx: "12", cy: "12", r: "9" }
            circle { cx: "12", cy: "12", r: "4" }
        }
    }
}

pub fn svg_minimize() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round",
            line { x1: "5", y1: "12", x2: "19", y2: "12" }
        }
    }
}

pub fn svg_close() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round",
            line { x1: "18", y1: "6", x2: "6", y2: "18" }
            line { x1: "6", y1: "6", x2: "18", y2: "18" }
        }
    }
}

pub fn svg_check() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2.4", stroke_linecap: "round", stroke_linejoin: "round",
            polyline { points: "20 6 9 17 4 12" }
        }
    }
}

pub fn svg_play() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "currentColor",
            polygon { points: "6 4 20 12 6 20 6 4" }
        }
    }
}

pub fn svg_pause() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "currentColor",
            rect { x: "6", y: "4", width: "4", height: "16" }
            rect { x: "14", y: "4", width: "4", height: "16" }
        }
    }
}

pub fn svg_skip() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "currentColor",
            polygon { points: "5 4 15 12 5 20 5 4" }
            rect { x: "17", y: "5", width: "2.5", height: "14" }
        }
    }
}

pub fn svg_stop() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "currentColor",
            rect { x: "6", y: "6", width: "12", height: "12", rx: "1.5" }
        }
    }
}

pub fn svg_plus() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2.2", stroke_linecap: "round",
            line { x1: "12", y1: "5", x2: "12", y2: "19" }
            line { x1: "5", y1: "12", x2: "19", y2: "12" }
        }
    }
}

pub fn svg_speaker() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            polygon { points: "11 5 6 9 2 9 2 15 6 15 11 19 11 5" }
            path { d: "M15.54 8.46a5 5 0 0 1 0 7.07" }
        }
    }
}

pub fn svg_headset() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M3 18v-6a9 9 0 0 1 18 0v6" }
            path { d: "M21 19a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3zM3 19a2 2 0 0 0 2 2h1a2 2 0 0 0 2-2v-3a2 2 0 0 0-2-2H3z" }
        }
    }
}

pub fn svg_refresh() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            polyline { points: "23 4 23 10 17 10" }
            path { d: "M20.49 15a9 9 0 1 1-2.12-9.36L23 10" }
        }
    }
}

pub fn svg_shield() -> Element {
    rsx! {
        svg { width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" }
        }
    }
}

pub fn svg_mail() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M4 4h16c1.1 0 2 .9 2 2v12c0 1.1-.9 2-2 2H4c-1.1 0-2-.9-2-2V6c0-1.1.9-2 2-2z" }
            polyline { points: "22,6 12,13 2,6" }
        }
    }
}

pub fn svg_download() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" }
            polyline { points: "7 10 12 15 17 10" }
            line { x1: "12", y1: "15", x2: "12", y2: "3" }
        }
    }
}

pub fn svg_mic_picker() -> Element {
    rsx! {
        svg { width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M9 3a2.5 2.5 0 0 0-2.5 2.5v5a2.5 2.5 0 0 0 5 0v-5A2.5 2.5 0 0 0 9 3z" }
            path { d: "M14 9.5v1a5 5 0 0 1-10 0v-1" }
            line { x1: "9", y1: "15.5", x2: "9", y2: "19" }
            polyline { points: "16 14 19 10 22 14" }
        }
    }
}

pub fn svg_eye() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z" }
            circle { cx: "12", cy: "12", r: "3" }
        }
    }
}

pub fn svg_eye_off() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19m-6.72-1.07a3 3 0 1 1-4.24-4.24" }
            line { x1: "1", y1: "1", x2: "23", y2: "23" }
        }
    }
}

pub fn svg_trash() -> Element {
    rsx! {
        svg { width: "14", height: "14", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            polyline { points: "3 6 5 6 21 6" }
            path { d: "M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" }
        }
    }
}

pub fn svg_discord() -> Element {
    rsx! {
        svg { width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.9", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M8.5 6.5A12 12 0 0 1 12 6c1.2 0 2.4.18 3.5.5l.8-1.5c1.5.5 2.8 1.3 3.7 2.4 1.3 3.3 1.7 6.8 1 10.1-1.3 1-2.8 1.6-4.4 2l-.9-1.6" }
            path { d: "M8.3 17.9l-.9 1.6c-1.6-.4-3.1-1-4.4-2-.7-3.3-.3-6.8 1-10.1.9-1.1 2.2-1.9 3.7-2.4l.8 1.5" }
            path { d: "M7.5 16.5c2.8 1.3 6.2 1.3 9 0" }
            circle { cx: "9.2", cy: "12.2", r: "1.1", fill: "currentColor" }
            circle { cx: "14.8", cy: "12.2", r: "1.1", fill: "currentColor" }
        }
    }
}

pub fn svg_telegram() -> Element {
    rsx! {
        svg { width: "16", height: "16", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "1.9", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M21.5 3.5L2.5 10.8l6.8 2.4 2.5 6.8 3.5-4.3 4.8 3.5L21.5 3.5z" }
            path { d: "M9.3 13.2L19 5.5" }
        }
    }
}

pub fn svg_key() -> Element {
    rsx! {
        svg { width: "14", height: "14", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M21 2l-2 2m-7.61 7.61a5.5 5.5 0 1 1-7.778 7.778 5.5 5.5 0 0 1 7.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4" }
        }
    }
}

pub fn svg_link() -> Element {
    rsx! {
        svg { width: "14", height: "14", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" }
            path { d: "M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" }
        }
    }
}

pub fn svg_power() -> Element {
    rsx! {
        svg { width: "14", height: "14", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M18.36 6.64a9 9 0 1 1-12.73 0" }
            line { x1: "12", y1: "2", x2: "12", y2: "12" }
        }
    }
}

pub fn svg_bolt() -> Element {
    rsx! {
        svg { width: "14", height: "14", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            polygon { points: "13 2 3 14 12 14 11 22 21 10 12 10 13 2" }
        }
    }
}

#[allow(dead_code)]
pub fn svg_copy() -> Element {
    rsx! {
        svg { width: "14", height: "14", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            rect { x: "9", y: "9", width: "13", height: "13", rx: "2", ry: "2" }
            path { d: "M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" }
        }
    }
}

pub fn svg_mute_speaker() -> Element {
    rsx! {
        svg { width: "15", height: "15", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            polygon { points: "11 5 6 9 2 9 2 15 6 15 11 19 11 5" }
            line { x1: "23", y1: "9", x2: "17", y2: "15" }
            line { x1: "17", y1: "9", x2: "23", y2: "15" }
        }
    }
}

pub fn svg_edit() -> Element {
    rsx! {
        svg { width: "14", height: "14", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M12 20h9" }
            path { d: "M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" }
        }
    }
}

pub fn svg_clear() -> Element {
    rsx! {
        svg { width: "14", height: "14", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            path { d: "M20 20H7L3 16c-.8-.8-.8-2 0-2.8L13.8 2.4c.8-.8 2-.8 2.8 0L21.6 7.4c.8.8.8 2 0 2.8L12 20" }
            line { x1: "6.5", y1: "12.5", x2: "14.5", y2: "20" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_discord_user_ids_handles_mentions_and_delimiters() {
        let parsed = parse_discord_user_ids_input(
            "362150827776933888, <@111222333444>, <@!555666777888> 0 -42 invalid 362150827776933888",
        );
        assert_eq!(
            parsed,
            vec![362150827776933888, 111222333444, 555666777888]
        );
        assert!(parse_discord_user_ids_input("   ,  0  abc ").is_empty());
    }

    #[test]
    fn parse_telegram_chat_ids_handles_positive_negative_and_dedup() {
        let parsed = parse_telegram_chat_ids_input("123456789, -100987654321 0 abc 123456789");
        assert_eq!(parsed, vec![123456789, -100987654321]);
        assert!(parse_telegram_chat_ids_input("0 , foo ").is_empty());
    }
}
