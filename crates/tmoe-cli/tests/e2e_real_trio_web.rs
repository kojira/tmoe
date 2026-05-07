//! Real-LLM Trio: web_fetch / web_search 経路の結線確認。
//!
//! `obscura` 実バイナリが PATH に無い環境でも、結線そのもの (Worker → ToolRegistry →
//! 子プロセス) は検証したい。そこで本テストはシナリオごとに **obscura のスタブ
//! シェルスクリプト** を生成し、`WebFetchTool::with_bin` / `WebSearchTool::with_bin` で
//! 直接そこを差す。スタブは引数を捨てて canned な plain text を stdout に吐くだけ。
//!
//! 検証点:
//!   - Worker が web_fetch を ToolCall として正しく構築できる
//!   - 子プロセス (スタブ) が `obscura fetch <URL> --dump text` の引数で呼ばれる
//!   - 標準出力が ToolOutput.stdout に乗る

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{single_agent_loop, AgentRole};
use tmoe_llm::{Backend, ChatMessage, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use tmoe_tools::{
    default_blocklist, EditFileTool, GrepTextTool, ListFilesTool, PatchFileTool, ReadFileTool,
    RunCmdTool, ToolRegistry, WebFetchTool, WebSearchTool,
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
            request_timeout_secs: None,
            retry_max_attempts: None,
        codex_auth_path: None,
    })
}

fn obscura_stub(parent: &std::path::Path, canned_stdout: &str) -> PathBuf {
    let stub = parent.join("obscura_stub.sh");
    // どんな引数で呼ばれても、canned_stdout を吐いて成功する。
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}.argv'\ncat <<'EOF'\n{}\nEOF\nexit 0\n",
            stub.display(),
            canned_stdout
        ),
    )
    .unwrap();
    let mut perm = fs::metadata(&stub).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&stub, perm).unwrap();
    stub
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
#[cfg(unix)]
async fn real_llm_worker_invokes_web_fetch_via_obscura_stub() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL not set");
            return;
        }
    };
    let llm: Arc<dyn LlmClient> = Arc::new(OpenAiCompatClient::new(cfg).unwrap());

    let dir = tempdir().unwrap();
    let canned = "Example Domain\nThis page hosts the literal token TMOE_WEB_E2E_OK.";
    let stub_path = obscura_stub(dir.path(), canned);

    // Worker の作業 root と obscura スタブは別ディレクトリ。
    let work = dir.path().join("work");
    fs::create_dir_all(&work).unwrap();

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ReadFileTool { root: work.clone() }));
    reg.register(Arc::new(EditFileTool { root: work.clone() }));
    reg.register(Arc::new(PatchFileTool { root: work.clone() }));
    reg.register(Arc::new(ListFilesTool { root: work.clone() }));
    reg.register(Arc::new(GrepTextTool { root: work.clone() }));
    reg.register(Arc::new(RunCmdTool {
        root: work.clone(),
        blocklist: default_blocklist(),
    }));
    reg.register(Arc::new(WebFetchTool::with_bin(stub_path.clone())));
    reg.register(Arc::new(WebSearchTool::with_bin(stub_path.clone())));

    let task = "Use the web_fetch tool to retrieve https://example.com and quote the literal token \
                that appears on the page (it begins with TMOE_). After the tool call, write the \
                quoted token in a single edit_file call to work/found.txt. Then output DONE.";

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
        pm.proposal
            .tool_calls
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
    );
    eprintln!("proposal raw (head 400 chars): {}", &pm.proposal.raw_text.chars().take(400).collect::<String>());

    let names: Vec<&str> = pm.proposal.tool_calls.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.iter().any(|n| *n == "web_fetch"),
        "Worker did not call web_fetch. tool_calls={names:?}"
    );

    // スタブが呼ばれた → argv 記録ファイルが残るはず。
    let argv_log = format!("{}.argv", stub_path.display());
    let recorded = fs::read_to_string(&argv_log)
        .unwrap_or_else(|_| panic!("obscura stub argv log missing at {argv_log}"));
    let lines: Vec<&str> = recorded.lines().collect();
    assert_eq!(lines.first().map(|s| *s), Some("fetch"), "obscura was not called as `fetch`: {lines:?}");
    assert!(lines.get(1).map(|s| s.starts_with("https://")).unwrap_or(false),
        "second arg should be the URL: {lines:?}");
    assert_eq!(lines.get(2).map(|s| *s), Some("--dump"), "expected --dump flag: {lines:?}");
    assert_eq!(lines.get(3).map(|s| *s), Some("text"), "expected text dump: {lines:?}");
}
