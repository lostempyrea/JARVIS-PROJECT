//! Core reactive state, approval payloads, UI commands, and broadcast event bus.

use crate::config::{ProviderKind, SecurityMode};
use crate::music::MusicQueueSnapshot;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Overall reactor state displayed by the orb UI and channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReactorState {
    #[default]
    Idle,
    Listening,
    Executing,
    AwaitingApproval,
    Notification,
}

/// Risk classification tier for tool and shell executions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTier {
    SafeReadOnly,
    RequiresApproval,
}

/// Resolution decision for a pending approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    Denied,
    TimedOut,
}

impl ApprovalDecision {
    /// Returns `true` only when the action was explicitly or policy-approved.
    pub fn is_approved(self) -> bool {
        matches!(self, Self::Approved)
    }
}

/// Metadata describing an action awaiting user confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub session_id: String,
    pub tool_name: String,
    pub command_or_args: String,
    pub reason: String,
    pub risk_tier: RiskTier,
    pub created_at: DateTime<Utc>,
}

/// Metadata describing an available application release discovered by the auto-updater.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReleaseInfo {
    pub tag_name: String,
    pub version: String,
    pub title: String,
    pub notes: String,
    pub html_url: String,
    pub published_at: Option<String>,
    pub exe_download_url: String,
    pub sha256_download_url: Option<String>,
    pub expected_sha256: Option<String>,
    pub size_bytes: Option<u64>,
}

/// Events emitted by `jarvis-core` to UI clients, tray/orb, and channels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum CoreEvent {
    ReactorStateChanged {
        state: ReactorState,
    },
    TextDelta {
        session_id: String,
        delta: String,
    },
    ToolCallStarted {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolCallCompleted {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        success: bool,
        output_preview: String,
    },
    TerminalLine {
        session_id: String,
        stream: String,
        line: String,
    },
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved {
        id: String,
        decision: ApprovalDecision,
    },
    TaskUpdated {
        task_id: String,
        status: String,
        title: String,
    },
    Notification {
        title: String,
        body: String,
    },
    VoiceTranscript {
        session_id: String,
        text: String,
        is_command: bool,
    },
    MicLevel {
        db: f32,
    },
    BinaryInstallProgress {
        binary: String,
        status: String,
        bytes_downloaded: u64,
        total_bytes: Option<u64>,
    },
    UpdateAvailable {
        release: UpdateReleaseInfo,
        manual_check: bool,
    },
    UpdateDownloadProgress {
        version: String,
        status: String,
        bytes_downloaded: u64,
        total_bytes: Option<u64>,
    },
    MusicStateUpdated(MusicQueueSnapshot),
    ChannelStatusChanged {
        channel: String,
        state: String,
        detail: Option<String>,
    },
    ChannelNotification {
        channel: String,
        title: String,
        body: String,
    },
    ChannelPairingCandidate {
        channel: String,
        sender_id: String,
        display_name: String,
        preview: String,
    },
    PtyUpdated {
        tab_id: String,
    },
    Error {
        session_id: Option<String>,
        message: String,
    },
    TurnComplete {
        session_id: String,
        full_text: String,
    },
}

/// Commands sent from UI / channel adapters into `jarvis-core`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum UiCommand {
    SubmitPrompt {
        session_id: String,
        prompt: String,
    },
    ResolveApproval {
        id: String,
        decision: ApprovalDecision,
    },
    CancelSession {
        session_id: String,
    },
    SetSecurityMode {
        mode: SecurityMode,
    },
    SetProvider {
        provider: ProviderKind,
        model: String,
    },
}

/// Broadcast event bus wrapping `tokio::sync::broadcast::Sender<CoreEvent>`.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<CoreEvent>,
}

impl EventBus {
    /// Create a new event bus with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(16));
        Self { sender }
    }

    /// Publish an event to all current subscribers.
    /// Returns the number of active receivers notified (0 if none are listening).
    pub fn publish(&self, event: CoreEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }

    /// Subscribe to core events.
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.sender.subscribe()
    }

    /// Access the underlying broadcast sender.
    pub fn sender(&self) -> &broadcast::Sender<CoreEvent> {
        &self.sender
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(2048)
    }
}
