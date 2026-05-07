//! `search_source`: tmoe-tree (リポジトリの AST 木) を tmoe-rag (LLM 駆動の木探索) で
//! 検索するツール。Phase 5 のライブラリが long unused なまま放置されないよう、ここで
//! Worker のツール表に組み込む。
//!
//! 1 度だけ workspace 全体を tree-sitter でパースして木を組み (Mutex キャッシュ)、以降は
//! クエリごとに LLM が「次に開く子」を選びながら降りていく。ベクトル類似度・埋め込みは
//! 使わない (= PageIndex 思想)。
//!
//! Tool trait は `tmoe-tools` 側に定義されているが、tmoe-tools は AST 言語に依存させたく
//! ないので SearchSourceTool は CLI 側で実装する (= 機能 boundary を保つ)。

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tmoe_llm::LlmClient;
use tmoe_tools::{Permission, Tool, ToolError, ToolOutput, ToolResult};
use tmoe_tree::{
    build_repo_tree, enrich_summaries, BuildOptions, EnrichOptions, InMemorySummaryCache,
    SourceNode,
};
use tokio::sync::Mutex;

pub struct SearchSourceTool {
    pub root: PathBuf,
    pub llm: Arc<dyn LlmClient>,
    pub max_depth: usize,
    pub max_results: usize,
    /// 初回 build 後に LLM ベースのノード要約 (Function/Class/Module) を上書きする。
    /// 既定 false (= 構造的フォールバック要約だけで動かす)。N 個のノードに対し N 回の
    /// LLM 呼び出しが要るため、ローカル軽量モデルか opt-in 環境で使うことを想定。
    pub enable_llm_summaries: bool,
    cache: Mutex<Option<SourceNode>>,
    summary_cache: Arc<InMemorySummaryCache>,
}

impl SearchSourceTool {
    pub fn new(root: PathBuf, llm: Arc<dyn LlmClient>) -> Self {
        Self {
            root,
            llm,
            max_depth: 4,
            max_results: 8,
            enable_llm_summaries: false,
            cache: Mutex::new(None),
            summary_cache: Arc::new(InMemorySummaryCache::new()),
        }
    }

    /// LLM 駆動の summary enrichment を有効化する (DESIGN.md「ボトムアップで Worker LLM に
    /// 要約させて木を完成」の opt-in 実装)。
    pub fn with_llm_summaries(mut self, on: bool) -> Self {
        self.enable_llm_summaries = on;
        self
    }

    /// テスト/特殊用途: 既に構築済みの SourceNode をキャッシュに直接注入する。
    /// `ensure_tree` はキャッシュがあればそれを返すので、build_repo_tree が
    /// 呼ばれず ULID も決定論的にできる (= 単体テストで合言葉照合が可能)。
    #[cfg(test)]
    pub fn with_tree(root: PathBuf, llm: Arc<dyn LlmClient>, tree: SourceNode) -> Self {
        Self {
            root,
            llm,
            max_depth: 4,
            max_results: 8,
            enable_llm_summaries: false,
            cache: Mutex::new(Some(tree)),
            summary_cache: Arc::new(InMemorySummaryCache::new()),
        }
    }

    async fn ensure_tree(&self) -> Result<SourceNode, ToolError> {
        let mut g = self.cache.lock().await;
        if let Some(t) = g.as_ref() {
            return Ok(t.clone());
        }
        let opts = BuildOptions {
            root: self.root.clone(),
            max_files: 5000,
            follow_links: false,
            skip_dirs: vec![
                "target".into(),
                ".git".into(),
                "node_modules".into(),
                "dist".into(),
                "build".into(),
                "__pycache__".into(),
                ".venv".into(),
                "venv".into(),
                ".tmoe-worktrees".into(),
            ],
        };
        let mut tree = build_repo_tree(&opts)
            .map_err(|e| ToolError::Args(format!("build source tree: {e}")))?;

        if self.enable_llm_summaries {
            let enrich_opts = EnrichOptions::new(self.root.clone());
            // SummaryCache trait は Arc<dyn ...> 経由で注入する。
            let cache_obj: Arc<dyn tmoe_tree::SummaryCache> = self.summary_cache.clone();
            enrich_summaries(&mut tree, self.llm.clone(), cache_obj, &enrich_opts).await;
        }

        *g = Some(tree.clone());
        Ok(tree)
    }
}

#[derive(serde::Deserialize)]
struct Args {
    query: String,
}

#[async_trait]
impl Tool for SearchSourceTool {
    fn name(&self) -> &str {
        "search_source"
    }

    fn requires(&self) -> Permission {
        Permission::Read
    }

    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: Args = serde_json::from_value(args.clone())
            .map_err(|e| ToolError::Args(format!("search_source args: {e}")))?;
        if a.query.trim().is_empty() {
            return Err(ToolError::Args("query must be non-empty".into()));
        }
        let tree = self.ensure_tree().await?;
        let ids = tmoe_rag::search(&a.query, &tree, self.llm.as_ref(), self.max_depth)
            .await
            .map_err(|e| ToolError::Args(format!("rag search: {e}")))?;

        // 木を再走査して id ヒットしたノードのメタを抽出。
        let mut hits: Vec<&SourceNode> = Vec::new();
        let mut frontier: Vec<&SourceNode> = vec![&tree];
        while let Some(n) = frontier.pop() {
            if ids.iter().any(|h| h == &n.id) {
                hits.push(n);
            }
            for c in &n.children {
                frontier.push(c);
            }
        }
        hits.truncate(self.max_results);

        if hits.is_empty() {
            return Ok(ToolOutput::text(format!(
                "no matches for query={:?} in {} files",
                a.query,
                tree.children.len()
            )));
        }
        let body = hits
            .iter()
            .map(|n| {
                format!(
                    "{path}:{start}-{end}  {kind:?} {name}{summary}",
                    path = n.path,
                    start = n.start_line,
                    end = n.end_line,
                    kind = n.kind,
                    name = n.name,
                    summary = if n.summary.is_empty() {
                        String::new()
                    } else {
                        format!("  -- {}", n.summary)
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::text(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmoe_llm::{MockLlmClient, ScriptedTurn};
    use tmoe_tree::NodeKind;

    fn leaf(id: &str, name: &str, path: &str, line: u32, summary: &str) -> SourceNode {
        SourceNode {
            id: id.into(),
            kind: NodeKind::Function,
            name: name.into(),
            path: path.into(),
            start_line: line,
            end_line: line,
            children: vec![],
            summary: summary.into(),
            content_hash: "h".into(),
        }
    }

    fn fixture_repo() -> SourceNode {
        let add = leaf("FN-add", "add", "src/lib.rs", 1, "compute a+b");
        let mul = leaf("FN-mul", "mul", "src/lib.rs", 2, "compute a*b");
        let file = SourceNode {
            id: "F-lib".into(),
            kind: NodeKind::File,
            name: "src/lib.rs".into(),
            path: "src/lib.rs".into(),
            start_line: 1,
            end_line: 2,
            children: vec![add, mul],
            summary: "lib.rs".into(),
            content_hash: "fh".into(),
        };
        SourceNode {
            id: "REPO".into(),
            kind: NodeKind::Repo,
            name: "test-repo".into(),
            path: "/tmp/test-repo".into(),
            start_line: 0,
            end_line: 0,
            children: vec![file],
            summary: "repo".into(),
            content_hash: "rh".into(),
        }
    }

    #[tokio::test]
    async fn search_source_returns_path_and_lines_for_match() {
        let llm = Arc::new(MockLlmClient::new("rag"));
        // Round 1: repo → drill into the file
        llm.push(ScriptedTurn::new(
            r#"{"next":["F-lib"],"terminal":false,"leaves":[]}"#,
        ));
        // Round 2: file → terminal=true returning the matched function id
        llm.push(ScriptedTurn::new(
            r#"{"next":[],"terminal":true,"leaves":["FN-add"]}"#,
        ));
        let tool = SearchSourceTool::with_tree(
            PathBuf::from("/tmp/test-repo"),
            llm.clone() as Arc<dyn LlmClient>,
            fixture_repo(),
        );
        let out = tool
            .call(&serde_json::json!({"query": "where is add"}))
            .await
            .expect("search ok");
        assert!(
            out.stdout.contains("src/lib.rs:1-1"),
            "stdout missing path:line — got {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("add"),
            "stdout missing name 'add' — got {:?}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn search_source_no_match_path() {
        let llm = Arc::new(MockLlmClient::new("rag"));
        // The LLM declines to descend (next=[], terminal=false) → search yields nothing.
        llm.push(ScriptedTurn::new(
            r#"{"next":[],"terminal":false,"leaves":[]}"#,
        ));
        let tool = SearchSourceTool::with_tree(
            PathBuf::from("/tmp/test-repo"),
            llm.clone() as Arc<dyn LlmClient>,
            fixture_repo(),
        );
        let out = tool
            .call(&serde_json::json!({"query": "totally absent"}))
            .await
            .unwrap();
        assert!(out.stdout.contains("no matches"), "got {:?}", out.stdout);
    }

    #[tokio::test]
    async fn search_source_rejects_empty_query() {
        let dir = tempfile::tempdir().unwrap();
        let llm = Arc::new(MockLlmClient::new("rag"));
        let tool = SearchSourceTool::new(
            dir.path().to_path_buf(),
            llm.clone() as Arc<dyn LlmClient>,
        );
        let err = tool
            .call(&serde_json::json!({"query": "   "}))
            .await
            .unwrap_err();
        match err {
            ToolError::Args(_) => {}
            other => panic!("expected Args, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_source_real_build_returns_no_matches_when_llm_declines() {
        // テスト fixture ではなく実物の build_repo_tree を回し、空木+デクライン応答で
        // ToolOutput::text("no matches ...") が出ることを確認する。これにより
        // ensure_tree (キャッシュ + tree-sitter パース) が落ちないことを担保する。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn ping() -> u8 { 1 }\n").unwrap();

        let llm = Arc::new(MockLlmClient::new("rag-decline"));
        llm.push(ScriptedTurn::new(
            r#"{"next":[],"terminal":false,"leaves":[]}"#,
        ));
        let tool = SearchSourceTool::new(root, llm.clone() as Arc<dyn LlmClient>);
        let out = tool
            .call(&serde_json::json!({"query": "ping"}))
            .await
            .unwrap();
        assert!(out.stdout.contains("no matches"), "stdout: {}", out.stdout);
    }
}
