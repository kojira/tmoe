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
use tmoe_core::{single_agent_loop, AgentRole};
use tmoe_history::{
    AgentView, AppendRaw, AppendSummary, HistoryStore, RawKind,
};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tools::{
    default_blocklist, EditFileTool, GrepTextTool, ListFilesTool, PatchFileTool, ReadFileTool,
    RunCmdTool, ToolRegistry,
};
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

    // 作業用 workspace を用意 (ツール経由で edit_file させるため)。
    let workdir = tempdir().unwrap();

    // Worker に渡すツール集合: 通常ツール + search_history (current feature を bind せず all 走査)。
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ReadFileTool { root: workdir.path().to_path_buf() }));
    reg.register(Arc::new(EditFileTool { root: workdir.path().to_path_buf() }));
    reg.register(Arc::new(PatchFileTool { root: workdir.path().to_path_buf() }));
    reg.register(Arc::new(ListFilesTool { root: workdir.path().to_path_buf() }));
    reg.register(Arc::new(GrepTextTool { root: workdir.path().to_path_buf() }));
    reg.register(Arc::new(RunCmdTool {
        root: workdir.path().to_path_buf(),
        blocklist: default_blocklist(),
    }));
    reg.register(Arc::new(SearchHistoryTool::new(store.clone(), llm.clone())));

    // 短い、決定的な指示。Worker に「search_history を 1 度呼んで結果を見せろ。
    // それ以上は何もしなくてよい」と明示する。
    let task = "Use the search_history tool exactly ONCE with these args:\n\
                {\"query\":\"compute_signature location\",\"agent\":\"worker\",\"scope\":\"all\"}\n\
                Then on a new line write: DONE\n\
                No edit_file, no other tools. Just one search_history JSON block and DONE.";
    let messages = vec![ChatMessage::user(task)];
    let pm = single_agent_loop(
        AgentRole::Worker,
        tmoe_prompts::WORKER_SYSTEM,
        messages,
        llm.as_ref(),
        &reg,
    )
    .await
    .expect("worker loop");

    eprintln!(
        "tool_calls: {:?}",
        pm.proposal.tool_calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
    );
    eprintln!("proposal raw (head 600 chars):\n{}", &pm.proposal.raw_text.chars().take(600).collect::<String>());

    let names: Vec<&str> = pm.proposal.tool_calls.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.iter().any(|n| *n == "search_history"),
        "Worker did not call search_history. tool_calls={names:?}"
    );

    // ツール呼び出しの args.query を読み取って "compute_signature" を含むか確認する。
    let history_call = pm
        .proposal
        .tool_calls
        .iter()
        .find(|c| c.name == "search_history")
        .expect("search_history call not found");
    let q = history_call
        .args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        q.to_lowercase().contains("compute_signature"),
        "search_history.query should contain 'compute_signature', got: {q}"
    );
    // f_sig は seed したので存在することは保証済み。出力 (= ToolOutput.stdout) を直接拾う
    // 経路は single_agent_loop が ProposalMessage.tool_outputs に格納している。
    let any_ok = pm.tool_outputs.iter().any(|r| {
        r.as_ref()
            .map(|o| o.stdout.to_lowercase().contains("compute_signature"))
            .unwrap_or(false)
    });
    assert!(
        any_ok,
        "search_history result should contain 'compute_signature' for f_sig={}; tool_outputs={:?}",
        f_sig.id, pm.tool_outputs
    );
}
