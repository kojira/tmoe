//! Plan モード: opencode の plan_enter / plan_exit を tmoe の Trio に持ち込む。
//!
//! tmoe のアーキテクチャは「Trio (Worker/Supervisor/Observer) は単一プロセス」なので、
//! opencode のように "plan agent → build agent に切り替え" という分離はしない。代わりに
//! plan_enter / plan_exit をツールとして提供し、Worker が以下の流れで使う:
//!
//!   1. Worker が `plan_enter` を呼ぶ →
//!      `<workdir>/.tmoe/plans/<feature_id>.md` に **計画 markdown を保存** する
//!      (空でも可、上書き)。Supervisor / Observer は `search_history` でこのファイルを後で
//!      参照できる
//!   2. Worker が `plan_exit` を呼ぶ →
//!      QuestionAsker (= TUI ChannelAsker / headless ScriptedAsker) でユーザに
//!      「この計画で進めるか?」を問う。Yes なら ToolOutput に "approved"、No なら
//!      "rejected" を返し、Worker が plan を書き直す
//!
//! Concierge は 4 人目のエージェントではないので、plan モードを「別エージェントに切替」
//! として扱わず、**ツール 2 つで状態遷移を表現** するだけにとどめる。これで合意平面の
//! 頂点数 (= 3) は変わらない。

use crate::question_tool::{QuestionAsker, QuestionPrompt};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tmoe_tools::{Permission, Tool, ToolError, ToolOutput, ToolResult};

/// 計画ファイルの保存先を決める。`<workdir>/.tmoe/plans/<feature_id>.md` に固定する。
/// `<feature_id>` は ULID なので衝突しない。
pub fn plan_path(workdir: &Path, feature_id: &str) -> PathBuf {
    workdir.join(".tmoe").join("plans").join(format!("{feature_id}.md"))
}

#[derive(Deserialize)]
struct PlanEnterArgs {
    /// markdown 本文。ヘッダ・節・チェックリストなど自由形式。
    plan: String,
    /// 任意のタイトル。先頭に `# {title}` で前置される。省略時は無し。
    #[serde(default)]
    title: Option<String>,
}

pub struct PlanEnterTool {
    pub workdir: PathBuf,
    pub feature_id: String,
}

#[async_trait]
impl Tool for PlanEnterTool {
    fn name(&self) -> &str {
        "plan_enter"
    }
    fn requires(&self) -> Permission {
        Permission::Write
    }
    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: PlanEnterArgs = serde_json::from_value(args.clone())
            .map_err(|e| ToolError::Args(format!("plan_enter args: {e}")))?;
        let path = plan_path(&self.workdir, &self.feature_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        if let Some(t) = a.title {
            body.push_str(&format!("# {}\n\n", t.trim()));
        }
        body.push_str(a.plan.trim());
        body.push('\n');
        std::fs::write(&path, &body)?;
        let out = format!(
            "plan written to {}\n\n--- plan ---\n{}",
            path.display(),
            body.trim()
        );
        Ok(ToolOutput::text(out))
    }
}

pub struct PlanExitTool {
    pub workdir: PathBuf,
    pub feature_id: String,
    pub asker: Arc<dyn QuestionAsker>,
}

#[async_trait]
impl Tool for PlanExitTool {
    fn name(&self) -> &str {
        "plan_exit"
    }
    fn requires(&self) -> Permission {
        Permission::Read
    }
    async fn call(&self, _args: &serde_json::Value) -> ToolResult {
        let path = plan_path(&self.workdir, &self.feature_id);
        if !path.exists() {
            return Err(ToolError::Args(format!(
                "plan_exit: no plan file at {}. Call plan_enter first.",
                path.display()
            )));
        }
        let plan_body = std::fs::read_to_string(&path)?;
        let q = QuestionPrompt {
            question: format!(
                "Plan at {} is ready. Approve and proceed to implementation?",
                path.display()
            ),
            header: Some("plan_exit".into()),
            options: vec!["yes".into(), "no".into()],
            multiple: false,
        };
        let answers = self
            .asker
            .ask(&[q])
            .await
            .map_err(|e| ToolError::Args(format!("plan_exit ask failed: {e}")))?;
        let first = answers
            .into_iter()
            .next()
            .and_then(|a| a.into_iter().next())
            .unwrap_or_default()
            .to_lowercase();
        let approved = first == "yes";
        let out = if approved {
            format!(
                "approved. plan_path: {}\n\n--- plan ---\n{}\n\nProceed to implement.",
                path.display(),
                plan_body.trim()
            )
        } else {
            format!(
                "rejected. The plan needs more work. plan_path: {}\nKeep refining via plan_enter.",
                path.display()
            )
        };
        Ok(ToolOutput::text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question_tool::ScriptedAsker;
    use tempfile::tempdir;

    #[tokio::test]
    async fn plan_enter_writes_markdown_file() {
        let d = tempdir().unwrap();
        let tool = PlanEnterTool {
            workdir: d.path().to_path_buf(),
            feature_id: "FEAT01".into(),
        };
        let out = tool
            .call(&serde_json::json!({
                "plan": "1. step a\n2. step b",
                "title": "Refactor x"
            }))
            .await
            .unwrap();
        assert!(out.stdout.contains("FEAT01.md"));
        let body = std::fs::read_to_string(d.path().join(".tmoe/plans/FEAT01.md")).unwrap();
        assert!(body.starts_with("# Refactor x\n"));
        assert!(body.contains("1. step a"));
        assert!(body.contains("2. step b"));
    }

    #[tokio::test]
    async fn plan_exit_returns_approved_when_user_says_yes() {
        let d = tempdir().unwrap();
        let asker = Arc::new(ScriptedAsker::new(vec![vec!["yes".into()]]));
        // 事前に plan_enter で計画を作っておく。
        let enter = PlanEnterTool {
            workdir: d.path().to_path_buf(),
            feature_id: "F02".into(),
        };
        enter
            .call(&serde_json::json!({"plan": "do thing"}))
            .await
            .unwrap();
        let exit = PlanExitTool {
            workdir: d.path().to_path_buf(),
            feature_id: "F02".into(),
            asker,
        };
        let out = exit.call(&serde_json::json!({})).await.unwrap();
        assert!(out.stdout.contains("approved"));
        assert!(out.stdout.contains("do thing"));
    }

    #[tokio::test]
    async fn plan_exit_returns_rejected_when_user_says_no() {
        let d = tempdir().unwrap();
        let asker = Arc::new(ScriptedAsker::new(vec![vec!["no".into()]]));
        let enter = PlanEnterTool {
            workdir: d.path().to_path_buf(),
            feature_id: "F03".into(),
        };
        enter.call(&serde_json::json!({"plan": "p"})).await.unwrap();
        let exit = PlanExitTool {
            workdir: d.path().to_path_buf(),
            feature_id: "F03".into(),
            asker,
        };
        let out = exit.call(&serde_json::json!({})).await.unwrap();
        assert!(out.stdout.contains("rejected"));
        assert!(out.stdout.contains("Keep refining"));
    }

    #[tokio::test]
    async fn plan_exit_rejects_when_no_plan_file() {
        let d = tempdir().unwrap();
        let asker = Arc::new(ScriptedAsker::new(vec![vec!["yes".into()]]));
        let exit = PlanExitTool {
            workdir: d.path().to_path_buf(),
            feature_id: "F04".into(),
            asker,
        };
        let err = exit.call(&serde_json::json!({})).await.unwrap_err();
        match err {
            ToolError::Args(m) => assert!(m.contains("no plan file"), "{m}"),
            other => panic!("expected Args, got {other:?}"),
        }
    }
}
