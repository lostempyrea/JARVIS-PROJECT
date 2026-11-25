//! Persistent memory tools (`memory_store`, `memory_recall`) backed by SQLite FTS5.

use crate::tools::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;

pub struct MemoryStoreTool;

#[derive(Deserialize)]
struct MemoryStoreArgs {
    key: String,
    content: String,
    #[serde(default)]
    tags: String,
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Store or update a persistent memory entry indexed for full-text recall."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Unique identifier for this memory" },
                "content": { "type": "string", "description": "Fact, preference, or note to remember" },
                "tags": { "type": "string", "description": "Optional comma-separated tags" }
            },
            "required": ["key", "content"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: MemoryStoreArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        if parsed.key.trim().is_empty() {
            return Err(ToolError::InvalidArgs("key must not be empty".to_string()));
        }
        let rec = ctx
            .db
            .upsert_memory(parsed.key.trim(), &parsed.content, &parsed.tags, "agent")?;
        Ok(ToolResult::ok(format!("Stored memory '{}' (id={})", rec.key, rec.id)))
    }
}

pub struct MemoryRecallTool;

#[derive(Deserialize)]
struct MemoryRecallArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Search persistent memories using SQLite FTS5 full-text search."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search keywords (empty returns most recent)" },
                "limit": { "type": "integer", "description": "Maximum results (default 10)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: MemoryRecallArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let records = ctx.db.search_memories(&parsed.query, parsed.limit.clamp(1, 50))?;
        if records.is_empty() {
            return Ok(ToolResult::ok("(no matching memories)"));
        }
        let formatted = records
            .into_iter()
            .map(|r| format!("- [{}] (tags: {}): {}", r.key, r.tags, r.content))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult::ok(formatted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::events::EventBus;
    use tempfile::TempDir;

    #[tokio::test]
    async fn memory_store_and_recall_round_trip() {
        let dir = TempDir::new().unwrap();
        let ctx = ToolContext {
            session_id: "s1".to_string(),
            workspace_root: dir.path().to_path_buf(),
            db: Database::open_in_memory().unwrap(),
            event_bus: EventBus::default(),
            full_auto_mode: false,
            agy_effort: "medium".to_string(),
            active_model: "test".to_string(),
            agy_model: None,
        };

        let res = MemoryStoreTool
            .execute(
                serde_json::json!({
                    "key": "tts_voice",
                    "content": "Preferred voice is vi-VN-HoaiMyNeural",
                    "tags": "voice,vietnamese"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.success);

        let recall = MemoryRecallTool
            .execute(serde_json::json!({"query": "HoaiMyNeural"}), &ctx)
            .await
            .unwrap();
        assert!(recall.success);
        assert!(recall.output.contains("tts_voice"));
    }
}
