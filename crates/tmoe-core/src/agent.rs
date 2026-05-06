//! 単体エージェントの行動ループ (Phase 3)。
//!
//! LLM が生成した Worker 提案を `Proposal` に解釈し、ツール呼び出しがあれば実行する。
//! Phase 4 でこのループは Trio オーケストレータに組み込まれる。

use crate::proposal::Proposal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tmoe_llm::{ChatMessage, ChatRequest, LlmClient};
use tmoe_tools::{PermissionProfile, ToolCall, ToolError, ToolRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    Worker,
    Supervisor,
    Observer,
}

impl AgentRole {
    pub fn permission_profile(self) -> PermissionProfile {
        match self {
            AgentRole::Worker => PermissionProfile::worker(),
            AgentRole::Supervisor => PermissionProfile::supervisor(),
            AgentRole::Observer => PermissionProfile::observer(),
        }
    }
}

/// LLM 出力 1 件分から抽出された (中間表現) ツール呼び出し。
#[derive(Debug, Clone)]
pub struct ParsedToolCall(pub ToolCall);

/// LLM の生テキストから `Proposal` を抽出する。
///
/// 抽出規則:
/// - `DONE` を 1 行で含めば `done = true`
/// - JSON を ```json ... ``` または独立した object として認識し、`{"tool":"name","args":{...}}` の
///   形ならツール呼び出しとして取り込む。複数あれば順序保持で取り込む
/// - その他テキストは `note` に蓄積
pub fn parse_proposal(text: &str) -> Proposal {
    let mut tool_calls = Vec::new();
    let mut note_lines: Vec<String> = Vec::new();
    let mut done = false;

    // 簡易フェンス対応: ```json ... ``` を取り出す。
    let mut chunks: Vec<String> = Vec::new();
    let mut buf = text;
    while let Some(start) = buf.find("```") {
        let after = &buf[start + 3..];
        let lang_end = after.find(['\n', '\r']).unwrap_or(after.len());
        let _lang = &after[..lang_end];
        let rest = &after[lang_end..];
        if let Some(end) = rest.find("```") {
            chunks.push(rest[..end].trim().to_string());
            buf = &rest[end + 3..];
        } else {
            // 閉じフェンスなし → 残りすべてを 1 チャンクとして取り込む。
            chunks.push(rest.trim().to_string());
            buf = "";
        }
    }
    // フェンスのテキストと残テキストの双方からツール呼び出しを抽出する。
    for chunk in chunks {
        if let Some(call) = try_parse_tool_call(&chunk) {
            tool_calls.push(call);
        }
    }

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "DONE" {
            done = true;
            continue;
        }
        if trimmed.starts_with("```") {
            continue;
        }
        if let Some(call) = try_parse_tool_call(trimmed) {
            // インラインで {"tool":...} だけが書かれた行も拾う。
            if !tool_calls.contains(&call) {
                tool_calls.push(call);
            }
            continue;
        }
        note_lines.push(line.to_string());
    }

    Proposal {
        raw_text: text.to_string(),
        tool_calls,
        done,
        note: note_lines.join("\n").trim().to_string(),
    }
}

fn try_parse_tool_call(text: &str) -> Option<ToolCall> {
    // 必要最小限: "tool" と "args" を含む JSON object であること。
    if !(text.contains("\"tool\"") && text.contains("\"args\"")) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let name = v.get("tool")?.as_str()?.to_string();
    let args = v.get("args")?.clone();
    Some(ToolCall { name, args })
}

#[derive(Debug)]
pub struct ProposalMessage {
    pub proposal: Proposal,
    pub tool_outputs: Vec<Result<tmoe_tools::ToolOutput, ToolError>>,
}

/// 単体エージェントを 1 ステップだけ回す: LLM へ問い、Proposal を抽出し、Worker 役割なら
/// ツールを実行する。Phase 4 ではこの 1 ステップが Trio の `worker.act` に相当する。
pub async fn single_agent_loop(
    role: AgentRole,
    system: &str,
    user_messages: Vec<ChatMessage>,
    llm: &dyn LlmClient,
    tools: &ToolRegistry,
) -> anyhow::Result<ProposalMessage> {
    let mut messages = Vec::with_capacity(user_messages.len() + 1);
    messages.push(ChatMessage::system(system));
    messages.extend(user_messages);
    let resp = llm.chat(ChatRequest { messages, ..Default::default() }).await?;
    let proposal = parse_proposal(&resp.content);
    let profile = role.permission_profile();
    let mut tool_outputs = Vec::with_capacity(proposal.tool_calls.len());
    if matches!(role, AgentRole::Worker) {
        for call in &proposal.tool_calls {
            let r = tools.invoke(call, &profile).await;
            tool_outputs.push(r);
        }
    }
    let _ = Arc::new(()); // 静的解析: Arc を依存に残しておく (将来の同時呼び出し対応)
    Ok(ProposalMessage { proposal, tool_outputs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tmoe_llm::{MockLlmClient, ScriptedTurn};
    use tmoe_tools::{EditFileTool, ReadFileTool};

    fn make_registry(root: std::path::PathBuf) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EditFileTool { root: root.clone() }));
        reg.register(Arc::new(ReadFileTool { root }));
        reg
    }

    #[test]
    fn parse_extracts_tool_call_in_fence() {
        let txt = "ok\n```json\n{\"tool\":\"edit_file\",\"args\":{\"path\":\"a.rs\",\"content\":\"fn main(){}\"}}\n```\n進めます\n";
        let p = parse_proposal(txt);
        assert_eq!(p.tool_calls.len(), 1);
        assert_eq!(p.tool_calls[0].name, "edit_file");
        assert!(!p.done);
    }

    #[test]
    fn parse_detects_done_marker() {
        let p = parse_proposal("作業完了\nDONE\n");
        assert!(p.done);
        assert!(p.tool_calls.is_empty());
    }

    #[test]
    fn parse_inline_tool_json_line() {
        let p = parse_proposal("{\"tool\":\"read_file\",\"args\":{\"path\":\"x.rs\"}}\n");
        assert_eq!(p.tool_calls.len(), 1);
        assert_eq!(p.tool_calls[0].name, "read_file");
    }

    #[tokio::test]
    async fn worker_executes_extracted_tool() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let reg = make_registry(root.clone());
        let llm = MockLlmClient::new("worker");
        llm.push(ScriptedTurn::new(
            "提案します\n```json\n{\"tool\":\"edit_file\",\"args\":{\"path\":\"hello.rs\",\"content\":\"fn main(){println!(\\\"hi\\\");}\"}}\n```\nDONE\n",
        ));
        let out = single_agent_loop(
            AgentRole::Worker,
            "system",
            vec![ChatMessage::user("hello.rs を作って")],
            &llm,
            &reg,
        )
        .await
        .unwrap();
        assert!(out.proposal.done);
        assert_eq!(out.tool_outputs.len(), 1);
        assert!(out.tool_outputs[0].is_ok());
        let written = std::fs::read_to_string(root.join("hello.rs")).unwrap();
        assert!(written.contains("println!"));
    }

    #[tokio::test]
    async fn supervisor_does_not_execute_tools_even_if_extracted() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let reg = make_registry(root.clone());
        let llm = MockLlmClient::new("supervisor");
        llm.push(ScriptedTurn::new(
            "{\"tool\":\"edit_file\",\"args\":{\"path\":\"x.rs\",\"content\":\"!!\"}}\n",
        ));
        let out = single_agent_loop(
            AgentRole::Supervisor,
            "system",
            vec![ChatMessage::user("review")],
            &llm,
            &reg,
        )
        .await
        .unwrap();
        // 抽出はされるが Supervisor は呼ばない。
        assert_eq!(out.proposal.tool_calls.len(), 1);
        assert_eq!(out.tool_outputs.len(), 0);
        assert!(!root.join("x.rs").exists());
    }
}
