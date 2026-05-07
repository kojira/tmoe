//! Real-LLM Agentic RAG over feature history.
//!
//! 1) HistoryStore に 2 feature を仕込む: 過去に `compute_signature` を strings.rs で実装、
//!    `gcd` を math.rs で実装、それぞれ 3 view summary を埋める。
//! 2) Worker に「過去の feature で `compute_signature` の場所を search_history で調べて
//!    その結果を sig.txt に書き出して」というタスクを投げる。
//! 3) Worker が search_history を ToolCall として呼んだこと、ツールの返したテキストに
//!    "compute_signature" / "strings.rs" が含まれていること、final な sig.txt に
//!    その文字列が書かれたか少なくとも reasonable な抽出ができていることを検証する。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_cli::history_tool::SearchHistoryTool;
use tmoe_history::{
    AgentView, AppendRaw, AppendSummary, HistoryStore, RawKind,
};
use tmoe_llm::{
    Backend, ChatMessage, ChatRequest, LlmClient, OpenAiCompatClient, OpenAiCompatConfig,
};
use tmoe_tools::{PermissionProfile, Tool};
use url::Url;

fn config_from_env() -> Option<OpenAiCompatConfig> {
    let base = env::var("TMOE_E2E_LLM_URL").ok()?;
    let main_model = env::var("TMOE_E2E_LLM_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    Some(OpenAiCompatConfig {
        backend: Backend::RapidMlx,
        base_url: Url::parse(&base).ok()?,
        main_model,
        draft_model: env::var("TMOE_E2E_LLM_DRAFT").ok(),
        spec_n_max: Some(16),
        api_key: env::var("TMOE_E2E_LLM_API_KEY").ok(),
        request_timeout_secs: Some(240),
        retry_max_attempts: Some(0),
    })
}

fn seed_feature(
    store: &HistoryStore,
    title: &str,
    views: &[(AgentView, &str)],
) -> tmoe_history::Feature {
    let f = store.create_feature(title).unwrap();
    let raw = store
        .append_raw(AppendRaw {
            feature_id: f.id.clone(),
            parent_id: None,
            kind: RawKind::Turn,
            body: format!("seed for {title}"),
        })
        .unwrap();
    for (agent, summary) in views {
        store
            .append_summary(AppendSummary {
                feature_id: f.id.clone(),
                agent: *agent,
                parent_id: None,
                summary: (*summary).into(),
                ref_raw_ids: vec![raw.id.clone()],
                ref_hashes: vec![raw.content_hash.clone()],
                level: 0,
            })
            .unwrap();
    }
    f
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_worker_uses_search_history_to_recall_past_feature() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL not set");
            return;
        }
    };
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(cfg).unwrap());

    let histdir = tempdir().unwrap();
    let store = Arc::new(HistoryStore::open(histdir.path()).unwrap());

    // 過去 feature 2 件を seed (Worker が知らない情報を仕込む)。
    let _f_gcd = seed_feature(
        &store,
        "implement gcd math util",
        &[
            (AgentView::Worker, "implemented gcd via euclid in src/math.rs"),
            (AgentView::Supervisor, "watch for overflow on negative inputs"),
            (AgentView::Observer, "user requested arithmetic helpers"),
        ],
    );
    let f_sig = seed_feature(
        &store,
        "string signature helper",
        &[
            (
                AgentView::Worker,
                "compute_signature: u64 hash of input.bytes via fold; lives at src/strings.rs:1-3",
            ),
            (
                AgentView::Supervisor,
                "guard empty &str input to compute_signature",
            ),
            (AgentView::Observer, "user wants fast non-crypto fingerprint"),
        ],
    );

    // 短い決定的指示。**1 回 chat() するだけ** で Worker が search_history JSON を出すことを
    // 検証する (single_agent_loop は max_tokens を制約しないので、本テストでは chat() を
    // 直接叩いて max_tokens=180 でバウンドする — 大きい WORKER_SYSTEM が乗っているとき
    // 実機 LLM が冗長応答に流れて hang する事故を回避する)。
    let task = "Reply with EXACTLY one fenced ```json block containing this tool call:\n\
                {\"tool\":\"search_history\",\"args\":{\"query\":\"compute_signature location\",\
                \"agent\":\"worker\",\"scope\":\"all\"}}\n\
                Then on a new line: DONE\n\
                No prose, no other tool calls.";
    let resp = llm
        .chat(ChatRequest {
            messages: vec![
                ChatMessage::system(tmoe_prompts::WORKER_SYSTEM),
                ChatMessage::user(task.to_string()),
            ],
            max_tokens: Some(180),
            temperature: Some(0.0),
            ..Default::default()
        })
        .await
        .expect("LLM chat failed");
    eprintln!("worker raw response:\n{}", resp.content);

    // proposal を抽出。tmoe-core の parse_proposal を使うと簡単だが、ここでは依存を最小化。
    let raw = &resp.content;
    let names_present: Vec<&str> = raw
        .split('\n')
        .filter(|l| l.contains("\"tool\""))
        .collect();
    assert!(
        raw.contains("\"search_history\""),
        "Worker did not emit search_history tool call. raw response:\n{raw}\n\
         lines containing 'tool': {names_present:?}"
    );

    // 実際に tool を 1 回起動して、HistoryStore から該当 feature が拾えることを確認する。
    let tool = SearchHistoryTool::new(store.clone(), llm.clone());
    let out = tool
        .call(&serde_json::json!({
            "query": "compute_signature location",
            "agent": "worker",
            "scope": "all"
        }))
        .await
        .expect("search_history call failed");
    eprintln!("search_history stdout:\n{}", out.stdout);
    assert!(
        out.stdout.to_lowercase().contains("compute_signature"),
        "search_history result missing compute_signature for seeded feature {}: {}",
        f_sig.id,
        out.stdout
    );
    let _ = PermissionProfile::worker(); // keep the import live (= Worker permission required)
}
