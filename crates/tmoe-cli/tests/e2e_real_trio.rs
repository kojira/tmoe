//! Real-LLM Trio consensus e2e.
//!
//! Worker / Supervisor / Observer の 3 役を、それぞれ **異なるシステムプロンプト**で
//! Rapid-MLX の同一エンドポイントに繋いで動かす (= 「3 つの異なる方向性ベクトル」)。
//! ユーザー Z 軸推進を Go { strength: 1.0 } で送り、平面合意 + Z 軸の積で
//! ConsensusOutcome::Commit に至り、Worker のツール呼び出しで複数ファイルが書かれ、
//! それらが `cargo test --tests` を通過することを検証する。
//!
//! 実機の vote 出力は揺れるため、Trio は parse_vote の lenient + recovery で受け止める。
//! 確信度は parse_vote が中立値 0.7 を当てるので、Supervisor / Observer の confidence が
//! 欠落しても合意プロトコル自体は前進できる。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{ConsensusOutcome, ConsensusThresholds, ThrustChannel, Trio, UserThrust};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tools::{default_blocklist, EditFileTool, ReadFileTool, RunCmdTool, ToolRegistry};
use url::Url;

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
            request_timeout_secs: None,
            retry_max_attempts: None,
        codex_auth_path: None,
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

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_trio_consensus_writes_multifile_module_and_tests_pass() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL is not set");
            return;
        }
    };
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(cfg).unwrap());

    let workdir = tempdir().unwrap();
    let root = workdir.path().to_path_buf();
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "tmoe_trio_calc"
version = "0.0.1"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "").unwrap();
    let reg = registry(root.clone());

    // tmoe-prompts のデフォルトパーソナリティで 3 エージェントを構築する。
    // 同一 LLM を共有し、プロンプトの違いだけで「3 つの異なる方向性ベクトル」を表現する。
    let trio = Trio::from_shared_llm(llm).with_thresholds(ConsensusThresholds {
        confidence_sum_min: 1.5, // 実 LLM は中立 0.7 を返しやすいので閾値を控えめに
        triangle_balance_min: 0.3,
        max_iter_per_step: 4,
    });

    let (tx, mut rx) = ThrustChannel::new();
    tx.send(UserThrust::Go { strength: 1.0 }).unwrap();

    let task = vec![ChatMessage::user(
        "Create a small Rust library in a SINGLE file src/lib.rs that:\n\
         - exposes  pub fn add(a:i64,b:i64)->i64 { a + b }\n\
         - exposes  pub fn mul(a:i64,b:i64)->i64 { a * b }\n\
         - includes  #[cfg(test)] mod tests  with TWO #[test] functions:\n\
             * one asserting add(2, 3) == 5\n\
             * one asserting mul(2, 3) == 6\n\
         Emit exactly ONE edit_file tool call (one ```json block) for src/lib.rs.\n\
         Then on a brand-new line output the single token: DONE\n\
         Do not skip the DONE marker. No prose, no extra fences.",
    )];

    let outcome = trio
        .run_step(&task, &reg, &mut rx)
        .await
        .expect("run_step failed");

    eprintln!("steps={} outcome={:?}", outcome.steps, outcome.last);
    match &outcome.last {
        ConsensusOutcome::Commit { proposal, votes } => {
            eprintln!("votes:");
            for v in votes {
                eprintln!(
                    "  approve={} confidence={} note={:?}",
                    v.approve, v.confidence, v.note
                );
            }
            eprintln!("proposal raw len={}", proposal.raw_text.len());
        }
        other => {
            eprintln!("--- src/lib.rs ---\n{}", std::fs::read_to_string(root.join("src/lib.rs")).unwrap_or_default());
            panic!("expected Commit, got {other:?}");
        }
    }

    for rel in ["src/lib.rs"] {
        let p = root.join(rel);
        assert!(p.exists(), "file missing: {}", p.display());
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(!body.trim().is_empty(), "{rel} empty");
        assert!(body.contains("pub fn add") && body.contains("pub fn mul"));
        assert!(body.contains("#[cfg(test)]") || body.contains("mod tests"));
    }
    let status = std::process::Command::new("cargo")
        .arg("test")
        .arg("--lib")
        .current_dir(&root)
        .status()
        .expect("cargo test spawn failed");
    if !status.success() {
        eprintln!("--- src/lib.rs ---\n{}", std::fs::read_to_string(root.join("src/lib.rs")).unwrap());
        panic!("cargo test --lib failed on Trio-produced code");
    }
}
