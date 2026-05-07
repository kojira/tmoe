//! Real-LLM Trio + ViewProvider e2e.
//!
//! 通常の Trio 経路 (e2e_real_trio.rs) に **HistoryViewProvider** を噛ませる。
//! HistoryStore に 3 view 分の brief をあらかじめ書き込み、Trio が `run_step_with_views`
//! 経由でそれを Supervisor / Observer / Worker self-vote の各プロンプトに prepend した
//! 状態で実 LLM に投票させる。「Worker view が誰にも読まれない」状態の解消が、Mock では
//! なく実機 LLM 越しでも壊れず Commit に至ることを検証する。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{ConsensusOutcome, ConsensusThresholds, ThrustChannel, Trio, UserThrust};
use tmoe_history::{
    AppendRaw, AppendSummary, AgentView, HistoryStore, HistoryViewProvider, RawKind, ViewProvider,
};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tools::{default_blocklist, EditFileTool, ReadFileTool, RunCmdTool, ToolRegistry};
use url::Url;

fn config_from_env() -> Option<OpenAiCompatConfig> {
    let base = env::var("TMOE_E2E_LLM_URL").ok()?;
    let main_model = env::var("TMOE_E2E_LLM_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    let backend = match env::var("TMOE_E2E_LLM_BACKEND")
        .unwrap_or_else(|_| "rapid_mlx".into())
        .as_str()
    {
        "vllm" => Backend::Vllm,
        "lm_studio" => Backend::LmStudio,
        "rapid_mlx" => Backend::RapidMlx,
        "openai_compat" => Backend::OpenAiCompat,
        _ => Backend::LlamaCpp,
    };
    Some(OpenAiCompatConfig {
        backend,
        base_url: Url::parse(&base).ok()?,
        main_model,
        draft_model: env::var("TMOE_E2E_LLM_DRAFT").ok(),
        spec_n_max: Some(16),
        api_key: env::var("TMOE_E2E_LLM_API_KEY").ok(),
            request_timeout_secs: None,
            retry_max_attempts: None,
    })
}

fn registry(root: std::path::PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EditFileTool { root: root.clone() }));
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(RunCmdTool {
        root,
        blocklist: default_blocklist(),
    }));
    reg
}

fn seed_view(
    store: &HistoryStore,
    feature_id: &str,
    raw_id: &str,
    raw_hash: &str,
    agent: AgentView,
    summary: &str,
) {
    store
        .append_summary(AppendSummary {
            feature_id: feature_id.into(),
            agent,
            parent_id: None,
            summary: summary.into(),
            ref_raw_ids: vec![raw_id.into()],
            ref_hashes: vec![raw_hash.into()],
            level: 0,
        })
        .unwrap();
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_trio_with_view_provider_still_commits() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL is not set");
            return;
        }
    };
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(cfg).unwrap());

    // ワークスペース (作業対象) と履歴ストア (view 供給元) は別ディレクトリ。
    let workdir = tempdir().unwrap();
    let root = workdir.path().to_path_buf();
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "tmoe_view_e2e"
version = "0.0.1"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "").unwrap();
    let reg = registry(root.clone());

    let histdir = tempdir().unwrap();
    let store = HistoryStore::open(histdir.path()).unwrap();
    let feature = store.create_feature("trio-view-e2e").unwrap();

    // 過去ターンの raw を 1 件だけ作り、その content_hash を 3 view summary が参照する
    // 形に整える (= summary が孤立しない、現実的な履歴形状)。
    let raw = store
        .append_raw(AppendRaw {
            feature_id: feature.id.clone(),
            parent_id: None,
            kind: RawKind::Turn,
            body: "previous turn: scaffolding for tmoe_view_e2e crate".into(),
        })
        .unwrap();

    // 3 view それぞれに自分のパーソナリティで残した「直前ターンの記憶」を seed する。
    // 表現はそれぞれ異なる (Worker = 実装記録、Supervisor = 規範・要件カバレッジ、
    // Observer = 意図・連続性) — 平面が縮退していないことを履歴側でも担保する。
    seed_view(
        &store,
        &feature.id,
        &raw.id,
        &raw.content_hash,
        AgentView::Worker,
        "[builder] previous turn: created empty src/lib.rs and Cargo.toml; \
         no public API yet; next step is to implement add() and mul() in one file.",
    );
    seed_view(
        &store,
        &feature.id,
        &raw.id,
        &raw.content_hash,
        AgentView::Supervisor,
        "[critic] requirements pending: pub fn add (i64,i64)->i64; pub fn mul (i64,i64)->i64; \
         #[cfg(test)] mod tests with two assertions. None of these are satisfied yet — \
         do NOT approve any proposal that omits even one of the four points.",
    );
    seed_view(
        &store,
        &feature.id,
        &raw.id,
        &raw.content_hash,
        AgentView::Observer,
        "[witness] user intent: small math util crate that compiles + passes its own tests \
         in a SINGLE src/lib.rs. Watch for: split-file proposals, missing DONE marker, \
         repeated proposal of the same partial code (= loop).",
    );

    let trio = Trio::from_shared_llm(llm).with_thresholds(ConsensusThresholds {
        confidence_sum_min: 1.5,
        triangle_balance_min: 0.3,
        max_iter_per_step: 4,
    });

    let (tx, mut rx) = ThrustChannel::new();
    tx.send(UserThrust::Go { strength: 1.0 }).unwrap();

    let task = vec![ChatMessage::user(
        "Implement the math util described in the PRIOR VIEWS supervisor brief: \
         a SINGLE file src/lib.rs that exposes pub fn add(i64,i64)->i64 and \
         pub fn mul(i64,i64)->i64, plus #[cfg(test)] mod tests with two #[test] \
         functions asserting add(2,3)==5 and mul(2,3)==6.\n\
         Emit exactly ONE edit_file tool call (one ```json block) for src/lib.rs.\n\
         Then on a brand-new line output the single token: DONE\n\
         Do not skip the DONE marker. No prose, no extra fences.",
    )];

    let provider = HistoryViewProvider::new(&store, feature.id.clone());
    // smoke: provider が 3 view brief を返せるか (履歴 seed の sanity check)。
    assert!(provider.brief(AgentView::Worker).unwrap().contains("[builder]"));
    assert!(provider.brief(AgentView::Supervisor).unwrap().contains("[critic]"));
    assert!(provider.brief(AgentView::Observer).unwrap().contains("[witness]"));

    let outcome = trio
        .run_step_with_views(&task, &reg, &mut rx, Some(&provider))
        .await
        .expect("run_step_with_views failed");

    eprintln!("steps={} outcome={:?}", outcome.steps, outcome.last);
    match &outcome.last {
        ConsensusOutcome::Commit { votes, .. } => {
            for (i, v) in votes.iter().enumerate() {
                eprintln!(
                    "vote[{i}]: approve={} confidence={} note={:?}",
                    v.approve, v.confidence, v.note
                );
            }
        }
        other => {
            eprintln!(
                "--- src/lib.rs ---\n{}",
                std::fs::read_to_string(root.join("src/lib.rs")).unwrap_or_default()
            );
            panic!("expected Commit with views, got {other:?}");
        }
    }

    let body = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(body.contains("pub fn add") && body.contains("pub fn mul"),
        "src/lib.rs missing required APIs. Body:\n{body}");
    assert!(body.contains("#[cfg(test)]") || body.contains("mod tests"),
        "src/lib.rs missing test module. Body:\n{body}");

    let status = std::process::Command::new("cargo")
        .arg("test")
        .arg("--lib")
        .current_dir(&root)
        .status()
        .expect("cargo test spawn failed");
    if !status.success() {
        eprintln!("--- src/lib.rs ---\n{body}");
        panic!("cargo test --lib failed on Trio-produced code");
    }
}
