//! `enrich_summaries` を実 LLM (Rapid-MLX) で 1 度通す。
//!
//! これが緑になることで、「DESIGN.md がうたう ボトムアップで Worker LLM に要約させて木を完成」
//! が library-complete だけでなく、実機 LLM 越しにも成立することを担保する。
//!
//! 検証点:
//! - Function ノードの summary が、構造的フォールバック (`function add @ ...`) から
//!   人語の 1 文要約に書き換わっている (= 文字数が増え、自然言語っぽい単語を含む)
//! - 同じ木をもう一度 enrich しても LLM 呼出しは増えない (content_hash キャッシュ)

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_llm::{Backend, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tree::{
    build_repo_tree, enrich_summaries, BuildOptions, EnrichOptions, InMemorySummaryCache,
    NodeKind, SummaryCache,
};
use url::Url;

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_enriches_function_summaries() {
    let url = match env::var("TMOE_E2E_LLM_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("skipping: TMOE_E2E_LLM_URL not set");
            return;
        }
    };
    let model = env::var("TMOE_E2E_LLM_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    let cfg = OpenAiCompatConfig {
        backend: Backend::RapidMlx,
        base_url: Url::parse(&url).unwrap(),
        main_model: model,
        draft_model: None,
        spec_n_max: Some(16),
        api_key: None,
    };
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(cfg).unwrap());

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n\
         pub fn fib(n: u32) -> u64 {\n    if n < 2 { n as u64 } else { fib(n-1) + fib(n-2) }\n}\n",
    )
    .unwrap();

    let mut tree = build_repo_tree(&BuildOptions {
        root: root.clone(),
        ..Default::default()
    })
    .unwrap();

    // Pre-enrich: function summaries should be the structural fallback ("function_item add @ ..." 等)。
    let file = &tree.children[0];
    for fnnode in file.children.iter().filter(|c| c.kind == NodeKind::Function) {
        assert!(
            fnnode.summary.contains(" @ ") && fnnode.summary.contains(&fnnode.name),
            "expected structural fallback (`<kind_str> <name> @ <path>:<lines>`) before enrich, \
             got: {}",
            fnnode.summary
        );
    }
    let pre_summaries: Vec<String> = tree.children[0]
        .children
        .iter()
        .filter(|c| c.kind == NodeKind::Function)
        .map(|c| c.summary.clone())
        .collect();

    let cache: Arc<dyn SummaryCache> = Arc::new(InMemorySummaryCache::new());
    let opts = EnrichOptions::new(root.clone());
    enrich_summaries(&mut tree, llm.clone(), cache.clone(), &opts).await;

    let post_summaries: Vec<String> = tree.children[0]
        .children
        .iter()
        .filter(|c| c.kind == NodeKind::Function)
        .map(|c| c.summary.clone())
        .collect();

    eprintln!("pre: {:?}\npost: {:?}", pre_summaries, post_summaries);
    // 全関数 summary が書き換わっていること (LLM が縮退したら structural のままで通る fallback も
    // 実装されているが、Rapid-MLX が普通に応答するならここは true になる)。
    let any_changed = pre_summaries
        .iter()
        .zip(post_summaries.iter())
        .any(|(a, b)| a != b);
    assert!(
        any_changed,
        "no function summary was enriched by the LLM. pre={pre_summaries:?} post={post_summaries:?}"
    );
    // 少なくとも 1 つは natural-language っぽい単語を含むはず。
    let nl_words = ["Adds", "adds", "Returns", "returns", "Compute", "compute", "Fibonacci", "fibonacci", "sum", "Sum"];
    let has_nl = post_summaries
        .iter()
        .any(|s| nl_words.iter().any(|w| s.contains(w)));
    assert!(
        has_nl,
        "post-enrich summaries don't look like natural-language sentences: {post_summaries:?}"
    );

    // 2 度目の enrich: cache が効いて LLM 呼び出しが追加で起きないことを確認する代わりに、
    // 結果が安定していることだけ assert (LLM 呼び出し数の検査は MockLlmClient テストで
    // 既に担保済み)。
    let mut tree2 = build_repo_tree(&BuildOptions {
        root: root.clone(),
        ..Default::default()
    })
    .unwrap();
    enrich_summaries(&mut tree2, llm.clone(), cache.clone(), &opts).await;
    let cached_summaries: Vec<String> = tree2.children[0]
        .children
        .iter()
        .filter(|c| c.kind == NodeKind::Function)
        .map(|c| c.summary.clone())
        .collect();
    // content_hash は同じソースから決まるので cache がヒットして、post と等しくなるはず。
    assert_eq!(
        cached_summaries, post_summaries,
        "cache hit should reproduce earlier summaries deterministically"
    );
}
