//! Application configuration persisted as TOML under `%APPDATA%\Jarvis\config.toml`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur when loading or saving [`AppConfig`].
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file '{path}': {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write config file '{path}': {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse TOML config '{path}': {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize TOML config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Active LLM backend provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    #[serde(rename = "openai_compat", alias = "open_ai_compat")]
    OpenAiCompat,
    Ollama,
    AgyCli,
}

/// Tool and command approval policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecurityMode {
    #[default]
    SmartTiered,
    FullAuto,
}

/// Voice pipeline configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    pub wake_word_enabled: bool,
    pub wake_phrases: Vec<String>,
    pub vad_pre_roll_ms: u32,
    pub stt_backend: String,
    pub tts_backend: String,
    pub tts_voice: String,
    pub vad_threshold_db: f32,
    pub vad_silence_ms: u64,
    pub vad_min_speech_ms: u64,
    pub listening_timeout_ms: u64,
    pub stt_language: Option<String>,
    pub stt_model: Option<String>,
    pub local_stt_url: String,
    pub whisper_cli_path: Option<String>,
    pub whisper_model_path: Option<String>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub tts_rate: f32,
    pub tts_pitch: f32,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            wake_word_enabled: true,
            wake_phrases: vec!["hey jarvis".to_string(), "jarvis".to_string()],
            vad_pre_roll_ms: 1200,
            stt_backend: "groq_whisper".to_string(),
            tts_backend: "edge_tts".to_string(),
            tts_voice: "vi-VN-HoaiMyNeural".to_string(),
            vad_threshold_db: -40.0,
            vad_silence_ms: 800,
            vad_min_speech_ms: 250,
            listening_timeout_ms: 8000,
            stt_language: None,
            stt_model: None,
            local_stt_url: "http://127.0.0.1:8080/v1".to_string(),
            whisper_cli_path: None,
            whisper_model_path: None,
            input_device: None,
            output_device: None,
            tts_rate: 1.0,
            tts_pitch: 0.0,
        }
    }
}

/// Messaging and email channel configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelsConfig {
    pub telegram_allowed_chat_ids: Vec<i64>,
    pub discord_guild_id: Option<u64>,
    pub discord_allowed_user_ids: Vec<u64>,
    pub discord_auto_join_voice: bool,
    pub discord_default_volume: f32,
    pub gmail_address: Option<String>,
    pub gmail_auth_mode: String,
    pub gmail_oauth_client_id: Option<String>,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            telegram_allowed_chat_ids: Vec::new(),
            discord_guild_id: None,
            discord_allowed_user_ids: Vec::new(),
            discord_auto_join_voice: false,
            discord_default_volume: 1.0,
            gmail_address: None,
            gmail_auth_mode: "app_password".to_string(),
            gmail_oauth_client_id: None,
        }
    }
}

/// Desktop orb and UI layout configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UiConfig {
    pub orb_x: Option<f64>,
    pub orb_y: Option<f64>,
}

/// Root JARVIS-RS configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub active_provider: ProviderKind,
    pub active_model: String,
    #[serde(default)]
    pub agy_model: Option<String>,
    pub openai_base_url: String,
    pub ollama_base_url: String,
    pub agy_effort: String,
    pub security_mode: SecurityMode,
    pub workspace_root: PathBuf,
    pub agy_use_windows_powershell: bool,
    pub voice: VoiceConfig,
    pub channels: ChannelsConfig,
    pub ui: UiConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            active_provider: ProviderKind::OpenAiCompat,
            active_model: "gpt-4o-mini".to_string(),
            agy_model: None,
            openai_base_url: "https://api.openai.com/v1".to_string(),
            ollama_base_url: "http://127.0.0.1:11434".to_string(),
            agy_effort: "medium".to_string(),
            security_mode: SecurityMode::SmartTiered,
            workspace_root,
            agy_use_windows_powershell: false,
            voice: VoiceConfig::default(),
            channels: ChannelsConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

impl AppConfig {
    /// Returns the default configuration directory (`%APPDATA%\Jarvis` on Windows).
    pub fn default_dir() -> PathBuf {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            if !appdata.is_empty() {
                return PathBuf::from(appdata).join("Jarvis");
            }
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Jarvis")
    }

    /// Returns the default configuration file path (`%APPDATA%\Jarvis\config.toml`).
    pub fn default_path() -> PathBuf {
        Self::default_dir().join("config.toml")
    }

    /// Returns the default SQLite database path (`%APPDATA%\Jarvis\jarvis.db`).
    pub fn default_db_path() -> PathBuf {
        Self::default_dir().join("jarvis.db")
    }

    /// Load configuration from the default path, returning [`AppConfig::default`]
    /// if the file does not yet exist.
    pub fn load_or_default() -> Result<Self, ConfigError> {
        Self::load_from_path(&Self::default_path())
    }

    /// Save configuration to the default path.
    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to_path(&Self::default_path())
    }

    /// Load configuration from a specific path, returning [`AppConfig::default`]
    /// if the file does not exist.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Persist configuration to a specific path, creating parent directories if needed.
    pub fn save_to_path(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
        }
        let serialized = toml::to_string_pretty(self)?;
        std::fs::write(path, serialized).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_match_specification() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.active_provider, ProviderKind::OpenAiCompat);
        assert_eq!(cfg.agy_model, None);
        assert_eq!(cfg.security_mode, SecurityMode::SmartTiered);
        assert_eq!(cfg.voice.vad_pre_roll_ms, 1200);
        assert_eq!(cfg.voice.tts_voice, "vi-VN-HoaiMyNeural");
        assert!(cfg.voice.wake_word_enabled);
        assert_eq!(cfg.channels.gmail_auth_mode, "app_password");
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.toml");
        let cfg = AppConfig::load_from_path(&path).unwrap();
        assert_eq!(cfg.active_provider, ProviderKind::OpenAiCompat);
        assert_eq!(cfg.voice.tts_voice, "vi-VN-HoaiMyNeural");
    }

    #[test]
    fn save_and_reload_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("config.toml");

        let mut cfg = AppConfig {
            active_provider: ProviderKind::Ollama,
            active_model: "qwen2.5:14b".to_string(),
            security_mode: SecurityMode::FullAuto,
            agy_effort: "high".to_string(),
            ..Default::default()
        };
        cfg.channels.telegram_allowed_chat_ids = vec![123456789, -987654321];
        cfg.channels.discord_guild_id = Some(42);
        cfg.channels.gmail_address = Some("jarvis@example.com".to_string());
        cfg.ui.orb_x = Some(120.5);
        cfg.ui.orb_y = Some(840.0);

        cfg.save_to_path(&path).unwrap();
        let loaded = AppConfig::load_from_path(&path).unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("partial.toml");
        std::fs::write(
            &path,
            r#"
active_provider = "agy_cli"
active_model = "gemini-pro"
"#,
        )
        .unwrap();

        let loaded = AppConfig::load_from_path(&path).unwrap();
        assert_eq!(loaded.active_provider, ProviderKind::AgyCli);
        assert_eq!(loaded.active_model, "gemini-pro");
        assert_eq!(loaded.voice.vad_pre_roll_ms, 1200);
        assert_eq!(loaded.voice.tts_voice, "vi-VN-HoaiMyNeural");
    }
}
