//! Tool trait, schema registry, execution context, and dispatcher.

pub mod agy_tool;
pub mod fs_tools;
pub mod memory_tool;
pub mod powershell_console;
pub mod pty_manager;
pub mod pty_tool;
pub mod terminal;
pub mod vt_emulator;

use crate::db::Database;
use crate::events::EventBus;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

use crate::providers::traits::ToolDefinition;
use agy_tool::AgyDelegateTool;
use fs_tools::{FsEditTool, FsGlobTool, FsGrepTool, FsReadTool, FsWriteTool};
use memory_tool::{MemoryRecallTool, MemoryStoreTool};
use terminal::TerminalTool;

/// Errors produced during tool validation or execution.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid tool arguments: {0}")]
    InvalidArgs(String),
    #[error("path '{path}' escapes workspace root '{workspace}'")]
    OutsideWorkspace { path: String, workspace: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Db(#[from] crate::db::DbError),
    #[error("execution error: {0}")]
    Execution(String),
}

/// Runtime context supplied to every tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub workspace_root: PathBuf,
    pub db: Database,
    pub event_bus: EventBus,
    pub full_auto_mode: bool,
    pub agy_effort: String,
    pub active_model: String,
    pub agy_model: Option<String>,
}

/// Structured output returned by a [`Tool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
        }
    }

    pub fn err(output: impl Into<String>) -> Self {
        Self {
            success: false,
            output: output.into(),
        }
    }
}

/// Asynchronous tool implemented by built-in JARVIS capabilities.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError>;
}

/// Registry holding built-in and dynamically registered tools.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty [`ToolRegistry`].
    pub fn empty() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Create a [`ToolRegistry`] populated with all Phase 1 built-in tools.
    pub fn with_builtins() -> Self {
        let mut reg = Self::empty();
        reg.register(Arc::new(TerminalTool::default()));
        reg.register(Arc::new(FsReadTool));
        reg.register(Arc::new(FsWriteTool));
        reg.register(Arc::new(FsEditTool));
        reg.register(Arc::new(FsGlobTool));
        reg.register(Arc::new(FsGrepTool));
        reg.register(Arc::new(MemoryStoreTool));
        reg.register(Arc::new(MemoryRecallTool));
        reg.register(Arc::new(AgyDelegateTool::new()));
        reg
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Return all registered tool definitions sorted deterministically by name.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> =
            self.tools.values().map(|t| t.definition()).collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Execute a named tool with JSON arguments.
    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?;
        tool.execute(args, ctx).await
    }
}
