//! ViewProvider: 各エージェントの「現在保持している view summary」を Trio に供給する I/F。
//!
//! `tmoe-core::Trio` は ask_vote の前に Worker/Supervisor/Observer の最新 level=0 要約を
//! "PRIOR VIEWS" ブロックとして prepend する。これにより:
//!
//! - **Supervisor の票** は Worker の縦断的な進捗主張を読み「3 ターン同じことを言い続けて
//!   いるが実体が伴っていない」を判定できる
//! - **Observer の票** は 3 view を並べて「Worker が同じ提案を反復している」「Supervisor が
//!   同じ指摘を繰り返している」というループを検知できる
//!
//! Worker view を「書き込むが誰も読まない」状態を回避するための経路。

use crate::error::Result;
use crate::store::HistoryStore;
use crate::types::AgentView;

/// Trio の vote 段階で各 view の brief (= 最新 level=0 要約) を供給する。
///
/// 同期 trait にしているのは、Trio のホットループで余計な await を増やさないため。
/// 実装は SQLite キャッシュ済みの値を返すだけで十分速い。
pub trait ViewProvider: Send + Sync {
    /// 指定 view の最新 brief を返す。view がまだ無ければ None。
    fn brief(&self, agent: AgentView) -> Option<String>;
}

/// `HistoryStore` に直結した既定実装。指定 feature の最新 level=0 要約を文字列で返す。
pub struct HistoryViewProvider<'a> {
    pub store: &'a HistoryStore,
    pub feature_id: String,
    /// 1 view あたりの最大文字数 (vote プロンプトを膨らませすぎないため)。
    pub max_chars: usize,
}

impl<'a> HistoryViewProvider<'a> {
    pub fn new(store: &'a HistoryStore, feature_id: impl Into<String>) -> Self {
        Self { store, feature_id: feature_id.into(), max_chars: 2000 }
    }

    fn fetch(&self, agent: AgentView) -> Result<Option<String>> {
        let node = self.store.latest_level0(&self.feature_id, agent)?;
        Ok(node.map(|n| {
            if n.summary.len() > self.max_chars {
                n.summary.chars().take(self.max_chars).collect()
            } else {
                n.summary
            }
        }))
    }
}

impl<'a> ViewProvider for HistoryViewProvider<'a> {
    fn brief(&self, agent: AgentView) -> Option<String> {
        self.fetch(agent).ok().flatten()
    }
}

/// 3 view 全部の brief を 1 つのテキストブロックに整形する。
/// 全て None なら空文字列を返す (= prepend 不要)。
pub fn render_prior_views_block(p: &dyn ViewProvider) -> String {
    let w = p.brief(AgentView::Worker);
    let s = p.brief(AgentView::Supervisor);
    let o = p.brief(AgentView::Observer);
    if w.is_none() && s.is_none() && o.is_none() {
        return String::new();
    }
    let na = "(no view yet)";
    format!(
        "PRIOR VIEWS (each agent's own rolling summary so far):\n\
         - Worker view: {}\n\
         - Supervisor view: {}\n\
         - Observer view: {}\n\
         Use these to detect drift, looping, or unsupported claims before voting.",
        w.as_deref().unwrap_or(na),
        s.as_deref().unwrap_or(na),
        o.as_deref().unwrap_or(na),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AppendRaw, AppendSummary};
    use crate::types::RawKind;
    use tempfile::tempdir;

    #[test]
    fn history_view_provider_returns_latest_level0() {
        let dir = tempdir().unwrap();
        let store = HistoryStore::open(dir.path()).unwrap();
        let f = store.create_feature("ft").unwrap();
        let raw = store
            .append_raw(AppendRaw {
                feature_id: f.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: "x".into(),
            })
            .unwrap();
        store
            .append_summary(AppendSummary {
                feature_id: f.id.clone(),
                agent: AgentView::Worker,
                parent_id: None,
                summary: "patched src/lib.rs".into(),
                ref_raw_ids: vec![raw.id.clone()],
                ref_hashes: vec![raw.content_hash.clone()],
                level: 0,
            })
            .unwrap();
        let p = HistoryViewProvider::new(&store, &f.id);
        assert_eq!(p.brief(AgentView::Worker).as_deref(), Some("patched src/lib.rs"));
        assert!(p.brief(AgentView::Supervisor).is_none());
        assert!(p.brief(AgentView::Observer).is_none());
    }

    #[test]
    fn render_block_skips_when_all_empty() {
        struct Empty;
        impl ViewProvider for Empty {
            fn brief(&self, _a: AgentView) -> Option<String> { None }
        }
        assert!(render_prior_views_block(&Empty).is_empty());
    }

    #[test]
    fn render_block_lists_each_view() {
        struct Mock;
        impl ViewProvider for Mock {
            fn brief(&self, a: AgentView) -> Option<String> {
                Some(match a {
                    AgentView::Worker => "W-summary".into(),
                    AgentView::Supervisor => "S-summary".into(),
                    AgentView::Observer => "O-summary".into(),
                })
            }
        }
        let block = render_prior_views_block(&Mock);
        assert!(block.contains("Worker view: W-summary"));
        assert!(block.contains("Supervisor view: S-summary"));
        assert!(block.contains("Observer view: O-summary"));
    }
}
