use crate::permission::PermissionProfile;
use crate::tool::{Tool, ToolCall, ToolError, ToolResult};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// 権限プロファイル付きで呼び出す。許可されていなければ Permission エラー。
    pub async fn invoke(&self, call: &ToolCall, profile: &PermissionProfile) -> ToolResult {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| ToolError::NotFound(call.name.clone()))?;
        let required = tool.requires();
        if !profile.allows(required) {
            return Err(ToolError::Permission { tool: call.name.clone(), required });
        }
        tool.call(&call.args).await
    }
}
