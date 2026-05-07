//! `apply_patch` ツール: 複数ファイルを 1 リクエストで編集できる包括的な diff フォーマット。
//!
//! opencode (sst/opencode) と互換のテキスト構文を採用し、Worker が 1 度のツール呼び出しで
//! 複数ファイル ([Add | Update | Delete | Move]) を一気に変更できるようにする。
//! 単一ファイル + 単一置換で済むときは `patch_file`、新規作成なら `edit_file` の方が
//! 安価。`apply_patch` は **構造変更 (リネーム / 同時複数ファイル更新 / 既存削除)**
//! を 1 アトムにまとめたいときに使う。
//!
//! # 構文
//!
//! ```text
//! *** Begin Patch
//! *** Add File: hello.txt
//! +Hello world
//! *** Update File: src/app.py
//! *** Move to: src/main.py
//! @@ def greet():
//! -print("Hi")
//! +print("Hello, world!")
//! *** Delete File: obsolete.txt
//! *** End Patch
//! ```
//!
//! - `*** Begin Patch` / `*** End Patch` でエンベロープ
//! - 各ファイルセクションは `*** Add File:` / `*** Update File:` / `*** Delete File:` のいずれか
//! - `Update File` の直後に `*** Move to: <new path>` を 1 行置けばリネーム + 編集
//! - チャンクは `@@ <context>` で始まり、` ` (保持) / `-` (削除) / `+` (追加) 行が続く
//! - `*** End of File` でファイル終端の anchor を示せる (チャンクが末尾に貼られる)
//!
//! # マッチング
//!
//! Worker が貼ってくる古い行は厳密一致ではマッチしないことが多い (空白の崩れ・
//! Unicode のクオートが化ける等)。そこで段階的にゆるめながら前方探索する:
//!   1. 完全一致
//!   2. `trim_end` 後の一致 (末尾の trailing space/CR を吸収)
//!   3. `trim` 後の一致 (前後の whitespace を吸収)
//! いずれもダメなら `ApplyPatchError::ChunkMismatch` で reject。LLM に書き直させる方が
//! ファイル内容を破壊するより安全。

use crate::permission::Permission;
use crate::tool::{Tool, ToolError, ToolOutput, ToolResult};
use crate::tools::join_within;
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    Add { path: String, contents: String },
    Delete { path: String },
    Update { path: String, move_path: Option<String>, chunks: Vec<UpdateChunk> },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateChunk {
    /// `@@ ...` の後の "context" 文字列 (関数シグネチャ等)。前方検索のヒント。
    pub change_context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// `*** End of File` が付いていれば true。EOF アンカーで末尾優先マッチに切り替える。
    pub is_end_of_file: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyPatchError {
    #[error("missing *** Begin Patch / *** End Patch envelope")]
    MissingEnvelope,
    #[error("empty patch (no hunks between markers)")]
    EmptyPatch,
    #[error("invalid header at line {line}: {raw}")]
    InvalidHeader { line: usize, raw: String },
    #[error("chunk could not be located in {path}: looking for `{first}`")]
    ChunkMismatch { path: String, first: String },
    #[error("file not found for update/delete: {path}")]
    FileMissing { path: String },
    #[error("file already exists for add: {path}")]
    FileExists { path: String },
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// パッチテキストを Hunk 列に分解する。ファイル I/O はしない。
///
/// パース時点で「このパッチ自体が文法的に通るか」だけを判定する。`Update` チャンクの
/// `old_lines` が実際の現状と一致するかどうかは [`apply_hunks`] の側で確認する。
pub fn parse_patch(patch_text: &str) -> Result<Vec<Hunk>, ApplyPatchError> {
    let cleaned = patch_text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = cleaned.lines().collect();
    let begin = lines
        .iter()
        .position(|l| l.trim() == "*** Begin Patch")
        .ok_or(ApplyPatchError::MissingEnvelope)?;
    let end = lines
        .iter()
        .rposition(|l| l.trim() == "*** End Patch")
        .ok_or(ApplyPatchError::MissingEnvelope)?;
    if end <= begin {
        return Err(ApplyPatchError::MissingEnvelope);
    }

    let mut hunks = Vec::new();
    let mut i = begin + 1;
    while i < end {
        let raw = lines[i];
        if let Some(rest) = raw.strip_prefix("*** Add File:") {
            let path = rest.trim().to_string();
            i += 1;
            let mut content = String::new();
            while i < end && !lines[i].starts_with("***") {
                if let Some(rest) = lines[i].strip_prefix('+') {
                    content.push_str(rest);
                    content.push('\n');
                }
                i += 1;
            }
            // 末尾改行は付けない (apply 側で書き出すときに付ける)。
            if content.ends_with('\n') {
                content.pop();
            }
            hunks.push(Hunk::Add { path, contents: content });
        } else if let Some(rest) = raw.strip_prefix("*** Delete File:") {
            let path = rest.trim().to_string();
            hunks.push(Hunk::Delete { path });
            i += 1;
        } else if let Some(rest) = raw.strip_prefix("*** Update File:") {
            let path = rest.trim().to_string();
            i += 1;
            let mut move_path: Option<String> = None;
            if i < end {
                if let Some(rest) = lines[i].strip_prefix("*** Move to:") {
                    move_path = Some(rest.trim().to_string());
                    i += 1;
                }
            }
            let mut chunks: Vec<UpdateChunk> = Vec::new();
            while i < end && !lines[i].starts_with("*** Add File:")
                && !lines[i].starts_with("*** Delete File:")
                && !lines[i].starts_with("*** Update File:")
            {
                if let Some(ctx) = lines[i].strip_prefix("@@") {
                    let change_context = {
                        let s = ctx.trim();
                        if s.is_empty() { None } else { Some(s.to_string()) }
                    };
                    i += 1;
                    let mut old_lines = Vec::new();
                    let mut new_lines = Vec::new();
                    let mut eof = false;
                    while i < end
                        && !lines[i].starts_with("@@")
                        && !lines[i].starts_with("*** Add File:")
                        && !lines[i].starts_with("*** Delete File:")
                        && !lines[i].starts_with("*** Update File:")
                    {
                        let l = lines[i];
                        if l == "*** End of File" {
                            eof = true;
                            i += 1;
                            break;
                        }
                        if let Some(rest) = l.strip_prefix(' ') {
                            old_lines.push(rest.to_string());
                            new_lines.push(rest.to_string());
                        } else if let Some(rest) = l.strip_prefix('-') {
                            old_lines.push(rest.to_string());
                        } else if let Some(rest) = l.strip_prefix('+') {
                            new_lines.push(rest.to_string());
                        } else if l.is_empty() {
                            // bare empty line: keep as both old/new (LLM が ` ` プレフィクスを
                            // 落としがちなので寛容に解釈)。
                            old_lines.push(String::new());
                            new_lines.push(String::new());
                        } else {
                            // 何もプレフィクスがなく空でもない行は context 行とみなす。
                            old_lines.push(l.to_string());
                            new_lines.push(l.to_string());
                        }
                        i += 1;
                    }
                    chunks.push(UpdateChunk {
                        change_context,
                        old_lines,
                        new_lines,
                        is_end_of_file: eof,
                    });
                } else {
                    // 未知の行: スキップ。`@@` の前に空行が来てもよい。
                    i += 1;
                }
            }
            hunks.push(Hunk::Update { path, move_path, chunks });
        } else {
            // ヘッダ以外がエンベロープ内に直接出るのは異常 (Add の +行 のような
            // セクション内コンテキストではない)。空行は無視。
            if !raw.trim().is_empty() {
                return Err(ApplyPatchError::InvalidHeader {
                    line: i + 1,
                    raw: raw.to_string(),
                });
            }
            i += 1;
        }
    }

    if hunks.is_empty() {
        return Err(ApplyPatchError::EmptyPatch);
    }
    Ok(hunks)
}

/// `root` 直下の (相対) パスに対して Hunk を順に適用する。worktree からの脱出は弾く。
///
/// 戻り値は実際に発生した変更のサマリ (`A path` / `M path` / `D path`)。
pub fn apply_hunks(root: &Path, hunks: &[Hunk]) -> Result<Vec<String>, ApplyPatchError> {
    let mut summary = Vec::new();
    for h in hunks {
        match h {
            Hunk::Add { path, contents } => {
                let abs = join_safe(root, path)?;
                if abs.exists() {
                    return Err(ApplyPatchError::FileExists { path: path.clone() });
                }
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| ApplyPatchError::Io {
                        path: parent.display().to_string(),
                        source: e,
                    })?;
                }
                let mut body = contents.clone();
                if !body.ends_with('\n') {
                    body.push('\n');
                }
                std::fs::write(&abs, body).map_err(|e| ApplyPatchError::Io {
                    path: path.clone(),
                    source: e,
                })?;
                summary.push(format!("A {path}"));
            }
            Hunk::Delete { path } => {
                let abs = join_safe(root, path)?;
                if !abs.exists() {
                    return Err(ApplyPatchError::FileMissing { path: path.clone() });
                }
                std::fs::remove_file(&abs).map_err(|e| ApplyPatchError::Io {
                    path: path.clone(),
                    source: e,
                })?;
                summary.push(format!("D {path}"));
            }
            Hunk::Update { path, move_path, chunks } => {
                let abs = join_safe(root, path)?;
                if !abs.exists() {
                    return Err(ApplyPatchError::FileMissing { path: path.clone() });
                }
                let original = std::fs::read_to_string(&abs).map_err(|e| ApplyPatchError::Io {
                    path: path.clone(),
                    source: e,
                })?;
                let new_text = apply_chunks_to_text(&original, chunks, path)?;
                let target = if let Some(mp) = move_path {
                    let mp_abs = join_safe(root, mp)?;
                    if let Some(parent) = mp_abs.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| ApplyPatchError::Io {
                            path: parent.display().to_string(),
                            source: e,
                        })?;
                    }
                    std::fs::write(&mp_abs, &new_text).map_err(|e| ApplyPatchError::Io {
                        path: mp.clone(),
                        source: e,
                    })?;
                    std::fs::remove_file(&abs).map_err(|e| ApplyPatchError::Io {
                        path: path.clone(),
                        source: e,
                    })?;
                    summary.push(format!("M {path} -> {mp}"));
                    mp_abs
                } else {
                    std::fs::write(&abs, &new_text).map_err(|e| ApplyPatchError::Io {
                        path: path.clone(),
                        source: e,
                    })?;
                    summary.push(format!("M {path}"));
                    abs
                };
                let _ = target; // (formatter 連携などは別フェーズ)
            }
        }
    }
    Ok(summary)
}

/// `root` の外に出ようとするパスを reject しつつ join する。
fn join_safe(root: &Path, rel: &str) -> Result<PathBuf, ApplyPatchError> {
    join_within(root, rel).map_err(|e| match e {
        ToolError::Args(m) => ApplyPatchError::InvalidHeader { line: 0, raw: m },
        other => ApplyPatchError::InvalidHeader {
            line: 0,
            raw: format!("{other:?}"),
        },
    })
}

/// 1 ファイル分の chunks を original テキストに対して適用し、新しいテキストを返す。
fn apply_chunks_to_text(
    original: &str,
    chunks: &[UpdateChunk],
    path: &str,
) -> Result<String, ApplyPatchError> {
    let had_trailing_newline = original.ends_with('\n');
    let mut lines: Vec<String> = original.lines().map(|s| s.to_string()).collect();
    let mut cursor: usize = 0;

    // (start, old_len, new_lines) を後段で逆順 splice。
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();

    for chunk in chunks {
        let mut search_from = cursor;
        if let Some(ctx) = &chunk.change_context {
            if let Some(pos) = seek_one(&lines, ctx, search_from) {
                search_from = pos + 1;
            }
            // context が見つからない場合は cursor を進めずに本パターン側で再探索する。
        }

        if chunk.old_lines.is_empty() {
            // 純粋追加: ファイル末尾 (もしくは current cursor) に挿入。
            let at = if chunk.is_end_of_file {
                lines.len()
            } else {
                search_from.min(lines.len())
            };
            replacements.push((at, 0, chunk.new_lines.clone()));
            cursor = at;
            continue;
        }

        let pat = chunk.old_lines.as_slice();
        let found = if chunk.is_end_of_file {
            seek_eof(&lines, pat).or_else(|| seek_seq(&lines, pat, search_from))
        } else {
            seek_seq(&lines, pat, search_from)
        };

        let Some(pos) = found else {
            return Err(ApplyPatchError::ChunkMismatch {
                path: path.to_string(),
                first: pat.first().cloned().unwrap_or_default(),
            });
        };

        replacements.push((pos, pat.len(), chunk.new_lines.clone()));
        cursor = pos + pat.len();
    }

    // 後ろから splice。chunks 自体は前から並んでいる前提。
    replacements.sort_by_key(|(p, _, _)| *p);
    for (start, old_len, new_segment) in replacements.into_iter().rev() {
        let end = start + old_len;
        let end = end.min(lines.len());
        let start = start.min(end);
        lines.splice(start..end, new_segment);
    }

    let mut joined = lines.join("\n");
    if had_trailing_newline {
        joined.push('\n');
    }
    Ok(joined)
}

/// 段階的に緩めながら lines 内で pattern と一致する開始位置を返す。
fn seek_seq(lines: &[String], pattern: &[String], start: usize) -> Option<usize> {
    if pattern.is_empty() || start > lines.len() {
        return None;
    }
    // pass 1: exact
    if let Some(p) = scan(lines, pattern, start, |a, b| a == b) {
        return Some(p);
    }
    // pass 2: rstrip
    if let Some(p) = scan(lines, pattern, start, |a, b| a.trim_end() == b.trim_end()) {
        return Some(p);
    }
    // pass 3: trim both sides
    if let Some(p) = scan(lines, pattern, start, |a, b| a.trim() == b.trim()) {
        return Some(p);
    }
    None
}

fn seek_eof(lines: &[String], pattern: &[String]) -> Option<usize> {
    if pattern.is_empty() || pattern.len() > lines.len() {
        return None;
    }
    let from_end = lines.len() - pattern.len();
    let exact = pattern
        .iter()
        .enumerate()
        .all(|(i, p)| lines[from_end + i] == *p);
    if exact {
        return Some(from_end);
    }
    let trimmed = pattern
        .iter()
        .enumerate()
        .all(|(i, p)| lines[from_end + i].trim() == p.trim());
    if trimmed {
        return Some(from_end);
    }
    None
}

fn scan<F>(lines: &[String], pattern: &[String], start: usize, eq: F) -> Option<usize>
where
    F: Fn(&str, &str) -> bool,
{
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len().saturating_sub(pattern.len());
    for i in start..=last {
        let mut ok = true;
        for j in 0..pattern.len() {
            if !eq(&lines[i + j], &pattern[j]) {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    None
}

fn seek_one(lines: &[String], needle: &str, start: usize) -> Option<usize> {
    let n = needle.trim();
    for (i, l) in lines.iter().enumerate().skip(start) {
        if l.trim() == n {
            return Some(i);
        }
    }
    None
}

// --- ツール本体 -------------------------------------------------------------

#[derive(Deserialize)]
struct ApplyPatchArgs {
    #[serde(alias = "patchText", alias = "patch_text", alias = "patch")]
    text: String,
}

pub struct ApplyPatchTool {
    pub root: PathBuf,
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn requires(&self) -> Permission {
        Permission::Write
    }
    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: ApplyPatchArgs = serde_json::from_value(args.clone())
            .map_err(|e| ToolError::Args(format!("apply_patch args: {e}")))?;
        let hunks = parse_patch(&a.text)
            .map_err(|e| ToolError::Args(format!("apply_patch parse: {e}")))?;
        let summary = apply_hunks(&self.root, &hunks)
            .map_err(|e| ToolError::Args(format!("apply_patch apply: {e}")))?;
        let body = format!(
            "Success. Applied {n} hunk(s):\n{lines}",
            n = hunks.len(),
            lines = summary.join("\n")
        );
        Ok(ToolOutput::text(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_simple_add_file() {
        let txt = "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n*** End Patch\n";
        let h = parse_patch(txt).unwrap();
        assert_eq!(h.len(), 1);
        match &h[0] {
            Hunk::Add { path, contents } => {
                assert_eq!(path, "hello.txt");
                assert_eq!(contents, "Hello world");
            }
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn parse_update_with_chunk_and_move() {
        let txt = "*** Begin Patch\n\
                   *** Update File: src/app.py\n\
                   *** Move to: src/main.py\n\
                   @@ def greet():\n\
                   -    print(\"Hi\")\n\
                   +    print(\"Hello, world!\")\n\
                   *** End Patch\n";
        let h = parse_patch(txt).unwrap();
        assert_eq!(h.len(), 1);
        match &h[0] {
            Hunk::Update { path, move_path, chunks } => {
                assert_eq!(path, "src/app.py");
                assert_eq!(move_path.as_deref(), Some("src/main.py"));
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].change_context.as_deref(), Some("def greet():"));
                assert_eq!(chunks[0].old_lines, vec!["    print(\"Hi\")"]);
                assert_eq!(chunks[0].new_lines, vec!["    print(\"Hello, world!\")"]);
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn parse_delete_file() {
        let txt = "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n";
        let h = parse_patch(txt).unwrap();
        assert_eq!(h, vec![Hunk::Delete { path: "gone.txt".into() }]);
    }

    #[test]
    fn parse_rejects_missing_envelope() {
        let txt = "*** Add File: x\n+y\n";
        assert!(matches!(
            parse_patch(txt).unwrap_err(),
            ApplyPatchError::MissingEnvelope
        ));
    }

    #[test]
    fn parse_rejects_empty_patch() {
        let txt = "*** Begin Patch\n*** End Patch\n";
        assert!(matches!(
            parse_patch(txt).unwrap_err(),
            ApplyPatchError::EmptyPatch
        ));
    }

    #[test]
    fn apply_creates_file_with_directory() {
        let d = tempdir().unwrap();
        let txt = "*** Begin Patch\n*** Add File: src/new/hello.txt\n+abc\n+def\n*** End Patch\n";
        let h = parse_patch(txt).unwrap();
        let summary = apply_hunks(d.path(), &h).unwrap();
        assert_eq!(summary, vec!["A src/new/hello.txt"]);
        let body = std::fs::read_to_string(d.path().join("src/new/hello.txt")).unwrap();
        assert_eq!(body, "abc\ndef\n");
    }

    #[test]
    fn apply_update_replaces_matching_block() {
        let d = tempdir().unwrap();
        let original = "alpha\n    print(\"Hi\")\ngamma\n";
        std::fs::write(d.path().join("a.py"), original).unwrap();
        let txt = "*** Begin Patch\n\
                   *** Update File: a.py\n\
                   @@\n\
                   -    print(\"Hi\")\n\
                   +    print(\"Hello\")\n\
                   *** End Patch\n";
        let h = parse_patch(txt).unwrap();
        apply_hunks(d.path(), &h).unwrap();
        let body = std::fs::read_to_string(d.path().join("a.py")).unwrap();
        assert_eq!(body, "alpha\n    print(\"Hello\")\ngamma\n");
    }

    #[test]
    fn apply_update_then_move() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("old.txt"), "one\ntwo\nthree\n").unwrap();
        let txt = "*** Begin Patch\n\
                   *** Update File: old.txt\n\
                   *** Move to: new.txt\n\
                   @@\n\
                   -two\n\
                   +TWO\n\
                   *** End Patch\n";
        let h = parse_patch(txt).unwrap();
        apply_hunks(d.path(), &h).unwrap();
        assert!(!d.path().join("old.txt").exists());
        let body = std::fs::read_to_string(d.path().join("new.txt")).unwrap();
        assert_eq!(body, "one\nTWO\nthree\n");
    }

    #[test]
    fn apply_delete_removes_file() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("g.txt"), "x\n").unwrap();
        let txt = "*** Begin Patch\n*** Delete File: g.txt\n*** End Patch\n";
        let h = parse_patch(txt).unwrap();
        apply_hunks(d.path(), &h).unwrap();
        assert!(!d.path().join("g.txt").exists());
    }

    #[test]
    fn apply_rejects_missing_update_target() {
        let d = tempdir().unwrap();
        let txt = "*** Begin Patch\n*** Update File: nope.txt\n@@\n-a\n+b\n*** End Patch\n";
        let h = parse_patch(txt).unwrap();
        assert!(matches!(
            apply_hunks(d.path(), &h).unwrap_err(),
            ApplyPatchError::FileMissing { .. }
        ));
    }

    #[test]
    fn apply_rejects_chunk_mismatch() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let txt = "*** Begin Patch\n*** Update File: a.txt\n@@\n-this-is-not-there\n+nope\n*** End Patch\n";
        let h = parse_patch(txt).unwrap();
        assert!(matches!(
            apply_hunks(d.path(), &h).unwrap_err(),
            ApplyPatchError::ChunkMismatch { .. }
        ));
    }

    #[test]
    fn apply_path_escape_blocked() {
        let d = tempdir().unwrap();
        let txt = "*** Begin Patch\n*** Add File: ../escape.txt\n+x\n*** End Patch\n";
        let h = parse_patch(txt).unwrap();
        assert!(apply_hunks(d.path(), &h).is_err());
    }

    #[tokio::test]
    async fn tool_call_round_trip_succeeds() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("greet.txt"), "Hi\n").unwrap();
        let tool = ApplyPatchTool { root: d.path().to_path_buf() };
        let patch = "*** Begin Patch\n\
                     *** Add File: README.md\n\
                     +# tmoe\n\
                     *** Update File: greet.txt\n\
                     @@\n\
                     -Hi\n\
                     +Hello\n\
                     *** End Patch\n";
        let out = tool
            .call(&serde_json::json!({"text": patch}))
            .await
            .expect("apply ok");
        assert!(out.stdout.contains("A README.md"));
        assert!(out.stdout.contains("M greet.txt"));
        let g = std::fs::read_to_string(d.path().join("greet.txt")).unwrap();
        assert_eq!(g, "Hello\n");
    }
}
