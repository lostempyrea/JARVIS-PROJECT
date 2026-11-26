//! Voice Activity Detection (VAD) with 1,200ms pre-roll ring buffer,
//! Push-to-Talk (PTT) controller, and in-memory 16-bit WAV encoding.

use super::audio_io::{compute_rms_db, TARGET_CHANNELS, TARGET_SAMPLE_RATE};
use super::{VoiceError, VoiceResult};
use crate::config::VoiceConfig;
use hound::{SampleFormat, WavSpec, WavWriter};
use std::collections::VecDeque;
use std::io::Cursor;

/// Compute root-mean-square (RMS) energy and dBFS for a slice of `f32` samples.
pub fn rms_db(samples: &[f32]) -> f32 {
    let (_, db) = compute_rms_db(samples);
    db
}
pub const DEFAULT_PRE_ROLL_MS: u64 = 1200;

/// Encode mono `f32` PCM samples in `[-1.0, 1.0]` into 16-bit PCM WAV bytes via `hound`.
pub fn encode_wav_i16_mono(samples: &[f32], sample_rate: u32) -> VoiceResult<Vec<u8>> {
    if samples.is_empty() {
        return Err(VoiceError::Encoding(
            "Cannot encode empty audio buffer to WAV".into(),
        ));
    }
    let spec = WavSpec {
        channels: TARGET_CHANNELS,
        sample_rate: sample_rate.max(1),
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut wav_bytes = Vec::with_capacity(44 + samples.len() * 2);
    {
        let cursor = Cursor::new(&mut wav_bytes);
        let mut writer = WavWriter::new(cursor, spec)
            .map_err(|e| VoiceError::Encoding(format!("Failed to initialize WAV writer: {e}")))?;
        for &sample in samples {
            let clamped = sample.clamp(-1.0, 1.0);
            let i16_sample = (clamped * i16::MAX as f32).round() as i16;
            writer
                .write_sample(i16_sample)
                .map_err(|e| VoiceError::Encoding(format!("Failed to write WAV sample: {e}")))?;
        }
        writer
            .finalize()
            .map_err(|e| VoiceError::Encoding(format!("Failed to finalize WAV stream: {e}")))?;
    }
    Ok(wav_bytes)
}

/// Configuration for [`VadProcessor`].
#[derive(Debug, Clone)]
pub struct VadConfig {
    /// Energy threshold in dBFS above which audio is classified as speech.
    pub threshold_db: f32,
    /// Trailing silence duration (ms) required to end a speech segment.
    pub silence_duration_ms: u64,
    /// Minimum speech duration (ms) required to emit a valid WAV segment.
    pub min_speech_ms: u64,
    /// Pre-roll buffer duration (ms) prepended when speech starts so initial phonemes aren't clipped.
    pub pre_roll_ms: u64,
    /// Sample rate in Hz (typically 16,000).
    pub sample_rate: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold_db: -40.0,
            silence_duration_ms: 800,
            min_speech_ms: 250,
            pre_roll_ms: DEFAULT_PRE_ROLL_MS,
            sample_rate: TARGET_SAMPLE_RATE,
        }
    }
}

impl From<&VoiceConfig> for VadConfig {
    fn from(cfg: &VoiceConfig) -> Self {
        Self {
            threshold_db: cfg.vad_threshold_db,
            silence_duration_ms: cfg.vad_silence_ms,
            min_speech_ms: cfg.vad_min_speech_ms,
            pre_roll_ms: cfg.vad_pre_roll_ms as u64,
            sample_rate: TARGET_SAMPLE_RATE,
        }
    }
}

/// Current state of the Auto-VAD state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadState {
    Idle,
    Speaking,
    SilenceCountdown { silence_samples: usize },
}

/// Event emitted when feeding audio frames into [`VadProcessor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadEvent {
    None,
    SpeechStarted,
    SpeechEnded(Vec<u8>),
}

/// Auto-VAD state machine that segments 16kHz mono audio into in-memory WAV utterances.
///
/// Maintains a configurable pre-roll ring buffer (default 1,200ms) so initial phonemes
/// of wake words such as `"hey jarvis"` or `"jarvis"` are retained when speech triggers.
pub struct VadProcessor {
    config: VadConfig,
    state: VadState,
    pre_roll: VecDeque<f32>,
    speech_buffer: Vec<f32>,
    pub voiced_samples: usize,
    max_pre_roll_samples: usize,
    silence_threshold_samples: usize,
    min_speech_samples: usize,
}

impl VadProcessor {
    pub fn new(config: VadConfig) -> Self {
        let rate = config.sample_rate.max(1) as usize;
        let max_pre_roll_samples = (rate * config.pre_roll_ms as usize) / 1000;
        let silence_threshold_samples = (rate * config.silence_duration_ms as usize) / 1000;
        let min_speech_samples = (rate * config.min_speech_ms as usize) / 1000;
        Self {
            config,
            state: VadState::Idle,
            pre_roll: VecDeque::with_capacity(max_pre_roll_samples + 512),
            speech_buffer: Vec::new(),
            voiced_samples: 0,
            max_pre_roll_samples,
            silence_threshold_samples,
            min_speech_samples,
        }
    }

    /// Return the current VAD state.
    pub fn state(&self) -> VadState {
        self.state
    }

    /// Number of samples currently buffered in the pre-roll ring buffer.
    pub fn pre_roll_len(&self) -> usize {
        self.pre_roll.len()
    }

    /// Configured pre-roll capacity in samples.
    pub fn max_pre_roll_samples(&self) -> usize {
        self.max_pre_roll_samples
    }

    /// Reset internal buffers and return to [`VadState::Idle`].
    pub fn reset(&mut self) {
        self.state = VadState::Idle;
        self.pre_roll.clear();
        self.speech_buffer.clear();
        self.voiced_samples = 0;
    }

    /// Process a frame of 16kHz mono `f32` PCM samples.
    pub fn process_frame(&mut self, samples: &[f32]) -> VoiceResult<VadEvent> {
        if samples.is_empty() {
            return Ok(VadEvent::None);
        }

        let (_, db) = compute_rms_db(samples);
        let is_speech = db >= self.config.threshold_db;

        match self.state {
            VadState::Idle => {
                if is_speech {
                    self.speech_buffer.clear();
                    self.speech_buffer.extend(self.pre_roll.drain(..));
                    self.speech_buffer.extend_from_slice(samples);
                    self.voiced_samples = samples.len();
                    self.state = VadState::Speaking;
                    Ok(VadEvent::SpeechStarted)
                } else {
                    self.push_pre_roll(samples);
                    Ok(VadEvent::None)
                }
            }
            VadState::Speaking => {
                self.speech_buffer.extend_from_slice(samples);
                if is_speech {
                    self.voiced_samples = self.voiced_samples.saturating_add(samples.len());
                }
                let max_speech_len = 30 * self.config.sample_rate.max(1) as usize;
                if self.speech_buffer.len() >= max_speech_len {
                    return self.finalize_utterance();
                }
                if !is_speech {
                    if self.silence_threshold_samples == 0
                        || samples.len() >= self.silence_threshold_samples
                    {
                        return self.finalize_utterance();
                    }
                    self.state = VadState::SilenceCountdown {
                        silence_samples: samples.len(),
                    };
                }
                Ok(VadEvent::None)
            }
            VadState::SilenceCountdown { silence_samples } => {
                self.speech_buffer.extend_from_slice(samples);
                if is_speech {
                    self.voiced_samples = self.voiced_samples.saturating_add(samples.len());
                    self.state = VadState::Speaking;
                    let max_speech_len = 30 * self.config.sample_rate.max(1) as usize;
                    if self.speech_buffer.len() >= max_speech_len {
                        return self.finalize_utterance();
                    }
                    Ok(VadEvent::None)
                } else {
                    let max_speech_len = 30 * self.config.sample_rate.max(1) as usize;
                    if self.speech_buffer.len() >= max_speech_len {
                        return self.finalize_utterance();
                    }
                    let total_silence = silence_samples.saturating_add(samples.len());
                    if total_silence >= self.silence_threshold_samples {
                        self.finalize_utterance()
                    } else {
                        self.state = VadState::SilenceCountdown {
                            silence_samples: total_silence,
                        };
                        Ok(VadEvent::None)
                    }
                }
            }
        }
    }

    /// Force-flush any in-progress speech segment into WAV bytes.
    pub fn flush(&mut self) -> VoiceResult<Option<Vec<u8>>> {
        if self.state == VadState::Idle || self.voiced_samples < self.min_speech_samples {
            self.reset();
            return Ok(None);
        }
        let wav = encode_wav_i16_mono(&self.speech_buffer, self.config.sample_rate)?;
        self.reset();
        Ok(Some(wav))
    }

    fn push_pre_roll(&mut self, samples: &[f32]) {
        if self.max_pre_roll_samples == 0 {
            return;
        }
        for &s in samples {
            if self.pre_roll.len() >= self.max_pre_roll_samples {
                self.pre_roll.pop_front();
            }
            self.pre_roll.push_back(s);
        }
    }

    fn finalize_utterance(&mut self) -> VoiceResult<VadEvent> {
        let event = if self.voiced_samples >= self.min_speech_samples {
            let wav = encode_wav_i16_mono(&self.speech_buffer, self.config.sample_rate)?;
            VadEvent::SpeechEnded(wav)
        } else {
            VadEvent::None
        };
        self.reset();
        Ok(event)
    }
}

/// Push-to-Talk (PTT) manual recording controller.
pub struct PttController {
    sample_rate: u32,
    active: bool,
    buffer: Vec<f32>,
}

impl PttController {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            active: false,
            buffer: Vec::new(),
        }
    }

    /// Begin capturing audio for Push-to-Talk.
    pub fn press(&mut self) {
        self.buffer.clear();
        self.active = true;
    }

    /// Append audio samples while PTT is held down.
    pub fn push_samples(&mut self, samples: &[f32]) {
        if self.active && !samples.is_empty() {
            self.buffer.extend_from_slice(samples);
        }
    }

    /// Release PTT and encode captured audio into WAV bytes (if non-empty).
    pub fn release(&mut self) -> VoiceResult<Option<Vec<u8>>> {
        self.active = false;
        if self.buffer.is_empty() {
            return Ok(None);
        }
        let wav = encode_wav_i16_mono(&self.buffer, self.sample_rate)?;
        self.buffer.clear();
        Ok(Some(wav))
    }

    /// Returns `true` when PTT is currently pressed.
    pub fn is_active(&self) -> bool {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vad_ignores_short_noise_spikes_despite_long_pre_roll() {
        let config = VadConfig {
            threshold_db: -30.0,
            silence_duration_ms: 800,
            min_speech_ms: 250, // 400 samples at 16kHz
            pre_roll_ms: 1200,   // 19200 samples at 16kHz
            sample_rate: 16000,
        };
        let mut vad = VadProcessor::new(config);

        // Feed 1,200ms of low-level noise (-60 dB) into pre-roll
        let noise_frame = vec![0.001f32; 800]; // about -60 dB
        for _ in 0..24 {
            let ev = vad.process_frame(&noise_frame).unwrap();
            assert_eq!(ev, VadEvent::None);
        }
        assert_eq!(vad.pre_roll_len(), 19200);

        // Feed 50ms (800 samples) of loud frame (-10 dB)
        let loud_frame = vec![0.5f32; 800];
        let ev = vad.process_frame(&loud_frame).unwrap();
        assert_eq!(ev, VadEvent::SpeechStarted);

        // Feed 800ms of silence (-60 dB) in chunks
        for _ in 0..16 {
            let ev = vad.process_frame(&noise_frame).unwrap();
            if let VadEvent::SpeechEnded(_) = ev {
                panic!("Expected VadEvent::None due to short voiced samples (< 250ms), but got SpeechEnded");
            }
        }
    }
}
