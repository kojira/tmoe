//! Real-LLM e2e: 単一ファイルのリファクタリング (最小実証)。
//!
//! 実 LLM が新ツール (grep_text + patch_file) を**チェーンで使えるか**だけを最小限示す
//! e2e。multi-file かつ tmoe-history (3 並走 index + 逐次コンパクション) を使う本格版は
//! `e2e_real_refactor_compacted.rs` 側で行う。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{single_agent_loop, AgentRole, ProposalMessage};
use tmoe_llm::{Backend, ChatMessage, OpenAiCompatClient, OpenAiCompatConfig};
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

Output ONE OR MORE ```json blocks per turn, then a single line: DONE if you are
finished. If not finished, omit DONE and stop after the tool calls.
No prose, no extra commentary.
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
            request_timeout_secs: None,
            retry_max_attempts: None,
        codex_auth_path: None,
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
    // 単一ファイルに同じ識別子が複数回出現する状況を作る。
    // Worker は grep_text → patch_file (replace_all=true) → grep_text で完遂する。
    let d = tempdir().unwrap();
    let p = d.path();
    std::fs::create_dir_all(p.join("src")).unwrap();
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
        "pub fn old_name() -> i32 { 41 }\n\n\
         pub fn caller() -> i32 { old_name() + 1 }\n\n\
         #[cfg(test)]\n\
         mod tests {\n\
             use super::*;\n\
             #[test]\n\
             fn it_works() { assert_eq!(old_name(), 41); }\n\
         }\n",
    )
    .unwrap();
    d
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
                s.push_str(&format!("[{name} #{i} ok]\n{}\n", snippet));
            }
            Err(e) => {
                s.push_str(&format!("[{name} #{i} error] {e}\n"));
            }
        }
    }
    s
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_refactors_identifier_via_grep_then_patch() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL is not set");
            return;
        }
    };
    let llm = OpenAiCompatClient::new(cfg).unwrap();

    let d = fixture();
    let root = d.path().to_path_buf();
    let reg = registry(root.clone());

    let task = ChatMessage::user(
        "Rename the identifier `old_name` to `new_name` in src/lib.rs. \
         The identifier appears multiple times in that single file. \
         Use grep_text(\"old_name\") to confirm where it appears, then issue ONE \
         patch_file call with search=\"old_name\", replace=\"new_name\", and \
         replace_all=true. After patching, you may finish.",
    );

    // Rolling history: 直近 N ターンの (assistant + user feedback) のみ保持し、
    // request body がターン数に応じて爆発するのを防ぐ。実 LLM のサーバー側
    // タイムアウト (Rapid-MLX 既定 300s) を回避する。
    const HISTORY_KEEP_TURNS: usize = 1;
    let mut recent: Vec<(ChatMessage, ChatMessage)> = Vec::new();
    let mut last_outputs: Vec<String> = Vec::new();

    for turn_idx in 0..8 {
        let mut messages = vec![task.clone()];
        for (assistant, user) in &recent {
            messages.push(assistant.clone());
            messages.push(user.clone());
        }
        let out = single_agent_loop(
            AgentRole::Worker,
            WORKER_PROMPT,
            messages,
            &llm,
            &reg,
        )
        .await
        .unwrap_or_else(|e| panic!("turn {turn_idx} loop failed: {e}"));
        eprintln!(
            "=== turn {turn_idx} ({} tool calls, done={}) ===\n{}\n=== outputs ===\n{}",
            out.proposal.tool_calls.len(),
            out.proposal.done,
            out.proposal.raw_text.chars().take(2000).collect::<String>(),
            format_tool_outputs(&out)
        );
        let formatted = format_tool_outputs(&out);
        last_outputs.push(formatted.clone());
        recent.push((
            ChatMessage::assistant(&out.proposal.raw_text),
            ChatMessage::user(formatted),
        ));
        if recent.len() > HISTORY_KEEP_TURNS {
            recent.remove(0);
        }
        if files_renamed(&root) {
            eprintln!("rename completed at turn {turn_idx}");
            break;
        }
    }

    // ── 物理検証 ─────────────────────────────────────────────────────
    let assertion = files_renamed(&root);
    if !assertion {
        eprintln!(
            "--- post src/lib.rs ---\n{}",
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap_or_default()
        );
        for (i, log) in last_outputs.iter().enumerate() {
            eprintln!("--- turn {i} outputs ---\n{log}");
        }
        panic!("real LLM did not complete the single-file rename");
    }
}

fn files_renamed(root: &std::path::Path) -> bool {
    let body = match std::fs::read_to_string(root.join("src/lib.rs")) {
        Ok(s) => s,
        Err(_) => return false,
    };
    !body.contains("old_name") && body.matches("new_name").count() >= 3
}
