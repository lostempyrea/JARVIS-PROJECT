//! Discord channel adapter built on `serenity` (`rustls_backend`)
//! supporting 2-way chat with [`AgentRuntime`] and music commands (`!play`, `!pause`,
//! `!resume`, `!skip`, `!stop`, `!join`, `!leave`, etc.) wired to [`DualMusicController`].

use super::{ChannelError, ChannelState, ChannelStatusInfo};
use crate::agent::AgentRuntime;
use crate::config::{AppConfig, ChannelsConfig};
use crate::events::{CoreEvent, EventBus};
use crate::music::{DualMusicController, MusicTarget};
use crate::secrets::{keys, SecretStore};
use serenity::all::{
    CommandInteraction, CommandOptionType, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage, EditInteractionResponse,
    GatewayIntents, Guild, GuildId, Interaction, Message, Ready, UserId, VoiceServerUpdateEvent,
    VoiceState,
};
use serenity::async_trait;
use serenity::prelude::{Client, Context, EventHandler};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Discord message length limit.
pub const DISCORD_MAX_LEN: usize = 2000;

/// Supported JARVIS Discord `!` / `/` music & status commands.
pub const DISCORD_COMMANDS: &[&str] = &[
    "join",
    "leave",
    "play",
    "pause",
    "resume",
    "skip",
    "stop",
    "queue",
    "nowplaying",
    "volume",
    "status",
];

/// Return `true` if `cmd` (without prefix) is a supported JARVIS Discord command.
pub fn is_jarvis_discord_command(cmd: &str) -> bool {
    let normalized = cmd.trim().to_ascii_lowercase();
    DISCORD_COMMANDS.contains(&normalized.as_str())
}

/// Parse a prefix command like `!play lofi` or `/skip` into `(command, args)`.
pub fn parse_text_command(content: &str) -> Option<(String, String)> {
    let trimmed = content.trim();
    if !(trimmed.starts_with('/') || trimmed.starts_with('!')) {
        return None;
    }
    let without_prefix = &trimmed[1..];
    let mut parts = without_prefix.splitn(2, char::is_whitespace);
    let cmd = parts.next()?.trim().to_lowercase();
    if cmd.is_empty() {
        return None;
    }
    let args = parts.next().unwrap_or("").trim().to_string();
    Some((cmd, args))
}

/// Check whether a Discord user ID is allowed by `discord_allowed_user_ids`.
/// Strict whitelist (fail-closed): returns `true` ONLY when `user_id` is explicitly listed.
pub fn is_discord_user_allowed(cfg: &ChannelsConfig, user_id: u64) -> bool {
    cfg.discord_allowed_user_ids.contains(&user_id)
}

/// Decide whether a Discord guild message should be handled by the bot.
pub fn should_handle_guild_message(
    content: &str,
    mentions_bot: bool,
    bot_user_id: Option<u64>,
) -> bool {
    if mentions_bot {
        return true;
    }
    if let Some(id) = bot_user_id {
        if content.contains(&format!("<@{id}>")) || content.contains(&format!("<@!{id}>")) {
            return true;
        }
    }
    if let Some((cmd, _)) = parse_text_command(content) {
        if is_jarvis_discord_command(&cmd) {
            return true;
        }
    }
    false
}

/// Strip raw `<@bot_id>` / `<@!bot_id>` mention tokens from message text.
pub fn strip_bot_mention(content: &str, bot_user_id: Option<u64>) -> String {
    let Some(id) = bot_user_id else {
        return content.trim().to_string();
    };
    content
        .replace(&format!("<@{id}>"), "")
        .replace(&format!("<@!{id}>"), "")
        .trim()
        .to_string()
}

/// Split a long message into chunks of at most `max_len` characters.
pub fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let limit = max_len.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.chars().count() >= limit {
            chunks.push(current);
            current = String::new();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Truncate a single Discord response cleanly at a UTF-8 char boundary.
pub fn truncate_discord_message(text: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }
    if text.chars().count() <= max_len {
        return text.to_string();
    }
    if max_len <= 3 {
        return text.chars().take(max_len).collect();
    }
    let keep = max_len - 3;
    let end_byte = text
        .char_indices()
        .nth(keep)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    format!("{}...", &text[..end_byte])
}

/// Translate Discord gateway close/disallowed-intent errors into actionable messages.
pub fn format_discord_gateway_error(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if raw.contains("4014") || lower.contains("disallowed") || lower.contains("privileged") {
        "Missing privileged MESSAGE_CONTENT intent in Discord Developer Portal".to_string()
    } else {
        raw.to_string()
    }
}

/// Build Discord slash command definitions wired to [`DualMusicController`].
pub fn build_slash_commands() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("join")
            .description("Join your current voice channel (or a specific channel ID)")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "channel",
                    "Voice channel ID (optional)",
                )
                .required(false),
            ),
        CreateCommand::new("leave").description("Leave the voice channel and clear the queue"),
        CreateCommand::new("play")
            .description("Play or queue a song by URL or search query")
            .add_option(
                CreateCommandOption::new(CommandOptionType::String, "query", "Song title or URL")
                    .required(true),
            ),
        CreateCommand::new("pause").description("Pause the current track"),
        CreateCommand::new("resume").description("Resume playback"),
        CreateCommand::new("skip").description("Skip to the next track in the queue"),
        CreateCommand::new("stop").description("Stop playback and clear the queue"),
        CreateCommand::new("queue").description("Show the current playback queue"),
        CreateCommand::new("nowplaying").description("Show the currently playing track"),
        CreateCommand::new("volume")
            .description("Set playback volume (0.0 to 2.0)")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Number,
                    "level",
                    "Volume multiplier (0.0 - 2.0)",
                )
                .required(true)
                .min_number_value(0.0)
                .max_number_value(2.0),
            ),
        CreateCommand::new("status").description("Show JARVIS Discord music & bot status"),
    ]
}

type StartupSignalTx = watch::Sender<Option<Result<(), String>>>;

struct DiscordHandler {
    config: Arc<RwLock<AppConfig>>,
    agent: AgentRuntime,
    music: Arc<DualMusicController>,
    event_bus: EventBus,
    status: Arc<Mutex<ChannelStatusInfo>>,
    startup_tx: Mutex<Option<StartupSignalTx>>,
}

impl DiscordHandler {
    fn channels_cfg(&self) -> ChannelsConfig {
        self.config
            .read()
            .map(|c| c.channels.clone())
            .unwrap_or_default()
    }

    fn publish_status(&self) {
        let snap = self.status.lock().expect("status mutex poisoned").clone();
        self.event_bus.publish(CoreEvent::ChannelStatusChanged {
            channel: "discord".into(),
            state: format!("{:?}", snap.state).to_ascii_lowercase(),
            detail: snap.last_error.or(snap.connected_as),
        });
    }

    fn signal_startup(&self, res: Result<(), String>) {
        if let Some(tx) = self
            .startup_tx
            .lock()
            .expect("startup_tx mutex poisoned")
            .take()
        {
            let _ = tx.send(Some(res));
        }
    }

    async fn execute_music_command(
        &self,
        cmd: &str,
        args: &str,
        guild_id: u64,
        user_id: u64,
        requester: &str,
    ) -> Result<String, String> {
        let vm = self.music.voice_manager();
        let gq = self.music.guild_queue();
        let gid_opt = (guild_id > 0).then_some(guild_id);

        match cmd {
            "join" => {
                let explicit_cid = args.trim().parse::<u64>().ok().filter(|&id| id > 0);
                let (joined_gid, cid, _) = if explicit_cid.is_some() {
                    self.music
                        .auto_join_discord_voice(gid_opt, explicit_cid)
                        .await
                        .map_err(|e| e.to_string())?
                } else if let Some((found_gid, found_cid)) =
                    vm.find_user_voice_location(&[user_id], gid_opt)
                {
                    let snap = self
                        .music
                        .join_discord_voice(Some(found_gid), found_cid)
                        .await
                        .map_err(|e| e.to_string())?;
                    (found_gid, found_cid, snap)
                } else {
                    self.music
                        .auto_join_discord_voice(gid_opt, None)
                        .await
                        .map_err(|e| e.to_string())?
                };
                if let Some(name) = vm.guild_name(joined_gid) {
                    Ok(format!("Joined voice channel <#{cid}> in **{name}**"))
                } else {
                    Ok(format!("Joined voice channel <#{cid}>"))
                }
            }
            "leave" => {
                self.music
                    .leave_discord_voice(gid_opt)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok("Left voice channel and cleared queue.".into())
            }
            "play" => {
                let query = args.trim();
                if query.is_empty() {
                    return Err("Provide a song title or URL.".into());
                }
                let play_gid_opt = if let Some((user_gid, user_cid)) =
                    vm.find_user_voice_location(&[user_id], gid_opt)
                {
                    if vm.connected_channel(user_gid).await != Some(user_cid) {
                        self.music
                            .join_discord_voice(Some(user_gid), user_cid)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    Some(user_gid)
                } else {
                    let check_gid = gid_opt.or_else(|| vm.active_guild_id());
                    let is_connected = match check_gid {
                        Some(g) => vm.connected_channel(g).await.is_some(),
                        None => false,
                    };
                    if !is_connected {
                        return Err("Join a voice channel first or run `!join`.".into());
                    }
                    check_gid
                };
                let outcomes = self
                    .music
                    .play(MusicTarget::DiscordVoice, query, requester, play_gid_opt)
                    .await
                    .map_err(|e| e.to_string())?;
                if let Some((pos, snap)) = outcomes.into_iter().next() {
                    let title = snap
                        .current
                        .as_ref()
                        .map(|t| t.title.clone())
                        .unwrap_or_else(|| query.to_string());
                    if pos == 0 {
                        Ok(format!("Now playing: **{title}**"))
                    } else {
                        let queued_title = snap
                            .upcoming
                            .last()
                            .map(|t| t.title.clone())
                            .unwrap_or(title);
                        Ok(format!("Queued **{queued_title}** at position #{pos}"))
                    }
                } else {
                    Ok("Track enqueued.".into())
                }
            }
            "pause" => {
                self.music
                    .pause(MusicTarget::DiscordVoice, gid_opt)
                    .map_err(|e| e.to_string())?;
                Ok("Playback paused.".into())
            }
            "resume" => {
                self.music
                    .resume(MusicTarget::DiscordVoice, gid_opt)
                    .map_err(|e| e.to_string())?;
                Ok("Playback resumed.".into())
            }
            "skip" => {
                let outcomes = self
                    .music
                    .skip(MusicTarget::DiscordVoice, gid_opt)
                    .map_err(|e| e.to_string())?;
                if let Some((skipped, next)) = outcomes.into_iter().next() {
                    let skipped_name = skipped.map(|t| t.title).unwrap_or_else(|| "track".into());
                    match next {
                        Some(n) => Ok(format!(
                            "Skipped **{skipped_name}**. Now playing: **{}**",
                            n.title
                        )),
                        None => Ok(format!("Skipped **{skipped_name}**. Queue is now empty.")),
                    }
                } else {
                    Ok("Skipped.".into())
                }
            }
            "stop" => {
                self.music.stop(MusicTarget::DiscordVoice, gid_opt);
                Ok("Playback stopped and queue cleared.".into())
            }
            "queue" => {
                let resolved_gid =
                    gid_opt.unwrap_or_else(|| self.music.active_or_default_guild_id());
                let snap = gq.snapshot(resolved_gid);
                if snap.current.is_none() && snap.upcoming.is_empty() {
                    return Ok("The queue is empty.".into());
                }
                let mut lines = Vec::new();
                if let Some(cur) = snap.current {
                    lines.push(format!("Now playing: **{}** ({:?})", cur.title, snap.state));
                }
                for (i, t) in snap.upcoming.iter().enumerate() {
                    lines.push(format!("{}. {}", i + 1, t.title));
                }
                Ok(lines.join("\n"))
            }
            "nowplaying" => {
                let resolved_gid =
                    gid_opt.unwrap_or_else(|| self.music.active_or_default_guild_id());
                match gq.now_playing(resolved_gid) {
                    Some(t) => Ok(format!("Now playing: **{}** ({})", t.title, t.webpage_url)),
                    None => Ok("Nothing is currently playing.".into()),
                }
            }
            "volume" => {
                let vol: f32 = args
                    .trim()
                    .parse()
                    .map_err(|_| "Provide volume between 0.0 and 2.0".to_string())?;
                let applied = self
                    .music
                    .set_volume(MusicTarget::DiscordVoice, vol, gid_opt)
                    .map_err(|e| e.to_string())?;
                Ok(format!("Volume set to {:.0}%", applied * 100.0))
            }
            "status" => {
                let resolved_gid =
                    gid_opt.unwrap_or_else(|| self.music.active_or_default_guild_id());
                let snap = gq.snapshot(resolved_gid);
                Ok(format!(
                    "State: {:?} | Volume: {:.0}% | Voice: {:?} | Queue: {} upcoming",
                    snap.state,
                    snap.volume * 100.0,
                    snap.connected_channel_id,
                    snap.upcoming.len()
                ))
            }
            other => Err(format!("Unknown music command: {other}")),
        }
    }
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        self.music
            .voice_manager()
            .set_ready(ready.user.id.get(), ctx.shard.clone());

        {
            let mut guard = self.status.lock().expect("discord status mutex poisoned");
            guard.state = ChannelState::Online;
            guard.connected_as = Some(ready.user.name.clone());
            guard.last_error = None;
        }
        self.publish_status();
        self.signal_startup(Ok(()));

        let cfg = self.channels_cfg();
        if let Some(guild_id) = cfg.discord_guild_id.filter(|&id| id > 0) {
            let vol = cfg.discord_default_volume.clamp(0.0, 2.0);
            let _ = self.music.guild_queue().set_volume(guild_id, vol);
            let cmds = build_slash_commands();
            if let Err(e) = GuildId::new(guild_id).set_commands(&ctx.http, cmds).await {
                tracing::error!(
                    "Failed to register Discord guild slash commands for {guild_id}: {e}"
                );
            }
        } else {
            let cmds = build_slash_commands();
            if let Err(e) = serenity::all::Command::set_global_commands(&ctx.http, cmds).await {
                tracing::error!("Failed to register Discord global slash commands: {e}");
            }
        }
    }

    async fn guild_create(&self, _ctx: Context, guild: Guild, _is_new: Option<bool>) {
        self.music.voice_manager().on_guild_create(&guild);

        let cfg = self.channels_cfg();
        if !cfg.discord_auto_join_voice {
            return;
        }
        let gid = guild.id.get();
        if let Some(target_gid) = cfg.discord_guild_id.filter(|&id| id > 0) {
            if gid != target_gid {
                return;
            }
        }
        if self.music.voice_manager().connected_channel(gid).await.is_some() {
            return;
        }
        if let Some(cid) = self
            .music
            .voice_manager()
            .pick_auto_join_channel(gid, &cfg.discord_allowed_user_ids)
        {
            let music = Arc::clone(&self.music);
            tokio::spawn(async move {
                if let Err(e) = music.join_discord_voice(Some(gid), cid).await {
                    tracing::warn!("Discord auto_join_voice failed for guild {gid}: {e}");
                }
            });
        }
    }

    async fn voice_state_update(&self, _ctx: Context, _old: Option<VoiceState>, new: VoiceState) {
        let vm = self.music.voice_manager();
        let bot_id = vm.bot_user_id();
        vm.on_voice_state_update(&new);

        if Some(new.user_id.get()) == bot_id && new.channel_id.is_none() {
            if let Some(gid) = new.guild_id {
                self.music.guild_queue().leave_voice(gid.get());
            }
            return;
        }

        let cfg = self.channels_cfg();
        if cfg.discord_auto_join_voice && Some(new.user_id.get()) != bot_id {
            let uid = new.user_id.get();
            if is_discord_user_allowed(&cfg, uid) {
                if let (Some(gid), Some(cid)) = (
                    new.guild_id.map(|g| g.get()),
                    new.channel_id.map(|c| c.get()),
                ) {
                    let guild_allowed = cfg
                        .discord_guild_id
                        .filter(|&id| id > 0)
                        .is_none_or(|target| target == gid);
                    if guild_allowed && vm.connected_channel(gid).await != Some(cid) {
                        let music = Arc::clone(&self.music);
                        tokio::spawn(async move {
                            if let Err(e) = music.join_discord_voice(Some(gid), cid).await {
                                tracing::warn!(
                                    "Discord auto_join_voice on voice_state_update failed for guild {gid}: {e}"
                                );
                            }
                        });
                    }
                }
            }
        }
    }

    async fn voice_server_update(&self, _ctx: Context, event: VoiceServerUpdateEvent) {
        if let Some(guild_id) = event.guild_id {
            self.music.voice_manager().on_voice_server_update(
                guild_id.get(),
                event.endpoint,
                event.token,
            );
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let cfg = self.channels_cfg();
        let user_id = msg.author.id.get();
        let bot_id = self.music.voice_manager().bot_user_id();
        let mentions_bot = bot_id.is_some_and(|id| msg.mentions_user_id(UserId::new(id)));

        if msg.guild_id.is_some()
            && !should_handle_guild_message(&msg.content, mentions_bot, bot_id)
        {
            return;
        }

        if !is_discord_user_allowed(&cfg, user_id) {
            self.event_bus.publish(CoreEvent::ChannelPairingCandidate {
                channel: "discord".into(),
                sender_id: user_id.to_string(),
                display_name: format!("@{}", msg.author.name),
                preview: msg.content.chars().take(60).collect(),
            });
            let _ = msg
                .channel_id
                .say(
                    &ctx.http,
                    format!("Unauthorized. Your Discord User ID is: {user_id}."),
                )
                .await;
            return;
        }

        if let Some((cmd, args)) = parse_text_command(&msg.content) {
            if is_jarvis_discord_command(&cmd) {
                let gid = msg
                    .guild_id
                    .map(|g| g.get())
                    .or(cfg.discord_guild_id)
                    .unwrap_or(0);
                let reply_text = match self
                    .execute_music_command(&cmd, &args, gid, user_id, &msg.author.name)
                    .await
                {
                    Ok(ok) => ok,
                    Err(e) => format!("Error: {e}"),
                };
                for chunk in split_message(&reply_text, DISCORD_MAX_LEN) {
                    let _ = msg.channel_id.say(&ctx.http, chunk).await;
                }
                return;
            }
        }

        let cleaned_text = if msg.guild_id.is_some() {
            strip_bot_mention(&msg.content, bot_id)
        } else {
            msg.content.trim().to_string()
        };
        if cleaned_text.is_empty() {
            return;
        }

        let session_id = format!("discord-{}", msg.channel_id.get());
        match self
            .agent
            .handle_user_prompt(&session_id, "discord", &cleaned_text)
            .await
        {
            Ok(outcome) => {
                let reply = if outcome.final_text.trim().is_empty() {
                    "(done)".to_string()
                } else {
                    outcome.final_text
                };
                for chunk in split_message(&reply, DISCORD_MAX_LEN) {
                    if let Err(e) = msg.channel_id.say(&ctx.http, chunk).await {
                        tracing::warn!("Discord failed to send message reply: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Discord AI routing failed: {e}");
                let _ = msg.channel_id.say(&ctx.http, format!("Error: {e}")).await;
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(cmd) = interaction {
            self.handle_slash_command(&ctx, &cmd).await;
        }
    }
}

impl DiscordHandler {
    async fn handle_slash_command(&self, ctx: &Context, cmd: &CommandInteraction) {
        let cfg = self.channels_cfg();
        let user_id = cmd.user.id.get();
        if !is_discord_user_allowed(&cfg, user_id) {
            self.event_bus.publish(CoreEvent::ChannelPairingCandidate {
                channel: "discord".into(),
                sender_id: user_id.to_string(),
                display_name: format!("@{}", cmd.user.name),
                preview: format!("/{}", cmd.data.name).chars().take(60).collect(),
            });
            let resp = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!("Unauthorized. Your Discord User ID is: {user_id}."))
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, resp).await;
            return;
        }

        let is_slow = matches!(cmd.data.name.as_str(), "play" | "join");
        if is_slow {
            let _ = cmd.defer(&ctx.http).await;
        }

        let args = cmd
            .data
            .options
            .first()
            .and_then(|opt| {
                opt.value
                    .as_str()
                    .map(ToString::to_string)
                    .or_else(|| opt.value.as_f64().map(|v| v.to_string()))
                    .or_else(|| opt.value.as_i64().map(|v| v.to_string()))
            })
            .unwrap_or_default();

        let gid = cmd
            .guild_id
            .map(|g| g.get())
            .or(cfg.discord_guild_id)
            .unwrap_or(0);

        let raw_content = match self
            .execute_music_command(&cmd.data.name, &args, gid, user_id, &cmd.user.name)
            .await
        {
            Ok(ok) => ok,
            Err(e) => format!("Error: {e}"),
        };
        let content = truncate_discord_message(&raw_content, DISCORD_MAX_LEN);

        if is_slow {
            let _ = cmd
                .edit_response(&ctx.http, EditInteractionResponse::new().content(&content))
                .await;
        } else {
            let resp = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new().content(content),
            );
            let _ = cmd.create_response(&ctx.http, resp).await;
        }
    }
}

/// Discord channel adapter managing `serenity` Gateway lifecycle and voice music control.
pub struct DiscordAdapter {
    config: Arc<RwLock<AppConfig>>,
    secrets: Arc<dyn SecretStore>,
    agent: AgentRuntime,
    music: Arc<DualMusicController>,
    event_bus: EventBus,
    status: Arc<Mutex<ChannelStatusInfo>>,
    cancel: Mutex<Option<CancellationToken>>,
}

impl DiscordAdapter {
    pub fn new(
        config: Arc<RwLock<AppConfig>>,
        secrets: Arc<dyn SecretStore>,
        agent: AgentRuntime,
        music: Arc<DualMusicController>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            config,
            secrets,
            agent,
            music,
            event_bus,
            status: Arc::new(Mutex::new(ChannelStatusInfo::offline("discord"))),
            cancel: Mutex::new(None),
        }
    }

    fn resolve_token(&self) -> Result<String, ChannelError> {
        let token = self
            .secrets
            .get(keys::DISCORD_BOT_TOKEN)
            .map_err(|e| ChannelError::Config(e.to_string()))?
            .unwrap_or_default();
        let trimmed = token.trim().to_string();
        if trimmed.is_empty() {
            return Err(ChannelError::Config(
                "Discord bot token is not configured in SecretStore".into(),
            ));
        }
        Ok(trimmed)
    }

    fn update_and_publish_status(&self, state: ChannelState, err: Option<String>) {
        {
            let mut st = self.status.lock().expect("status mutex poisoned");
            st.state = state;
            st.last_error = err.clone();
            if state == ChannelState::Offline {
                st.connected_as = None;
            }
        }
        let snap = self.status();
        self.event_bus.publish(CoreEvent::ChannelStatusChanged {
            channel: "discord".into(),
            state: format!("{:?}", snap.state).to_ascii_lowercase(),
            detail: err.or(snap.connected_as),
        });
    }

    pub fn status(&self) -> ChannelStatusInfo {
        let mut st = self.status.lock().expect("status mutex poisoned").clone();
        let gid = self
            .config
            .read()
            .ok()
            .and_then(|c| c.channels.discord_guild_id)
            .filter(|&id| id > 0)
            .or_else(|| self.music.voice_manager().active_guild_id());
        st.active_target = self.music.guild_queue().active_voice_channel(gid);
        st
    }

    pub async fn start(&self) -> Result<(), ChannelError> {
        let token = self.resolve_token()?;
        let full_intents = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_VOICE_STATES
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        match self.start_with_intents(&token, full_intents).await {
            Ok(()) => Ok(()),
            Err(ChannelError::Adapter(_, msg))
                if msg.contains("MESSAGE_CONTENT")
                    || msg.contains("4014")
                    || msg.to_ascii_lowercase().contains("disallowed")
                    || msg.to_ascii_lowercase().contains("privileged") =>
            {
                tracing::warn!(
                    "Discord Gateway disallowed MESSAGE_CONTENT intent (4014); retrying without MESSAGE_CONTENT"
                );
                let fallback_intents = GatewayIntents::GUILDS
                    | GatewayIntents::GUILD_VOICE_STATES
                    | GatewayIntents::GUILD_MESSAGES
                    | GatewayIntents::DIRECT_MESSAGES;
                self.start_with_intents(&token, fallback_intents).await
            }
            Err(err) => Err(err),
        }
    }

    async fn start_with_intents(
        &self,
        token: &str,
        intents: GatewayIntents,
    ) -> Result<(), ChannelError> {
        let cancel_token = CancellationToken::new();
        {
            let mut guard = self.cancel.lock().expect("cancel mutex poisoned");
            if let Some(prev) = guard.replace(cancel_token.clone()) {
                prev.cancel();
            }
        }
        self.update_and_publish_status(ChannelState::Connecting, None);

        self.music.start_discord_voice_loop(cancel_token.clone());

        let (startup_tx, mut startup_rx) = watch::channel::<Option<Result<(), String>>>(None);
        let startup_tx_err = startup_tx.clone();

        let handler = DiscordHandler {
            config: Arc::clone(&self.config),
            agent: self.agent.clone(),
            music: Arc::clone(&self.music),
            event_bus: self.event_bus.clone(),
            status: Arc::clone(&self.status),
            startup_tx: Mutex::new(Some(startup_tx)),
        };

        let mut client = match Client::builder(token, intents).event_handler(handler).await {
            Ok(c) => c,
            Err(e) => {
                cancel_token.cancel();
                self.music.guild_queue().set_voice_controller(None);
                let formatted = format_discord_gateway_error(&e.to_string());
                self.update_and_publish_status(ChannelState::Error, Some(formatted.clone()));
                return Err(ChannelError::Adapter("discord".into(), formatted));
            }
        };

        let shard_manager = Arc::clone(&client.shard_manager);
        let status_ref = Arc::clone(&self.status);
        let bus_ref = self.event_bus.clone();
        let music_ref = Arc::clone(&self.music);
        let spawn_cancel = cancel_token.clone();

        tokio::spawn(async move {
            tokio::select! {
                _ = spawn_cancel.cancelled() => {
                    music_ref.guild_queue().set_voice_controller(None);
                    music_ref.voice_manager().shutdown_all().await;
                    shard_manager.shutdown_all().await;
                }
                res = client.start() => {
                    spawn_cancel.cancel();
                    music_ref.guild_queue().set_voice_controller(None);
                    music_ref.voice_manager().shutdown_all().await;
                    let (state_str, detail) = {
                        let mut st = status_ref.lock().expect("status mutex poisoned");
                        st.connected_as = None;
                        match res {
                            Err(e) => {
                                let msg = format_discord_gateway_error(&e.to_string());
                                st.state = ChannelState::Error;
                                st.last_error = Some(msg.clone());
                                let _ = startup_tx_err.send(Some(Err(msg.clone())));
                                ("error".to_string(), Some(msg))
                            }
                            Ok(()) => {
                                st.state = if st.last_error.is_some() {
                                    ChannelState::Error
                                } else {
                                    ChannelState::Offline
                                };
                                (
                                    format!("{:?}", st.state).to_ascii_lowercase(),
                                    st.last_error.clone(),
                                )
                            }
                        }
                    };
                    bus_ref.publish(CoreEvent::ChannelStatusChanged {
                        channel: "discord".into(),
                        state: state_str,
                        detail,
                    });
                }
            }
        });

        match tokio::time::timeout(Duration::from_secs(8), startup_rx.wait_for(|v| v.is_some())).await
        {
            Ok(Ok(guard)) => {
                if let Some(Err(ref err_msg)) = *guard {
                    cancel_token.cancel();
                    return Err(ChannelError::Adapter("discord".into(), err_msg.clone()));
                }
            }
            Ok(Err(_)) => {}
            Err(_elapsed) => {
                tracing::info!("Discord gateway still connecting after 8s; continuing in background");
            }
        }

        Ok(())
    }

    pub async fn stop(&self) -> Result<(), ChannelError> {
        {
            let mut guard = self.cancel.lock().expect("cancel mutex poisoned");
            if let Some(tok) = guard.take() {
                tok.cancel();
            }
        }
        self.music.guild_queue().set_voice_controller(None);
        self.music.voice_manager().shutdown_all().await;
        self.update_and_publish_status(ChannelState::Offline, None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_command_parsing_and_guild_filtering() {
        assert_eq!(
            parse_text_command("!play Lofi Hip Hop"),
            Some(("play".into(), "Lofi Hip Hop".into()))
        );
        assert_eq!(parse_text_command("!skip"), Some(("skip".into(), "".into())));
        assert_eq!(parse_text_command("hello world"), None);

        assert!(should_handle_guild_message("!play lofi", false, Some(42)));
        assert!(should_handle_guild_message("!join 123", false, Some(42)));
        assert!(should_handle_guild_message("<@42> xin chao", false, Some(42)));
        assert!(!should_handle_guild_message("!otherbot help", false, Some(42)));
        assert!(!should_handle_guild_message("random chat", false, Some(42)));
        assert_eq!(strip_bot_mention("<@!42> play music", Some(42)), "play music");

        let mut cfg = ChannelsConfig::default();
        assert!(!is_discord_user_allowed(&cfg, 999));
        cfg.discord_allowed_user_ids = vec![111, 222];
        assert!(is_discord_user_allowed(&cfg, 111));
        assert!(!is_discord_user_allowed(&cfg, 999));
    }
}
