//! Speech-to-Text (`SttBackend`) trait and provider implementations
//! (Groq Whisper, OpenAI Whisper, Local HTTP Whisper, and Offline `whisper-cli` subprocess).

use super::{VoiceError, VoiceResult};
use crate::config::VoiceConfig;
use crate::secrets::{keys, SecretStore};
use async_trait::async_trait;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

pub const DEFAULT_GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
pub const DEFAULT_GROQ_WHISPER_MODEL: &str = "whisper-large-v3-turbo";
pub const FALLBACK_GROQ_WHISPER_MODEL: &str = "whisper-large-v3";

pub fn is_rate_limit_error(err: &VoiceError) -> bool {
    let lower = err.to_string().to_ascii_lowercase();
    lower.contains("429")
        || lower.contains("rate_limit_exceeded")
        || lower.contains("too many requests")
}

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENAI_WHISPER_MODEL: &str = "whisper-1";

pub const DEFAULT_LOCAL_WHISPER_BASE_URL: &str = "http://127.0.0.1:8080/v1";
pub const DEFAULT_LOCAL_WHISPER_MODEL: &str = "whisper-1";

pub fn is_allowed_en_vi_language(lang: &str) -> bool {
    matches!(
        lang.trim().to_ascii_lowercase().as_str(),
        "vi" | "vie" | "vi-vn" | "vietnamese" | "en" | "eng" | "en-us" | "en-gb" | "english"
    )
}

pub fn is_latin_or_vietnamese_text(text: &str) -> bool {
    text.chars().all(|ch| {
        if !ch.is_alphabetic() {
            true
        } else {
            ch.is_ascii_alphabetic()
                || ('\u{00C0}'..='\u{024F}').contains(&ch)
                || ('\u{1E00}'..='\u{1EFF}').contains(&ch)
        }
    })
}

fn normalize_explicit_en_vi_language(language: Option<&str>) -> Option<&'static str> {
    let l = language?.trim().to_ascii_lowercase();
    match l.as_str() {
        "vi" | "vie" | "vi-vn" | "vietnamese" => Some("vi"),
        "en" | "eng" | "en-us" | "en-gb" | "english" => Some("en"),
        _ => None,
    }
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Normalized Speech-to-Text transcription result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SttResult {
    pub text: String,
    pub language: Option<String>,
    pub duration_ms: Option<u64>,
    pub provider: String,
    pub model: String,
}

/// Map an audio filename extension to its corresponding MIME type.
pub fn mime_type_for_filename(filename: &str) -> &'static str {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("wav")
        .to_ascii_lowercase();
    match ext.as_str() {
        "wav" => "audio/wav",
        "webm" => "audio/webm",
        "ogg" | "oga" => "audio/ogg",
        "mp4" | "m4a" => "audio/mp4",
        "mp3" | "mpeg" => "audio/mpeg",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

/// Asynchronous Speech-to-Text backend trait.
#[async_trait]
pub trait SttBackend: Send + Sync {
    /// Human-readable provider identifier (e.g. `"groq_whisper"`, `"openai_whisper"`).
    fn provider_name(&self) -> &'static str;

    /// Transcribe in-memory WAV bytes into text.
    async fn transcribe_wav(
        &self,
        wav_bytes: &[u8],
        language: Option<&str>,
    ) -> VoiceResult<SttResult>;

    /// Transcribe arbitrary encoded audio bytes (`wav`, `webm`, `ogg`, `mp3`, etc.).
    async fn transcribe_audio(
        &self,
        audio_bytes: &[u8],
        filename: &str,
        language: Option<&str>,
    ) -> VoiceResult<SttResult> {
        let _ = filename;
        self.transcribe_wav(audio_bytes, language).await
    }
}

#[derive(Debug, Deserialize)]
struct WhisperApiResponse {
    text: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
}

#[allow(clippy::too_many_arguments)]
async fn transcribe_openai_compatible_once(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    provider: &'static str,
    model: &str,
    audio_bytes: &[u8],
    filename: &str,
    explicit_lang: Option<&str>,
) -> VoiceResult<SttResult> {
    if audio_bytes.is_empty() {
        return Err(VoiceError::Stt("Cannot transcribe empty audio payload".into()));
    }

    let trimmed_base = base_url.trim_end_matches('/');
    let endpoint = if trimmed_base.ends_with("/audio/transcriptions") {
        trimmed_base.to_string()
    } else {
        format!("{trimmed_base}/audio/transcriptions")
    };
    let safe_name = if filename.trim().is_empty() {
        "audio.wav"
    } else {
        filename
    };
    let mime = mime_type_for_filename(safe_name);
    let file_part = Part::bytes(audio_bytes.to_vec())
        .file_name(safe_name.to_string())
        .mime_str(mime)
        .map_err(|e| VoiceError::Stt(format!("Invalid multipart mime type: {e}")))?;

    let mut form = Form::new()
        .part("file", file_part)
        .text("model", model.to_string())
        .text("temperature", "0")
        .text(
            "prompt",
            "Chỉ nhận diện Tiếng Việt và English. Hey Jarvis, Jarvis, xin chào Jarvis, mở nhạc, kiểm tra hệ thống, mở terminal, AGY.",
        )
        .text("response_format", "verbose_json");

    if let Some(lang) = explicit_lang.filter(|l| !l.trim().is_empty()) {
        form = form.text("language", lang.to_string());
    }

    let mut req = client.post(&endpoint).multipart(form);
    if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        req = req.bearer_auth(key);
    }

    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(VoiceError::Stt(format!(
            "{provider} STT request failed ({status}): {body}"
        )));
    }

    let parsed: WhisperApiResponse = resp.json().await.map_err(|e| {
        VoiceError::Stt(format!("Failed to parse {provider} STT JSON response: {e}"))
    })?;

    Ok(SttResult {
        text: parsed.text.trim().to_string(),
        language: parsed.language.or_else(|| explicit_lang.map(str::to_string)),
        duration_ms: parsed.duration.map(|s| (s * 1000.0).round() as u64),
        provider: provider.to_string(),
        model: model.to_string(),
    })
}

/// Shared OpenAI-compatible `/audio/transcriptions` multipart uploader.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn transcribe_openai_compatible(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    provider: &'static str,
    model: &str,
    audio_bytes: &[u8],
    filename: &str,
    language: Option<&str>,
) -> VoiceResult<SttResult> {
    let explicit = normalize_explicit_en_vi_language(language);
    match explicit {
        Some(lang) => {
            let mut res = transcribe_openai_compatible_once(
                client,
                base_url,
                api_key,
                provider,
                model,
                audio_bytes,
                filename,
                Some(lang),
            )
            .await?;
            if !is_latin_or_vietnamese_text(&res.text) {
                res.text.clear();
            }
            Ok(res)
        }
        None => {
            let res = transcribe_openai_compatible_once(
                client,
                base_url,
                api_key,
                provider,
                model,
                audio_bytes,
                filename,
                None,
            )
            .await?;
            let lang_ok = res.language.as_deref().is_none_or(is_allowed_en_vi_language);
            let script_ok = is_latin_or_vietnamese_text(&res.text);
            if !res.text.is_empty() && (!lang_ok || !script_ok) {
                let mut retry = transcribe_openai_compatible_once(
                    client,
                    base_url,
                    api_key,
                    provider,
                    model,
                    audio_bytes,
                    filename,
                    Some("vi"),
                )
                .await?;
                if !is_latin_or_vietnamese_text(&retry.text) {
                    retry.text.clear();
                }
                return Ok(retry);
            }
            Ok(res)
        }
    }
}

/// High-speed Groq Whisper STT client (`whisper-large-v3-turbo`).
#[derive(Debug, Clone)]
pub struct GroqWhisperStt {
    api_key: String,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl GroqWhisperStt {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_GROQ_WHISPER_MODEL.to_string(),
            base_url: DEFAULT_GROQ_BASE_URL.to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[async_trait]
impl SttBackend for GroqWhisperStt {
    fn provider_name(&self) -> &'static str {
        "groq_whisper"
    }

    async fn transcribe_wav(
        &self,
        wav_bytes: &[u8],
        language: Option<&str>,
    ) -> VoiceResult<SttResult> {
        self.transcribe_audio(wav_bytes, "audio.wav", language).await
    }

    async fn transcribe_audio(
        &self,
        audio_bytes: &[u8],
        filename: &str,
        language: Option<&str>,
    ) -> VoiceResult<SttResult> {
        match transcribe_openai_compatible(
            &self.client,
            &self.base_url,
            Some(&self.api_key),
            self.provider_name(),
            &self.model,
            audio_bytes,
            filename,
            language,
        )
        .await
        {
            Ok(res) => Ok(res),
            Err(err) if is_rate_limit_error(&err) => {
                let alt_model = if self.model == DEFAULT_GROQ_WHISPER_MODEL {
                    FALLBACK_GROQ_WHISPER_MODEL
                } else {
                    DEFAULT_GROQ_WHISPER_MODEL
                };
                match transcribe_openai_compatible(
                    &self.client,
                    &self.base_url,
                    Some(&self.api_key),
                    self.provider_name(),
                    alt_model,
                    audio_bytes,
                    filename,
                    language,
                )
                .await
                {
                    Ok(res) => Ok(res),
                    Err(err2) if is_rate_limit_error(&err2) => {
                        tokio::time::sleep(Duration::from_millis(3200)).await;
                        transcribe_openai_compatible(
                            &self.client,
                            &self.base_url,
                            Some(&self.api_key),
                    self.provider_name(),
                            &self.model,
                            audio_bytes,
                            filename,
                            language,
                        )
                        .await
                    }
                    Err(err2) => Err(err2),
                }
            }
            Err(err) => Err(err),
        }
    }
}

/// OpenAI Whisper STT client (`whisper-1`).
#[derive(Debug, Clone)]
pub struct OpenAiWhisperStt {
    api_key: String,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl OpenAiWhisperStt {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_OPENAI_WHISPER_MODEL.to_string(),
            base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[async_trait]
impl SttBackend for OpenAiWhisperStt {
    fn provider_name(&self) -> &'static str {
        "openai_whisper"
    }

    async fn transcribe_wav(
        &self,
        wav_bytes: &[u8],
        language: Option<&str>,
    ) -> VoiceResult<SttResult> {
        self.transcribe_audio(wav_bytes, "audio.wav", language).await
    }

    async fn transcribe_audio(
        &self,
        audio_bytes: &[u8],
        filename: &str,
        language: Option<&str>,
    ) -> VoiceResult<SttResult> {
        match transcribe_openai_compatible(
            &self.client,
            &self.base_url,
            Some(&self.api_key),
            self.provider_name(),
            &self.model,
            audio_bytes,
            filename,
            language,
        )
        .await
        {
            Ok(res) => Ok(res),
            Err(err) if is_rate_limit_error(&err) => {
                let alt_model = if self.model == DEFAULT_GROQ_WHISPER_MODEL {
                    FALLBACK_GROQ_WHISPER_MODEL
                } else {
                    DEFAULT_GROQ_WHISPER_MODEL
                };
                match transcribe_openai_compatible(
                    &self.client,
                    &self.base_url,
                    Some(&self.api_key),
                    self.provider_name(),
                    alt_model,
                    audio_bytes,
                    filename,
                    language,
                )
                .await
                {
                    Ok(res) => Ok(res),
                    Err(err2) if is_rate_limit_error(&err2) => {
                        tokio::time::sleep(Duration::from_millis(3200)).await;
                        transcribe_openai_compatible(
                            &self.client,
                            &self.base_url,
                            Some(&self.api_key),
                    self.provider_name(),
                            &self.model,
                            audio_bytes,
                            filename,
                            language,
                        )
                        .await
                    }
                    Err(err2) => Err(err2),
                }
            }
            Err(err) => Err(err),
        }
    }
}

/// Local OpenAI-compatible HTTP Whisper endpoint client (e.g. `whisper.cpp` server, `faster-whisper-server`).
#[derive(Debug, Clone)]
pub struct LocalHttpWhisperStt {
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl LocalHttpWhisperStt {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: DEFAULT_LOCAL_WHISPER_MODEL.to_string(),
            api_key: None,
            client: reqwest::Client::new(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }
}

impl Default for LocalHttpWhisperStt {
    fn default() -> Self {
        Self::new(DEFAULT_LOCAL_WHISPER_BASE_URL)
    }
}

#[async_trait]
impl SttBackend for LocalHttpWhisperStt {
    fn provider_name(&self) -> &'static str {
        "local_http_whisper"
    }

    async fn transcribe_wav(
        &self,
        wav_bytes: &[u8],
        language: Option<&str>,
    ) -> VoiceResult<SttResult> {
        self.transcribe_audio(wav_bytes, "audio.wav", language).await
    }

    async fn transcribe_audio(
        &self,
        audio_bytes: &[u8],
        filename: &str,
        language: Option<&str>,
    ) -> VoiceResult<SttResult> {
        transcribe_openai_compatible(
            &self.client,
            &self.base_url,
            self.api_key.as_deref(),
            self.provider_name(),
            &self.model,
            audio_bytes,
            filename,
            language,
        )
        .await
        .map_err(|e| {
            if is_connection_error(&e) {
                VoiceError::Stt(format!(
                    "Local HTTP Whisper server is unreachable at {}. Start whisper-server or switch to 'Offline: whisper.cpp Local' / 'Offline: Windows Speech'.",
                    self.base_url
                ))
            } else {
                e
            }
        })
    }
}

fn is_connection_error(err: &VoiceError) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("connect") || s.contains("connection refused") || s.contains("error sending request") || s.contains("timeout")
}

/// Offline STT backend invoking local `whisper-cli.exe` / `whisper.exe` subprocess.
#[derive(Debug, Clone)]
pub struct OfflineWhisperCliStt {
    executable_path: PathBuf,
    model_path: Option<PathBuf>,
    timeout: Duration,
}

impl OfflineWhisperCliStt {
    pub fn new(executable_path: impl Into<PathBuf>, model_path: Option<PathBuf>) -> Self {
        Self {
            executable_path: executable_path.into(),
            model_path,
            timeout: Duration::from_secs(60),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Default for OfflineWhisperCliStt {
    fn default() -> Self {
        #[cfg(windows)]
        let default_exe = PathBuf::from("whisper-cli.exe");
        #[cfg(not(windows))]
        let default_exe = PathBuf::from("whisper-cli");
        Self::new(default_exe, None)
    }
}

struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[async_trait]
impl SttBackend for OfflineWhisperCliStt {
    fn provider_name(&self) -> &'static str {
        "offline_whisper_cli"
    }

    async fn transcribe_wav(
        &self,
        wav_bytes: &[u8],
        language: Option<&str>,
    ) -> VoiceResult<SttResult> {
        if wav_bytes.is_empty() {
            return Err(VoiceError::Stt("Cannot transcribe empty WAV payload".into()));
        }

        let temp_wav = std::env::temp_dir().join(format!("jarvis_stt_{}.wav", Uuid::new_v4()));
        tokio::fs::write(&temp_wav, wav_bytes)
            .await
            .map_err(|e| VoiceError::Stt(format!("Failed writing temp WAV file: {e}")))?;
        let _guard = TempFileGuard(temp_wav.clone());

        let mut cmd = Command::new(&self.executable_path);
        if let Some(ref model) = self.model_path {
            cmd.arg("-m").arg(model);
        }
        cmd.arg("-f").arg(&temp_wav).arg("-nt").arg("-np");

        if let Some(lang) = normalize_explicit_en_vi_language(language) {
            cmd.arg("-l").arg(lang);
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(windows)]
        {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let output = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| VoiceError::Stt("whisper-cli transcription timed out".into()))?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    VoiceError::Stt(
                        "Offline whisper-cli is not installed. Open the Voice tab and click 'Install Offline Whisper (~145 MB)' or select 'Offline: Windows Speech'.".into()
                    )
                } else {
                    VoiceError::Stt(format!(
                        "Failed to launch '{}': {e}",
                        self.executable_path.display()
                    ))
                }
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("model") || stderr.contains("load") || stderr.contains("ggml") || stderr.contains("No such file") {
                return Err(VoiceError::Stt(
                    "Missing GGML model for whisper-cli. Open the Voice tab and click 'Install Offline Whisper (~145 MB)'.".into()
                ));
            }
            return Err(VoiceError::Stt(format!(
                "whisper-cli exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }

        let mut text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !is_latin_or_vietnamese_text(&text) {
            text.clear();
        }
        let model_label = self
            .model_path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or("whisper-cli")
            .to_string();

        Ok(SttResult {
            text,
            language: language.map(str::to_string),
            duration_ms: None,
            provider: self.provider_name().to_string(),
            model: model_label,
        })
    }
}

/// Build an [`SttBackend`] from [`VoiceConfig`] and [`SecretStore`].
pub fn create_stt_backend(
    config: &VoiceConfig,
    secrets: &dyn SecretStore,
) -> VoiceResult<Arc<dyn SttBackend>> {
    let custom_model = config
        .stt_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match config.stt_backend.trim().to_ascii_lowercase().as_str() {
        "" | "groq" | "groq_whisper" => {
            let key = secrets
                .get(keys::GROQ_API_KEY)
                .map_err(|e| VoiceError::Config(e.to_string()))?
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| {
                    VoiceError::Config("groq_api_key is required in SecretStore for Groq Whisper STT".into())
                })?;
            let mut backend = GroqWhisperStt::new(key);
            if let Some(m) = custom_model {
                backend = backend.with_model(m);
            }
            Ok(Arc::new(backend))
        }
        "openai" | "openai_whisper" => {
            let key = secrets
                .get(keys::OPENAI_API_KEY)
                .map_err(|e| VoiceError::Config(e.to_string()))?
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| {
                    VoiceError::Config("openai_api_key is required in SecretStore for OpenAI Whisper STT".into())
                })?;
            let mut backend = OpenAiWhisperStt::new(key);
            if let Some(m) = custom_model {
                backend = backend.with_model(m);
            }
            Ok(Arc::new(backend))
        }
        "local" | "local_http" | "local_http_whisper" => {
            let base_url = if config.local_stt_url.trim().is_empty() {
                DEFAULT_LOCAL_WHISPER_BASE_URL
            } else {
                config.local_stt_url.trim()
            };
            let mut backend = LocalHttpWhisperStt::new(base_url);
            if let Some(m) = custom_model {
                backend = backend.with_model(m);
            }
            if let Ok(Some(key)) = secrets.get(keys::OPENAI_API_KEY) {
                if !key.trim().is_empty() {
                    backend = backend.with_api_key(key);
                }
            }
            Ok(Arc::new(backend))
        }
        "offline" | "whisper_cli" | "offline_whisper_cli" => {
            let exe = config
                .whisper_cli_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| OfflineWhisperCliStt::default().executable_path);
            let model = config
                .whisper_model_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
            Ok(Arc::new(OfflineWhisperCliStt::new(exe, model)))
        }
        other => Err(VoiceError::Config(format!(
            "Unsupported STT backend: '{other}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn en_vi_language_and_script_guard_rejects_foreign_languages_and_scripts() {
        // Test is_allowed_en_vi_language
        assert!(is_allowed_en_vi_language("vi"));
        assert!(is_allowed_en_vi_language("VIE"));
        assert!(is_allowed_en_vi_language("vi-VN"));
        assert!(is_allowed_en_vi_language("Vietnamese"));
        assert!(is_allowed_en_vi_language("en"));
        assert!(is_allowed_en_vi_language("ENG"));
        assert!(is_allowed_en_vi_language("en-US"));
        assert!(is_allowed_en_vi_language("en-GB"));
        assert!(is_allowed_en_vi_language("English"));

        assert!(!is_allowed_en_vi_language("ko"));
        assert!(!is_allowed_en_vi_language("ja"));
        assert!(!is_allowed_en_vi_language("zh"));
        assert!(!is_allowed_en_vi_language("ru"));
        assert!(!is_allowed_en_vi_language("th"));
        assert!(!is_allowed_en_vi_language("fr"));
        assert!(!is_allowed_en_vi_language("es"));
        assert!(!is_allowed_en_vi_language("vi_en"));

        // Test is_latin_or_vietnamese_text
        assert!(is_latin_or_vietnamese_text("Xin chào Jarvis, mở nhạc. Hello world!"));
        assert!(is_latin_or_vietnamese_text("Đường đi nước bước rất rõ ràng 123!?"));
        assert!(!is_latin_or_vietnamese_text("안녕하세요 Jarvis"));
        assert!(!is_latin_or_vietnamese_text("こんにちは Jarvis"));
        assert!(!is_latin_or_vietnamese_text("你好 Jarvis"));
        assert!(!is_latin_or_vietnamese_text("Привет Jarvis"));
        assert!(!is_latin_or_vietnamese_text("สวัสดี Jarvis"));

        // Test normalize_explicit_en_vi_language
        assert_eq!(normalize_explicit_en_vi_language(Some("vi")), Some("vi"));
        assert_eq!(normalize_explicit_en_vi_language(Some("Vietnamese")), Some("vi"));
        assert_eq!(normalize_explicit_en_vi_language(Some("en")), Some("en"));
        assert_eq!(normalize_explicit_en_vi_language(Some("English")), Some("en"));
        assert_eq!(normalize_explicit_en_vi_language(Some("vi_en")), None);
        assert_eq!(normalize_explicit_en_vi_language(Some("ko")), None);
        assert_eq!(normalize_explicit_en_vi_language(None), None);
    }
}
