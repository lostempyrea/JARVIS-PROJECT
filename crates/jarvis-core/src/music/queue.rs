//! Unified music queue state, snapshots, and `yt-dlp` metadata/stream resolution.

use crate::binary_manager::{hidden_tokio_command, locate_binary};
use crate::events::{CoreEvent, EventBus};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use tokio::sync::mpsc;

use super::MusicError;

/// Audio playback destination for [`crate::music::DualMusicController`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MusicTarget {
    #[default]
    Local,
    DiscordVoice,
    Both,
}

/// Playback state of a local or guild music queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// Metadata for a queued or playing audio track.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackMeta {
    pub title: String,
    pub url: String,
    pub webpage_url: String,
    pub duration_seconds: u64,
    pub requested_by: String,
    pub thumbnail: Option<String>,
    /// Whether `url` is already a resolved direct audio stream (skipping `yt-dlp -g`).
    #[serde(default)]
    pub stream_resolved: bool,
}

/// Unified snapshot of local or Discord guild playback state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MusicQueueSnapshot {
    pub target: MusicTarget,
    pub guild_id: Option<u64>,
    pub connected_channel_id: Option<u64>,
    pub state: PlaybackState,
    pub volume: f32,
    pub current: Option<TrackMeta>,
    pub upcoming: Vec<TrackMeta>,
}

/// Control events emitted by [`LocalMusicQueue`] to [`crate::music::LocalMusicPlayer`].
#[derive(Debug, Clone)]
pub enum LocalControlEvent {
    TrackStarted(TrackMeta),
    Paused,
    Resumed,
    Skipped(Option<TrackMeta>),
    Stopped,
    VolumeChanged(f32),
}

/// Control events emitted by [`GuildMusicQueue`] to [`crate::music::DiscordVoiceManager`].
#[derive(Debug, Clone)]
pub enum VoiceControlEvent {
    JoinRequested(u64, u64),
    LeaveRequested(u64),
    TrackStarted(u64, TrackMeta),
    Paused(u64),
    Resumed(u64),
    Skipped(u64, Option<TrackMeta>),
    Stopped(u64),
    VolumeChanged(u64, f32),
}

#[derive(Debug, Clone)]
struct QueueInnerState {
    connected_channel_id: Option<u64>,
    state: PlaybackState,
    volume: f32,
    current: Option<TrackMeta>,
    queue: VecDeque<TrackMeta>,
}

impl Default for QueueInnerState {
    fn default() -> Self {
        Self {
            connected_channel_id: None,
            state: PlaybackState::Stopped,
            volume: 1.0,
            current: None,
            queue: VecDeque::new(),
        }
    }
}

impl QueueInnerState {
    fn snapshot(&self, target: MusicTarget, guild_id: Option<u64>) -> MusicQueueSnapshot {
        MusicQueueSnapshot {
            target,
            guild_id,
            connected_channel_id: self.connected_channel_id,
            state: self.state,
            volume: self.volume,
            current: self.current.clone(),
            upcoming: self.queue.iter().cloned().collect(),
        }
    }
}

/// Thread-safe local PC speaker music queue.
#[derive(Default)]
pub struct LocalMusicQueue {
    state: Mutex<QueueInnerState>,
    control_tx: Mutex<Option<mpsc::UnboundedSender<LocalControlEvent>>>,
    event_bus: Option<EventBus>,
}

impl LocalMusicQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_event_bus(event_bus: EventBus) -> Self {
        Self {
            state: Mutex::new(QueueInnerState::default()),
            control_tx: Mutex::new(None),
            event_bus: Some(event_bus),
        }
    }

    pub fn set_controller(&self, tx: Option<mpsc::UnboundedSender<LocalControlEvent>>) {
        *self
            .control_tx
            .lock()
            .expect("local_control_tx mutex poisoned") = tx;
    }

    fn emit_control(&self, ev: LocalControlEvent) {
        let tx = self
            .control_tx
            .lock()
            .expect("local_control_tx mutex poisoned")
            .clone();
        if let Some(sender) = tx {
            let _ = sender.send(ev);
        }
    }

    fn publish_snapshot(&self, snap: &MusicQueueSnapshot) {
        if let Some(bus) = &self.event_bus {
            bus.publish(CoreEvent::MusicStateUpdated(snap.clone()));
        }
    }

    /// Enqueue a track. Position `0` means it started playing immediately.
    pub fn enqueue(&self, track: TrackMeta) -> (usize, MusicQueueSnapshot) {
        let (pos, snap, started) = {
            let mut guard = self.state.lock().expect("local music mutex poisoned");
            let (pos, started) = if guard.current.is_none() {
                guard.current = Some(track.clone());
                guard.state = PlaybackState::Playing;
                (0, Some(track))
            } else {
                guard.queue.push_back(track);
                (guard.queue.len(), None)
            };
            (pos, guard.snapshot(MusicTarget::Local, None), started)
        };
        if let Some(trk) = started {
            self.emit_control(LocalControlEvent::TrackStarted(trk));
        }
        self.publish_snapshot(&snap);
        (pos, snap)
    }

    /// Advance automatically to the next track when the active track finishes naturally.
    pub fn advance_next(&self) -> Option<TrackMeta> {
        let (next, snap) = {
            let mut guard = self.state.lock().expect("local music mutex poisoned");
            let next = guard.queue.pop_front();
            guard.current = next.clone();
            guard.state = if guard.current.is_some() {
                PlaybackState::Playing
            } else {
                PlaybackState::Stopped
            };
            (next, guard.snapshot(MusicTarget::Local, None))
        };
        self.publish_snapshot(&snap);
        next
    }

    pub fn skip(&self) -> Result<(Option<TrackMeta>, Option<TrackMeta>), MusicError> {
        let (skipped, next, snap) = {
            let mut guard = self.state.lock().expect("local music mutex poisoned");
            let skipped = guard.current.take();
            if skipped.is_none() && guard.queue.is_empty() {
                return Err(MusicError::Queue("Local music queue is empty".into()));
            }
            let next = guard.queue.pop_front();
            guard.current = next.clone();
            guard.state = if guard.current.is_some() {
                PlaybackState::Playing
            } else {
                PlaybackState::Stopped
            };
            (skipped, next, guard.snapshot(MusicTarget::Local, None))
        };
        self.emit_control(LocalControlEvent::Skipped(next.clone()));
        self.publish_snapshot(&snap);
        Ok((skipped, next))
    }

    pub fn pause(&self) -> Result<MusicQueueSnapshot, MusicError> {
        let snap = {
            let mut guard = self.state.lock().expect("local music mutex poisoned");
            if guard.current.is_none() {
                return Err(MusicError::Queue("Nothing is playing locally".into()));
            }
            guard.state = PlaybackState::Paused;
            guard.snapshot(MusicTarget::Local, None)
        };
        self.emit_control(LocalControlEvent::Paused);
        self.publish_snapshot(&snap);
        Ok(snap)
    }

    pub fn resume(&self) -> Result<MusicQueueSnapshot, MusicError> {
        let snap = {
            let mut guard = self.state.lock().expect("local music mutex poisoned");
            if guard.current.is_none() {
                return Err(MusicError::Queue("Nothing is in the local player".into()));
            }
            guard.state = PlaybackState::Playing;
            guard.snapshot(MusicTarget::Local, None)
        };
        self.emit_control(LocalControlEvent::Resumed);
        self.publish_snapshot(&snap);
        Ok(snap)
    }

    pub fn stop(&self) -> MusicQueueSnapshot {
        let snap = {
            let mut guard = self.state.lock().expect("local music mutex poisoned");
            guard.current = None;
            guard.queue.clear();
            guard.state = PlaybackState::Stopped;
            guard.snapshot(MusicTarget::Local, None)
        };
        self.emit_control(LocalControlEvent::Stopped);
        self.publish_snapshot(&snap);
        snap
    }

    /// Set local playback volume in `0.0..=2.0`.
    pub fn set_volume(&self, volume: f32) -> Result<f32, MusicError> {
        if !(0.0..=2.0).contains(&volume) || volume.is_nan() {
            return Err(MusicError::Queue(format!(
                "Volume must be between 0.0 and 2.0 (got {volume})"
            )));
        }
        let snap = {
            let mut guard = self.state.lock().expect("local music mutex poisoned");
            guard.volume = volume;
            guard.snapshot(MusicTarget::Local, None)
        };
        self.emit_control(LocalControlEvent::VolumeChanged(volume));
        self.publish_snapshot(&snap);
        Ok(volume)
    }

    pub fn now_playing(&self) -> Option<TrackMeta> {
        self.state
            .lock()
            .expect("local music mutex poisoned")
            .current
            .clone()
    }

    pub fn snapshot(&self) -> MusicQueueSnapshot {
        self.state
            .lock()
            .expect("local music mutex poisoned")
            .snapshot(MusicTarget::Local, None)
    }
}

/// Thread-safe per-guild Discord voice music queue manager.
#[derive(Default)]
pub struct GuildMusicQueue {
    guilds: Mutex<HashMap<u64, QueueInnerState>>,
    voice_tx: Mutex<Option<mpsc::UnboundedSender<VoiceControlEvent>>>,
    event_bus: Option<EventBus>,
}

impl GuildMusicQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_event_bus(event_bus: EventBus) -> Self {
        Self {
            guilds: Mutex::new(HashMap::new()),
            voice_tx: Mutex::new(None),
            event_bus: Some(event_bus),
        }
    }

    pub fn set_voice_controller(&self, tx: Option<mpsc::UnboundedSender<VoiceControlEvent>>) {
        *self.voice_tx.lock().expect("voice_tx mutex poisoned") = tx;
    }

    fn emit(&self, ev: VoiceControlEvent) {
        let tx = self.voice_tx.lock().expect("voice_tx mutex poisoned").clone();
        if let Some(sender) = tx {
            let _ = sender.send(ev);
        }
    }

    fn publish_snapshot(&self, snap: &MusicQueueSnapshot) {
        if let Some(bus) = &self.event_bus {
            bus.publish(CoreEvent::MusicStateUpdated(snap.clone()));
        }
    }

    pub fn join_voice(&self, guild_id: u64, channel_id: u64) -> MusicQueueSnapshot {
        let snap = {
            let mut guard = self.guilds.lock().expect("music mutex poisoned");
            let entry = guard.entry(guild_id).or_default();
            entry.connected_channel_id = Some(channel_id);
            entry.snapshot(MusicTarget::DiscordVoice, Some(guild_id))
        };
        self.emit(VoiceControlEvent::JoinRequested(guild_id, channel_id));
        self.publish_snapshot(&snap);
        snap
    }

    pub fn leave_voice(&self, guild_id: u64) -> MusicQueueSnapshot {
        let snap = {
            let mut guard = self.guilds.lock().expect("music mutex poisoned");
            let entry = guard.entry(guild_id).or_default();
            entry.connected_channel_id = None;
            entry.state = PlaybackState::Stopped;
            entry.current = None;
            entry.queue.clear();
            entry.snapshot(MusicTarget::DiscordVoice, Some(guild_id))
        };
        self.emit(VoiceControlEvent::LeaveRequested(guild_id));
        self.publish_snapshot(&snap);
        snap
    }

    /// Enqueue a track. Position `0` means it started playing immediately.
    pub fn enqueue(&self, guild_id: u64, track: TrackMeta) -> (usize, MusicQueueSnapshot) {
        let (pos, snap, started) = {
            let mut guard = self.guilds.lock().expect("music mutex poisoned");
            let entry = guard.entry(guild_id).or_default();
            let (pos, started) = if entry.current.is_none() {
                entry.current = Some(track.clone());
                entry.state = PlaybackState::Playing;
                (0, Some(track))
            } else {
                entry.queue.push_back(track);
                (entry.queue.len(), None)
            };
            (
                pos,
                entry.snapshot(MusicTarget::DiscordVoice, Some(guild_id)),
                started,
            )
        };
        if let Some(trk) = started {
            self.emit(VoiceControlEvent::TrackStarted(guild_id, trk));
        }
        self.publish_snapshot(&snap);
        (pos, snap)
    }

    /// Advance automatically to the next queued track when the current track finishes naturally.
    pub fn advance_next(&self, guild_id: u64) -> Option<TrackMeta> {
        let (next, snap) = {
            let mut guard = self.guilds.lock().expect("music mutex poisoned");
            let entry = guard.get_mut(&guild_id)?;
            let next = entry.queue.pop_front();
            entry.current = next.clone();
            entry.state = if entry.current.is_some() {
                PlaybackState::Playing
            } else {
                PlaybackState::Stopped
            };
            (next, entry.snapshot(MusicTarget::DiscordVoice, Some(guild_id)))
        };
        self.publish_snapshot(&snap);
        next
    }

    pub fn skip(
        &self,
        guild_id: u64,
    ) -> Result<(Option<TrackMeta>, Option<TrackMeta>), MusicError> {
        let (skipped, next, snap) = {
            let mut guard = self.guilds.lock().expect("music mutex poisoned");
            let entry = guard
                .get_mut(&guild_id)
                .ok_or_else(|| MusicError::Queue("No active music session in this guild".into()))?;
            let skipped = entry.current.take();
            if skipped.is_none() && entry.queue.is_empty() {
                return Err(MusicError::Queue("Queue is empty".into()));
            }
            let next = entry.queue.pop_front();
            entry.current = next.clone();
            entry.state = if entry.current.is_some() {
                PlaybackState::Playing
            } else {
                PlaybackState::Stopped
            };
            (
                skipped,
                next,
                entry.snapshot(MusicTarget::DiscordVoice, Some(guild_id)),
            )
        };
        self.emit(VoiceControlEvent::Skipped(guild_id, next.clone()));
        self.publish_snapshot(&snap);
        Ok((skipped, next))
    }

    pub fn pause(&self, guild_id: u64) -> Result<MusicQueueSnapshot, MusicError> {
        let snap = {
            let mut guard = self.guilds.lock().expect("music mutex poisoned");
            let entry = guard
                .get_mut(&guild_id)
                .ok_or_else(|| MusicError::Queue("Nothing is playing".into()))?;
            if entry.current.is_none() {
                return Err(MusicError::Queue("Nothing is playing".into()));
            }
            entry.state = PlaybackState::Paused;
            entry.snapshot(MusicTarget::DiscordVoice, Some(guild_id))
        };
        self.emit(VoiceControlEvent::Paused(guild_id));
        self.publish_snapshot(&snap);
        Ok(snap)
    }

    pub fn resume(&self, guild_id: u64) -> Result<MusicQueueSnapshot, MusicError> {
        let snap = {
            let mut guard = self.guilds.lock().expect("music mutex poisoned");
            let entry = guard
                .get_mut(&guild_id)
                .ok_or_else(|| MusicError::Queue("Nothing is paused".into()))?;
            if entry.current.is_none() {
                return Err(MusicError::Queue("Nothing is in the player".into()));
            }
            entry.state = PlaybackState::Playing;
            entry.snapshot(MusicTarget::DiscordVoice, Some(guild_id))
        };
        self.emit(VoiceControlEvent::Resumed(guild_id));
        self.publish_snapshot(&snap);
        Ok(snap)
    }

    pub fn stop(&self, guild_id: u64) -> MusicQueueSnapshot {
        let snap = {
            let mut guard = self.guilds.lock().expect("music mutex poisoned");
            let entry = guard.entry(guild_id).or_default();
            entry.current = None;
            entry.queue.clear();
            entry.state = PlaybackState::Stopped;
            entry.snapshot(MusicTarget::DiscordVoice, Some(guild_id))
        };
        self.emit(VoiceControlEvent::Stopped(guild_id));
        self.publish_snapshot(&snap);
        snap
    }

    /// Set playback volume in `0.0..=2.0`.
    pub fn set_volume(&self, guild_id: u64, volume: f32) -> Result<f32, MusicError> {
        if !(0.0..=2.0).contains(&volume) || volume.is_nan() {
            return Err(MusicError::Queue(format!(
                "Volume must be between 0.0 and 2.0 (got {volume})"
            )));
        }
        let snap = {
            let mut guard = self.guilds.lock().expect("music mutex poisoned");
            let entry = guard.entry(guild_id).or_default();
            entry.volume = volume;
            entry.snapshot(MusicTarget::DiscordVoice, Some(guild_id))
        };
        self.emit(VoiceControlEvent::VolumeChanged(guild_id, volume));
        self.publish_snapshot(&snap);
        Ok(volume)
    }

    pub fn now_playing(&self, guild_id: u64) -> Option<TrackMeta> {
        let guard = self.guilds.lock().expect("music mutex poisoned");
        guard.get(&guild_id).and_then(|g| g.current.clone())
    }

    pub fn active_voice_channel(&self, preferred_guild_id: Option<u64>) -> Option<String> {
        let guard = self.guilds.lock().expect("music mutex poisoned");
        if let Some(gid) = preferred_guild_id {
            if let Some(cid) = guard.get(&gid).and_then(|g| g.connected_channel_id) {
                return Some(cid.to_string());
            }
        }
        guard
            .values()
            .find_map(|g| g.connected_channel_id)
            .map(|cid| cid.to_string())
    }

    pub fn snapshot(&self, guild_id: u64) -> MusicQueueSnapshot {
        let guard = self.guilds.lock().expect("music mutex poisoned");
        match guard.get(&guild_id) {
            Some(state) => state.snapshot(MusicTarget::DiscordVoice, Some(guild_id)),
            None => QueueInnerState::default().snapshot(MusicTarget::DiscordVoice, Some(guild_id)),
        }
    }
}

/// Return `true` when `url` points directly to a known audio media file extension.
pub fn is_direct_media_url(url: &str) -> bool {
    let trimmed = url.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return false;
    }
    let path_part = trimmed.split(&['?', '#'][..]).next().unwrap_or(trimmed);
    let lower = path_part.to_ascii_lowercase();
    [".mp3", ".ogg", ".opus", ".wav", ".flac", ".m4a", ".aac"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Return `true` if `track.url` still needs resolution via `yt-dlp -f bestaudio/best -g`.
pub fn should_resolve_stream(track: &TrackMeta) -> bool {
    !track.stream_resolved && !is_direct_media_url(&track.url)
}

/// Parse `yt-dlp --dump-json` output into a [`TrackMeta`].
pub fn parse_yt_dlp_json(raw_json: &str, requested_by: &str) -> Result<TrackMeta, MusicError> {
    let first_line = raw_json
        .lines()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| MusicError::YtDlp("Empty yt-dlp JSON output".into()))?;
    let val: serde_json::Value = serde_json::from_str(first_line)?;

    let title = val["title"]
        .as_str()
        .unwrap_or("Unknown Track")
        .to_string();
    let webpage_url = val["webpage_url"]
        .as_str()
        .or_else(|| val["original_url"].as_str())
        .unwrap_or_default()
        .to_string();
    let has_direct_url = val["url"].as_str().is_some();
    let url = val["url"].as_str().unwrap_or(&webpage_url).to_string();
    let duration_seconds = val["duration"]
        .as_f64()
        .map(|d| d.max(0.0) as u64)
        .or_else(|| val["duration"].as_u64())
        .unwrap_or(0);
    let thumbnail = val["thumbnail"].as_str().map(ToString::to_string);

    Ok(TrackMeta {
        title,
        url: url.clone(),
        webpage_url,
        duration_seconds,
        requested_by: requested_by.to_string(),
        thumbnail,
        stream_resolved: has_direct_url || is_direct_media_url(&url),
    })
}

/// Extract track metadata using `yt-dlp -f bestaudio/best --dump-json` (`CREATE_NO_WINDOW`).
pub async fn extract_track_meta(
    query: &str,
    requested_by: &str,
    bin_dir: Option<&Path>,
) -> Result<TrackMeta, MusicError> {
    let yt_dlp = locate_binary("yt-dlp", bin_dir).ok_or_else(|| {
        MusicError::BinaryMissing(
            "yt-dlp not found. Install yt-dlp into %APPDATA%\\Jarvis\\bin\\ or PATH.".into(),
        )
    })?;

    let target = if query.starts_with("http://") || query.starts_with("https://") {
        query.to_string()
    } else {
        format!("ytsearch1:{query}")
    };

    let output = hidden_tokio_command(&yt_dlp)
        .args([
            "-f",
            "bestaudio/best",
            "--dump-json",
            "--no-playlist",
            "--no-warnings",
            &target,
        ])
        .stdin(Stdio::null())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MusicError::YtDlp(format!(
            "yt-dlp failed ({}): {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_yt_dlp_json(&stdout, requested_by)
}

/// Resolve a direct playable audio stream URL using `yt-dlp -f bestaudio/best -g` (`CREATE_NO_WINDOW`).
pub async fn resolve_audio_stream_url(
    query_or_url: &str,
    bin_dir: Option<&Path>,
) -> Result<String, MusicError> {
    let yt_dlp = locate_binary("yt-dlp", bin_dir).ok_or_else(|| {
        MusicError::BinaryMissing(
            "yt-dlp not found. Install yt-dlp into %APPDATA%\\Jarvis\\bin\\ or PATH.".into(),
        )
    })?;

    let target = if query_or_url.starts_with("http://") || query_or_url.starts_with("https://") {
        query_or_url.to_string()
    } else {
        format!("ytsearch1:{query_or_url}")
    };

    let output = hidden_tokio_command(&yt_dlp)
        .args([
            "-f",
            "bestaudio/best",
            "-g",
            "--no-playlist",
            "--no-warnings",
            &target,
        ])
        .stdin(Stdio::null())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MusicError::YtDlp(format!(
            "yt-dlp -g failed ({}): {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let url = stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| MusicError::YtDlp("yt-dlp -g returned no stream URL".into()))?;
    Ok(url.to_string())
}
