//! 逐次コンパクション (incremental rolling summary)。
//!
//! 一括圧縮ではなく、ターンごとに各エージェントが**自分のパーソナリティで** raw を取捨選択し
//! `agent_summary_node` を 1 ノードずつ延伸する。膨らんだら 1 階層上に rollup する。
//! 中立要約は作らない (3 エージェントが同じ raw を異なる視点で持つことが本機構の本質)。

use crate::error::{HistoryError, Result};
use crate::store::{AppendSummary, HistoryStore};
use crate::types::{AgentSummaryNode, AgentView, RawNode};

/// 各エージェントの「視点」を抽象化する trait。
/// 実装はパーソナリティごとに異なる取捨選択ルールを表現する。
pub trait AgentLens: Send + Sync {
    fn agent(&self) -> AgentView;

    /// 末端 (level=0) の要約に追記するテキストを返す。
    /// `existing` は前回までの末端要約 (なければ空)、`raw_body` は今回の raw 本文。
    fn extend_leaf(&self, existing: &str, raw_node: &RawNode, raw_body: &str) -> String;

    /// 1 階層上 (level+1) への巻き上げ要約を返す。
    /// `children_summaries` は同じ親ノードの直下にある子要約群。
    fn rollup(&self, children_summaries: &[String]) -> String;
}

/// パーソナリティが似てしまうと合意平面が縮退する (3 点が同一直線上に並ぶ)。
/// 既定実装は **冒頭ラベル** で各レンズを差別化する最小実装。実装フェーズでは LLM 呼び出しに置換する。
pub struct LabeledLens {
    pub agent: AgentView,
    pub label: &'static str,
}

impl AgentLens for LabeledLens {
    fn agent(&self) -> AgentView {
        self.agent
    }

    fn extend_leaf(&self, existing: &str, _raw_node: &RawNode, raw_body: &str) -> String {
        let head = match self.agent {
            AgentView::Worker => "[builder]",
            AgentView::Supervisor => "[critic]",
            AgentView::Observer => "[witness]",
        };
        let kept = self.filter_for_view(raw_body);
        if existing.is_empty() {
            format!("{} {}: {}", head, self.label, kept)
        } else {
            format!("{}\n{} {}: {}", existing, head, self.label, kept)
        }
    }

    fn rollup(&self, children: &[String]) -> String {
        let head = match self.agent {
            AgentView::Worker => "[builder/rollup]",
            AgentView::Supervisor => "[critic/rollup]",
            AgentView::Observer => "[witness/rollup]",
        };
        format!("{} {}", head, children.join(" | "))
    }
}

impl LabeledLens {
    /// 各視点の取捨選択を「キーワードに引っかかるか」で粗く模倣する。
    /// LLM 呼び出しに置換する前提だが、テストの三角形検証はこれで成立する。
    fn filter_for_view(&self, body: &str) -> String {
        match self.agent {
            // Worker: 実装決定・差分・完了に関する語を優先。
            AgentView::Worker => Self::keep_lines_with(body, &[
                "impl", "fn ", "patch", "edit", "diff", "build", "完了", "実装", "追加", "修正",
            ]),
            // Supervisor: 規範・違反・差し戻し・整合性。
            AgentView::Supervisor => Self::keep_lines_with(body, &[
                "warn", "error", "invalid", "reject", "must", "should", "拒否", "差し戻し",
                "違反", "整合", "安全",
            ]),
            // Observer: 意図・連続性・ループ・俯瞰。
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
        if lines.is_empty() {
            // フォールバック: 先頭 80 文字を要約として残す (空 view を避ける)。
            body.chars().take(80).collect()
        } else {
            lines.join(" / ")
        }
    }
}

/// 1 ターン分を 3 エージェント並走で逐次コンパクションする。
/// 既存の level=0 ノードがあればそれを update、無ければ新規 append する。
pub fn compact_turn_for_all(
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
            Some(node) => lens.extend_leaf(&node.summary, raw_node, raw_body),
            None => lens.extend_leaf("", raw_node, raw_body),
        };
        match latest {
            Some(node) => {
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
pub fn rollup_one_level(
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
        summary: lens.rollup(&summaries),
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

    #[test]
    fn three_agents_diverge_on_same_raw() {
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
        let updated = compact_turn_for_all(&s, &f.id, &raw, body, &lenses).unwrap();
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

    #[test]
    fn second_turn_extends_existing_leaf_in_place() {
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
        compact_turn_for_all(&s, &f.id, &r1, body1, &lenses).unwrap();
        compact_turn_for_all(&s, &f.id, &r2, body2, &lenses).unwrap();
        // Worker 視点: 1 つの level=0 ノードが両ターンを参照しているはず。
        let nodes = s.list_summary(&f.id, AgentView::Worker).unwrap();
        let level0: Vec<_> = nodes.iter().filter(|n| n.level == 0).collect();
        assert_eq!(level0.len(), 1, "incremental compaction must extend the leaf, not append a new node per turn");
        assert_eq!(level0[0].ref_raw_ids.len(), 2);
    }

    #[test]
    fn rollup_one_level_creates_higher_node() {
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
        let rolled = rollup_one_level(&s, &f.id, AgentView::Worker, 0, &lens).unwrap().unwrap();
        assert_eq!(rolled.level, 1);
        assert!(rolled.summary.contains("rollup"));
    }

    #[test]
    fn requires_three_lenses() {
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
        let err = compact_turn_for_all(&s, &f.id, &r, "x", &only_two).unwrap_err();
        assert!(matches!(err, HistoryError::Invalid(_)));
    }

    #[test]
    fn rejects_duplicate_lens() {
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
        let err = compact_turn_for_all(&s, &f.id, &r, "x", &dup).unwrap_err();
        assert!(matches!(err, HistoryError::Invalid(_)));
    }
}
