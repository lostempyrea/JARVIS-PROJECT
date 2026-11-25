//! External Communication Channels (Phase 3):
//! - [`discord`]: `serenity` Gateway bot + `!play`/`!pause`/`!resume`/`!skip`/`!stop`/`!join`/`!leave` wired to `DualMusicController`
//! - [`telegram`]: `teloxide` 2-way bot + strict `allowed_chat_ids` + push notifications + inline approval buttons
//! - [`gmail`]: Pure-Rust Gmail IMAP/SMTP + OAuth2 XOAUTH2 + 4 `ToolRegistry` tools
//! - [`ChannelManager`]: Lifecycle coordinator broadcasting `CoreEvent::ChannelStatusChanged` & `CoreEvent::ChannelNotification`

pub mod discord;
pub mod gmail;
pub mod telegram;

pub use discord::DiscordAdapter;
pub use gmail::{
    format_xoauth2_base64, format_xoauth2_sasl, parse_rfc822_email, GmailClient, GmailError,
    GmailReadTool, GmailSearchTool, GmailSendConfirmedTool, GmailSummarizeUnreadTool, ParsedEmail,
};
pub use telegram::{
    build_approval_keyboard, enforce_chat_whitelist, is_telegram_chat_allowed,
    parse_approval_callback_data, split_telegram_message, TelegramAdapter, TELEGRAM_MAX_LEN,
};

use crate::agent::approval::ApprovalBroker;
use crate::agent::AgentRuntime;
use crate::config::AppConfig;
use crate::events::{CoreEvent, EventBus};
use crate::music::DualMusicController;
use crate::secrets::{keys, SecretStore};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Errors produced by channel adapters and lifecycle management.
#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("channel configuration error: {0}")]
    Config(String),
    #[error("unauthorized user/chat '{0}' on channel '{1}'")]
    Unauthorized(String, String),
    #[error("channel '{0}' adapter error: {1}")]
    Adapter(String, String),
    #[error("unknown channel '{0}'")]
    UnknownChannel(String),
}

/// Connection state of a channel adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChannelState {
    #[default]
    Offline,
    Connecting,
    Online,
    Error,
}

/// Runtime status snapshot for a channel adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelStatusInfo {
    pub channel_id: String,
    pub state: ChannelState,
    pub connected_as: Option<String>,
    pub active_target: Option<String>,
    pub last_error: Option<String>,
}

impl ChannelStatusInfo {
    pub fn offline(channel_id: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            state: ChannelState::Offline,
            connected_as: None,
            active_target: None,
            last_error: None,
        }
    }
}

/// Coordinates Discord, Telegram, and Gmail adapters and bridges [`CoreEvent`] notifications.
pub struct ChannelManager {
    config: Arc<RwLock<AppConfig>>,
    secrets: Arc<dyn SecretStore>,
    event_bus: EventBus,
    discord: Arc<DiscordAdapter>,
    telegram: Arc<TelegramAdapter>,
    gmail: Arc<GmailClient>,
    bridge_cancel: std::sync::Mutex<Option<CancellationToken>>,
    bridge_task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl ChannelManager {
    pub fn new(
        config: Arc<RwLock<AppConfig>>,
        secrets: Arc<dyn SecretStore>,
        agent: AgentRuntime,
        approval_broker: Arc<ApprovalBroker>,
        music: Arc<DualMusicController>,
        event_bus: EventBus,
    ) -> Arc<Self> {
        let discord = Arc::new(DiscordAdapter::new(
            Arc::clone(&config),
            Arc::clone(&secrets),
            agent.clone(),
            music,
            event_bus.clone(),
        ));
        let telegram = Arc::new(TelegramAdapter::new(
            Arc::clone(&config),
            Arc::clone(&secrets),
            agent,
            approval_broker,
            event_bus.clone(),
        ));
        let gmail = Arc::new(GmailClient::new(Arc::clone(&config), Arc::clone(&secrets)));

        Arc::new(Self {
            config,
            secrets,
            event_bus,
            discord,
            telegram,
            gmail,
            bridge_cancel: std::sync::Mutex::new(None),
            bridge_task: tokio::sync::Mutex::new(None),
        })
    }

    pub fn discord(&self) -> &Arc<DiscordAdapter> {
        &self.discord
    }

    pub fn telegram(&self) -> &Arc<TelegramAdapter> {
        &self.telegram
    }

    pub fn gmail(&self) -> &Arc<GmailClient> {
        &self.gmail
    }

    /// Return current status snapshots for all 3 Phase 3 channels (`discord`, `telegram`, `gmail`).
    pub fn statuses(&self) -> Vec<ChannelStatusInfo> {
        let gmail_addr = self
            .config
            .read()
            .ok()
            .and_then(|c| c.channels.gmail_address.clone());
        let gmail_status = if self.gmail.is_configured() {
            ChannelStatusInfo {
                channel_id: "gmail".into(),
                state: ChannelState::Online,
                connected_as: gmail_addr,
                active_target: None,
                last_error: None,
            }
        } else {
            ChannelStatusInfo::offline("gmail")
        };

        vec![self.discord.status(), self.telegram.status(), gmail_status]
    }

    /// Start all configured channels and spawn the [`CoreEvent`] notification/approval bridge.
    pub async fn start_all(&self) -> Vec<(String, Result<(), ChannelError>)> {
        self.start_event_bridge().await;

        let mut results = Vec::new();
        if self
            .secrets
            .has(keys::DISCORD_BOT_TOKEN)
            .unwrap_or(false)
        {
            results.push(("discord".to_string(), self.discord.start().await));
        } else {
            self.event_bus.publish(CoreEvent::ChannelStatusChanged {
                channel: "discord".into(),
                state: "offline".into(),
                detail: Some("token not configured".into()),
            });
        }

        if self
            .secrets
            .has(keys::TELEGRAM_BOT_TOKEN)
            .unwrap_or(false)
        {
            results.push(("telegram".to_string(), self.telegram.start().await));
        } else {
            self.event_bus.publish(CoreEvent::ChannelStatusChanged {
                channel: "telegram".into(),
                state: "offline".into(),
                detail: Some("token not configured".into()),
            });
        }

        let gmail_state = if self.gmail.is_configured() {
            "online"
        } else {
            "offline"
        };
        let gmail_detail = self
            .config
            .read()
            .ok()
            .and_then(|c| c.channels.gmail_address.clone());
        self.event_bus.publish(CoreEvent::ChannelStatusChanged {
            channel: "gmail".into(),
            state: gmail_state.into(),
            detail: gmail_detail,
        });

        results
    }

    /// Start a specific channel (`"discord"` or `"telegram"`).
    pub async fn start_channel(&self, channel_id: &str) -> Result<(), ChannelError> {
        self.start_event_bridge().await;
        match channel_id {
            "discord" => self.discord.start().await,
            "telegram" => self.telegram.start().await,
            other => Err(ChannelError::UnknownChannel(other.to_string())),
        }
    }

    /// Stop a specific channel (`"discord"` or `"telegram"`).
    pub async fn stop_channel(&self, channel_id: &str) -> Result<(), ChannelError> {
        match channel_id {
            "discord" => self.discord.stop().await,
            "telegram" => self.telegram.stop().await,
            other => Err(ChannelError::UnknownChannel(other.to_string())),
        }
    }

    /// Cleanly stop all running channels and the event bridge.
    pub async fn stop_all(&self) {
        if let Some(cancel) = self
            .bridge_cancel
            .lock()
            .expect("bridge_cancel mutex poisoned")
            .take()
        {
            cancel.cancel();
        }
        if let Some(handle) = self.bridge_task.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
        let _ = self.discord.stop().await;
        let _ = self.telegram.stop().await;
    }

    async fn start_event_bridge(&self) {
        {
            let guard = self.bridge_task.lock().await;
            if guard.is_some() {
                return;
            }
        }

        let cancel = CancellationToken::new();
        {
            let mut cguard = self
                .bridge_cancel
                .lock()
                .expect("bridge_cancel mutex poisoned");
            *cguard = Some(cancel.clone());
        }

        let mut rx = self.event_bus.subscribe();
        let telegram = Arc::clone(&self.telegram);
        let event_bus = self.event_bus.clone();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    recv_res = rx.recv() => {
                        match recv_res {
                            Ok(CoreEvent::ApprovalRequested(req)) => {
                                if telegram.status().state == ChannelState::Online {
                                    let telegram_clone = Arc::clone(&telegram);
                                    let event_bus_clone = event_bus.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = telegram_clone.send_approval_prompt(&req).await {
                                            tracing::warn!("Failed to push Telegram approval prompt: {e}");
                                        } else {
                                            event_bus_clone.publish(CoreEvent::ChannelNotification {
                                                channel: "telegram".into(),
                                                title: format!("Approval requested: {}", req.tool_name),
                                                body: req.reason.clone(),
                                            });
                                        }
                                    });
                                }
                            }
                            Ok(CoreEvent::ApprovalResolved { id, decision }) => {
                                if telegram.status().state == ChannelState::Online {
                                    let telegram_clone = Arc::clone(&telegram);
                                    let id_clone = id.clone();
                                    tokio::spawn(async move {
                                        let _ = telegram_clone.sync_approval_resolved(&id_clone, decision).await;
                                    });
                                }
                            }
                            Ok(CoreEvent::TaskUpdated { task_id, status, title }) => {
                                let lower = status.to_ascii_lowercase();
                                if (lower == "completed" || lower == "failed")
                                    && telegram.status().state == ChannelState::Online
                                {
                                    let body = format!("Task `{task_id}` is now **{status}**.");
                                    let telegram_clone = Arc::clone(&telegram);
                                    let event_bus_clone = event_bus.clone();
                                    let title_clone = title.clone();
                                    let body_clone = body.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = telegram_clone.notify_task_completed(&title_clone, &body_clone).await {
                                            tracing::warn!("Failed to push Telegram task notification: {e}");
                                        } else {
                                            event_bus_clone.publish(CoreEvent::ChannelNotification {
                                                channel: "telegram".into(),
                                                title: title_clone,
                                                body: body_clone,
                                            });
                                        }
                                    });
                                }
                            }
                            Ok(CoreEvent::Notification { title, body }) => {
                                if telegram.status().state == ChannelState::Online {
                                    let telegram_clone = Arc::clone(&telegram);
                                    let event_bus_clone = event_bus.clone();
                                    let title_clone = title.clone();
                                    let body_clone = body.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = telegram_clone.notify_task_completed(&title_clone, &body_clone).await {
                                            tracing::warn!("Failed to push Telegram notification: {e}");
                                        } else {
                                            event_bus_clone.publish(CoreEvent::ChannelNotification {
                                                channel: "telegram".into(),
                                                title: title_clone,
                                                body: body_clone,
                                            });
                                        }
                                    });
                                }
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        });

        *self.bridge_task.lock().await = Some(handle);
    }
}
