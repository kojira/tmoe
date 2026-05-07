//! tmoe-tree: PageIndex 概念のソース版。
//!
//! tree-sitter で複数言語のソースをパースし、Repo / File / Module / Function 階層の
//! 統一表現 `SourceNode` を構築する。各ノードに content_hash (BLAKE3) を持たせ、
//! ベクトル類似度を使わないエージェンティック木探索検索 (tmoe-rag) の対象にする。

pub mod build;
pub mod enrich;
pub mod node;
pub mod parse;

pub use build::{build_repo_tree, BuildOptions};
pub use enrich::{enrich_summaries, EnrichOptions, InMemorySummaryCache, SummaryCache};
pub use node::{NodeId, NodeKind, SourceNode};
pub use parse::{
    available_languages, language_for_path, LanguageId, LanguageOps, ParseOutput,
};
