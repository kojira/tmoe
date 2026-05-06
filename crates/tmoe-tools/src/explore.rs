//! 探索系ツール: list_files (glob) / grep_text (line search).
//!
//! tmoe-rag (LLM 推論検索) の補完として、**愚直な文字列 / glob 一致** を
//! 高速に返す。Worker は「TODO を全部探す」「`**/*.rs` を列挙する」のような
//! 機械的な探索を tmoe-rag を経由せずにここで済ませられる。

use crate::permission::Permission;
use crate::tool::{Tool, ToolError, ToolOutput, ToolResult};
use crate::tools::{default_skip_dirs, join_within};
use async_trait::async_trait;
use globset::{Glob, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const DEFAULT_LIST_LIMIT: usize = 500;
const DEFAULT_GREP_LIMIT: usize = 200;
const GREP_MAX_LINE_BYTES: usize = 4096; // 1 ヒットあたり最大 4 KB に切り詰める

fn is_skipped_dir(path: &Path, skip: &[String]) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        if skip.iter().any(|s| s == name) {
            return true;
        }
    }
    false
}

// --- list_files -------------------------------------------------------

#[derive(Default)]
pub struct ListFilesTool {
    pub root: PathBuf,
}

#[derive(Deserialize)]
struct ListArgs {
    /// glob パターン (例: `**/*.rs`, `src/**/*.py`)。複数指定可 (OR)。
    /// 省略時は全ファイル列挙 (skip_dirs を除く)。
    #[serde(default)]
    patterns: Vec<String>,
    /// 単一パターンのショートカット。`patterns` と併用可。
    #[serde(default)]
    pattern: Option<String>,
    /// 結果の上限。
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Serialize)]
struct ListEntry {
    path: String,
    bytes: u64,
}

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &str { "list_files" }
    fn requires(&self) -> Permission { Permission::Read }

    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: ListArgs = serde_json::from_value(args.clone())?;
        let mut all_patterns = a.patterns;
        if let Some(p) = a.pattern {
            all_patterns.push(p);
        }
        let limit = a.limit.unwrap_or(DEFAULT_LIST_LIMIT);
        let skip = default_skip_dirs();

        let glob_set = if all_patterns.is_empty() {
            None
        } else {
            let mut b = GlobSetBuilder::new();
            for p in &all_patterns {
                let g = Glob::new(p)
                    .map_err(|e| ToolError::Args(format!("invalid glob {p:?}: {e}")))?;
                b.add(g);
            }
            Some(
                b.build()
                    .map_err(|e| ToolError::Args(format!("glob build: {e}")))?,
            )
        };

        let mut entries: Vec<ListEntry> = Vec::new();
        for entry in WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !is_skipped_dir(e.path(), &skip))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path())
                .to_path_buf();
            let rel_str = rel.to_string_lossy().to_string();
            if let Some(g) = &glob_set {
                if !g.is_match(&rel) && !g.is_match(rel_str.as_str()) {
                    continue;
                }
            }
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            entries.push(ListEntry { path: rel_str, bytes });
            if entries.len() >= limit {
                break;
            }
        }
        let stdout = entries
            .iter()
            .map(|e| format!("{} ({} bytes)", e.path, e.bytes))
            .collect::<Vec<_>>()
            .join("\n");
        let total = entries.len();
        let payload = serde_json::json!({
            "count": total,
            "limit": limit,
            "truncated": total >= limit,
            "entries": entries,
        });
        Ok(ToolOutput {
            stdout,
            data: Some(payload),
        })
    }
}

// --- grep_text --------------------------------------------------------

#[derive(Default)]
pub struct GrepTextTool {
    pub root: PathBuf,
}

#[derive(Deserialize)]
struct GrepArgs {
    /// 検索パターン (リテラル既定、`regex=true` で正規表現)。
    pattern: String,
    /// 検索範囲を限定するサブパス (相対)。省略時はリポ全体。
    #[serde(default)]
    path: Option<String>,
    /// 拡張子フィルタ (例: ["rs","md"])。省略時はテキストっぽい既定セット。
    #[serde(default)]
    extensions: Option<Vec<String>>,
    #[serde(default)]
    regex: bool,
    /// 大文字小文字を無視する (リテラル / regex 両方で有効)。
    #[serde(default)]
    case_insensitive: bool,
    /// 結果の上限 (ヒット行数)。
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Serialize)]
struct GrepHit {
    path: String,
    line: u32,
    text: String,
}

fn default_extensions() -> Vec<String> {
    vec![
        "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "kt", "swift", "c", "h", "cpp", "hpp",
        "cc", "cs", "rb", "php", "scala", "ex", "exs", "ml", "mli", "hs", "clj", "lua", "sh",
        "bash", "zsh", "fish", "toml", "yaml", "yml", "json", "xml", "html", "css", "md", "txt",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn extension_allowed(path: &Path, allow: &[String]) -> bool {
    match path.extension().and_then(|s| s.to_str()) {
        Some(e) => allow.iter().any(|a| a.eq_ignore_ascii_case(e)),
        None => false,
    }
}

#[async_trait]
impl Tool for GrepTextTool {
    fn name(&self) -> &str { "grep_text" }
    fn requires(&self) -> Permission { Permission::Read }

    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: GrepArgs = serde_json::from_value(args.clone())?;
        if a.pattern.is_empty() {
            return Err(ToolError::Args("grep_text: pattern must not be empty".into()));
        }
        let limit = a.limit.unwrap_or(DEFAULT_GREP_LIMIT);
        let skip = default_skip_dirs();
        let exts = a.extensions.unwrap_or_else(default_extensions);

        let regex = if a.regex {
            let mut b = regex::RegexBuilder::new(&a.pattern);
            b.case_insensitive(a.case_insensitive);
            Some(
                b.build()
                    .map_err(|e| ToolError::Args(format!("invalid regex: {e}")))?,
            )
        } else {
            None
        };

        let needle_lower = if a.case_insensitive {
            Some(a.pattern.to_lowercase())
        } else {
            None
        };

        let scope_root: PathBuf = if let Some(sub) = a.path.as_ref() {
            join_within(&self.root, sub)?
        } else {
            self.root.clone()
        };
        if !scope_root.exists() {
            return Err(ToolError::Args(format!(
                "grep_text: path not found: {}",
                a.path.unwrap_or_default()
            )));
        }

        let mut hits: Vec<GrepHit> = Vec::new();
        let walker = WalkDir::new(&scope_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !is_skipped_dir(e.path(), &skip));
        'outer: for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            if !extension_allowed(entry.path(), &exts) {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            let body = match std::fs::read_to_string(entry.path()) {
                Ok(s) => s,
                Err(_) => continue, // バイナリ等は静かに skip
            };
            for (idx, line) in body.lines().enumerate() {
                let matched = if let Some(re) = &regex {
                    re.is_match(line)
                } else if let Some(nl) = &needle_lower {
                    line.to_lowercase().contains(nl)
                } else {
                    line.contains(&a.pattern)
                };
                if matched {
                    let snippet = if line.len() > GREP_MAX_LINE_BYTES {
                        format!("{}…", &line[..GREP_MAX_LINE_BYTES])
                    } else {
                        line.to_string()
                    };
                    hits.push(GrepHit {
                        path: rel.clone(),
                        line: (idx as u32) + 1,
                        text: snippet,
                    });
                    if hits.len() >= limit {
                        break 'outer;
                    }
                }
            }
        }

        let stdout = hits
            .iter()
            .map(|h| format!("{}:{}: {}", h.path, h.line, h.text))
            .collect::<Vec<_>>()
            .join("\n");
        let count = hits.len();
        let payload = serde_json::json!({
            "count": count,
            "limit": limit,
            "truncated": count >= limit,
            "hits": hits,
        });
        Ok(ToolOutput { stdout, data: Some(payload) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionProfile;
    use crate::registry::ToolRegistry;
    use crate::tool::ToolCall;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn fixture() -> tempfile::TempDir {
        let d = tempdir().unwrap();
        let p = d.path();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::create_dir_all(p.join("tests")).unwrap();
        std::fs::create_dir_all(p.join("target/junk")).unwrap();
        std::fs::write(p.join("src/lib.rs"), "// TODO refactor\nfn main() {}\n").unwrap();
        std::fs::write(p.join("src/util.rs"), "fn util() {}\n").unwrap();
        std::fs::write(p.join("tests/it.rs"), "// TODO write tests\n").unwrap();
        std::fs::write(p.join("README.md"), "# tmoe\nTODO maintain\n").unwrap();
        std::fs::write(p.join("target/junk/garbage.rs"), "// TODO ignore me\n").unwrap();
        d
    }

    #[tokio::test]
    async fn list_files_globs_rust_sources_and_skips_target() {
        let d = fixture();
        let root = d.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ListFilesTool { root }));
        let out = reg
            .invoke(
                &ToolCall {
                    name: "list_files".into(),
                    args: serde_json::json!({"pattern": "**/*.rs"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(out.stdout.contains("src/lib.rs"));
        assert!(out.stdout.contains("src/util.rs"));
        assert!(out.stdout.contains("tests/it.rs"));
        assert!(
            !out.stdout.contains("garbage.rs"),
            "target/ should be skipped: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn list_files_no_pattern_lists_all() {
        let d = fixture();
        let root = d.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ListFilesTool { root }));
        let out = reg
            .invoke(
                &ToolCall {
                    name: "list_files".into(),
                    args: serde_json::json!({}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(out.stdout.contains("README.md"));
        assert!(out.stdout.contains("src/lib.rs"));
    }

    #[tokio::test]
    async fn list_files_invalid_glob_is_args_error() {
        let d = fixture();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(ListFilesTool { root: d.path().to_path_buf() }));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "list_files".into(),
                    args: serde_json::json!({"pattern": "[unclosed"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Args(_)));
    }

    #[tokio::test]
    async fn grep_text_finds_todo_across_files_skipping_target() {
        let d = fixture();
        let root = d.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(GrepTextTool { root }));
        let out = reg
            .invoke(
                &ToolCall {
                    name: "grep_text".into(),
                    args: serde_json::json!({"pattern": "TODO"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(out.stdout.contains("src/lib.rs:1:"));
        assert!(out.stdout.contains("tests/it.rs:1:"));
        assert!(out.stdout.contains("README.md:2:"));
        assert!(
            !out.stdout.contains("garbage.rs"),
            "target/ should be skipped: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn grep_text_regex_mode() {
        let d = fixture();
        let root = d.path().to_path_buf();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(GrepTextTool { root }));
        let out = reg
            .invoke(
                &ToolCall {
                    name: "grep_text".into(),
                    args: serde_json::json!({
                        "pattern": "fn\\s+\\w+",
                        "regex": true,
                    }),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(out.stdout.contains("fn main()"));
        assert!(out.stdout.contains("fn util()"));
    }

    #[tokio::test]
    async fn grep_text_invalid_regex_is_args_error() {
        let d = fixture();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(GrepTextTool { root: d.path().to_path_buf() }));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "grep_text".into(),
                    args: serde_json::json!({"pattern": "(", "regex": true}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Args(_)));
    }

    #[tokio::test]
    async fn grep_text_case_insensitive() {
        let d = fixture();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(GrepTextTool { root: d.path().to_path_buf() }));
        let out = reg
            .invoke(
                &ToolCall {
                    name: "grep_text".into(),
                    args: serde_json::json!({
                        "pattern": "todo",
                        "case_insensitive": true,
                    }),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(out.stdout.contains("src/lib.rs:1:"));
    }

    #[tokio::test]
    async fn grep_text_scoped_path() {
        let d = fixture();
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(GrepTextTool { root: d.path().to_path_buf() }));
        let out = reg
            .invoke(
                &ToolCall {
                    name: "grep_text".into(),
                    args: serde_json::json!({"pattern": "TODO", "path": "tests"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(out.stdout.contains("tests/it.rs:1:"));
        assert!(!out.stdout.contains("src/lib.rs:"));
    }
}
