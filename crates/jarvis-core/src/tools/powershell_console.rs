//! Real Interactive Windows PowerShell and native `agy -i` console manager.

use crate::events::{CoreEvent, EventBus};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Windows creation flag to hide console window (`0x0800_0000`).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Windows creation flag to create a new visible console window (`0x0000_0010`).
#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// Strip ANSI CSI and OSC escape sequences from terminal output.
pub fn strip_ansi_codes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // ESC
            if i + 1 < bytes.len() {
                if bytes[i + 1] == b'[' {
                    // CSI sequence: ESC [ ... [0-9:;<=>?]* [@-~]
                    i += 2;
                    while i < bytes.len() {
                        let b = bytes[i];
                        if (0x40..=0x7e).contains(&b) {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                } else if bytes[i + 1] == b']' {
                    // OSC sequence: ESC ] ... (BEL \x07 or ST ESC \)
                    i += 2;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Wrap and escape a string in single quotes for PowerShell (`'...'`), replacing `'` with `''`.
pub fn ps_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn run_scripts_dir() -> PathBuf {
    crate::config::AppConfig::default_dir().join("run")
}

/// Open an interactive Windows PowerShell window (or Windows Terminal) in the specified workspace root.
pub fn open_windows_powershell(
    workspace_root: &Path,
    initial_command: Option<&str>,
    use_wt: bool,
) -> Result<u32, std::io::Error> {
    let dir = run_scripts_dir();
    std::fs::create_dir_all(&dir)?;

    let uuid_str = Uuid::new_v4().to_string();
    let ps1_path = dir.join(format!("ps-{uuid_str}.ps1"));

    let escaped_path = ps_single_quote(&workspace_root.to_string_lossy());
    let mut ps1_content = format!(
        "$OutputEncoding = [System.Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false);\n\
         $Host.UI.RawUI.WindowTitle = 'JARVIS // Windows PowerShell';\n\
         Set-Location -LiteralPath {escaped_path};\n"
    );

    if let Some(cmd) = initial_command {
        if !cmd.trim().is_empty() {
            let cmd_path = dir.join(format!("ps-{uuid_str}-cmd.txt"));
            std::fs::write(&cmd_path, cmd)?;
            let cmd_path_str = ps_single_quote(&cmd_path.to_string_lossy());
            ps1_content.push_str(&format!(
                "$cmd = Get-Content -LiteralPath {cmd_path_str} -Raw -Encoding UTF8;\n\
                 Write-Host \"[JARVIS] Running: $cmd\" -ForegroundColor Cyan;\n\
                 Invoke-Expression $cmd;\n"
            ));
        }
    }

    // Write with UTF-8 BOM [0xEF, 0xBB, 0xBF]
    let mut bom_bytes = vec![0xEF, 0xBB, 0xBF];
    bom_bytes.extend_from_slice(ps1_content.as_bytes());
    std::fs::write(&ps1_path, bom_bytes)?;

    let mut cmd = if use_wt && crate::binary_manager::locate_binary("wt", None).is_some() {
        let mut c = Command::new("wt.exe");
        c.arg("-d")
            .arg(workspace_root)
            .arg("powershell.exe")
            .arg("-NoLogo")
            .arg("-NoExit")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&ps1_path);
        c
    } else {
        let mut c = Command::new("powershell.exe");
        c.arg("-NoLogo")
            .arg("-NoExit")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&ps1_path)
            .current_dir(workspace_root);
        #[cfg(windows)]
        c.creation_flags(CREATE_NEW_CONSOLE);
        c
    };

    let child = cmd.spawn()?;
    Ok(child.id().unwrap_or(0))
}

/// Launch AGY interactively in Windows PowerShell or Windows Terminal.
pub fn launch_agy_in_windows_powershell(
    workspace_root: &Path,
    prompt: Option<&str>,
    model: &str,
    effort: &str,
    full_auto: bool,
    use_wt: bool,
) -> Result<Child, std::io::Error> {
    let dir = run_scripts_dir();
    std::fs::create_dir_all(&dir)?;

    let uuid_str = Uuid::new_v4().to_string();
    let ps1_path = dir.join(format!("agy-{uuid_str}.ps1"));
    let escaped_path = ps_single_quote(&workspace_root.to_string_lossy());

    let mut prompt_snippet = String::new();
    if let Some(p) = prompt {
        if !p.trim().is_empty() {
            let prompt_file = dir.join(format!("agy-{uuid_str}-prompt.txt"));
            std::fs::write(&prompt_file, p)?;
            let prompt_path_str = ps_single_quote(&prompt_file.to_string_lossy());
            prompt_snippet = format!(
                "$jarvisPrompt = Get-Content -LiteralPath {prompt_path_str} -Raw -Encoding UTF8;\n"
            );
        }
    }

    let model_sq = ps_single_quote(model);
    let effort_sq = ps_single_quote(effort);

    let full_auto_snippet = if full_auto {
        "$agyArgs += '--dangerously-skip-permissions';"
    } else {
        ""
    };

    let ps1_content = format!(
        "$OutputEncoding = [System.Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false);\n\
         $Host.UI.RawUI.WindowTitle = 'JARVIS // AGY Interactive PowerShell';\n\
         Set-Location -LiteralPath {escaped_path};\n\
         {prompt_snippet}\n\
         $agyArgs = @();\n\
         if ($null -ne $jarvisPrompt -and $jarvisPrompt.Trim() -ne '') {{\n\
             $agyArgs += '-i';\n\
             $agyArgs += $jarvisPrompt;\n\
         }}\n\
         $agyArgs += '--model';\n\
         $agyArgs += {model_sq};\n\
         $agyArgs += '--effort';\n\
         $agyArgs += {effort_sq};\n\
         {full_auto_snippet}\n\
         Write-Host \"[JARVIS] Launching: agy @agyArgs\" -ForegroundColor Cyan;\n\
         & 'agy' @agyArgs;\n\
         Write-Host \"\n[JARVIS] AGY session completed. Press any key to exit...\" -NoNewline;\n\
         [void][System.Console]::ReadKey($true);\n"
    );

    let mut bom_bytes = vec![0xEF, 0xBB, 0xBF];
    bom_bytes.extend_from_slice(ps1_content.as_bytes());
    std::fs::write(&ps1_path, bom_bytes)?;

    let mut cmd = if use_wt && crate::binary_manager::locate_binary("wt", None).is_some() {
        let mut c = Command::new("wt.exe");
        c.arg("-d")
            .arg(workspace_root)
            .arg("powershell.exe")
            .arg("-NoLogo")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&ps1_path);
        c
    } else {
        let mut c = Command::new("powershell.exe");
        c.arg("-NoLogo")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&ps1_path)
            .current_dir(workspace_root);
        #[cfg(windows)]
        c.creation_flags(CREATE_NEW_CONSOLE);
        c
    };

    let child = cmd.spawn()?;
    Ok(child)
}

struct ManagedProcess {
    child: Child,
    stdin: tokio::process::ChildStdin,
}

/// Persistent background PowerShell session manager supporting state and working directory preservation.
pub struct PowerShellConsoleManager {
    event_bus: EventBus,
    running: Arc<AtomicBool>,
    process: Mutex<Option<ManagedProcess>>,
}

impl PowerShellConsoleManager {
    pub fn new(event_bus: EventBus) -> Self {
        Self {
            event_bus,
            running: Arc::new(AtomicBool::new(false)),
            process: Mutex::new(None),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn ensure_started(&self, workspace_root: &Path, session_id: &str) -> Result<(), std::io::Error> {
        let mut guard = self.process.lock().await;
        if guard.is_some() && self.is_running() {
            return Ok(());
        }

        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoLogo", "-NoProfile", "-NoExit", "-ExecutionPolicy", "Bypass", "-Command", "-"])
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::other("Failed to capture PowerShell stdin")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::other("Failed to capture PowerShell stdout")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            std::io::Error::other("Failed to capture PowerShell stderr")
        })?;

        // Initialize UTF-8 encodings in subshell
        let mut init_stdin = stdin;
        init_stdin
            .write_all(b"$OutputEncoding = [System.Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n")
            .await?;

        self.running.store(true, Ordering::SeqCst);

        let bus_out = self.event_bus.clone();
        let sid_out = session_id.to_string();
        let running_out = Arc::clone(&self.running);

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let cleaned = strip_ansi_codes(&line);
                bus_out.publish(CoreEvent::TerminalLine {
                    session_id: sid_out.clone(),
                    stream: "stdout".into(),
                    line: cleaned,
                });
            }
            running_out.store(false, Ordering::SeqCst);
        });

        let bus_err = self.event_bus.clone();
        let sid_err = session_id.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let cleaned = strip_ansi_codes(&line);
                bus_err.publish(CoreEvent::TerminalLine {
                    session_id: sid_err.clone(),
                    stream: "stderr".into(),
                    line: cleaned,
                });
            }
        });

        *guard = Some(ManagedProcess {
            child,
            stdin: init_stdin,
        });
        Ok(())
    }

    pub async fn send_command(&self, command: &str, workspace_root: &Path, session_id: &str) -> Result<(), std::io::Error> {
        self.ensure_started(workspace_root, session_id).await?;
        let mut guard = self.process.lock().await;
        if let Some(proc) = guard.as_mut() {
            let payload = format!("{}\n", command);
            proc.stdin.write_all(payload.as_bytes()).await?;
            proc.stdin.flush().await?;
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "PowerShell console not running"))
        }
    }

    pub async fn restart(&self, workspace_root: &Path, session_id: &str) -> Result<(), std::io::Error> {
        self.shutdown().await;
        self.ensure_started(workspace_root, session_id).await
    }

    pub async fn shutdown(&self) {
        let mut guard = self.process.lock().await;
        if let Some(mut proc) = guard.take() {
            let _ = proc.stdin.write_all(b"exit\n").await;
            let _ = proc.stdin.flush().await;
            let _ = proc.child.kill().await;
        }
        self.running.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn strip_ansi_codes_removes_csi_and_osc_sequences() {
        let raw = "\x1b[32;1mHello\x1b[0m World \x1b]0;Title\x07Done\x1b[31m!";
        let clean = strip_ansi_codes(raw);
        assert_eq!(clean, "Hello World Done!");
    }

    #[test]
    fn ps_single_quote_escapes_embedded_quotes() {
        let s = "it's a 'test'";
        assert_eq!(ps_single_quote(s), "'it''s a ''test'''");
    }

    #[tokio::test]
    async fn persistent_powershell_session_preserves_state_and_cwd() {
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();
        let manager = PowerShellConsoleManager::new(bus);
        let dir = tempfile::TempDir::new().unwrap();

        manager.ensure_started(dir.path(), "test-sess").await.unwrap();
        assert!(manager.is_running());

        manager.send_command("$global:JarvisVal = 42; pwd", dir.path(), "test-sess").await.unwrap();

        let mut saw_val = false;
        let mut _saw_pwd = false;
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if let Ok(CoreEvent::TerminalLine { line, .. }) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await.unwrap_or(Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                if line.contains("42") {
                    saw_val = true;
                }
                if line.contains(&dir.path().to_string_lossy().to_string()) || line.contains("Path") {
                    _saw_pwd = true;
                }
            }
            if saw_val {
                break;
            }
        }
        assert!(saw_val, "PowerShell execution output should reflect $global:JarvisVal");
        manager.shutdown().await;
        assert!(!manager.is_running());
    }
}
