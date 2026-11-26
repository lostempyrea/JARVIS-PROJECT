//! PTY Terminal Tool (`pty_terminal`) for interactive ConPTY tabs (PowerShell or AGY).

use crate::tools::pty_manager::PtyManager;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

pub struct PtyTerminalTool {
    pty: Arc<PtyManager>,
}

impl PtyTerminalTool {
    pub fn new(pty: Arc<PtyManager>) -> Self {
        Self { pty }
    }
}

#[derive(Deserialize)]
struct PtyArgs {
    action: String,
    #[serde(default)]
    tab_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    input: Option<String>,
    #[serde(default, alias = "pressEnter")]
    press_enter: Option<bool>,
    #[serde(default, alias = "ctrlC")]
    ctrl_c: Option<bool>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[async_trait]
impl Tool for PtyTerminalTool {
    fn name(&self) -> &str {
        "pty_terminal"
    }

    fn description(&self) -> &str {
        "Manage and interact with persistent Windows ConPTY terminal tabs (PowerShell or AGY CLI)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "open_powershell",
                        "open_agy",
                        "list_tabs",
                        "read_screen",
                        "send_input",
                        "restart_tab",
                        "close_tab"
                    ],
                    "description": "Terminal action to perform"
                },
                "tab_id": { "type": "string", "description": "Optional tab identifier" },
                "title": { "type": "string", "description": "Optional tab title for PowerShell" },
                "command": { "type": "string", "description": "Optional initial command for PowerShell" },
                "prompt": { "type": "string", "description": "Optional prompt for AGY CLI" },
                "input": { "type": "string", "description": "Text input to send to terminal" },
                "press_enter": { "type": "boolean", "description": "Whether to press Enter after input (default true)" },
                "ctrl_c": { "type": "boolean", "description": "Whether to send Ctrl+C signal (default false)" },
                "model": { "type": "string", "description": "Optional model override for AGY" },
                "cwd": { "type": "string", "description": "Optional working directory" }
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: PtyArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;

        let cwd_path = match parsed.cwd {
            Some(ref d) if !d.trim().is_empty() => {
                let p = PathBuf::from(d);
                if p.is_absolute() {
                    p
                } else {
                    ctx.workspace_root.join(p)
                }
            }
            _ => ctx.workspace_root.clone(),
        };

        if !cwd_path.exists() {
            return Ok(ToolResult::err(format!(
                "Working directory does not exist: {}",
                cwd_path.display()
            )));
        }
        let cwd_str = cwd_path.to_str();

        match parsed.action.as_str() {
            "open_agy" => {
                let prompt_val = parsed.prompt.as_deref();
                let model_val = parsed.model.as_deref().or(ctx.agy_model.as_deref());
                let effort_val = Some(ctx.agy_effort.as_str());
                match self.pty.spawn_agy_tab_with_options(
                    prompt_val,
                    model_val,
                    effort_val,
                    ctx.full_auto_mode,
                    cwd_str,
                    120,
                    30,
                ) {
                    Ok(tab_id) => Ok(ToolResult::ok(format!(
                        "Opened interactive AGY ConPTY tab ({tab_id}) in JARVIS HUD Terminal."
                    ))),
                    Err(e) => Ok(ToolResult::err(format!(
                        "Failed to open interactive AGY tab: {e}"
                    ))),
                }
            }
            "open_powershell" => {
                match self.pty.spawn_powershell_tab_with_command(
                    parsed.title.clone(),
                    parsed.command.as_deref(),
                    cwd_str,
                    120,
                    30,
                ) {
                    Ok(tab_id) => {
                        if parsed.command.as_deref().is_some_and(|c| !c.trim().is_empty()) {
                            sleep(Duration::from_millis(500)).await;
                            let snap = self.pty.plain_text_snapshot(&tab_id, 30);
                            Ok(ToolResult::ok(format!(
                                "Opened PowerShell tab ({tab_id}) with command.\nSnapshot:\n{snap}"
                            )))
                        } else {
                            Ok(ToolResult::ok(format!(
                                "Opened PowerShell tab ({tab_id})."
                            )))
                        }
                    }
                    Err(e) => Ok(ToolResult::err(format!(
                        "Failed to open PowerShell tab: {e}"
                    ))),
                }
            }
            "list_tabs" => {
                let tabs = self.pty.list_tabs();
                if tabs.is_empty() {
                    Ok(ToolResult::ok("No active PTY terminal tabs."))
                } else {
                    let mut lines = vec!["Active PTY Tabs:".to_string()];
                    for t in tabs {
                        lines.push(format!(
                            "- ID: {}, Title: {}, Kind: {:?}, Alive: {}, ExitCode: {:?}",
                            t.tab_id, t.title, t.kind, t.alive, t.exit_code
                        ));
                    }
                    Ok(ToolResult::ok(lines.join("\n")))
                }
            }
            "read_screen" => {
                let tid = if let Some(ref id) = parsed.tab_id {
                    id.clone()
                } else {
                    let tabs = self.pty.list_tabs();
                    if let Some(t) = tabs.iter().find(|t| t.alive).or_else(|| tabs.first()) {
                        t.tab_id.clone()
                    } else {
                        return Ok(ToolResult::err("No PTY terminal tabs found."));
                    }
                };
                let snap = self.pty.plain_text_snapshot(&tid, 40);
                Ok(ToolResult::ok(snap))
            }
            "send_input" => {
                let tabs = self.pty.list_tabs();
                let tid = if let Some(ref id) = parsed.tab_id {
                    id.clone()
                } else if let Some(t) = tabs.iter().find(|t| t.alive).or_else(|| tabs.first()) {
                    t.tab_id.clone()
                } else {
                    return Ok(ToolResult::err("No PTY terminal tabs found."));
                };

                let is_ctrl_c = parsed.ctrl_c.unwrap_or(false);
                let press_enter = parsed.press_enter.unwrap_or(true);

                if is_ctrl_c {
                    if let Err(e) = self.pty.write_input(&tid, &[0x03]) {
                        return Ok(ToolResult::err(format!("Failed to send Ctrl+C: {e}")));
                    }
                } else if let Some(ref input_str) = parsed.input {
                    let mut content = input_str.clone();
                    if press_enter && !content.ends_with('\r') && !content.ends_with('\n') {
                        content.push('\r');
                    }
                    if let Err(e) = self.pty.write_input(&tid, content.as_bytes()) {
                        return Ok(ToolResult::err(format!("Failed to send input: {e}")));
                    }
                } else {
                    return Err(ToolError::InvalidArgs(
                        "Either input or ctrl_c must be specified for send_input".to_string(),
                    ));
                }

                sleep(Duration::from_millis(600)).await;
                let snap = self.pty.plain_text_snapshot(&tid, 40);
                Ok(ToolResult::ok(snap))
            }
            "restart_tab" => {
                let tabs = self.pty.list_tabs();
                let tid = if let Some(ref id) = parsed.tab_id {
                    id.clone()
                } else if let Some(t) = tabs.first() {
                    t.tab_id.clone()
                } else {
                    return Ok(ToolResult::err("No PTY terminal tabs found."));
                };
                match self.pty.restart_tab(&tid) {
                    Ok(()) => Ok(ToolResult::ok(format!("Restarted PTY tab {tid}."))),
                    Err(e) => Ok(ToolResult::err(format!("Failed to restart PTY tab: {e}"))),
                }
            }
            "close_tab" => {
                let tabs = self.pty.list_tabs();
                let tid = if let Some(ref id) = parsed.tab_id {
                    id.clone()
                } else if let Some(t) = tabs.first() {
                    t.tab_id.clone()
                } else {
                    return Ok(ToolResult::err("No PTY terminal tabs found."));
                };
                match self.pty.close_tab(&tid) {
                    Ok(()) => Ok(ToolResult::ok(format!("Closed PTY tab {tid}."))),
                    Err(e) => Ok(ToolResult::err(format!("Failed to close PTY tab: {e}"))),
                }
            }
            other => Err(ToolError::InvalidArgs(format!(
                "Unknown pty_terminal action: {other}"
            ))),
        }
    }
}
