//! Pure-Rust Discord Voice Gateway v8 + UDP Audio Client with `davey` DAVE E2EE
//! and `aead_xchacha20_poly1305_rtpsize` RTP encryption.
//!
//! All `ffmpeg` subprocesses on Windows are spawned with `CREATE_NO_WINDOW` (`0x08000000`).

use super::queue::{
    resolve_audio_stream_url, should_resolve_stream, GuildMusicQueue, TrackMeta, VoiceControlEvent,
};
use super::MusicError;
use crate::binary_manager::{
    hidden_tokio_command, BinaryManager, DEFAULT_FFMPEG_URL, DEFAULT_YT_DLP_URL,
};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use davey::{DaveSession, ProposalsOperationType, DAVE_PROTOCOL_VERSION};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use serenity::all::{Guild, ShardMessenger, VoiceState};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

/// Discord Voice RTP frame cadence (20 ms at 48 kHz = 960 samples).
pub const OPUS_FRAME_DURATION_MS: u64 = 20;
pub const OPUS_SAMPLES_PER_FRAME: u32 = 960;
/// 3-byte Opus silence frame sent when pausing or finishing playback.
pub const OPUS_SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];

/// Timeout for each Discord Voice Gateway v8 handshake step.
const VOICE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum bytes retained from `ffmpeg` stderr for error reporting.
const FFMPEG_STDERR_TAIL_BYTES: usize = 2048;

/// Build the 12-byte RTP header (`version=2, payload_type=120` for Opus).
pub fn build_rtp_header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    let mut header = [0u8; 12];
    header[0] = 0x80;
    header[1] = 0x78;
    header[2..4].copy_from_slice(&sequence.to_be_bytes());
    header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());
    header
}

/// Encrypt a single 20ms Opus frame using Discord's `aead_xchacha20_poly1305_rtpsize` scheme:
/// `[12-byte RTP header] || [AEAD ciphertext + 16-byte tag] || [4-byte big-endian nonce]`.
pub fn encrypt_opus_rtp_packet(
    secret_key: &[u8; 32],
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    nonce_counter: u32,
    opus_frame: &[u8],
) -> Result<Vec<u8>, MusicError> {
    let header = build_rtp_header(sequence, timestamp, ssrc);
    let nonce_4 = nonce_counter.to_be_bytes();
    let mut nonce_24 = [0u8; 24];
    nonce_24[..4].copy_from_slice(&nonce_4);

    let cipher = XChaCha20Poly1305::new(secret_key.into());
    let ciphertext = cipher
        .encrypt(
            (&nonce_24).into(),
            Payload {
                msg: opus_frame,
                aad: &header,
            },
        )
        .map_err(|e| MusicError::Voice(format!("AEAD XChaCha20-Poly1305 encrypt failed: {e}")))?;

    let mut packet = Vec::with_capacity(12 + ciphertext.len() + 4);
    packet.extend_from_slice(&header);
    packet.extend_from_slice(&ciphertext);
    packet.extend_from_slice(&nonce_4);
    Ok(packet)
}

/// Build the 74-byte Discord UDP IP Discovery request packet.
pub fn build_ip_discovery_packet(ssrc: u32) -> [u8; 74] {
    let mut buf = [0u8; 74];
    buf[0..2].copy_from_slice(&1u16.to_be_bytes());
    buf[2..4].copy_from_slice(&70u16.to_be_bytes());
    buf[4..8].copy_from_slice(&ssrc.to_be_bytes());
    buf
}

/// Parse the 74-byte Discord UDP IP Discovery response into `(external_ip, external_port)`.
pub fn parse_ip_discovery_response(buf: &[u8]) -> Result<(String, u16), MusicError> {
    if buf.len() < 74 {
        return Err(MusicError::Voice(format!(
            "UDP IP discovery response too short: {} bytes",
            buf.len()
        )));
    }
    let resp_type = u16::from_be_bytes([buf[0], buf[1]]);
    if resp_type != 2 {
        return Err(MusicError::Voice(format!(
            "Unexpected UDP IP discovery response type: {resp_type}"
        )));
    }
    let ip_slice = &buf[8..72];
    let nul_pos = ip_slice.iter().position(|&b| b == 0).unwrap_or(ip_slice.len());
    let ip = std::str::from_utf8(&ip_slice[..nul_pos])
        .map_err(|e| MusicError::Voice(format!("Invalid IP in UDP discovery: {e}")))?
        .trim()
        .to_string();
    if ip.is_empty() {
        return Err(MusicError::Voice("Empty IP in UDP discovery response".into()));
    }
    let port = u16::from_be_bytes([buf[72], buf[73]]);
    Ok((ip, port))
}

/// Streaming pure-Rust Ogg/Opus page splitter and packet reassembler.
#[derive(Debug, Default)]
pub struct OggOpusDemuxer {
    buffer: Vec<u8>,
    pending_packet: Vec<u8>,
}

impl OggOpusDemuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of Ogg/Opus bytes and return any completed Opus audio frames.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(chunk);
        let mut packets = Vec::new();

        loop {
            let Some(sync_idx) = self.buffer.windows(4).position(|w| w == b"OggS") else {
                if self.buffer.len() > 3 {
                    let drain_to = self.buffer.len() - 3;
                    self.buffer.drain(..drain_to);
                }
                break;
            };
            if sync_idx > 0 {
                self.buffer.drain(..sync_idx);
            }

            if self.buffer.len() < 27 {
                break;
            }

            let header_type = self.buffer[5];
            let page_segments = self.buffer[26] as usize;
            let header_len = 27 + page_segments;
            if self.buffer.len() < header_len {
                break;
            }

            let body_len: usize = self.buffer[27..header_len]
                .iter()
                .map(|&b| b as usize)
                .sum();
            let page_len = header_len + body_len;
            if self.buffer.len() < page_len {
                break;
            }

            if (header_type & 0x01) == 0 && !self.pending_packet.is_empty() {
                self.pending_packet.clear();
            }

            let mut offset = header_len;
            for idx in 27..header_len {
                let seg_len = self.buffer[idx] as usize;
                if seg_len > 0 {
                    self.pending_packet
                        .extend_from_slice(&self.buffer[offset..offset + seg_len]);
                    offset += seg_len;
                }
                if seg_len < 255 {
                    let pkt = std::mem::take(&mut self.pending_packet);
                    if !pkt.is_empty()
                        && !pkt.starts_with(b"OpusHead")
                        && !pkt.starts_with(b"OpusTags")
                    {
                        packets.push(pkt);
                    }
                }
            }

            self.buffer.drain(..page_len);
        }

        packets
    }
}

/// Convenience wrapper for demuxing an in-memory Ogg/Opus byte slice.
pub fn demux_ogg_opus_slice(data: &[u8]) -> Vec<Vec<u8>> {
    let mut demuxer = OggOpusDemuxer::new();
    demuxer.feed(data)
}

/// Build the Voice Gateway v8 `Identify` (`op: 0`) JSON payload advertising DAVE support.
pub fn build_voice_identify_payload(
    guild_id: u64,
    bot_user_id: u64,
    session_id: &str,
    token: &str,
) -> serde_json::Value {
    json!({
        "op": 0,
        "d": {
            "server_id": guild_id.to_string(),
            "user_id": bot_user_id.to_string(),
            "session_id": session_id,
            "token": token,
            "max_dave_protocol_version": DAVE_PROTOCOL_VERSION,
        }
    })
}

/// Build the Voice Gateway v8 `Heartbeat` (`op: 3`) JSON payload with `t` and `seq_ack`.
pub fn build_voice_heartbeat_payload(nonce: u64, last_seq: i64) -> serde_json::Value {
    json!({
        "op": 3,
        "d": {
            "t": nonce,
            "seq_ack": last_seq,
        }
    })
}

/// Encode client-to-server binary DAVE Opcode 26 (`dave_mls_key_package`): `[26][key_package...]`.
pub fn encode_dave_key_package(key_package: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + key_package.len());
    out.push(26);
    out.extend_from_slice(key_package);
    out
}

/// Encode client-to-server binary DAVE Opcode 28 (`dave_mls_commit_welcome`):
/// `[28][commit...][welcome_if_present...]`.
pub fn encode_dave_commit_welcome(commit: &[u8], welcome: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + commit.len() + welcome.map_or(0, |w| w.len()));
    out.push(28);
    out.extend_from_slice(commit);
    if let Some(w) = welcome {
        out.extend_from_slice(w);
    }
    out
}

fn parse_snowflake_u64(val: &serde_json::Value) -> Option<u64> {
    val.as_u64()
        .or_else(|| val.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
}

fn build_dave_transition_ready_msg(transition_id: u16) -> WsMessage {
    WsMessage::Text(
        json!({
            "op": 23,
            "d": { "transition_id": transition_id }
        })
        .to_string(),
    )
}

fn build_dave_invalid_commit_welcome_msg(transition_id: u16) -> WsMessage {
    WsMessage::Text(
        json!({
            "op": 31,
            "d": { "transition_id": transition_id }
        })
        .to_string(),
    )
}

/// Manages DAVE MLS session state and Voice Gateway v8 opcodes `21..=31`.
pub struct DaveGatewayState {
    bot_user_id: u64,
    channel_id: u64,
    session: Arc<Mutex<Option<DaveSession>>>,
    protocol_version: Arc<AtomicU16>,
    pending_transitions: HashMap<u16, u16>,
    recognized_user_ids: HashSet<u64>,
    seen_clients_connect: bool,
}

impl DaveGatewayState {
    pub fn new(
        bot_user_id: u64,
        channel_id: u64,
        session: Arc<Mutex<Option<DaveSession>>>,
        protocol_version: Arc<AtomicU16>,
    ) -> Self {
        let mut recognized_user_ids = HashSet::new();
        recognized_user_ids.insert(bot_user_id);
        Self {
            bot_user_id,
            channel_id,
            session,
            protocol_version,
            pending_transitions: HashMap::new(),
            recognized_user_ids,
            seen_clients_connect: false,
        }
    }

    pub fn protocol_version(&self) -> u16 {
        self.protocol_version.load(Ordering::Relaxed)
    }

    pub fn can_encrypt(&self) -> bool {
        if self.protocol_version() == 0 {
            return false;
        }
        self.session
            .lock()
            .expect("dave_session mutex poisoned")
            .as_ref()
            .is_some_and(DaveSession::can_encrypt)
    }

    /// Initialize DAVE session from `SessionDescription` (`op: 4`), returning a binary
    /// `KeyPackage` (`op: 26`) WebSocket message when `dave_protocol_version > 0`.
    pub fn init_from_session_description(
        &mut self,
        dave_protocol_version: u16,
    ) -> Result<Option<WsMessage>, MusicError> {
        self.protocol_version
            .store(dave_protocol_version, Ordering::Relaxed);
        let Some(ver) = NonZeroU16::new(dave_protocol_version) else {
            return Ok(None);
        };

        let mut dave = DaveSession::new(ver, self.bot_user_id, self.channel_id, None)
            .map_err(|e| MusicError::Voice(format!("DAVE session init failed: {e}")))?;
        let key_package = dave
            .create_key_package()
            .map_err(|e| MusicError::Voice(format!("DAVE key package creation failed: {e}")))?;
        *self.session.lock().expect("dave_session mutex poisoned") = Some(dave);
        Ok(Some(WsMessage::Binary(encode_dave_key_package(&key_package))))
    }

    fn expected_users_slice(&self) -> Option<Vec<u64>> {
        if self.seen_clients_connect {
            Some(self.recognized_user_ids.iter().copied().collect())
        } else {
            None
        }
    }

    fn reinit_dave_session(&mut self) -> Result<Option<WsMessage>, MusicError> {
        let ver_u16 = self.protocol_version();
        let mut guard = self.session.lock().expect("dave_session mutex poisoned");
        if let Some(ver) = NonZeroU16::new(ver_u16) {
            let key_package = if let Some(ref mut dave) = *guard {
                dave.reinit(ver, self.bot_user_id, self.channel_id, None)
                    .map_err(|e| MusicError::Voice(format!("DAVE reinit failed: {e}")))?;
                dave.create_key_package()
                    .map_err(|e| MusicError::Voice(format!("DAVE key package failed: {e}")))?
            } else {
                let mut dave = DaveSession::new(ver, self.bot_user_id, self.channel_id, None)
                    .map_err(|e| MusicError::Voice(format!("DAVE init failed: {e}")))?;
                let kp = dave
                    .create_key_package()
                    .map_err(|e| MusicError::Voice(format!("DAVE key package failed: {e}")))?;
                *guard = Some(dave);
                kp
            };
            Ok(Some(WsMessage::Binary(encode_dave_key_package(&key_package))))
        } else {
            if let Some(ref mut dave) = *guard {
                let _ = dave.reset();
                dave.set_passthrough_mode(true, Some(10));
            }
            Ok(None)
        }
    }

    fn execute_transition(&mut self, transition_id: u16) {
        let Some(new_ver) = self.pending_transitions.remove(&transition_id) else {
            tracing::debug!("Ignoring ExecuteTransition for unknown id {transition_id}");
            return;
        };
        let old_ver = self.protocol_version.swap(new_ver, Ordering::Relaxed);
        let mut guard = self.session.lock().expect("dave_session mutex poisoned");
        if let Some(ref mut dave) = *guard {
            if let Err(e) = dave.execute_transition(transition_id) {
                tracing::warn!("DAVE execute_transition({transition_id}) failed: {e}");
            }
            if transition_id > 0 && old_ver == 0 && new_ver != 0 {
                dave.set_passthrough_mode(true, Some(10));
            }
        }
    }

    /// Handle incoming Voice Gateway JSON events (ClientsConnect/Disconnect + DAVE opcodes 21, 22, 24).
    pub fn handle_json_event(&mut self, val: &serde_json::Value) -> Vec<WsMessage> {
        let mut replies = Vec::new();
        let Some(op) = val["op"].as_u64() else {
            return replies;
        };
        let d = &val["d"];

        match op {
            11 => {
                self.seen_clients_connect = true;
                if let Some(arr) = d["user_ids"].as_array() {
                    for item in arr {
                        if let Some(uid) = parse_snowflake_u64(item) {
                            self.recognized_user_ids.insert(uid);
                        }
                    }
                }
            }
            13 => {
                if let Some(uid) = parse_snowflake_u64(&d["user_id"]) {
                    self.recognized_user_ids.remove(&uid);
                }
            }
            21 => {
                let proto = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                let tid = d["transition_id"].as_u64().unwrap_or(0) as u16;
                self.pending_transitions.insert(tid, proto);

                {
                    let mut guard = self.session.lock().expect("dave_session mutex poisoned");
                    if let Some(ref mut dave) = *guard {
                        if proto == 0 {
                            dave.set_passthrough_mode(true, Some(120));
                        }
                        if let Err(e) = dave.prepare_transition(tid, proto) {
                            tracing::warn!("DAVE prepare_transition({tid}, {proto}) failed: {e}");
                        }
                    }
                }

                if tid == 0 {
                    self.execute_transition(0);
                } else {
                    replies.push(build_dave_transition_ready_msg(tid));
                }
            }
            22 => {
                let tid = d["transition_id"].as_u64().unwrap_or(0) as u16;
                self.execute_transition(tid);
            }
            24 => {
                let epoch = d["epoch"].as_u64().unwrap_or(0);
                let proto = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                if epoch == 1 {
                    self.protocol_version.store(proto, Ordering::Relaxed);
                    match self.reinit_dave_session() {
                        Ok(Some(msg)) => replies.push(msg),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("DAVE PrepareEpoch reinit error: {e}"),
                    }
                }
            }
            _ => {}
        }

        replies
    }

    /// Handle a Voice Gateway v8 binary frame: `[seq: u16 BE][opcode: u8][payload...]`.
    pub fn handle_binary_frame(
        &mut self,
        data: &[u8],
    ) -> Result<(Option<u16>, Vec<WsMessage>), MusicError> {
        if data.len() < 3 {
            return Err(MusicError::Voice(format!(
                "Voice binary frame too short ({} bytes, expected >= 3 for v8 seq+op)",
                data.len()
            )));
        }
        let seq = u16::from_be_bytes([data[0], data[1]]);
        let opcode = data[2];
        let payload = &data[3..];
        let mut replies = Vec::new();
        let expected_users = self.expected_users_slice();
        let current_ver = self.protocol_version();

        match opcode {
            25 => {
                let mut guard = self.session.lock().expect("dave_session mutex poisoned");
                if let Some(ref mut dave) = *guard {
                    if let Err(e) = dave.set_external_sender(payload) {
                        tracing::warn!("DAVE set_external_sender failed: {e}");
                    }
                }
            }
            27 => {
                if payload.is_empty() {
                    return Err(MusicError::Voice(
                        "DAVE Proposals frame missing operation_type".into(),
                    ));
                }
                let op_type = match payload[0] {
                    0 => ProposalsOperationType::APPEND,
                    1 => ProposalsOperationType::REVOKE,
                    other => {
                        tracing::warn!("Unknown DAVE proposals operation_type {other}");
                        return Ok((Some(seq), replies));
                    }
                };
                let commit_welcome = {
                    let mut guard = self.session.lock().expect("dave_session mutex poisoned");
                    guard.as_mut().and_then(|dave| {
                        match dave.process_proposals(
                            op_type,
                            &payload[1..],
                            expected_users.as_deref(),
                        ) {
                            Ok(cw) => cw,
                            Err(e) => {
                                tracing::warn!("DAVE process_proposals failed: {e}");
                                None
                            }
                        }
                    })
                };
                if let Some(cw) = commit_welcome {
                    replies.push(WsMessage::Binary(encode_dave_commit_welcome(
                        &cw.commit,
                        cw.welcome.as_deref(),
                    )));
                }
            }
            29 => {
                if payload.len() < 2 {
                    return Err(MusicError::Voice(
                        "DAVE AnnounceCommitTransition missing transition_id".into(),
                    ));
                }
                let tid = u16::from_be_bytes([payload[0], payload[1]]);
                let commit = &payload[2..];
                let res = {
                    let mut guard = self.session.lock().expect("dave_session mutex poisoned");
                    guard.as_mut().map(|dave| {
                        let proc_res = dave.process_commit(commit)?;
                        if proc_res {
                            let _ = dave.prepare_transition(tid, current_ver);
                        }
                        Ok::<bool, davey::errors::ProcessCommitError>(proc_res)
                    })
                };
                match res {
                    Some(Ok(true)) if tid != 0 => {
                        self.pending_transitions.insert(tid, current_ver);
                        replies.push(build_dave_transition_ready_msg(tid));
                    }
                    Some(Ok(_)) | None => {}
                    Some(Err(e)) => {
                        tracing::warn!("DAVE process_commit failed (transition {tid}): {e}");
                        replies.push(build_dave_invalid_commit_welcome_msg(tid));
                        if let Ok(Some(kp_msg)) = self.reinit_dave_session() {
                            replies.push(kp_msg);
                        }
                    }
                }
            }
            30 => {
                if payload.len() < 2 {
                    return Err(MusicError::Voice(
                        "DAVE Welcome missing transition_id".into(),
                    ));
                }
                let tid = u16::from_be_bytes([payload[0], payload[1]]);
                let welcome = &payload[2..];
                let res = {
                    let mut guard = self.session.lock().expect("dave_session mutex poisoned");
                    guard.as_mut().map(|dave| {
                        dave.process_welcome(welcome, expected_users.as_deref())?;
                        let _ = dave.prepare_transition(tid, current_ver);
                        Ok::<(), davey::errors::ProcessWelcomeError>(())
                    })
                };
                match res {
                    Some(Ok(())) if tid != 0 => {
                        self.pending_transitions.insert(tid, current_ver);
                        replies.push(build_dave_transition_ready_msg(tid));
                    }
                    Some(Ok(())) | None => {}
                    Some(Err(e)) => {
                        tracing::warn!("DAVE process_welcome failed (transition {tid}): {e}");
                        replies.push(build_dave_invalid_commit_welcome_msg(tid));
                        if let Ok(Some(kp_msg)) = self.reinit_dave_session() {
                            replies.push(kp_msg);
                        }
                    }
                }
            }
            other => {
                tracing::debug!("Ignoring unknown Voice Gateway binary opcode {other}");
            }
        }

        Ok((Some(seq), replies))
    }
}

#[derive(Debug, Default, Clone)]
struct VoiceHandshakeData {
    session_id: Option<String>,
    endpoint: Option<String>,
    token: Option<String>,
    channel_id: Option<u64>,
}

struct RtpCounters {
    sequence: u16,
    timestamp: u32,
    nonce: u32,
}

struct GuildVoiceConnection {
    channel_id: u64,
    ssrc: u32,
    secret_key: [u8; 32],
    dave_session: Arc<Mutex<Option<DaveSession>>>,
    dave_protocol_version: Arc<AtomicU16>,
    udp: Arc<UdpSocket>,
    ws_tx: mpsc::UnboundedSender<WsMessage>,
    rtp: tokio::sync::Mutex<RtpCounters>,
    shutdown: CancellationToken,
}

impl GuildVoiceConnection {
    fn set_speaking(&self, speaking: bool) {
        let payload = json!({
            "op": 5,
            "d": {
                "speaking": if speaking { 1 } else { 0 },
                "delay": 0,
                "ssrc": self.ssrc,
            }
        });
        let _ = self.ws_tx.send(WsMessage::Text(payload.to_string()));
    }

    async fn send_opus_frame(&self, frame: &[u8]) -> Result<(), MusicError> {
        let dave_payload: Cow<[u8]> = if self.dave_protocol_version.load(Ordering::Relaxed) != 0 {
            let mut guard = self.dave_session.lock().expect("dave_session mutex poisoned");
            if let Some(ref mut session) = *guard {
                if session.can_encrypt() {
                    Cow::Owned(
                        session
                            .encrypt_opus(frame)
                            .map_err(|e| MusicError::Voice(format!("DAVE encrypt_opus failed: {e}")))?
                            .into_owned(),
                    )
                } else {
                    Cow::Borrowed(frame)
                }
            } else {
                Cow::Borrowed(frame)
            }
        } else {
            Cow::Borrowed(frame)
        };

        let packet = {
            let mut rtp = self.rtp.lock().await;
            let pkt = encrypt_opus_rtp_packet(
                &self.secret_key,
                rtp.sequence,
                rtp.timestamp,
                self.ssrc,
                rtp.nonce,
                &dave_payload,
            )?;
            rtp.sequence = rtp.sequence.wrapping_add(1);
            rtp.timestamp = rtp.timestamp.wrapping_add(OPUS_SAMPLES_PER_FRAME);
            rtp.nonce = rtp.nonce.wrapping_add(1);
            pkt
        };
        self.udp
            .send(&packet)
            .await
            .map_err(|e| MusicError::Voice(format!("UDP voice send failed: {e}")))?;
        Ok(())
    }

    async fn send_silence_burst(&self) {
        for _ in 0..5 {
            let _ = self.send_opus_frame(&OPUS_SILENCE_FRAME).await;
            tokio::time::sleep(Duration::from_millis(OPUS_FRAME_DURATION_MS)).await;
        }
    }
}

struct GuildPlaybackHandle {
    cancel: CancellationToken,
    preempted: Arc<AtomicBool>,
    task: JoinHandle<()>,
    paused_tx: watch::Sender<bool>,
    volume_tx: watch::Sender<f32>,
}

/// Manages Discord voice connections, user voice-channel tracking, and real UDP Opus playback.
pub struct DiscordVoiceManager {
    binaries: Arc<BinaryManager>,
    bot_user_id: Mutex<Option<u64>>,
    shard: Mutex<Option<ShardMessenger>>,
    user_voice_channels: Mutex<HashMap<(u64, u64), u64>>,
    channel_guilds: Mutex<HashMap<u64, u64>>,
    guild_names: Mutex<HashMap<u64, String>>,
    last_active_guild: Mutex<Option<u64>>,
    handshakes: Mutex<HashMap<u64, VoiceHandshakeData>>,
    handshake_notify: Arc<Notify>,
    connections: RwLock<HashMap<u64, Arc<GuildVoiceConnection>>>,
    playback_tasks: tokio::sync::Mutex<HashMap<u64, GuildPlaybackHandle>>,
}

impl Default for DiscordVoiceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscordVoiceManager {
    pub fn new() -> Self {
        Self::with_binaries(Arc::new(BinaryManager::new()))
    }

    pub fn with_bin_dir(bin_dir: PathBuf) -> Self {
        Self::with_binaries(Arc::new(BinaryManager::with_config(
            bin_dir,
            DEFAULT_YT_DLP_URL,
            DEFAULT_FFMPEG_URL,
            None,
        )))
    }

    pub fn with_binaries(binaries: Arc<BinaryManager>) -> Self {
        Self {
            binaries,
            bot_user_id: Mutex::new(None),
            shard: Mutex::new(None),
            user_voice_channels: Mutex::new(HashMap::new()),
            channel_guilds: Mutex::new(HashMap::new()),
            guild_names: Mutex::new(HashMap::new()),
            last_active_guild: Mutex::new(None),
            handshakes: Mutex::new(HashMap::new()),
            handshake_notify: Arc::new(Notify::new()),
            connections: RwLock::new(HashMap::new()),
            playback_tasks: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn binaries(&self) -> &Arc<BinaryManager> {
        &self.binaries
    }

    pub fn set_ready(&self, bot_user_id: u64, shard: ShardMessenger) {
        *self.bot_user_id.lock().expect("bot_user_id mutex poisoned") = Some(bot_user_id);
        *self.shard.lock().expect("shard mutex poisoned") = Some(shard);
    }

    pub fn bot_user_id(&self) -> Option<u64> {
        *self.bot_user_id.lock().expect("bot_user_id mutex poisoned")
    }

    pub fn clear_shard(&self) {
        *self.shard.lock().expect("shard mutex poisoned") = None;
    }

    pub fn on_guild_create(&self, guild: &Guild) {
        let gid = guild.id.get();
        self.guild_names
            .lock()
            .expect("guild_names mutex poisoned")
            .insert(gid, guild.name.clone());

        {
            let mut cg = self
                .channel_guilds
                .lock()
                .expect("channel_guilds mutex poisoned");
            cg.retain(|_, &mut g| g != gid);
            for ch_id in guild.channels.keys() {
                cg.insert(ch_id.get(), gid);
            }
        }

        let mut map = self
            .user_voice_channels
            .lock()
            .expect("user_voice_channels mutex poisoned");
        map.retain(|&(g, _), _| g != gid);
        for (uid, vs) in &guild.voice_states {
            if let Some(cid) = vs.channel_id {
                map.insert((gid, uid.get()), cid.get());
                self.channel_guilds
                    .lock()
                    .expect("channel_guilds mutex poisoned")
                    .insert(cid.get(), gid);
            }
        }
    }

    pub fn on_voice_state_update(&self, new_state: &VoiceState) {
        let Some(gid) = new_state.guild_id.map(|g| g.get()) else {
            return;
        };
        let uid = new_state.user_id.get();
        let cid = new_state.channel_id.map(|c| c.get());

        {
            let mut map = self
                .user_voice_channels
                .lock()
                .expect("user_voice_channels mutex poisoned");
            match cid {
                Some(channel_id) => {
                    map.insert((gid, uid), channel_id);
                    self.channel_guilds
                        .lock()
                        .expect("channel_guilds mutex poisoned")
                        .insert(channel_id, gid);
                }
                None => {
                    map.remove(&(gid, uid));
                }
            }
        }

        if self.bot_user_id() == Some(uid) {
            {
                let mut lag = self
                    .last_active_guild
                    .lock()
                    .expect("last_active_guild mutex poisoned");
                if cid.is_some() {
                    *lag = Some(gid);
                } else if *lag == Some(gid) {
                    *lag = None;
                }
            }
            let mut hs = self
                .handshakes
                .lock()
                .expect("handshakes mutex poisoned");
            let entry = hs.entry(gid).or_default();
            entry.session_id = Some(new_state.session_id.clone());
            entry.channel_id = cid;
            self.handshake_notify.notify_waiters();
        }
    }

    pub fn on_voice_server_update(&self, guild_id: u64, endpoint: Option<String>, token: String) {
        let mut hs = self
            .handshakes
            .lock()
            .expect("handshakes mutex poisoned");
        let entry = hs.entry(guild_id).or_default();
        entry.endpoint = endpoint;
        entry.token = Some(token);
        self.handshake_notify.notify_waiters();
    }

    pub fn active_guild_id(&self) -> Option<u64> {
        *self
            .last_active_guild
            .lock()
            .expect("last_active_guild mutex poisoned")
    }

    pub fn guild_for_channel(&self, channel_id: u64) -> Option<u64> {
        self.channel_guilds
            .lock()
            .expect("channel_guilds mutex poisoned")
            .get(&channel_id)
            .copied()
    }

    pub fn guild_name(&self, guild_id: u64) -> Option<String> {
        self.guild_names
            .lock()
            .expect("guild_names mutex poisoned")
            .get(&guild_id)
            .cloned()
    }

    pub fn set_user_voice_state_for_test(
        &self,
        guild_id: u64,
        user_id: u64,
        channel_id: Option<u64>,
    ) {
        let mut map = self
            .user_voice_channels
            .lock()
            .expect("user_voice_channels mutex poisoned");
        match channel_id {
            Some(cid) => {
                map.insert((guild_id, user_id), cid);
                self.channel_guilds
                    .lock()
                    .expect("channel_guilds mutex poisoned")
                    .insert(cid, guild_id);
            }
            None => {
                map.remove(&(guild_id, user_id));
            }
        }
    }

    pub fn get_user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
        self.user_voice_channels
            .lock()
            .expect("user_voice_channels mutex poisoned")
            .get(&(guild_id, user_id))
            .copied()
    }

    pub fn find_user_voice_location(
        &self,
        allowed_user_ids: &[u64],
        preferred_guild_id: Option<u64>,
    ) -> Option<(u64, u64)> {
        let bot_id = self.bot_user_id();
        let pref = preferred_guild_id.filter(|&g| g > 0);
        let map = self
            .user_voice_channels
            .lock()
            .expect("user_voice_channels mutex poisoned");

        if !allowed_user_ids.is_empty() {
            if let Some(pg) = pref {
                for &uid in allowed_user_ids {
                    if Some(uid) == bot_id {
                        continue;
                    }
                    if let Some(&cid) = map.get(&(pg, uid)) {
                        return Some((pg, cid));
                    }
                }
            }
            for &uid in allowed_user_ids {
                if Some(uid) == bot_id {
                    continue;
                }
                if let Some((&(gid, _), &cid)) =
                    map.iter().find(|(&(gid, u), _)| gid > 0 && u == uid)
                {
                    return Some((gid, cid));
                }
            }
            return None;
        }

        if let Some(pg) = pref {
            if let Some((&(gid, _), &cid)) = map
                .iter()
                .find(|(&(gid, uid), _)| gid == pg && Some(uid) != bot_id)
            {
                return Some((gid, cid));
            }
        }

        map.iter()
            .find(|(&(gid, uid), _)| gid > 0 && Some(uid) != bot_id)
            .map(|(&(gid, _), &cid)| (gid, cid))
    }

    pub fn pick_auto_join_channel(
        &self,
        guild_id: u64,
        allowed_user_ids: &[u64],
    ) -> Option<u64> {
        self.find_user_voice_location(allowed_user_ids, Some(guild_id))
            .and_then(|(g, c)| (g == guild_id).then_some(c))
    }

    pub async fn connected_channel(&self, guild_id: u64) -> Option<u64> {
        self.connections
            .read()
            .await
            .get(&guild_id)
            .filter(|c| !c.shutdown.is_cancelled())
            .map(|c| c.channel_id)
    }

    fn send_gateway_voice_state(
        &self,
        guild_id: u64,
        channel_id: Option<u64>,
    ) -> Result<(), MusicError> {
        let shard = self
            .shard
            .lock()
            .expect("shard mutex poisoned")
            .clone()
            .ok_or_else(|| MusicError::NotConnected("discord gateway not ready".into()))?;
        let payload = json!({
            "op": 4,
            "d": {
                "guild_id": guild_id.to_string(),
                "channel_id": channel_id.map(|c| c.to_string()),
                "self_mute": false,
                "self_deaf": true,
            }
        });
        shard.websocket_message(serenity_tungstenite::Message::Text(payload.to_string()));
        Ok(())
    }

    /// Connect to a Discord voice channel via Gateway Opcode 4 + Voice Gateway v8 + UDP Discovery.
    pub async fn join_channel(&self, guild_id: u64, channel_id: u64) -> Result<(), MusicError> {
        if guild_id == 0 {
            return Err(MusicError::Voice("Invalid Discord guild_id (0)".into()));
        }
        if let Some(existing) = self.connections.read().await.get(&guild_id).cloned() {
            if existing.channel_id == channel_id && !existing.shutdown.is_cancelled() {
                *self
                    .last_active_guild
                    .lock()
                    .expect("last_active_guild mutex poisoned") = Some(guild_id);
                return Ok(());
            }
            existing.shutdown.cancel();
        }

        let bot_user_id = self
            .bot_user_id()
            .ok_or_else(|| MusicError::NotConnected("discord bot user id unavailable".into()))?;

        {
            let mut hs = self.handshakes.lock().expect("handshakes mutex poisoned");
            let entry = hs.entry(guild_id).or_default();
            entry.endpoint = None;
            entry.token = None;
            entry.channel_id = Some(channel_id);
        }

        self.send_gateway_voice_state(guild_id, Some(channel_id))?;

        let (session_id, endpoint, token) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                {
                    let hs = self.handshakes.lock().expect("handshakes mutex poisoned");
                    if let Some(d) = hs.get(&guild_id) {
                        if let (Some(sid), Some(ep), Some(tok)) =
                            (&d.session_id, &d.endpoint, &d.token)
                        {
                            if !ep.trim().is_empty() && !tok.trim().is_empty() {
                                return (sid.clone(), ep.clone(), tok.clone());
                            }
                        }
                    }
                }
                self.handshake_notify.notified().await;
            }
        })
        .await
        .map_err(|_| {
            MusicError::Voice(
                "Timed out waiting for Discord VOICE_STATE_UPDATE / VOICE_SERVER_UPDATE".into(),
            )
        })?;

        let conn = establish_voice_connection(
            guild_id,
            channel_id,
            bot_user_id,
            &session_id,
            &endpoint,
            &token,
        )
        .await?;

        self.connections.write().await.insert(guild_id, conn);
        *self
            .last_active_guild
            .lock()
            .expect("last_active_guild mutex poisoned") = Some(guild_id);
        Ok(())
    }

    /// Disconnect from voice in `guild_id`, stop playback, and send Gateway Opcode 4 (`null`).
    pub async fn leave_channel(&self, guild_id: u64) -> Result<(), MusicError> {
        self.stop_playback(guild_id).await;
        let removed = {
            let mut guard = self.connections.write().await;
            guard.remove(&guild_id)
        };
        if let Some(conn) = removed {
            conn.send_silence_burst().await;
            conn.set_speaking(false);
            conn.shutdown.cancel();
        }
        {
            let next_active = self
                .connections
                .read()
                .await
                .iter()
                .find(|(&gid, c)| gid != guild_id && !c.shutdown.is_cancelled())
                .map(|(&gid, _)| gid);
            let mut lag = self
                .last_active_guild
                .lock()
                .expect("last_active_guild mutex poisoned");
            if *lag == Some(guild_id) {
                *lag = next_active;
            }
        }
        let _ = self.send_gateway_voice_state(guild_id, None);
        Ok(())
    }

    /// Shut down all active voice connections.
    pub async fn shutdown_all(&self) {
        let guild_ids: Vec<u64> = self.connections.read().await.keys().copied().collect();
        for gid in guild_ids {
            let _ = self.leave_channel(gid).await;
        }
        *self
            .last_active_guild
            .lock()
            .expect("last_active_guild mutex poisoned") = None;
        self.clear_shard();
    }

    /// Start or restart real UDP playback for the guild's current track in `queue`.
    pub async fn start_guild_playback(
        self: &Arc<Self>,
        guild_id: u64,
        queue: Arc<GuildMusicQueue>,
        track: TrackMeta,
    ) {
        self.stop_playback_with_preempt(guild_id, true).await;

        let cancel = CancellationToken::new();
        let preempted = Arc::new(AtomicBool::new(false));
        let initial_vol = queue.snapshot(guild_id).volume;
        let (paused_tx, paused_rx) = watch::channel(false);
        let (volume_tx, volume_rx) = watch::channel(initial_vol);

        let mgr = Arc::clone(self);
        let task_cancel = cancel.clone();
        let task_preempted = Arc::clone(&preempted);
        let binaries = Arc::clone(&self.binaries);
        let task = tokio::spawn(async move {
            let mut current_track = Some(track);
            while let Some(trk) = current_track.take() {
                if task_cancel.is_cancelled() {
                    break;
                }
                let Some(conn) = mgr.connections.read().await.get(&guild_id).cloned() else {
                    tracing::warn!(
                        "No active voice UDP connection for guild {guild_id}; skipping playback"
                    );
                    break;
                };

                let finished_naturally = play_single_track(
                    &conn,
                    &trk,
                    &binaries,
                    task_cancel.clone(),
                    paused_rx.clone(),
                    volume_rx.clone(),
                )
                .await;

                if !finished_naturally || task_cancel.is_cancelled() {
                    break;
                }
                current_track = queue.advance_next(guild_id);
            }
            if !task_preempted.load(Ordering::SeqCst) {
                if let Some(conn) = mgr.connections.read().await.get(&guild_id).cloned() {
                    conn.send_silence_burst().await;
                    conn.set_speaking(false);
                }
            }
        });

        let mut tasks = self.playback_tasks.lock().await;
        tasks.insert(
            guild_id,
            GuildPlaybackHandle {
                cancel,
                preempted,
                task,
                paused_tx,
                volume_tx,
            },
        );
    }

    pub async fn set_paused(&self, guild_id: u64, paused: bool) {
        let tasks = self.playback_tasks.lock().await;
        if let Some(handle) = tasks.get(&guild_id) {
            let _ = handle.paused_tx.send(paused);
        }
    }

    pub async fn set_volume(&self, guild_id: u64, volume: f32) {
        let tasks = self.playback_tasks.lock().await;
        if let Some(handle) = tasks.get(&guild_id) {
            let _ = handle.volume_tx.send(volume);
        }
    }

    pub async fn stop_playback(&self, guild_id: u64) {
        self.stop_playback_with_preempt(guild_id, false).await;
    }

    pub async fn stop_playback_with_preempt(&self, guild_id: u64, preempt: bool) {
        let handle = self.playback_tasks.lock().await.remove(&guild_id);
        let had_handle = handle.is_some();
        if let Some(mut h) = handle {
            if preempt {
                h.preempted.store(true, Ordering::SeqCst);
            }
            h.cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(2), &mut h.task)
                .await
                .is_err()
            {
                tracing::warn!(
                    "Timed out awaiting previous voice playback task for guild {guild_id}; aborting"
                );
                h.task.abort();
                let _ = h.task.await;
            }
        }
        if !preempt && !had_handle {
            if let Some(conn) = self.connections.read().await.get(&guild_id).cloned() {
                conn.set_speaking(false);
            }
        }
    }

    /// Stream synthesized Edge-TTS audio bytes directly into the guild's connected voice channel.
    pub async fn play_tts_bytes(&self, guild_id: u64, mp3_bytes: Vec<u8>) -> Result<(), MusicError> {
        let conn = self
            .connections
            .read()
            .await
            .get(&guild_id)
            .cloned()
            .ok_or_else(|| {
                MusicError::Voice("Bot is not connected to a voice channel. Run !join first.".into())
            })?;

        let ffmpeg = self.binaries.ensure_ffmpeg().await?;

        let mut child = hidden_tokio_command(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                "pipe:0",
                "-vn",
                "-c:a",
                "libopus",
                "-b:a",
                "64k",
                "-ar",
                "48000",
                "-ac",
                "2",
                "-frame_duration",
                "20",
                "-f",
                "ogg",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = stdin.write_all(&mp3_bytes).await;
                let _ = stdin.shutdown().await;
            });
        }

        let stderr_task = child.stderr.take().map(spawn_stderr_tail);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MusicError::Voice("Failed to capture ffmpeg stdout for TTS".into()))?;

        conn.set_speaking(true);
        let mut frames_emitted = 0u64;
        let res = stream_ogg_stdout_over_udp(
            &conn,
            stdout,
            CancellationToken::new(),
            None,
            &mut frames_emitted,
        )
        .await;
        conn.send_silence_burst().await;
        conn.set_speaking(false);

        let status = child.wait().await;
        let stderr_tail = collect_stderr_tail(stderr_task).await;

        res?;
        if let Ok(exit) = status {
            if !exit.success() {
                if frames_emitted == 0 {
                    return Err(MusicError::Voice(format!(
                        "ffmpeg TTS exited with {exit}: {stderr_tail}"
                    )));
                }
                tracing::warn!("ffmpeg TTS exited with {exit}: {stderr_tail}");
            }
        }
        Ok(())
    }

    /// Spawn the background control loop bridging [`GuildMusicQueue`] events to real Discord Voice UDP playback.
    pub fn spawn_queue_controller(
        self: Arc<Self>,
        queue: Arc<GuildMusicQueue>,
        mut rx: mpsc::UnboundedReceiver<VoiceControlEvent>,
        cancel: CancellationToken,
    ) {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    maybe_ev = rx.recv() => {
                        let Some(ev) = maybe_ev else { break };
                        match ev {
                            VoiceControlEvent::JoinRequested(gid, cid) => {
                                if let Err(e) = self.join_channel(gid, cid).await {
                                    tracing::warn!("Voice join failed for guild {gid} channel {cid}: {e}");
                                }
                            }
                            VoiceControlEvent::LeaveRequested(gid) => {
                                if let Err(e) = self.leave_channel(gid).await {
                                    tracing::warn!("Voice leave failed for guild {gid}: {e}");
                                }
                            }
                            VoiceControlEvent::TrackStarted(gid, track) => {
                                self.start_guild_playback(gid, Arc::clone(&queue), track).await;
                            }
                            VoiceControlEvent::Paused(gid) => {
                                self.set_paused(gid, true).await;
                            }
                            VoiceControlEvent::Resumed(gid) => {
                                self.set_paused(gid, false).await;
                            }
                            VoiceControlEvent::Skipped(gid, next) => {
                                if let Some(track) = next {
                                    self.start_guild_playback(gid, Arc::clone(&queue), track).await;
                                } else {
                                    self.stop_playback(gid).await;
                                }
                            }
                            VoiceControlEvent::Stopped(gid) => {
                                self.stop_playback(gid).await;
                            }
                            VoiceControlEvent::VolumeChanged(gid, vol) => {
                                self.set_volume(gid, vol).await;
                            }
                        }
                    }
                }
            }
        });
    }
}

fn spawn_stderr_tail(mut stderr: tokio::process::ChildStderr) -> JoinHandle<String> {
    tokio::spawn(async move {
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > FFMPEG_STDERR_TAIL_BYTES {
                        let excess = tail.len() - FFMPEG_STDERR_TAIL_BYTES;
                        tail.drain(..excess);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).trim().to_string()
    })
}

async fn collect_stderr_tail(handle: Option<JoinHandle<String>>) -> String {
    let Some(mut h) = handle else {
        return String::new();
    };
    match tokio::time::timeout(Duration::from_millis(500), &mut h).await {
        Ok(Ok(s)) => s,
        _ => {
            h.abort();
            String::new()
        }
    }
}

fn format_ws_close_frame(
    frame: Option<&tokio_tungstenite::tungstenite::protocol::CloseFrame<'_>>,
) -> String {
    match frame {
        Some(f) => format!("close code {}: {}", f.code, f.reason),
        None => "no close frame".to_string(),
    }
}

fn update_seq_from_json(val: &serde_json::Value, last_seq: &mut i64) {
    if let Some(seq) = val["seq"].as_i64().or_else(|| val["s"].as_i64()) {
        *last_seq = seq;
    }
}

async fn recv_voice_ws_text_step<S>(
    read: &mut S,
    step_name: &str,
    last_seq: &mut i64,
    dave: &mut DaveGatewayState,
    outgoing: &mut Vec<WsMessage>,
) -> Result<serde_json::Value, MusicError>
where
    S: futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let next_item = tokio::time::timeout(VOICE_HANDSHAKE_TIMEOUT, read.next())
            .await
            .map_err(|_| {
                MusicError::Voice(format!(
                    "Timed out after {}s waiting for Voice Gateway {step_name}",
                    VOICE_HANDSHAKE_TIMEOUT.as_secs()
                ))
            })?;

        let msg = next_item
            .ok_or_else(|| MusicError::Voice(format!("Voice WS closed before {step_name}")))?
            .map_err(|e| MusicError::Voice(format!("Voice WS read error before {step_name}: {e}")))?;

        match msg {
            WsMessage::Text(txt) => {
                let val: serde_json::Value = serde_json::from_str(&txt)?;
                update_seq_from_json(&val, last_seq);
                outgoing.extend(dave.handle_json_event(&val));
                return Ok(val);
            }
            WsMessage::Binary(bin) => {
                if let Ok((maybe_seq, replies)) = dave.handle_binary_frame(&bin) {
                    if let Some(seq) = maybe_seq {
                        *last_seq = seq as i64;
                    }
                    outgoing.extend(replies);
                }
            }
            WsMessage::Close(frame) => {
                let detail = format_ws_close_frame(frame.as_ref());
                tracing::warn!("Discord Voice WS closed before {step_name}: {detail}");
                return Err(MusicError::Voice(format!(
                    "Voice WS closed before {step_name} ({detail})"
                )));
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {}
        }
    }
}

async fn flush_ws_outgoing<W>(write: &mut W, outgoing: &mut Vec<WsMessage>) -> Result<(), MusicError>
where
    W: futures::Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    for msg in outgoing.drain(..) {
        write
            .send(msg)
            .await
            .map_err(|e| MusicError::Voice(format!("Voice WS send failed: {e}")))?;
    }
    Ok(())
}

async fn establish_voice_connection(
    guild_id: u64,
    channel_id: u64,
    bot_user_id: u64,
    session_id: &str,
    endpoint: &str,
    token: &str,
) -> Result<Arc<GuildVoiceConnection>, MusicError> {
    let clean_ep = endpoint
        .trim()
        .trim_start_matches("wss://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let ws_url = format!("wss://{clean_ep}/?v=8");

    let (ws_stream, _) = tokio::time::timeout(VOICE_HANDSHAKE_TIMEOUT, connect_async(&ws_url))
        .await
        .map_err(|_| MusicError::Voice(format!("Voice WS connect timed out ({ws_url})")))?
        .map_err(|e| MusicError::Voice(format!("Voice WS connect failed ({ws_url}): {e}")))?;

    let (mut write, mut read) = ws_stream.split();
    let dave_session = Arc::new(Mutex::new(None));
    let dave_protocol_version = Arc::new(AtomicU16::new(0));
    let mut dave_state = DaveGatewayState::new(
        bot_user_id,
        channel_id,
        Arc::clone(&dave_session),
        Arc::clone(&dave_protocol_version),
    );
    let mut last_seq: i64 = -1;
    let mut outgoing = Vec::new();

    // 1. Expect Opcode 8 Hello
    let heartbeat_interval_ms = loop {
        let val = recv_voice_ws_text_step(
            &mut read,
            "Hello",
            &mut last_seq,
            &mut dave_state,
            &mut outgoing,
        )
        .await?;
        flush_ws_outgoing(&mut write, &mut outgoing).await?;
        if val["op"].as_u64() == Some(8) {
            break val["d"]["heartbeat_interval"].as_u64().unwrap_or(13750);
        }
    };

    // 2. Send Opcode 0 Identify (with max_dave_protocol_version)
    let identify = build_voice_identify_payload(guild_id, bot_user_id, session_id, token);
    write
        .send(WsMessage::Text(identify.to_string()))
        .await
        .map_err(|e| MusicError::Voice(format!("Voice WS Identify send failed: {e}")))?;

    // 3. Expect Opcode 2 Ready
    let (ssrc, udp_ip, udp_port, modes) = loop {
        let val = recv_voice_ws_text_step(
            &mut read,
            "Ready",
            &mut last_seq,
            &mut dave_state,
            &mut outgoing,
        )
        .await?;
        flush_ws_outgoing(&mut write, &mut outgoing).await?;
        if val["op"].as_u64() == Some(2) {
            let d = &val["d"];
            let ssrc = d["ssrc"].as_u64().unwrap_or(0) as u32;
            let ip = d["ip"].as_str().unwrap_or("").to_string();
            let port = d["port"].as_u64().unwrap_or(0) as u16;
            let modes: Vec<String> = d["modes"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(ToString::to_string))
                        .collect()
                })
                .unwrap_or_default();
            break (ssrc, ip, port, modes);
        }
    };

    let preferred_mode = "aead_xchacha20_poly1305_rtpsize";
    if !modes.is_empty() && !modes.iter().any(|m| m == preferred_mode) {
        return Err(MusicError::Voice(format!(
            "Discord voice server does not offer {preferred_mode} (offered: {modes:?})"
        )));
    }

    // 4. UDP IP Discovery
    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    udp.connect(format!("{udp_ip}:{udp_port}")).await?;
    let discovery_req = build_ip_discovery_packet(ssrc);
    udp.send(&discovery_req).await?;
    let mut discovery_resp = [0u8; 128];
    let n = tokio::time::timeout(Duration::from_secs(5), udp.recv(&mut discovery_resp))
        .await
        .map_err(|_| MusicError::Voice("UDP IP discovery timed out".into()))??;
    let (ext_ip, ext_port) = parse_ip_discovery_response(&discovery_resp[..n])?;

    // 5. Send Opcode 1 Select Protocol
    let select_proto = json!({
        "op": 1,
        "d": {
            "protocol": "udp",
            "data": {
                "address": ext_ip,
                "port": ext_port,
                "mode": preferred_mode,
            }
        }
    });
    write
        .send(WsMessage::Text(select_proto.to_string()))
        .await
        .map_err(|e| MusicError::Voice(format!("Voice WS SelectProtocol send failed: {e}")))?;

    // 6. Expect Opcode 4 SessionDescription
    let (secret_key, negotiated_dave_ver) = loop {
        let val = recv_voice_ws_text_step(
            &mut read,
            "SessionDescription",
            &mut last_seq,
            &mut dave_state,
            &mut outgoing,
        )
        .await?;
        flush_ws_outgoing(&mut write, &mut outgoing).await?;
        if val["op"].as_u64() == Some(4) {
            let arr = val["d"]["secret_key"].as_array().ok_or_else(|| {
                MusicError::Voice("Missing secret_key in SessionDescription".into())
            })?;
            if arr.len() != 32 {
                return Err(MusicError::Voice(format!(
                    "Invalid secret_key length: {}",
                    arr.len()
                )));
            }
            let mut key = [0u8; 32];
            for (i, item) in arr.iter().enumerate() {
                key[i] = item.as_u64().unwrap_or(0) as u8;
            }
            let dave_ver = val["d"]["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
            break (key, dave_ver);
        }
    };

    if let Some(key_pkg_msg) = dave_state.init_from_session_description(negotiated_dave_ver)? {
        write
            .send(key_pkg_msg)
            .await
            .map_err(|e| MusicError::Voice(format!("Voice WS DAVE KeyPackage send failed: {e}")))?;
    }

    // 7. Spawn background Voice WS writer + v8 heartbeat + DAVE opcode reader
    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<WsMessage>();
    let shutdown = CancellationToken::new();
    let ws_shutdown = shutdown.clone();
    let hb_ms = (heartbeat_interval_ms * 8 / 10).max(2000);

    tokio::spawn(async move {
        let mut hb_interval = tokio::time::interval(Duration::from_millis(hb_ms));
        hb_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut nonce: u64 = 1;

        loop {
            tokio::select! {
                _ = ws_shutdown.cancelled() => {
                    let _ = write.close().await;
                    break;
                }
                _ = hb_interval.tick() => {
                    let hb = build_voice_heartbeat_payload(nonce, last_seq);
                    nonce = nonce.wrapping_add(1);
                    if write.send(WsMessage::Text(hb.to_string())).await.is_err() {
                        ws_shutdown.cancel();
                        break;
                    }
                }
                Some(msg) = ws_rx.recv() => {
                    if write.send(msg).await.is_err() {
                        ws_shutdown.cancel();
                        break;
                    }
                }
                incoming = read.next() => {
                    match incoming {
                        Some(Ok(WsMessage::Text(txt))) => {
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&txt) {
                                update_seq_from_json(&val, &mut last_seq);
                                let mut replies = dave_state.handle_json_event(&val);
                                if flush_ws_outgoing(&mut write, &mut replies).await.is_err() {
                                    ws_shutdown.cancel();
                                    break;
                                }
                            }
                        }
                        Some(Ok(WsMessage::Binary(bin))) => {
                            match dave_state.handle_binary_frame(&bin) {
                                Ok((maybe_seq, mut replies)) => {
                                    if let Some(seq) = maybe_seq {
                                        last_seq = seq as i64;
                                    }
                                    if flush_ws_outgoing(&mut write, &mut replies).await.is_err() {
                                        ws_shutdown.cancel();
                                        break;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("Voice WS binary frame error: {e}");
                                }
                            }
                        }
                        Some(Ok(WsMessage::Close(frame))) => {
                            tracing::warn!(
                                "Voice WS closed for guild {guild_id} ({})",
                                format_ws_close_frame(frame.as_ref())
                            );
                            ws_shutdown.cancel();
                            break;
                        }
                        None => {
                            tracing::info!("Voice WS stream ended for guild {guild_id}");
                            ws_shutdown.cancel();
                            break;
                        }
                        Some(Err(e)) => {
                            tracing::warn!("Voice WS read error for guild {guild_id}: {e}");
                            ws_shutdown.cancel();
                            break;
                        }
                        Some(Ok(WsMessage::Ping(_)))
                        | Some(Ok(WsMessage::Pong(_)))
                        | Some(Ok(WsMessage::Frame(_))) => {}
                    }
                }
            }
        }
    });

    Ok(Arc::new(GuildVoiceConnection {
        channel_id,
        ssrc,
        secret_key,
        dave_session,
        dave_protocol_version,
        udp: Arc::new(udp),
        ws_tx,
        rtp: tokio::sync::Mutex::new(RtpCounters {
            sequence: 0,
            timestamp: 0,
            nonce: 1,
        }),
        shutdown,
    }))
}

async fn play_single_track(
    conn: &Arc<GuildVoiceConnection>,
    track: &TrackMeta,
    binaries: &BinaryManager,
    cancel: CancellationToken,
    mut paused_rx: watch::Receiver<bool>,
    mut volume_rx: watch::Receiver<f32>,
) -> bool {
    let stream_url = if !should_resolve_stream(track) {
        track.url.clone()
    } else {
        match resolve_audio_stream_url(&track.url, Some(binaries.bin_dir())).await {
            Ok(url) => url,
            Err(e) => {
                tracing::warn!("Failed to resolve stream URL for '{}': {e}", track.title);
                return false;
            }
        }
    };

    let ffmpeg = match binaries.ensure_ffmpeg().await {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!("ffmpeg unavailable for '{}': {err}", track.title);
            return false;
        }
    };

    let mut elapsed_frames: u64 = 0;

    loop {
        if cancel.is_cancelled() || conn.shutdown.is_cancelled() {
            return false;
        }

        let current_vol = *volume_rx.borrow();
        let seek_secs = (elapsed_frames * OPUS_FRAME_DURATION_MS) as f64 / 1000.0;

        let mut args = vec![
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "error".to_string(),
            "-reconnect".to_string(),
            "1".to_string(),
            "-reconnect_streamed".to_string(),
            "1".to_string(),
            "-reconnect_delay_max".to_string(),
            "5".to_string(),
        ];
        if seek_secs >= 0.5 {
            args.push("-ss".to_string());
            args.push(format!("{seek_secs:.2}"));
        }
        args.extend([
            "-i".to_string(),
            stream_url.clone(),
            "-vn".to_string(),
            "-filter:a".to_string(),
            format!("volume={current_vol:.2}"),
            "-c:a".to_string(),
            "libopus".to_string(),
            "-b:a".to_string(),
            "96k".to_string(),
            "-ar".to_string(),
            "48000".to_string(),
            "-ac".to_string(),
            "2".to_string(),
            "-frame_duration".to_string(),
            "20".to_string(),
            "-f".to_string(),
            "ogg".to_string(),
            "pipe:1".to_string(),
        ]);

        let mut child = match hidden_tokio_command(&ffmpeg)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to spawn ffmpeg for '{}': {e}", track.title);
                return false;
            }
        };

        let stderr_task = child.stderr.take().map(spawn_stderr_tail);
        let Some(stdout) = child.stdout.take() else {
            return false;
        };

        conn.set_speaking(true);

        enum StreamOutcome {
            Eof,
            Cancelled,
            VolumeChanged,
            Error(String),
        }

        let frames_before = elapsed_frames;
        let outcome = tokio::select! {
            _ = cancel.cancelled() => StreamOutcome::Cancelled,
            _ = conn.shutdown.cancelled() => StreamOutcome::Cancelled,
            _ = volume_rx.changed() => StreamOutcome::VolumeChanged,
            res = stream_ogg_stdout_over_udp_with_pause(
                conn,
                stdout,
                cancel.clone(),
                &mut paused_rx,
                &mut elapsed_frames,
            ) => match res {
                Ok(true) => StreamOutcome::Eof,
                Ok(false) => StreamOutcome::Cancelled,
                Err(e) => StreamOutcome::Error(e.to_string()),
            }
        };

        match outcome {
            StreamOutcome::Eof => {
                let status = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                let stderr_tail = collect_stderr_tail(stderr_task).await;
                if let Ok(Ok(exit)) = status {
                    if !exit.success() {
                        tracing::warn!(
                            "ffmpeg exited with {exit} for '{}': {stderr_tail}",
                            track.title
                        );
                        if elapsed_frames == frames_before {
                            return false;
                        }
                    }
                } else {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                return true;
            }
            StreamOutcome::Cancelled => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                if let Some(h) = stderr_task {
                    h.abort();
                }
                return false;
            }
            StreamOutcome::VolumeChanged => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                if let Some(h) = stderr_task {
                    h.abort();
                }
                continue;
            }
            StreamOutcome::Error(e) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let stderr_tail = collect_stderr_tail(stderr_task).await;
                tracing::warn!(
                    "Audio stream error for '{}': {e} (stderr: {stderr_tail})",
                    track.title
                );
                return false;
            }
        }
    }
}

async fn stream_ogg_stdout_over_udp(
    conn: &GuildVoiceConnection,
    stdout: tokio::process::ChildStdout,
    cancel: CancellationToken,
    paused_rx: Option<watch::Receiver<bool>>,
    frames_emitted: &mut u64,
) -> Result<bool, MusicError> {
    if let Some(mut rx) = paused_rx {
        stream_ogg_stdout_over_udp_with_pause(conn, stdout, cancel, &mut rx, frames_emitted).await
    } else {
        let (_tx, mut rx) = watch::channel(false);
        stream_ogg_stdout_over_udp_with_pause(conn, stdout, cancel, &mut rx, frames_emitted).await
    }
}

async fn stream_ogg_stdout_over_udp_with_pause(
    conn: &GuildVoiceConnection,
    mut stdout: tokio::process::ChildStdout,
    cancel: CancellationToken,
    paused_rx: &mut watch::Receiver<bool>,
    elapsed_frames: &mut u64,
) -> Result<bool, MusicError> {
    let mut demuxer = OggOpusDemuxer::new();
    let mut read_buf = [0u8; 4096];
    let mut next_tick = tokio::time::Instant::now();

    loop {
        if cancel.is_cancelled() || conn.shutdown.is_cancelled() {
            return Ok(false);
        }

        let n = stdout.read(&mut read_buf).await?;
        if n == 0 {
            return Ok(true);
        }

        let frames = demuxer.feed(&read_buf[..n]);
        for frame in frames {
            while *paused_rx.borrow() {
                conn.send_silence_burst().await;
                conn.set_speaking(false);
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(false),
                    _ = conn.shutdown.cancelled() => return Ok(false),
                    changed = paused_rx.changed() => {
                        if changed.is_err() {
                            return Ok(false);
                        }
                    }
                }
                if !*paused_rx.borrow() {
                    conn.set_speaking(true);
                    next_tick = tokio::time::Instant::now();
                }
            }

            if cancel.is_cancelled() || conn.shutdown.is_cancelled() {
                return Ok(false);
            }

            tokio::time::sleep_until(next_tick).await;
            conn.send_opus_frame(&frame).await?;
            *elapsed_frames += 1;
            next_tick += Duration::from_millis(OPUS_FRAME_DURATION_MS);
            let now = tokio::time::Instant::now();
            if next_tick + Duration::from_millis(200) < now {
                next_tick = now;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn build_test_ogg_page(header_type: u8, page_seq: u32, segments: &[&[u8]]) -> Vec<u8> {
        let mut page = Vec::new();
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(header_type);
        page.extend_from_slice(&0u64.to_le_bytes());
        page.extend_from_slice(&1u32.to_le_bytes());
        page.extend_from_slice(&page_seq.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes());

        let mut lacing = Vec::new();
        let mut body = Vec::new();
        for seg in segments {
            let mut remaining = *seg;
            while remaining.len() >= 255 {
                lacing.push(255);
                body.extend_from_slice(&remaining[..255]);
                remaining = &remaining[255..];
            }
            lacing.push(remaining.len() as u8);
            body.extend_from_slice(remaining);
        }
        page.push(lacing.len() as u8);
        page.extend_from_slice(&lacing);
        page.extend_from_slice(&body);
        page
    }

    #[test]
    fn ogg_opus_demuxer_filters_headers_and_handles_large_packets() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"GARBAGE");
        stream.extend_from_slice(&build_test_ogg_page(0x02, 0, &[b"OpusHead\x01\x02"]));
        stream.extend_from_slice(&build_test_ogg_page(0x00, 1, &[b"OpusTags\x00\x00"]));

        let large_frame = vec![0xABu8; 310];
        let small_frame = vec![0xCDu8; 40];
        stream.extend_from_slice(&build_test_ogg_page(0x00, 2, &[&large_frame, &small_frame]));

        let mut demuxer = OggOpusDemuxer::new();
        let mut out = Vec::new();
        for chunk in stream.chunks(17) {
            out.extend(demuxer.feed(chunk));
        }

        assert_eq!(out.len(), 2);
        assert_eq!(out[0], large_frame);
        assert_eq!(out[1], small_frame);
    }

    #[test]
    fn rtp_framing_and_aead_xchacha20_poly1305_roundtrip() {
        let secret_key = [0x42u8; 32];
        let opus_frame = vec![0xFC, 0x01, 0x02, 0x03, 0x04, 0x05];
        let packet = encrypt_opus_rtp_packet(&secret_key, 123, 9600, 0xDEAD_BEEF, 77, &opus_frame)
            .unwrap();

        assert_eq!(packet.len(), 12 + opus_frame.len() + 16 + 4);
        let aad = &packet[..12];
        let ciphertext = &packet[12..packet.len() - 4];
        let tail_nonce = &packet[packet.len() - 4..];

        let mut nonce_24 = [0u8; 24];
        nonce_24[..4].copy_from_slice(tail_nonce);

        let cipher = XChaCha20Poly1305::new((&secret_key).into());
        let decrypted = cipher
            .decrypt((&nonce_24).into(), Payload { msg: ciphertext, aad })
            .unwrap();
        assert_eq!(decrypted, opus_frame);
    }

    #[test]
    fn dave_gateway_state_handles_opcodes_and_binary_frames() {
        let session = Arc::new(Mutex::new(None));
        let proto_ver = Arc::new(AtomicU16::new(0));
        let mut dave = DaveGatewayState::new(111, 222, Arc::clone(&session), Arc::clone(&proto_ver));

        let kp_msg = dave
            .init_from_session_description(1)
            .unwrap()
            .expect("expected KeyPackage");
        assert!(matches!(kp_msg, WsMessage::Binary(ref b) if b[0] == 26));
        assert_eq!(dave.protocol_version(), 1);

        let replies = dave.handle_json_event(&json!({
            "op": 21,
            "d": { "protocol_version": 0, "transition_id": 7 }
        }));
        assert_eq!(replies.len(), 1);

        let _ = dave.handle_json_event(&json!({
            "op": 22,
            "d": { "transition_id": 7 }
        }));
        assert_eq!(dave.protocol_version(), 0);
    }
}
