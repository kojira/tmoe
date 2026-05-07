//! Real-LLM Worker が `question` ツールを実際に呼べることを確認する e2e。
//!
//! 仕様確認のためだけのスポット e2e。実 LLM (qwen3-coder-30b) に
//! 「ユーザに 1 つだけ確認を取れ。それから DONE」と短く指示し、Worker が
//! `{"tool":"question","args":{"questions":[...]}}` を ToolCall として正しく
//! 構築することを assert する。回答は ScriptedAsker で決定論的に "yes" を返す。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_cli::question_tool::{QuestionTool, ScriptedAsker};
use tmoe_core::{single_agent_loop, AgentRole};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tools::ToolRegistry;
use url::Url;

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_worker_invokes_question_tool() {
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

    let _dir = tempdir().unwrap();
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(QuestionTool::new(Arc::new(ScriptedAsker::new(vec![
        vec!["yes".to_string()],
    ])))));

    let task = "Use the `question` tool exactly ONCE with these exact args:\n\
                {\"questions\":[{\"question\":\"Proceed with implementation?\",\"options\":[\"yes\",\"no\"]}]}\n\
                Then on a new line write: DONE\n\
                No other tools, no prose.";
    let resp = llm
        .chat(tmoe_llm::ChatRequest {
            messages: vec![
                ChatMessage::system(tmoe_prompts::WORKER_SYSTEM),
                ChatMessage::user(task.to_string()),
            ],
            max_tokens: Some(150),
            temperature: Some(0.0),
            ..Default::default()
        })
        .await
        .expect("LLM chat");
    eprintln!("worker response:\n{}", resp.content);
    assert!(
        resp.content.contains("\"question\"") && resp.content.contains("\"questions\""),
        "Worker did not emit question tool call"
    );

    // 実際に Tool 経路で 1 度起動して "yes" を受け取れるか。
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
    assert!(names.contains(&"question"), "tool_calls={names:?}");

    let ok = pm.tool_outputs.iter().any(|r| {
        r.as_ref()
            .map(|o| o.stdout.contains("yes"))
            .unwrap_or(false)
    });
    assert!(ok, "scripted asker reply did not propagate; tool_outputs={:?}", pm.tool_outputs);
}
