//! tmoe-tree: PageIndex 概念のソース版。
//!
//! tree-sitter で複数言語のソースをパースし、File / Module / Class / Function 階層の
//! 統一表現 `SourceNode` を構築する。各ノードに LLM 要約と content_hash を持たせ、
//! ベクトル類似度を使わないエージェンティック木探索検索の対象にする。
