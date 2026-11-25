//! Antigravity CLI (`agy`) subprocess bridge with `--output-format stream-json`.

use crate::providers::traits::{
    ChatMessage, LlmProvider, ProviderError, StreamEvent, ToolDefinition,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Subprocess bridge invoking the `agy` CLI headless runner.
#[derive(Debug, Clone)]
pub struct AgyCliBridge {
    executable: PathBuf,
    working_dir: PathBuf,
    effort: String,
    full_auto_mode: bool,
}

impl AgyCliBridge {
    pub fn new(working_dir: impl Into<PathBuf>, effort: impl Into<String>, full_auto_mode: bool) -> Self {
        Self {
            executable: PathBuf::from("agy"),
            working_dir: working_dir.into(),
            effort: effort.into(),
            full_auto_mode,
        }
    }

    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = executable.into();
        self
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Build CLI arguments for `agy`.
    /// `--dangerously-skip-permissions` is added ONLY when `full_auto_mode == true`.
    pub fn build_args(
        prompt: &str,
        model: &str,
        effort: &str,
        full_auto_mode: bool,
    ) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            prompt.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
        ];
        if !model.trim().is_empty() {
            args.push("--model".to_string());
            args.push(model.trim().to_string());
        }
        if !effort.trim().is_empty() {
            args.push("--effort".to_string());
            args.push(effort.trim().to_string());
        }
        if full_auto_mode {
            args.push("--dangerously-skip-permissions".to_string());
        }
        args
    }

    /// Spawn `agy` for a single prompt and stream parsed [`StreamEvent`]s.
    pub async fn run_prompt(
        &self,
        prompt: &str,
        model: &str,
    ) -> Result<mpsc::Receiver<StreamEvent>, ProviderError> {
        let args = Self::build_args(prompt, model, &self.effort, self.full_auto_mode);
        let mut cmd = Command::new(&self.executable);
        cmd.args(&args)
            .current_dir(&self.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(windows)]
        {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Config("failed to capture agy stdout".to_string()))?;
        let stderr = child.stderr.take();

        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let stderr_handle = tokio::spawn(async move {
                let mut collected = String::new();
                if let Some(err_stream) = stderr {
                    let mut lines = BufReader::new(err_stream).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if !collected.is_empty() {
                            collected.push('\n');
                        }
                        collected.push_str(&line);
                    }
                }
                collected
            });

            let mut reader = BufReader::new(stdout).lines();
            let mut saw_done = false;
            let mut saw_text_delta = false;

            while let Ok(Some(line)) = reader.next_line().await {
                for ev in process_agy_stream_line(&line, &mut saw_text_delta) {
                    if matches!(ev, StreamEvent::Done) {
                        saw_done = true;
                    }
                    if tx.send(ev).await.is_err() {
                        let _ = child.kill().await;
                        return;
                    }
                }
            }

            let status = child.wait().await;
            let stderr_text = stderr_handle.await.unwrap_or_default();

            match status {
                Ok(exit) if !exit.success() => {
                    let msg = if stderr_text.trim().is_empty() {
                        format!("agy exited with status {exit}")
                    } else {
                        format!("agy exited with status {exit}: {}", stderr_text.trim())
                    };
                    let _ = tx.send(StreamEvent::Error(msg)).await;
                }
                Err(err) => {
                    let _ = tx.send(StreamEvent::Error(err.to_string())).await;
                }
                _ => {}
            }

            if !saw_done {
                let _ = tx.send(StreamEvent::Done).await;
            }
        });

        Ok(rx)
    }
}

/// Parse a single stdout line from `agy --output-format stream-json`.
/// Falls back to plain [`StreamEvent::TextDelta`] when the line is not valid JSON.
pub fn parse_agy_stream_line(raw_line: &str) -> Vec<StreamEvent> {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return vec![StreamEvent::TextDelta(format!("{raw_line}\n"))];
    };

    let Some(obj) = val.as_object() else {
        return vec![StreamEvent::TextDelta(format!("{raw_line}\n"))];
    };

    let kind = obj
        .get("type")
        .or_else(|| obj.get("event"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match kind {
        "init" => Vec::new(),
        "step_update" => {
            let Some(su) = obj.get("step_update").and_then(|v| v.as_object()) else {
                return Vec::new();
            };
            let step_type = su.get("step_type").and_then(|v| v.as_str()).unwrap_or("");
            if step_type == "user_input" {
                return Vec::new();
            }
            if let Some(text_delta) = su.get("text_delta").and_then(|v| v.as_str()) {
                if !text_delta.is_empty() {
                    return vec![StreamEvent::TextDelta(text_delta.to_string())];
                }
            }
            let state = su.get("state").and_then(|v| v.as_str()).unwrap_or("");
            if step_type != "agent_response" && state == "ACTIVE" {
                vec![StreamEvent::ToolProgress(format!("{step_type} ({state})"))]
            } else {
                Vec::new()
            }
        }
        "error" => {
            let msg = obj
                .get("message")
                .or_else(|| obj.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or(trimmed);
            vec![StreamEvent::Error(msg.to_string())]
        }
        "tool_use" | "tool_call" | "tool_progress" | "progress" => {
            let tool_name = obj
                .get("name")
                .or_else(|| obj.get("tool"))
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            let detail = obj
                .get("status")
                .or_else(|| obj.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or(trimmed);
            vec![StreamEvent::ToolProgress(format!("{tool_name}: {detail}"))]
        }
        "result" | "done" | "complete" => {
            let nested = obj.get("result").and_then(|v| v.as_object());
            let is_error_flag = nested
                .and_then(|r| r.get("is_error"))
                .or_else(|| obj.get("is_error"))
                .and_then(|v| v.as_bool())
                == Some(true);
            let status_is_error = nested
                .and_then(|r| r.get("status"))
                .or_else(|| obj.get("status"))
                .and_then(|v| v.as_str())
                .is_some_and(|s| {
                    s.eq_ignore_ascii_case("ERROR") || s.eq_ignore_ascii_case("FAILED")
                });

            if is_error_flag || status_is_error {
                let msg = nested
                    .and_then(|r| {
                        r.get("error")
                            .or_else(|| r.get("message"))
                            .or_else(|| r.get("response"))
                    })
                    .or_else(|| obj.get("result").filter(|v| v.is_string()))
                    .or_else(|| obj.get("error"))
                    .or_else(|| obj.get("message"))
                    .or_else(|| obj.get("response"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("agy reported an error");
                vec![StreamEvent::Error(msg.to_string()), StreamEvent::Done]
            } else {
                vec![StreamEvent::Done]
            }
        }
        _ => {
            if let Some(text) = extract_text_field(obj) {
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![StreamEvent::TextDelta(text)]
                }
            } else {
                vec![StreamEvent::ToolProgress(trimmed.to_string())]
            }
        }
    }
}

/// Process a stream line while tracking whether any [`StreamEvent::TextDelta`] was seen,
/// emitting a fallback `result.response` delta before [`StreamEvent::Done`] when none was streamed.
fn process_agy_stream_line(raw_line: &str, saw_text_delta: &mut bool) -> Vec<StreamEvent> {
    let parsed = parse_agy_stream_line(raw_line);
    let has_error = parsed.iter().any(|ev| matches!(ev, StreamEvent::Error(_)));
    let mut out = Vec::with_capacity(parsed.len() + 1);

    for ev in parsed {
        match &ev {
            StreamEvent::TextDelta(_) => {
                *saw_text_delta = true;
            }
            StreamEvent::Done if !*saw_text_delta && !has_error => {
                if let Some(resp) = extract_result_response(raw_line) {
                    *saw_text_delta = true;
                    out.push(StreamEvent::TextDelta(resp));
                }
            }
            _ => {}
        }
        out.push(ev);
    }

    out
}

fn extract_result_response(raw_line: &str) -> Option<String> {
    let val: serde_json::Value = serde_json::from_str(raw_line.trim()).ok()?;
    let obj = val.as_object()?;
    let kind = obj
        .get("type")
        .or_else(|| obj.get("event"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !matches!(kind, "result" | "done" | "complete") {
        return None;
    }
    let response = obj
        .get("result")
        .and_then(|v| v.as_object())
        .and_then(|r| r.get("response"))
        .or_else(|| obj.get("response"))
        .and_then(|v| v.as_str())?;
    if response.is_empty() {
        None
    } else {
        Some(response.to_string())
    }
}

fn extract_text_field(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if let Some(s) = obj.get("delta").and_then(|v| {
        v.as_str()
            .or_else(|| v.get("text").and_then(|t| t.as_str()))
    }) {
        return Some(s.to_string());
    }
    if let Some(s) = obj.get("text").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = obj.get("content").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(msg) = obj.get("message").and_then(|v| v.as_object()) {
        if let Some(s) = msg.get("content").and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
        if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
            let mut buf = String::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    buf.push_str(t);
                }
            }
            if !buf.is_empty() {
                return Some(buf);
            }
        }
    }
    None
}

fn format_messages_for_agy(messages: &[ChatMessage]) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        if msg.content.trim().is_empty() {
            continue;
        }
        parts.push(format!("[{}]: {}", msg.role.as_str(), msg.content));
    }
    parts.join("\n\n")
}

#[async_trait]
impl LlmProvider for AgyCliBridge {
    fn name(&self) -> &str {
        "agy_cli"
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        model: &str,
    ) -> Result<mpsc::Receiver<StreamEvent>, ProviderError> {
        let prompt = format_messages_for_agy(messages);
        self.run_prompt(&prompt, model).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_gates_dangerously_skip_permissions_on_full_auto() {
        let safe_args = AgyCliBridge::build_args("hello", "gemini-pro", "high", false);
        assert_eq!(
            safe_args,
            vec![
                "-p",
                "hello",
                "--output-format",
                "stream-json",
                "--model",
                "gemini-pro",
                "--effort",
                "high",
            ]
        );
        assert!(!safe_args
            .iter()
            .any(|a| a == "--dangerously-skip-permissions"));

        let auto_args = AgyCliBridge::build_args("hello", "", "medium", true);
        assert!(auto_args
            .iter()
            .any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn parse_agy_stream_line_handles_json_and_plain_text_fallback() {
        assert_eq!(
            parse_agy_stream_line(r#"{"type":"delta","delta":{"text":"Hello "}}"#),
            vec![StreamEvent::TextDelta("Hello ".to_string())]
        );
        assert_eq!(
            parse_agy_stream_line(r#"{"type":"tool_use","name":"fs_read","status":"reading"}"#),
            vec![StreamEvent::ToolProgress("fs_read: reading".to_string())]
        );
        assert_eq!(
            parse_agy_stream_line(r#"{"type":"result","is_error":false}"#),
            vec![StreamEvent::Done]
        );
        assert_eq!(
            parse_agy_stream_line(r#"{"type":"error","message":"boom"}"#),
            vec![StreamEvent::Error("boom".to_string())]
        );
        assert_eq!(
            parse_agy_stream_line("plain text output line"),
            vec![StreamEvent::TextDelta("plain text output line\n".to_string())]
        );
    }

    #[test]
    fn parse_agy_1_5_14_stream_events() {
        // init is suppressed
        assert!(parse_agy_stream_line(
            r#"{"event":"init","conversation_id":"conv-1","init":{"version":"1.5.14"}}"#
        )
        .is_empty());

        // user_input step_update is suppressed
        assert!(parse_agy_stream_line(
            r#"{"event":"step_update","step_update":{"conversation_id":"conv-1","step_index":0,"state":"DONE","step_type":"user_input"}}"#
        )
        .is_empty());

        // agent_response text_delta emits TextDelta
        assert_eq!(
            parse_agy_stream_line(
                r#"{"event":"step_update","step_update":{"conversation_id":"conv-1","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"READY"}}"#
            ),
            vec![StreamEvent::TextDelta("READY".to_string())]
        );

        // agent_response DONE without text_delta emits nothing
        assert!(parse_agy_stream_line(
            r#"{"event":"step_update","step_update":{"conversation_id":"conv-1","step_index":1,"state":"DONE","step_type":"agent_response"}}"#
        )
        .is_empty());

        // non-agent_response ACTIVE step emits ToolProgress
        assert_eq!(
            parse_agy_stream_line(
                r#"{"event":"step_update","step_update":{"conversation_id":"conv-1","step_index":2,"state":"ACTIVE","step_type":"code_action"}}"#
            ),
            vec![StreamEvent::ToolProgress("code_action (ACTIVE)".to_string())]
        );

        // non-agent_response DONE step without text_delta emits nothing
        assert!(parse_agy_stream_line(
            r#"{"event":"step_update","step_update":{"conversation_id":"conv-1","step_index":2,"state":"DONE","step_type":"code_action"}}"#
        )
        .is_empty());

        // nested result SUCCESS emits Done
        assert_eq!(
            parse_agy_stream_line(
                r#"{"event":"result","result":{"conversation_id":"conv-1","status":"SUCCESS","response":"READY\n"}}"#
            ),
            vec![StreamEvent::Done]
        );

        // nested result ERROR / FAILED emits Error + Done
        assert_eq!(
            parse_agy_stream_line(
                r#"{"event":"result","result":{"conversation_id":"conv-1","status":"FAILED","error":"execution aborted"}}"#
            ),
            vec![
                StreamEvent::Error("execution aborted".to_string()),
                StreamEvent::Done
            ]
        );
    }

    #[test]
    fn process_agy_stream_line_emits_fallback_result_only_when_no_text_delta_seen() {
        let result_line = r#"{"event":"result","result":{"conversation_id":"conv-1","status":"SUCCESS","response":"READY\n"}}"#;

        // No prior text_delta: result.response is emitted before Done.
        let mut saw_delta = false;
        assert_eq!(
            process_agy_stream_line(result_line, &mut saw_delta),
            vec![
                StreamEvent::TextDelta("READY\n".to_string()),
                StreamEvent::Done
            ]
        );
        assert!(saw_delta);

        // Prior text_delta seen: result.response is NOT duplicated.
        let mut saw_delta_already = false;
        assert_eq!(
            process_agy_stream_line(
                r#"{"event":"step_update","step_update":{"conversation_id":"conv-1","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"READY"}}"#,
                &mut saw_delta_already
            ),
            vec![StreamEvent::TextDelta("READY".to_string())]
        );
        assert!(saw_delta_already);
        assert_eq!(
            process_agy_stream_line(result_line, &mut saw_delta_already),
            vec![StreamEvent::Done]
        );
    }
}
