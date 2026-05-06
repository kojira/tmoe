use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Repo,
    File,
    Module,
    Class,
    Function,
}

pub type NodeId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub children: Vec<SourceNode>,
    /// LLM 要約。Phase 5 時点では決定的なフォールバック要約を入れる (Phase 後半で LLM 化)。
    pub summary: String,
    /// 本文 (このノードに対応するソース範囲) の BLAKE3 ハッシュ。
    pub content_hash: String,
}
