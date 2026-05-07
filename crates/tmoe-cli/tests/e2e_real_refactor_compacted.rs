//! Real-LLM e2e: multi-file リファクタリングを **tmoe-history の 3 並走 index +
//! 逐次コンパクション** で記憶圧縮しながら駆動する。
//!
//! 各ターンで:
//!   1. raw_node を `HistoryStore` に append (Worker 提案 + tool 出力)
//!   2. `compact_turn_for_all` で Worker / Supervisor / Observer view を逐次延伸
//!   3. 次ターンの Worker への入力 = タスク + **Worker view summary (パーソナリティ要約)**
//!      + **直近 raw 1 件**
//!
//! request サイズはターン数に対し線形でなく、Worker view の容量に比例する程度に抑えられる。
//! これが tmoe の設計通りの「機能ごとの記憶圧縮」を実機 LLM 駆動で動かす検証。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{
    single_agent_loop, AgentRole, ProgressVerifier, ProposalMessage, VerifierOutcome,
};
use tmoe_history::{
    compact_turn_for_all, AgentLens, AgentView, AppendRaw, HistoryStore, LlmLens, RawKind,
};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_prompts::{OBSERVER_SYSTEM, SUPERVISOR_SYSTEM, WORKER_SYSTEM};
use tmoe_tools::{
    GrepTextTool, ListFilesTool, PatchFileTool, ReadFileTool, ToolRegistry,
};
use url::Url;

const WORKER_PROMPT: &str = r#"You are a refactor agent. Emit tool calls as fenced ```json blocks.

Available tools:
  {"tool":"list_files","args":{"pattern":"<glob>"}}
  {"tool":"grep_text","args":{"pattern":"<text>","regex":false}}
  {"tool":"read_file","args":{"path":"<rel>"}}
  {"tool":"patch_file","args":{"path":"<rel>","search":"<exact>","replace":"<new>","replace_all":true}}

JSON rules:
  - inner double quotes inside a string MUST be escaped as \"
  - newlines inside string values MUST be \n
  - backslashes must be doubled to \\

CRITICAL refactor rules:
  - When renaming an identifier, set search to the BARE token (e.g. "old_name"), NOT
    a multi-line surrounding snippet. Multi-line searches are fragile and often fail.
  - Use replace_all=true so all occurrences in a file are replaced atomically.
  - When the [concierge] message lists multiple files, emit ONE patch_file per file
    in the SAME turn (multiple ```json blocks). Do NOT spread file edits across many
    turns — one turn should knock out all listed files.

Output ONE OR MORE ```json blocks per turn. Do NOT include prose.
"#;

fn config_from_env() -> Option<OpenAiCompatConfig> {
    let base = env::var("TMOE_E2E_LLM_URL").ok()?;
    let main_model =
        env::var("TMOE_E2E_LLM_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
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
    })
}

fn registry(root: std::path::PathBuf) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(PatchFileTool { root: root.clone() }));
    reg.register(Arc::new(ListFilesTool { root: root.clone() }));
    reg.register(Arc::new(GrepTextTool { root }));
    reg
}

fn fixture() -> tempfile::TempDir {
    let d = tempdir().unwrap();
    let p = d.path();
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::create_dir_all(p.join("tests")).unwrap();
    std::fs::write(
        p.join("Cargo.toml"),
        r#"[package]
name = "demo_pkg"
version = "0.0.1"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .unwrap();
    std::fs::write(
        p.join("src/lib.rs"),
        "pub mod util;\n\npub fn old_name() -> i32 {\n    util::old_name() + 1\n}\n",
    )
    .unwrap();
    std::fs::write(p.join("src/util.rs"), "pub fn old_name() -> i32 { 41 }\n").unwrap();
    std::fs::write(
        p.join("tests/it.rs"),
        "use demo_pkg::old_name;\n#[test]\nfn t() { assert_eq!(old_name(), 42); }\n",
    )
    .unwrap();
    d
}

fn lenses(llm: Arc<dyn LlmClient>) -> Vec<Box<dyn AgentLens>> {
    // 本物の LlmLens を使う。3 つの Lens は同一 LLM クライアントを共有しつつ、
    // パーソナリティプロンプト (tmoe-prompts) で 3 つの異なる方向性ベクトルを表現。
    // 各ターン後に追加 LLM 呼び出し 3 回が走るが、Worker への context は固定サイズの
    // summary に圧縮されるので、ターン数を重ねても request body は爆発しない。
    let tighten = |sys: &str| -> String {
        // 要約 LLM への system は短く絞る。tmoe-prompts のフルプロンプトは Worker 駆動用なので
        // ここでは Lens の役割だけを伝える短い系統に置換。
        sys.lines().take(3).collect::<Vec<_>>().join("\n")
    };
    vec![
        Box::new({
            let mut l = LlmLens::new(AgentView::Worker, tighten(WORKER_SYSTEM), llm.clone());
            l.max_summary_chars = 1500;
            l
        }),
        Box::new({
            let mut l =
                LlmLens::new(AgentView::Supervisor, tighten(SUPERVISOR_SYSTEM), llm.clone());
            l.max_summary_chars = 1000;
            l
        }),
        Box::new({
            let mut l = LlmLens::new(AgentView::Observer, tighten(OBSERVER_SYSTEM), llm);
            l.max_summary_chars = 1000;
            l
        }),
    ]
}

fn format_tool_outputs(out: &ProposalMessage) -> String {
    let mut s = String::new();
    for (i, r) in out.tool_outputs.iter().enumerate() {
        let name = out
            .proposal
            .tool_calls
            .get(i)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "?".into());
        match r {
            Ok(o) => {
                let snippet: String = o.stdout.chars().take(2000).collect();
                s.push_str(&format!(
                    "[{name} #{i} ok] implement step succeeded\n{}\n",
                    snippet
                ));
            }
            Err(e) => {
                s.push_str(&format!(
                    "[{name} #{i} error] implement step failed: {e}\n"
                ));
            }
        }
    }
    s
}

fn all_files_renamed(root: &std::path::Path) -> bool {
    for rel in ["src/lib.rs", "src/util.rs", "tests/it.rs"] {
        let body = match std::fs::read_to_string(root.join(rel)) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if body.contains("old_name") || !body.contains("new_name") {
            return false;
        }
    }
    true
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_multifile_refactor_with_personality_compaction() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL is not set");
            return;
        }
    };
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(cfg).unwrap());

    let work = fixture();
    let root = work.path().to_path_buf();
    let reg = registry(root.clone());

    // tmoe-history を起動。各ターンの raw + 3 view を本物の HistoryStore に書き、
    // Worker view summary を次ターンの context に注入する。3 view の Lens は **LlmLens**
    // (= 各エージェントが自分のパーソナリティで取捨選択して要約) を使う。
    let store_dir = tempdir().unwrap();
    let store = HistoryStore::open(store_dir.path()).unwrap();
    let feature = store
        .create_feature("rename old_name -> new_name across demo_pkg")
        .unwrap();
    let lenses = lenses(llm.clone());

    let task = ChatMessage::user(
        "Rename the identifier `old_name` to `new_name` in EVERY file under src/ and \
         tests/. Use grep_text(\"old_name\") first to enumerate every file. Then for \
         each file emit a patch_file call with search=\"old_name\", replace=\"new_name\", \
         and replace_all=true. Three files need updating: src/lib.rs, src/util.rs, \
         tests/it.rs. After all three patches, you may stop.",
    );

    // 直近 raw 出力 + 毎ターン Concierge が機械的に確認する **残タスク fact**。
    let mut last_raw_outputs = String::new();
    let mut last_raw_lens: Vec<usize> = Vec::new();

    // Concierge / Verifier 役を tmoe-core::ProgressVerifier の実装として外出しする。
    // これは task-specific (rename target) だが、**機構自体は汎用** で、tmoe-core の
    // 汎用 helper `run_worker_until_verified` から同じ trait オブジェクトを呼べる。
    struct RenameVerifier {
        root: std::path::PathBuf,
    }
    #[async_trait::async_trait]
    impl ProgressVerifier for RenameVerifier {
        async fn verify(&self) -> VerifierOutcome {
            use tmoe_tools::Tool;
            match (GrepTextTool { root: self.root.clone() })
                .call(&serde_json::json!({"pattern": "old_name"}))
                .await
            {
                Ok(o) if o.stdout.trim().is_empty() => VerifierOutcome::Done,
                Ok(o) => VerifierOutcome::Continue {
                    hint: format!(
                        "[concierge] occurrences of `old_name` STILL PRESENT — you MUST issue \
                         patch_file (search=\"old_name\", replace_all=true) for each file below:\n{}",
                        o.stdout
                    ),
                },
                Err(e) => VerifierOutcome::Continue {
                    hint: format!("[concierge] residual scan failed: {e}"),
                },
            }
        }
    }
    let verifier = RenameVerifier { root: root.clone() };

    for turn_idx in 0..8 {
        // 各ターン頭で機械的事実 (= 残タスク) を取得する。Done なら即脱出。
        let outstanding_msg = match verifier.verify().await {
            VerifierOutcome::Done => {
                eprintln!("verifier: rename complete before turn {turn_idx}");
                break;
            }
            VerifierOutcome::Continue { hint } => hint,
        };

        // Worker の自己 view は Worker 自身に渡さない (自己欺瞞を温存するため)。
        // 代わりに Supervisor view (= 失敗指摘) を渡し、何が残っているかを批判軸経由で示す。
        let mut messages = vec![task.clone()];
        if let Some(sup_view) = store
            .latest_level0(&feature.id, AgentView::Supervisor)
            .unwrap()
        {
            if !sup_view.summary.trim().is_empty() {
                messages.push(ChatMessage::user(format!(
                    "[supervisor critique so far — read carefully and address every point]\n{}",
                    sup_view.summary
                )));
            }
        }
        messages.push(ChatMessage::user(outstanding_msg.clone()));
        if !last_raw_outputs.is_empty() {
            messages.push(ChatMessage::user(format!(
                "[most recent tool outputs]\n{}",
                last_raw_outputs
            )));
        }
        let request_size: usize = messages.iter().map(|m| m.content.len()).sum();
        last_raw_lens.push(request_size);

        let out = single_agent_loop(
            AgentRole::Worker,
            WORKER_PROMPT,
            messages,
            llm.as_ref(),
            &reg,
        )
        .await
        .unwrap_or_else(|e| panic!("turn {turn_idx} loop failed: {e}"));

        eprintln!(
            "=== turn {turn_idx} (req~{}B, {} tool calls) ===\n{}\n=== outputs ===\n{}",
            request_size,
            out.proposal.tool_calls.len(),
            out.proposal.raw_text.chars().take(2000).collect::<String>(),
            format_tool_outputs(&out)
        );

        // raw + 3 view 並走 index に書き込む (= tmoe の本番フロー)。
        let body = format!(
            "implement turn {turn_idx}.\nworker output:\n{}\n\ntool outputs (impl results):\n{}",
            out.proposal.raw_text,
            format_tool_outputs(&out)
        );
        let raw = store
            .append_raw(AppendRaw {
                feature_id: feature.id.clone(),
                parent_id: None,
                kind: RawKind::Turn,
                body: body.clone(),
            })
            .unwrap();
        compact_turn_for_all(&store, &feature.id, &raw, &body, &lenses)
            .await
            .unwrap();

        last_raw_outputs = format_tool_outputs(&out);

        if all_files_renamed(&root) {
            eprintln!("multi-file rename completed at turn {turn_idx}");
            break;
        }
    }

    // ── 不変条件: request サイズはターンを重ねても爆発しない (compaction が効いている) ──
    if last_raw_lens.len() >= 4 {
        let early = last_raw_lens[1]; // turn 1 の req size
        let late = *last_raw_lens.last().unwrap();
        eprintln!("request sizes per turn: {:?}", last_raw_lens);
        // late <= 4 * early を許容。compaction が崩壊して爆発したら fail。
        assert!(
            late <= early * 4 + 4096,
            "request size grew unboundedly across turns: early={early} late={late} (compaction broke)"
        );
    }

    // ── 物理 fact ────────────────────────────────────────────────────
    let renamed = all_files_renamed(&root);
    if !renamed {
        for rel in ["src/lib.rs", "src/util.rs", "tests/it.rs"] {
            eprintln!(
                "--- post {rel} ---\n{}",
                std::fs::read_to_string(root.join(rel)).unwrap_or_default()
            );
        }
        // 3 view summary も dump して、Supervisor / Observer view が独立に伸びていることも示す。
        for ag in AgentView::all() {
            if let Some(node) = store.latest_level0(&feature.id, ag).unwrap() {
                eprintln!(
                    "--- {ag:?} view summary ({} chars) ---\n{}",
                    node.summary.len(),
                    node.summary
                );
            }
        }
        panic!("real LLM did not complete the multi-file rename");
    }

    // ── 3 view が独立に伸びていることを history 上で確認 ──
    let w = store
        .latest_level0(&feature.id, AgentView::Worker)
        .unwrap()
        .map(|n| n.summary)
        .unwrap_or_default();
    let s = store
        .latest_level0(&feature.id, AgentView::Supervisor)
        .unwrap()
        .map(|n| n.summary)
        .unwrap_or_default();
    let _o = store
        .latest_level0(&feature.id, AgentView::Observer)
        .unwrap()
        .map(|n| n.summary)
        .unwrap_or_default();
    assert!(
        !w.is_empty(),
        "worker view should have accumulated impl signals"
    );
    // Supervisor view は実装語彙には反応しないので、tool error が起きていなければ空のことがある。
    // 「empty なら直線縮退」ではなく「もし内容があるなら worker と異なる」を検証。
    if !s.is_empty() {
        assert_ne!(w, s, "worker and supervisor views collapsed");
    }
}
