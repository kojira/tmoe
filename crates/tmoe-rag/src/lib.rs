//! tmoe-rag: ベクトルなしエージェンティック木探索検索。
//!
//! ベクトル類似度や埋め込みは使わず、LLM がノード要約を読んで「次に開く子」を選ぶ
//! 推論ベースの検索を提供する。検索対象は `tmoe_tree::SourceNode` のツリー。

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tmoe_llm::{ChatMessage, ChatRequest, LlmClient};
use tmoe_tree::{NodeId, SourceNode};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavigateDecision {
    /// 次に開く子のリスト。空ならこの分岐は捨てる。
    pub next: Vec<NodeId>,
    /// この階層を「終端」とみなすか (=これ以上掘らない)。
    pub terminal: bool,
    /// 終端時に返す葉ノード。terminal=true のときに使われる。
    pub leaves: Vec<NodeId>,
}

pub async fn search(
    query: &str,
    root: &SourceNode,
    llm: &dyn LlmClient,
    max_depth: usize,
) -> anyhow::Result<Vec<NodeId>> {
    let mut results: Vec<NodeId> = Vec::new();
    let mut frontier: Vec<(&SourceNode, usize)> = vec![(root, 0)];
    while let Some((node, depth)) = frontier.pop() {
        if node.children.is_empty() || depth >= max_depth {
            results.push(node.id.clone());
            continue;
        }
        let decision = ask_navigate(query, node, llm).await?;
        if decision.terminal {
            results.extend(decision.leaves);
            continue;
        }
        for next_id in &decision.next {
            if let Some(child) = node.children.iter().find(|c| &c.id == next_id) {
                frontier.push((child, depth + 1));
            }
        }
    }
    Ok(results)
}

async fn ask_navigate(
    query: &str,
    node: &SourceNode,
    llm: &dyn LlmClient,
) -> anyhow::Result<NavigateDecision> {
    let mut prompt = String::new();
    prompt.push_str(&format!("Query: {query}\n"));
    prompt.push_str(&format!("Current node: {} ({:?})\n", node.name, node.kind));
    prompt.push_str("Children:\n");
    for c in &node.children {
        prompt.push_str(&format!("- id={} kind={:?} name={} summary={}\n", c.id, c.kind, c.name, c.summary));
    }
    prompt.push_str(
        r#"
JSON で {"next": ["id1", "id2", ...], "terminal": bool, "leaves": ["id", ...]} を返してください。
"#,
    );
    let resp = llm
        .chat(ChatRequest {
            messages: vec![
                ChatMessage::system(tmoe_prompts_navigate()),
                ChatMessage::user(prompt),
            ],
            ..Default::default()
        })
        .await
        .with_context(|| "ask_navigate llm chat failed")?;
    parse_decision(&resp.content).with_context(|| format!("decode navigate: {}", resp.content))
}

fn tmoe_prompts_navigate() -> &'static str {
    "あなたは tmoe-rag のナビゲータです。ノード要約を読んで、クエリに関連する子を選んでください。"
}

fn parse_decision(text: &str) -> anyhow::Result<NavigateDecision> {
    let start = text
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("no JSON object end"))?;
    let payload = &text[start..=end];
    let v: NavigateDecision = serde_json::from_str(payload)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmoe_llm::{MockLlmClient, ScriptedTurn};
    use tmoe_tree::{NodeKind, SourceNode};

    fn leaf(id: &str, name: &str, summary: &str) -> SourceNode {
        SourceNode {
            id: id.into(),
            kind: NodeKind::Function,
            name: name.into(),
            path: "x.rs".into(),
            start_line: 1,
            end_line: 1,
            children: vec![],
            summary: summary.into(),
            content_hash: "h".into(),
        }
    }

    fn file_node(id: &str, name: &str, children: Vec<SourceNode>) -> SourceNode {
        SourceNode {
            id: id.into(),
            kind: NodeKind::File,
            name: name.into(),
            path: name.into(),
            start_line: 1,
            end_line: 1,
            children,
            summary: format!("file {name}"),
            content_hash: "f".into(),
        }
    }

    #[tokio::test]
    async fn search_navigates_to_target_leaf() {
        let target = leaf("target", "ConciergeAgent", "concierge user io channel");
        let other = leaf("other", "GcdComputer", "math util gcd");
        let f = file_node("f1", "agent.rs", vec![target.clone(), other]);
        let repo = SourceNode {
            id: "repo".into(),
            kind: NodeKind::Repo,
            name: "repo".into(),
            path: "/".into(),
            start_line: 0,
            end_line: 0,
            children: vec![f],
            summary: "repo".into(),
            content_hash: "r".into(),
        };
        let llm = MockLlmClient::new("rag");
        // 1 度目: repo → file f1 へ
        llm.push(ScriptedTurn::new(
            r#"{"next":["f1"],"terminal":false,"leaves":[]}"#,
        ));
        // 2 度目: file f1 → target を terminal=true で返す
        llm.push(ScriptedTurn::new(
            r#"{"next":[],"terminal":true,"leaves":["target"]}"#,
        ));
        let res = search("ConciergeAgent", &repo, &llm, 4).await.unwrap();
        assert!(res.contains(&"target".to_string()));
    }

    #[tokio::test]
    async fn search_falls_through_when_node_has_no_children() {
        let n = leaf("only", "x", "nothing");
        let llm = MockLlmClient::new("rag");
        let res = search("anything", &n, &llm, 4).await.unwrap();
        assert_eq!(res, vec!["only".to_string()]);
    }
}
