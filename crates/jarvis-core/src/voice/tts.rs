//! Text-to-Speech (`TtsBackend`) trait, `AudioPlayer` (`rodio` on a dedicated OS thread),
//! and provider implementations:
//! - [`EdgeTtsBackend`]: Microsoft Edge Neural TTS WebSocket client (DRM v2 `Sec-MS-GEC` SHA-256 + CS64)
//! - [`OpenAiTtsBackend`]: OpenAI `/audio/speech` (`tts-1`)
//! - [`OfflineWindowsSapiTts`]: Windows `System.Speech.Synthesis.SpeechSynthesizer` via hidden PowerShell

use super::{VoiceError, VoiceResult};
use crate::config::VoiceConfig;
use crate::secrets::{keys, SecretStore};
use async_trait::async_trait;
use chrono::Utc;
use cpal::traits::{DeviceTrait, HostTrait};
use futures::{SinkExt, StreamExt};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

pub const TRUSTED_CLIENT_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
pub const EDGE_TTS_WSS_BASE: &str =
    "wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1";
pub const DEFAULT_VIETNAMESE_FEMALE_VOICE: &str = "vi-VN-HoaiMyNeural";
pub const DEFAULT_VIETNAMESE_MALE_VOICE: &str = "vi-VN-NamMinhNeural";

pub const CHROMIUM_FULL_VERSION: &str = "143.0.3650.75";
pub const CHROMIUM_MAJOR_VERSION: &str = "143";
pub const SEC_MS_GEC_VERSION: &str = "2-143.0.3650.75";
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36 Edg/143.0.0.0";
pub const ORIGIN: &str = "chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold";

pub const DEFAULT_OPENAI_TTS_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENAI_TTS_MODEL: &str = "tts-1";
pub const DEFAULT_OPENAI_TTS_VOICE: &str = "alloy";

const WIN_EPOCH: u64 = 11_644_473_600;
const TICKS_PER_SECOND: u64 = 10_000_000;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Asynchronous Text-to-Speech backend trait returning encoded audio bytes (MP3 or WAV).
#[async_trait]
pub trait TtsBackend: Send + Sync {
    /// Human-readable provider identifier (e.g. `"edge_tts"`, `"openai_tts"`, `"windows_sapi"`).
    fn provider_name(&self) -> &'static str;

    /// Synthesize `text` using `voice`, playback `rate` (1.0 = normal), and `pitch` (Hz offset, 0.0 = normal).
    async fn synthesize(
        &self,
        text: &str,
        voice: &str,
        rate: f32,
        pitch: f32,
    ) -> VoiceResult<Vec<u8>>;
}

// ============================================================================
// AudioPlayer (dedicated OS thread for rodio::OutputStream)
// ============================================================================

enum PlayerCmd {
    Enqueue { bytes: Vec<u8>, generation: u64 },
    Stop,
    SetVolume(f32),
    SetOutputDevice(Option<String>),
    Shutdown,
}

#[derive(Debug, Default)]
struct QueueState {
    generation: u64,
    pending_decode: usize,
}

fn open_output_stream(device_name: Option<&str>) -> Option<(OutputStream, OutputStreamHandle)> {
    let trimmed = device_name.map(str::trim).filter(|s| !s.is_empty());
    if let Some(target) = trimmed {
        let host = cpal::default_host();
        if let Ok(mut devices) = host.output_devices() {
            let lower = target.to_ascii_lowercase();
            let matched = devices.find(|d| {
                d.name()
                    .ok()
                    .is_some_and(|n| n == target || n.to_ascii_lowercase() == lower)
            });
            if let Some(dev) = matched {
                match OutputStream::try_from_device(&dev) {
                    Ok(pair) => return Some(pair),
                    Err(err) => {
                        tracing::warn!(
                            device = %target,
                            error = %err,
                            "Failed to open requested audio output device; falling back to default"
                        );
                    }
                }
            } else {
                tracing::warn!(
                    device = %target,
                    "Requested audio output device not found; falling back to default"
                );
            }
        }
    }

    match OutputStream::try_default() {
        Ok(pair) => Some(pair),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "No audio output device available; AudioPlayer running in headless mode"
            );
            None
        }
    }
}

/// Non-blocking, thread-safe audio playback queue backed by `rodio` on a dedicated OS thread.
///
/// Supports sequential queueing (`enqueue_audio_bytes`) and instant barge-in (`stop_playback`).
pub struct AudioPlayer {
    cmd_tx: Sender<PlayerCmd>,
    queue_state: Arc<Mutex<QueueState>>,
    sink_queued: Arc<AtomicUsize>,
    playing: Arc<AtomicBool>,
    volume_bits: Arc<AtomicU32>,
    has_device: Arc<AtomicBool>,
    output_device: Arc<Mutex<Option<String>>>,
}

impl AudioPlayer {
    /// Create a new `AudioPlayer` backed by a dedicated `rodio` output thread.
    pub fn new() -> Self {
        Self::spawn_worker(false, None)
    }

    /// Create a new `AudioPlayer` targeting a specific output device name (or default if `None`).
    pub fn new_with_device(output_device: Option<String>) -> Self {
        let normalized = output_device
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self::spawn_worker(false, normalized)
    }

    /// Create a headless `AudioPlayer` that tracks queue state without opening an OS audio device.
    pub fn new_headless() -> Self {
        Self::spawn_worker(true, None)
    }

    fn spawn_worker(force_headless: bool, initial_device: Option<String>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCmd>();
        let queue_state = Arc::new(Mutex::new(QueueState::default()));
        let sink_queued = Arc::new(AtomicUsize::new(0));
        let playing = Arc::new(AtomicBool::new(false));
        let volume_bits = Arc::new(AtomicU32::new(1.0_f32.to_bits()));
        let has_device = Arc::new(AtomicBool::new(false));
        let output_device = Arc::new(Mutex::new(initial_device.clone()));

        let state_w = Arc::clone(&queue_state);
        let sink_q_w = Arc::clone(&sink_queued);
        let playing_w = Arc::clone(&playing);
        let vol_w = Arc::clone(&volume_bits);
        let has_dev_w = Arc::clone(&has_device);

        let _ = std::thread::Builder::new()
            .name("jarvis-audio-player".into())
            .spawn(move || {
                let mut output: Option<(OutputStream, OutputStreamHandle)> = if force_headless {
                    None
                } else {
                    let pair = open_output_stream(initial_device.as_deref());
                    has_dev_w.store(pair.is_some(), Ordering::SeqCst);
                    pair
                };

                let mut sink: Option<Sink> = output
                    .as_ref()
                    .and_then(|(_, handle)| Sink::try_new(handle).ok());

                loop {
                    match cmd_rx.recv_timeout(Duration::from_millis(25)) {
                        Ok(PlayerCmd::Enqueue { bytes, generation }) => {
                            let is_current = {
                                let guard = state_w.lock().unwrap_or_else(|e| e.into_inner());
                                guard.generation == generation
                            };
                            if !is_current {
                                continue;
                            }

                            if let Some(ref s) = sink {
                                match Decoder::new(Cursor::new(bytes)) {
                                    Ok(source) => {
                                        s.set_volume(f32::from_bits(vol_w.load(Ordering::SeqCst)));
                                        s.append(source);
                                        s.play();
                                        playing_w.store(true, Ordering::SeqCst);
                                    }
                                    Err(err) => {
                                        tracing::warn!(error = %err, "Failed to decode audio bytes");
                                    }
                                }
                            }

                            let mut guard = state_w.lock().unwrap_or_else(|e| e.into_inner());
                            if guard.generation == generation {
                                guard.pending_decode = guard.pending_decode.saturating_sub(1);
                            }
                        }
                        Ok(PlayerCmd::Stop) => {
                            if let Some(old_sink) = sink.take() {
                                old_sink.stop();
                            }
                            if let Some((_, ref handle)) = output {
                                sink = Sink::try_new(handle).ok();
                                if let Some(ref s) = sink {
                                    s.set_volume(f32::from_bits(vol_w.load(Ordering::SeqCst)));
                                }
                            }
                            sink_q_w.store(0, Ordering::SeqCst);
                            let has_newer_pending = {
                                let guard = state_w.lock().unwrap_or_else(|e| e.into_inner());
                                guard.pending_decode > 0
                            };
                            if !has_newer_pending {
                                playing_w.store(false, Ordering::SeqCst);
                            }
                        }
                        Ok(PlayerCmd::SetVolume(vol)) => {
                            if let Some(ref s) = sink {
                                s.set_volume(vol);
                            }
                        }
                        Ok(PlayerCmd::SetOutputDevice(dev)) => {
                            if let Some(old_sink) = sink.take() {
                                old_sink.stop();
                            }
                            drop(output.take());
                            sink_q_w.store(0, Ordering::SeqCst);
                            if !force_headless {
                                output = open_output_stream(dev.as_deref());
                                has_dev_w.store(output.is_some(), Ordering::SeqCst);
                                sink = output
                                    .as_ref()
                                    .and_then(|(_, handle)| Sink::try_new(handle).ok());
                                if let Some(ref s) = sink {
                                    s.set_volume(f32::from_bits(vol_w.load(Ordering::SeqCst)));
                                }
                            }
                        }
                        Ok(PlayerCmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                            if let Some(old_sink) = sink.take() {
                                old_sink.stop();
                            }
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                    }

                    let active_in_sink = sink.as_ref().map_or(0, |s| s.len());
                    sink_q_w.store(active_in_sink, Ordering::SeqCst);
                    let pending_decode = {
                        let guard = state_w.lock().unwrap_or_else(|e| e.into_inner());
                        guard.pending_decode
                    };
                    if active_in_sink == 0 && pending_decode == 0 {
                        playing_w.store(false, Ordering::SeqCst);
                    }
                }
            });

        Self {
            cmd_tx,
            queue_state,
            sink_queued,
            playing,
            volume_bits,
            has_device,
            output_device,
        }
    }

    /// Enqueue encoded audio bytes (MP3, WAV, OGG, FLAC) for sequential playback.
    pub fn enqueue_audio_bytes(&self, audio_bytes: Vec<u8>) -> VoiceResult<()> {
        if audio_bytes.is_empty() {
            return Err(VoiceError::Playback(
                "Cannot enqueue empty audio payload".into(),
            ));
        }
        let gen = {
            let mut guard = self.queue_state.lock().unwrap_or_else(|e| e.into_inner());
            guard.pending_decode += 1;
            guard.generation
        };
        self.playing.store(true, Ordering::SeqCst);
        self.cmd_tx
            .send(PlayerCmd::Enqueue {
                bytes: audio_bytes,
                generation: gen,
            })
            .map_err(|e| {
                let mut guard = self.queue_state.lock().unwrap_or_else(|e| e.into_inner());
                if guard.generation == gen {
                    guard.pending_decode = guard.pending_decode.saturating_sub(1);
                }
                VoiceError::Playback(format!("Audio player channel closed: {e}"))
            })
    }

    /// Immediately halt current playback and clear all pending queued audio (barge-in).
    pub fn stop_playback(&self) {
        {
            let mut guard = self.queue_state.lock().unwrap_or_else(|e| e.into_inner());
            guard.generation = guard.generation.wrapping_add(1);
            guard.pending_decode = 0;
        }
        self.sink_queued.store(0, Ordering::SeqCst);
        self.playing.store(false, Ordering::SeqCst);
        let _ = self.cmd_tx.send(PlayerCmd::Stop);
    }

    /// Set playback volume (`0.0` = mute, `1.0` = normal, clamped to `0.0..=2.0`).
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 2.0);
        self.volume_bits.store(clamped.to_bits(), Ordering::SeqCst);
        let _ = self.cmd_tx.send(PlayerCmd::SetVolume(clamped));
    }

    /// Current playback volume.
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::SeqCst))
    }

    /// Returns `true` if audio is currently playing or queued.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::SeqCst)
    }

    /// Number of audio chunks currently pending decode or queued in the output sink.
    pub fn queued_count(&self) -> usize {
        let pending_decode = self
            .queue_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_decode;
        pending_decode + self.sink_queued.load(Ordering::SeqCst)
    }

    /// Route playback to a specific output device name (or system default when `None`).
    pub fn set_output_device(&self, device: Option<String>) {
        let normalized = device
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        {
            let mut guard = self.output_device.lock().unwrap_or_else(|e| e.into_inner());
            if *guard == normalized {
                return;
            }
            *guard = normalized.clone();
        }
        let _ = self.cmd_tx.send(PlayerCmd::SetOutputDevice(normalized));
    }

    /// Currently configured output device name (`None` = system default).
    pub fn output_device(&self) -> Option<String> {
        self.output_device
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Returns `true` if a physical audio output device was opened.
    pub fn has_output_device(&self) -> bool {
        self.has_device.load(Ordering::SeqCst)
    }
}

impl Default for AudioPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PlayerCmd::Shutdown);
    }
}

// ============================================================================
// EdgeTtsBackend (DRM v2 Sec-MS-GEC: SHA-256 + CS64)
// ============================================================================

const SHA256_INITIAL: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
    0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];

const SHA256_STATE: [u32; 8] = [
    0xFD9FBEA2, 0x91243CE7, 0xBE499094, 0xCD2E1E0A,
    0x6C006F49, 0x04BD35AB, 0xFEB0A7E4, 0x03EFFBD2,
];

const SHA256_SCHEDULE: [u32; 14] = [
    0xEB24BF9E, 0x1558691F, 0xA0C09EE2, 0x0CFA32BA,
    0xA622EE8C, 0xD1DD9491, 0xC0DD25A0, 0xF9E70AB6,
    0xF0C31573, 0x7058CE4E, 0x7188E606, 0x00E5AB33,
    0xD258B060, 0x72766564,
];

const SHA256_ROUND_CONSTANTS: [u32; 64] = [
    0x428A2F98, 0x71374491, 0xB5C0FBCF, 0xE9B5DBA5, 0x3956C25B, 0x59F111F1, 0x923F82A4, 0xAB1C5ED5,
    0xD807AA98, 0x12835B01, 0x243185BE, 0x550C7DC3, 0x72BE5D74, 0x80DEB1FE, 0x9BDC06A7, 0xC19BF174,
    0xE49B69C1, 0xEFBE4786, 0x0FC19DC6, 0x240CA1CC, 0x2DE92C6F, 0x4A7484AA, 0x5CB0A9DC, 0x76F988DA,
    0x983E5152, 0xA831C66D, 0xB00327C8, 0xBF597FC7, 0xC6E00BF3, 0xD5A79147, 0x06CA6351, 0x14292967,
    0x27B70A85, 0x2E1B2138, 0x4D2C6DFC, 0x53380D13, 0x650A7354, 0x766A0ABB, 0x81C2C92E, 0x92722C85,
    0xA2BFE8A1, 0xA81A664B, 0xC24B8B70, 0xC76C51A3, 0xD192E819, 0xD6990624, 0xF40E3585, 0x106AA070,
    0x19A4C116, 0x1E376C08, 0x2748774C, 0x34B0BCB5, 0x391C0CB3, 0x4ED8AA4A, 0x5B9CCA4F, 0x682E6FF3,
    0x748F82EE, 0x78A5636F, 0x84C87814, 0x8CC70208, 0x90BEFFFA, 0xA4506CEB, 0xBEF9A3F7, 0xC67178F2,
];

const SHA256_ADJUSTMENT: u32 = 0xC9DC06A7;
const SHA256_SIGMA0_ADJUSTMENT: u32 = 0x059C0B80;
const SHA256_SIGMA1_ADJUSTMENT: u32 = 0x000B92C0;

const CS64_COEFFICIENTS: [(u32, u32); 14] = [
    (0xE82E91B1, 0xB32700D0),
    (0x44DE24C6, 0x88DFF51D),
    (0x3BEB65BC, 0x29514038),
    (0xC89C5933, 0xB328CBDF),
    (0x8D1EC211, 0xD405B4D5),
    (0x314EF5D5, 0x26E55F3C),
    (0xE92491B1, 0x607801C6),
    (0xF0A5B9D5, 0x82B2C1FB),
    (0x06064CF3, 0xFA51A3DA),
    (0xDA79FA60, 0x207CC464),
    (0xBED20560, 0x3C3C74F3),
    (0x89F9C9F8, 0xD2D14B61),
    (0x35FA38C6, 0xBB04A2C1),
    (0xCB49E3C6, 0xC5626167),
];

const CS64_FACTOR: u32 = 0x82893933;
const CS64_ADJUSTMENT0: u32 = 0xF2F90B1A;
const CS64_ADJUSTMENT1: u32 = 0x0B1A0000;

#[inline]
fn sigma0(val: u32) -> u32 {
    val.rotate_right(7) ^ val.rotate_right(18) ^ (val >> 3)
}

#[inline]
fn sigma1(val: u32) -> u32 {
    val.rotate_right(17) ^ val.rotate_right(19) ^ (val >> 10)
}

fn expand_sha256_schedule(words: &mut Vec<u32>, first_word: usize) {
    for index in first_word..64 {
        let x = words[index - 2];
        let y = words[index - 15];
        let s1 = sigma1(x);
        let s0 = sigma0(y);
        let w = s1
            .wrapping_add(words[index - 7])
            .wrapping_add(s0)
            .wrapping_add(words[index - 16]);
        words.push(w);
    }
}

fn sha256_rounds(state: [u32; 8], words: &[u32], first_round: usize) -> [u32; 8] {
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
    for index in first_round..64 {
        let rot_e = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = g ^ (e & (f ^ g));
        let k = if first_round == 14 && index == 14 {
            SHA256_ADJUSTMENT
        } else {
            SHA256_ROUND_CONSTANTS[index]
        };
        let t1 = h
            .wrapping_add(rot_e)
            .wrapping_add(ch)
            .wrapping_add(k)
            .wrapping_add(words[index]);
        let rot_a = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) | (c & (a | b));
        let t2 = rot_a.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    [a, b, c, d, e, f, g, h]
}

fn sha256_init(data: &[u8]) -> [u32; 8] {
    assert!(data.len() >= 7, "Token salt must contain at least 7 bytes");
    let mut words = Vec::with_capacity(64);
    words.resize(14, 0u32);
    let w14 = ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | (data[2] as u32);
    words.push(w14);
    let w15 = u32::from_be_bytes([data[3], data[4], data[5], data[6]]);
    words.push(w15);

    for index in 16..30 {
        let mut value = SHA256_SCHEDULE[index - 16];
        if index - 2 >= 14 {
            let s1 = if index == 16 {
                sigma1(words[index - 2]) ^ SHA256_SIGMA1_ADJUSTMENT
            } else {
                sigma1(words[index - 2])
            };
            value = value.wrapping_add(s1);
        }
        if index - 7 >= 14 {
            value = value.wrapping_add(words[index - 7]);
        }
        if index - 15 >= 14 {
            let s0 = if index == 29 {
                sigma0(words[index - 15]) ^ SHA256_SIGMA0_ADJUSTMENT
            } else {
                sigma0(words[index - 15])
            };
            value = value.wrapping_add(s0);
        }
        words.push(value);
    }

    let w30 = sigma1(words[28])
        .wrapping_add(words[23])
        .wrapping_add(sigma0(words[15]))
        .wrapping_add(words[14])
        .wrapping_add(SHA256_ADJUSTMENT)
        .wrapping_sub(SHA256_ROUND_CONSTANTS[14]);
    words.push(w30);

    expand_sha256_schedule(&mut words, 31);
    let working = sha256_rounds(SHA256_STATE, &words, 14);

    let mut state = [0u32; 8];
    for i in 0..8 {
        state[i] = SHA256_INITIAL[i].wrapping_add(working[i]);
    }
    state
}

fn sha256_digest(data: &[u8]) -> [u8; 32] {
    assert!(data.len() >= 7, "Token salt must contain at least 7 bytes");
    let length = 57 + data.len();
    let pad_len = (55_isize - length as isize).rem_euclid(64) as usize;

    let mut padded = Vec::with_capacity((data.len() - 7) + 1 + pad_len + 8);
    padded.extend_from_slice(&data[7..]);
    padded.push(0x80);
    padded.resize(padded.len() + pad_len, 0);
    padded.extend_from_slice(&((length as u64) * 8).to_be_bytes());

    let mut state = sha256_init(&data[..7]);
    for chunk in padded.chunks_exact(64) {
        let mut words = Vec::with_capacity(64);
        for word_bytes in chunk.chunks_exact(4) {
            words.push(u32::from_be_bytes(word_bytes.try_into().unwrap()));
        }
        expand_sha256_schedule(&mut words, 16);
        let working = sha256_rounds(state, &words, 0);
        for i in 0..8 {
            state[i] = state[i].wrapping_add(working[i]);
        }
    }

    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn words_le4_even(data: &[u8]) -> Vec<u32> {
    if data.is_empty() {
        return Vec::new();
    }
    let pad_len = (4 - (data.len() % 4)) % 4;
    let total_len = data.len() + pad_len;
    let mut words = Vec::with_capacity(total_len / 4 + 1);
    let mut i = 0;
    while i < data.len() {
        let mut b = [0u8; 4];
        let take = (data.len() - i).min(4);
        b[..take].copy_from_slice(&data[i..i + take]);
        words.push(u32::from_le_bytes(b));
        i += 4;
    }
    if words.len() % 2 != 0 {
        words.push(0);
    }
    words
}

fn cs64_digest(data: &[u8], digest: &[u8; 32]) -> (u32, u32) {
    let key_a = u32::from_le_bytes(digest[0..4].try_into().unwrap()) | 1;
    let key_b = u32::from_le_bytes(digest[4..8].try_into().unwrap()) | 1;
    let factors = [
        key_a.wrapping_mul(CS64_FACTOR),
        key_b.wrapping_mul(CS64_FACTOR),
    ];
    let mut terms = Vec::with_capacity(14 + data.len() / 4 + 2);
    let mut offsets = Vec::with_capacity(14 + data.len() / 4 + 2);

    for (index, &(coef, offset)) in CS64_COEFFICIENTS.iter().enumerate() {
        terms.push(coef.wrapping_mul(factors[index % 2]));
        offsets.push(offset);
    }

    let mut data_with_zero = Vec::with_capacity(data.len() + 1);
    data_with_zero.push(0u8);
    data_with_zero.extend_from_slice(data);
    let words = words_le4_even(&data_with_zero);

    for (index, &word) in words.iter().enumerate() {
        let mut term = word.wrapping_mul(if index % 2 == 0 { key_a } else { key_b });
        let mut offset = word.rotate_left(16).wrapping_mul(if index % 2 == 0 {
            0xEF0569FB
        } else {
            0xC31713DB
        });
        if index == 0 {
            term = term.wrapping_add(CS64_ADJUSTMENT0.wrapping_mul(factors[0]));
            offset = offset.wrapping_add(CS64_ADJUSTMENT1);
        }
        terms.push(term);
        offsets.push(offset);
    }

    let mut swap_acc = 0u32;
    let mut swap_last = 0u32;
    let mut state = 0u32;
    let mut acc = 0u32;

    for i in (0..terms.len()).step_by(2) {
        let x = terms[i];
        let y = terms[i + 1];

        let mut t = offsets[i].wrapping_add(x).rotate_left(16);
        let mut q = t.wrapping_mul(0x76F50F34);
        swap_acc = swap_acc.wrapping_add(q.rotate_left(16).wrapping_mul(0xC3EFEA97).wrapping_add(q));

        t = offsets[i + 1].wrapping_add(y).rotate_left(16);
        q = t.wrapping_mul(0x1881CE3C);
        swap_last = q.rotate_left(16).wrapping_mul(0x35BD1EC9).wrapping_add(q);
        swap_acc = swap_acc.wrapping_add(swap_last);

        state = state.wrapping_mul(key_a).wrapping_add(x);
        state = state.rotate_left(16).wrapping_mul(0xCF98B111);
        state = state.rotate_left(16).wrapping_mul(0x87085B9F);
        state = state.rotate_left(16).wrapping_mul(0x12CEB96D);
        state = state.rotate_left(16).wrapping_mul(0x257E1D83);
        acc = acc.wrapping_add(state);

        t = state.wrapping_mul(key_b).wrapping_add(y);
        t = t.rotate_left(16).wrapping_mul(0xA27416F5);
        t = t.rotate_left(16).wrapping_mul(0xD38396FF);
        state = t
            .rotate_left(16)
            .wrapping_mul(0x7C932B89)
            .rotate_left(16)
            .wrapping_mul(0xBFA49F69);
        acc = acc.wrapping_add(state);
    }

    (swap_last ^ state, swap_acc ^ acc)
}

fn token_hash(data: &[u8]) -> (String, String) {
    let digest = sha256_digest(data);
    let (cs_w0, cs_w1) = cs64_digest(data, &digest);
    let mut cs64_bytes = [0u8; 8];
    cs64_bytes[0..4].copy_from_slice(&cs_w0.to_le_bytes());
    cs64_bytes[4..8].copy_from_slice(&cs_w1.to_le_bytes());
    (hex::encode_upper(cs64_bytes), hex::encode_upper(digest))
}

/// Generate the `Sec-MS-GEC` token value (DRM v2 format: `<16 hex CS64>-<64 hex SHA-256>`).
pub fn generate_sec_ms_gec_v2(unix_timestamp_secs: u64) -> String {
    let mut ticks = unix_timestamp_secs.saturating_add(WIN_EPOCH);
    ticks -= ticks % 300;
    ticks = ticks.saturating_mul(TICKS_PER_SECOND);
    let payload = format!("{ticks}{TRUSTED_CLIENT_TOKEN}");
    let (cs64, sha256) = token_hash(payload.as_bytes());
    format!("{cs64}-{sha256}")
}

/// Generate the `Sec-MS-GEC` token for a given Unix timestamp in seconds.
pub fn generate_sec_ms_gec(unix_timestamp_secs: u64) -> String {
    generate_sec_ms_gec_v2(unix_timestamp_secs)
}

/// Generate the `Sec-MS-GEC` token for the current system clock.
pub fn current_sec_ms_gec() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    generate_sec_ms_gec_v2(secs)
}

/// Generate a 32-character uppercase hex MUID.
pub fn generate_muid() -> String {
    let id = Uuid::new_v4();
    hex::encode_upper(id.as_bytes())
}

/// Format Javascript-style UTC date string expected by Edge TTS WebSocket headers.
pub fn date_to_string() -> String {
    Utc::now()
        .format("%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)")
        .to_string()
}

/// Metadata for a curated Edge Neural TTS voice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EdgeVoiceInfo {
    pub short_name: &'static str,
    pub display_name: &'static str,
    pub locale: &'static str,
    pub gender: &'static str,
}

/// Curated list of Edge Neural TTS voices, led by Vietnamese voices.
pub fn list_edge_voices() -> Vec<EdgeVoiceInfo> {
    vec![
        EdgeVoiceInfo {
            short_name: DEFAULT_VIETNAMESE_FEMALE_VOICE,
            display_name: "Hoài My (Tiếng Việt - Nữ)",
            locale: "vi-VN",
            gender: "Female",
        },
        EdgeVoiceInfo {
            short_name: DEFAULT_VIETNAMESE_MALE_VOICE,
            display_name: "Nam Minh (Tiếng Việt - Nam)",
            locale: "vi-VN",
            gender: "Male",
        },
        EdgeVoiceInfo {
            short_name: "en-US-AriaNeural",
            display_name: "Aria (English US - Female)",
            locale: "en-US",
            gender: "Female",
        },
        EdgeVoiceInfo {
            short_name: "en-US-GuyNeural",
            display_name: "Guy (English US - Male)",
            locale: "en-US",
            gender: "Male",
        },
    ]
}

/// Escape XML special characters for safe SSML interpolation.
pub fn escape_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Derive canonical Microsoft full voice name from short voice name.
pub fn canonical_voice_name(voice: &str) -> String {
    let trimmed = voice.trim();
    if trimmed.is_empty() {
        return "Microsoft Server Speech Text to Speech Voice (vi-VN, HoaiMyNeural)".to_string();
    }
    if trimmed.starts_with("Microsoft Server Speech Text to Speech Voice") {
        return trimmed.to_string();
    }
    let parts: Vec<&str> = trimmed.split('-').collect();
    if parts.len() >= 3 {
        let lang = parts[0];
        let region = parts[1];
        let rest = &parts[2..];
        if rest.len() == 1 {
            let name = rest[0];
            format!("Microsoft Server Speech Text to Speech Voice ({lang}-{region}, {name})")
        } else {
            let subregion = rest[..rest.len() - 1].join("-");
            let name = rest[rest.len() - 1];
            format!(
                "Microsoft Server Speech Text to Speech Voice ({lang}-{region}-{subregion}, {name})"
            )
        }
    } else {
        trimmed.to_string()
    }
}

/// Build an SSML `<speak>` document matching Microsoft Edge Browser format.
pub fn build_ssml(text: &str, voice: &str, rate: f32, pitch: f32) -> String {
    let voice_canonical = canonical_voice_name(voice);
    let rate_pct = ((rate - 1.0) * 100.0).round().clamp(-80.0, 200.0) as i32;
    let pitch_hz = pitch.round().clamp(-100.0, 100.0) as i32;
    let rate_str = format!("{rate_pct:+}%");
    let pitch_str = format!("{pitch_hz:+}Hz");
    let escaped_voice = escape_xml(&voice_canonical);
    let escaped_text = escape_xml(text);

    format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'>\
         <voice name='{escaped_voice}'>\
         <prosody pitch='{pitch_str}' rate='{rate_str}' volume='+0%'>{escaped_text}</prosody>\
         </voice></speak>"
    )
}

/// Extract MP3 audio bytes from a binary Edge-TTS WebSocket frame.
pub fn extract_audio_from_binary_frame(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() < 2 {
        return None;
    }
    let header_len = u16::from_be_bytes([frame[0], frame[1]]) as usize;
    let payload_start = 2usize.checked_add(header_len)?;
    if payload_start > frame.len() {
        return None;
    }
    let header = std::str::from_utf8(&frame[2..payload_start]).ok()?;
    if header.lines().any(|line| line.trim() == "Path:audio") {
        Some(&frame[payload_start..])
    } else {
        None
    }
}

/// Pure-Rust Microsoft Edge Neural TTS WebSocket client (free, no API key required).
#[derive(Debug, Clone)]
pub struct EdgeTtsBackend {
    ws_base_url: String,
    default_voice: String,
}

impl EdgeTtsBackend {
    pub fn new() -> Self {
        Self {
            ws_base_url: EDGE_TTS_WSS_BASE.to_string(),
            default_voice: DEFAULT_VIETNAMESE_FEMALE_VOICE.to_string(),
        }
    }

    /// Override default voice (e.g. `vi-VN-NamMinhNeural` or `vi-VN-HoaiMyNeural`).
    pub fn with_default_voice(mut self, voice: impl Into<String>) -> Self {
        self.default_voice = voice.into();
        self
    }

    /// Override WebSocket endpoint (useful for mock WebSocket tests).
    pub fn with_ws_url(mut self, ws_base_url: impl Into<String>) -> Self {
        self.ws_base_url = ws_base_url.into();
        self
    }

    /// Deterministic DRM v2 `Sec-MS-GEC` generator (`<16 hex CS64>-<64 hex SHA-256>`).
    pub fn generate_sec_ms_gec(unix_timestamp_secs: u64) -> String {
        generate_sec_ms_gec_v2(unix_timestamp_secs)
    }

    pub fn build_ws_url(&self, connection_id: &str) -> String {
        let gec = current_sec_ms_gec();
        let base = self.ws_base_url.trim_end_matches('?');
        let sep = if base.contains('?') { '&' } else { '?' };
        let normalized = if !base.contains('?') && base.matches('/').count() == 2 {
            format!("{base}/")
        } else {
            base.to_string()
        };
        format!(
            "{normalized}{sep}TrustedClientToken={TRUSTED_CLIENT_TOKEN}&ConnectionId={connection_id}&Sec-MS-GEC={gec}&Sec-MS-GEC-Version={SEC_MS_GEC_VERSION}"
        )
    }
}

impl Default for EdgeTtsBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TtsBackend for EdgeTtsBackend {
    fn provider_name(&self) -> &'static str {
        "edge_tts"
    }

    async fn synthesize(
        &self,
        text: &str,
        voice: &str,
        rate: f32,
        pitch: f32,
    ) -> VoiceResult<Vec<u8>> {
        if text.trim().is_empty() {
            return Err(VoiceError::Tts("Cannot synthesize empty text".into()));
        }

        let target_voice = if voice.trim().is_empty() {
            self.default_voice.as_str()
        } else {
            voice.trim()
        };

        let connection_id = Uuid::new_v4().simple().to_string();
        let request_id = Uuid::new_v4().simple().to_string();
        let ws_url = self.build_ws_url(&connection_id);

        let mut request = ws_url
            .into_client_request()
            .map_err(|e| VoiceError::WebSocket(format!("Invalid Edge-TTS WebSocket URL: {e}")))?;
        let headers = request.headers_mut();

        headers.insert("Pragma", "no-cache".parse().expect("valid header"));
        headers.insert("Cache-Control", "no-cache".parse().expect("valid header"));
        headers.insert("Origin", ORIGIN.parse().expect("valid header"));
        headers.insert("User-Agent", USER_AGENT.parse().expect("valid header"));
        headers.insert(
            "Accept-Encoding",
            "gzip, deflate, br, zstd".parse().expect("valid header"),
        );
        headers.insert(
            "Accept-Language",
            "en-US,en;q=0.9".parse().expect("valid header"),
        );
        headers.insert(
            "Cookie",
            format!("muid={};", generate_muid())
                .parse()
                .expect("valid header"),
        );

        let (mut ws_stream, _) = connect_async(request)
            .await
            .map_err(|e| VoiceError::WebSocket(format!("Edge-TTS WebSocket connect failed: {e}")))?;

        let config_timestamp = date_to_string();
        let config_msg = format!(
            "X-Timestamp:{config_timestamp}\r\nContent-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n\
{{\"context\":{{\"synthesis\":{{\"audio\":{{\"metadataoptions\":{{\"sentenceBoundaryEnabled\":\"false\",\"wordBoundaryEnabled\":\"false\"}},\"outputFormat\":\"audio-24khz-48kbitrate-mono-mp3\"}}}}}}}}\r\n"
        );
        ws_stream
            .send(Message::Text(config_msg))
            .await
            .map_err(|e| VoiceError::WebSocket(format!("Failed sending speech.config: {e}")))?;

        let ssml = build_ssml(text, target_voice, rate, pitch);
        let ssml_timestamp = date_to_string();
        let ssml_msg = format!(
            "X-RequestId:{request_id}\r\nContent-Type:application/ssml+xml\r\nX-Timestamp:{ssml_timestamp}Z\r\nPath:ssml\r\n\r\n{ssml}"
        );
        ws_stream
            .send(Message::Text(ssml_msg))
            .await
            .map_err(|e| VoiceError::WebSocket(format!("Failed sending SSML: {e}")))?;

        let mut audio_bytes = Vec::new();
        while let Some(msg_res) = ws_stream.next().await {
            let msg = msg_res
                .map_err(|e| VoiceError::WebSocket(format!("Edge-TTS WebSocket read error: {e}")))?;
            match msg {
                Message::Binary(bin) => {
                    if let Some(chunk) = extract_audio_from_binary_frame(&bin) {
                        audio_bytes.extend_from_slice(chunk);
                    }
                }
                Message::Text(txt) => {
                    if txt.contains("Path:turn.end") {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }

        let _ = ws_stream.close(None).await;

        if audio_bytes.is_empty() {
            return Err(VoiceError::Tts(
                "Edge-TTS returned empty audio stream".into(),
            ));
        }

        Ok(audio_bytes)
    }
}

// ============================================================================
// OpenAiTtsBackend (`tts-1`)
// ============================================================================

/// OpenAI `/audio/speech` TTS client (`tts-1`).
#[derive(Debug, Clone)]
pub struct OpenAiTtsBackend {
    api_key: String,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct OpenAiTtsRequest<'a> {
    model: &'a str,
    input: &'a str,
    voice: &'a str,
    speed: f32,
    response_format: &'static str,
}

impl OpenAiTtsBackend {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_OPENAI_TTS_MODEL.to_string(),
            base_url: DEFAULT_OPENAI_TTS_BASE_URL.to_string(),
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
impl TtsBackend for OpenAiTtsBackend {
    fn provider_name(&self) -> &'static str {
        "openai_tts"
    }

    async fn synthesize(
        &self,
        text: &str,
        voice: &str,
        rate: f32,
        _pitch: f32,
    ) -> VoiceResult<Vec<u8>> {
        if text.trim().is_empty() {
            return Err(VoiceError::Tts("Cannot synthesize empty text".into()));
        }

        let target_voice = if voice.trim().is_empty() || voice.contains("Neural") {
            DEFAULT_OPENAI_TTS_VOICE
        } else {
            voice.trim()
        };

        let endpoint = format!("{}/audio/speech", self.base_url.trim_end_matches('/'));
        let payload = OpenAiTtsRequest {
            model: &self.model,
            input: text,
            voice: target_voice,
            speed: rate.clamp(0.25, 4.0),
            response_format: "mp3",
        };

        let resp = self
            .client
            .post(&endpoint)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(VoiceError::Tts(format!(
                "OpenAI TTS request failed ({status}): {body}"
            )));
        }

        let bytes = resp.bytes().await?;
        Ok(bytes.to_vec())
    }
}

// ============================================================================
// OfflineWindowsSapiTts (hidden PowerShell System.Speech.Synthesis)
// ============================================================================

/// Zero-dependency offline Windows TTS using `System.Speech.Synthesis.SpeechSynthesizer`
/// invoked via hidden PowerShell (`CREATE_NO_WINDOW = 0x08000000`).
#[derive(Debug, Clone)]
pub struct OfflineWindowsSapiTts {
    timeout: Duration,
}

impl OfflineWindowsSapiTts {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Default for OfflineWindowsSapiTts {
    fn default() -> Self {
        Self::new()
    }
}

struct TempWavGuard(PathBuf);

impl Drop for TempWavGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(windows)]
fn text_has_vietnamese_diacritics(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c, 'ă' | 'â' | 'đ' | 'ê' | 'ô' | 'ơ' | 'ư' | 'Ă' | 'Â' | 'Đ' | 'Ê' | 'Ô' | 'Ơ' | 'Ư')
            || ('\u{00C0}'..='\u{024F}').contains(&c)
            || ('\u{1E00}'..='\u{1EFF}').contains(&c)
    })
}

#[cfg(windows)]
fn ps_utf8_expr(text: &str) -> String {
    let bytes_list = text
        .as_bytes()
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("[System.Text.Encoding]::UTF8.GetString([byte[]]@({bytes_list}))")
}

#[async_trait]
impl TtsBackend for OfflineWindowsSapiTts {
    fn provider_name(&self) -> &'static str {
        "windows_sapi"
    }

    async fn synthesize(
        &self,
        text: &str,
        voice: &str,
        rate: f32,
        _pitch: f32,
    ) -> VoiceResult<Vec<u8>> {
        if text.trim().is_empty() {
            return Err(VoiceError::Tts("Cannot synthesize empty text".into()));
        }

        #[cfg(not(windows))]
        {
            let _ = (voice, rate);
            return Err(VoiceError::Tts(
                "OfflineWindowsSapiTts is only supported on Windows".into(),
            ));
        }

        #[cfg(windows)]
        {
            let out_wav = std::env::temp_dir().join(format!("jarvis_sapi_{}.wav", Uuid::new_v4()));
            let _guard = TempWavGuard(out_wav.clone());

            let prefer_vi = text_has_vietnamese_diacritics(text) || voice.trim().to_ascii_lowercase().starts_with("vi") || voice.to_ascii_lowercase().contains("an");
            let winrt_rate = rate.clamp(0.5, 3.0);
            let sapi_rate = ((rate - 1.0) * 5.0).round().clamp(-10.0, 10.0) as i32;
            let req_voice_expr = ps_utf8_expr(voice.trim());
            let text_expr = ps_utf8_expr(text);
            let path_expr = ps_utf8_expr(&out_wav.to_string_lossy());
            let prefer_vi_ps = if prefer_vi { "$true" } else { "$false" };

            let script = format!(
                "$text = {text_expr}; \
                 $outPath = {path_expr}; \
                 $reqVoice = {req_voice_expr}; \
                 $preferVi = {prefer_vi_ps}; \
                 $done = $false; \
                 try {{ \
                     Add-Type -AssemblyName System.Runtime.WindowsRuntime; \
                     $null = [Windows.Media.SpeechSynthesis.SpeechSynthesizer, Windows.Media.SpeechSynthesis, ContentType = WindowsRuntime]; \
                     $null = [Windows.Storage.Streams.IInputStream, Windows.Storage.Streams, ContentType = WindowsRuntime]; \
                     $asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {{ \
                         $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' \
                     }})[0]; \
                     $synth = New-Object Windows.Media.SpeechSynthesis.SpeechSynthesizer; \
                     try {{ \
                         $voices = [Windows.Media.SpeechSynthesis.SpeechSynthesizer]::AllVoices; \
                         $picked = $null; \
                         if ($reqVoice -ne '' -and $reqVoice -notlike '*Neural*') {{ \
                             $picked = $voices | Where-Object {{ $_.DisplayName -like \"*$reqVoice*\" -or $_.Language -eq $reqVoice }} | Select-Object -First 1; \
                         }} \
                         if ($null -eq $picked -and $preferVi) {{ \
                             $picked = $voices | Where-Object {{ $_.Language -like 'vi*' }} | Select-Object -First 1; \
                         }} \
                         if ($null -eq $picked -and -not $preferVi) {{ \
                             $picked = $voices | Where-Object {{ $_.Language -like 'en*' }} | Select-Object -First 1; \
                         }} \
                         if ($null -ne $picked) {{ $synth.Voice = $picked; }} \
                         $synth.Options.SpeakingRate = {winrt_rate}; \
                         $asTask = $asTaskGeneric.MakeGenericMethod([Windows.Media.SpeechSynthesis.SpeechSynthesisStream]); \
                         $netTask = $asTask.Invoke($null, @($synth.SynthesizeTextToStreamAsync($text))); \
                         $netTask.Wait(-1) | Out-Null; \
                         $stream = $netTask.Result; \
                         $netStream = [System.IO.WindowsRuntimeStreamExtensions]::AsStreamForRead($stream.GetInputStreamAt(0)); \
                         $fs = [System.IO.File]::Create($outPath); \
                         try {{ $netStream.CopyTo($fs); }} finally {{ $fs.Dispose(); $netStream.Dispose(); $stream.Dispose(); }} \
                         if ((Test-Path $outPath) -and (Get-Item $outPath).Length -gt 44) {{ $done = $true; }} \
                     }} finally {{ \
                         $synth.Dispose(); \
                     }} \
                 }} catch {{}} \
                 if (-not $done) {{ \
                     Add-Type -AssemblyName System.Speech; \
                     $sapi = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
                     try {{ \
                         $sapi.Rate = {sapi_rate}; \
                         $installed = $sapi.GetInstalledVoices() | Where-Object {{ $_.Enabled }}; \
                         $matched = $null; \
                         if ($reqVoice -ne '' -and $reqVoice -notlike '*Neural*') {{ \
                             $matched = $installed | Where-Object {{ $_.VoiceInfo.Name -like \"*$reqVoice*\" -or $_.VoiceInfo.Culture.Name -eq $reqVoice }} | Select-Object -First 1; \
                         }} \
                         if ($null -eq $matched -and $preferVi) {{ \
                             $matched = $installed | Where-Object {{ $_.VoiceInfo.Culture.Name -like 'vi*' }} | Select-Object -First 1; \
                         }} \
                         if ($null -ne $matched) {{ $sapi.SelectVoice($matched.VoiceInfo.Name); }} \
                         $sapi.SetOutputToWaveFile($outPath); \
                         $sapi.Speak($text); \
                     }} finally {{ \
                         $sapi.Dispose(); \
                     }} \
                 }}"
            );

            let mut cmd = Command::new("powershell");
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .creation_flags(CREATE_NO_WINDOW);

            let output = tokio::time::timeout(self.timeout, cmd.output())
                .await
                .map_err(|_| VoiceError::Tts("Windows SAPI TTS synthesis timed out".into()))?
                .map_err(|e| VoiceError::Tts(format!("Failed to launch PowerShell SAPI TTS: {e}")))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(VoiceError::Tts(format!(
                    "Windows SAPI TTS failed ({}): {}",
                    output.status,
                    stderr.trim()
                )));
            }

            let wav_bytes = tokio::fs::read(&out_wav)
                .await
                .map_err(|e| VoiceError::Tts(format!("Failed reading synthesized SAPI WAV: {e}")))?;

            if wav_bytes.is_empty() {
                return Err(VoiceError::Tts(
                    "Windows SAPI TTS produced an empty WAV file".into(),
                ));
            }
            Ok(wav_bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn test_offline_windows_sapi_tts_synthesis() {
        let tts = OfflineWindowsSapiTts::new();
        let result = tts.synthesize("Xin chào, tôi là Jarvis.", "vi-VN-NamMinhNeural", 1.0, 0.0).await;
        assert!(result.is_ok(), "TTS synthesis failed: {:?}", result.err());
        let wav_bytes = result.unwrap();
        assert!(wav_bytes.len() > 1000, "WAV bytes too short: {}", wav_bytes.len());
        assert_eq!(&wav_bytes[0..4], b"RIFF", "Not a valid WAV/RIFF file");
    }
}

/// Build a [`TtsBackend`] from [`VoiceConfig`] and [`SecretStore`].
pub fn create_tts_backend(
    config: &VoiceConfig,
    secrets: &dyn SecretStore,
) -> VoiceResult<Arc<dyn TtsBackend>> {
    match config.tts_backend.trim().to_ascii_lowercase().as_str() {
        "" | "edge" | "edge_tts" => {
            let voice = if config.tts_voice.trim().is_empty() {
                DEFAULT_VIETNAMESE_FEMALE_VOICE
            } else {
                config.tts_voice.trim()
            };
            Ok(Arc::new(EdgeTtsBackend::new().with_default_voice(voice)))
        }
        "openai" | "openai_tts" => {
            let key = secrets
                .get(keys::OPENAI_API_KEY)
                .map_err(|e| VoiceError::Config(e.to_string()))?
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| {
                    VoiceError::Config("openai_api_key is required in SecretStore for OpenAI TTS".into())
                })?;
            Ok(Arc::new(OpenAiTtsBackend::new(key)))
        }
        "sapi" | "windows_sapi" | "offline" | "offline_sapi" => {
            Ok(Arc::new(OfflineWindowsSapiTts::new()))
        }
        other => Err(VoiceError::Config(format!(
            "Unsupported TTS backend: '{other}'"
        ))),
    }
}
