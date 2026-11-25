//! Local PC speaker music player powered by `yt-dlp`, `ffmpeg`, and `rodio`
//! running on a dedicated OS audio thread.

use super::queue::{
    extract_track_meta, is_direct_media_url, resolve_audio_stream_url, should_resolve_stream,
    LocalControlEvent, LocalMusicQueue, MusicQueueSnapshot, TrackMeta,
};
use super::MusicError;
use crate::binary_manager::{hidden_tokio_command, BinaryManager};
use rodio::{OutputStream, OutputStreamHandle, Sink, Source};
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const PCM_SAMPLE_RATE: u32 = 48_000;
const PCM_CHANNELS: u16 = 2;

/// Streaming `rodio::Source` consuming interleaved `i16` stereo 48 kHz PCM chunks
/// produced by `ffmpeg -f wav -ac 2 -ar 48000 pipe:1`.
pub struct StreamingPcmSource {
    rx: std_mpsc::Receiver<Vec<i16>>,
    current: VecDeque<i16>,
    finished: bool,
    cancelled: Arc<AtomicBool>,
}

impl StreamingPcmSource {
    pub fn new(rx: std_mpsc::Receiver<Vec<i16>>, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            rx,
            current: VecDeque::new(),
            finished: false,
            cancelled,
        }
    }
}

impl Iterator for StreamingPcmSource {
    type Item = i16;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.cancelled.load(Ordering::Relaxed) || self.finished {
                return None;
            }
            if let Some(sample) = self.current.pop_front() {
                return Some(sample);
            }
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => {
                    if !chunk.is_empty() {
                        self.current.extend(chunk);
                    }
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    self.finished = true;
                    return None;
                }
            }
        }
    }
}

impl Source for StreamingPcmSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        PCM_CHANNELS
    }

    fn sample_rate(&self) -> u32 {
        PCM_SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

enum AudioThreadCmd {
    PlayStream {
        generation: u64,
        rx: std_mpsc::Receiver<Vec<i16>>,
        cancelled: Arc<AtomicBool>,
    },
    Pause,
    Resume,
    Stop,
    SetVolume(f32),
    Shutdown,
}

/// Find the byte offset of the `data` PCM payload inside a WAV header,
/// returning `None` if more bytes are needed.
pub fn find_wav_data_offset(buf: &[u8]) -> Option<usize> {
    if buf.len() < 12 {
        return None;
    }
    if &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Some(0);
    }
    let mut pos = 12usize;
    while pos + 8 <= buf.len() {
        let chunk_id = &buf[pos..pos + 4];
        let chunk_size =
            u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
        if chunk_id == b"data" {
            return Some(pos + 8);
        }
        pos = pos.saturating_add(8).saturating_add(chunk_size);
        if pos % 2 == 1 {
            pos = pos.saturating_add(1);
        }
    }
    None
}

/// Convert little-endian byte pairs into `i16` PCM samples, keeping any trailing odd byte
/// in `carry`.
pub fn decode_s16le_bytes(chunk: &[u8], carry: &mut Option<u8>) -> Vec<i16> {
    let mut bytes = Vec::with_capacity(chunk.len() + 1);
    if let Some(prev) = carry.take() {
        bytes.push(prev);
    }
    bytes.extend_from_slice(chunk);

    if bytes.len() % 2 == 1 {
        *carry = bytes.pop();
    }

    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

struct ActiveStreamTask {
    cancel: CancellationToken,
    pcm_cancelled: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

/// Local PC speaker audio player backed by `ffmpeg` + `rodio` on a dedicated OS thread.
pub struct LocalMusicPlayer {
    queue: Arc<LocalMusicQueue>,
    binaries: Arc<BinaryManager>,
    audio_tx: std_mpsc::Sender<AudioThreadCmd>,
    volume_bits: Arc<AtomicU32>,
    generation: Arc<AtomicU64>,
    has_device: Arc<AtomicBool>,
    active_task: Arc<Mutex<Option<ActiveStreamTask>>>,
    controller_started: AtomicBool,
    shutdown: CancellationToken,
}

impl LocalMusicPlayer {
    /// Create a [`LocalMusicPlayer`] attached to `queue` and `binaries`, spawning the
    /// dedicated OS `rodio` output thread.
    pub fn new(queue: Arc<LocalMusicQueue>, binaries: Arc<BinaryManager>) -> Arc<Self> {
        Self::create_inner(queue, binaries, false)
    }

    /// Create a headless [`LocalMusicPlayer`] for deterministic tests without audio hardware.
    pub fn new_headless(queue: Arc<LocalMusicQueue>, binaries: Arc<BinaryManager>) -> Arc<Self> {
        Self::create_inner(queue, binaries, true)
    }

    fn create_inner(
        queue: Arc<LocalMusicQueue>,
        binaries: Arc<BinaryManager>,
        force_headless: bool,
    ) -> Arc<Self> {
        let (audio_tx, audio_rx) = std_mpsc::channel::<AudioThreadCmd>();
        let volume_bits = Arc::new(AtomicU32::new(1.0_f32.to_bits()));
        let generation = Arc::new(AtomicU64::new(0));
        let has_device = Arc::new(AtomicBool::new(false));
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<u64>();

        let vol_w = Arc::clone(&volume_bits);
        let gen_w = Arc::clone(&generation);
        let dev_w = Arc::clone(&has_device);

        let _ = std::thread::Builder::new()
            .name("jarvis-local-music".into())
            .spawn(move || {
                let output: Option<(OutputStream, OutputStreamHandle)> = if force_headless {
                    None
                } else {
                    match OutputStream::try_default() {
                        Ok(pair) => {
                            dev_w.store(true, Ordering::SeqCst);
                            Some(pair)
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "No audio output device for LocalMusicPlayer; running headless"
                            );
                            None
                        }
                    }
                };

                let mut sink: Option<Sink> = output
                    .as_ref()
                    .and_then(|(_, h)| Sink::try_new(h).ok());
                let mut active_gen: Option<u64> = None;

                loop {
                    match audio_rx.recv_timeout(Duration::from_millis(30)) {
                        Ok(AudioThreadCmd::PlayStream {
                            generation: track_gen,
                            rx,
                            cancelled,
                        }) => {
                            if track_gen != gen_w.load(Ordering::SeqCst) {
                                cancelled.store(true, Ordering::SeqCst);
                                continue;
                            }
                            if let Some(old) = sink.take() {
                                old.stop();
                            }
                            if let Some((_, ref handle)) = output {
                                sink = Sink::try_new(handle).ok();
                                if let Some(ref s) = sink {
                                    s.set_volume(f32::from_bits(vol_w.load(Ordering::SeqCst)));
                                    s.append(StreamingPcmSource::new(rx, cancelled));
                                    s.play();
                                    active_gen = Some(track_gen);
                                }
                            } else {
                                // Headless mode: drain channel on background or keep active_gen
                                active_gen = None;
                            }
                        }
                        Ok(AudioThreadCmd::Pause) => {
                            if let Some(ref s) = sink {
                                s.pause();
                            }
                        }
                        Ok(AudioThreadCmd::Resume) => {
                            if let Some(ref s) = sink {
                                s.play();
                            }
                        }
                        Ok(AudioThreadCmd::Stop) => {
                            active_gen = None;
                            if let Some(old) = sink.take() {
                                old.stop();
                            }
                            if let Some((_, ref handle)) = output {
                                sink = Sink::try_new(handle).ok();
                            }
                        }
                        Ok(AudioThreadCmd::SetVolume(v)) => {
                            if let Some(ref s) = sink {
                                s.set_volume(v);
                            }
                        }
                        Ok(AudioThreadCmd::Shutdown)
                        | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                            if let Some(old) = sink.take() {
                                old.stop();
                            }
                            break;
                        }
                        Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                    }

                    if let Some(track_gen) = active_gen {
                        if sink.as_ref().is_some_and(Sink::empty) {
                            active_gen = None;
                            let _ = done_tx.send(track_gen);
                        }
                    }
                }
            });

        let player = Arc::new(Self {
            queue,
            binaries,
            audio_tx,
            volume_bits,
            generation,
            has_device,
            active_task: Arc::new(Mutex::new(None)),
            controller_started: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
        });

        // Spawn natural track-completion listener if a Tokio runtime is active.
        if tokio::runtime::Handle::try_current().is_ok() {
            let player_done = Arc::clone(&player);
            let cancel_done = player.shutdown.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel_done.cancelled() => break,
                        maybe_gen = done_rx.recv() => {
                            let Some(finished_gen) = maybe_gen else { break };
                            if finished_gen == player_done.generation.load(Ordering::SeqCst) {
                                if let Some(next_track) = player_done.queue.advance_next() {
                                    player_done.start_track_stream(next_track).await;
                                }
                            }
                        }
                    }
                }
            });
        }

        player
    }

    pub fn queue(&self) -> &Arc<LocalMusicQueue> {
        &self.queue
    }

    pub fn has_output_device(&self) -> bool {
        self.has_device.load(Ordering::SeqCst)
    }

    /// Attach this player to [`LocalMusicQueue`] and spawn the async control loop.
    pub fn start_controller(self: &Arc<Self>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if self
            .controller_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<LocalControlEvent>();
        self.queue.set_controller(Some(tx));
        let player = Arc::clone(self);
        let cancel = self.shutdown.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    maybe_ev = rx.recv() => {
                        let Some(ev) = maybe_ev else { break };
                        match ev {
                            LocalControlEvent::TrackStarted(track) => {
                                player.start_track_stream(track).await;
                            }
                            LocalControlEvent::Paused => {
                                let _ = player.audio_tx.send(AudioThreadCmd::Pause);
                            }
                            LocalControlEvent::Resumed => {
                                let _ = player.audio_tx.send(AudioThreadCmd::Resume);
                            }
                            LocalControlEvent::Skipped(next) => {
                                if let Some(track) = next {
                                    player.start_track_stream(track).await;
                                } else {
                                    player.halt_active_stream().await;
                                }
                            }
                            LocalControlEvent::Stopped => {
                                player.halt_active_stream().await;
                            }
                            LocalControlEvent::VolumeChanged(vol) => {
                                player.volume_bits.store(vol.to_bits(), Ordering::SeqCst);
                                let _ = player.audio_tx.send(AudioThreadCmd::SetVolume(vol));
                            }
                        }
                    }
                }
            }
        });
    }

    /// Resolve a search query (`ytsearch1:...`) or URL via `yt-dlp` and enqueue on local playback.
    pub async fn play(
        self: &Arc<Self>,
        query_or_url: &str,
        requested_by: &str,
    ) -> Result<(usize, MusicQueueSnapshot), MusicError> {
        self.start_controller();
        let trimmed = query_or_url.trim();
        if trimmed.is_empty() {
            return Err(MusicError::Queue("Provide a song title or URL".into()));
        }

        let bin_dir = self.binaries.bin_dir();
        let track = match extract_track_meta(trimmed, requested_by, Some(bin_dir)).await {
            Ok(t) => t,
            Err(_) if trimmed.starts_with("http://") || trimmed.starts_with("https://") => {
                TrackMeta {
                    title: trimmed.to_string(),
                    url: trimmed.to_string(),
                    webpage_url: trimmed.to_string(),
                    duration_seconds: 0,
                    requested_by: requested_by.to_string(),
                    thumbnail: None,
                    stream_resolved: is_direct_media_url(trimmed),
                }
            }
            Err(err) => return Err(err),
        };

        Ok(self.queue.enqueue(track))
    }

    pub fn pause(&self) -> Result<MusicQueueSnapshot, MusicError> {
        self.queue.pause()
    }

    pub fn resume(&self) -> Result<MusicQueueSnapshot, MusicError> {
        self.queue.resume()
    }

    pub fn skip(&self) -> Result<(Option<TrackMeta>, Option<TrackMeta>), MusicError> {
        self.queue.skip()
    }

    pub fn stop(&self) -> MusicQueueSnapshot {
        self.queue.stop()
    }

    pub fn set_volume(&self, volume: f32) -> Result<f32, MusicError> {
        self.queue.set_volume(volume)
    }

    pub async fn halt_active_stream(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let _ = self.audio_tx.send(AudioThreadCmd::Stop);
        if let Some(mut old) = self.active_task.lock().await.take() {
            old.pcm_cancelled.store(true, Ordering::SeqCst);
            old.cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(2), &mut old.task)
                .await
                .is_err()
            {
                old.task.abort();
                let _ = old.task.await;
            }
        }
    }

    async fn start_track_stream(self: &Arc<Self>, track: TrackMeta) {
        self.halt_active_stream().await;

        let track_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let cancel = self.shutdown.child_token();
        let pcm_cancelled = Arc::new(AtomicBool::new(false));
        let (pcm_tx, pcm_rx) = std_mpsc::sync_channel::<Vec<i16>>(64);

        let _ = self.audio_tx.send(AudioThreadCmd::PlayStream {
            generation: track_gen,
            rx: pcm_rx,
            cancelled: Arc::clone(&pcm_cancelled),
        });

        let bin_dir = self.binaries.bin_dir().to_path_buf();
        let binaries = Arc::clone(&self.binaries);
        let task_cancel = cancel.clone();
        let task_pcm_cancel = Arc::clone(&pcm_cancelled);
        let queue = Arc::clone(&self.queue);
        let player = Arc::clone(self);
        let gen_ref = Arc::clone(&self.generation);

        let task = tokio::spawn(async move {
            let stream_url = if !should_resolve_stream(&track) {
                track.url.clone()
            } else {
                match resolve_audio_stream_url(&track.url, Some(&bin_dir)).await {
                    Ok(u) => u,
                    Err(err) => {
                        tracing::warn!("Local player failed to resolve '{}': {err}", track.title);
                        task_pcm_cancel.store(true, Ordering::SeqCst);
                        return;
                    }
                }
            };

            let ffmpeg = match binaries.ensure_ffmpeg().await {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!("Local player missing ffmpeg: {err}");
                    task_pcm_cancel.store(true, Ordering::SeqCst);
                    return;
                }
            };

            let mut cmd = hidden_tokio_command(&ffmpeg);
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-reconnect",
                "1",
                "-reconnect_streamed",
                "1",
                "-reconnect_delay_max",
                "5",
                "-i",
                &stream_url,
                "-vn",
                "-f",
                "wav",
                "-ac",
                "2",
                "-ar",
                "48000",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!("Failed to spawn ffmpeg for local playback: {err}");
                    task_pcm_cancel.store(true, Ordering::SeqCst);
                    return;
                }
            };

            let Some(mut stdout) = child.stdout.take() else {
                task_pcm_cancel.store(true, Ordering::SeqCst);
                return;
            };

            let mut buf = [0u8; 8192];
            let mut header_buf = Vec::new();
            let mut header_parsed = false;
            let mut carry: Option<u8> = None;

            loop {
                tokio::select! {
                    _ = task_cancel.cancelled() => {
                        task_pcm_cancel.store(true, Ordering::SeqCst);
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return;
                    }
                    read_res = stdout.read(&mut buf) => {
                        match read_res {
                            Ok(0) => break,
                            Ok(n) => {
                                let slice = &buf[..n];
                                let pcm_bytes: &[u8] = if !header_parsed {
                                    header_buf.extend_from_slice(slice);
                                    if let Some(offset) = find_wav_data_offset(&header_buf) {
                                        header_parsed = true;
                                        &header_buf[offset..]
                                    } else if header_buf.len() > 4096 {
                                        header_parsed = true;
                                        &header_buf[..]
                                    } else {
                                        continue;
                                    }
                                } else {
                                    slice
                                };

                                if !pcm_bytes.is_empty() {
                                    let samples = decode_s16le_bytes(pcm_bytes, &mut carry);
                                    if !samples.is_empty() && pcm_tx.send(samples).is_err() {
                                        break;
                                    }
                                }
                                if header_parsed && !header_buf.is_empty() {
                                    header_buf.clear();
                                }
                            }
                            Err(err) => {
                                tracing::warn!("Local ffmpeg stdout read error: {err}");
                                break;
                            }
                        }
                    }
                }
            }

            drop(pcm_tx);
            let _ = child.wait().await;

            // In headless mode (no audio device), advance automatically once EOF is reached.
            if !player.has_output_device()
                && !task_cancel.is_cancelled()
                && gen_ref.load(Ordering::SeqCst) == track_gen
            {
                let _ = queue.advance_next();
            }
        });

        *self.active_task.lock().await = Some(ActiveStreamTask {
            cancel,
            pcm_cancelled,
            task,
        });
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
        let _ = self.audio_tx.send(AudioThreadCmd::Shutdown);
    }
}

impl Drop for LocalMusicPlayer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        let _ = self.audio_tx.send(AudioThreadCmd::Shutdown);
    }
}
