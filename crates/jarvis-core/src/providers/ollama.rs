//! Local Ollama `/api/chat` NDJSON streaming provider and `/api/tags` model listing.

use crate::providers::traits::{
    ChatMessage, LlmProvider, MessageRole, ProviderError, StreamEvent, ToolCall, ToolDefinition,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Local Ollama HTTP provider.
#[derive(Debug, Clone)]
pub struct OllamaProvider {
    base_url: String,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Fetch available local model names via `GET /api/tags`.
    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ProviderError::ApiStatus {
                status: status.as_u16(),
                body,
            });
        }
        parse_ollama_tags(&body)
    }
}

#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: Vec<OllamaMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OllamaTool>,
    stream: bool,
}

#[derive(Serialize)]
struct OllamaMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Serialize, Deserialize)]
struct OllamaToolCall {
    #[serde(default)]
    id: Option<String>,
    function: OllamaFunctionCall,
}

#[derive(Serialize, Deserialize)]
struct OllamaFunctionCall {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Serialize)]
struct OllamaTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OllamaToolDef,
}

#[derive(Serialize)]
struct OllamaToolDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize)]
struct OllamaStreamLine {
    #[serde(default)]
    message: Option<OllamaStreamMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct OllamaStreamMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaModelTag>,
}

#[derive(Deserialize)]
struct OllamaModelTag {
    name: String,
}

/// Parse `/api/tags` JSON response into a list of model names.
pub fn parse_ollama_tags(body: &str) -> Result<Vec<String>, ProviderError> {
    let parsed: OllamaTagsResponse =
        serde_json::from_str(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    Ok(parsed.models.into_iter().map(|m| m.name).collect())
}

/// Parse a single NDJSON line from Ollama `/api/chat` into [`StreamEvent`]s.
pub fn parse_ollama_ndjson_line(raw_line: &str) -> Vec<StreamEvent> {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let parsed: OllamaStreamLine = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(err) => {
            return vec![StreamEvent::Error(format!(
                "invalid Ollama NDJSON line: {err}"
            ))];
        }
    };

    if let Some(err) = parsed.error {
        return vec![StreamEvent::Error(err)];
    }

    let mut events = Vec::new();
    if let Some(msg) = parsed.message {
        if let Some(content) = msg.content {
            if !content.is_empty() {
                events.push(StreamEvent::TextDelta(content));
            }
        }
        if let Some(calls) = msg.tool_calls {
            for call in calls {
                let id = call
                    .id
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("ollama_{}", Uuid::new_v4().simple()));
                events.push(StreamEvent::ToolCallComplete(ToolCall {
                    id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                }));
            }
        }
    }

    if parsed.done {
        events.push(StreamEvent::Done);
    }

    events
}

fn to_ollama_messages(messages: &[ChatMessage]) -> Vec<OllamaMessage> {
    messages
        .iter()
        .map(|msg| {
            let tool_calls = msg.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|tc| OllamaToolCall {
                        id: Some(tc.id.clone()),
                        function: OllamaFunctionCall {
                            name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                        },
                    })
                    .collect()
            });
            let role = match msg.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };
            OllamaMessage {
                role,
                content: msg.content.clone(),
                tool_calls,
            }
        })
        .collect()
}

fn to_ollama_tools(tools: &[ToolDefinition]) -> Vec<OllamaTool> {
    tools
        .iter()
        .map(|t| OllamaTool {
            kind: "function",
            function: OllamaToolDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect()
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        model: &str,
    ) -> Result<mpsc::Receiver<StreamEvent>, ProviderError> {
        let url = format!("{}/api/chat", self.base_url);
        let payload = OllamaChatRequest {
            model,
            messages: to_ollama_messages(messages),
            tools: to_ollama_tools(tools),
            stream: true,
        };

        let resp = self.client.post(url).json(&payload).send().await?;
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
            let mut byte_buf: Vec<u8> = Vec::new();
            let mut saw_done = false;

            while let Some(chunk_res) = byte_stream.next().await {
                match chunk_res {
                    Ok(bytes) => {
                        byte_buf.extend_from_slice(&bytes);
                        while let Some(pos) = byte_buf.iter().position(|&b| b == b'\n') {
                            let line_bytes: Vec<u8> = byte_buf.drain(..=pos).collect();
                            let line = String::from_utf8_lossy(&line_bytes);
                            for ev in parse_ollama_ndjson_line(&line) {
                                if matches!(ev, StreamEvent::Done) {
                                    saw_done = true;
                                }
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
                    for ev in parse_ollama_ndjson_line(&line) {
                        if matches!(ev, StreamEvent::Done) {
                            saw_done = true;
                        }
                        let _ = tx.send(ev).await;
                    }
                }
            }

            if !saw_done {
                let _ = tx.send(StreamEvent::Done).await;
            }
        });

        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_ndjson_parses_deltas_tool_calls_and_done() {
        let ev1 = parse_ollama_ndjson_line(
            r#"{"model":"qwen2.5","message":{"role":"assistant","content":"Checking "},"done":false}"#,
        );
        assert_eq!(ev1, vec![StreamEvent::TextDelta("Checking ".to_string())]);

        let ev2 = parse_ollama_ndjson_line(
            r#"{"model":"qwen2.5","message":{"role":"assistant","content":"","tool_calls":[{"id":"tc_1","function":{"name":"terminal","arguments":{"command":"dir"}}}]},"done":true}"#,
        );
        assert_eq!(
            ev2,
            vec![
                StreamEvent::ToolCallComplete(ToolCall {
                    id: "tc_1".to_string(),
                    name: "terminal".to_string(),
                    arguments: serde_json::json!({"command": "dir"}),
                }),
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn ollama_tags_parses_models() {
        let json = r#"{"models":[{"name":"llama3.2:latest"},{"name":"qwen2.5:14b"}]}"#;
        let models = parse_ollama_tags(json).unwrap();
        assert_eq!(models, vec!["llama3.2:latest", "qwen2.5:14b"]);
    }

    #[tokio::test]
    async fn ollama_stream_preserves_multibyte_utf8_split_across_chunks() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let ndjson = "{\"model\":\"qwen2.5\",\"message\":{\"role\":\"assistant\",\"content\":\"Xin chào thế giới tiếng Việt\"},\"done\":true}\n";
        let raw_bytes = ndjson.as_bytes().to_vec();
        let split_at = raw_bytes
            .windows(2)
            .position(|w| w[0] > 0x7F && (w[1] & 0xC0) == 0x80)
            .map(|i| i + 1)
            .unwrap();

        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut req_buf = [0u8; 2048];
                let _ = sock.read(&mut req_buf).await;
                let headers = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n";
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

        let provider = OllamaProvider::new(format!("http://{addr}"));
        let mut rx = provider
            .chat_stream(&[ChatMessage::user("hi")], &[], "qwen2.5")
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
