//! Real-LLM Worker が `skill` ツールでプロジェクトに置かれた SKILL.md を取り込めることを確認する e2e。
//!
//! 仕様確認のスポット e2e。実 LLM (qwen3-coder-30b) に「skill ツールを 1 度だけ使って
//! `pirate-style` を読み込め。それから DONE」と短く指示し、Worker が
//! `{"tool":"skill","args":{"name":"pirate-style"}}` を ToolCall として正しく構築することを
//! assert する。skill 本文 (`yarrr-marker-XYZ`) が ToolOutput に流れることまで確認する
//! ことで、registry 登録 → tool dispatch → frontmatter パースの一気通貫が壊れていない
//! ことを保証する。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_cli::skill_tool::{SkillRegistry, SkillTool};
use tmoe_core::{single_agent_loop, AgentRole};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tools::ToolRegistry;
use url::Url;

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_worker_invokes_skill_tool() {
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

    // workdir/.tmoe/skills/pirate-style/SKILL.md を仕込む。本文の中にユニークな
    // marker を含めて、ToolOutput がそれを含んでいるかで「skill が実際に読まれた」ことを
    // 判定する。
    let dir = tempdir().unwrap();
    let workdir = dir.path();
    let skill_dir = workdir.join(".tmoe/skills/pirate-style");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: pirate-style\ndescription: Speak like a pirate.\n---\n\n# Pirate style\n\nyarrr-marker-XYZ\n",
    )
    .unwrap();

    let mut reg = ToolRegistry::new();
    let registry = Arc::new(SkillRegistry::scan(workdir, None));
    assert!(
        registry.get("pirate-style").is_some(),
        "scan failed to pick up the SKILL.md fixture"
    );
    reg.register(Arc::new(SkillTool {
        registry: registry.clone(),
    }));

    let task = "Use the `skill` tool exactly ONCE with these exact args:\n\
                {\"name\":\"pirate-style\"}\n\
                Then on a new line write: DONE\n\
                No other tools, no prose.";

    // (a) raw chat で Worker が skill ツール呼び出し JSON を文字列として組み立てるかを観察。
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
        resp.content.contains("\"skill\"") && resp.content.contains("pirate-style"),
        "Worker did not emit skill tool call: {}",
        resp.content
    );

    // (b) Tool 経路で 1 度起動したとき、本文の marker が ToolOutput に乗ること。
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
    assert!(names.contains(&"skill"), "tool_calls={names:?}");

    let yarrr = pm.tool_outputs.iter().any(|r| {
        r.as_ref()
            .map(|o| o.stdout.contains("yarrr-marker-XYZ"))
            .unwrap_or(false)
    });
    assert!(
        yarrr,
        "skill body did not propagate; tool_outputs={:?}",
        pm.tool_outputs
    );
}
