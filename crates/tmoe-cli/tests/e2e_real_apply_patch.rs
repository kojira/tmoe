//! Real-LLM Worker が `apply_patch` ツールで複数ファイル diff を適用できるかの e2e。
//!
//! 仕様確認のスポット e2e。実 LLM (qwen3-coder-30b) に「指定の `*** Begin Patch ... ***
//! End Patch` ブロックを 1 度だけ apply_patch ツールで送れ。それから DONE」とだけ
//! 短く指示する。Worker が `{"tool":"apply_patch","args":{"text":"..."}}` を ToolCall
//! として正しく構築し、ツール側で実際にファイルが更新される (`old.txt` の "Hi" が
//! "Hello" に置き換わり、新規 `README.md` が作られる) ところまで確認する。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{single_agent_loop, AgentRole};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tools::{ApplyPatchTool, ToolRegistry};
use url::Url;

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_worker_invokes_apply_patch_tool() {
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
        request_timeout_secs: Some(120),
        retry_max_attempts: Some(0),
        codex_auth_path: None,
    };
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(cfg).unwrap());

    let dir = tempdir().unwrap();
    let workdir = dir.path();
    std::fs::write(workdir.join("greet.txt"), "Hi\n").unwrap();

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ApplyPatchTool {
        root: workdir.to_path_buf(),
    }));

    // Worker に「これをそのまま 1 度だけ送れ」と指示する。改行・引用は escape して 1 行に
    // 収まる JSON にしてあるので Worker が文字列としてコピーするだけでよい。
    let task = "Use the `apply_patch` tool exactly ONCE with these exact args:\n\
                {\"text\":\"*** Begin Patch\\n*** Add File: README.md\\n+# tmoe\\n*** Update File: greet.txt\\n@@\\n-Hi\\n+Hello\\n*** End Patch\\n\"}\n\
                Then on a new line write: DONE\n\
                No other tools, no prose.";

    let resp = llm
        .chat(tmoe_llm::ChatRequest {
            messages: vec![
                ChatMessage::system(tmoe_prompts::WORKER_SYSTEM),
                ChatMessage::user(task.to_string()),
            ],
            max_tokens: Some(220),
            temperature: Some(0.0),
            ..Default::default()
        })
        .await
        .expect("LLM chat");
    eprintln!("worker response:\n{}", resp.content);
    assert!(
        resp.content.contains("apply_patch") && resp.content.contains("Begin Patch"),
        "Worker did not emit apply_patch tool call: {}",
        resp.content
    );

    let pm = single_agent_loop(
        AgentRole::Worker,
        tmoe_prompts::WORKER_SYSTEM,
        vec![ChatMessage::user(task.to_string())],
        llm.as_ref(),
        &reg,
    )
    .await
    .expect("worker loop");
    let names: Vec<&str> = pm.proposal.tool_calls.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"apply_patch"), "tool_calls={names:?}");

    let success = pm.tool_outputs.iter().any(|r| {
        r.as_ref()
            .map(|o| o.stdout.contains("A README.md") && o.stdout.contains("M greet.txt"))
            .unwrap_or(false)
    });
    assert!(
        success,
        "apply_patch did not apply both hunks; tool_outputs={:?}",
        pm.tool_outputs
    );

    let g = std::fs::read_to_string(workdir.join("greet.txt")).unwrap();
    assert_eq!(g, "Hello\n");
    let r = std::fs::read_to_string(workdir.join("README.md")).unwrap();
    assert_eq!(r.trim(), "# tmoe");
}
