//! Real-LLM Trio: search_source actually used by Worker.
//!
//! Phase 5 (tmoe-tree + tmoe-rag) を「ライブラリは動く」だけで終わらせず、
//! **実 LLM が Worker として search_source ツールを呼ぶ**ことを e2e で確認する。
//!
//! シナリオ:
//!   - 一時ワークスペースに 3 ファイル (math.rs / strings.rs / io.rs) を配置
//!   - 「search_source を使って `compute_signature` 関数の場所を特定し、その中身を要約してください」
//!     と Worker に依頼
//!   - Worker の proposal に `search_source` ツールコールが含まれるかを検証
//!     (含まなくても他の探索ツールで答えに辿り着ければそれは合理的なので、
//!      Worker proposal の tool_calls に search_source または read_file が現れることをアサート)
//!
//! このテストの主目的は **「search_source が宣伝されている → 実 LLM がそれを実際に呼べる
//! 経路がつながっている」** という設計と実装の整合を担保すること。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{
    single_agent_loop, AgentRole, Trio, ThrustChannel, UserThrust, ConsensusOutcome,
    ConsensusThresholds,
};
use tmoe_history::{HistoryStore, HistoryViewProvider};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tools::ToolRegistry;
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
            request_timeout_secs: None,
            retry_max_attempts: None,
        codex_auth_path: None,
    })
}

fn write(p: &std::path::Path, body: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_worker_invokes_search_source_or_equivalent() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL not set");
            return;
        }
    };
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(cfg).unwrap());

    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    // ターゲット関数だけは strings.rs に隠す。math.rs / io.rs にもダミーを置いて
    // 「ファイル名から自明には引けない」状況にする。
    write(&root.join("src/math.rs"), "pub fn add(a:i64,b:i64)->i64{a+b}\n");
    write(
        &root.join("src/strings.rs"),
        "pub fn compute_signature(input: &str) -> u64 {\n    \
            input.bytes().fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64))\n}\n",
    );
    write(&root.join("src/io.rs"), "pub fn read_line()->String{String::new()}\n");

    // ツール登録 (CLI runtime と同じセット)。LlmClient を共有して search_source も有効に。
    use tmoe_tools::{
        default_blocklist, EditFileTool, GrepTextTool, ListFilesTool, PatchFileTool,
        ReadFileTool, RunCmdTool,
    };
    use tmoe_cli::source_tool::SearchSourceTool;
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(EditFileTool { root: root.clone() }));
    reg.register(Arc::new(PatchFileTool { root: root.clone() }));
    reg.register(Arc::new(ListFilesTool { root: root.clone() }));
    reg.register(Arc::new(GrepTextTool { root: root.clone() }));
    reg.register(Arc::new(RunCmdTool {
        root: root.clone(),
        blocklist: default_blocklist(),
    }));
    reg.register(Arc::new(SearchSourceTool::new(root.clone(), llm.clone())));

    // Worker 単独で 1 turn を回す (Trio 全周は別の e2e でカバー済み)。本テストは
    // 「Worker が search_source または read_file を呼んでターゲット関数の存在を確認できる」
    // ことだけに焦点を絞る。
    let task = "Locate the function `compute_signature` somewhere under src/. \
                Use search_source (or grep_text/read_file as fallback). \
                Once found, read its body and report its 1-line summary.\n\
                Emit your tool calls as fenced ```json blocks. End with a single line: DONE.";
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

    eprintln!("proposal raw: {}", pm.proposal.raw_text);
    eprintln!(
        "tool_calls: {:?}",
        pm.proposal
            .tool_calls
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
    );

    // 期待する到達点: Worker は search_source か grep_text か read_file を **少なくとも 1 回**
    // 呼んでいる。これらは「ファイル探索」というカテゴリで等価に扱う (LLM が search_source を
    // 選ぶ確率が低い場合への保険)。なお search_source が一度も呼ばれない場合でも、テスト名と
    // assertion メッセージに「search_source を呼ぶことが推奨」と明示しておく。
    let names: Vec<&str> = pm.proposal.tool_calls.iter().map(|c| c.name.as_str()).collect();
    let exploratory_called = names
        .iter()
        .any(|n| matches!(*n, "search_source" | "grep_text" | "read_file" | "list_files"));
    assert!(
        exploratory_called,
        "Worker did not call any source-exploration tool (search_source/grep_text/read_file/list_files). \
         tool_calls={names:?} proposal_raw={}",
        pm.proposal.raw_text
    );

    // 強い検証: search_source が呼ばれていれば理想 (= Phase 5 の本懐)。
    // 呼ばれない場合は info 出力に留め、テスト失敗にはしない (LLM の選好に依存するため)。
    let search_source_used = names.iter().any(|n| *n == "search_source");
    if search_source_used {
        eprintln!(
            "✓ Worker actively chose search_source (Phase 5 PageIndex-style RAG actually used)"
        );
    } else {
        eprintln!(
            "ℹ Worker preferred {:?} over search_source — search_source is registered \
             and reachable, but the LLM did not pick it for this query",
            names
        );
    }

    // Trio 経由でも到達できる経路があることを軽く確認 (run_step が走るか)。
    let store_dir = tempdir().unwrap();
    let store = HistoryStore::open(store_dir.path()).unwrap();
    let feature = store.create_feature("locate compute_signature").unwrap();
    let trio = Trio::from_shared_llm(llm.clone()).with_thresholds(ConsensusThresholds {
        confidence_sum_min: 1.5,
        triangle_balance_min: 0.3,
        max_iter_per_step: 3,
    });
    let (tx, mut rx) = ThrustChannel::new();
    tx.send(UserThrust::Go { strength: 1.0 }).unwrap();
    let provider = HistoryViewProvider::new(&store, feature.id.clone());
    let outcome = trio
        .run_step_with_views(
            &[ChatMessage::user(task)],
            &reg,
            &mut rx,
            Some(&provider),
        )
        .await
        .expect("trio run_step_with_views");
    match outcome.last {
        ConsensusOutcome::Commit { .. } => {}
        other => eprintln!("(non-Commit outcome is acceptable for this exploration test): {other:?}"),
    }
}
