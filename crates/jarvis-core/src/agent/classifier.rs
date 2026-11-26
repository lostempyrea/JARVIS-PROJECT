//! Command & tool risk classifier distinguishing `SafeReadOnly` from `RequiresApproval`.

use crate::events::RiskTier;
use crate::tools::fs_tools::is_path_in_workspace;
use std::path::Path;

/// Result of classifying a command or tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationResult {
    pub tier: RiskTier,
    pub reason: String,
}

impl ClassificationResult {
    fn safe(reason: impl Into<String>) -> Self {
        Self {
            tier: RiskTier::SafeReadOnly,
            reason: reason.into(),
        }
    }

    fn approval(reason: impl Into<String>) -> Self {
        Self {
            tier: RiskTier::RequiresApproval,
            reason: reason.into(),
        }
    }
}

/// Classifies PowerShell commands and JARVIS tool calls into security risk tiers.
pub struct CommandRiskClassifier;

impl CommandRiskClassifier {
    /// Classify a tool call before execution.
    pub fn classify_tool_call(
        tool_name: &str,
        args: &serde_json::Value,
        workspace_root: &Path,
    ) -> ClassificationResult {
        match tool_name {
            "gmail_send_confirmed" => ClassificationResult::approval(
                "Sending email always requires explicit user confirmation",
            ),
            "gmail_search" | "gmail_read" | "gmail_summarize_unread" => {
                ClassificationResult::safe(format!("Read-only Gmail tool '{tool_name}'"))
            }
            "music_control" => {
                ClassificationResult::safe("Media playback control tool 'music_control'")
            }
            "delegate_to_agy" => {
                ClassificationResult::approval("Delegating work to agy sub-agent requires approval")
            }
            "terminal" => {
                let cmd = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                Self::classify_terminal_command(cmd)
            }
            "pty_terminal" => {
                let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("read_screen");
                match action {
                    "list_tabs" | "read_screen" => {
                        ClassificationResult::safe("Read-only pty_terminal action")
                    }
                    "open_agy" => {
                        let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                        if prompt.trim().is_empty() {
                            ClassificationResult::safe("Opening interactive AGY without initial prompt is safe read-only")
                        } else {
                            ClassificationResult::approval("Opening interactive AGY with initial prompt requires approval")
                        }
                    }
                    "open_powershell" => {
                        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        if command.trim().is_empty() {
                            ClassificationResult::safe("Opening empty PowerShell tab is safe read-only")
                        } else {
                            Self::classify_terminal_command(command)
                        }
                    }
                    "send_input" => {
                        let ctrl_c = args.get("ctrl_c").and_then(|v| v.as_bool()).unwrap_or(false);
                        let input = args.get("input").and_then(|v| v.as_str()).unwrap_or("");
                        if ctrl_c && input.trim().is_empty() {
                            ClassificationResult::safe("Sending Ctrl+C signal is safe read-only")
                        } else {
                            Self::classify_terminal_command(input)
                        }
                    }
                    "restart_tab" | "close_tab" => {
                        ClassificationResult::approval(format!("Mutating pty_terminal action '{action}' requires approval"))
                    }
                    _ => {
                        ClassificationResult::approval(format!("Unknown pty_terminal action '{action}' requires approval"))
                    }
                }
            }
            "fs_read" | "fs_glob" | "fs_grep" => {
                let path_key = if tool_name == "fs_read" { "path" } else { "root" };
                if let Some(p) = args.get(path_key).and_then(|v| v.as_str()) {
                    if !p.trim().is_empty() && !is_path_in_workspace(workspace_root, p) {
                        return ClassificationResult::approval(format!(
                            "Read path '{p}' is outside workspace root"
                        ));
                    }
                }
                ClassificationResult::safe(format!("Read-only workspace tool '{tool_name}'"))
            }
            "memory_recall" | "memory_store" => {
                ClassificationResult::safe(format!("Internal memory tool '{tool_name}'"))
            }
            "fs_write" | "fs_edit" => {
                if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                    if !is_path_in_workspace(workspace_root, p) {
                        return ClassificationResult::approval(format!(
                            "Write path '{p}' is outside workspace root"
                        ));
                    }
                }
                ClassificationResult::approval(format!(
                    "Filesystem mutation tool '{tool_name}' requires approval"
                ))
            }
            other => ClassificationResult::approval(format!(
                "Unknown or uncategorized tool '{other}' requires approval"
            )),
        }
    }

    /// Classify a PowerShell command string.
    ///
    /// Returns [`RiskTier::SafeReadOnly`] only when every segment in a pipeline or
    /// command chain is an explicitly allowlisted read-only command without redirection,
    /// unbalanced quotes, grouping/script-block syntax, or subexpression injection.
    pub fn classify_terminal_command(cmd: &str) -> ClassificationResult {
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return ClassificationResult::approval("Empty command requires approval");
        }

        if has_unbalanced_quotes(trimmed) {
            return ClassificationResult::approval(
                "Command contains unbalanced quotes or trailing escape",
            );
        }

        if contains_unquoted_subexpression(trimmed) {
            return ClassificationResult::approval(
                "Command contains subexpression '$()', '@()', script block '{}', grouping '()', or backtick execution",
            );
        }

        if contains_unquoted_redirection(trimmed) {
            return ClassificationResult::approval(
                "Command contains output redirection ('>' or '>>')",
            );
        }

        let segments = split_command_segments(trimmed);
        if segments.is_empty() {
            return ClassificationResult::approval("Unable to parse command segments");
        }

        for seg in segments {
            let res = classify_single_segment(&seg);
            if res.tier == RiskTier::RequiresApproval {
                return res;
            }
        }

        ClassificationResult::safe("All command segments are safe read-only operations")
    }
}

fn classify_single_segment(segment: &str) -> ClassificationResult {
    let Some(tokens) = tokenize_shell(segment) else {
        return ClassificationResult::approval("Command segment contains unbalanced quotes");
    };
    if tokens.is_empty() {
        return ClassificationResult::approval("Empty command segment");
    }

    let head = tokens[0].to_ascii_lowercase();

    // Explicitly blocked mutating / dangerous commands.
    const MUTATING_COMMANDS: &[&str] = &[
        "remove-item",
        "rm",
        "del",
        "ri",
        "rmdir",
        "rd",
        "erase",
        "winget",
        "stop-process",
        "kill",
        "spps",
        "invoke-expression",
        "iex",
        "invoke-webrequest",
        "iwr",
        "invoke-restmethod",
        "irm",
        "out-file",
        "set-content",
        "add-content",
        "clear-content",
        "new-item",
        "move-item",
        "mv",
        "copy-item",
        "cp",
        "rename-item",
        "start-process",
        "saps",
        "taskkill",
        "reg",
        "sc",
        "net",
        "shutdown",
        "restart-computer",
        "stop-computer",
    ];
    if MUTATING_COMMANDS.contains(&head.as_str()) {
        return ClassificationResult::approval(format!(
            "Mutating or high-impact command '{head}' requires approval"
        ));
    }

    // Disallow script-block-taking cmdlets whenever braces `{` / `}` are present.
    if matches!(
        head.as_str(),
        "where-object" | "where" | "select-object" | "select" | "foreach-object" | "for"
    ) && segment.contains(['{', '}'])
    {
        return ClassificationResult::approval(format!(
            "Script block cmdlet '{head}' with '{{'/'}}' requires approval"
        ));
    }

    // Safe read-only PowerShell cmdlets & aliases.
    const SAFE_READONLY_CMDS: &[&str] = &[
        "get-childitem",
        "gci",
        "dir",
        "ls",
        "get-content",
        "gc",
        "cat",
        "type",
        "get-process",
        "gps",
        "ps",
        "get-date",
        "get-location",
        "gl",
        "pwd",
        "get-item",
        "gi",
        "test-path",
        "measure-object",
        "select-object",
        "select",
        "select-string",
        "sls",
        "where-object",
        "where",
        "sort-object",
        "sort",
        "format-table",
        "ft",
        "format-list",
        "fl",
        "out-string",
        "write-output",
        "echo",
    ];
    if SAFE_READONLY_CMDS.contains(&head.as_str()) {
        return ClassificationResult::safe(format!("Safe read-only command '{head}'"));
    }

    if head == "git" || head == "git.exe" {
        return classify_git(&tokens[1..]);
    }

    if head == "cargo" || head == "cargo.exe" {
        return classify_cargo(&tokens[1..]);
    }

    ClassificationResult::approval(format!(
        "Unlisted command '{head}' requires approval"
    ))
}

fn classify_git(args: &[String]) -> ClassificationResult {
    // Skip safe leading global flags like `-C <dir>` or `--no-pager`,
    // while rejecting code-execution / path-override flags (`-c`, `--config-env`, `--exec-path`, `--git-dir`, `--work-tree`).
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "-c"
            || arg.starts_with("-c")
            || arg == "--config-env"
            || arg.starts_with("--config-env=")
            || arg == "--exec-path"
            || arg.starts_with("--exec-path=")
            || arg == "--git-dir"
            || arg.starts_with("--git-dir=")
            || arg == "--work-tree"
            || arg.starts_with("--work-tree=")
        {
            return ClassificationResult::approval(format!(
                "Git global flag '{arg}' can override config or execution paths and requires approval"
            ));
        }
        if arg == "-C" {
            i += 2;
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }

    let Some(sub) = args.get(i).map(|s| s.to_ascii_lowercase()) else {
        return ClassificationResult::safe("git help/status");
    };

    match sub.as_str() {
        "diff" | "log" | "show" => {
            if args[i + 1..]
                .iter()
                .any(|a| a == "--output" || a.starts_with("--output="))
            {
                return ClassificationResult::approval(format!(
                    "'git {sub} --output' writes to a file and requires approval"
                ));
            }
            ClassificationResult::safe(format!("Safe read-only git subcommand '{sub}'"))
        }
        "status" | "rev-parse" | "describe" | "ls-files" => {
            ClassificationResult::safe(format!("Safe read-only git subcommand '{sub}'"))
        }
        "branch" => {
            let mutating = args[i + 1..].iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-d" | "-D" | "-m" | "-M" | "-c" | "-C" | "--delete" | "--move" | "--copy"
                )
            });
            if mutating {
                ClassificationResult::approval("Mutating 'git branch' flag requires approval")
            } else {
                ClassificationResult::safe("Read-only 'git branch'")
            }
        }
        other => ClassificationResult::approval(format!(
            "Mutating or state-changing 'git {other}' requires approval"
        )),
    }
}

fn classify_cargo(args: &[String]) -> ClassificationResult {
    let Some(sub) = args.first().map(|s| s.to_ascii_lowercase()) else {
        return ClassificationResult::safe("cargo help");
    };
    match sub.as_str() {
        "check" | "metadata" | "tree" | "version" | "--version" | "help" => {
            ClassificationResult::safe(format!("Safe read-only 'cargo {sub}'"))
        }
        other => ClassificationResult::approval(format!(
            "Command 'cargo {other}' can execute build scripts/binaries and requires approval"
        )),
    }
}

/// PowerShell-aware quote scanner step.
///
/// Inside single quotes: `''` is an escaped quote.
/// Inside double quotes: `` `x `` escapes the next char, and `""` is an escaped quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteState {
    Unquoted,
    Single,
    Double,
}

fn has_unbalanced_quotes(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let mut state = QuoteState::Unquoted;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match state {
            QuoteState::Unquoted => {
                if c == '\'' {
                    state = QuoteState::Single;
                } else if c == '"' {
                    state = QuoteState::Double;
                }
                i += 1;
            }
            QuoteState::Single => {
                if c == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            QuoteState::Double => {
                if c == '`' {
                    if i + 1 < chars.len() {
                        i += 2;
                    } else {
                        return true;
                    }
                } else if c == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
        }
    }

    state != QuoteState::Unquoted
}

fn contains_unquoted_subexpression(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let mut state = QuoteState::Unquoted;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match state {
            QuoteState::Unquoted => {
                if c == '\'' {
                    state = QuoteState::Single;
                    i += 1;
                } else if c == '"' {
                    state = QuoteState::Double;
                    i += 1;
                } else if c == '`'
                    || ((c == '$' || c == '@') && chars.get(i + 1) == Some(&'('))
                    || matches!(c, '{' | '}' | '(' | ')')
                {
                    return true;
                } else {
                    i += 1;
                }
            }
            QuoteState::Single => {
                if c == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            QuoteState::Double => {
                if c == '`' {
                    if i + 1 < chars.len() {
                        i += 2;
                    } else {
                        return true;
                    }
                } else if c == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        i += 1;
                    }
                } else if c == '$' && chars.get(i + 1) == Some(&'(') {
                    return true;
                } else {
                    i += 1;
                }
            }
        }
    }
    false
}

fn contains_unquoted_redirection(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let mut state = QuoteState::Unquoted;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match state {
            QuoteState::Unquoted => {
                if c == '\'' {
                    state = QuoteState::Single;
                    i += 1;
                } else if c == '"' {
                    state = QuoteState::Double;
                    i += 1;
                } else if c == '>' {
                    return true;
                } else {
                    i += 1;
                }
            }
            QuoteState::Single => {
                if c == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            QuoteState::Double => {
                if c == '`' {
                    if i + 1 < chars.len() {
                        i += 2;
                    } else {
                        return true;
                    }
                } else if c == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
    false
}

fn split_command_segments(s: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut state = QuoteState::Unquoted;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match state {
            QuoteState::Single => {
                if c == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        current.push('\'');
                        current.push('\'');
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        current.push(c);
                        i += 1;
                    }
                } else {
                    current.push(c);
                    i += 1;
                }
            }
            QuoteState::Double => {
                if c == '`' {
                    current.push(c);
                    if let Some(&next) = chars.get(i + 1) {
                        current.push(next);
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else if c == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        current.push('"');
                        current.push('"');
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        current.push(c);
                        i += 1;
                    }
                } else {
                    current.push(c);
                    i += 1;
                }
            }
            QuoteState::Unquoted => {
                if c == '\'' {
                    state = QuoteState::Single;
                    current.push(c);
                    i += 1;
                } else if c == '"' {
                    state = QuoteState::Double;
                    current.push(c);
                    i += 1;
                } else if c == ';' || c == '|' || c == '\n' {
                    if !current.trim().is_empty() {
                        segments.push(current.trim().to_string());
                    }
                    current.clear();
                    if c == '|' && chars.get(i + 1) == Some(&'|') {
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else if c == '&' {
                    if !current.trim().is_empty() {
                        segments.push(current.trim().to_string());
                    }
                    current.clear();
                    if chars.get(i + 1) == Some(&'&') {
                        i += 2;
                    } else {
                        i += 1;
                    }
                } else {
                    current.push(c);
                    i += 1;
                }
            }
        }
    }

    if !current.trim().is_empty() {
        segments.push(current.trim().to_string());
    }
    segments
}

fn tokenize_shell(s: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut state = QuoteState::Unquoted;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match state {
            QuoteState::Single => {
                if c == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        current.push('\'');
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        i += 1;
                    }
                } else {
                    current.push(c);
                    i += 1;
                }
            }
            QuoteState::Double => {
                if c == '`' {
                    if let Some(&next) = chars.get(i + 1) {
                        current.push(next);
                        i += 2;
                    } else {
                        return None;
                    }
                } else if c == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        current.push('"');
                        i += 2;
                    } else {
                        state = QuoteState::Unquoted;
                        i += 1;
                    }
                } else {
                    current.push(c);
                    i += 1;
                }
            }
            QuoteState::Unquoted => {
                if c == '\'' {
                    state = QuoteState::Single;
                    i += 1;
                } else if c == '"' {
                    state = QuoteState::Double;
                    i += 1;
                } else if c.is_whitespace() {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                    i += 1;
                } else {
                    current.push(c);
                    i += 1;
                }
            }
        }
    }

    if state != QuoteState::Unquoted {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn safe_read_only_powershell_commands() {
        let safe = [
            "Get-ChildItem",
            "Get-Content Cargo.toml",
            "Get-Process | Select-Object -First 5",
            "Get-Date",
            "Get-Location",
            "git status",
            "git log -n 5",
            "git diff HEAD~1",
            "cargo check",
            "dir",
            "ls",
            "pwd",
            "echo 'hello world'",
        ];
        for cmd in safe {
            let res = CommandRiskClassifier::classify_terminal_command(cmd);
            assert_eq!(res.tier, RiskTier::SafeReadOnly, "expected safe for: {cmd}");
        }
    }

    #[test]
    fn mutating_and_chained_commands_require_approval() {
        let dangerous = [
            "Remove-Item foo.txt",
            "rm -rf ./target",
            "del secret.txt",
            "winget install foo",
            "Stop-Process -Name notepad",
            "git push origin main",
            "git reset --hard HEAD",
            "Invoke-Expression $payload",
            "iex (Get-Content script.ps1)",
            "Get-ChildItem; Remove-Item foo.txt",
            "Get-Process | Stop-Process",
            "echo hello > out.txt",
            "Get-Date >> log.txt",
            "Write-Output $(Remove-Item x)",
        ];
        for cmd in dangerous {
            let res = CommandRiskClassifier::classify_terminal_command(cmd);
            assert_eq!(
                res.tier,
                RiskTier::RequiresApproval,
                "expected RequiresApproval for: {cmd}"
            );
        }
    }

    #[test]
    fn powershell_bypass_vectors_require_approval() {
        let bypasses = [
            "Get-ChildItem | Where-Object { Remove-Item secret.txt }",
            "Write-Output (Remove-Item secret.txt)",
            r#"echo "x`"" ; Remove-Item secret.txt; echo "z""#,
            "git -c diff.external=calc.exe diff",
            "git -cdiff.external=calc.exe status",
            "git --exec-path=/tmp status",
            "git --exec-path /tmp status",
            "git --git-dir=/tmp/.git status",
            "git --work-tree=/tmp status",
            "Write-Output @(Remove-Item secret.txt)",
            "echo \"unbalanced",
            "echo 'unbalanced",
            "echo \"x\"\" ; Remove-Item secret.txt; echo \"z\"",
            "echo \"pwn\"=>evil.txt",
            "Write-Output x=>evil.ps1",
            "git --config-env=diff.external=COMSPEC diff",
            "git --config-env diff.external=COMSPEC diff",
            "git diff --output=evil.txt",
            "git log --output evil.txt",
            "git show --output=evil.txt",
        ];
        for cmd in bypasses {
            let res = CommandRiskClassifier::classify_terminal_command(cmd);
            assert_eq!(
                res.tier,
                RiskTier::RequiresApproval,
                "expected RequiresApproval for bypass vector: {cmd}"
            );
        }

        // Valid escaped quotes in a safe read-only command remain allowed
        assert_eq!(
            CommandRiskClassifier::classify_terminal_command(
                r#"echo "hello `"world`"" ; echo 'it''s ok'"#
            )
            .tier,
            RiskTier::SafeReadOnly
        );
    }

    #[test]
    fn tool_classification_gates_writes_agy_and_gmail() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("pwn.txt").to_string_lossy().to_string();

        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "fs_read",
                &serde_json::json!({"path": "Cargo.toml"}),
                dir.path()
            )
            .tier,
            RiskTier::SafeReadOnly
        );

        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "fs_read",
                &serde_json::json!({"path": outside_file}),
                dir.path()
            )
            .tier,
            RiskTier::RequiresApproval
        );

        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "fs_write",
                &serde_json::json!({"path": outside_file, "content": "x"}),
                dir.path()
            )
            .tier,
            RiskTier::RequiresApproval
        );

        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "delegate_to_agy",
                &serde_json::json!({"prompt": "refactor"}),
                dir.path()
            )
            .tier,
            RiskTier::RequiresApproval
        );

        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "gmail_send_confirmed",
                &serde_json::json!({"to": "a@b.com"}),
                dir.path()
            )
            .tier,
            RiskTier::RequiresApproval
        );
    }

    #[test]
    fn pty_terminal_classification_gates_mutating_actions() {
        let dir = TempDir::new().unwrap();

        // list_tabs & read_screen are safe read-only
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "pty_terminal",
                &serde_json::json!({"action": "list_tabs"}),
                dir.path()
            )
            .tier,
            RiskTier::SafeReadOnly
        );
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "pty_terminal",
                &serde_json::json!({"action": "read_screen"}),
                dir.path()
            )
            .tier,
            RiskTier::SafeReadOnly
        );

        // open_agy without prompt is safe read-only
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "pty_terminal",
                &serde_json::json!({"action": "open_agy"}),
                dir.path()
            )
            .tier,
            RiskTier::SafeReadOnly
        );
        // open_agy with prompt requires approval
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "pty_terminal",
                &serde_json::json!({"action": "open_agy", "prompt": "fix bug"}),
                dir.path()
            )
            .tier,
            RiskTier::RequiresApproval
        );

        // open_powershell with safe command is safe read-only
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "pty_terminal",
                &serde_json::json!({"action": "open_powershell", "command": "Get-ChildItem"}),
                dir.path()
            )
            .tier,
            RiskTier::SafeReadOnly
        );
        // open_powershell with mutating command requires approval
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "pty_terminal",
                &serde_json::json!({"action": "open_powershell", "command": "Remove-Item foo"}),
                dir.path()
            )
            .tier,
            RiskTier::RequiresApproval
        );

        // close_tab requires approval
        assert_eq!(
            CommandRiskClassifier::classify_tool_call(
                "pty_terminal",
                &serde_json::json!({"action": "close_tab", "tab_id": "1"}),
                dir.path()
            )
            .tier,
            RiskTier::RequiresApproval
        );
    }
}
