//! Microphone capture, device discovery, resampling, and real-time RMS/dB metering via `cpal`.

use super::{VoiceError, VoiceResult};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Target sample rate for STT and VAD pipelines (16 kHz).
pub const TARGET_SAMPLE_RATE: u32 = 16_000;
/// Target channel count for STT and VAD pipelines (mono).
pub const TARGET_CHANNELS: u16 = 1;
/// Interval for ~30fps RMS dB level callbacks (~33.3 ms).
const LEVEL_EMIT_INTERVAL: Duration = Duration::from_millis(33);

/// Callback invoked at ~30fps with the current microphone RMS level in dBFS.
pub type MicLevelCallback = Arc<dyn Fn(f32) + Send + Sync>;

/// Metadata describing an audio input or output device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub is_default: bool,
    pub channels: u16,
    pub sample_rate: u32,
}

/// Compute root-mean-square (RMS) energy of normalized `f32` PCM samples.
///
/// Returns `0.0` for an empty slice.
pub fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

/// Convert linear RMS amplitude to decibels relative to full scale (dBFS).
///
/// Clamps the floor at `1e-9` (`-180.0` dBFS) to avoid `-inf` or `NaN`.
pub fn rms_to_db(rms: f32) -> f32 {
    20.0 * rms.max(1e-9).log10()
}

/// Compute both linear RMS energy and dBFS for a slice of `f32` samples.
pub fn compute_rms_db(samples: &[f32]) -> (f32, f32) {
    let rms = compute_rms(samples);
    (rms, rms_to_db(rms))
}

/// Downmix multi-channel interleaved `f32` PCM to mono and linearly resample to `dst_rate`.
pub fn resample_linear_mono(
    samples: &[f32],
    src_rate: u32,
    src_channels: u16,
    dst_rate: u32,
) -> Vec<f32> {
    if samples.is_empty() || src_channels == 0 || src_rate == 0 || dst_rate == 0 {
        return Vec::new();
    }

    let ch = src_channels as usize;
    let mono: Vec<f32> = if ch == 1 {
        samples.to_vec()
    } else {
        samples
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    };

    if mono.is_empty() || src_rate == dst_rate {
        return mono;
    }

    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = ((mono.len() as f64) / ratio).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let s0 = mono[idx.min(mono.len() - 1)];
        let s1 = mono[(idx + 1).min(mono.len() - 1)];
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}

/// List available audio input devices.
///
/// Returns an empty list gracefully if no input devices or audio host are available.
pub fn list_input_devices() -> VoiceResult<Vec<AudioDeviceInfo>> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());

    let devices = match host.input_devices() {
        Ok(devs) => devs,
        Err(err) => {
            tracing::warn!(error = %err, "Failed to enumerate input devices; returning empty list");
            return Ok(Vec::new());
        }
    };

    let mut result = Vec::new();
    for dev in devices {
        let Ok(name) = dev.name() else { continue };
        let (channels, sample_rate) = dev
            .default_input_config()
            .map(|c| (c.channels(), c.sample_rate().0))
            .unwrap_or((1, TARGET_SAMPLE_RATE));
        let is_default = default_name.as_deref() == Some(name.as_str());
        result.push(AudioDeviceInfo {
            name,
            is_default,
            channels,
            sample_rate,
        });
    }
    Ok(result)
}

/// List available audio output devices.
///
/// Returns an empty list gracefully if no output devices or audio host are available.
pub fn list_output_devices() -> VoiceResult<Vec<AudioDeviceInfo>> {
    let host = cpal::default_host();
    let default_name = host.default_output_device().and_then(|d| d.name().ok());

    let devices = match host.output_devices() {
        Ok(devs) => devs,
        Err(err) => {
            tracing::warn!(error = %err, "Failed to enumerate output devices; returning empty list");
            return Ok(Vec::new());
        }
    };

    let mut result = Vec::new();
    for dev in devices {
        let Ok(name) = dev.name() else { continue };
        let (channels, sample_rate) = dev
            .default_output_config()
            .map(|c| (c.channels(), c.sample_rate().0))
            .unwrap_or((2, 44_100));
        let is_default = default_name.as_deref() == Some(name.as_str());
        result.push(AudioDeviceInfo {
            name,
            is_default,
            channels,
            sample_rate,
        });
    }
    Ok(result)
}

/// Thread-safe handle to an active `cpal` microphone capture stream.
///
/// Isolates `cpal::Stream` (which is `!Send`/`!Sync` on some platforms) onto a dedicated
/// OS thread communicating via [`UnboundedSender`] and [`AtomicBool`].
pub struct MicCapture {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MicCapture {
    /// Start capturing 16kHz mono PCM audio from the specified (or default) input device.
    pub fn start(
        device_name: Option<&str>,
        level_callback: Option<MicLevelCallback>,
        sample_tx: Option<UnboundedSender<Vec<f32>>>,
    ) -> VoiceResult<Self> {
        let host = cpal::default_host();
        let device = if let Some(target_name) = device_name.filter(|s| !s.trim().is_empty()) {
            let target_lower = target_name.to_ascii_lowercase();
            host.input_devices()
                .map_err(|e| VoiceError::AudioDevice(e.to_string()))?
                .find(|d| {
                    d.name()
                        .ok()
                        .is_some_and(|n| n == target_name || n.to_ascii_lowercase() == target_lower)
                })
                .ok_or_else(|| {
                    VoiceError::AudioDevice(format!("Input device '{target_name}' not found"))
                })?
        } else {
            host.default_input_device().ok_or_else(|| {
                VoiceError::AudioDevice("No default microphone input device found".into())
            })?
        };

        let default_cfg = device
            .default_input_config()
            .map_err(|e| VoiceError::AudioDevice(e.to_string()))?;

        let sample_format = default_cfg.sample_format();
        let stream_config: StreamConfig = default_cfg.into();
        let src_rate = stream_config.sample_rate.0;
        let src_channels = stream_config.channels;

        let running = Arc::new(AtomicBool::new(true));
        let running_thread = Arc::clone(&running);
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<VoiceResult<()>>(1);

        let worker = std::thread::Builder::new()
            .name("jarvis-mic-capture".into())
            .spawn(move || {
                let mut level_acc: Vec<f32> = Vec::with_capacity(1024);
                let mut last_emit = Instant::now();

                let mut handle_mono_f32 = move |raw: &[f32]| {
                    let mono_16k =
                        resample_linear_mono(raw, src_rate, src_channels, TARGET_SAMPLE_RATE);
                    if mono_16k.is_empty() {
                        return;
                    }
                    if let Some(cb) = &level_callback {
                        level_acc.extend_from_slice(&mono_16k);
                        if last_emit.elapsed() >= LEVEL_EMIT_INTERVAL {
                            let (_, db) = compute_rms_db(&level_acc);
                            cb(db);
                            level_acc.clear();
                            last_emit = Instant::now();
                        }
                    }
                    if let Some(tx) = &sample_tx {
                        let _ = tx.send(mono_16k);
                    }
                };

                let err_fn = |err: cpal::StreamError| {
                    tracing::error!(error = %err, "cpal input stream error");
                };

                let stream_res = match sample_format {
                    SampleFormat::F32 => device.build_input_stream(
                        &stream_config,
                        move |data: &[f32], _| handle_mono_f32(data),
                        err_fn,
                        None,
                    ),
                    SampleFormat::I16 => device.build_input_stream(
                        &stream_config,
                        move |data: &[i16], _| {
                            let converted: Vec<f32> =
                                data.iter().map(|&s| s as f32 / 32768.0).collect();
                            handle_mono_f32(&converted);
                        },
                        err_fn,
                        None,
                    ),
                    SampleFormat::U16 => device.build_input_stream(
                        &stream_config,
                        move |data: &[u16], _| {
                            let converted: Vec<f32> = data
                                .iter()
                                .map(|&s| (s as f32 - 32768.0) / 32768.0)
                                .collect();
                            handle_mono_f32(&converted);
                        },
                        err_fn,
                        None,
                    ),
                    other => {
                        let _ = init_tx.send(Err(VoiceError::AudioStream(format!(
                            "Unsupported sample format: {other:?}"
                        ))));
                        return;
                    }
                };

                let stream = match stream_res {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = init_tx.send(Err(VoiceError::AudioStream(e.to_string())));
                        return;
                    }
                };

                if let Err(e) = stream.play() {
                    let _ = init_tx.send(Err(VoiceError::AudioStream(e.to_string())));
                    return;
                }

                let _ = init_tx.send(Ok(()));

                while running_thread.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                drop(stream);
            })
            .map_err(|e| VoiceError::AudioStream(e.to_string()))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                running,
                worker: Some(worker),
            }),
            Ok(Err(err)) => {
                running.store(false, Ordering::Relaxed);
                let _ = worker.join();
                Err(err)
            }
            Err(err) => {
                running.store(false, Ordering::Relaxed);
                let _ = worker.join();
                Err(VoiceError::AudioStream(err.to_string()))
            }
        }
    }

    /// Stop capturing audio and join the capture thread.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }

    /// Returns `true` while the capture loop is active.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
