use crate::permission::Permission;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolOutput {
    pub stdout: String,
    pub data: Option<serde_json::Value>,
}

impl ToolOutput {
    pub fn text(stdout: impl Into<String>) -> Self {
        Self { stdout: stdout.into(), data: None }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),

    #[error("permission denied for tool {tool}: requires {required:?}")]
    Permission { tool: String, required: Permission },

    #[error("argument error: {0}")]
    Args(String),

    #[error("execution failed: {0}")]
    Exec(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("blocked dangerous command: {0}")]
    Dangerous(String),
}

pub type ToolResult = std::result::Result<ToolOutput, ToolError>;

/// 単一ツールの呼び出し可能境界。
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn requires(&self) -> Permission;
    async fn call(&self, args: &serde_json::Value) -> ToolResult;
}
