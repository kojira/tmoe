//! 個別ツールの実装。

use crate::permission::Permission;
use crate::tool::{Tool, ToolError, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use tokio::process::Command;

pub(crate) fn join_within(root: &Path, rel: &str) -> Result<PathBuf, ToolError> {
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

pub(crate) fn default_skip_dirs() -> Vec<String> {
    vec![
        "target".into(),
        "node_modules".into(),
        ".git".into(),
        "dist".into(),
        "build".into(),
        "__pycache__".into(),
        ".venv".into(),
        "venv".into(),
    ]
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

// --- patch_file (Aider 風 search/replace) ----------------------------

/// 既存ファイルに対する **位置指定の部分編集**。
/// `edit_file` のような全文上書きではなく、正確な search 文字列を replace に置換する。
/// LLM のトークン消費を抑え、長いファイルへの介入を最小化するための主力ツール。
pub struct PatchFileTool {
    pub root: PathBuf,
}

#[derive(Deserialize)]
struct PatchArgs {
    path: String,
    /// 置換対象の文字列。空は不可。改行を含めて構わない。
    search: String,
    /// 置換後の文字列。
    replace: String,
    /// true なら全マッチを置換。false (既定) は **唯一のマッチ** を要求し、
    /// 0 件 / 2 件以上ならエラー (= 意図せざる別箇所への置換を防ぐ)。
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for PatchFileTool {
    fn name(&self) -> &str { "patch_file" }
    fn requires(&self) -> Permission { Permission::Write }

    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: PatchArgs = serde_json::from_value(args.clone())?;
        if a.search.is_empty() {
            return Err(ToolError::Args("patch_file: search must not be empty".into()));
        }
        let path = join_within(&self.root, &a.path)?;
        if !path.exists() {
            return Err(ToolError::Args(format!(
                "patch_file: file does not exist: {} (use edit_file to create)",
                a.path
            )));
        }
        let original = tokio::fs::read_to_string(&path).await?;
        let occurrences = original.matches(&a.search).count();
        if occurrences == 0 {
            // actionable な error: search の最初の 1 行をヒントにファイルから候補行を抽出する。
            // LLM が次ターンで正しい search を組み立てるための素材を返す。
            let hint_seed = a
                .search
                .lines()
                .next()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty());
            let mut candidates: Vec<(usize, &str)> = Vec::new();
            if let Some(seed) = hint_seed {
                let key = if seed.len() > 8 {
                    // search の冒頭 8 文字以上は安定した手がかりになる
                    &seed[..8.min(seed.len())]
                } else {
                    seed
                };
                for (i, line) in original.lines().enumerate() {
                    if line.contains(key) {
                        candidates.push((i + 1, line));
                        if candidates.len() >= 5 {
                            break;
                        }
                    }
                }
            }
            let hint = if candidates.is_empty() {
                String::from("(no similar lines found; try a shorter or single-token search)")
            } else {
                let lines: Vec<String> = candidates
                    .iter()
                    .map(|(n, l)| format!("  {n}: {}", l.trim_end()))
                    .collect();
                format!("similar lines in current file:\n{}", lines.join("\n"))
            };
            return Err(ToolError::Args(format!(
                "patch_file: search not found in {} ({} byte search string).\n\
                 Tip: prefer a short bare-token search (e.g. \"old_name\") with replace_all=true; \
                 multi-line spans often mismatch on whitespace.\n{hint}",
                a.path,
                a.search.len(),
            )));
        }
        if !a.replace_all && occurrences > 1 {
            return Err(ToolError::Args(format!(
                "patch_file: search matched {occurrences} times in {} but replace_all=false; \
                 expand the search to be unique or pass replace_all=true",
                a.path
            )));
        }
        let updated = if a.replace_all {
            original.replace(&a.search, &a.replace)
        } else {
            original.replacen(&a.search, &a.replace, 1)
        };
        tokio::fs::write(&path, updated.as_bytes()).await?;
        Ok(ToolOutput::text(format!(
            "patched {} ({} replacement{})",
            a.path,
            occurrences,
            if occurrences == 1 { "" } else { "s" }
        )))
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
    async fn patch_file_unique_search_replaces_in_place() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "fn old_name() { 1 + 1 }\n").unwrap();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(PatchFileTool { root: root.clone() }));
        let out = reg
            .invoke(
                &ToolCall {
                    name: "patch_file".into(),
                    args: serde_json::json!({
                        "path": "a.rs",
                        "search": "fn old_name()",
                        "replace": "fn new_name()",
                    }),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(out.stdout.contains("1 replacement"));
        let body = std::fs::read_to_string(root.join("a.rs")).unwrap();
        assert!(body.contains("fn new_name()"));
        assert!(!body.contains("fn old_name()"));
    }

    #[tokio::test]
    async fn patch_file_rejects_ambiguous_match() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "todo\ntodo\nthird line\n").unwrap();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(PatchFileTool { root: root.clone() }));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "patch_file".into(),
                    args: serde_json::json!({
                        "path": "a.rs",
                        "search": "todo",
                        "replace": "done",
                    }),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Args(msg) => assert!(msg.contains("matched 2 times")),
            other => panic!("expected Args error, got {other:?}"),
        }
        // 元のファイルは変更されていないこと
        let body = std::fs::read_to_string(root.join("a.rs")).unwrap();
        assert_eq!(body, "todo\ntodo\nthird line\n");
    }

    #[tokio::test]
    async fn patch_file_replace_all_overrides_uniqueness() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "old\nold\nkeep\n").unwrap();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(PatchFileTool { root: root.clone() }));
        let out = reg
            .invoke(
                &ToolCall {
                    name: "patch_file".into(),
                    args: serde_json::json!({
                        "path": "a.rs",
                        "search": "old",
                        "replace": "new",
                        "replace_all": true,
                    }),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(out.stdout.contains("2 replacements"));
        let body = std::fs::read_to_string(root.join("a.rs")).unwrap();
        assert_eq!(body, "new\nnew\nkeep\n");
    }

    #[tokio::test]
    async fn patch_file_search_not_found_is_args_error() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "abc\n").unwrap();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(PatchFileTool { root: root.clone() }));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "patch_file".into(),
                    args: serde_json::json!({
                        "path": "a.rs",
                        "search": "xyz",
                        "replace": "qwerty",
                    }),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Args(_)));
    }

    #[tokio::test]
    async fn patch_file_empty_search_rejected() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "abc\n").unwrap();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(PatchFileTool { root: root.clone() }));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "patch_file".into(),
                    args: serde_json::json!({
                        "path": "a.rs",
                        "search": "",
                        "replace": "x",
                    }),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Args(_)));
    }

    #[tokio::test]
    async fn patch_file_missing_file_is_args_error() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(PatchFileTool { root: root.clone() }));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "patch_file".into(),
                    args: serde_json::json!({
                        "path": "nope.rs",
                        "search": "x",
                        "replace": "y",
                    }),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Args(msg) => assert!(msg.contains("does not exist")),
            other => panic!("expected Args error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn patch_file_supervisor_cannot_call() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "x\n").unwrap();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(PatchFileTool { root: root.clone() }));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "patch_file".into(),
                    args: serde_json::json!({
                        "path": "a.rs",
                        "search": "x",
                        "replace": "y",
                    }),
                },
                &PermissionProfile::supervisor(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Permission { .. }));
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
