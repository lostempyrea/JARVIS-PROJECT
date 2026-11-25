//! Hands-Free Voice Pipeline (Phase 2):
//! - [`audio_io`]: `cpal` 16kHz mono microphone capture & device discovery
//! - [`vad`]: `VadProcessor` with 1,200ms pre-roll ring buffer, `PttController`, WAV encoding
//! - [`wake_word`]: Configurable Unicode/punctuation-tolerant wake phrase matcher & command splitter
//! - [`stt`][]: `SttBackend` (Groq Whisper, OpenAI Whisper, Local HTTP Whisper, Offline `whisper-cli`)
//! - [`tts`]: `TtsBackend` (`EdgeTtsBackend` DRM v2, `OpenAiTtsBackend`, `OfflineWindowsSapiTts`) & `AudioPlayer`

pub mod audio_io;
pub mod stt;
pub mod tts;
pub mod vad;
pub mod wake_word;

pub use audio_io::{
    compute_rms, compute_rms_db, list_input_devices, list_output_devices, resample_linear_mono,
    rms_to_db, AudioDeviceInfo, MicCapture, MicLevelCallback, TARGET_CHANNELS, TARGET_SAMPLE_RATE,
};
pub use stt::{
    create_stt_backend, GroqWhisperStt, LocalHttpWhisperStt, OfflineWhisperCliStt,
    OpenAiWhisperStt, SttBackend, SttResult,
};
pub use tts::{
    create_tts_backend, AudioPlayer, EdgeTtsBackend, EdgeVoiceInfo, OfflineWindowsSapiTts,
    OpenAiTtsBackend, TtsBackend, DEFAULT_VIETNAMESE_FEMALE_VOICE, DEFAULT_VIETNAMESE_MALE_VOICE,
};
pub use vad::{
    encode_wav_i16_mono, PttController, VadConfig, VadEvent, VadProcessor, VadState,
    DEFAULT_PRE_ROLL_MS,
};
pub use wake_word::{WakeMatch, WakeWordMatcher};

use crate::agent::AgentRuntime;
use crate::config::{AppConfig, VoiceConfig};
use crate::events::{CoreEvent, EventBus, ReactorState};
use crate::secrets::SecretStore;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Errors produced by the voice pipeline and speech backends.
#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("audio device error: {0}")]
    AudioDevice(String),
    #[error("audio stream error: {0}")]
    AudioStream(String),
    #[error("audio encoding error: {0}")]
    Encoding(String),
    #[error("STT error: {0}")]
    Stt(String),
    #[error("TTS error: {0}")]
    Tts(String),
    #[error("audio playback error: {0}")]
    Playback(String),
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    #[error("voice configuration error: {0}")]
    Config(String),
    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("agent runtime error: {0}")]
    Agent(String),
}

pub type VoiceResult<T> = Result<T, VoiceError>;

/// Routing decision for a transcribed utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptAction {
    /// Utterance did not contain wake phrase and no active Listening window is open.
    Ignore,
    /// Wake phrase alone detected; enter or extend active `Listening` window.
    EnterListening,
    /// Dispatch command text to the agent runtime immediately.
    DispatchCommand(String),
}

/// Pure, deterministic transcript router combining `WakeWordMatcher` and the active `Listening` window.
pub fn route_transcript(
    transcript: &str,
    wake_word_enabled: bool,
    matcher: &WakeWordMatcher,
    listening_until: &mut Option<Instant>,
    listening_timeout: Duration,
    now: Instant,
) -> TranscriptAction {
    let trimmed = transcript.trim();
    if trimmed.is_empty() {
        return TranscriptAction::Ignore;
    }

    let in_listening_window = listening_until.is_some_and(|deadline| now < deadline);
    if !in_listening_window {
        *listening_until = None;
    }

    if !wake_word_enabled {
        *listening_until = None;
        return match matcher.match_transcript(trimmed) {
            WakeMatch::WakeWithCommand(cmd) => TranscriptAction::DispatchCommand(cmd),
            WakeMatch::WakeOnly => TranscriptAction::EnterListening,
            WakeMatch::NoMatch => TranscriptAction::DispatchCommand(trimmed.to_string()),
        };
    }

    match matcher.match_transcript(trimmed) {
        WakeMatch::WakeOnly => {
            *listening_until = Some(now + listening_timeout);
            TranscriptAction::EnterListening
        }
        WakeMatch::WakeWithCommand(cmd) => {
            *listening_until = None;
            TranscriptAction::DispatchCommand(cmd)
        }
        WakeMatch::NoMatch => {
            if in_listening_window {
                *listening_until = None;
                TranscriptAction::DispatchCommand(trimmed.to_string())
            } else {
                TranscriptAction::Ignore
            }
        }
    }
}

const TTS_ECHO_TAIL_MS: u64 = 350;

pub(crate) fn evaluate_tts_mute_gate(
    is_playing: bool,
    tts_mute_until: &mut Option<Instant>,
    now: Instant,
    echo_tail: Duration,
) -> bool {
    if is_playing {
        *tts_mute_until = Some(now + echo_tail);
        return true;
    }
    match *tts_mute_until {
        Some(until) if now < until => true,
        Some(_) => {
            *tts_mute_until = None;
            false
        }
        None => false,
    }
}

struct PipelineHandle {
    shutdown: Arc<AtomicBool>,
    mic: MicCapture,
    task: JoinHandle<()>,
}

struct PttMicHandle {
    mic: MicCapture,
    forwarder: std::thread::JoinHandle<()>,
}

/// Orchestrator for the hands-free voice pipeline:
/// `MicCapture` -> `VadProcessor` (1.2s pre-roll) -> `SttBackend` -> `WakeWordMatcher` / active `Listening` window -> `AgentRuntime` -> `TtsBackend` -> `AudioPlayer`.
#[derive(Clone)]
pub struct VoicePipeline {
    config: Arc<RwLock<AppConfig>>,
    secrets: Arc<dyn SecretStore>,
    event_bus: EventBus,
    agent: AgentRuntime,
    player: Arc<OnceLock<Arc<AudioPlayer>>>,
    ptt: Arc<Mutex<PttController>>,
    ptt_mic: Arc<Mutex<Option<PttMicHandle>>>,
    listening_until: Arc<Mutex<Option<Instant>>>,
    tts_mute_until: Arc<Mutex<Option<Instant>>>,
    active_session_id: Arc<Mutex<Option<String>>>,
    running_handle: Arc<Mutex<Option<PipelineHandle>>>,
}

impl VoicePipeline {
    /// Construct a new [`VoicePipeline`] wired to core configuration, secrets, event bus, and agent runtime.
    pub fn new(
        config: Arc<RwLock<AppConfig>>,
        secrets: Arc<dyn SecretStore>,
        event_bus: EventBus,
        agent: AgentRuntime,
    ) -> Self {
        Self {
            config,
            secrets,
            event_bus,
            agent,
            player: Arc::new(OnceLock::new()),
            ptt: Arc::new(Mutex::new(PttController::new(TARGET_SAMPLE_RATE))),
            ptt_mic: Arc::new(Mutex::new(None)),
            listening_until: Arc::new(Mutex::new(None)),
            tts_mute_until: Arc::new(Mutex::new(None)),
            active_session_id: Arc::new(Mutex::new(None)),
            running_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Access (and lazily initialize) the thread-safe [`AudioPlayer`].
    pub fn player(&self) -> Arc<AudioPlayer> {
        self.player
            .get_or_init(|| {
                let output_device = self
                    .config
                    .read()
                    .ok()
                    .and_then(|c| c.voice.output_device.clone());
                Arc::new(AudioPlayer::new_with_device(output_device))
            })
            .clone()
    }

    /// List available audio input devices.
    pub fn list_input_devices() -> VoiceResult<Vec<AudioDeviceInfo>> {
        list_input_devices()
    }

    /// List available audio output devices.
    pub fn list_output_devices() -> VoiceResult<Vec<AudioDeviceInfo>> {
        list_output_devices()
    }

    /// Returns `true` if the hands-free capture loop is currently active.
    pub fn is_running(&self) -> bool {
        self.running_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|h| h.mic.is_running() && !h.shutdown.load(Ordering::Relaxed))
    }

    /// Start the hands-free voice capture and processing pipeline for `session_id`.
    pub fn start(&self, session_id: impl Into<String>) -> VoiceResult<()> {
        self.stop();

        let voice_cfg = self
            .config
            .read()
            .map_err(|_| VoiceError::Config("config lock poisoned".into()))?
            .voice
            .clone();

        if let Some(p) = self.player.get() {
            p.set_output_device(voice_cfg.output_device.clone());
        }

        let stt = create_stt_backend(&voice_cfg, self.secrets.as_ref())?;
        let tts = create_tts_backend(&voice_cfg, self.secrets.as_ref()).ok();
        let vad_cfg = VadConfig::from(&voice_cfg);
        let matcher = WakeWordMatcher::from(&voice_cfg);
        let listening_timeout = Duration::from_millis(voice_cfg.listening_timeout_ms.max(500));

        let (sample_tx, mut sample_rx) = mpsc::unbounded_channel::<Vec<f32>>();
        let bus_level = self.event_bus.clone();
        let level_cb: MicLevelCallback = Arc::new(move |db| {
            bus_level.publish(CoreEvent::MicLevel { db });
        });

        let mic = MicCapture::start(
            voice_cfg.input_device.as_deref(),
            Some(level_cb),
            Some(sample_tx),
        )?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_task = Arc::clone(&shutdown);
        let session_id = session_id.into();
        *self
            .active_session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(session_id.clone());
        let config_lock = Arc::clone(&self.config);
        let event_bus = self.event_bus.clone();
        let agent = self.agent.clone();
        let ptt = Arc::clone(&self.ptt);
        let listening_until = Arc::clone(&self.listening_until);
        let tts_mute_until = Arc::clone(&self.tts_mute_until);
        let player = self.player();

        let task = tokio::spawn(async move {
            let mut vad = VadProcessor::new(vad_cfg);
            let mut timeout_tick = tokio::time::interval(Duration::from_millis(100));

            loop {
                if shutdown_task.load(Ordering::Relaxed) {
                    break;
                }

                tokio::select! {
                    maybe_frame = sample_rx.recv() => {
                        let Some(frame) = maybe_frame else {
                            break;
                        };

                        let ptt_active = {
                            let mut guard = ptt.lock().unwrap_or_else(|e| e.into_inner());
                            if guard.is_active() {
                                guard.push_samples(&frame);
                                true
                            } else {
                                false
                            }
                        };
                        if ptt_active {
                            continue;
                        }

                        // Mute VAD while JARVIS TTS is speaking + 350ms echo tail so speaker output
                        // cannot self-trigger SpeechStarted or interrupt itself.
                        let muted_by_tts = {
                            let mut mute_guard = tts_mute_until.lock().unwrap_or_else(|e| e.into_inner());
                            evaluate_tts_mute_gate(
                                player.is_playing(),
                                &mut mute_guard,
                                Instant::now(),
                                Duration::from_millis(TTS_ECHO_TAIL_MS),
                            )
                        };
                        if muted_by_tts {
                            vad.reset();
                            continue;
                        }

                        match vad.process_frame(&frame) {
                            Ok(VadEvent::None) => {}
                            Ok(VadEvent::SpeechStarted) => {
                                if player.is_playing() {
                                    player.stop_playback();
                                }
                                let in_window = listening_until
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .is_some_and(|d| Instant::now() < d);
                                if !voice_cfg.wake_word_enabled || in_window {
                                    event_bus.publish(CoreEvent::ReactorStateChanged {
                                        state: ReactorState::Listening,
                                    });
                                }
                            }
                            Ok(VadEvent::SpeechEnded(wav_bytes)) => {
                                let current_voice_cfg = config_lock
                                    .read()
                                    .map(|c| c.voice.clone())
                                    .unwrap_or_else(|_| voice_cfg.clone());
                                Self::process_utterance_internal(
                                    &session_id,
                                    &wav_bytes,
                                    &current_voice_cfg,
                                    &matcher,
                                    &stt,
                                    tts.as_ref(),
                                    &player,
                                    &listening_until,
                                    listening_timeout,
                                    &event_bus,
                                    &agent,
                                )
                                .await;
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, "VAD processing error");
                            }
                        }
                    }
                    _ = timeout_tick.tick() => {
                        let expired = {
                            let mut guard = listening_until.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(deadline) = *guard {
                                if Instant::now() >= deadline && vad.state() == VadState::Idle {
                                    *guard = None;
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        };
                        if expired {
                            event_bus.publish(CoreEvent::ReactorStateChanged {
                                state: ReactorState::Idle,
                            });
                        }
                    }
                }
            }
        });

        let mut handle_guard = self
            .running_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *handle_guard = Some(PipelineHandle {
            shutdown,
            mic,
            task,
        });

        Ok(())
    }

    /// Stop the hands-free voice capture loop and halt any active playback.
    pub fn stop(&self) {
        let taken = self
            .running_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        if let Some(mut handle) = taken {
            handle.shutdown.store(true, Ordering::Relaxed);
            handle.mic.stop();
            handle.task.abort();
        }

        *self
            .listening_until
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;

        if let Some(p) = self.player.get() {
            p.stop_playback();
        }

        self.event_bus.publish(CoreEvent::ReactorStateChanged {
            state: ReactorState::Idle,
        });
    }

    /// Press Push-to-Talk to begin recording manual speech.
    ///
    /// If the hands-free capture loop is stopped, starts a temporary [`MicCapture`]
    /// feeding the PTT buffer until [`Self::ptt_release_wav`] stops and drops it.
    pub fn ptt_press(&self) {
        if let Some(p) = self.player.get() {
            p.stop_playback();
        }
        {
            let mut guard = self.ptt.lock().unwrap_or_else(|e| e.into_inner());
            guard.press();
        }

        if !self.is_running() {
            let mut ptt_mic_guard = self.ptt_mic.lock().unwrap_or_else(|e| e.into_inner());
            if ptt_mic_guard.is_none() {
                let input_device = self
                    .config
                    .read()
                    .ok()
                    .and_then(|c| c.voice.input_device.clone());
                let (sample_tx, mut sample_rx) = mpsc::unbounded_channel::<Vec<f32>>();
                let bus_level = self.event_bus.clone();
                let level_cb: MicLevelCallback = Arc::new(move |db| {
                    bus_level.publish(CoreEvent::MicLevel { db });
                });

                match MicCapture::start(input_device.as_deref(), Some(level_cb), Some(sample_tx)) {
                    Ok(mic) => {
                        let ptt = Arc::clone(&self.ptt);
                        match std::thread::Builder::new()
                            .name("jarvis-ptt-forward".into())
                            .spawn(move || {
                                while let Some(frame) = sample_rx.blocking_recv() {
                                    let mut guard = ptt.lock().unwrap_or_else(|e| e.into_inner());
                                    if !guard.is_active() {
                                        break;
                                    }
                                    guard.push_samples(&frame);
                                }
                            }) {
                            Ok(forwarder) => {
                                *ptt_mic_guard = Some(PttMicHandle { mic, forwarder });
                            }
                            Err(err) => {
                                let mut mic = mic;
                                mic.stop();
                                let _ = self.ptt.lock().unwrap_or_else(|e| e.into_inner()).release();
                                self.event_bus.publish(CoreEvent::Error {
                                    session_id: None,
                                    message: format!("Push-to-Talk thread spawn failed: {err}"),
                                });
                                self.event_bus.publish(CoreEvent::ReactorStateChanged {
                                    state: ReactorState::Idle,
                                });
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        let _ = self.ptt.lock().unwrap_or_else(|e| e.into_inner()).release();
                        self.event_bus.publish(CoreEvent::Error {
                            session_id: None,
                            message: format!("Microphone unavailable for Push-to-Talk: {err}"),
                        });
                        self.event_bus.publish(CoreEvent::ReactorStateChanged {
                            state: ReactorState::Idle,
                        });
                        return;
                    }
                }
            }
        }

        self.event_bus.publish(CoreEvent::ReactorStateChanged {
            state: ReactorState::Listening,
        });
    }

    /// Release Push-to-Talk, stopping any temporary capture and returning encoded WAV bytes if speech was captured.
    pub fn ptt_release_wav(&self) -> VoiceResult<Option<Vec<u8>>> {
        let ptt_handle = self.ptt_mic.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut handle) = ptt_handle {
            handle.mic.stop();
            let _ = handle.forwarder.join();
        }

        let res = self
            .ptt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release();
        match res {
            Ok(Some(wav)) => Ok(Some(wav)),
            Ok(None) => {
                self.event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Idle,
                });
                Ok(None)
            }
            Err(err) => {
                self.event_bus.publish(CoreEvent::Error {
                    session_id: None,
                    message: format!("Push-to-Talk audio encoding failed: {err}"),
                });
                self.event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Idle,
                });
                Err(err)
            }
        }
    }

    /// Transcribe a Push-to-Talk WAV buffer and dispatch the resulting command to [`AgentRuntime`].
    ///
    /// Emits `CoreEvent::Error` on STT or agent failures and always resets `ReactorState::Idle`
    /// when no command is dispatched or after execution completes.
    pub async fn process_ptt_wav(&self, session_id: &str, wav: &[u8]) -> VoiceResult<()> {
        let voice_cfg = self
            .config
            .read()
            .map_err(|_| VoiceError::Config("config lock poisoned".into()))?
            .voice
            .clone();

        let stt = match create_stt_backend(&voice_cfg, self.secrets.as_ref()) {
            Ok(backend) => backend,
            Err(err) => {
                self.event_bus.publish(CoreEvent::Error {
                    session_id: Some(session_id.to_string()),
                    message: format!("Voice STT config error: {err}"),
                });
                self.event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Idle,
                });
                return Err(err);
            }
        };

        let stt_res = match stt
            .transcribe_wav(wav, voice_cfg.stt_language.as_deref())
            .await
        {
            Ok(res) => res,
            Err(err) => {
                self.event_bus.publish(CoreEvent::Error {
                    session_id: Some(session_id.to_string()),
                    message: format!("Voice STT failed: {err}"),
                });
                self.event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Idle,
                });
                return Err(err);
            }
        };

        let txt = stt_res.text.trim().to_string();
        if txt.is_empty() {
            self.event_bus.publish(CoreEvent::ReactorStateChanged {
                state: ReactorState::Idle,
            });
            return Ok(());
        }

        self.event_bus.publish(CoreEvent::VoiceTranscript {
            session_id: session_id.to_string(),
            text: txt.clone(),
            is_command: true,
        });
        self.event_bus.publish(CoreEvent::ReactorStateChanged {
            state: ReactorState::Executing,
        });

        match self
            .agent
            .handle_user_prompt(session_id, "voice_ptt", &txt)
            .await
        {
            Ok(_) => {
                self.event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Idle,
                });
                Ok(())
            }
            Err(err) => {
                let msg = format!("Voice agent execution failed: {err}");
                self.event_bus.publish(CoreEvent::Error {
                    session_id: Some(session_id.to_string()),
                    message: msg.clone(),
                });
                self.event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Idle,
                });
                Err(VoiceError::Agent(msg))
            }
        }
    }

    /// Apply updated [`VoiceConfig`] immediately: updates [`AudioPlayer`] output device
    /// and restarts the hands-free pipeline if it is currently running.
    pub fn apply_updated_config(&self) -> VoiceResult<bool> {
        let output_device = self
            .config
            .read()
            .map_err(|_| VoiceError::Config("config lock poisoned".into()))?
            .voice
            .output_device
            .clone();

        if let Some(p) = self.player.get() {
            p.set_output_device(output_device);
        }

        if self.is_running() {
            let sid = self
                .active_session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| "hud-main".to_string());
            self.start(sid)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Synthesize text using the configured [`TtsBackend`] and enqueue playback in [`AudioPlayer`].
    pub async fn speak(&self, text: &str) -> VoiceResult<()> {
        let voice_cfg = self
            .config
            .read()
            .map_err(|_| VoiceError::Config("config lock poisoned".into()))?
            .voice
            .clone();

        let tts = create_tts_backend(&voice_cfg, self.secrets.as_ref())?;
        let audio_bytes = tts
            .synthesize(
                text,
                &voice_cfg.tts_voice,
                voice_cfg.tts_rate,
                voice_cfg.tts_pitch,
            )
            .await?;

        self.player().enqueue_audio_bytes(audio_bytes)
    }

    /// Immediately halt active TTS playback (barge-in).
    pub fn stop_speaking(&self) {
        if let Some(p) = self.player.get() {
            p.stop_playback();
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_utterance_internal(
        session_id: &str,
        wav_bytes: &[u8],
        voice_cfg: &VoiceConfig,
        matcher: &WakeWordMatcher,
        stt: &Arc<dyn SttBackend>,
        tts: Option<&Arc<dyn TtsBackend>>,
        player: &Arc<AudioPlayer>,
        listening_until: &Arc<Mutex<Option<Instant>>>,
        listening_timeout: Duration,
        event_bus: &EventBus,
        agent: &AgentRuntime,
    ) {
        let stt_res = match stt
            .transcribe_wav(wav_bytes, voice_cfg.stt_language.as_deref())
            .await
        {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, "Voice STT transcription failed");
                event_bus.publish(CoreEvent::Error {
                    session_id: Some(session_id.to_string()),
                    message: format!("Voice STT failed: {err}"),
                });
                event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Idle,
                });
                return;
            }
        };

        let action = {
            let mut guard = listening_until.lock().unwrap_or_else(|e| e.into_inner());
            route_transcript(
                &stt_res.text,
                voice_cfg.wake_word_enabled,
                matcher,
                &mut guard,
                listening_timeout,
                Instant::now(),
            )
        };

        match action {
            TranscriptAction::Ignore => {}
            TranscriptAction::EnterListening => {
                event_bus.publish(CoreEvent::VoiceTranscript {
                    session_id: session_id.to_string(),
                    text: stt_res.text,
                    is_command: false,
                });
                event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Listening,
                });
            }
            TranscriptAction::DispatchCommand(command) => {
                event_bus.publish(CoreEvent::VoiceTranscript {
                    session_id: session_id.to_string(),
                    text: command.clone(),
                    is_command: true,
                });
                event_bus.publish(CoreEvent::ReactorStateChanged {
                    state: ReactorState::Executing,
                });

                match agent.handle_user_prompt(session_id, "voice", &command).await {
                    Ok(outcome) => {
                        event_bus.publish(CoreEvent::ReactorStateChanged {
                            state: ReactorState::Idle,
                        });
                        if !outcome.final_text.trim().is_empty() {
                            if let Some(tts_backend) = tts {
                                match tts_backend
                                    .synthesize(
                                        &outcome.final_text,
                                        &voice_cfg.tts_voice,
                                        voice_cfg.tts_rate,
                                        voice_cfg.tts_pitch,
                                    )
                                    .await
                                {
                                    Ok(audio) => {
                                        if let Err(err) = player.enqueue_audio_bytes(audio) {
                                            tracing::warn!(error = %err, "Failed to enqueue TTS audio");
                                        }
                                    }
                                    Err(err) => {
                                        tracing::warn!(error = %err, "Voice TTS synthesis failed");
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => {
                        event_bus.publish(CoreEvent::Error {
                            session_id: Some(session_id.to_string()),
                            message: format!("Voice agent execution failed: {err}"),
                        });
                        event_bus.publish(CoreEvent::ReactorStateChanged {
                            state: ReactorState::Idle,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::InMemorySecretStore;
    use hound::{SampleFormat, WavReader};
    use std::io::Cursor;

    fn sine_wave(freq_hz: f32, amp: f32, sample_rate: u32, num_samples: usize) -> Vec<f32> {
        (0..num_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                amp * (2.0 * std::f32::consts::PI * freq_hz * t).sin()
            })
            .collect()
    }

    #[test]
    fn encode_wav_i16_mono_round_trip() {
        let samples = sine_wave(440.0, 0.5, TARGET_SAMPLE_RATE, 1600);
        let wav_bytes = encode_wav_i16_mono(&samples, TARGET_SAMPLE_RATE).unwrap();

        let mut reader = WavReader::new(Cursor::new(&wav_bytes)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, SampleFormat::Int);

        let decoded: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / i16::MAX as f32)
            .collect();
        assert_eq!(decoded.len(), samples.len());
        for (orig, dec) in samples.iter().zip(decoded.iter()) {
            assert!(
                (orig - dec).abs() < 1e-4,
                "round-trip sample mismatch: orig={orig}, dec={dec}"
            );
        }

        // Empty buffer must return an error.
        assert!(encode_wav_i16_mono(&[], TARGET_SAMPLE_RATE).is_err());
    }

    #[test]
    fn vad_pre_roll_1200ms_ring_buffer_retains_initial_frames_without_clipping() {
        let cfg = VadConfig::default();
        assert_eq!(cfg.pre_roll_ms, 1200);

        let mut vad = VadProcessor::new(cfg);
        assert_eq!(vad.max_pre_roll_samples(), 19_200); // 1.2s * 16,000 Hz

        // Feed 200ms of older quiet noise (amplitude 0.0005, ~-66 dBFS, well below -40 dB)
        // that should be evicted once the 1,200ms ring buffer fills up.
        let old_frame = vec![0.0005_f32; 3_200];
        assert_eq!(vad.process_frame(&old_frame).unwrap(), VadEvent::None);

        // Feed exactly 1,200ms (19,200 samples = 6 frames of 200ms) of distinct quiet pre-roll
        // frames (amplitude 0.002, ~-54 dBFS, still below -40 dB threshold) representing soft onset.
        let retained_pre_roll_sample = 0.002_f32;
        let pre_roll_frame = vec![retained_pre_roll_sample; 3_200];
        for _ in 0..6 {
            assert_eq!(vad.process_frame(&pre_roll_frame).unwrap(), VadEvent::None);
        }
        assert_eq!(vad.pre_roll_len(), 19_200);

        // Feed 300ms (4,800 samples) of loud speech (~-6 dBFS).
        let speech = sine_wave(300.0, 0.5, TARGET_SAMPLE_RATE, 4_800);
        assert_eq!(vad.process_frame(&speech).unwrap(), VadEvent::SpeechStarted);

        // Feed 800ms (12,800 samples) of trailing silence to close the utterance.
        let silence = vec![0.0_f32; 12_800];
        let event = vad.process_frame(&silence).unwrap();
        let wav_bytes = match event {
            VadEvent::SpeechEnded(bytes) => bytes,
            other => panic!("expected SpeechEnded, got {other:?}"),
        };

        let mut reader = WavReader::new(Cursor::new(&wav_bytes)).unwrap();
        let decoded: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();

        // Total = 19,200 (1,200ms pre-roll) + 4,800 (speech) + 12,800 (silence) = 36,800 samples.
        assert_eq!(decoded.len(), 19_200 + 4_800 + 12_800);

        // Verify the initial 19,200 samples in the WAV are from the retained 1,200ms pre-roll
        // (0.002 * 32767 ≈ 66), NOT the evicted older frame (0.0005 * 32767 ≈ 16).
        let expected_i16 = (retained_pre_roll_sample * i16::MAX as f32).round() as i16;
        for (idx, &sample) in decoded[..19_200].iter().enumerate() {
            assert_eq!(
                sample, expected_i16,
                "pre-roll sample mismatch at index {idx}"
            );
        }
    }

    #[test]
    fn wake_word_matcher_handles_wake_only_inline_commands_and_non_matches() {
        let matcher = WakeWordMatcher::default();

        // Wake phrase alone
        assert_eq!(matcher.match_transcript("hey jarvis"), WakeMatch::WakeOnly);
        assert_eq!(matcher.match_transcript("jarvis"), WakeMatch::WakeOnly);
        assert_eq!(matcher.match_transcript("Hey Jarvis!"), WakeMatch::WakeOnly);
        assert_eq!(matcher.match_transcript("  Jarvis...  "), WakeMatch::WakeOnly);

        // Wake phrase + inline command (including Vietnamese Unicode & punctuation)
        assert_eq!(
            matcher.match_transcript("jarvis, đọc file Cargo.toml"),
            WakeMatch::WakeWithCommand("đọc file Cargo.toml".to_string())
        );
        assert_eq!(
            matcher.match_transcript("Hey Jarvis hãy mở nhạc"),
            WakeMatch::WakeWithCommand("hãy mở nhạc".to_string())
        );
        assert_eq!(
            matcher.match_transcript("jarvis, hãy làm cái này"),
            WakeMatch::WakeWithCommand("hãy làm cái này".to_string())
        );
        assert_eq!(
            matcher.match_transcript("Hey Jarvis: kiểm tra git"),
            WakeMatch::WakeWithCommand("kiểm tra git".to_string())
        );
        assert_eq!(
            matcher.match_transcript("Hey, Jarvis, kiểm tra git"),
            WakeMatch::WakeWithCommand("kiểm tra git".to_string())
        );

        // Non-matching phrases
        assert_eq!(matcher.match_transcript("xin chào bạn"), WakeMatch::NoMatch);
        assert_eq!(matcher.match_transcript("jarvison status"), WakeMatch::NoMatch);
        assert_eq!(
            matcher.match_transcript("tôi đang nói về jarvis"),
            WakeMatch::NoMatch
        );
        assert_eq!(matcher.match_transcript("   "), WakeMatch::NoMatch);
    }

    #[test]
    fn edge_tts_sec_ms_gec_token_is_deterministic_and_uppercase_hex() {
        let ts_a = 1_700_000_100_u64;
        let ts_b = 1_700_000_250_u64; // Same 300s (5-minute) bucket

        let token_a = EdgeTtsBackend::generate_sec_ms_gec(ts_a);
        let token_b = EdgeTtsBackend::generate_sec_ms_gec(ts_b);
        assert_eq!(token_a, token_b);
        assert_eq!(token_a.len(), 81); // 16 hex CS64 + '-' + 64 hex SHA-256

        let parts: Vec<&str> = token_a.split('-').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 16);
        assert_eq!(parts[1].len(), 64);
        assert!(parts[0]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
        assert!(parts[1]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));

        // Crossing 300s boundary produces a new token
        let token_next = EdgeTtsBackend::generate_sec_ms_gec(ts_a + 300);
        assert_ne!(token_a, token_next);

        // Golden DRM v2 reference vectors
        assert_eq!(
            EdgeTtsBackend::generate_sec_ms_gec(1_700_000_000),
            "2CD23A2CA0B2595A-F3C846C370E05F9CC83ACC5F995C5F5D87A4D0F14CE8936A0F902E64099E38A2"
        );
        assert_eq!(
            EdgeTtsBackend::generate_sec_ms_gec(1_747_474_747),
            "3572F8223FCC2F27-E80CC63D7D435A2F68B5E839AC9E7EDE427BB8DCA3326E27BF0156DD6449995F"
        );
    }

    #[test]
    fn route_transcript_manages_listening_window_and_inline_commands() {
        let matcher = WakeWordMatcher::default();
        let timeout = Duration::from_secs(8);
        let t0 = Instant::now();
        let mut listening_until = None;

        // Ambient speech without wake word is ignored
        assert_eq!(
            route_transcript("trời hôm nay đẹp quá", true, &matcher, &mut listening_until, timeout, t0),
            TranscriptAction::Ignore
        );

        // Wake phrase alone opens Listening window
        assert_eq!(
            route_transcript("Hey Jarvis", true, &matcher, &mut listening_until, timeout, t0),
            TranscriptAction::EnterListening
        );
        assert!(listening_until.is_some());

        // Follow-up command within window dispatches immediately and closes window
        assert_eq!(
            route_transcript(
                "kiểm tra git status",
                true,
                &matcher,
                &mut listening_until,
                timeout,
                t0 + Duration::from_secs(2)
            ),
            TranscriptAction::DispatchCommand("kiểm tra git status".to_string())
        );
        assert!(listening_until.is_none());

        // Wake phrase opens window again, then expires after 8s
        assert_eq!(
            route_transcript("jarvis", true, &matcher, &mut listening_until, timeout, t0),
            TranscriptAction::EnterListening
        );
        assert_eq!(
            route_transcript(
                "mở trình duyệt",
                true,
                &matcher,
                &mut listening_until,
                timeout,
                t0 + Duration::from_secs(9)
            ),
            TranscriptAction::Ignore
        );
        assert!(listening_until.is_none());

        // Inline command dispatches directly even when not in Listening window
        assert_eq!(
            route_transcript(
                "Jarvis: đọc file Cargo.toml",
                true,
                &matcher,
                &mut listening_until,
                timeout,
                t0 + Duration::from_secs(10)
            ),
            TranscriptAction::DispatchCommand("đọc file Cargo.toml".to_string())
        );
    }

    #[test]
    fn stt_and_tts_factories_select_configured_backends() {
        let secrets = InMemorySecretStore::with_entries([
            (crate::secrets::keys::GROQ_API_KEY, "gsk-test"),
            (crate::secrets::keys::OPENAI_API_KEY, "sk-test"),
        ]);

        let mut cfg = VoiceConfig::default();
        assert_eq!(
            create_stt_backend(&cfg, &secrets).unwrap().provider_name(),
            "groq_whisper"
        );

        cfg.stt_backend = "openai_whisper".to_string();
        assert_eq!(
            create_stt_backend(&cfg, &secrets).unwrap().provider_name(),
            "openai_whisper"
        );

        cfg.stt_backend = "local_http_whisper".to_string();
        assert_eq!(
            create_stt_backend(&cfg, &secrets).unwrap().provider_name(),
            "local_http_whisper"
        );

        cfg.stt_backend = "offline_whisper_cli".to_string();
        assert_eq!(
            create_stt_backend(&cfg, &secrets).unwrap().provider_name(),
            "offline_whisper_cli"
        );

        assert_eq!(
            create_tts_backend(&cfg, &secrets).unwrap().provider_name(),
            "edge_tts"
        );

        cfg.tts_backend = "openai_tts".to_string();
        assert_eq!(
            create_tts_backend(&cfg, &secrets).unwrap().provider_name(),
            "openai_tts"
        );

        cfg.tts_backend = "windows_sapi".to_string();
        assert_eq!(
            create_tts_backend(&cfg, &secrets).unwrap().provider_name(),
            "windows_sapi"
        );
    }

    #[test]
    fn audio_resample_and_rms_db_work_as_expected() {
        let (rms_empty, db_empty) = compute_rms_db(&[]);
        assert_eq!(rms_empty, 0.0);
        assert!((db_empty - (-180.0)).abs() < 1e-3);

        // Stereo [1.0, -1.0] downmixes to 0.0 mono
        let mono_zero = resample_linear_mono(&[1.0, -1.0, 0.5, -0.5], 16_000, 2, 16_000);
        assert_eq!(mono_zero, vec![0.0, 0.0]);

        // 48kHz -> 16kHz reduces sample count by ~3x
        let src_48k = vec![0.25_f32; 4800];
        let dst_16k = resample_linear_mono(&src_48k, 48_000, 1, 16_000);
        assert_eq!(dst_16k.len(), 1600);
    }

    #[test]
    fn tts_mute_gate_blocks_during_playback_and_350ms_echo_tail() {
        let t0 = Instant::now();
        let tail = Duration::from_millis(TTS_ECHO_TAIL_MS);
        let mut mute_until = None;

        // Not playing initially -> unmuted
        assert!(!evaluate_tts_mute_gate(false, &mut mute_until, t0, tail));
        assert!(mute_until.is_none());

        // Playing -> muted and schedules 350ms tail
        assert!(evaluate_tts_mute_gate(true, &mut mute_until, t0, tail));
        assert!(mute_until.is_some());

        // Playback stops, 100ms later -> still muted by 350ms echo tail
        assert!(evaluate_tts_mute_gate(
            false,
            &mut mute_until,
            t0 + Duration::from_millis(100),
            tail
        ));

        // 350ms elapsed -> unmuted and cleared
        assert!(!evaluate_tts_mute_gate(
            false,
            &mut mute_until,
            t0 + Duration::from_millis(350),
            tail
        ));
        assert!(mute_until.is_none());
    }

    #[tokio::test]
    async fn process_ptt_wav_emits_error_and_resets_idle_on_stt_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let db_path = dir.path().join("jarvis.db");
        let secrets = Arc::new(InMemorySecretStore::new());
        let state = crate::AppState::init_with_paths(&cfg_path, &db_path, secrets).unwrap();

        let mut rx = state.event_bus.subscribe();
        let wav = encode_wav_i16_mono(&[0.1_f32; 1600], TARGET_SAMPLE_RATE).unwrap();
        let res = state.voice.process_ptt_wav("s-ptt", &wav).await;
        assert!(res.is_err());

        let mut saw_error = false;
        let mut saw_idle = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                CoreEvent::Error { session_id, .. } if session_id.as_deref() == Some("s-ptt") => {
                    saw_error = true;
                }
                CoreEvent::ReactorStateChanged {
                    state: ReactorState::Idle,
                } => {
                    saw_idle = true;
                }
                _ => {}
            }
        }
        assert!(saw_error, "expected CoreEvent::Error on STT failure");
        assert!(saw_idle, "expected ReactorState::Idle reset on STT failure");
    }
}
