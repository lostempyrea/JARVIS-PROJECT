//! Pure-Rust Gmail integration supporting App Password over TLS
//! (`async-imap` + `tokio-native-tls` at `imap.gmail.com:993`, `mail-parser`,
//! `lettre` SMTP TLS at `smtp.gmail.com:465`) and OAuth2 `XOAUTH2`.
//!
//! Exposes 4 tools for [`ToolRegistry`]:
//! - `gmail_search` (`SafeReadOnly`)
//! - `gmail_read` (`SafeReadOnly`)
//! - `gmail_summarize_unread` (`SafeReadOnly`)
//! - `gmail_send_confirmed` (`RequiresApproval`, ALWAYS gated by `ApprovalBroker`)

use crate::config::AppConfig;
use crate::secrets::{keys, SecretStore};
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use base64::Engine as _;
use futures::TryStreamExt;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as EmailMessage, Tokio1Executor};
use mail_parser::{Addr, MessageParser};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_native_tls::{TlsConnector, TlsStream};

pub const GMAIL_IMAP_HOST: &str = "imap.gmail.com";
pub const GMAIL_IMAP_PORT: u16 = 993;
pub const GMAIL_SMTP_HOST: &str = "smtp.gmail.com";
pub const GMAIL_SMTP_PORT: u16 = 465;
pub const GMAIL_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

#[derive(Debug, Error)]
pub enum GmailError {
    #[error("Gmail configuration error: {0}")]
    Config(String),
    #[error("TLS/network error: {0}")]
    Network(String),
    #[error("IMAP error: {0}")]
    Imap(#[from] async_imap::error::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SMTP error: {0}")]
    Smtp(String),
    #[error("Email parse error: {0}")]
    Parse(String),
    #[error("OAuth2 error: {0}")]
    OAuth2(String),
}

/// Gmail authentication mode resolved from [`AppConfig`] and [`SecretStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GmailAuth {
    AppPassword { email: String, password: String },
    OAuth2 { email: String, access_token: String },
}

/// Format the raw SASL `XOAUTH2` string: `user=<email>\x01auth=Bearer <token>\x01\x01`.
pub fn format_xoauth2_sasl(email: &str, access_token: &str) -> String {
    format!(
        "user={}\x01auth=Bearer {}\x01\x01",
        email.trim(),
        access_token.trim()
    )
}

/// Format the base64-encoded `XOAUTH2` token payload.
pub fn format_xoauth2_base64(email: &str, access_token: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format_xoauth2_sasl(email, access_token))
}

/// `async-imap` SASL `XOAUTH2` authenticator.
struct GmailXoauth2Auth {
    email: String,
    access_token: String,
}

impl async_imap::Authenticator for &GmailXoauth2Auth {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        format_xoauth2_sasl(&self.email, &self.access_token)
    }
}

/// Parsed RFC822 email returned by [`parse_rfc822_email`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedEmail {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub to: Vec<String>,
    pub date: Option<String>,
    pub body_text: String,
    pub snippet: String,
}

fn format_addr(addr: &Addr<'_>) -> String {
    match (addr.name(), addr.address()) {
        (Some(name), Some(email)) if !name.trim().is_empty() => format!("{name} <{email}>"),
        (_, Some(email)) => email.to_string(),
        (Some(name), None) => name.to_string(),
        (None, None) => String::new(),
    }
}

fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn collapse_snippet(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

/// Parse raw RFC822 email bytes using `mail-parser`.
pub fn parse_rfc822_email(uid: u32, raw: &[u8]) -> Result<ParsedEmail, GmailError> {
    let msg = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| GmailError::Parse(format!("Failed to parse RFC822 message uid {uid}")))?;

    let subject = msg.subject().unwrap_or("(no subject)").to_string();
    let from = msg
        .from()
        .and_then(|addrs| addrs.first())
        .map(format_addr)
        .unwrap_or_else(|| "(unknown sender)".to_string());

    let to = msg
        .to()
        .map(|addrs| {
            addrs
                .iter()
                .map(format_addr)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let date = msg.date().map(|d| d.to_rfc3339());
    let body_text = msg
        .body_text(0)
        .map(|cow| cow.into_owned())
        .or_else(|| msg.body_html(0).map(|cow| strip_html_tags(&cow)))
        .unwrap_or_default()
        .trim()
        .to_string();

    let snippet = collapse_snippet(&body_text, 240);

    Ok(ParsedEmail {
        uid,
        subject,
        from,
        to,
        date,
        body_text,
        snippet,
    })
}

/// Service handling Gmail IMAP read/search and SMTP sending.
#[derive(Clone)]
pub struct GmailClient {
    config: Arc<RwLock<AppConfig>>,
    secrets: Arc<dyn SecretStore>,
}

impl GmailClient {
    pub fn new(config: Arc<RwLock<AppConfig>>, secrets: Arc<dyn SecretStore>) -> Self {
        Self { config, secrets }
    }

    pub fn is_configured(&self) -> bool {
        let Ok(cfg) = self.config.read() else {
            return false;
        };
        let Some(addr) = cfg.channels.gmail_address.as_deref() else {
            return false;
        };
        if addr.trim().is_empty() {
            return false;
        }
        if cfg.channels.gmail_auth_mode.eq_ignore_ascii_case("oauth2") {
            self.secrets
                .has(keys::GMAIL_OAUTH_REFRESH_TOKEN)
                .unwrap_or(false)
        } else {
            self.secrets.has(keys::GMAIL_APP_PASSWORD).unwrap_or(false)
        }
    }

    async fn resolve_auth(&self) -> Result<GmailAuth, GmailError> {
        let (email, mode, client_id) = {
            let cfg = self
                .config
                .read()
                .map_err(|_| GmailError::Config("config lock poisoned".into()))?;
            let email = cfg
                .channels
                .gmail_address
                .clone()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    GmailError::Config("channels.gmail_address is not configured".into())
                })?;
            (
                email,
                cfg.channels.gmail_auth_mode.clone(),
                cfg.channels.gmail_oauth_client_id.clone(),
            )
        };

        if mode.eq_ignore_ascii_case("oauth2") {
            let stored = self
                .secrets
                .get(keys::GMAIL_OAUTH_REFRESH_TOKEN)
                .map_err(|e| GmailError::Config(e.to_string()))?
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    GmailError::Config(
                        "Gmail OAuth2 token missing in SecretStore (gmail_oauth_refresh_token)"
                            .into(),
                    )
                })?;

            let client_secret = self
                .secrets
                .get(keys::GMAIL_OAUTH_CLIENT_SECRET)
                .map_err(|e| GmailError::Config(e.to_string()))?;

            let access_token = match (client_id, client_secret) {
                (Some(cid), Some(csec)) if !cid.trim().is_empty() && !csec.trim().is_empty() => {
                    exchange_refresh_token(&cid, &csec, &stored).await?
                }
                _ => stored,
            };

            Ok(GmailAuth::OAuth2 {
                email,
                access_token,
            })
        } else {
            let password = self
                .secrets
                .get(keys::GMAIL_APP_PASSWORD)
                .map_err(|e| GmailError::Config(e.to_string()))?
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    GmailError::Config(
                        "Gmail App Password missing in SecretStore (gmail_app_password)".into(),
                    )
                })?;
            Ok(GmailAuth::AppPassword { email, password })
        }
    }

    async fn connect_imap(
        &self,
    ) -> Result<async_imap::Session<TlsStream<TcpStream>>, GmailError> {
        let auth = self.resolve_auth().await?;
        let tcp = TcpStream::connect((GMAIL_IMAP_HOST, GMAIL_IMAP_PORT))
            .await
            .map_err(|e| GmailError::Network(format!("IMAP TCP connect failed: {e}")))?;
        let native_tls = native_tls::TlsConnector::new()
            .map_err(|e| GmailError::Network(format!("TLS init failed: {e}")))?;
        let connector = TlsConnector::from(native_tls);
        let tls_stream = connector
            .connect(GMAIL_IMAP_HOST, tcp)
            .await
            .map_err(|e| GmailError::Network(format!("IMAP TLS handshake failed: {e}")))?;

        let mut client = async_imap::Client::new(tls_stream);
        let _greeting = client.read_response().await?;

        let session = match auth {
            GmailAuth::AppPassword { email, password } => client
                .login(email, password)
                .await
                .map_err(|(e, _)| GmailError::Imap(e))?,
            GmailAuth::OAuth2 {
                email,
                access_token,
            } => {
                let xoauth2 = GmailXoauth2Auth {
                    email,
                    access_token,
                };
                client
                    .authenticate("XOAUTH2", &xoauth2)
                    .await
                    .map_err(|(e, _)| GmailError::Imap(e))?
            }
        };

        Ok(session)
    }

    /// Search INBOX messages matching an IMAP query or plain text keyword.
    pub async fn search_inbox(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ParsedEmail>, GmailError> {
        let mut session = self.connect_imap().await?;
        session.select("INBOX").await?;

        let criteria = build_imap_search_criteria(query);
        let uids = session.uid_search(&criteria).await?;
        let mut sorted: Vec<u32> = uids.into_iter().collect();
        sorted.sort_unstable();
        let take_uids: Vec<u32> = sorted.into_iter().rev().take(limit.clamp(1, 25)).collect();

        let emails = fetch_uids(&mut session, &take_uids).await?;
        let _ = session.logout().await;
        Ok(emails)
    }

    /// Fetch and parse a single email by IMAP UID.
    pub async fn read_by_uid(&self, uid: u32) -> Result<ParsedEmail, GmailError> {
        let mut session = self.connect_imap().await?;
        session.select("INBOX").await?;

        let mut emails = fetch_uids(&mut session, &[uid]).await?;
        let _ = session.logout().await;
        emails
            .pop()
            .ok_or_else(|| GmailError::Imap(async_imap::error::Error::Bad(format!("UID {uid} not found"))))
    }

    /// Fetch and parse recent unread (`UNSEEN`) messages for summarization.
    pub async fn fetch_unread(&self, limit: usize) -> Result<Vec<ParsedEmail>, GmailError> {
        self.search_inbox("UNSEEN", limit).await
    }

    /// Send an email over `smtp.gmail.com:465` using TLS (`lettre`).
    pub async fn send_email(
        &self,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<String, GmailError> {
        let auth = self.resolve_auth().await?;
        let from_addr = match &auth {
            GmailAuth::AppPassword { email, .. } | GmailAuth::OAuth2 { email, .. } => email.clone(),
        };

        let message = EmailMessage::builder()
            .from(
                from_addr
                    .parse()
                    .map_err(|e| GmailError::Config(format!("Invalid sender address: {e}")))?,
            )
            .to(to
                .trim()
                .parse()
                .map_err(|e| GmailError::Config(format!("Invalid recipient address: {e}")))?)
            .subject(subject)
            .body(body.to_string())
            .map_err(|e| GmailError::Smtp(format!("Failed to construct email: {e}")))?;

        let transport_builder = AsyncSmtpTransport::<Tokio1Executor>::relay(GMAIL_SMTP_HOST)
            .map_err(|e| GmailError::Smtp(format!("SMTP relay init failed: {e}")))?
            .port(GMAIL_SMTP_PORT);

        let mailer = match auth {
            GmailAuth::AppPassword { email, password } => transport_builder
                .credentials(Credentials::new(email, password))
                .authentication(vec![Mechanism::Plain, Mechanism::Login])
                .build(),
            GmailAuth::OAuth2 {
                email,
                access_token,
            } => transport_builder
                .credentials(Credentials::new(email, access_token))
                .authentication(vec![Mechanism::Xoauth2])
                .build(),
        };

        let response = mailer
            .send(message)
            .await
            .map_err(|e| GmailError::Smtp(format!("SMTP send failed: {e}")))?;

        Ok(format!(
            "Email sent to {} (SMTP code: {})",
            to.trim(),
            response.code()
        ))
    }
}

async fn exchange_refresh_token(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<String, GmailError> {
    let client = reqwest::Client::new();
    let resp = client
        .post(GMAIL_OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", client_id.trim()),
            ("client_secret", client_secret.trim()),
            ("refresh_token", refresh_token.trim()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|e| GmailError::OAuth2(format!("OAuth2 token request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(GmailError::OAuth2(format!(
            "OAuth2 token endpoint returned {status}: {text}"
        )));
    }

    let val: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| GmailError::OAuth2(format!("Invalid OAuth2 JSON response: {e}")))?;
    val["access_token"]
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| GmailError::OAuth2("Missing access_token in OAuth2 response".into()))
}

fn build_imap_search_criteria(query: &str) -> String {
    let sanitized: String = query.chars().filter(|&c| c != '\r' && c != '\n').collect();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        return "ALL".to_string();
    }
    let upper = trimmed.to_ascii_uppercase();
    if upper == "ALL"
        || upper == "UNSEEN"
        || upper == "SEEN"
        || upper.starts_with("FROM ")
        || upper.starts_with("TO ")
        || upper.starts_with("SUBJECT ")
        || upper.starts_with("SINCE ")
        || upper.starts_with("TEXT ")
    {
        return trimmed.to_string();
    }
    let escaped = trimmed.replace('\\', "\\\\").replace('"', "\\\"");
    format!("TEXT \"{escaped}\"")
}

async fn fetch_uids(
    session: &mut async_imap::Session<TlsStream<TcpStream>>,
    uids: &[u32],
) -> Result<Vec<ParsedEmail>, GmailError> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let uid_set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let stream = session.uid_fetch(&uid_set, "(UID RFC822)").await?;
    let fetches: Vec<_> = stream.try_collect().await?;
    let mut parsed = Vec::new();
    for fetch in fetches {
        let uid = fetch.uid.unwrap_or(0);
        if let Some(raw) = fetch.body() {
            match parse_rfc822_email(uid, raw) {
                Ok(email) => parsed.push(email),
                Err(e) => tracing::warn!("Skipping malformed email uid {uid}: {e}"),
            }
        }
    }
    Ok(parsed)
}

// ============================================================================
// Built-in Tools (`gmail_search`, `gmail_read`, `gmail_summarize_unread`, `gmail_send_confirmed`)
// ============================================================================

#[derive(Deserialize)]
struct GmailSearchArgs {
    #[serde(default)]
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

#[derive(Deserialize)]
struct GmailReadArgs {
    uid: u32,
}

#[derive(Deserialize)]
struct GmailSummarizeArgs {
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
struct GmailSendArgs {
    to: String,
    subject: String,
    body: String,
}

/// `gmail_search` (`SafeReadOnly`): searches Gmail INBOX via IMAP.
pub struct GmailSearchTool {
    client: Arc<GmailClient>,
}

impl GmailSearchTool {
    pub fn new(client: Arc<GmailClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for GmailSearchTool {
    fn name(&self) -> &str {
        "gmail_search"
    }

    fn description(&self) -> &str {
        "Search Gmail inbox messages by keyword or IMAP query (e.g. UNSEEN, FROM \"alice@example.com\", or plain text)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search keyword or IMAP search criteria (defaults to ALL)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 25,
                    "description": "Maximum number of matching messages to return (default 10)"
                }
            }
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: GmailSearchArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        match self.client.search_inbox(&parsed.query, parsed.limit).await {
            Ok(emails) => {
                let summaries: Vec<serde_json::Value> = emails
                    .into_iter()
                    .map(|e| {
                        serde_json::json!({
                            "uid": e.uid,
                            "from": e.from,
                            "subject": e.subject,
                            "date": e.date,
                            "snippet": e.snippet,
                        })
                    })
                    .collect();
                Ok(ToolResult::ok(
                    serde_json::to_string_pretty(&summaries).unwrap_or_else(|_| "[]".into()),
                ))
            }
            Err(e) => Ok(ToolResult::err(e.to_string())),
        }
    }
}

/// `gmail_read` (`SafeReadOnly`): fetches and parses a full email by IMAP UID.
pub struct GmailReadTool {
    client: Arc<GmailClient>,
}

impl GmailReadTool {
    pub fn new(client: Arc<GmailClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for GmailReadTool {
    fn name(&self) -> &str {
        "gmail_read"
    }

    fn description(&self) -> &str {
        "Read the full content of a Gmail message by its numeric IMAP UID."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "uid": {
                    "type": "integer",
                    "description": "IMAP UID of the email to read"
                }
            },
            "required": ["uid"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: GmailReadArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        match self.client.read_by_uid(parsed.uid).await {
            Ok(email) => Ok(ToolResult::ok(
                serde_json::to_string_pretty(&email).unwrap_or_else(|_| "{}".into()),
            )),
            Err(e) => Ok(ToolResult::err(e.to_string())),
        }
    }
}

/// `gmail_summarize_unread` (`SafeReadOnly`): fetches unread Gmail messages for summarization.
pub struct GmailSummarizeUnreadTool {
    client: Arc<GmailClient>,
}

impl GmailSummarizeUnreadTool {
    pub fn new(client: Arc<GmailClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for GmailSummarizeUnreadTool {
    fn name(&self) -> &str {
        "gmail_summarize_unread"
    }

    fn description(&self) -> &str {
        "Fetch recent unread (UNSEEN) Gmail messages with sender, subject, and body excerpts for summarization."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 25,
                    "description": "Maximum number of unread messages to inspect (default 10)"
                }
            }
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: GmailSummarizeArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        match self.client.fetch_unread(parsed.limit).await {
            Ok(emails) => {
                let payload = serde_json::json!({
                    "unread_count": emails.len(),
                    "messages": emails,
                });
                Ok(ToolResult::ok(
                    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into()),
                ))
            }
            Err(e) => Ok(ToolResult::err(e.to_string())),
        }
    }
}

/// `gmail_send_confirmed` (`RequiresApproval` — always gated by `ApprovalBroker`, even in FullAuto).
pub struct GmailSendConfirmedTool {
    client: Arc<GmailClient>,
}

impl GmailSendConfirmedTool {
    pub fn new(client: Arc<GmailClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for GmailSendConfirmedTool {
    fn name(&self) -> &str {
        "gmail_send_confirmed"
    }

    fn description(&self) -> &str {
        "Send an email via Gmail SMTP after explicit user approval."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient email address"
                },
                "subject": {
                    "type": "string",
                    "description": "Email subject line"
                },
                "body": {
                    "type": "string",
                    "description": "Plain-text email body"
                }
            },
            "required": ["to", "subject", "body"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: GmailSendArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        if parsed.to.trim().is_empty() {
            return Err(ToolError::InvalidArgs("recipient 'to' must not be empty".into()));
        }
        match self
            .client
            .send_email(&parsed.to, &parsed.subject, &parsed.body)
            .await
        {
            Ok(status) => Ok(ToolResult::ok(status)),
            Err(e) => Ok(ToolResult::err(e.to_string())),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agent::approval::ApprovalBroker;
    use crate::agent::classifier::CommandRiskClassifier;
    use crate::events::RiskTier;
    use std::path::Path;

    #[test]
    fn mail_parser_parses_rfc822_message() {
        let raw = concat!(
            "From: Alice <alice@example.com>\r\n",
            "To: Bob <bob@example.com>\r\n",
            "Subject: Project Update\r\n",
            "Date: Wed, 25 Nov 2026 10:00:00 +0000\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "Hello Bob,\r\nPhase 3 Gmail integration is ready.\r\n"
        )
        .as_bytes();

        let parsed = parse_rfc822_email(42, raw).unwrap();
        assert_eq!(parsed.uid, 42);
        assert_eq!(parsed.subject, "Project Update");
        assert_eq!(parsed.from, "Alice <alice@example.com>");
        assert_eq!(parsed.to, vec!["Bob <bob@example.com>"]);
        assert!(parsed.date.is_some());
        assert!(parsed.body_text.contains("Phase 3 Gmail integration is ready."));
        assert!(parsed.snippet.contains("Hello Bob,"));
    }

    #[test]
    fn xoauth2_token_formatting_matches_google_sasl_spec() {
        let raw = format_xoauth2_sasl("jarvis@gmail.com", "ya29.secret_token");
        assert_eq!(raw, "user=jarvis@gmail.com\x01auth=Bearer ya29.secret_token\x01\x01");

        let b64 = format_xoauth2_base64("jarvis@gmail.com", "ya29.secret_token");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), raw);
    }

    #[test]
    fn command_risk_classifier_gates_gmail_send_and_allows_gmail_reads() {
        let ws = Path::new(".");
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "gmail_search",
                &serde_json::json!({"query": "UNSEEN"}),
                ws
            )
            .tier,
            RiskTier::SafeReadOnly
        );
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "gmail_read",
                &serde_json::json!({"uid": 10}),
                ws
            )
            .tier,
            RiskTier::SafeReadOnly
        );
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "gmail_summarize_unread",
                &serde_json::json!({"limit": 5}),
                ws
            )
            .tier,
            RiskTier::SafeReadOnly
        );
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "gmail_send_confirmed",
                &serde_json::json!({"to": "a@b.com", "subject": "Hi", "body": "Test"}),
                ws
            )
            .tier,
            RiskTier::RequiresApproval
        );
        assert!(ApprovalBroker::requires_explicit_even_in_full_auto(
            "gmail_send_confirmed"
        ));
    }

    #[test]
    fn build_imap_search_criteria_strips_crlf() {
        let unseen_inj = build_imap_search_criteria("UNSEEN\r\nA999 LOGOUT");
        assert!(!unseen_inj.contains('\r') && !unseen_inj.contains('\n'));
        assert_eq!(unseen_inj, "TEXT \"UNSEENA999 LOGOUT\"");

        let from_inj = build_imap_search_criteria("FROM alice@example.com\r\nA2 FETCH 1");
        assert!(!from_inj.contains('\r') && !from_inj.contains('\n'));
        assert_eq!(from_inj, "FROM alice@example.comA2 FETCH 1");

        assert_eq!(
            build_imap_search_criteria("hello\r\nworld"),
            "TEXT \"helloworld\""
        );
        assert_eq!(build_imap_search_criteria("\r\n  \r\n"), "ALL");
    }

    #[test]
    fn gmail_oauth_client_secret_lives_in_secret_store_not_config() {
        let secrets = crate::secrets::InMemorySecretStore::with_entries([(
            keys::GMAIL_OAUTH_CLIENT_SECRET,
            "gocspx-secret-123",
        )]);
        assert_eq!(
            secrets.get(keys::GMAIL_OAUTH_CLIENT_SECRET).unwrap().as_deref(),
            Some("gocspx-secret-123")
        );
        let toml_str = toml::to_string(&AppConfig::default()).unwrap();
        assert!(!toml_str.contains("gmail_oauth_client_secret"));
    }
}
