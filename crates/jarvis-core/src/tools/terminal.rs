//! PowerShell terminal execution tool with UTF-8 output, `CREATE_NO_WINDOW`,
//! timeout kill-guard, 16KB output truncation, and live event streaming.

use crate::events::CoreEvent;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::task::JoinHandle;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Maximum captured stdout+stderr payload returned to the LLM (16 KiB).
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;
/// Default command timeout in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Executes PowerShell commands inside the workspace root.
#[derive(Debug, Clone)]
pub struct TerminalTool {
    pub default_timeout_secs: u64,
}

impl Default for TerminalTool {
    fn default() -> Self {
        Self {
            default_timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

#[derive(Deserialize)]
struct TerminalArgs {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Build the UTF-8 prefixed PowerShell script string.
pub fn wrap_powershell_utf8(command: &str) -> String {
    format!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $OutputEncoding = [System.Text.Encoding]::UTF8; {command}"
    )
}

/// Truncate a UTF-8 string safely to at most `max_bytes`, appending a truncation marker.
pub fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[output truncated to {} bytes]", &input[..end], max_bytes)
}

#[async_trait]
impl Tool for TerminalTool {
    fn name(&self) -> &str {
        "terminal"
    }

    fn description(&self) -> &str {
        "Execute a PowerShell command in the workspace root with UTF-8 encoding and timeout protection."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "PowerShell command to execute"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional timeout in seconds (default 60)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: TerminalArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        if parsed.command.trim().is_empty() {
            return Err(ToolError::InvalidArgs("command must not be empty".to_string()));
        }

        let timeout_secs = parsed.timeout_secs.unwrap_or(self.default_timeout_secs).max(1);
        let script = wrap_powershell_utf8(&parsed.command);

        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .current_dir(&ctx.workspace_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(windows)]
        {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let bus_out = ctx.event_bus.clone();
        let sess_out = ctx.session_id.clone();
        let out_buf = Arc::new(Mutex::new(String::new()));
        let out_buf_task = Arc::clone(&out_buf);
        let stdout_task = tokio::spawn(async move {
            if let Some(stream) = stdout {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    bus_out.publish(CoreEvent::TerminalLine {
                        session_id: sess_out.clone(),
                        stream: "stdout".to_string(),
                        line: line.clone(),
                    });
                    let mut guard = out_buf_task.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.len() <= MAX_OUTPUT_BYTES * 2 {
                        guard.push_str(&line);
                        guard.push('\n');
                    }
                }
            }
            out_buf_task.lock().unwrap_or_else(|e| e.into_inner()).clone()
        });

        let bus_err = ctx.event_bus.clone();
        let sess_err = ctx.session_id.clone();
        let err_buf = Arc::new(Mutex::new(String::new()));
        let err_buf_task = Arc::clone(&err_buf);
        let stderr_task = tokio::spawn(async move {
            if let Some(stream) = stderr {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    bus_err.publish(CoreEvent::TerminalLine {
                        session_id: sess_err.clone(),
                        stream: "stderr".to_string(),
                        line: line.clone(),
                    });
                    let mut guard = err_buf_task.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.len() <= MAX_OUTPUT_BYTES * 2 {
                        guard.push_str(&line);
                        guard.push('\n');
                    }
                }
            }
            err_buf_task.lock().unwrap_or_else(|e| e.into_inner()).clone()
        });

        let wait_result =
            tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;

        let exit_status = match wait_result {
            Ok(res) => res?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let partial_out = drain_pipe_with_timeout(stdout_task, &out_buf).await;
                let partial_err = drain_pipe_with_timeout(stderr_task, &err_buf).await;
                let combined = format_combined_output(&partial_out, &partial_err);
                return Ok(ToolResult::err(format!(
                    "Command timed out after {timeout_secs}s and was terminated.\n{}",
                    truncate_utf8_bytes(&combined, MAX_OUTPUT_BYTES)
                )));
            }
        };

        let out_text = drain_pipe_with_timeout(stdout_task, &out_buf).await;
        let err_text = drain_pipe_with_timeout(stderr_task, &err_buf).await;
        let combined = format_combined_output(&out_text, &err_text);
        let truncated = truncate_utf8_bytes(&combined, MAX_OUTPUT_BYTES);

        if exit_status.success() {
            Ok(ToolResult::ok(if truncated.is_empty() {
                "(command completed with no output)".to_string()
            } else {
                truncated
            }))
        } else {
            let code = exit_status.code().unwrap_or(-1);
            Ok(ToolResult::err(format!(
                "Command exited with code {code}.\n{truncated}"
            )))
        }
    }
}

async fn drain_pipe_with_timeout(
    mut handle: JoinHandle<String>,
    shared_buf: &Arc<Mutex<String>>,
) -> String {
    match tokio::time::timeout(Duration::from_secs(2), &mut handle).await {
        Ok(Ok(text)) => text,
        Ok(Err(_)) => shared_buf.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        Err(_) => {
            handle.abort();
            shared_buf.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }
}

fn format_combined_output(stdout: &str, stderr: &str) -> String {
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => format!("[stderr]\n{stderr}"),
        (false, false) => format!("{stdout}\n[stderr]\n{stderr}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::events::EventBus;

    #[test]
    fn truncate_utf8_preserves_multibyte_boundaries() {
        let text = "Xin chào thế giới 🌍🚀";
        let truncated = truncate_utf8_bytes(text, 10);
        assert!(truncated.contains("[output truncated to 10 bytes]"));
        // Verify valid UTF-8 and prefix length <= 10 + marker
        let prefix = truncated.split("\n...[output truncated").next().unwrap();
        assert!(prefix.len() <= 10);
    }

    #[tokio::test]
    async fn terminal_executes_utf8_and_streams_events() {
        let dir = tempfile::TempDir::new().unwrap();
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();

        let ctx = ToolContext {
            session_id: "test-sess".to_string(),
            workspace_root: dir.path().to_path_buf(),
            db: Database::open_in_memory().unwrap(),
            event_bus: bus,
            full_auto_mode: false,
            agy_effort: "medium".to_string(),
            active_model: "test".to_string(),
            agy_model: None,
        };

        let tool = TerminalTool::default();
        let res = tool
            .execute(serde_json::json!({"command": "Write-Output 'jarvis_utf8_ok'"}), &ctx)
            .await
            .unwrap();
        assert!(res.success, "output: {}", res.output);
        assert!(res.output.contains("jarvis_utf8_ok"));

        let mut saw_line = false;
        while let Ok(ev) = rx.try_recv() {
            if let CoreEvent::TerminalLine { line, .. } = ev {
                if line.contains("jarvis_utf8_ok") {
                    saw_line = true;
                }
            }
        }
        assert!(saw_line);
    }

    #[tokio::test]
    async fn drain_pipe_with_timeout_returns_partial_output_when_pipe_hangs() {
        let shared = Arc::new(Mutex::new("partial line\n".to_string()));
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            "never reached".to_string()
        });

        let start = std::time::Instant::now();
        let out = drain_pipe_with_timeout(handle, &shared).await;
        assert_eq!(out, "partial line\n");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
