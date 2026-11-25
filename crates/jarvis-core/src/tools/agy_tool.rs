//! Sub-agent delegation tool (`delegate_to_agy`) spawning `AgyCliBridge` or interactive Windows PowerShell in a target directory.

use crate::config::AppConfig;
use crate::events::CoreEvent;
use crate::providers::agy_cli::AgyCliBridge;
use crate::providers::traits::StreamEvent;
use crate::tools::powershell_console::launch_agy_in_windows_powershell;
use crate::tools::pty_manager::PtyManager;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub struct AgyDelegateTool {
    config: Option<Arc<RwLock<AppConfig>>>,
    pty: Option<Arc<PtyManager>>,
}

impl Default for AgyDelegateTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AgyDelegateTool {
    pub fn new() -> Self {
        Self {
            config: None,
            pty: None,
        }
    }

    pub fn with_config(config: Arc<RwLock<AppConfig>>) -> Self {
        Self {
            config: Some(config),
            pty: None,
        }
    }

    pub fn with_config_and_pty(config: Arc<RwLock<AppConfig>>, pty: Arc<PtyManager>) -> Self {
        Self {
            config: Some(config),
            pty: Some(pty),
        }
    }
}

#[derive(Deserialize)]
struct DelegateArgs {
    prompt: String,
    #[serde(default)]
    workspace_dir: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    interactive_window: Option<bool>,
}

#[async_trait]
impl Tool for AgyDelegateTool {
    fn name(&self) -> &str {
        "delegate_to_agy"
    }

    fn description(&self) -> &str {
        "Delegate a complex coding or multi-step task to the Antigravity CLI (`agy`) sub-agent in a workspace directory."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "Task specification for the agy sub-agent" },
                "workspace_dir": { "type": "string", "description": "Optional working directory (defaults to active workspace root)" },
                "model": { "type": "string", "description": "Optional model override" },
                "effort": { "type": "string", "description": "Optional effort level (low, medium, high)" },
                "interactive_window": { "type": "boolean", "description": "Whether to launch interactively in a Windows PowerShell console window" }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: DelegateArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        if parsed.prompt.trim().is_empty() {
            return Err(ToolError::InvalidArgs("prompt must not be empty".to_string()));
        }

        let target_dir = match parsed.workspace_dir {
            Some(dir) if !dir.trim().is_empty() => {
                let p = PathBuf::from(&dir);
                if p.is_absolute() {
                    p
                } else {
                    ctx.workspace_root.join(p)
                }
            }
            _ => ctx.workspace_root.clone(),
        };

        if !target_dir.exists() {
            return Ok(ToolResult::err(format!(
                "Target workspace directory does not exist: {}",
                target_dir.display()
            )));
        }

        let effort = parsed.effort.unwrap_or_else(|| ctx.agy_effort.clone());
        let model = parsed
            .model
            .or_else(|| ctx.agy_model.clone())
            .unwrap_or_default();

        let cfg_flag = self
            .config
            .as_ref()
            .and_then(|c| c.read().ok().map(|cfg| cfg.agy_use_windows_powershell))
            .unwrap_or(false);

        let is_chat_channel = ctx.session_id.starts_with("telegram-") || ctx.session_id.starts_with("discord-");
        let use_interactive = parsed
            .interactive_window
            .unwrap_or(cfg_flag && !is_chat_channel);

        if use_interactive {
            if let Some(pty) = &self.pty {
                let prompt_opt = if parsed.prompt.trim().is_empty() {
                    None
                } else {
                    Some(parsed.prompt.as_str())
                };
                match pty.spawn_agy_tab(
                    prompt_opt,
                    Some(target_dir.to_str().unwrap_or(".")),
                    120,
                    30,
                ) {
                    Ok(tab_id) => {
                        ctx.event_bus.publish(CoreEvent::TerminalLine {
                            session_id: ctx.session_id.clone(),
                            stream: "agy".to_string(),
                            line: format!("Spawned embedded AGY ConPTY tab {tab_id} for target: {}", target_dir.display()),
                        });
                        return Ok(ToolResult::ok("Opened AGY CLI interactively in embedded ConPTY tab."));
                    }
                    Err(e) => {
                        return Ok(ToolResult::err(format!(
                            "Failed to spawn embedded AGY ConPTY tab: {e}"
                        )));
                    }
                }
            }

            let prompt_opt = if parsed.prompt.trim().is_empty() {
                None
            } else {
                Some(parsed.prompt.as_str())
            };
            match launch_agy_in_windows_powershell(
                &target_dir,
                prompt_opt,
                &model,
                &effort,
                ctx.full_auto_mode,
                false,
            ) {
                Ok(_child) => {
                    ctx.event_bus.publish(CoreEvent::TerminalLine {
                        session_id: ctx.session_id.clone(),
                        stream: "agy".to_string(),
                        line: format!("Opened AGY CLI interactively in Windows PowerShell for target: {}", target_dir.display()),
                    });
                    return Ok(ToolResult::ok("Opened AGY CLI interactively in Windows PowerShell."));
                }
                Err(e) => {
                    return Ok(ToolResult::err(format!(
                        "Failed to launch interactive AGY in Windows PowerShell: {e}"
                    )));
                }
            }
        }

        let bridge = AgyCliBridge::new(target_dir, effort, ctx.full_auto_mode);

        let mut rx = bridge
            .run_prompt(&parsed.prompt, &model)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let mut output = String::new();
        let mut errors = Vec::new();

        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::TextDelta(delta) => {
                    ctx.event_bus.publish(CoreEvent::TerminalLine {
                        session_id: ctx.session_id.clone(),
                        stream: "agy".to_string(),
                        line: delta.trim_end_matches('\n').to_string(),
                    });
                    output.push_str(&delta);
                }
                StreamEvent::ToolProgress(msg) => {
                    ctx.event_bus.publish(CoreEvent::TerminalLine {
                        session_id: ctx.session_id.clone(),
                        stream: "agy".to_string(),
                        line: msg,
                    });
                }
                StreamEvent::ToolCallComplete(tc) => {
                    output.push_str(&format!("\n[agy tool call: {}]", tc.name));
                }
                StreamEvent::Error(err) => {
                    errors.push(err);
                }
                StreamEvent::Done => break,
            }
        }

        if errors.is_empty() {
            Ok(ToolResult::ok(if output.trim().is_empty() {
                "(agy sub-agent completed with no text output)".to_string()
            } else {
                output
            }))
        } else {
            Ok(ToolResult::err(format!(
                "agy sub-agent failed: {}\n{}",
                errors.join("; "),
                output
            )))
        }
    }
}
