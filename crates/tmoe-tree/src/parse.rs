//! 言語別のパーサ抽象。Rust / Python を最小サポートし、必要に応じて拡張する。

use crate::node::{NodeKind, SourceNode};
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageId {
    Rust,
    Python,
}

impl LanguageId {
    pub fn as_str(&self) -> &'static str {
        match self {
            LanguageId::Rust => "rust",
            LanguageId::Python => "python",
        }
    }
}

pub fn available_languages() -> Vec<LanguageId> {
    vec![LanguageId::Rust, LanguageId::Python]
}

pub fn language_for_path(path: &Path) -> Option<LanguageId> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("rs") => Some(LanguageId::Rust),
        Some("py") => Some(LanguageId::Python),
        _ => None,
    }
}

pub struct LanguageOps {
    pub id: LanguageId,
    pub language: Language,
    pub function_kinds: &'static [&'static str],
    pub module_kinds: &'static [&'static str],
    pub class_kinds: &'static [&'static str],
}

impl LanguageOps {
    pub fn for_language(id: LanguageId) -> Self {
        match id {
            LanguageId::Rust => Self {
                id,
                language: tree_sitter_rust::LANGUAGE.into(),
                function_kinds: &["function_item"],
                module_kinds: &["mod_item"],
                class_kinds: &["struct_item", "enum_item", "trait_item", "impl_item"],
            },
            LanguageId::Python => Self {
                id,
                language: tree_sitter_python::LANGUAGE.into(),
                function_kinds: &["function_definition"],
                module_kinds: &[],
                class_kinds: &["class_definition"],
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParseOutput {
    pub roots: Vec<SourceNode>,
}

/// 1 ファイルから子ノード (関数・モジュール・クラス) を抽出する。
pub fn parse_file(path: &Path, src: &str, lang_id: LanguageId) -> anyhow::Result<ParseOutput> {
    let ops = LanguageOps::for_language(lang_id);
    let mut parser = Parser::new();
    parser
        .set_language(&ops.language)
        .map_err(|e| anyhow::anyhow!("set_language failed: {e}"))?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("parse returned None"))?;
    let mut nodes = Vec::new();
    walk(&ops, tree.root_node(), src, path, &mut nodes);
    Ok(ParseOutput { roots: nodes })
}

fn walk(ops: &LanguageOps, node: Node<'_>, src: &str, path: &Path, out: &mut Vec<SourceNode>) {
    let kind_str = node.kind();
    let kind = if ops.function_kinds.contains(&kind_str) {
        Some(NodeKind::Function)
    } else if ops.module_kinds.contains(&kind_str) {
        Some(NodeKind::Module)
    } else if ops.class_kinds.contains(&kind_str) {
        Some(NodeKind::Class)
    } else {
        None
    };
    if let Some(kind) = kind {
        let name = identifier_of(node, src).unwrap_or_else(|| anonymous_name(kind_str));
        let span = node.byte_range();
        let body = &src[span.clone()];
        let hash = blake3::hash(body.as_bytes()).to_hex().to_string();
        let mut children = Vec::new();
        for i in 0..node.child_count() {
            if let Some(c) = node.child(i) {
                walk(ops, c, src, path, &mut children);
            }
        }
        out.push(SourceNode {
            id: ulid::Ulid::new().to_string(),
            kind,
            name: name.clone(),
            path: path.display().to_string(),
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            children,
            summary: format!(
                "{} {} @ {}:{}-{}",
                kind_str,
                name,
                path.display(),
                node.start_position().row + 1,
                node.end_position().row + 1
            ),
            content_hash: hash,
        });
        return;
    }
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i) {
            walk(ops, c, src, path, out);
        }
    }
}

fn identifier_of(node: Node<'_>, src: &str) -> Option<String> {
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i) {
            if matches!(c.kind(), "identifier" | "type_identifier" | "name") {
                return Some(src[c.byte_range()].to_string());
            }
        }
    }
    None
}

fn anonymous_name(kind: &str) -> String {
    format!("<anonymous {kind}>")
}
