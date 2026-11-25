//! Dual Music Engine (Phase 3):
//! - [`queue`]: Unified `TrackMeta`, `MusicTarget`, `PlaybackState`, `MusicQueueSnapshot`, `LocalMusicQueue`, `GuildMusicQueue`
//! - [`local_player`]: `yt-dlp` + `ffmpeg` + `rodio` local PC speaker streaming player
//! - [`discord_voice`]: Pure-Rust Discord Voice Gateway v8 + UDP RTP + `davey` DAVE E2EE + `chacha20poly1305`
//! - [`DualMusicController`] & [`MusicControlTool`] (`music_control` tool)

pub mod discord_voice;
pub mod local_player;
pub mod queue;

pub use discord_voice::{
    build_ip_discovery_packet, build_rtp_header, build_voice_heartbeat_payload,
    build_voice_identify_payload, demux_ogg_opus_slice, encode_dave_commit_welcome,
    encode_dave_key_package, encrypt_opus_rtp_packet, parse_ip_discovery_response,
    DaveGatewayState, DiscordVoiceManager, OggOpusDemuxer, OPUS_FRAME_DURATION_MS,
    OPUS_SAMPLES_PER_FRAME, OPUS_SILENCE_FRAME,
};
pub use local_player::{decode_s16le_bytes, find_wav_data_offset, LocalMusicPlayer};
pub use queue::{
    extract_track_meta, is_direct_media_url, parse_yt_dlp_json, resolve_audio_stream_url,
    should_resolve_stream, GuildMusicQueue, LocalControlEvent, LocalMusicQueue, MusicQueueSnapshot,
    MusicTarget, PlaybackState, TrackMeta, VoiceControlEvent,
};

use crate::binary_manager::{BinaryError, BinaryManager};
use crate::config::AppConfig;
use crate::events::EventBus;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::{Arc, RwLock};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Pair of `(skipped_track, next_track)` returned by queue skip operations.
pub type SkipOutcome = (Option<TrackMeta>, Option<TrackMeta>);

/// Errors produced by the dual music subsystem.
#[derive(Debug, Error)]
pub enum MusicError {
    #[error("music queue error: {0}")]
    Queue(String),
    #[error("missing required binary: {0}")]
    BinaryMissing(String),
    #[error("binary manager error: {0}")]
    Binary(#[from] BinaryError),
    #[error("yt-dlp error: {0}")]
    YtDlp(String),
    #[error("voice error: {0}")]
    Voice(String),
    #[error("not connected: {0}")]
    NotConnected(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Coordinates local speaker playback ([`LocalMusicPlayer`]) and Discord voice playback
/// ([`DiscordVoiceManager`]) under a unified API.
pub struct DualMusicController {
    config: Arc<RwLock<AppConfig>>,
    binaries: Arc<BinaryManager>,
    local_queue: Arc<LocalMusicQueue>,
    local_player: Arc<LocalMusicPlayer>,
    guild_queue: Arc<GuildMusicQueue>,
    voice_manager: Arc<DiscordVoiceManager>,
}

impl DualMusicController {
    /// Construct a new [`DualMusicController`] with real audio output.
    pub fn new(
        config: Arc<RwLock<AppConfig>>,
        binaries: Arc<BinaryManager>,
        event_bus: EventBus,
    ) -> Arc<Self> {
        Self::build(config, binaries, event_bus, false)
    }

    /// Construct a headless [`DualMusicController`] (ideal for unit tests).
    pub fn new_headless(
        config: Arc<RwLock<AppConfig>>,
        binaries: Arc<BinaryManager>,
        event_bus: EventBus,
    ) -> Arc<Self> {
        Self::build(config, binaries, event_bus, true)
    }

    fn build(
        config: Arc<RwLock<AppConfig>>,
        binaries: Arc<BinaryManager>,
        event_bus: EventBus,
        headless: bool,
    ) -> Arc<Self> {
        let local_queue = Arc::new(LocalMusicQueue::with_event_bus(event_bus.clone()));
        let local_player = if headless {
            LocalMusicPlayer::new_headless(Arc::clone(&local_queue), Arc::clone(&binaries))
        } else {
            LocalMusicPlayer::new(Arc::clone(&local_queue), Arc::clone(&binaries))
        };
        let guild_queue = Arc::new(GuildMusicQueue::with_event_bus(event_bus));
        let voice_manager = Arc::new(DiscordVoiceManager::with_binaries(Arc::clone(&binaries)));

        Arc::new(Self {
            config,
            binaries,
            local_queue,
            local_player,
            guild_queue,
            voice_manager,
        })
    }

    pub fn local_queue(&self) -> &Arc<LocalMusicQueue> {
        &self.local_queue
    }

    pub fn local_player(&self) -> &Arc<LocalMusicPlayer> {
        &self.local_player
    }

    pub fn guild_queue(&self) -> &Arc<GuildMusicQueue> {
        &self.guild_queue
    }

    pub fn voice_manager(&self) -> &Arc<DiscordVoiceManager> {
        &self.voice_manager
    }

    pub fn binaries(&self) -> &Arc<BinaryManager> {
        &self.binaries
    }

    /// Attach the Discord voice queue controller loop (called by `DiscordAdapter`).
    pub fn start_discord_voice_loop(&self, cancel: CancellationToken) {
        let (tx, rx) = mpsc::unbounded_channel::<VoiceControlEvent>();
        self.guild_queue.set_voice_controller(Some(tx));
        Arc::clone(&self.voice_manager).spawn_queue_controller(
            Arc::clone(&self.guild_queue),
            rx,
            cancel,
        );
    }

    pub fn configured_guild_id(&self) -> Option<u64> {
        self.config
            .read()
            .ok()
            .and_then(|c| c.channels.discord_guild_id)
            .filter(|&g| g > 0)
    }

    pub fn allowed_discord_users(&self) -> Vec<u64> {
        self.config
            .read()
            .map(|c| c.channels.discord_allowed_user_ids.clone())
            .unwrap_or_default()
    }

    pub fn default_guild_id(&self) -> u64 {
        self.configured_guild_id().unwrap_or(0)
    }

    pub fn active_or_default_guild_id(&self) -> u64 {
        if let Some(gid) = self.voice_manager.active_guild_id() {
            if gid > 0 {
                return gid;
            }
        }
        let allowed = self.allowed_discord_users();
        let preferred = self.configured_guild_id();
        if let Some((gid, _)) = self
            .voice_manager
            .find_user_voice_location(&allowed, preferred)
        {
            return gid;
        }
        preferred.unwrap_or(0)
    }

    fn resolve_guild_id(&self, guild_id: Option<u64>) -> u64 {
        guild_id
            .filter(|&g| g > 0)
            .unwrap_or_else(|| self.active_or_default_guild_id())
    }

    /// Resolve track metadata (falling back to a lazy search/URL track when `yt-dlp`
    /// is absent or offline for direct URLs / queued descriptors) and enqueue on `target`.
    pub async fn play(
        &self,
        target: MusicTarget,
        query: &str,
        requested_by: &str,
        guild_id: Option<u64>,
    ) -> Result<Vec<(usize, MusicQueueSnapshot)>, MusicError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(MusicError::Queue("Provide a song title or URL".into()));
        }

        let mut resolved_gid: Option<u64> = None;
        if matches!(target, MusicTarget::DiscordVoice | MusicTarget::Both)
            && self.voice_manager.bot_user_id().is_some()
        {
            let allowed = self.allowed_discord_users();
            let preferred = guild_id
                .filter(|&g| g > 0)
                .or_else(|| self.voice_manager.active_guild_id())
                .or_else(|| self.configured_guild_id());
            if let Some((user_gid, user_cid)) =
                self.voice_manager.find_user_voice_location(&allowed, preferred)
            {
                if self.voice_manager.connected_channel(user_gid).await != Some(user_cid) {
                    self.join_discord_voice(Some(user_gid), user_cid).await?;
                }
                resolved_gid = Some(user_gid);
            } else {
                let mut connected_gid = None;
                if let Some(pg) = preferred {
                    if self.voice_manager.connected_channel(pg).await.is_some() {
                        connected_gid = Some(pg);
                    }
                }
                if connected_gid.is_none() {
                    if let Some(ag) = self.voice_manager.active_guild_id() {
                        if self.voice_manager.connected_channel(ag).await.is_some() {
                            connected_gid = Some(ag);
                        }
                    }
                }
                if let Some(cg) = connected_gid {
                    resolved_gid = Some(cg);
                } else {
                    return Err(MusicError::NotConnected(
                        "Không tìm thấy bạn trong kênh thoại (Voice Channel) nào trên Discord. Hãy vào một kênh thoại trên server có bot J.A.R.V.I.S trước rồi phát nhạc.".into(),
                    ));
                }
            }
        }

        let bin_dir = self.binaries.bin_dir();
        let track = if is_direct_media_url(trimmed) {
            TrackMeta {
                title: trimmed.to_string(),
                url: trimmed.to_string(),
                webpage_url: trimmed.to_string(),
                duration_seconds: 0,
                requested_by: requested_by.to_string(),
                thumbnail: None,
                stream_resolved: true,
            }
        } else {
            if !self.binaries.is_local_only() && self.binaries.locate("yt-dlp").is_none() {
                let _ = self.binaries.ensure_yt_dlp().await;
            }
            match extract_track_meta(trimmed, requested_by, Some(bin_dir)).await {
                Ok(t) => t,
                Err(_) if trimmed.starts_with("http://") || trimmed.starts_with("https://") => {
                    TrackMeta {
                        title: trimmed.to_string(),
                        url: trimmed.to_string(),
                        webpage_url: trimmed.to_string(),
                        duration_seconds: 0,
                        requested_by: requested_by.to_string(),
                        thumbnail: None,
                        stream_resolved: false,
                    }
                }
                Err(err) => return Err(err),
            }
        };

        Ok(self.enqueue_track(target, track, resolved_gid.or(guild_id)))
    }

    /// Enqueue an already-constructed [`TrackMeta`] onto the selected target(s).
    pub fn enqueue_track(
        &self,
        target: MusicTarget,
        track: TrackMeta,
        guild_id: Option<u64>,
    ) -> Vec<(usize, MusicQueueSnapshot)> {
        let mut results = Vec::new();
        if matches!(target, MusicTarget::Local | MusicTarget::Both) {
            self.local_player.start_controller();
            results.push(self.local_queue.enqueue(track.clone()));
        }
        if matches!(target, MusicTarget::DiscordVoice | MusicTarget::Both) {
            let gid = self.resolve_guild_id(guild_id);
            results.push(self.guild_queue.enqueue(gid, track));
        }
        results
    }

    pub fn pause(
        &self,
        target: MusicTarget,
        guild_id: Option<u64>,
    ) -> Result<Vec<MusicQueueSnapshot>, MusicError> {
        let mut snaps = Vec::new();
        if matches!(target, MusicTarget::Local | MusicTarget::Both) {
            snaps.push(self.local_queue.pause()?);
        }
        if matches!(target, MusicTarget::DiscordVoice | MusicTarget::Both) {
            let gid = self.resolve_guild_id(guild_id);
            snaps.push(self.guild_queue.pause(gid)?);
        }
        Ok(snaps)
    }

    pub fn resume(
        &self,
        target: MusicTarget,
        guild_id: Option<u64>,
    ) -> Result<Vec<MusicQueueSnapshot>, MusicError> {
        let mut snaps = Vec::new();
        if matches!(target, MusicTarget::Local | MusicTarget::Both) {
            snaps.push(self.local_queue.resume()?);
        }
        if matches!(target, MusicTarget::DiscordVoice | MusicTarget::Both) {
            let gid = self.resolve_guild_id(guild_id);
            snaps.push(self.guild_queue.resume(gid)?);
        }
        Ok(snaps)
    }

    pub fn skip(
        &self,
        target: MusicTarget,
        guild_id: Option<u64>,
    ) -> Result<Vec<SkipOutcome>, MusicError> {
        let mut outcomes = Vec::new();
        if matches!(target, MusicTarget::Local | MusicTarget::Both) {
            outcomes.push(self.local_queue.skip()?);
        }
        if matches!(target, MusicTarget::DiscordVoice | MusicTarget::Both) {
            let gid = self.resolve_guild_id(guild_id);
            outcomes.push(self.guild_queue.skip(gid)?);
        }
        Ok(outcomes)
    }

    pub fn stop(&self, target: MusicTarget, guild_id: Option<u64>) -> Vec<MusicQueueSnapshot> {
        let mut snaps = Vec::new();
        if matches!(target, MusicTarget::Local | MusicTarget::Both) {
            snaps.push(self.local_queue.stop());
        }
        if matches!(target, MusicTarget::DiscordVoice | MusicTarget::Both) {
            let gid = self.resolve_guild_id(guild_id);
            snaps.push(self.guild_queue.stop(gid));
        }
        snaps
    }

    pub fn set_volume(
        &self,
        target: MusicTarget,
        volume: f32,
        guild_id: Option<u64>,
    ) -> Result<f32, MusicError> {
        if matches!(target, MusicTarget::Local | MusicTarget::Both) {
            self.local_queue.set_volume(volume)?;
        }
        if matches!(target, MusicTarget::DiscordVoice | MusicTarget::Both) {
            let gid = self.resolve_guild_id(guild_id);
            self.guild_queue.set_volume(gid, volume)?;
        }
        Ok(volume)
    }

    pub async fn join_discord_voice(
        &self,
        guild_id: Option<u64>,
        channel_id: u64,
    ) -> Result<MusicQueueSnapshot, MusicError> {
        let gid = self.resolve_guild_id(guild_id);
        if self.voice_manager.bot_user_id().is_some() {
            self.voice_manager.join_channel(gid, channel_id).await?;
        }
        Ok(self.guild_queue.join_voice(gid, channel_id))
    }

    pub async fn auto_join_discord_voice(
        &self,
        guild_id: Option<u64>,
        channel_id: Option<u64>,
    ) -> Result<(u64, u64, MusicQueueSnapshot), MusicError> {
        let explicit_gid = guild_id.filter(|&g| g > 0);
        let explicit_cid = channel_id.filter(|&c| c > 0);

        let (gid, cid) = if let Some(cid) = explicit_cid {
            let gid = explicit_gid
                .or_else(|| self.voice_manager.guild_for_channel(cid))
                .or_else(|| {
                    let fallback = self.active_or_default_guild_id();
                    (fallback > 0).then_some(fallback)
                })
                .ok_or_else(|| {
                    MusicError::NotConnected(
                        "Could not resolve Discord server for the specified voice channel.".into(),
                    )
                })?;
            (gid, cid)
        } else {
            let allowed = self.allowed_discord_users();
            let preferred = explicit_gid
                .or_else(|| self.voice_manager.active_guild_id())
                .or_else(|| self.configured_guild_id());
            if let Some((found_gid, found_cid)) = self
                .voice_manager
                .find_user_voice_location(&allowed, preferred)
            {
                (found_gid, found_cid)
            } else if let Some(pg) = preferred {
                if let Some(existing_cid) = self.guild_queue.snapshot(pg).connected_channel_id {
                    (pg, existing_cid)
                } else {
                    return Err(MusicError::NotConnected(
                        "Không tìm thấy bạn trong kênh thoại (Voice Channel) nào trên Discord. Hãy vào một kênh thoại trước hoặc nhập Channel ID.".into(),
                    ));
                }
            } else {
                return Err(MusicError::NotConnected(
                    "Không tìm thấy bạn trong kênh thoại (Voice Channel) nào trên Discord. Hãy vào một kênh thoại trước hoặc nhập Channel ID.".into(),
                ));
            }
        };

        let snap = self.join_discord_voice(Some(gid), cid).await?;
        Ok((gid, cid, snap))
    }

    pub async fn leave_discord_voice(
        &self,
        guild_id: Option<u64>,
    ) -> Result<MusicQueueSnapshot, MusicError> {
        let gid = self.resolve_guild_id(guild_id);
        if self.voice_manager.connected_channel(gid).await.is_some() {
            self.voice_manager.leave_channel(gid).await?;
        }
        Ok(self.guild_queue.leave_voice(gid))
    }

    pub fn snapshots(
        &self,
        target: MusicTarget,
        guild_id: Option<u64>,
    ) -> Vec<MusicQueueSnapshot> {
        let mut snaps = Vec::new();
        if matches!(target, MusicTarget::Local | MusicTarget::Both) {
            snaps.push(self.local_queue.snapshot());
        }
        if matches!(target, MusicTarget::DiscordVoice | MusicTarget::Both) {
            let gid = self.resolve_guild_id(guild_id);
            snaps.push(self.guild_queue.snapshot(gid));
        }
        snaps
    }
}

#[derive(Debug, Deserialize)]
struct MusicControlArgs {
    action: String,
    #[serde(default)]
    target: Option<MusicTarget>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    volume: Option<f32>,
    #[serde(default)]
    guild_id: Option<u64>,
    #[serde(default)]
    channel_id: Option<u64>,
    #[serde(default)]
    requested_by: Option<String>,
}

/// Built-in [`Tool`] (`music_control`) allowing the agent to control local PC playback
/// and Discord Voice playback from chat or voice commands.
pub struct MusicControlTool {
    controller: Arc<DualMusicController>,
}

impl MusicControlTool {
    pub fn new(controller: Arc<DualMusicController>) -> Self {
        Self { controller }
    }
}

#[async_trait]
impl Tool for MusicControlTool {
    fn name(&self) -> &str {
        "music_control"
    }

    fn description(&self) -> &str {
        "Control JARVIS dual music playback (local PC speakers, Discord voice channel, or both): play, pause, resume, skip, stop, queue, status, volume, join, leave. Discord server (guild_id) and voice channel (channel_id) are optional and auto-detected across all servers."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["play", "pause", "resume", "skip", "stop", "queue", "status", "volume", "join", "leave"],
                    "description": "Music playback action"
                },
                "target": {
                    "type": "string",
                    "enum": ["local", "discord_voice", "both"],
                    "description": "Audio output target: 'local' (PC speakers), 'discord_voice', or 'both'"
                },
                "query": {
                    "type": "string",
                    "description": "Song name, search query, or URL (required for 'play')"
                },
                "volume": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 2.0,
                    "description": "Volume multiplier between 0.0 and 2.0 (required for 'volume')"
                },
                "guild_id": {
                    "type": "integer",
                    "description": "Optional Discord guild ID; auto-detected from active connection or user's voice channel if omitted"
                },
                "channel_id": {
                    "type": "integer",
                    "description": "Optional Discord voice channel ID (used with 'join'); auto-detected from user's voice channel if omitted"
                },
                "requested_by": {
                    "type": "string",
                    "description": "Requester display name"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: MusicControlArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let action = parsed.action.trim().to_ascii_lowercase();
        let target = parsed.target.unwrap_or_else(|| {
            if ctx.session_id.starts_with("discord-")
                || matches!(action.as_str(), "join" | "leave")
                || parsed.guild_id.is_some()
                || parsed.channel_id.is_some()
            {
                MusicTarget::DiscordVoice
            } else {
                MusicTarget::Local
            }
        });

        let requested_by = parsed
            .requested_by
            .as_deref()
            .unwrap_or("JARVIS Chat")
            .to_string();

        match action.as_str() {
            "play" => {
                let query = parsed
                    .query
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ToolError::InvalidArgs("missing 'query' for 'play' action".into())
                    })?;

                // Try full yt-dlp extraction first; if yt-dlp isn't installed in a local
                // non-URL flow, surface the clear BinaryMissing error unless it's a URL.
                match self
                    .controller
                    .play(target, query, &requested_by, parsed.guild_id)
                    .await
                {
                    Ok(res) => Ok(ToolResult::ok(
                        serde_json::to_string(&res).unwrap_or_else(|_| "ok".into()),
                    )),
                    Err(e) => Ok(ToolResult::err(e.to_string())),
                }
            }
            "pause" => match self.controller.pause(target, parsed.guild_id) {
                Ok(snaps) => Ok(ToolResult::ok(
                    serde_json::to_string(&snaps).unwrap_or_else(|_| "paused".into()),
                )),
                Err(e) => Ok(ToolResult::err(e.to_string())),
            },
            "resume" => match self.controller.resume(target, parsed.guild_id) {
                Ok(snaps) => Ok(ToolResult::ok(
                    serde_json::to_string(&snaps).unwrap_or_else(|_| "resumed".into()),
                )),
                Err(e) => Ok(ToolResult::err(e.to_string())),
            },
            "skip" => match self.controller.skip(target, parsed.guild_id) {
                Ok(outcomes) => Ok(ToolResult::ok(
                    serde_json::to_string(&outcomes).unwrap_or_else(|_| "skipped".into()),
                )),
                Err(e) => Ok(ToolResult::err(e.to_string())),
            },
            "stop" => {
                let snaps = self.controller.stop(target, parsed.guild_id);
                Ok(ToolResult::ok(
                    serde_json::to_string(&snaps).unwrap_or_else(|_| "stopped".into()),
                ))
            }
            "volume" => {
                let vol = parsed.volume.ok_or_else(|| {
                    ToolError::InvalidArgs("missing numeric 'volume' (0.0..=2.0)".into())
                })?;
                match self.controller.set_volume(target, vol, parsed.guild_id) {
                    Ok(v) => Ok(ToolResult::ok(serde_json::json!({ "volume": v }).to_string())),
                    Err(e) => Ok(ToolResult::err(e.to_string())),
                }
            }
            "join" => {
                match self
                    .controller
                    .auto_join_discord_voice(parsed.guild_id, parsed.channel_id)
                    .await
                {
                    Ok((gid, cid, snap)) => {
                        let guild_name = self.controller.voice_manager().guild_name(gid);
                        Ok(ToolResult::ok(
                            serde_json::json!({
                                "guild_id": gid,
                                "guild_name": guild_name,
                                "channel_id": cid,
                                "snapshot": snap,
                            })
                            .to_string(),
                        ))
                    }
                    Err(e) => Ok(ToolResult::err(e.to_string())),
                }
            }
            "leave" => match self.controller.leave_discord_voice(parsed.guild_id).await {
                Ok(snap) => Ok(ToolResult::ok(
                    serde_json::to_string(&snap).unwrap_or_else(|_| "left".into()),
                )),
                Err(e) => Ok(ToolResult::err(e.to_string())),
            },
            "queue" | "status" => {
                let snaps = self.controller.snapshots(target, parsed.guild_id);
                Ok(ToolResult::ok(
                    serde_json::to_string(&snaps).unwrap_or_else(|_| "[]".into()),
                ))
            }
            other => Err(ToolError::InvalidArgs(format!(
                "unsupported music action '{other}'"
            ))),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::Database;
    use tempfile::TempDir;

    fn sample_track(title: &str) -> TrackMeta {
        TrackMeta {
            title: title.to_string(),
            url: format!("https://cdn.example.com/{title}.mp3"),
            webpage_url: format!("https://youtube.com/watch?v={title}"),
            duration_seconds: 180,
            requested_by: "Huy".to_string(),
            thumbnail: None,
            stream_resolved: true,
        }
    }

    fn make_controller(dir: &TempDir) -> Arc<DualMusicController> {
        let mut cfg = AppConfig::default();
        cfg.channels.discord_guild_id = Some(4242);
        let cfg_lock = Arc::new(RwLock::new(cfg));
        let bus = EventBus::new(64);
        let binaries = Arc::new(BinaryManager::with_config(
            dir.path().to_path_buf(),
            "http://127.0.0.1:0/yt",
            "http://127.0.0.1:0/ff",
            Some(bus.clone()),
        ));
        DualMusicController::new_headless(cfg_lock, binaries, bus)
    }

    #[test]
    fn local_and_guild_queues_enqueue_pause_resume_skip_stop_and_volume() {
        let local = LocalMusicQueue::new();
        let (p1, s1) = local.enqueue(sample_track("Lac Troi"));
        assert_eq!(p1, 0);
        assert_eq!(s1.state, PlaybackState::Playing);
        assert_eq!(s1.target, MusicTarget::Local);
        assert_eq!(s1.current.as_ref().unwrap().title, "Lac Troi");

        let (p2, s2) = local.enqueue(sample_track("Lofi Chill"));
        assert_eq!(p2, 1);
        assert_eq!(s2.upcoming.len(), 1);

        assert_eq!(local.pause().unwrap().state, PlaybackState::Paused);
        assert_eq!(local.resume().unwrap().state, PlaybackState::Playing);
        let (skipped, next) = local.skip().unwrap();
        assert_eq!(skipped.unwrap().title, "Lac Troi");
        assert_eq!(next.unwrap().title, "Lofi Chill");

        assert!(local.set_volume(1.4).is_ok());
        assert!(local.set_volume(2.5).is_err());
        let stopped = local.stop();
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert!(stopped.current.is_none());

        let guild = GuildMusicQueue::new();
        let joined = guild.join_voice(99, 777);
        assert_eq!(joined.connected_channel_id, Some(777));
        let (gp1, gs1) = guild.enqueue(99, sample_track("Song 1"));
        assert_eq!(gp1, 0);
        assert_eq!(gs1.guild_id, Some(99));
        assert_eq!(gs1.target, MusicTarget::DiscordVoice);
        assert!(guild.skip(99).is_ok());
        assert_eq!(guild.stop(99).state, PlaybackState::Stopped);
    }

    #[test]
    fn dual_music_controller_targets_local_discord_and_both() {
        let dir = TempDir::new().unwrap();
        let ctrl = make_controller(&dir);

        // Local target only affects local queue
        ctrl.enqueue_track(MusicTarget::Local, sample_track("LocalOnly"), None);
        assert_eq!(ctrl.local_queue().now_playing().unwrap().title, "LocalOnly");
        assert!(ctrl.guild_queue().now_playing(4242).is_none());

        // DiscordVoice target only affects guild queue
        ctrl.enqueue_track(MusicTarget::DiscordVoice, sample_track("DiscordOnly"), None);
        assert_eq!(
            ctrl.guild_queue().now_playing(4242).unwrap().title,
            "DiscordOnly"
        );
        assert_eq!(ctrl.local_queue().now_playing().unwrap().title, "LocalOnly");

        // Both target affects both queues simultaneously
        let both_res = ctrl.enqueue_track(MusicTarget::Both, sample_track("SharedTrack"), None);
        assert_eq!(both_res.len(), 2);
        assert_eq!(ctrl.local_queue().snapshot().upcoming.len(), 1);
        assert_eq!(ctrl.guild_queue().snapshot(4242).upcoming.len(), 1);

        let paused = ctrl.pause(MusicTarget::Both, None).unwrap();
        assert_eq!(paused.len(), 2);
        assert!(paused.iter().all(|s| s.state == PlaybackState::Paused));

        let resumed = ctrl.resume(MusicTarget::Both, None).unwrap();
        assert!(resumed.iter().all(|s| s.state == PlaybackState::Playing));

        let skipped = ctrl.skip(MusicTarget::Both, None).unwrap();
        assert_eq!(skipped.len(), 2);
        assert_eq!(ctrl.local_queue().now_playing().unwrap().title, "SharedTrack");
        assert_eq!(
            ctrl.guild_queue().now_playing(4242).unwrap().title,
            "SharedTrack"
        );
    }

    #[test]
    fn parse_yt_dlp_json_extracts_metadata() {
        let raw = r#"{"title":"Sơn Tùng M-TP - Lạc Trôi","url":"https://cdn.example/audio.webm","webpage_url":"https://www.youtube.com/watch?v=Llw9Q6akRo4","duration":233.7,"thumbnail":"https://img.example/thumb.jpg"}"#;
        let meta = parse_yt_dlp_json(raw, "Huy").unwrap();
        assert_eq!(meta.title, "Sơn Tùng M-TP - Lạc Trôi");
        assert_eq!(meta.duration_seconds, 233);
        assert_eq!(meta.requested_by, "Huy");
        assert!(meta.stream_resolved);
        assert_eq!(
            meta.thumbnail.as_deref(),
            Some("https://img.example/thumb.jpg")
        );
    }

    #[tokio::test]
    async fn music_control_tool_plays_direct_url_and_controls_queues() {
        let dir = TempDir::new().unwrap();
        let ctrl = make_controller(&dir);
        let tool = MusicControlTool::new(Arc::clone(&ctrl));
        let ctx = ToolContext {
            session_id: "s1".into(),
            workspace_root: dir.path().to_path_buf(),
            db: Database::open_in_memory().unwrap(),
            event_bus: EventBus::new(16),
            full_auto_mode: false,
            agy_effort: "medium".into(),
            active_model: "test".into(),
            agy_model: None,
        };

        let join = tool
            .execute(
                serde_json::json!({"action": "join", "channel_id": 888}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(join.success);

        let play = tool
            .execute(
                serde_json::json!({
                    "action": "play",
                    "target": "both",
                    "query": "https://cdn.example.com/lofi.mp3"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(play.success, "play failed: {}", play.output);
        assert_eq!(
            ctrl.local_queue().now_playing().unwrap().title,
            "https://cdn.example.com/lofi.mp3"
        );
        assert_eq!(
            ctrl.guild_queue().now_playing(4242).unwrap().title,
            "https://cdn.example.com/lofi.mp3"
        );

        let vol = tool
            .execute(
                serde_json::json!({"action": "volume", "target": "both", "volume": 0.75}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(vol.success);

        let stop = tool
            .execute(
                serde_json::json!({"action": "stop", "target": "both"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(stop.success);
        assert_eq!(ctrl.local_queue().snapshot().state, PlaybackState::Stopped);
        assert_eq!(
            ctrl.guild_queue().snapshot(4242).state,
            PlaybackState::Stopped
        );
    }

    #[test]
    fn find_user_voice_location_scans_multiple_guilds_and_ignores_strangers() {
        let vm = DiscordVoiceManager::new();
        // Stranger in guild 100, allowed user 555 in guild 200
        vm.set_user_voice_state_for_test(100, 999, Some(1001));
        vm.set_user_voice_state_for_test(200, 555, Some(2002));

        // Only allowed user 555 should be matched
        assert_eq!(vm.find_user_voice_location(&[555], None), Some((200, 2002)));
        // Stranger-only check returns None when allowed list doesn't match
        assert_eq!(vm.find_user_voice_location(&[777], Some(100)), None);
        // pick_auto_join_channel must also ignore strangers
        assert_eq!(vm.pick_auto_join_channel(100, &[555]), None);
        assert_eq!(vm.pick_auto_join_channel(200, &[555]), Some(2002));

        // Preferred guild wins when allowed user(s) are in multiple guilds
        vm.set_user_voice_state_for_test(300, 555, Some(3003));
        assert_eq!(
            vm.find_user_voice_location(&[555], Some(300)),
            Some((300, 3003))
        );
    }

    #[test]
    fn guild_for_channel_resolves_channel_to_owning_guild() {
        let vm = DiscordVoiceManager::new();
        vm.set_user_voice_state_for_test(8888, 123, Some(9999));
        assert_eq!(vm.guild_for_channel(9999), Some(8888));
        assert_eq!(vm.guild_for_channel(1111), None);

        // Also verify on_voice_state_update populates channel_guilds via JSON VoiceState
        let vs: serenity::all::VoiceState = serde_json::from_value(serde_json::json!({
            "guild_id": "7777",
            "channel_id": "4444",
            "user_id": "321",
            "session_id": "sess_1",
            "deaf": false,
            "mute": false,
            "self_deaf": false,
            "self_mute": false,
            "self_video": false,
            "suppress": false
        }))
        .unwrap();
        vm.on_voice_state_update(&vs);
        assert_eq!(vm.guild_for_channel(4444), Some(7777));
    }

    #[test]
    fn active_or_default_guild_id_resolves_without_hardcoded_config() {
        let dir = TempDir::new().unwrap();
        let mut cfg = AppConfig::default();
        cfg.channels.discord_guild_id = None;
        cfg.channels.discord_allowed_user_ids = vec![555];
        let cfg_lock = Arc::new(RwLock::new(cfg));
        let bus = EventBus::new(16);
        let binaries = Arc::new(BinaryManager::with_config(
            dir.path().to_path_buf(),
            "http://127.0.0.1:0/yt",
            "http://127.0.0.1:0/ff",
            Some(bus.clone()),
        ));
        let ctrl = DualMusicController::new_headless(cfg_lock, binaries, bus);

        assert_eq!(ctrl.active_or_default_guild_id(), 0);

        ctrl.voice_manager()
            .set_user_voice_state_for_test(5555, 555, Some(6666));
        assert_eq!(ctrl.active_or_default_guild_id(), 5555);
    }

    #[tokio::test]
    async fn music_control_tool_defaults_to_discord_voice_in_discord_session() {
        let dir = TempDir::new().unwrap();
        let ctrl = make_controller(&dir);
        let tool = MusicControlTool::new(Arc::clone(&ctrl));
        let discord_ctx = ToolContext {
            session_id: "discord-123456".into(),
            workspace_root: dir.path().to_path_buf(),
            db: Database::open_in_memory().unwrap(),
            event_bus: EventBus::new(16),
            full_auto_mode: false,
            agy_effort: "medium".into(),
            active_model: "test".into(),
            agy_model: None,
        };

        let play = tool
            .execute(
                serde_json::json!({
                    "action": "play",
                    "query": "https://cdn.example.com/discord-default.mp3"
                }),
                &discord_ctx,
            )
            .await
            .unwrap();
        assert!(play.success, "play failed: {}", play.output);
        assert!(ctrl.local_queue().now_playing().is_none());
        assert_eq!(
            ctrl.guild_queue().now_playing(4242).unwrap().title,
            "https://cdn.example.com/discord-default.mp3"
        );
    }
}
