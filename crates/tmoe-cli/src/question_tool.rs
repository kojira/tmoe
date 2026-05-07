//! `question` ツール: Worker が user に確認を取れる経路。
//!
//! opencode の question tool に類似:
//!   {"tool":"question","args":{"questions":[
//!     {"question":"...","header":"...","options":["a","b","c"],"multiple":false}
//!   ]}}
//!
//! ただし tmoe では **Tool 自体に asker を注入** する形をとる (runtime や TUI 側の
//! 実装で差し替えられるように)。
//! - TUI モードでは `ChannelAsker` が Concierge ペインに question を流し、user の Enter
//!   入力を answer として返す。
//! - Headless モードでは `HeadlessAsker` が即エラー (= 「対話が無いので答えられない」)。
//! - 単体テストでは `ScriptedAsker` で決定論的に回答列を流す。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tmoe_tools::{Permission, Tool, ToolError, ToolOutput, ToolResult};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuestionPrompt {
    pub question: String,
    /// 短いラベル (TUI ペインのタイトル等)。`question` と同じでもよい。
    #[serde(default)]
    pub header: Option<String>,
    /// 選択肢。空なら自由記述として扱う。
    #[serde(default)]
    pub options: Vec<String>,
    /// 複数選択を許すか。
    #[serde(default)]
    pub multiple: bool,
}

/// 1 question への user 回答。`options` 経由なら option ラベル文字列の配列、
/// 自由記述なら 1 要素の文字列配列。空配列は「未回答」(タイムアウトや拒否) を表す。
pub type Answer = Vec<String>;

#[async_trait]
pub trait QuestionAsker: Send + Sync {
    /// 1 個以上の質問を提示し、各 question への answer を順に返す。
    /// 失敗時は `Err` (= ツールが `ToolError::Args` で Worker に伝播)。
    async fn ask(&self, prompts: &[QuestionPrompt]) -> Result<Vec<Answer>, String>;
}

/// `tmoe-cli/--headless` では対話できないので、即座にエラーを返す。
pub struct HeadlessAsker;

#[async_trait]
impl QuestionAsker for HeadlessAsker {
    async fn ask(&self, _prompts: &[QuestionPrompt]) -> Result<Vec<Answer>, String> {
        Err(
            "questions are not answerable in --headless mode; rerun without --headless or \
             provide the missing context in the task prompt"
                .into(),
        )
    }
}

/// テスト用: あらかじめ詰めた回答配列を順に返す。
pub struct ScriptedAsker {
    pub answers: std::sync::Mutex<Vec<Vec<String>>>,
}

impl ScriptedAsker {
    pub fn new(answers: Vec<Vec<String>>) -> Self {
        Self {
            answers: std::sync::Mutex::new(answers),
        }
    }
}

#[async_trait]
impl QuestionAsker for ScriptedAsker {
    async fn ask(&self, prompts: &[QuestionPrompt]) -> Result<Vec<Answer>, String> {
        let mut g = self.answers.lock().unwrap();
        let mut out = Vec::with_capacity(prompts.len());
        for _ in 0..prompts.len() {
            if g.is_empty() {
                return Err("ScriptedAsker exhausted".into());
            }
            out.push(g.remove(0));
        }
        Ok(out)
    }
}

/// TUI / runtime と接続するための tokio チャネルベース asker。
/// Tool::call から `tx` で `(prompts, reply_tx)` を送り、reply_tx の値を待つ。
pub struct ChannelAsker {
    pub tx: tokio::sync::mpsc::Sender<(Vec<QuestionPrompt>, tokio::sync::oneshot::Sender<Vec<Answer>>)>,
}

impl ChannelAsker {
    pub fn new(
        tx: tokio::sync::mpsc::Sender<(Vec<QuestionPrompt>, tokio::sync::oneshot::Sender<Vec<Answer>>)>,
    ) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl QuestionAsker for ChannelAsker {
    async fn ask(&self, prompts: &[QuestionPrompt]) -> Result<Vec<Answer>, String> {
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        self.tx
            .send((prompts.to_vec(), rtx))
            .await
            .map_err(|e| format!("question channel send failed: {e}"))?;
        rrx.await
            .map_err(|e| format!("question reply channel closed: {e}"))
    }
}

pub struct QuestionTool {
    pub asker: Arc<dyn QuestionAsker>,
}

impl QuestionTool {
    pub fn new(asker: Arc<dyn QuestionAsker>) -> Self {
        Self { asker }
    }
}

#[derive(Deserialize)]
struct ToolArgs {
    questions: Vec<QuestionPrompt>,
}

#[async_trait]
impl Tool for QuestionTool {
    fn name(&self) -> &str {
        "question"
    }
    fn requires(&self) -> Permission {
        Permission::Read
    }
    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: ToolArgs = serde_json::from_value(args.clone())
            .map_err(|e| ToolError::Args(format!("question args: {e}")))?;
        if a.questions.is_empty() {
            return Err(ToolError::Args("at least one question is required".into()));
        }
        let answers = self
            .asker
            .ask(&a.questions)
            .await
            .map_err(ToolError::Args)?;
        let body = a
            .questions
            .iter()
            .zip(answers.iter())
            .map(|(q, ans)| {
                let joined = if ans.is_empty() {
                    "(no answer)".to_string()
                } else {
                    ans.join(", ")
                };
                format!("Q: {} -> A: {}", q.question, joined)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::text(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn question_tool_returns_scripted_answer_per_question() {
        let asker = Arc::new(ScriptedAsker::new(vec![
            vec!["Yes".into()],
            vec!["b".into(), "c".into()],
        ]));
        let tool = QuestionTool::new(asker);
        let out = tool
            .call(&serde_json::json!({
                "questions": [
                    {"question": "Proceed?", "options": ["Yes", "No"]},
                    {"question": "Pick all that apply", "options": ["a","b","c"], "multiple": true}
                ]
            }))
            .await
            .unwrap();
        assert!(out.stdout.contains("Q: Proceed? -> A: Yes"), "got: {}", out.stdout);
        assert!(
            out.stdout.contains("Q: Pick all that apply -> A: b, c"),
            "got: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn question_tool_rejects_empty_questions() {
        let asker = Arc::new(ScriptedAsker::new(vec![]));
        let tool = QuestionTool::new(asker);
        let err = tool
            .call(&serde_json::json!({"questions": []}))
            .await
            .unwrap_err();
        match err {
            ToolError::Args(_) => {}
            other => panic!("expected Args, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn headless_asker_always_errors_with_helpful_hint() {
        let tool = QuestionTool::new(Arc::new(HeadlessAsker));
        let err = tool
            .call(&serde_json::json!({"questions":[{"question":"x","options":["a"]}]}))
            .await
            .unwrap_err();
        match err {
            ToolError::Args(m) => assert!(m.contains("--headless")),
            other => panic!("expected Args, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn channel_asker_round_trip() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let asker = ChannelAsker::new(tx);

        // 別タスクで Tool::call 相当を起動。
        let fut = tokio::spawn(async move {
            let tool = QuestionTool::new(Arc::new(asker));
            tool.call(&serde_json::json!({
                "questions":[{"question":"A?","options":["yes","no"]}]
            }))
            .await
        });

        // user 側: prompts を受け取って oneshot に reply する。
        let (prompts, reply_tx) = rx.recv().await.expect("prompts not received");
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].question, "A?");
        reply_tx.send(vec![vec!["yes".into()]]).unwrap();

        let out = fut.await.unwrap().unwrap();
        assert!(out.stdout.contains("yes"), "got: {}", out.stdout);
    }
}
