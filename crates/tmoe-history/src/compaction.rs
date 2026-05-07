//! 逐次コンパクション (incremental rolling summary)。
//!
//! 一括圧縮ではなく、ターンごとに各エージェントが**自分のパーソナリティで** raw を取捨選択し
//! `agent_summary_node` を 1 ノードずつ延伸する。膨らんだら 1 階層上に rollup する。
//! 中立要約は作らない (3 エージェントが同じ raw を異なる視点で持つことが本機構の本質)。

use crate::error::{HistoryError, Result};
use crate::store::{AppendSummary, HistoryStore};
use crate::types::{AgentSummaryNode, AgentView, RawNode};
use async_trait::async_trait;
use std::sync::Arc;
use tmoe_llm::{ChatMessage, ChatRequest, LlmClient};

/// 各エージェントの「視点」を抽象化する trait。
/// 実装はパーソナリティごとに異なる取捨選択ルールを表現する。
///
/// `extend_leaf` / `rollup` は async (LLM 呼び出し可能)。決定論的なフォールバック
/// 実装 (`LabeledLens`) は async でも即座に値を返すため、テスト用途では同期同等に扱える。
#[async_trait]
pub trait AgentLens: Send + Sync {
    fn agent(&self) -> AgentView;

    /// 末端 (level=0) の要約に追記するテキストを返す。
    async fn extend_leaf(&self, existing: &str, raw_node: &RawNode, raw_body: &str) -> String;

    /// 1 階層上 (level+1) への巻き上げ要約を返す。
    async fn rollup(&self, children_summaries: &[String]) -> String;
}

/// パーソナリティが似てしまうと合意平面が縮退する (3 点が同一直線上に並ぶ)。
/// 既定実装は **冒頭ラベル** で各レンズを差別化する最小実装。実装フェーズでは LLM 呼び出しに置換する。
pub struct LabeledLens {
    pub agent: AgentView,
    pub label: &'static str,
}

#[async_trait]
impl AgentLens for LabeledLens {
    fn agent(&self) -> AgentView {
        self.agent
    }

    async fn extend_leaf(&self, existing: &str, _raw_node: &RawNode, raw_body: &str) -> String {
        let head = match self.agent {
            AgentView::Worker => "[builder]",
            AgentView::Supervisor => "[critic]",
            AgentView::Observer => "[witness]",
        };
        let kept = self.filter_for_view(raw_body);
        if kept.trim().is_empty() {
            // このターンは自分の関心事ではない → 何も追記しない (パーソナリティ別フィルタ)。
            return existing.to_string();
        }
        if existing.is_empty() {
            format!("{} {}: {}", head, self.label, kept)
        } else {
            format!("{}\n{} {}: {}", existing, head, self.label, kept)
        }
    }

    async fn rollup(&self, children: &[String]) -> String {
        let head = match self.agent {
            AgentView::Worker => "[builder/rollup]",
            AgentView::Supervisor => "[critic/rollup]",
            AgentView::Observer => "[witness/rollup]",
        };
        format!("{} {}", head, children.join(" | "))
    }
}

/// LLM 駆動 Lens (デフォルト)。エージェントのパーソナリティプロンプトを LLM に渡し、
/// raw 本文の中から自分の view に残すべき内容を **そのエージェント自身の判断で**
/// 取捨選択させる。新ツールを追加してもキーワード辞書を更新する必要はない。
///
/// プロンプトには `existing` summary を含めるので、Lens は「過去の自分の要約に何を
/// 追記すべきか」を判断できる (= rolling summary)。
pub struct LlmLens {
    pub agent: AgentView,
    pub system: String,
    pub llm: Arc<dyn LlmClient>,
    /// 1 ノードあたりの要約上限文字数。膨らみ続けないようにする。
    pub max_summary_chars: usize,
}

impl LlmLens {
    pub fn new(agent: AgentView, system: impl Into<String>, llm: Arc<dyn LlmClient>) -> Self {
        Self {
            agent,
            system: system.into(),
            llm,
            max_summary_chars: 4000,
        }
    }

    fn personality_brief(&self) -> &'static str {
        match self.agent {
            AgentView::Worker => {
                "You are tracking ONLY implementation progress: tool calls that succeeded, \
                 files written, identifiers renamed, code added or removed. Discard normative \
                 critiques (those go to Supervisor) and user-intent commentary (Observer)."
            }
            AgentView::Supervisor => {
                "You are tracking ONLY normative concerns: rule violations, failed tool calls, \
                 invalid arguments, safety issues, requirement gaps. Discard raw implementation \
                 details and user-intent commentary."
            }
            AgentView::Observer => {
                "You are tracking ONLY context, intent and continuity: what the user asked for, \
                 whether the work has stayed on intent, signs of looping or repetition. Discard \
                 implementation details and normative critiques."
            }
        }
    }
}

#[async_trait]
impl AgentLens for LlmLens {
    fn agent(&self) -> AgentView {
        self.agent
    }

    async fn extend_leaf(
        &self,
        existing: &str,
        _raw_node: &RawNode,
        raw_body: &str,
    ) -> String {
        let prompt = format!(
            "{persona}\n\nYour previous summary (may be empty):\n{existing}\n\n\
             New raw turn content:\n{raw}\n\n\
             Update your summary so it captures only what is relevant to your view. \
             Stay under {max} characters total. Output ONLY the updated summary, \
             with no surrounding prose.",
            persona = self.personality_brief(),
            existing = if existing.is_empty() { "(empty)" } else { existing },
            raw = raw_body,
            max = self.max_summary_chars,
        );
        let req = ChatRequest {
            messages: vec![
                ChatMessage::system(self.system.clone()),
                ChatMessage::user(prompt),
            ],
            max_tokens: Some(800),
            temperature: Some(0.0),
            ..Default::default()
        };
        match self.llm.chat(req).await {
            Ok(resp) => {
                let trimmed = resp.content.trim();
                if trimmed.is_empty() {
                    existing.to_string()
                } else if trimmed.len() > self.max_summary_chars {
                    trimmed.chars().take(self.max_summary_chars).collect()
                } else {
                    trimmed.to_string()
                }
            }
            Err(_) => existing.to_string(),
        }
    }

    async fn rollup(&self, children: &[String]) -> String {
        let prompt = format!(
            "{persona}\n\nThese are several level-N summaries from your view. Merge them into \
             ONE concise level-(N+1) summary, keeping only what is still relevant going forward. \
             Stay under {max} characters. Output ONLY the merged summary.\n\n\
             Children:\n- {joined}",
            persona = self.personality_brief(),
            max = self.max_summary_chars,
            joined = children.join("\n- "),
        );
        let req = ChatRequest {
            messages: vec![
                ChatMessage::system(self.system.clone()),
                ChatMessage::user(prompt),
            ],
            max_tokens: Some(1200),
            temperature: Some(0.0),
            ..Default::default()
        };
        match self.llm.chat(req).await {
            Ok(resp) => {
                let trimmed = resp.content.trim();
                if trimmed.is_empty() {
                    children.join(" | ")
                } else if trimmed.len() > self.max_summary_chars {
                    trimmed.chars().take(self.max_summary_chars).collect()
                } else {
                    trimmed.to_string()
                }
            }
            Err(_) => children.join(" | "),
        }
    }
}

impl LabeledLens {
    /// 各視点の取捨選択を「キーワードに引っかかるか」で粗く模倣する **テスト/フォールバック専用**
    /// 実装。新しいツールを追加するたびに辞書を増やすべき設計ではない (本来は LLM 駆動の
    /// `LlmLens` がデフォルト)。本フィルタはあくまで決定論テストや LLM 不在環境用。
    fn filter_for_view(&self, body: &str) -> String {
        match self.agent {
            AgentView::Worker => Self::keep_lines_with(body, &[
                "impl", "fn ", "patch", "edit", "diff", "build", "完了", "実装", "追加", "修正",
            ]),
            AgentView::Supervisor => Self::keep_lines_with(body, &[
                "warn", "error", "invalid", "reject", "must", "should", "拒否", "差し戻し",
                "違反", "整合", "安全",
            ]),
            AgentView::Observer => Self::keep_lines_with(body, &[
                "intent", "user", "loop", "context", "意図", "ユーザー", "履歴", "繰り返し",
            ]),
        }
    }

    fn keep_lines_with(body: &str, needles: &[&str]) -> String {
        let lines: Vec<&str> = body
            .lines()
            .filter(|line| {
                let lower = line.to_lowercase();
                needles
                    .iter()
                    .any(|n| lower.contains(&n.to_lowercase()))
            })
            .collect();
        // 関心事が無いなら空を返す (= このターンは自分の view に残さない)。
        // 空フォールバックを使わないことで「3 view が異なる粒度を持つ」が成立する。
        lines.join(" / ")
    }
}

/// 1 ターン分を 3 エージェント並走で逐次コンパクションする。
/// 既存の level=0 ノードがあればそれを update、無ければ新規 append する。
pub async fn compact_turn_for_all(
    store: &HistoryStore,
    feature_id: &str,
    raw_node: &RawNode,
    raw_body: &str,
    lenses: &[Box<dyn AgentLens>],
) -> Result<Vec<AgentSummaryNode>> {
    if lenses.len() != 3 {
        return Err(HistoryError::Invalid(format!(
            "tmoe requires exactly 3 lenses (Worker/Supervisor/Observer), got {}",
            lenses.len()
        )));
    }
    // 3 つの agent が必ず 1 つずつ存在することを検証 (頂点が同一直線に並ばないように)。
    let mut seen = [false; 3];
    for lens in lenses {
        let idx = match lens.agent() {
            AgentView::Worker => 0,
            AgentView::Supervisor => 1,
            AgentView::Observer => 2,
        };
        if seen[idx] {
            return Err(HistoryError::Invalid(format!(
                "duplicate lens for agent {:?}",
                lens.agent()
            )));
        }
        seen[idx] = true;
    }
    if !seen.iter().all(|x| *x) {
        return Err(HistoryError::Invalid(
            "tmoe lenses must cover Worker, Supervisor, and Observer".into(),
        ));
    }

    let mut updated = Vec::with_capacity(3);
    for lens in lenses {
        let agent = lens.agent();
        let latest = store.latest_level0(feature_id, agent)?;
        let next_summary = match &latest {
            Some(node) => lens.extend_leaf(&node.summary, raw_node, raw_body).await,
            None => lens.extend_leaf("", raw_node, raw_body).await,
        };
        match latest {
            Some(node) => {
                // 関心事ゼロ (extend_leaf が変化なしを返した) のターンは無視する。
                // ref_raw_ids も伸ばさず、Supervisor / Observer の view が「関心事のない
                // ターン」で汚染されないようにする (= 三角形を維持する)。
                if next_summary == node.summary {
                    updated.push(node);
                    continue;
                }
                let mut ref_raw = node.ref_raw_ids.clone();
                let mut ref_h = node.ref_hashes.clone();
                if !ref_raw.contains(&raw_node.id) {
                    ref_raw.push(raw_node.id.clone());
                    ref_h.push(raw_node.content_hash.clone());
                }
                store.update_summary(&node.id, &next_summary, &ref_raw, &ref_h)?;
                let refreshed = store
                    .list_summary(feature_id, agent)?
                    .into_iter()
                    .find(|n| n.id == node.id)
                    .ok_or_else(|| HistoryError::Invalid("summary vanished".into()))?;
                updated.push(refreshed);
            }
            None => {
                if next_summary.is_empty() {
                    // このエージェントの最初のターンが関心事ゼロ → ノードを作らない。
                    // 後続ターンで関心事が来たら、そのとき初めて level=0 ノードを生成する。
                    continue;
                }
                let new_node = store.append_summary(AppendSummary {
                    feature_id: feature_id.to_string(),
                    agent,
                    parent_id: None,
                    summary: next_summary,
                    ref_raw_ids: vec![raw_node.id.clone()],
                    ref_hashes: vec![raw_node.content_hash.clone()],
                    level: 0,
                })?;
                updated.push(new_node);
            }
        }
    }
    Ok(updated)
}

/// level=L のノード群を 1 つの level=L+1 ノードに巻き上げる。
/// 1 回呼び出しで 1 段だけ進める (急峻なスパイクを避ける)。
pub async fn rollup_one_level(
    store: &HistoryStore,
    feature_id: &str,
    agent: AgentView,
    level: i32,
    lens: &dyn AgentLens,
) -> Result<Option<AgentSummaryNode>> {
    if lens.agent() != agent {
        return Err(HistoryError::Invalid("lens agent mismatch".into()));
    }
    let nodes: Vec<AgentSummaryNode> = store
        .list_summary(feature_id, agent)?
        .into_iter()
        .filter(|n| n.level == level)
        .collect();
    if nodes.len() < 2 {
        return Ok(None); // 1 個以下の場合は巻き上げ対象なし。
    }
    let summaries: Vec<String> = nodes.iter().map(|n| n.summary.clone()).collect();
    let merged_raw_ids: Vec<String> = nodes
        .iter()
        .flat_map(|n| n.ref_raw_ids.clone())
        .collect();
    let merged_hashes: Vec<String> = nodes
        .iter()
        .flat_map(|n| n.ref_hashes.clone())
        .collect();
    let next = store.append_summary(AppendSummary {
        feature_id: feature_id.to_string(),
        agent,
        parent_id: None,
        summary: lens.rollup(&summaries).await,
        ref_raw_ids: merged_raw_ids,
        ref_hashes: merged_hashes,
        level: level + 1,
    })?;
    Ok(Some(next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AppendRaw;
    use crate::types::RawKind;
    use tempfile::tempdir;

    fn lenses() -> Vec<Box<dyn AgentLens>> {
        vec![
            Box::new(LabeledLens { agent: AgentView::Worker, label: "W" }),
            Box::new(LabeledLens { agent: AgentView::Supervisor, label: "S" }),
            Box::new(LabeledLens { agent: AgentView::Observer, label: "O" }),
        ]
    }

    fn store() -> (tempfile::TempDir, HistoryStore) {
        let d = tempdir().unwrap();
        let s = HistoryStore::open(d.path()).unwrap();
        (d, s)
    }

    #[tokio::test]
    async fn three_agents_diverge_on_same_raw() {
        let (_d, s) = store();
        let f = s.create_feature("ft").unwrap();
        // 3 視点それぞれが拾うキーワードを混ぜた raw 本文。
        let body = "implement gcd / must check overflow / user intent: math util\nfn gcd() { ... }";
        let raw = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: body.into(),
            })
            .unwrap();

        let lenses = lenses();
        let updated = compact_turn_for_all(&s, &f.id, &raw, body, &lenses).await.unwrap();
        assert_eq!(updated.len(), 3);
        // 3 view の要約が互いに異なる (=三角形が縮退していない)。
        let mut summaries: Vec<String> = updated.iter().map(|n| n.summary.clone()).collect();
        summaries.sort();
        summaries.dedup();
        assert_eq!(summaries.len(), 3, "summaries must be distinct: {summaries:?}");
        // 各 view が想定する語を最低 1 つ含むか確認。
        let w = s.latest_level0(&f.id, AgentView::Worker).unwrap().unwrap();
        let p = s.latest_level0(&f.id, AgentView::Supervisor).unwrap().unwrap();
        let o = s.latest_level0(&f.id, AgentView::Observer).unwrap().unwrap();
        assert!(w.summary.contains("implement") || w.summary.contains("fn gcd"));
        assert!(p.summary.contains("must") || p.summary.contains("overflow"));
        assert!(o.summary.contains("intent") || o.summary.contains("user"));
    }

    #[tokio::test]
    async fn second_turn_extends_existing_leaf_in_place() {
        let (_d, s) = store();
        let f = s.create_feature("ft").unwrap();
        let body1 = "implement gcd\nuser asked";
        let body2 = "fn gcd() impl complete";
        let r1 = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: body1.into(),
            })
            .unwrap();
        let r2 = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: body2.into(),
            })
            .unwrap();
        let lenses = lenses();
        compact_turn_for_all(&s, &f.id, &r1, body1, &lenses).await.unwrap();
        compact_turn_for_all(&s, &f.id, &r2, body2, &lenses).await.unwrap();
        // Worker 視点: 1 つの level=0 ノードが両ターンを参照しているはず。
        let nodes = s.list_summary(&f.id, AgentView::Worker).unwrap();
        let level0: Vec<_> = nodes.iter().filter(|n| n.level == 0).collect();
        assert_eq!(level0.len(), 1, "incremental compaction must extend the leaf, not append a new node per turn");
        assert_eq!(level0[0].ref_raw_ids.len(), 2);
    }

    #[tokio::test]
    async fn rollup_one_level_creates_higher_node() {
        let (_d, s) = store();
        let f = s.create_feature("ft").unwrap();
        // 同じ raw に対して 2 件の手動 level=0 ノードを作って rollup 入力にする。
        let r = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: "x".into(),
            })
            .unwrap();
        for label in ["a", "b"] {
            s.append_summary(AppendSummary {
                feature_id: f.id.clone(),
                agent: AgentView::Worker,
                parent_id: None,
                summary: label.into(),
                ref_raw_ids: vec![r.id.clone()],
                ref_hashes: vec![r.content_hash.clone()],
                level: 0,
            })
            .unwrap();
        }
        let lens = LabeledLens { agent: AgentView::Worker, label: "W" };
        let rolled = rollup_one_level(&s, &f.id, AgentView::Worker, 0, &lens).await.unwrap().unwrap();
        assert_eq!(rolled.level, 1);
        assert!(rolled.summary.contains("rollup"));
    }

    #[tokio::test]
    async fn requires_three_lenses() {
        let (_d, s) = store();
        let f = s.create_feature("ft").unwrap();
        let r = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: "x".into(),
            })
            .unwrap();
        let only_two: Vec<Box<dyn AgentLens>> = vec![
            Box::new(LabeledLens { agent: AgentView::Worker, label: "W" }),
            Box::new(LabeledLens { agent: AgentView::Supervisor, label: "S" }),
        ];
        let err = compact_turn_for_all(&s, &f.id, &r, "x", &only_two).await.unwrap_err();
        assert!(matches!(err, HistoryError::Invalid(_)));
    }

    #[tokio::test]
    async fn llm_lens_uses_llm_for_extend_leaf() {
        use tmoe_llm::{MockLlmClient, ScriptedTurn};
        let llm = Arc::new(MockLlmClient::new("worker-llm-lens"));
        llm.push(ScriptedTurn::new("- patched src/lib.rs (1 replacement)"));
        let lens = LlmLens::new(AgentView::Worker, "worker-system", llm.clone() as Arc<dyn LlmClient>);

        let (_d, s) = store();
        let f = s.create_feature("ft").unwrap();
        let raw = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: "tool: patch_file -> patched src/lib.rs (1 replacement)".into(),
            })
            .unwrap();
        let summary = lens.extend_leaf("", &raw, "tool: patch_file -> patched src/lib.rs (1 replacement)").await;
        assert!(summary.contains("patched src/lib.rs"));
        assert_eq!(llm.calls().len(), 1);
    }

    #[tokio::test]
    async fn llm_lens_falls_back_to_existing_on_llm_error() {
        use tmoe_llm::{MockLlmClient};
        let llm = Arc::new(MockLlmClient::new("flaky"));
        // No scripted turns -> chat() returns MockExhausted error
        let lens = LlmLens::new(AgentView::Supervisor, "sup-system", llm.clone() as Arc<dyn LlmClient>);

        let (_d, s) = store();
        let f = s.create_feature("ft").unwrap();
        let raw = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: "warn: must check overflow".into(),
            })
            .unwrap();
        let prev = "previous critique view content";
        let updated = lens.extend_leaf(prev, &raw, "warn: must check overflow").await;
        // LLM 失敗時は既存 summary をそのまま維持 (= 破損しない)。
        assert_eq!(updated, prev);
    }

    #[tokio::test]
    async fn llm_lens_truncates_overlong_responses() {
        use tmoe_llm::{MockLlmClient, ScriptedTurn};
        let huge: String = "x".repeat(20_000);
        let llm = Arc::new(MockLlmClient::new("verbose"));
        llm.push(ScriptedTurn::new(huge));
        let mut lens = LlmLens::new(
            AgentView::Worker,
            "worker-system",
            llm.clone() as Arc<dyn LlmClient>,
        );
        lens.max_summary_chars = 200;

        let (_d, s) = store();
        let f = s.create_feature("ft").unwrap();
        let raw = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: "anything".into(),
            })
            .unwrap();
        let updated = lens.extend_leaf("", &raw, "anything").await;
        assert!(updated.len() <= 200);
    }

    #[tokio::test]
    async fn llm_lens_rollup_merges_via_llm() {
        use tmoe_llm::{MockLlmClient, ScriptedTurn};
        let llm = Arc::new(MockLlmClient::new("merger"));
        llm.push(ScriptedTurn::new("merged: a + b"));
        let lens = LlmLens::new(AgentView::Worker, "worker-system", llm.clone() as Arc<dyn LlmClient>);
        let merged = lens.rollup(&["a".to_string(), "b".to_string()]).await;
        assert_eq!(merged, "merged: a + b");
    }

    #[tokio::test]
    async fn rejects_duplicate_lens() {
        let (_d, s) = store();
        let f = s.create_feature("ft").unwrap();
        let r = s
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: "x".into(),
            })
            .unwrap();
        let dup: Vec<Box<dyn AgentLens>> = vec![
            Box::new(LabeledLens { agent: AgentView::Worker, label: "W" }),
            Box::new(LabeledLens { agent: AgentView::Worker, label: "W2" }),
            Box::new(LabeledLens { agent: AgentView::Observer, label: "O" }),
        ];
        let err = compact_turn_for_all(&s, &f.id, &r, "x", &dup).await.unwrap_err();
        assert!(matches!(err, HistoryError::Invalid(_)));
    }
}
