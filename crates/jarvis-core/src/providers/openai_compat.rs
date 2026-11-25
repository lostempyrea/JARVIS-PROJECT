//! OpenAI-compatible `/v1/chat/completions` SSE streaming provider.

use crate::providers::traits::{
    ChatMessage, LlmProvider, MessageRole, ProviderError, StreamEvent, ToolCall, ToolDefinition,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

/// Async HTTP SSE provider for OpenAI-compatible endpoints (OpenAI, Groq, OpenRouter, vLLM, etc.).
#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            client: reqwest::Client::new(),
        }
    }

    pub fn with_client(
        base_url: impl Into<String>,
        api_key: Option<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            client,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

#[derive(Serialize)]
struct OpenAiChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAiTool>,
    stream: bool,
}

#[derive(Serialize)]
struct OpenAiMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiMessageToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize)]
struct OpenAiMessageToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiFunctionCall,
}

#[derive(Serialize)]
struct OpenAiFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct OpenAiTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiToolDef,
}

#[derive(Serialize)]
struct OpenAiToolDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize)]
struct SseChunk {
    #[serde(default)]
    choices: Vec<SseChoice>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct SseChoice {
    #[serde(default)]
    delta: SseDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<SseToolCallDelta>>,
}

#[derive(Deserialize)]
struct SseToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<SseFunctionDelta>,
}

#[derive(Deserialize)]
struct SseFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments_buf: String,
}

/// Stateful accumulator that parses OpenAI SSE lines and reconstructs
/// incremental `tool_calls` fragments indexed by `index`.
#[derive(Debug, Default)]
pub struct SseAccumulator {
    pending_tools: BTreeMap<usize, PartialToolCall>,
    done_emitted: bool,
}

impl SseAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a single SSE line (e.g. `data: {...}` or `data: [DONE]`).
    pub fn feed_line(&mut self, raw_line: &str) -> Vec<StreamEvent> {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(':') {
            return Vec::new();
        }
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            return Vec::new();
        };

        if data == "[DONE]" {
            let mut events = self.flush_tools();
            if !self.done_emitted {
                self.done_emitted = true;
                events.push(StreamEvent::Done);
            }
            return events;
        }

        let chunk: SseChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(err) => {
                return vec![StreamEvent::Error(format!(
                    "failed to parse SSE JSON chunk: {err}"
                ))];
            }
        };

        if let Some(err) = chunk.error {
            return vec![StreamEvent::Error(err.to_string())];
        }

        let mut events = Vec::new();
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content {
                if !content.is_empty() {
                    events.push(StreamEvent::TextDelta(content));
                }
            }

            if let Some(tool_deltas) = choice.delta.tool_calls {
                for delta in tool_deltas {
                    let entry = self.pending_tools.entry(delta.index).or_default();
                    if let Some(id) = delta.id {
                        if !id.is_empty() {
                            entry.id = id;
                        }
                    }
                    if let Some(func) = delta.function {
                        if let Some(name) = func.name {
                            entry.name.push_str(&name);
                        }
                        if let Some(args) = func.arguments {
                            entry.arguments_buf.push_str(&args);
                        }
                    }
                }
            }

            if let Some(reason) = choice.finish_reason {
                if reason == "tool_calls" || reason == "stop" {
                    events.extend(self.flush_tools());
                }
            }
        }

        events
    }

    /// Flush any remaining accumulated tool calls and emit `Done` if not yet emitted.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        let mut events = self.flush_tools();
        if !self.done_emitted {
            self.done_emitted = true;
            events.push(StreamEvent::Done);
        }
        events
    }

    fn flush_tools(&mut self) -> Vec<StreamEvent> {
        let pending = std::mem::take(&mut self.pending_tools);
        let mut events = Vec::with_capacity(pending.len());
        for (idx, partial) in pending {
            let id = if partial.id.is_empty() {
                format!("call_{idx}")
            } else {
                partial.id
            };
            let args_str = partial.arguments_buf.trim();
            let arguments = if args_str.is_empty() {
                serde_json::json!({})
            } else {
                match serde_json::from_str::<serde_json::Value>(args_str) {
                    Ok(v) => v,
                    Err(_) => serde_json::Value::String(partial.arguments_buf),
                }
            };
            events.push(StreamEvent::ToolCallComplete(ToolCall {
                id,
                name: partial.name,
                arguments,
            }));
        }
        events
    }
}

fn to_openai_messages(messages: &[ChatMessage]) -> Vec<OpenAiMessage> {
    messages
        .iter()
        .map(|msg| {
            let tool_calls = msg.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|tc| OpenAiMessageToolCall {
                        id: tc.id.clone(),
                        kind: "function",
                        function: OpenAiFunctionCall {
                            name: tc.name.clone(),
                            arguments: serde_json::to_string(&tc.arguments)
                                .unwrap_or_else(|_| "{}".to_string()),
                        },
                    })
                    .collect()
            });
            let content = if msg.role == MessageRole::Assistant
                && msg.content.is_empty()
                && tool_calls.is_some()
            {
                None
            } else {
                Some(msg.content.clone())
            };
            OpenAiMessage {
                role: msg.role.as_str(),
                content,
                tool_calls,
                tool_call_id: msg.tool_call_id.clone(),
                name: msg.name.clone(),
            }
        })
        .collect()
}

fn to_openai_tools(tools: &[ToolDefinition]) -> Vec<OpenAiTool> {
    tools
        .iter()
        .map(|t| OpenAiTool {
            kind: "function",
            function: OpenAiToolDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect()
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        "openai_compat"
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<mpsc::Receiver<StreamEvent>, ProviderError> {
        let payload = OpenAiChatRequest {
            model,
            messages: to_openai_messages(messages),
            tools: to_openai_tools(tools),
            stream: true,
        };

        let mut req = self.client.post(self.endpoint()).json(&payload);
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::ApiStatus {
                status: status.as_u16(),
                body,
            });
        }

        let (tx, rx) = mpsc::channel(64);
        let mut byte_stream = resp.bytes_stream();

        tokio::spawn(async move {
            let mut acc = SseAccumulator::new();
            let mut byte_buf: Vec<u8> = Vec::new();

            while let Some(chunk_res) = byte_stream.next().await {
                match chunk_res {
                    Ok(bytes) => {
                        byte_buf.extend_from_slice(&bytes);
                        while let Some(pos) = byte_buf.iter().position(|&b| b == b'\n') {
                            let line_bytes: Vec<u8> = byte_buf.drain(..=pos).collect();
                            let line = String::from_utf8_lossy(&line_bytes);
                            for ev in acc.feed_line(&line) {
                                if tx.send(ev).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(StreamEvent::Error(err.to_string())).await;
                        return;
                    }
                }
            }

            if !byte_buf.is_empty() {
                let line = String::from_utf8_lossy(&byte_buf);
                if !line.trim().is_empty() {
                    for ev in acc.feed_line(&line) {
                        let _ = tx.send(ev).await;
                    }
                }
            }
            for ev in acc.finish() {
                let _ = tx.send(ev).await;
            }
        });

        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_accumulator_assembles_text_and_indexed_tool_calls() {
        let mut acc = SseAccumulator::new();

        let lines = [
            r#"data: {"choices":[{"delta":{"content":"Hello "},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"world"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"fs_","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"memory_recall","arguments":"{\"query\":\"rust\"}"}}]},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"read","arguments":"\"Cargo.toml\"}"}}]},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"data: [DONE]"#,
        ];

        let mut events = Vec::new();
        for line in lines {
            events.extend(acc.feed_line(line));
        }

        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Hello ".to_string()),
                StreamEvent::TextDelta("world".to_string()),
                StreamEvent::ToolCallComplete(ToolCall {
                    id: "call_1".to_string(),
                    name: "fs_read".to_string(),
                    arguments: serde_json::json!({"path": "Cargo.toml"}),
                }),
                StreamEvent::ToolCallComplete(ToolCall {
                    id: "call_2".to_string(),
                    name: "memory_recall".to_string(),
                    arguments: serde_json::json!({"query": "rust"}),
                }),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn sse_accumulator_handles_comments_and_empty_lines() {
        let mut acc = SseAccumulator::new();
        assert!(acc.feed_line("").is_empty());
        assert!(acc.feed_line(": keep-alive").is_empty());
        assert_eq!(acc.feed_line("data: [DONE]"), vec![StreamEvent::Done]);
    }

    #[tokio::test]
    async fn chat_stream_preserves_multibyte_utf8_split_across_chunks() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let sse_line = "data: {\"choices\":[{\"delta\":{\"content\":\"Xin chào thế giới tiếng Việt\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
        let raw_bytes = sse_line.as_bytes().to_vec();
        // Split right inside the multi-byte UTF-8 sequence of 'à' or 'ế'
        let split_at = raw_bytes
            .windows(2)
            .position(|w| w[0] > 0x7F && (w[1] & 0xC0) == 0x80)
            .map(|i| i + 1)
            .unwrap();

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut req_buf = [0u8; 2048];
                let _ = sock.read(&mut req_buf).await;
                let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
                let _ = sock.write_all(headers.as_bytes()).await;

                let part1 = &raw_bytes[..split_at];
                let part2 = &raw_bytes[split_at..];
                for part in [part1, part2] {
                    let chunk_hdr = format!("{:x}\r\n", part.len());
                    let _ = sock.write_all(chunk_hdr.as_bytes()).await;
                    let _ = sock.write_all(part).await;
                    let _ = sock.write_all(b"\r\n").await;
                    let _ = sock.flush().await;
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                let _ = sock.write_all(b"0\r\n\r\n").await;
            }
        });

        let provider = OpenAiCompatProvider::new(format!("http://{addr}"), None);
        let mut rx = provider
            .chat_stream(&[ChatMessage::user("hi")], &[], "gpt-test")
            .await
            .unwrap();

        let mut text = String::new();
        while let Some(ev) = rx.recv().await {
            if let StreamEvent::TextDelta(d) = ev {
                text.push_str(&d);
            }
        }
        assert_eq!(text, "Xin chào thế giới tiếng Việt");
        assert!(!text.contains('\u{FFFD}'));
    }
}
