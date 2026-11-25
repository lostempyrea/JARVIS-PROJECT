//! `jarvis-core`: Pure-Rust core runtime for JARVIS-RS.
//!
//! Provides configuration, OS credential storage, SQLite WAL + FTS5 persistence,
//! LLM streaming providers, risk-tiered approval brokering, built-in tools, and
//! the ReAct agent loop.

pub mod agent;
pub mod binary_manager;
pub mod channels;
pub mod config;
pub mod db;
pub mod events;
pub mod music;
pub mod providers;
pub mod secrets;
pub mod tools;
pub mod voice;

use agent::approval::ApprovalBroker;
use agent::AgentRuntime;
use binary_manager::BinaryManager;
use channels::{
    ChannelManager, GmailClient, GmailReadTool, GmailSearchTool, GmailSendConfirmedTool,
    GmailSummarizeUnreadTool,
};
use config::{AppConfig, ConfigError};
use db::{Database, DbError};
use events::EventBus;
use music::{DualMusicController, MusicControlTool};
use providers::ProviderRegistry;
use secrets::{KeyringSecretStore, SecretStore};
use std::path::Path;
use std::sync::{Arc, RwLock};
use thiserror::Error;
use tools::ToolRegistry;
use voice::VoicePipeline;

pub use tools::powershell_console::{
    launch_agy_in_windows_powershell, open_windows_powershell, PowerShellConsoleManager,
};
pub use tools::pty_manager::{PtyManager, PtyTabInfo};
pub use tools::vt_emulator::{CellStyle, StyledSpan, VtColor, VtScreenSnapshot};
pub use agent::classifier::{ClassificationResult, CommandRiskClassifier};
pub use agent::tool_loop::{LoopError, LoopGuard, ToolLoop, ToolLoopOutcome};
pub use binary_manager::{check_binaries, locate_binary, BinaryError, BinaryStatus};
pub use channels::{
    ChannelError, ChannelState, ChannelStatusInfo, DiscordAdapter, ParsedEmail, TelegramAdapter,
};
pub use config::{ChannelsConfig, ProviderKind, SecurityMode, UiConfig, VoiceConfig};
pub use db::{
    MemoryRecord, MessageRecord, PendingApprovalRecord, SessionRecord, TaskRecord,
};
pub use events::{ApprovalDecision, ApprovalRequest, CoreEvent, ReactorState, RiskTier, UiCommand};
pub use music::{
    GuildMusicQueue, LocalMusicPlayer, LocalMusicQueue, MusicError, MusicQueueSnapshot,
    MusicTarget, PlaybackState, TrackMeta,
};
pub use providers::traits::{ChatMessage, LlmProvider, MessageRole, StreamEvent, ToolCall, ToolDefinition};
pub use secrets::{InMemorySecretStore, SecretError};
pub use voice::{
    AudioDeviceInfo, AudioPlayer, EdgeTtsBackend, GroqWhisperStt, LocalHttpWhisperStt,
    MicCapture, OfflineWhisperCliStt, OfflineWindowsSapiTts, OpenAiTtsBackend, OpenAiWhisperStt,
    PttController, SttBackend, SttResult, TtsBackend, VadConfig, VadEvent, VadProcessor, VadState,
    VoiceError, VoiceResult, WakeMatch, WakeWordMatcher,
};

/// Errors during [`AppState`] initialization.
#[derive(Debug, Error)]
pub enum CoreInitError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("database error: {0}")]
    Db(#[from] DbError),
}

/// Shared application state wiring together configuration, secrets, SQLite,
/// provider registry, built-in tools, approval broker, event bus, voice pipeline,
/// binary manager, dual music controller, channel manager, and agent runtime.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub secrets: Arc<dyn SecretStore>,
    pub db: Database,
    pub event_bus: EventBus,
    pub providers: ProviderRegistry,
    pub tools: Arc<ToolRegistry>,
    pub approval_broker: Arc<ApprovalBroker>,
    pub binary_manager: Arc<BinaryManager>,
    pub music: Arc<DualMusicController>,
    pub agent: AgentRuntime,
    pub voice: VoicePipeline,
    pub channels: Arc<ChannelManager>,
    pub ps_console: Arc<PowerShellConsoleManager>,
    pub pty: Arc<PtyManager>,
}

impl AppState {
    /// Initialize [`AppState`] using `%APPDATA%\Jarvis\config.toml`,
    /// `%APPDATA%\Jarvis\jarvis.db`, and Windows Credential Manager (`jarvis-rs`).
    pub fn init_default() -> Result<Self, CoreInitError> {
        let config = AppConfig::load_or_default()?;
        let db = Database::open(&AppConfig::default_db_path())?;
        let secrets: Arc<dyn SecretStore> = Arc::new(KeyringSecretStore::new());
        Ok(Self::from_parts(config, secrets, db))
    }

    /// Initialize [`AppState`] with custom config and database paths plus a provided [`SecretStore`].
    pub fn init_with_paths(
        config_path: &Path,
        db_path: &Path,
        secrets: Arc<dyn SecretStore>,
    ) -> Result<Self, CoreInitError> {
        let config = AppConfig::load_from_path(config_path)?;
        let db = Database::open(db_path)?;
        Ok(Self::from_parts(config, secrets, db))
    }

    /// Construct [`AppState`] from pre-built components (ideal for testing).
    pub fn from_parts(
        config: AppConfig,
        secrets: Arc<dyn SecretStore>,
        db: Database,
    ) -> Self {
        let event_bus = EventBus::default();
        let providers = ProviderRegistry::new(Arc::clone(&secrets));
        let approval_broker = Arc::new(ApprovalBroker::new(event_bus.clone(), Some(db.clone())));
        let config_lock = Arc::new(RwLock::new(config));

        let binary_manager = Arc::new(BinaryManager::with_event_bus(event_bus.clone()));
        let music = DualMusicController::new(
            Arc::clone(&config_lock),
            Arc::clone(&binary_manager),
            event_bus.clone(),
        );
        let gmail_client = Arc::new(GmailClient::new(
            Arc::clone(&config_lock),
            Arc::clone(&secrets),
        ));

        #[allow(unused_variables)]
        let ps_console = Arc::new(PowerShellConsoleManager::new(event_bus.clone()));
        let pty = Arc::new(PtyManager::new(event_bus.clone()));

        let mut registry = ToolRegistry::with_builtins();
        registry.register(Arc::new(MusicControlTool::new(Arc::clone(&music))));
        registry.register(Arc::new(GmailSearchTool::new(Arc::clone(&gmail_client))));
        registry.register(Arc::new(GmailReadTool::new(Arc::clone(&gmail_client))));
        registry.register(Arc::new(GmailSummarizeUnreadTool::new(Arc::clone(
            &gmail_client,
        ))));
        registry.register(Arc::new(GmailSendConfirmedTool::new(gmail_client)));
        registry.register(Arc::new(tools::agy_tool::AgyDelegateTool::with_config_and_pty(
            Arc::clone(&config_lock),
            Arc::clone(&pty),
        )));
        let tools = Arc::new(registry);

        let agent = AgentRuntime::new(
            Arc::clone(&config_lock),
            db.clone(),
            providers.clone(),
            Arc::clone(&tools),
            Arc::clone(&approval_broker),
            event_bus.clone(),
        );
        let voice = VoicePipeline::new(
            Arc::clone(&config_lock),
            Arc::clone(&secrets),
            event_bus.clone(),
            agent.clone(),
        );
        let channels = ChannelManager::new(
            Arc::clone(&config_lock),
            Arc::clone(&secrets),
            agent.clone(),
            Arc::clone(&approval_broker),
            Arc::clone(&music),
            event_bus.clone(),
        );
        #[allow(unused_variables)]
        let ps_console = Arc::new(PowerShellConsoleManager::new(event_bus.clone()));

        Self {
            config: config_lock,
            secrets,
            db,
            event_bus,
            providers,
            tools,
            approval_broker,
            binary_manager,
            music,
            agent,
            voice,
            channels,
            ps_console,
            pty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn app_state_initializes_with_paths_and_in_memory_secrets() {
        let dir = TempDir::new().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let db_path = dir.path().join("jarvis.db");
        let secrets = Arc::new(InMemorySecretStore::new());

        let state = AppState::init_with_paths(&cfg_path, &db_path, secrets).unwrap();
        assert!(state.tools.get("terminal").is_some());
        assert!(state.tools.get("fs_read").is_some());
        assert!(state.tools.get("memory_recall").is_some());
        assert!(state.tools.get("delegate_to_agy").is_some());
        assert!(state.tools.get("music_control").is_some());
        assert!(state.tools.get("gmail_search").is_some());
        assert!(state.tools.get("gmail_read").is_some());
        assert!(state.tools.get("gmail_summarize_unread").is_some());
        assert!(state.tools.get("gmail_send_confirmed").is_some());
    }
}
