//! Telegram channel adapter built on pure-Rust `teloxide`.
//!
//! Features:
//! - Strict `allowed_chat_ids` whitelist enforcement
//! - 2-way text/command execution via [`AgentRuntime`]
//! - Automatic completion push notifications (`notify_task_completed`)
//! - Inline Keyboard approval buttons (`approve:<id>` / `reject:<id>`) wired to [`ApprovalBroker::resolve`]

use super::{ChannelError, ChannelState, ChannelStatusInfo};
use crate::agent::approval::ApprovalBroker;
use crate::agent::AgentRuntime;
use crate::config::{AppConfig, ChannelsConfig};
use crate::events::{ApprovalDecision, ApprovalRequest, CoreEvent, EventBus};
use crate::secrets::{keys, SecretStore};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use teloxide::dispatching::{ShutdownToken, UpdateFilterExt};
use teloxide::dptree;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MaybeInaccessibleMessage, MessageId,
};
use tokio_util::sync::CancellationToken;

/// Telegram message character limit.
pub const TELEGRAM_MAX_LEN: usize = 4096;

/// Strict whitelist check against `ChannelsConfig::telegram_allowed_chat_ids`.
/// Returns `true` ONLY when `chat_id` is explicitly listed.
pub fn is_telegram_chat_allowed(config: &ChannelsConfig, chat_id: i64) -> bool {
    config.telegram_allowed_chat_ids.contains(&chat_id)
}

/// Enforce the `telegram_allowed_chat_ids` whitelist, returning `ChannelError::Unauthorized`
/// if `chat_id` is not allowed.
pub fn enforce_chat_whitelist(config: &ChannelsConfig, chat_id: i64) -> Result<(), ChannelError> {
    if is_telegram_chat_allowed(config, chat_id) {
        Ok(())
    } else {
        Err(ChannelError::Unauthorized(
            chat_id.to_string(),
            "telegram".to_string(),
        ))
    }
}

/// Format an unauthorized notice displaying the caller's numeric Telegram Chat ID.
pub fn format_unauthorized_chat_message(chat_id: i64) -> String {
    format!(
        "Unauthorized. Your Telegram Chat ID is: {chat_id}. Add this ID to channels.telegram_allowed_chat_ids in JARVIS settings."
    )
}

/// Parse inline keyboard callback data (`approve:<id>` or `reject:<id>`).
pub fn parse_approval_callback_data(data: &str) -> Option<(ApprovalDecision, String)> {
    let trimmed = data.trim();
    if let Some(id) = trimmed.strip_prefix("approve:") {
        let clean = id.trim();
        if !clean.is_empty() {
            return Some((ApprovalDecision::Approved, clean.to_string()));
        }
    } else if let Some(id) = trimmed.strip_prefix("reject:") {
        let clean = id.trim();
        if !clean.is_empty() {
            return Some((ApprovalDecision::Denied, clean.to_string()));
        }
    }
    None
}

/// Build an inline keyboard with `Approve` (`approve:<id>`) and `Reject` (`reject:<id>`) buttons.
pub fn build_approval_keyboard(approval_id: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ Approve", format!("approve:{approval_id}")),
        InlineKeyboardButton::callback("❌ Reject", format!("reject:{approval_id}")),
    ]])
}

/// Split a long Telegram reply into character-bounded chunks (`<= max_len` chars).
pub fn split_telegram_message(text: &str, max_len: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let limit = max_len.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    for ch in text.chars() {
        if current_chars >= limit {
            chunks.push(current);
            current = String::new();
            current_chars = 0;
        }
        current.push(ch);
        current_chars += 1;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Process a Telegram callback query payload (`approve:<id>` / `reject:<id>`) against
/// the whitelist and [`ApprovalBroker`].
pub fn handle_approval_callback_data(
    config: &ChannelsConfig,
    chat_id: i64,
    data: &str,
    broker: &ApprovalBroker,
) -> Result<(ApprovalDecision, String, bool), ChannelError> {
    enforce_chat_whitelist(config, chat_id)?;
    let (decision, id) = parse_approval_callback_data(data).ok_or_else(|| {
        ChannelError::Adapter("telegram".into(), format!("Invalid callback payload: {data}"))
    })?;
    let resolved = broker.resolve(&id, decision);
    Ok((decision, id, resolved))
}

/// Telegram channel adapter managing `teloxide` polling, push notifications, and inline approvals.
pub struct TelegramAdapter {
    config: Arc<RwLock<AppConfig>>,
    secrets: Arc<dyn SecretStore>,
    agent: AgentRuntime,
    approval_broker: Arc<ApprovalBroker>,
    event_bus: EventBus,
    status: Arc<Mutex<ChannelStatusInfo>>,
    #[allow(clippy::type_complexity)]
    approval_messages: Arc<Mutex<HashMap<String, Vec<(ChatId, MessageId, String)>>>>,
    cancel: Mutex<Option<CancellationToken>>,
    shutdown_token: Mutex<Option<ShutdownToken>>,
    task_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pairing_until: Arc<Mutex<Option<Instant>>>,
}

impl TelegramAdapter {
    pub fn new(
        config: Arc<RwLock<AppConfig>>,
        secrets: Arc<dyn SecretStore>,
        agent: AgentRuntime,
        approval_broker: Arc<ApprovalBroker>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            config,
            secrets,
            agent,
            approval_broker,
            event_bus,
            status: Arc::new(Mutex::new(ChannelStatusInfo::offline("telegram"))),
            approval_messages: Arc::new(Mutex::new(HashMap::new())),
            cancel: Mutex::new(None),
            shutdown_token: Mutex::new(None),
            task_handle: tokio::sync::Mutex::new(None),
            pairing_until: Arc::new(Mutex::new(None)),
        }
    }

    /// Enable single-shot `/start` quick-pairing for the given `duration`.
    pub fn enable_pairing_window(&self, duration: Duration) {
        let mut guard = self
            .pairing_until
            .lock()
            .expect("pairing_until mutex poisoned");
        *guard = Some(Instant::now() + duration);
    }

    /// Cancel any active quick-pairing window.
    pub fn cancel_pairing_window(&self) {
        let mut guard = self
            .pairing_until
            .lock()
            .expect("pairing_until mutex poisoned");
        *guard = None;
    }

    /// Return remaining seconds in the quick-pairing window, or `None` if inactive/expired.
    pub fn pairing_remaining_secs(&self) -> Option<u64> {
        let mut guard = self
            .pairing_until
            .lock()
            .expect("pairing_until mutex poisoned");
        let until = (*guard)?;
        let now = Instant::now();
        if until > now {
            let rem_ms = until.saturating_duration_since(now).as_millis() as u64;
            Some(rem_ms.div_ceil(1000).max(1))
        } else {
            *guard = None;
            None
        }
    }

    /// Send a connectivity test ping to all whitelisted Telegram chat IDs.
    pub async fn send_test_ping(&self) -> Result<usize, ChannelError> {
        let chat_ids = self.channels_cfg().telegram_allowed_chat_ids;
        if chat_ids.is_empty() {
            return Err(ChannelError::Config(
                "Chưa có Telegram Chat ID nào trong danh sách cho phép.".into(),
            ));
        }
        let token = self.resolve_token()?;
        let bot = Bot::new(token);
        let msg = "⚡ [JARVIS-RS] Kết nối Telegram hoạt động bình thường!";
        let mut sent = 0usize;
        for chat_id in chat_ids {
            bot.send_message(ChatId(chat_id), msg)
                .await
                .map_err(|e| ChannelError::Adapter("telegram".into(), e.to_string()))?;
            sent += 1;
        }
        Ok(sent)
    }

    fn resolve_token(&self) -> Result<String, ChannelError> {
        let token = self
            .secrets
            .get(keys::TELEGRAM_BOT_TOKEN)
            .map_err(|e| ChannelError::Config(e.to_string()))?
            .unwrap_or_default();
        let trimmed = token.trim().to_string();
        if trimmed.is_empty() {
            return Err(ChannelError::Config(
                "Telegram bot token is not configured in SecretStore".into(),
            ));
        }
        Ok(trimmed)
    }

    fn channels_cfg(&self) -> ChannelsConfig {
        self.config
            .read()
            .map(|c| c.channels.clone())
            .unwrap_or_default()
    }

    fn update_and_publish_status(&self, state: ChannelState, err: Option<String>) {
        {
            let mut st = self.status.lock().expect("status mutex poisoned");
            st.state = state;
            st.last_error = err.clone();
            if state == ChannelState::Offline {
                st.connected_as = None;
            }
        }
        let snap = self.status();
        self.event_bus.publish(CoreEvent::ChannelStatusChanged {
            channel: "telegram".into(),
            state: format!("{:?}", snap.state).to_ascii_lowercase(),
            detail: err.or(snap.connected_as),
        });
    }

    pub fn status(&self) -> ChannelStatusInfo {
        self.status.lock().expect("status mutex poisoned").clone()
    }

    /// Push a task completion notification to all whitelisted Telegram chat IDs.
    pub async fn notify_task_completed(&self, title: &str, body: &str) -> Result<(), ChannelError> {
        let chat_ids = self.channels_cfg().telegram_allowed_chat_ids;
        if chat_ids.is_empty() {
            return Ok(());
        }
        let token = self.resolve_token()?;
        let bot = Bot::new(token);
        let text = if body.trim().is_empty() {
            format!("[JARVIS] {title}")
        } else {
            format!("[JARVIS] {title}\n\n{body}")
        };

        for chat_id in chat_ids {
            for chunk in split_telegram_message(&text, TELEGRAM_MAX_LEN) {
                bot.send_message(ChatId(chat_id), chunk)
                    .await
                    .map_err(|e| ChannelError::Adapter("telegram".into(), e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Send an interactive approval prompt with Inline Keyboard (`approve:<id>` / `reject:<id>`)
    /// to all whitelisted Telegram chat IDs.
    pub async fn send_approval_prompt(&self, req: &ApprovalRequest) -> Result<(), ChannelError> {
        let chat_ids = self.channels_cfg().telegram_allowed_chat_ids;
        if chat_ids.is_empty() {
            return Ok(());
        }
        let token = self.resolve_token()?;
        let bot = Bot::new(token);
        let text = format!(
            "[Approval Required]\nTool: {}\nReason: {}\nArgs: {}",
            req.tool_name, req.reason, req.command_or_args
        );
        let truncated = split_telegram_message(&text, TELEGRAM_MAX_LEN)
            .into_iter()
            .next()
            .unwrap_or(text);
        let keyboard = build_approval_keyboard(&req.id);

        let mut entry_list = Vec::new();
        for chat_id in chat_ids {
            let sent = bot.send_message(ChatId(chat_id), truncated.clone())
                .reply_markup(keyboard.clone())
                .await
                .map_err(|e| ChannelError::Adapter("telegram".into(), e.to_string()))?;
            entry_list.push((ChatId(chat_id), sent.id, truncated.clone()));
        }
        self.approval_messages
            .lock()
            .expect("approval_messages mutex poisoned")
            .insert(req.id.clone(), entry_list);
        Ok(())
    }

    /// Return status footer string for resolved approvals.
    pub fn decision_status_footer(decision: ApprovalDecision) -> &'static str {
        match decision {
            ApprovalDecision::Approved => "✅ ĐÃ CHẤP THUẬN (Approved)",
            ApprovalDecision::Denied => "❌ ĐÃ TỪ CHỐI (Rejected)",
            ApprovalDecision::TimedOut => "⏱ ĐÃ HẾT HẠN (Timed Out)",
        }
    }

    /// Synchronize resolution of a pending approval across all tracked Telegram messages.
    pub async fn sync_approval_resolved(&self, id: &str, decision: ApprovalDecision) -> bool {
        let entries = self
            .approval_messages
            .lock()
            .expect("approval_messages mutex poisoned")
            .remove(id);
        let Some(list) = entries else {
            return false;
        };
        if list.is_empty() {
            return false;
        }

        let Ok(token) = self.resolve_token() else {
            return false;
        };
        let bot = Bot::new(token);
        let footer = Self::decision_status_footer(decision);

        for (chat_id, msg_id, orig_text) in list {
            let combined = format!("{orig_text}\n\n━━━━━━━━━━━━━━\n{footer}");
            let edited = split_telegram_message(&combined, TELEGRAM_MAX_LEN)
                .into_iter()
                .next()
                .unwrap_or(combined);
            let _ = bot.edit_message_text(chat_id, msg_id, edited).await;
        }
        true
    }

    pub async fn start(&self) -> Result<(), ChannelError> {
        let token = self.resolve_token()?;
        let _ = self.stop().await;

        let cancel_token = CancellationToken::new();
        {
            let mut guard = self.cancel.lock().expect("cancel mutex poisoned");
            *guard = Some(cancel_token.clone());
        }
        self.update_and_publish_status(ChannelState::Connecting, None);

        let bot = Bot::new(&token);
        let me = match bot.get_me().await {
            Ok(m) => m,
            Err(e) => {
                let msg = e.to_string();
                self.update_and_publish_status(ChannelState::Error, Some(msg.clone()));
                return Err(ChannelError::Adapter("telegram".into(), msg));
            }
        };

        {
            let mut st = self.status.lock().expect("status mutex poisoned");
            st.state = ChannelState::Online;
            st.connected_as = me.username.clone().or_else(|| Some(me.first_name.clone()));
            st.last_error = None;
        }
        let snap = self.status();
        self.event_bus.publish(CoreEvent::ChannelStatusChanged {
            channel: "telegram".into(),
            state: "online".into(),
            detail: snap.connected_as,
        });

        let cfg_msg = Arc::clone(&self.config);
        let agent_msg = self.agent.clone();
        let pairing_msg = Arc::clone(&self.pairing_until);
        let bus_msg = self.event_bus.clone();

        let cfg_cb = Arc::clone(&self.config);
        let broker_cb = Arc::clone(&self.approval_broker);
        let approval_msgs_cb = Arc::clone(&self.approval_messages);

        let message_handler = Update::filter_message().endpoint(move |bot: Bot, msg: Message| {
            let cfg = Arc::clone(&cfg_msg);
            let agent = agent_msg.clone();
            let pairing_until = Arc::clone(&pairing_msg);
            let event_bus = bus_msg.clone();
            async move {
                let chat_id = msg.chat.id.0;
                let channels_cfg = cfg.read().map(|c| c.channels.clone()).unwrap_or_default();

                let display_name = match msg.from.as_ref() {
                    Some(user) => match user.username.as_deref().filter(|u| !u.is_empty()) {
                        Some(uname) if !user.first_name.trim().is_empty() => {
                            format!("@{uname} ({})", user.first_name.trim())
                        }
                        Some(uname) => format!("@{uname}"),
                        None if !user.first_name.trim().is_empty() => {
                            user.first_name.trim().to_string()
                        }
                        None => format!("Chat {chat_id}"),
                    },
                    None => format!("Chat {chat_id}"),
                };

                if enforce_chat_whitelist(&channels_cfg, chat_id).is_err() {
                    let paired_now = {
                        let mut guard = pairing_until
                            .lock()
                            .expect("pairing_until mutex poisoned");
                        match *guard {
                            Some(until) if Instant::now() < until => {
                                *guard = None;
                                true
                            }
                            Some(_) => {
                                *guard = None;
                                false
                            }
                            None => false,
                        }
                    };

                    if paired_now {
                        if let Ok(mut app_cfg) = cfg.write() {
                            if !app_cfg.channels.telegram_allowed_chat_ids.contains(&chat_id) {
                                app_cfg.channels.telegram_allowed_chat_ids.push(chat_id);
                            }
                            let _ = app_cfg.save();
                        }
                        event_bus.publish(CoreEvent::Notification {
                            title: "Telegram Paired".into(),
                            body: format!("Đã ghép nối Chat ID {chat_id} ({display_name})"),
                        });
                        event_bus.publish(CoreEvent::ChannelNotification {
                            channel: "telegram".into(),
                            title: "Đã ghép nối Telegram".into(),
                            body: format!("Chat ID {chat_id} ({display_name})"),
                        });
                        let _ = bot
                            .send_message(
                                msg.chat.id,
                                format!(
                                    "✅ Đã ghép nối thành công với JARVIS-RS!\nChat ID `{chat_id}` đã được lưu vào danh sách cho phép. Bạn có thể gửi lệnh hoặc duyệt tác vụ ngay tại đây."
                                ),
                            )
                            .await;
                        return respond(());
                    }

                    event_bus.publish(CoreEvent::ChannelPairingCandidate {
                        channel: "telegram".into(),
                        sender_id: chat_id.to_string(),
                        display_name,
                        preview: msg.text().unwrap_or("").chars().take(60).collect(),
                    });
                    let _ = bot
                        .send_message(
                            msg.chat.id,
                            format!(
                                "Unauthorized. Your Telegram Chat ID is: {chat_id}. Bấm '+ Cho phép' trên JARVIS HUD hoặc dán ID này vào mục Telegram Bot."
                            ),
                        )
                        .await;
                    return respond(());
                }

                let Some(text) = msg.text() else {
                    return respond(());
                };
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return respond(());
                }

                if trimmed == "/start" {
                    let _ = bot
                        .send_message(
                            msg.chat.id,
                            "JARVIS online. Send any command or question to begin.",
                        )
                        .await;
                    return respond(());
                }

                let session_id = format!("telegram-{chat_id}");
                let owned_text = trimmed.to_string();
                let bot_spawn = bot.clone();
                let msg_chat_id = msg.chat.id;

                tokio::spawn(async move {
                    match agent.handle_user_prompt(&session_id, "telegram", &owned_text).await {
                        Ok(outcome) => {
                            let reply = if outcome.final_text.trim().is_empty() {
                                "(done)".to_string()
                            } else {
                                outcome.final_text
                            };
                            for chunk in split_telegram_message(&reply, TELEGRAM_MAX_LEN) {
                                if let Err(e) = bot_spawn.send_message(msg_chat_id, chunk).await {
                                    tracing::warn!("Telegram send_message failed: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            let _ = bot_spawn
                                .send_message(msg_chat_id, format!("Error: {e}"))
                                .await;
                        }
                    }
                });
                respond(())
            }
        });

        let callback_handler =
            Update::filter_callback_query().endpoint(move |bot: Bot, q: CallbackQuery| {
                let cfg = Arc::clone(&cfg_cb);
                let broker = Arc::clone(&broker_cb);
                let approval_msgs = Arc::clone(&approval_msgs_cb);
                async move {
                    let Some(data) = q.data.as_deref() else {
                        return respond(());
                    };
                    let msg_chat_id = q.message.as_ref().map(|m| m.chat().id);
                    let Some(chat_id) = msg_chat_id else {
                        return respond(());
                    };
                    let channels_cfg = cfg.read().map(|c| c.channels.clone()).unwrap_or_default();

                    match handle_approval_callback_data(&channels_cfg, chat_id.0, data, &broker) {
                        Ok((decision, id, resolved)) => {
                            let footer = TelegramAdapter::decision_status_footer(decision);
                            let label = if resolved {
                                match decision {
                                    ApprovalDecision::Approved => format!("Approved ({id})"),
                                    ApprovalDecision::Denied => format!("Rejected ({id})"),
                                    ApprovalDecision::TimedOut => format!("Timed out ({id})"),
                                }
                            } else {
                                format!("Approval {id} was already resolved")
                            };
                            let _ = bot.answer_callback_query(q.id).text(&label).await;

                            let tracked = approval_msgs
                                .lock()
                                .expect("approval_messages mutex poisoned")
                                .remove(&id);

                            if let Some(list) = tracked {
                                for (c_id, m_id, orig_text) in list {
                                    let combined = format!("{orig_text}\n\n━━━━━━━━━━━━━━\n{footer}");
                                    let edited = split_telegram_message(&combined, TELEGRAM_MAX_LEN)
                                        .into_iter()
                                        .next()
                                        .unwrap_or(combined);
                                    let _ = bot.edit_message_text(c_id, m_id, edited).await;
                                }
                            } else if let Some(MaybeInaccessibleMessage::Regular(m)) = q.message {
                                let orig_text = m.text().unwrap_or_default().to_string();
                                let combined = format!("{orig_text}\n\n━━━━━━━━━━━━━━\n{footer}");
                                let edited = split_telegram_message(&combined, TELEGRAM_MAX_LEN)
                                    .into_iter()
                                    .next()
                                    .unwrap_or(combined);
                                let _ = bot
                                    .edit_message_text(chat_id, m.id, edited)
                                    .await;
                            }
                        }
                        Err(err) => {
                            let _ = bot
                                .answer_callback_query(q.id)
                                .text(err.to_string())
                                .await;
                        }
                    }
                    respond(())
                }
            });

        let handler = dptree::entry()
            .branch(message_handler)
            .branch(callback_handler);

        let mut dispatcher = Dispatcher::builder(bot, handler)
            .distribution_function(|_| None::<std::convert::Infallible>)
            .build();
        let shutdown_token = dispatcher.shutdown_token();
        *self
            .shutdown_token
            .lock()
            .expect("shutdown_token mutex poisoned") = Some(shutdown_token.clone());

        let status_ref = Arc::clone(&self.status);
        let bus_ref = self.event_bus.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    let _ = shutdown_token.shutdown();
                }
                _ = dispatcher.dispatch() => {
                    {
                        let mut st = status_ref.lock().expect("status mutex poisoned");
                        st.state = ChannelState::Offline;
                        st.connected_as = None;
                    }
                    bus_ref.publish(CoreEvent::ChannelStatusChanged {
                        channel: "telegram".into(),
                        state: "offline".into(),
                        detail: None,
                    });
                }
            }
        });

        *self.task_handle.lock().await = Some(handle);
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), ChannelError> {
        {
            let mut guard = self.cancel.lock().expect("cancel mutex poisoned");
            if let Some(tok) = guard.take() {
                tok.cancel();
            }
        }
        {
            let mut sd = self
                .shutdown_token
                .lock()
                .expect("shutdown_token mutex poisoned");
            if let Some(tok) = sd.take() {
                let _ = tok.shutdown();
            }
        }
        if let Some(handle) = self.task_handle.lock().await.take() {
            let abort_handle = handle.abort_handle();
            if tokio::time::timeout(Duration::from_secs(3), handle)
                .await
                .is_err()
            {
                abort_handle.abort();
            }
        }
        self.update_and_publish_status(ChannelState::Offline, None);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn whitelist_enforcement_is_strict() {
        let empty = ChannelsConfig::default();
        assert!(!is_telegram_chat_allowed(&empty, 123456));
        assert!(matches!(
            enforce_chat_whitelist(&empty, 123456),
            Err(ChannelError::Unauthorized(id, ch)) if id == "123456" && ch == "telegram"
        ));

        let cfg = ChannelsConfig {
            telegram_allowed_chat_ids: vec![1001, -9988],
            ..Default::default()
        };
        assert!(is_telegram_chat_allowed(&cfg, 1001));
        assert!(is_telegram_chat_allowed(&cfg, -9988));
        assert!(!is_telegram_chat_allowed(&cfg, 9999));
        assert!(enforce_chat_whitelist(&cfg, 1001).is_ok());
        assert!(enforce_chat_whitelist(&cfg, 9999).is_err());
    }

    #[test]
    fn inline_callback_data_parsing_handles_approve_reject_and_invalid() {
        assert_eq!(
            parse_approval_callback_data("approve:req-123"),
            Some((ApprovalDecision::Approved, "req-123".to_string()))
        );
        assert_eq!(
            parse_approval_callback_data("reject:req-456"),
            Some((ApprovalDecision::Denied, "req-456".to_string()))
        );
        assert_eq!(parse_approval_callback_data("approve:"), None);
        assert_eq!(parse_approval_callback_data("reject:   "), None);
        assert_eq!(parse_approval_callback_data("other:req-1"), None);
    }

    #[tokio::test]
    async fn inline_callback_resolves_approval_broker_and_blocks_double_resolve() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        let broker = Arc::new(ApprovalBroker::new(bus, None));
        let cfg = ChannelsConfig {
            telegram_allowed_chat_ids: vec![777],
            ..Default::default()
        };

        let broker_req = Arc::clone(&broker);
        let handle = tokio::spawn(async move {
            broker_req
                .request_approval(
                    "s1",
                    "gmail_send_confirmed",
                    &serde_json::json!({"to": "a@b.com"}),
                    "send email",
                    crate::events::RiskTier::RequiresApproval,
                    true,
                )
                .await
        });

        let req_id = loop {
            if let CoreEvent::ApprovalRequested(req) = rx.recv().await.unwrap() {
                break req.id;
            }
        };

        // Unauthorized chat cannot resolve
        assert!(handle_approval_callback_data(
            &cfg,
            999,
            &format!("approve:{req_id}"),
            &broker
        )
        .is_err());

        // Authorized chat resolves once
        let (decision, id, resolved) =
            handle_approval_callback_data(&cfg, 777, &format!("approve:{req_id}"), &broker)
                .unwrap();
        assert_eq!(decision, ApprovalDecision::Approved);
        assert_eq!(id, req_id);
        assert!(resolved);

        // Second click reports already resolved
        let (_, _, second_resolved) =
            handle_approval_callback_data(&cfg, 777, &format!("reject:{req_id}"), &broker)
                .unwrap();
        assert!(!second_resolved);

        assert_eq!(handle.await.unwrap(), ApprovalDecision::Approved);
    }

    #[test]
    fn message_chunking_respects_limit_and_utf8() {
        let long = "Xin chào 🌍 ".repeat(600);
        let chunks = split_telegram_message(&long, TELEGRAM_MAX_LEN);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|c| c.chars().count() <= TELEGRAM_MAX_LEN));
        assert_eq!(chunks.concat(), long);
        assert_eq!(split_telegram_message("", 4096), vec![String::new()]);
    }

    #[test]
    fn pairing_window_lifecycle_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let db_path = dir.path().join("jarvis.db");
        let secrets = Arc::new(crate::secrets::InMemorySecretStore::new());
        let state = crate::AppState::init_with_paths(&cfg_path, &db_path, secrets).unwrap();
        let adapter = state.channels.telegram();

        assert_eq!(adapter.pairing_remaining_secs(), None);

        adapter.enable_pairing_window(Duration::from_secs(60));
        let rem = adapter.pairing_remaining_secs();
        assert!(rem.is_some());
        assert!(rem.unwrap() <= 60);

        adapter.cancel_pairing_window();
        assert_eq!(adapter.pairing_remaining_secs(), None);

        // Expired window automatically clears back to None
        {
            let mut guard = adapter.pairing_until.lock().unwrap();
            *guard = Some(Instant::now() - Duration::from_secs(1));
        }
        assert_eq!(adapter.pairing_remaining_secs(), None);
        assert!(adapter.pairing_until.lock().unwrap().is_none());
    }
}
