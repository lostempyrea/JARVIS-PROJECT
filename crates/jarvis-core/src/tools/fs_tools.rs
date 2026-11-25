//! Workspace-scoped filesystem tools: `fs_read`, `fs_write`, `fs_edit`, `fs_glob`, `fs_grep`.

use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use glob::Pattern;
use regex::Regex;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_MATCHES: usize = 200;

/// Strip Windows verbatim `\\?\` prefix so path comparisons and user display stay consistent.
pub fn normalize_verbatim_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

fn paths_within(candidate: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let c_str = candidate.to_string_lossy().to_lowercase();
        let r_str = root.to_string_lossy().to_lowercase();
        if c_str == r_str {
            return true;
        }
        let r_with_sep = if r_str.ends_with('\\') || r_str.ends_with('/') {
            r_str
        } else {
            format!("{r_str}\\")
        };
        c_str.starts_with(&r_with_sep)
    }
    #[cfg(not(windows))]
    {
        candidate.starts_with(root)
    }
}

/// Canonicalize `requested` relative to `workspace_root` and verify it stays inside `workspace_root`.
pub fn resolve_workspace_path(
    workspace_root: &Path,
    requested: &str,
) -> Result<PathBuf, ToolError> {
    let canon_root = normalize_verbatim_path(std::fs::canonicalize(workspace_root)?);
    let req_path = Path::new(requested);
    let candidate = if req_path.is_absolute() {
        req_path.to_path_buf()
    } else {
        canon_root.join(req_path)
    };

    let resolved = if candidate.exists() {
        normalize_verbatim_path(std::fs::canonicalize(&candidate)?)
    } else {
        // Canonicalize nearest existing ancestor and append clean relative tail.
        let mut existing = candidate.as_path();
        let mut tail: Vec<String> = Vec::new();
        while !existing.exists() {
            let name = existing
                .file_name()
                .ok_or_else(|| ToolError::OutsideWorkspace {
                    path: requested.to_string(),
                    workspace: canon_root.display().to_string(),
                })?;
            tail.push(name.to_string_lossy().to_string());
            existing = existing.parent().ok_or_else(|| ToolError::OutsideWorkspace {
                path: requested.to_string(),
                workspace: canon_root.display().to_string(),
            })?;
        }
        let mut base = normalize_verbatim_path(std::fs::canonicalize(existing)?);
        for seg in tail.into_iter().rev() {
            if seg == ".." {
                return Err(ToolError::OutsideWorkspace {
                    path: requested.to_string(),
                    workspace: canon_root.display().to_string(),
                });
            }
            if seg != "." {
                base.push(seg);
            }
        }
        // Also verify no `..` components remained in original path.
        if candidate
            .components()
            .any(|c| matches!(c, Component::ParentDir))
            && !paths_within(&base, &canon_root)
        {
            return Err(ToolError::OutsideWorkspace {
                path: requested.to_string(),
                workspace: canon_root.display().to_string(),
            });
        }
        base
    };

    if !paths_within(&resolved, &canon_root) {
        return Err(ToolError::OutsideWorkspace {
            path: requested.to_string(),
            workspace: canon_root.display().to_string(),
        });
    }

    Ok(resolved)
}

/// Check whether a path stays inside `workspace_root` without requiring the target to exist.
pub fn is_path_in_workspace(workspace_root: &Path, requested: &str) -> bool {
    resolve_workspace_path(workspace_root, requested).is_ok()
}

// -----------------------------------------------------------------------------
// fs_read
// -----------------------------------------------------------------------------

pub struct FsReadTool;

#[derive(Deserialize)]
struct FsReadArgs {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

#[async_trait]
impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }

    fn description(&self) -> &str {
        "Read UTF-8 text file contents within the workspace root, optionally sliced by line range."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative or workspace-scoped path" },
                "start_line": { "type": "integer", "description": "1-based inclusive start line" },
                "end_line": { "type": "integer", "description": "1-based inclusive end line" }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: FsReadArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let target = resolve_workspace_path(&ctx.workspace_root, &parsed.path)?;
        let content = tokio::fs::read_to_string(&target).await?;

        let output = if parsed.start_line.is_some() || parsed.end_line.is_some() {
            let start = parsed.start_line.unwrap_or(1).max(1);
            let end = parsed.end_line.unwrap_or(usize::MAX);
            content
                .lines()
                .enumerate()
                .filter_map(|(idx, line)| {
                    let ln = idx + 1;
                    if ln >= start && ln <= end {
                        Some(format!("{ln}: {line}"))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            content
        };

        if output.len() > MAX_READ_BYTES {
            let mut end = MAX_READ_BYTES;
            while end > 0 && !output.is_char_boundary(end) {
                end -= 1;
            }
            Ok(ToolResult::ok(format!(
                "{}\n...[truncated to {} bytes]",
                &output[..end],
                MAX_READ_BYTES
            )))
        } else {
            Ok(ToolResult::ok(output))
        }
    }
}

// -----------------------------------------------------------------------------
// fs_write
// -----------------------------------------------------------------------------

pub struct FsWriteTool;

#[derive(Deserialize)]
struct FsWriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }

    fn description(&self) -> &str {
        "Write or overwrite a file within the workspace root, creating parent directories if needed."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: FsWriteArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let target = resolve_workspace_path(&ctx.workspace_root, &parsed.path)?;
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&target, parsed.content.as_bytes()).await?;
        Ok(ToolResult::ok(format!(
            "Wrote {} bytes to {}",
            parsed.content.len(),
            target.display()
        )))
    }
}

// -----------------------------------------------------------------------------
// fs_edit
// -----------------------------------------------------------------------------

pub struct FsEditTool;

#[derive(Deserialize)]
struct FsEditArgs {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for FsEditTool {
    fn name(&self) -> &str {
        "fs_edit"
    }

    fn description(&self) -> &str {
        "Replace exact text in a workspace file."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string" },
                "new_text": { "type": "string" },
                "replace_all": { "type": "boolean" }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: FsEditArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        if parsed.old_text.is_empty() {
            return Err(ToolError::InvalidArgs(
                "old_text must not be empty".to_string(),
            ));
        }

        let target = resolve_workspace_path(&ctx.workspace_root, &parsed.path)?;
        let content = tokio::fs::read_to_string(&target).await?;
        let count = content.matches(&parsed.old_text).count();
        if count == 0 {
            return Ok(ToolResult::err(format!(
                "Target text not found in {}",
                target.display()
            )));
        }
        if count > 1 && !parsed.replace_all {
            return Ok(ToolResult::err(format!(
                "Target text matched {count} times in {}; pass replace_all=true or provide more context",
                target.display()
            )));
        }

        let updated = if parsed.replace_all {
            content.replace(&parsed.old_text, &parsed.new_text)
        } else {
            content.replacen(&parsed.old_text, &parsed.new_text, 1)
        };
        tokio::fs::write(&target, updated.as_bytes()).await?;
        Ok(ToolResult::ok(format!(
            "Updated {} ({count} replacement(s))",
            target.display()
        )))
    }
}

// -----------------------------------------------------------------------------
// fs_glob
// -----------------------------------------------------------------------------

pub struct FsGlobTool;

#[derive(Deserialize)]
struct FsGlobArgs {
    pattern: String,
    #[serde(default)]
    root: Option<String>,
}

#[async_trait]
impl Tool for FsGlobTool {
    fn name(&self) -> &str {
        "fs_glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern within the workspace root."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern such as '**/*.rs'" },
                "root": { "type": "string", "description": "Optional workspace-relative subdirectory" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: FsGlobArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let base = match parsed.root.as_deref() {
            Some(sub) if !sub.trim().is_empty() => {
                resolve_workspace_path(&ctx.workspace_root, sub)?
            }
            _ => normalize_verbatim_path(std::fs::canonicalize(&ctx.workspace_root)?),
        };
        let pattern = Pattern::new(&parsed.pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("invalid glob pattern: {e}")))?;

        let matches = tokio::task::spawn_blocking(move || {
            let mut found = Vec::new();
            for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
                if !entry.file_type().is_file() {
                    continue;
                }
                if let Ok(rel) = entry.path().strip_prefix(&base) {
                    let rel_slash = rel.to_string_lossy().replace('\\', "/");
                    if pattern.matches(&rel_slash) {
                        found.push(rel_slash);
                        if found.len() >= MAX_MATCHES {
                            break;
                        }
                    }
                }
            }
            found.sort();
            found
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;

        if matches.is_empty() {
            Ok(ToolResult::ok("(no matching files)"))
        } else {
            Ok(ToolResult::ok(matches.join("\n")))
        }
    }
}

// -----------------------------------------------------------------------------
// fs_grep
// -----------------------------------------------------------------------------

pub struct FsGrepTool;

#[derive(Deserialize)]
struct FsGrepArgs {
    pattern: String,
    #[serde(default)]
    root: Option<String>,
}

#[async_trait]
impl Tool for FsGrepTool {
    fn name(&self) -> &str {
        "fs_grep"
    }

    fn description(&self) -> &str {
        "Search workspace files for lines matching a regular expression."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for" },
                "root": { "type": "string", "description": "Optional workspace-relative subdirectory or file" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: FsGrepArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let regex = Regex::new(&parsed.pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("invalid regex: {e}")))?;
        let base = match parsed.root.as_deref() {
            Some(sub) if !sub.trim().is_empty() => {
                resolve_workspace_path(&ctx.workspace_root, sub)?
            }
            _ => normalize_verbatim_path(std::fs::canonicalize(&ctx.workspace_root)?),
        };
        let root_canon = normalize_verbatim_path(std::fs::canonicalize(&ctx.workspace_root)?);

        let hits = tokio::task::spawn_blocking(move || {
            let mut results = Vec::new();
            for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                let rel = entry
                    .path()
                    .strip_prefix(&root_canon)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/");
                for (idx, line) in text.lines().enumerate() {
                    if regex.is_match(line) {
                        results.push(format!("{rel}:{}: {line}", idx + 1));
                        if results.len() >= MAX_MATCHES {
                            return results;
                        }
                    }
                }
            }
            results
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;

        if hits.is_empty() {
            Ok(ToolResult::ok("(no matches)"))
        } else {
            Ok(ToolResult::ok(hits.join("\n")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::events::EventBus;
    use tempfile::TempDir;

    fn make_ctx(dir: &TempDir) -> ToolContext {
        ToolContext {
            session_id: "s1".to_string(),
            workspace_root: dir.path().to_path_buf(),
            db: Database::open_in_memory().unwrap(),
            event_bus: EventBus::default(),
            full_auto_mode: false,
            agy_effort: "medium".to_string(),
            active_model: "test".to_string(),
            agy_model: None,
        }
    }

    #[test]
    fn resolve_workspace_path_blocks_traversal_and_outside_paths() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("secret.txt");

        assert!(resolve_workspace_path(dir.path(), "../escape.txt").is_err());
        assert!(resolve_workspace_path(dir.path(), "sub/../../escape.txt").is_err());
        assert!(resolve_workspace_path(dir.path(), &outside_file.to_string_lossy()).is_err());
        assert!(resolve_workspace_path(dir.path(), "nested/new_file.txt").is_ok());
    }

    #[tokio::test]
    async fn fs_tools_write_read_edit_glob_grep() {
        let dir = TempDir::new().unwrap();
        let ctx = make_ctx(&dir);

        let write_res = FsWriteTool
            .execute(
                serde_json::json!({"path": "src/hello.rs", "content": "fn main() {\n    println!(\"hi\");\n}\n"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(write_res.success);

        let edit_res = FsEditTool
            .execute(
                serde_json::json!({"path": "src/hello.rs", "old_text": "hi", "new_text": "jarvis"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(edit_res.success);

        let read_res = FsReadTool
            .execute(
                serde_json::json!({"path": "src/hello.rs", "start_line": 2, "end_line": 2}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(read_res.output.contains("2:     println!(\"jarvis\");"));

        let glob_res = FsGlobTool
            .execute(serde_json::json!({"pattern": "**/*.rs"}), &ctx)
            .await
            .unwrap();
        assert!(glob_res.output.contains("src/hello.rs"));

        let grep_res = FsGrepTool
            .execute(serde_json::json!({"pattern": "jarvis"}), &ctx)
            .await
            .unwrap();
        assert!(grep_res.output.contains("src/hello.rs:2:"));
    }
}
