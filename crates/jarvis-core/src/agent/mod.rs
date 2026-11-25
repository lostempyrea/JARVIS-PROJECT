//! Agent runtime coordinating sessions, database history, providers, tools, and approvals.

pub mod approval;
pub mod classifier;
pub mod tool_loop;

use crate::config::{AppConfig, ProviderKind, SecurityMode};
use crate::db::{Database, DbError, MessageRecord};
use crate::events::EventBus;
use crate::providers::traits::{ChatMessage, MessageRole};
use crate::providers::ProviderRegistry;
use crate::tools::{ToolContext, ToolRegistry};
use approval::ApprovalBroker;
use std::sync::{Arc, RwLock};
use thiserror::Error;
use tool_loop::{LoopError, ToolLoop, ToolLoopOutcome};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("database error: {0}")]
    Db(#[from] DbError),
    #[error("provider error: {0}")]
    Provider(#[from] crate::providers::traits::ProviderError),
    #[error("tool loop error: {0}")]
    Loop(#[from] LoopError),
    #[error("config lock poisoned")]
    ConfigLock,
}

const DEFAULT_SYSTEM_PROMPT: &str = "You are JARVIS, a helpful, concise, and security-conscious Windows desktop AI assistant. Reply in the user's language (for example, Vietnamese when the user writes in Vietnamese). When the user asks to play or control music on Discord (or in a Discord channel/DM), immediately call the `music_control` tool with `target: \"discord_voice\"` (or `\"both\"` if requested for both local and Discord) without asking for `guild_id` or `channel_id` — JARVIS auto-detects the user's server and voice channel.";

/// High-level agent orchestrator for a session turn.
#[derive(Clone)]
pub struct AgentRuntime {
    config: Arc<RwLock<AppConfig>>,
    db: Database,
    providers: ProviderRegistry,
    tools: Arc<ToolRegistry>,
    approval_broker: Arc<ApprovalBroker>,
    event_bus: EventBus,
}

impl AgentRuntime {
    pub fn new(
        config: Arc<RwLock<AppConfig>>,
        db: Database,
        providers: ProviderRegistry,
        tools: Arc<ToolRegistry>,
        approval_broker: Arc<ApprovalBroker>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            config,
            db,
            providers,
            tools,
            approval_broker,
            event_bus,
        }
    }

    /// Execute one user prompt in `session_id`, persisting messages to SQLite and streaming events.
    pub async fn handle_user_prompt(
        &self,
        session_id: &str,
        channel: &str,
        user_prompt: &str,
    ) -> Result<ToolLoopOutcome, AgentError> {
        let cfg = self
            .config
            .read()
            .map_err(|_| AgentError::ConfigLock)?
            .clone();

        self.db.ensure_session(session_id, "JARVIS Session", channel)?;
        self.db.add_message(
            &Uuid::new_v4().to_string(),
            session_id,
            MessageRole::User.as_str(),
            user_prompt,
            None,
            None,
        )?;

        let history = self.db.list_messages(session_id, 50)?;
        let messages = build_history_messages(&history);

        let provider = self.providers.active_provider(&cfg)?;
        let ctx = ToolContext {
            session_id: session_id.to_string(),
            workspace_root: cfg.workspace_root.clone(),
            db: self.db.clone(),
            event_bus: self.event_bus.clone(),
            full_auto_mode: cfg.security_mode == SecurityMode::FullAuto,
            agy_effort: cfg.agy_effort.clone(),
            active_model: cfg.active_model.clone(),
            agy_model: cfg.agy_model.clone(),
        };

        let initial_len = messages.len();
        let mut tool_loop = ToolLoop::new(
            provider,
            Arc::clone(&self.tools),
            Arc::clone(&self.approval_broker),
            self.event_bus.clone(),
        );

        let run_model = match cfg.active_provider {
            ProviderKind::AgyCli => cfg.agy_model.as_deref().unwrap_or(""),
            _ => cfg.active_model.as_str(),
        };
        let outcome = tool_loop.run(messages, run_model, &ctx).await?;

        // Persist newly produced assistant & tool messages.
        for msg in outcome.messages.iter().skip(initial_len) {
            let tc_json = msg
                .tool_calls
                .as_ref()
                .and_then(|tc| serde_json::to_string(tc).ok());
            self.db.add_message(
                &Uuid::new_v4().to_string(),
                session_id,
                msg.role.as_str(),
                &msg.content,
                tc_json.as_deref(),
                msg.tool_call_id.as_deref(),
            )?;
        }

        Ok(outcome)
    }
}

/// Build provider prompt history, trimming any orphaned leading `"tool"` rows whose
/// matching `"assistant"` tool-call turn was truncated outside the retained window.
fn build_history_messages(history: &[MessageRecord]) -> Vec<ChatMessage> {
    let mut messages = vec![ChatMessage::system(DEFAULT_SYSTEM_PROMPT)];
    let start_idx = history
        .iter()
        .position(|rec| rec.role != "tool")
        .unwrap_or(history.len());

    for rec in &history[start_idx..] {
        let role = match rec.role.as_str() {
            "system" => MessageRole::System,
            "assistant" => MessageRole::Assistant,
            "tool" => MessageRole::Tool,
            _ => MessageRole::User,
        };
        let tool_calls = rec
            .tool_calls_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        messages.push(ChatMessage {
            role,
            content: rec.content.clone(),
            tool_calls,
            tool_call_id: rec.tool_call_id.clone(),
            name: None,
        });
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_rec(role: &str, content: &str, tool_call_id: Option<&str>) -> MessageRecord {
        MessageRecord {
            id: Uuid::new_v4().to_string(),
            session_id: "s1".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            tool_calls_json: None,
            tool_call_id: tool_call_id.map(ToString::to_string),
            created_at: String::new(),
        }
    }

    #[test]
    fn build_history_messages_trims_leading_orphaned_tool_rows() {
        let history = vec![
            mk_rec("tool", "orphaned output 1", Some("tc_1")),
            mk_rec("tool", "orphaned output 2", Some("tc_2")),
            mk_rec("assistant", "summary", None),
            mk_rec("user", "next prompt", None),
        ];

        let msgs = build_history_messages(&history);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, MessageRole::System);
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[2].role, MessageRole::User);
    }
}
