//! Real-LLM Worker が `plan_enter` → `plan_exit` の 2 ステップを順に呼べるかの e2e。
//!
//! 仕様確認のスポット e2e。実 LLM (qwen3-coder-30b) に「指定の plan markdown を
//! plan_enter に通し、続けて plan_exit を呼べ。それから DONE」と短く指示する。
//! ScriptedAsker で plan_exit は yes を返すよう仕込み、Worker が ToolCall を 2 件
//! (plan_enter / plan_exit) として正しく構築すること、その結果 plan ファイルが
//! 永続化されることを assert する。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_cli::plan_tool::{plan_path, PlanEnterTool, PlanExitTool};
use tmoe_cli::question_tool::ScriptedAsker;
use tmoe_core::{single_agent_loop, AgentRole};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tools::ToolRegistry;
use url::Url;

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_worker_invokes_plan_enter_then_plan_exit() {
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
    };
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(cfg).unwrap());

    let dir = tempdir().unwrap();
    let workdir = dir.path();
    let feature_id = "EFE2EPLAN".to_string();

    let asker = Arc::new(ScriptedAsker::new(vec![vec!["yes".into()]]));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(PlanEnterTool {
        workdir: workdir.to_path_buf(),
        feature_id: feature_id.clone(),
    }));
    reg.register(Arc::new(PlanExitTool {
        workdir: workdir.to_path_buf(),
        feature_id: feature_id.clone(),
        asker,
    }));

    let task = "Use the `plan_enter` tool exactly ONCE with these exact args:\n\
                {\"plan\":\"1. read foo.rs\\n2. patch foo.rs\\n3. run tests\",\"title\":\"Refactor foo\"}\n\
                Then call `plan_exit` exactly ONCE with empty args:\n\
                {}\n\
                Then on a new line write: DONE\n\
                No other tools, no prose.";

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
    eprintln!("tool_calls: {names:?}");
    assert!(names.contains(&"plan_enter"), "no plan_enter in tool_calls={names:?}");
    assert!(names.contains(&"plan_exit"), "no plan_exit in tool_calls={names:?}");

    // plan ファイルが書かれたか
    let path = plan_path(workdir, &feature_id);
    assert!(path.exists(), "plan file not written: {}", path.display());
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("Refactor foo"), "plan body missing title: {body}");
    assert!(body.contains("read foo.rs"), "plan body missing step 1: {body}");

    // plan_exit が "approved" を返しているか
    let approved = pm.tool_outputs.iter().any(|r| {
        r.as_ref()
            .map(|o| o.stdout.contains("approved"))
            .unwrap_or(false)
    });
    assert!(
        approved,
        "plan_exit did not return approved; tool_outputs={:?}",
        pm.tool_outputs
    );
}
