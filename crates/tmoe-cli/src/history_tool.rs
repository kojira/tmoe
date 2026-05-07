//! `search_history`: tmoe-history の機能別 3 view 要約に対する Agentic RAG。
//!
//! tmoe-rag (ソース AST 用) と同じ思想 (= ベクトル類似度を使わず LLM がノード要約を読んで
//! 「次に開く子」を選ぶ推論探索) を、**会話履歴の階層** に当てる。
//!
//! 木構造:
//!   root
//!     ├ feature(id_1)
//!     │   ├ worker_view (level=N..0)
//!     │   ├ supervisor_view (level=N..0)
//!     │   └ observer_view (level=N..0)
//!     ├ feature(id_2) ...
//!
//! 既定では **全 feature を対象** にする (= 「過去の機能で似たことをやった記憶」を
//! 横断的に引ける)。`scope=current` 指定 + 文脈の現在 feature_id があれば 1 feature に
//! 絞る。`agent=worker|supervisor|observer|any` で view を絞れる。
//!
//! LLM 呼び出しは「特定 feature を開くか / 特定 view を読むか」を 1 ステップずつ判断する形で、
//! 深さは worst-case でも `1 (root) + 1 (feature) + 1 (view)` = 3 段階。

use anyhow::Context;
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use tmoe_history::{AgentView, Feature, HistoryStore};
use tmoe_llm::{ChatMessage, ChatRequest, LlmClient};
use tmoe_tools::{Permission, Tool, ToolError, ToolOutput, ToolResult};

pub struct SearchHistoryTool {
    pub store: Arc<HistoryStore>,
    pub llm: Arc<dyn LlmClient>,
    /// 1 query で返すヒット件数の上限。
    pub max_results: usize,
    /// 走査対象の最大 feature 数。古い feature を切り捨てるため。
    pub max_features: usize,
    /// 1 feature あたり 1 view の summary を LLM に見せるときの最大文字数。
    pub max_summary_chars: usize,
    /// 現在のセッションが属する feature_id (= `scope=current` の対象)。
    pub current_feature_id: Option<String>,
}

impl SearchHistoryTool {
    pub fn new(store: Arc<HistoryStore>, llm: Arc<dyn LlmClient>) -> Self {
        Self {
            store,
            llm,
            max_results: 5,
            max_features: 30,
            max_summary_chars: 800,
            current_feature_id: None,
        }
    }

    pub fn with_current_feature(mut self, id: impl Into<String>) -> Self {
        self.current_feature_id = Some(id.into());
        self
    }
}

#[derive(Deserialize)]
struct Args {
    query: String,
    #[serde(default)]
    agent: Option<String>, // "worker"|"supervisor"|"observer"|"any" (default any)
    #[serde(default)]
    scope: Option<String>, // "current"|"all" (default all)
}

#[derive(Debug, Clone)]
pub struct HistoryHit {
    pub feature_id: String,
    pub feature_title: String,
    pub agent: AgentView,
    pub summary: String,
}

#[async_trait]
impl Tool for SearchHistoryTool {
    fn name(&self) -> &str {
        "search_history"
    }
    fn requires(&self) -> Permission {
        Permission::Read
    }
    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: Args = serde_json::from_value(args.clone())
            .map_err(|e| ToolError::Args(format!("search_history args: {e}")))?;
        if a.query.trim().is_empty() {
            return Err(ToolError::Args("query must be non-empty".into()));
        }
        let agent_filter = parse_agent(a.agent.as_deref())?;
        let scope_current = matches!(a.scope.as_deref(), Some("current"));

        let mut features = self
            .store
            .list_features()
            .map_err(|e| ToolError::Args(format!("list features: {e}")))?;
        if scope_current {
            if let Some(cur) = &self.current_feature_id {
                features.retain(|f| &f.id == cur);
            } else {
                return Err(ToolError::Args(
                    "scope=current requested but no current_feature_id is bound".into(),
                ));
            }
        }
        if features.len() > self.max_features {
            features.truncate(self.max_features);
        }
        if features.is_empty() {
            return Ok(ToolOutput::text("(no features in history)"));
        }

        // 1) LLM に「どの feature を開くか」を選ばせる。各 feature に短い手がかり
        //    (title + 3 view brief の冒頭) を渡す。
        let chosen_features = pick_features(
            &self.llm,
            &a.query,
            &features,
            &self.store,
            self.max_summary_chars / 4,
        )
        .await
        .map_err(|e| ToolError::Args(format!("pick_features: {e}")))?;

        if chosen_features.is_empty() {
            return Ok(ToolOutput::text(format!(
                "(LLM declined to enter any of {} feature(s) for query: {:?})",
                features.len(),
                a.query
            )));
        }

        // 2) 選ばれた feature ごとに、各 view summary を読み込み、agent_filter / max_results 制約で
        //    HistoryHit を組み立てる。
        let mut hits: Vec<HistoryHit> = Vec::new();
        for f in chosen_features {
            for view in AgentView::all() {
                if let Some(only) = agent_filter {
                    if only != view {
                        continue;
                    }
                }
                if let Ok(Some(node)) = self.store.latest_level0(&f.id, view) {
                    let summary = if node.summary.len() > self.max_summary_chars {
                        node.summary
                            .chars()
                            .take(self.max_summary_chars)
                            .collect::<String>()
                    } else {
                        node.summary.clone()
                    };
                    hits.push(HistoryHit {
                        feature_id: f.id.clone(),
                        feature_title: f.title.clone(),
                        agent: view,
                        summary,
                    });
                    if hits.len() >= self.max_results {
                        break;
                    }
                }
            }
            if hits.len() >= self.max_results {
                break;
            }
        }

        if hits.is_empty() {
            return Ok(ToolOutput::text(format!(
                "(features matched but no view summaries available for query: {:?})",
                a.query
            )));
        }

        let body = hits
            .iter()
            .map(|h| {
                format!(
                    "feature {} [{}] ({:?}):\n  title: {}\n  summary: {}\n",
                    h.feature_id,
                    h.agent.as_str(),
                    h.agent,
                    h.feature_title,
                    h.summary.lines().take(8).collect::<Vec<_>>().join("\n  "),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::text(body))
    }
}

fn parse_agent(s: Option<&str>) -> Result<Option<AgentView>, ToolError> {
    Ok(match s {
        None | Some("any") | Some("all") | Some("") => None,
        Some("worker") => Some(AgentView::Worker),
        Some("supervisor") => Some(AgentView::Supervisor),
        Some("observer") => Some(AgentView::Observer),
        Some(other) => {
            return Err(ToolError::Args(format!(
                "unknown agent filter: {other} (expected worker|supervisor|observer|any)"
            )))
        }
    })
}

#[derive(Deserialize, Debug)]
struct PickDecision {
    /// 選ばれた feature の id 列 (順序保持)。空なら「該当無し」。
    pub features: Vec<String>,
}

async fn pick_features(
    llm: &Arc<dyn LlmClient>,
    query: &str,
    features: &[Feature],
    store: &HistoryStore,
    brief_chars: usize,
) -> anyhow::Result<Vec<Feature>> {
    let mut listing = String::new();
    for f in features {
        listing.push_str(&format!("- id={} title={:?}\n", f.id, f.title));
        for view in AgentView::all() {
            if let Ok(Some(node)) = store.latest_level0(&f.id, view) {
                let head: String = node.summary.chars().take(brief_chars).collect();
                let head = head.replace('\n', " ");
                listing.push_str(&format!("    {}: {}\n", view.as_str(), head));
            }
        }
    }
    let prompt = format!(
        "You are tmoe-history navigator. Pick the feature ids whose summaries are most likely to \
         contain information relevant to the query. Return JSON: \
         {{\"features\": [\"id1\", \"id2\", ...]}}. If none match, return \
         {{\"features\": []}}. No prose.\n\nQuery: {query}\n\nFeatures:\n{listing}"
    );
    let resp = llm
        .chat(ChatRequest {
            messages: vec![
                ChatMessage::system(
                    "You answer ONLY with one JSON object {\"features\": [...]}.",
                ),
                ChatMessage::user(prompt),
            ],
            max_tokens: Some(200),
            temperature: Some(0.0),
            ..Default::default()
        })
        .await
        .context("history navigator chat")?;
    let pick = parse_pick(&resp.content).unwrap_or(PickDecision { features: vec![] });
    let chosen: Vec<Feature> = pick
        .features
        .iter()
        .filter_map(|id| features.iter().find(|f| &f.id == id).cloned())
        .collect();
    Ok(chosen)
}

fn parse_pick(text: &str) -> Option<PickDecision> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tmoe_history::{AppendSummary, HistoryStore};
    use tmoe_llm::{MockLlmClient, ScriptedTurn};

    fn populate(store: &HistoryStore, fid: &str, title: &str, views: &[(AgentView, &str)]) {
        let raw = store
            .append_raw(tmoe_history::AppendRaw {
                feature_id: fid.into(),
                parent_id: None,
                kind: tmoe_history::RawKind::Turn,
                body: format!("seed for {title}"),
            })
            .unwrap();
        for (agent, summary) in views {
            store
                .append_summary(AppendSummary {
                    feature_id: fid.into(),
                    agent: *agent,
                    parent_id: None,
                    summary: (*summary).into(),
                    ref_raw_ids: vec![raw.id.clone()],
                    ref_hashes: vec![raw.content_hash.clone()],
                    level: 0,
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn search_history_returns_view_summary_for_picked_feature() {
        let dir = tempdir().unwrap();
        let store = Arc::new(HistoryStore::open(dir.path()).unwrap());
        let f1 = store.create_feature("compute gcd helper").unwrap();
        let f2 = store.create_feature("levenshtein util").unwrap();
        populate(
            &store,
            &f1.id,
            "compute gcd helper",
            &[
                (AgentView::Worker, "implemented gcd via euclid in src/math.rs"),
                (AgentView::Supervisor, "watch for overflow on extreme inputs"),
                (AgentView::Observer, "user wanted a math util crate"),
            ],
        );
        populate(
            &store,
            &f2.id,
            "levenshtein util",
            &[
                (AgentView::Worker, "DP table levenshtein in src/strings.rs"),
                (AgentView::Supervisor, "guard empty strings"),
                (AgentView::Observer, "user asked for fuzzy match"),
            ],
        );

        let llm = Arc::new(MockLlmClient::new("history-rag"));
        // navigator が gcd の feature を選ぶ。
        llm.push(ScriptedTurn::new(format!(
            r#"{{"features":["{}"]}}"#,
            f1.id
        )));
        let tool = SearchHistoryTool::new(store.clone(), llm.clone() as Arc<dyn LlmClient>);

        let out = tool
            .call(&serde_json::json!({"query": "gcd helper for math util"}))
            .await
            .expect("search ok");
        assert!(
            out.stdout.contains(&f1.id),
            "expected hit on feature 1 id; got:\n{}",
            out.stdout
        );
        assert!(
            out.stdout.contains("euclid"),
            "expected worker view summary in output; got:\n{}",
            out.stdout
        );
        // levenshtein が混入してはいけない (= LLM が選ばなかった feature の view はスキップ)。
        assert!(
            !out.stdout.contains("levenshtein"),
            "non-picked feature leaked: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn search_history_filters_to_single_view_when_agent_is_set() {
        let dir = tempdir().unwrap();
        let store = Arc::new(HistoryStore::open(dir.path()).unwrap());
        let f = store.create_feature("only feature").unwrap();
        populate(
            &store,
            &f.id,
            "only feature",
            &[
                (AgentView::Worker, "WORKER_VIEW_SECRET"),
                (AgentView::Supervisor, "SUPERVISOR_VIEW_SECRET"),
                (AgentView::Observer, "OBSERVER_VIEW_SECRET"),
            ],
        );
        let llm = Arc::new(MockLlmClient::new("history-rag"));
        llm.push(ScriptedTurn::new(format!(
            r#"{{"features":["{}"]}}"#,
            f.id
        )));
        let tool = SearchHistoryTool::new(store.clone(), llm.clone() as Arc<dyn LlmClient>);
        let out = tool
            .call(&serde_json::json!({"query": "anything", "agent": "supervisor"}))
            .await
            .unwrap();
        assert!(
            out.stdout.contains("SUPERVISOR_VIEW_SECRET"),
            "supervisor view should appear: {}",
            out.stdout
        );
        assert!(
            !out.stdout.contains("WORKER_VIEW_SECRET"),
            "worker view should be filtered out: {}",
            out.stdout
        );
        assert!(
            !out.stdout.contains("OBSERVER_VIEW_SECRET"),
            "observer view should be filtered out: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn search_history_scope_current_requires_binding() {
        let dir = tempdir().unwrap();
        let store = Arc::new(HistoryStore::open(dir.path()).unwrap());
        let _f = store.create_feature("solo").unwrap();
        let llm = Arc::new(MockLlmClient::new("history-rag"));
        let tool = SearchHistoryTool::new(store.clone(), llm.clone() as Arc<dyn LlmClient>);
        let err = tool
            .call(&serde_json::json!({"query": "x", "scope": "current"}))
            .await
            .unwrap_err();
        match err {
            ToolError::Args(m) => assert!(m.contains("scope=current")),
            other => panic!("expected Args, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_history_empty_query_rejected() {
        let dir = tempdir().unwrap();
        let store = Arc::new(HistoryStore::open(dir.path()).unwrap());
        let llm = Arc::new(MockLlmClient::new("history-rag"));
        let tool = SearchHistoryTool::new(store, llm.clone() as Arc<dyn LlmClient>);
        let err = tool
            .call(&serde_json::json!({"query": "  "}))
            .await
            .unwrap_err();
        match err {
            ToolError::Args(_) => {}
            other => panic!("expected Args, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_history_handles_no_features() {
        let dir = tempdir().unwrap();
        let store = Arc::new(HistoryStore::open(dir.path()).unwrap());
        let llm = Arc::new(MockLlmClient::new("history-rag"));
        let tool = SearchHistoryTool::new(store, llm.clone() as Arc<dyn LlmClient>);
        let out = tool
            .call(&serde_json::json!({"query": "anything"}))
            .await
            .unwrap();
        assert!(out.stdout.contains("no features"), "got: {}", out.stdout);
    }
}
