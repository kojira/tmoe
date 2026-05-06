//! tmoe-history: PageIndex 思想を会話に適用した階層履歴。
//!
//! 共通 raw ツリー 1 本 + エージェント別要約 index 3 本 (Worker / Supervisor / Observer) の
//! 四層構造で持つ。中立要約は作らない (三角形哲学の否定になるため)。
//!
//! コンパクションは閾値到達時の一括処理ではなく、ターン追加ごとに各エージェントが
//! 自分の view を 1 ノードぶん延伸する逐次方式 (incremental rolling summary)。

pub mod compaction;
pub mod error;
pub mod store;
pub mod types;

pub use compaction::{compact_turn_for_all, rollup_one_level, AgentLens, LabeledLens};
pub use error::{HistoryError, Result};
pub use store::{AppendRaw, AppendSummary, HistoryStore};
pub use types::{
    AgentSummaryNode, AgentView, Feature, FeatureStatus, RawKind, RawNode,
};
