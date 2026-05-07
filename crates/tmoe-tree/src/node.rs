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
    /// 要約。`build_repo_tree` は決定的な構造的要約 (kind + name + path + 公開子の名前リスト) を入れる。
    /// 本物の LLM 駆動要約は `enrich_summaries_with_llm` (opt-in) でこのフィールドを上書きできる。
    /// rag::search はこの文字列を navigate 判断材料に使うので、空でなければ何でも良い。
    pub summary: String,
    /// 本文 (このノードに対応するソース範囲) の BLAKE3 ハッシュ。
    pub content_hash: String,
}
