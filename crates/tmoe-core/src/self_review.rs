//! 自己レビュー: Worker が commit する前に Supervisor が diff を読んで approve/reject を返す。
//!
//! 「平面合意は既に取れているが、実体ファイル変更が要件と矛盾していないか」を確認する
//! 最終ガード。Supervisor の拒否権が紙の上の合意を実体の commit から守る。

use crate::vote::Vote;
use tmoe_llm::{ChatMessage, ChatRequest, LlmClient};

#[derive(Debug, Clone, PartialEq)]
pub enum SelfReviewOutcome {
    Approved(Vote),
    Rejected(Vote),
}

pub async fn supervisor_review_diff(
    supervisor_llm: &dyn LlmClient,
    supervisor_prompt: &str,
    diff_text: &str,
    intent: &str,
) -> anyhow::Result<SelfReviewOutcome> {
    let mut messages = vec![ChatMessage::system(supervisor_prompt)];
    messages.push(ChatMessage::user(format!(
        "Worker は以下の意図で実装を行いました:\n{intent}\n\n結果の git diff:\n{diff_text}\n\n\
         この diff を commit すべきかを JSON で返してください: {{\"approve\": bool, \"confidence\": 0.0-1.0, \"note\": \"...\"}}"
    )));
    let resp = supervisor_llm
        .chat(ChatRequest {
            messages,
            max_tokens: Some(256),
            temperature: Some(0.0),
            ..Default::default()
        })
        .await?;
    let vote = parse_vote(&resp.content)
        .ok_or_else(|| anyhow::anyhow!("vote unparseable: {}", resp.content))?;
    Ok(if vote.approve {
        SelfReviewOutcome::Approved(vote)
    } else {
        SelfReviewOutcome::Rejected(vote)
    })
}

fn parse_vote(text: &str) -> Option<Vote> {
    crate::trio::parse_vote(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmoe_llm::{MockLlmClient, ScriptedTurn};

    #[tokio::test]
    async fn approves_clean_diff() {
        let llm = MockLlmClient::new("sup");
        llm.push(ScriptedTurn::new(
            r#"{"approve":true,"confidence":0.9,"note":"diff is small and on-target"}"#,
        ));
        let r = supervisor_review_diff(&llm, "sup", "+ fn ok() {}", "add ok()").await.unwrap();
        assert!(matches!(r, SelfReviewOutcome::Approved(_)));
    }

    #[tokio::test]
    async fn rejects_off_target_diff() {
        let llm = MockLlmClient::new("sup");
        llm.push(ScriptedTurn::new(
            r#"{"approve":false,"confidence":0.95,"note":"diff touches unrelated module"}"#,
        ));
        let r = supervisor_review_diff(&llm, "sup", "+ wholesale rewrite", "tiny rename").await.unwrap();
        assert!(matches!(r, SelfReviewOutcome::Rejected(_)));
    }
}
