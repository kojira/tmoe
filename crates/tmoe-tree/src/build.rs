//! リポジトリ全体を走査して階層木を作る。

use crate::node::{NodeKind, SourceNode};
use crate::parse::{language_for_path, parse_file};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub root: PathBuf,
    pub max_files: usize,
    pub follow_links: bool,
    /// 除外するディレクトリ名の固定リスト。
    pub skip_dirs: Vec<String>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            max_files: 5_000,
            follow_links: false,
            skip_dirs: vec![
                "target".into(),
                "node_modules".into(),
                ".git".into(),
                "dist".into(),
                "build".into(),
                "__pycache__".into(),
            ],
        }
    }
}

pub fn build_repo_tree(opts: &BuildOptions) -> anyhow::Result<SourceNode> {
    let root = opts.root.clone();
    let mut file_nodes = Vec::new();
    let mut count = 0usize;
    for entry in WalkDir::new(&root)
        .follow_links(opts.follow_links)
        .into_iter()
        .filter_entry(|e| !is_skipped(e.path(), &opts.skip_dirs))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let lang = match language_for_path(path) {
            Some(l) => l,
            None => continue,
        };
        if count >= opts.max_files {
            break;
        }
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let parsed = parse_file(path, &src, lang)?;
        let rel = path
            .strip_prefix(&root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        let file_hash = blake3::hash(src.as_bytes()).to_hex().to_string();
        let line_count = src.lines().count() as u32;
        let file_node = SourceNode {
            id: ulid::Ulid::new().to_string(),
            kind: NodeKind::File,
            name: rel.clone(),
            path: rel.clone(),
            start_line: 1,
            end_line: line_count.max(1),
            children: parsed.roots,
            summary: format!("file {} ({} lines)", rel, line_count),
            content_hash: file_hash,
        };
        file_nodes.push(file_node);
        count += 1;
    }
    let combined_hash = combined_hash_of(&file_nodes);
    Ok(SourceNode {
        id: ulid::Ulid::new().to_string(),
        kind: NodeKind::Repo,
        name: root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into()),
        path: root.display().to_string(),
        start_line: 0,
        end_line: 0,
        children: file_nodes,
        summary: format!("repo at {} ({} files)", root.display(), count),
        content_hash: combined_hash,
    })
}

fn combined_hash_of(nodes: &[SourceNode]) -> String {
    let mut hasher = blake3::Hasher::new();
    for n in nodes {
        hasher.update(n.content_hash.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn is_skipped(path: &Path, skip: &[String]) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        if skip.iter().any(|s| s == name) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn build_finds_rust_functions_and_modules() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            r#"
pub mod util {
    pub fn add(a: i32, b: i32) -> i32 { a + b }
}

pub fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}
"#,
        )
        .unwrap();

        let opts = BuildOptions { root: root.clone(), ..Default::default() };
        let tree = build_repo_tree(&opts).unwrap();
        assert_eq!(tree.kind, NodeKind::Repo);
        assert_eq!(tree.children.len(), 1);
        let file = &tree.children[0];
        // ファイル直下に少なくとも mod_item と function_item の子が現れる。
        let kinds: Vec<NodeKind> = file.children.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&NodeKind::Module), "expected Module, got {kinds:?}");
        assert!(kinds.contains(&NodeKind::Function), "expected Function, got {kinds:?}");
    }

    #[test]
    fn build_picks_up_python_class_and_function() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(
            root.join("a.py"),
            r#"
class Foo:
    def bar(self):
        return 1

def baz():
    return 2
"#,
        )
        .unwrap();
        let opts = BuildOptions { root: root.clone(), ..Default::default() };
        let tree = build_repo_tree(&opts).unwrap();
        let file = &tree.children[0];
        let kinds: Vec<NodeKind> = file.children.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&NodeKind::Class));
        assert!(kinds.contains(&NodeKind::Function));
    }

    #[test]
    fn build_skips_target_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("target/x")).unwrap();
        fs::write(root.join("target/x/junk.rs"), "fn no() {}").unwrap();
        fs::write(root.join("ok.rs"), "fn ok() {}").unwrap();
        let opts = BuildOptions { root: root.clone(), ..Default::default() };
        let tree = build_repo_tree(&opts).unwrap();
        let names: Vec<String> = tree.children.iter().map(|c| c.name.clone()).collect();
        assert!(names.contains(&"ok.rs".to_string()));
        assert!(!names.iter().any(|n| n.contains("junk.rs")));
    }

    #[test]
    fn nodes_have_blake3_hashes() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(root.join("a.rs"), "fn a(){}\nfn b(){}").unwrap();
        let opts = BuildOptions { root: root.clone(), ..Default::default() };
        let tree = build_repo_tree(&opts).unwrap();
        let file = &tree.children[0];
        assert_eq!(file.content_hash.len(), 64);
        for c in &file.children {
            assert_eq!(c.content_hash.len(), 64);
        }
    }
}
