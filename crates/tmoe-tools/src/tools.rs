//! 個別ツールの実装。

use crate::permission::Permission;
use crate::tool::{Tool, ToolError, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use tokio::process::Command;

fn join_within(root: &Path, rel: &str) -> Result<PathBuf, ToolError> {
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        return Err(ToolError::Args(format!("path must be relative: {rel}")));
    }
    for c in candidate.components() {
        if matches!(c, Component::ParentDir) {
            return Err(ToolError::Args(format!("'..' not allowed in path: {rel}")));
        }
    }
    Ok(root.join(candidate))
}

// --- read_file ---------------------------------------------------------

pub struct ReadFileTool {
    pub root: PathBuf,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str { "read_file" }
    fn requires(&self) -> Permission { Permission::Read }

    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: ReadArgs = serde_json::from_value(args.clone())?;
        let path = join_within(&self.root, &a.path)?;
        let content = tokio::fs::read_to_string(&path).await?;
        Ok(ToolOutput::text(content))
    }
}

// --- edit_file (overwrite) --------------------------------------------

pub struct EditFileTool {
    pub root: PathBuf,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str { "edit_file" }
    fn requires(&self) -> Permission { Permission::Write }

    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: EditArgs = serde_json::from_value(args.clone())?;
        let path = join_within(&self.root, &a.path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, a.content.as_bytes()).await?;
        Ok(ToolOutput::text(format!("wrote {} bytes to {}", a.content.len(), a.path)))
    }
}

// --- run_cmd ----------------------------------------------------------

pub struct RunCmdTool {
    pub root: PathBuf,
    pub blocklist: Vec<String>,
}

#[derive(Deserialize)]
struct RunArgs {
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

#[async_trait]
impl Tool for RunCmdTool {
    fn name(&self) -> &str { "run_cmd" }
    fn requires(&self) -> Permission { Permission::Run }

    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: RunArgs = serde_json::from_value(args.clone())?;
        let cmd_line = std::iter::once(a.program.as_str())
            .chain(a.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        for needle in &self.blocklist {
            if cmd_line.contains(needle) {
                return Err(ToolError::Dangerous(cmd_line));
            }
        }
        let output = Command::new(&a.program)
            .args(&a.args)
            .current_dir(&self.root)
            .output()
            .await
            .map_err(|e| ToolError::Exec(format!("spawn failed: {e}")))?;
        if !output.status.success() {
            let body = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(ToolError::Exec(format!(
                "exit {}: {}",
                output.status,
                body.trim()
            )));
        }
        let body = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(ToolOutput::text(body))
    }
}

/// tmoe の既定ブロックリスト。Worker 経由でも実行禁止。
pub fn default_blocklist() -> Vec<String> {
    vec![
        "rm -rf".into(),
        "git reset --hard".into(),
        "git clean -f".into(),
        "git push --force".into(),
        "git push -f ".into(),
        "pkill ".into(),
        "killall".into(),
        "shutdown".into(),
        "mkfs.".into(),
        ":(){:|:&};:".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionProfile;
    use crate::registry::ToolRegistry;
    use crate::tool::ToolCall;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[tokio::test]
    async fn read_then_edit_then_read_roundtrip() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ReadFileTool { root: root.clone() }));
        reg.register(Arc::new(EditFileTool { root: root.clone() }));
        let prof = PermissionProfile::worker();

        reg.invoke(
            &ToolCall {
                name: "edit_file".into(),
                args: serde_json::json!({"path": "src/hello.rs", "content": "fn main() {}\n"}),
            },
            &prof,
        )
        .await
        .unwrap();
        let out = reg
            .invoke(
                &ToolCall {
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "src/hello.rs"}),
                },
                &prof,
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, "fn main() {}\n");
    }

    #[tokio::test]
    async fn supervisor_cannot_edit() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EditFileTool { root: root.clone() }));
        let prof = PermissionProfile::supervisor();
        let err = reg
            .invoke(
                &ToolCall {
                    name: "edit_file".into(),
                    args: serde_json::json!({"path": "x.rs", "content": ""}),
                },
                &prof,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Permission { .. }));
    }

    #[tokio::test]
    async fn run_cmd_blocks_dangerous() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(RunCmdTool {
            root: root.clone(),
            blocklist: default_blocklist(),
        }));
        let prof = PermissionProfile::worker();
        let err = reg
            .invoke(
                &ToolCall {
                    name: "run_cmd".into(),
                    args: serde_json::json!({"program": "sh", "args": ["-c", "rm -rf /"]}),
                },
                &prof,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Dangerous(_)));
    }

    #[tokio::test]
    async fn run_cmd_executes_safe_program() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(RunCmdTool {
            root: root.clone(),
            blocklist: default_blocklist(),
        }));
        let prof = PermissionProfile::worker();
        let out = reg
            .invoke(
                &ToolCall {
                    name: "run_cmd".into(),
                    args: serde_json::json!({"program": "echo", "args": ["tmoe"]}),
                },
                &prof,
            )
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "tmoe");
    }

    #[tokio::test]
    async fn read_rejects_parent_traversal() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ReadFileTool { root: root.clone() }));
        let prof = PermissionProfile::worker();
        let err = reg
            .invoke(
                &ToolCall {
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "../etc/passwd"}),
                },
                &prof,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Args(_)));
    }
}
