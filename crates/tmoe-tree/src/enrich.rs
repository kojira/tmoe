//! 木の各ノードの `summary` を LLM 駆動の content 要約に書き換える (opt-in)。
//!
//! `build_repo_tree` が出力する summary は決定的な構造的メタデータ
//! (kind + name + 子の名前リスト) で、rag::search の navigate-LLM が降りる先を
//! 決めるには十分機能するが、DESIGN.md は「ボトムアップで Worker LLM に要約させて
//! 木を完成」を約束していた。本モジュールはその約束を「使うときだけコストを払う」形で
//! 実装する:
//!
//! - 呼び出し側は `enrich_summaries(tree, llm, ctx)` を必要なときだけ呼ぶ
//! - Function / Class / Module は **そのノードの本文 (path から read して span 切り出し)** を
//!   1 文要約させる
//! - File / Repo はその時点の子の summary を集約 (子から先に enrich される)
//! - 同じ `content_hash` を 2 度要約しないよう、`SummaryCache` (in-process) に乗せる
//! - LLM 失敗時は元の構造的 summary をそのまま残す (= 縮退、破壊しない)
//!
//! 既定の `SummaryCache` は `tokio::sync::Mutex<HashMap<String, String>>` 1 個。
//! 永続化が欲しければ rusqlite ベースの実装を後段で差し込めるよう trait 化してある。

use crate::node::{NodeKind, SourceNode};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tmoe_llm::{ChatMessage, ChatRequest, LlmClient};
use tokio::sync::Mutex;

#[async_trait]
pub trait SummaryCache: Send + Sync {
    async fn get(&self, content_hash: &str) -> Option<String>;
    async fn set(&self, content_hash: &str, summary: String);
}

#[derive(Default)]
pub struct InMemorySummaryCache {
    inner: Mutex<HashMap<String, String>>,
}

impl InMemorySummaryCache {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn into_arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl SummaryCache for InMemorySummaryCache {
    async fn get(&self, content_hash: &str) -> Option<String> {
        self.inner.lock().await.get(content_hash).cloned()
    }
    async fn set(&self, content_hash: &str, summary: String) {
        self.inner.lock().await.insert(content_hash.to_string(), summary);
    }
}

#[derive(Debug, Clone)]
pub struct EnrichOptions {
    /// summary 1 件あたりの最大文字数。LLM 出力が長すぎたら頭から切り詰める。
    pub max_chars: usize,
    /// File ノードを LLM 要約するか。子ノードを enrich すれば File の構造的要約は十分情報量を
    /// 持つので、ここは false がデフォルト。
    pub enrich_files: bool,
    /// Repo ノードを LLM 要約するか。同上で false 既定。
    pub enrich_repo: bool,
    /// 各ノードに渡すソース本文の最大文字数 (LLM プロンプトを膨らませない上限)。
    pub source_excerpt_chars: usize,
    /// `path` を解決する基準ディレクトリ。`SourceNode.path` が相対のとき root と join する。
    pub root: PathBuf,
}

impl EnrichOptions {
    pub fn new(root: PathBuf) -> Self {
        Self {
            max_chars: 240,
            enrich_files: false,
            enrich_repo: false,
            source_excerpt_chars: 1500,
            root,
        }
    }
}

/// 木をボトムアップで巡回して `summary` を LLM 要約に置換する (opt-in)。
/// 失敗したノードは元の summary を保つ。同じ `content_hash` は 1 回だけ LLM 呼出しする。
pub async fn enrich_summaries(
    tree: &mut SourceNode,
    llm: Arc<dyn LlmClient>,
    cache: Arc<dyn SummaryCache>,
    opts: &EnrichOptions,
) {
    enrich_node(tree, llm.as_ref(), cache.as_ref(), opts).await;
}

async fn enrich_node(
    node: &mut SourceNode,
    llm: &dyn LlmClient,
    cache: &dyn SummaryCache,
    opts: &EnrichOptions,
) {
    // 子から先に enrich する (ボトムアップ)。
    // recursion in async は box で囲む。
    for child in node.children.iter_mut() {
        Box::pin(enrich_node(child, llm, cache, opts)).await;
    }

    let should_enrich = match node.kind {
        NodeKind::Function | NodeKind::Class | NodeKind::Module => true,
        NodeKind::File => opts.enrich_files,
        NodeKind::Repo => opts.enrich_repo,
    };
    if !should_enrich {
        return;
    }
    if let Some(cached) = cache.get(&node.content_hash).await {
        node.summary = cached;
        return;
    }

    let excerpt = read_source_excerpt(node, &opts.root, opts.source_excerpt_chars);
    let prompt = format!(
        "Summarize this code node in ONE concise English sentence (under {max} characters). \
         No prose framing, no quoting — output just the sentence.\n\n\
         kind={kind:?} name={name} path={path} lines={start}..{end}\n\n\
         Source excerpt:\n{src}",
        max = opts.max_chars,
        kind = node.kind,
        name = node.name,
        path = node.path,
        start = node.start_line,
        end = node.end_line,
        src = excerpt,
    );
    let req = ChatRequest {
        messages: vec![
            ChatMessage::system("You write 1-sentence summaries of code nodes."),
            ChatMessage::user(prompt),
        ],
        max_tokens: Some(180),
        temperature: Some(0.0),
        ..Default::default()
    };
    if let Ok(resp) = llm.chat(req).await {
        let trimmed: String = resp.content.trim().chars().take(opts.max_chars).collect();
        if !trimmed.is_empty() {
            cache.set(&node.content_hash, trimmed.clone()).await;
            node.summary = trimmed;
        }
    }
    // LLM 失敗 / 空応答 → 既存 summary を維持 (= 構造的フォールバック)。
}

fn read_source_excerpt(node: &SourceNode, root: &std::path::Path, max_chars: usize) -> String {
    let abs = if std::path::Path::new(&node.path).is_absolute() {
        PathBuf::from(&node.path)
    } else {
        root.join(&node.path)
    };
    let Ok(body) = std::fs::read_to_string(&abs) else {
        return format!("(could not read {})", abs.display());
    };
    let lines: Vec<&str> = body.lines().collect();
    let start = (node.start_line as usize).saturating_sub(1);
    let end = (node.end_line as usize).min(lines.len());
    if start >= end {
        return String::new();
    }
    let slice = lines[start..end].join("\n");
    if slice.len() > max_chars {
        slice.chars().take(max_chars).collect()
    } else {
        slice
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{build_repo_tree, BuildOptions};
    use tmoe_llm::{MockLlmClient, ScriptedTurn};

    #[tokio::test]
    async fn enrich_replaces_function_summary_with_llm_output_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn add(a:i64,b:i64)->i64{a+b}\npub fn mul(a:i64,b:i64)->i64{a*b}\n",
        )
        .unwrap();
        let mut tree = build_repo_tree(&BuildOptions {
            root: root.clone(),
            ..Default::default()
        })
        .unwrap();

        let llm = Arc::new(MockLlmClient::new("enricher"));
        // 関数ノード 2 件分の応答を仕込む。
        llm.push(ScriptedTurn::new("Adds two i64 values."));
        llm.push(ScriptedTurn::new("Multiplies two i64 values."));

        let cache = InMemorySummaryCache::into_arc();
        let opts = EnrichOptions::new(root.clone());
        enrich_summaries(&mut tree, llm.clone() as Arc<dyn LlmClient>, cache.clone(), &opts).await;

        // ファイル直下の 2 関数の summary が LLM 出力で書き換わっているはず。
        let file = &tree.children[0];
        let fns: Vec<&SourceNode> = file
            .children
            .iter()
            .filter(|c| c.kind == NodeKind::Function)
            .collect();
        assert_eq!(fns.len(), 2);
        let summaries: Vec<String> = fns.iter().map(|f| f.summary.clone()).collect();
        assert!(
            summaries.iter().any(|s| s.contains("Adds")),
            "expected LLM-enriched summary 'Adds...', got: {summaries:?}"
        );
        assert!(
            summaries.iter().any(|s| s.contains("Multiplies")),
            "expected LLM-enriched summary 'Multiplies...', got: {summaries:?}"
        );

        // File / Repo は既定では enrich しない。
        assert!(file.summary.starts_with("file "));
        assert!(tree.summary.starts_with("repo at "));

        // 同じ木をもう一度 enrich しても LLM は呼ばれず、cache が効いている。
        let calls_before = llm.calls().len();
        enrich_summaries(&mut tree, llm.clone() as Arc<dyn LlmClient>, cache.clone(), &opts).await;
        let calls_after = llm.calls().len();
        assert_eq!(
            calls_before, calls_after,
            "second enrich must hit cache, not LLM (before={calls_before} after={calls_after})"
        );
    }

    #[tokio::test]
    async fn enrich_falls_back_to_structural_summary_on_llm_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "fn x(){}\n").unwrap();
        let mut tree = build_repo_tree(&BuildOptions {
            root: root.clone(),
            ..Default::default()
        })
        .unwrap();
        // No scripted turns -> mock chat returns Err -> enricher leaves summaries alone.
        let llm = Arc::new(MockLlmClient::new("flaky"));
        let cache = InMemorySummaryCache::into_arc();
        let opts = EnrichOptions::new(root.clone());

        let original_summary = tree.children[0].children[0].summary.clone();
        enrich_summaries(&mut tree, llm.clone() as Arc<dyn LlmClient>, cache.clone(), &opts).await;
        assert_eq!(tree.children[0].children[0].summary, original_summary);
    }

    #[test]
    fn file_summary_lists_child_kinds_and_names() {
        // build_repo_tree が出す File summary が「子の名前リスト」を含むことの回帰テスト。
        // 旧来の "file path (N lines)" だけ → rag::search の navigate-LLM が手がかりに乏しい
        // という問題への構造的フォールバック改善が機能している。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn alpha(){}\npub fn beta(){}\npub fn gamma(){}\n",
        )
        .unwrap();
        let tree = build_repo_tree(&BuildOptions {
            root: root.clone(),
            ..Default::default()
        })
        .unwrap();
        let file = &tree.children[0];
        assert!(file.summary.contains("alpha"), "summary missing alpha: {}", file.summary);
        assert!(file.summary.contains("beta"));
        assert!(file.summary.contains("gamma"));
        assert!(file.summary.starts_with("file "));
    }
}
