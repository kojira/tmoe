//! Gated end-to-end: 実 LLM に「FizzBuzz を Rust で書いて、`src/fizzbuzz.rs` に保存して」と
//! 投げ、Worker が tool 呼び出しでファイルを書き、内容が妥当かを検証する。
//!
//! `TMOE_E2E_LLM_URL` がセットされた時のみ走る。Trio の合意プロトコル全体を実 LLM に
//! 通すと vote 形式 (JSON) のパースで揺れが出るため、ここでは **Worker 単体の loop**
//! (= `single_agent_loop`) を実 LLM で走らせる。
//!
//! Trio フルパスの実 LLM テストは将来追加するが、まず「実 LLM がツールを呼んでファイルを
//! 作れる」ところを実証する。

use std::env;
use std::sync::Arc;
use tempfile::tempdir;
use tmoe_core::{single_agent_loop, AgentRole};
use tmoe_llm::{
    Backend, ChatMessage, OpenAiCompatClient, OpenAiCompatConfig,
};
use tmoe_tools::{default_blocklist, EditFileTool, ReadFileTool, RunCmdTool, ToolRegistry};
use url::Url;

const WORKER_PROMPT: &str = r#"You are a coding agent. Emit tool calls as a single fenced ```json block.

Available tools (note JSON-escaping rules):
  - {"tool":"edit_file","args":{"path":"<relative path>","content":"<file content>"}}
  - {"tool":"read_file","args":{"path":"<relative path>"}}
  - {"tool":"run_cmd","args":{"program":"<bin>","args":["..."]}}

CRITICAL JSON RULES (this is JSON, not a code block):
  1. Every double quote inside a string MUST be escaped as \" — for Rust code that contains
     "Foo" you must write \"Foo\" inside the JSON string.
  2. Every newline inside a string value MUST be written as \n (two characters: backslash, n).
     Do NOT put real newlines inside a JSON string value.
  3. Backslashes must be doubled to \\.
  4. The JSON must parse with serde_json::from_str.

After the JSON block, output a single line: DONE

Do not explain. Output only the JSON block followed by DONE.
"#;

fn config_from_env() -> Option<OpenAiCompatConfig> {
    let base = env::var("TMOE_E2E_LLM_URL").ok()?;
    let main_model =
        env::var("TMOE_E2E_LLM_MODEL").unwrap_or_else(|_| "qwen2.5-coder-32b-instruct".into());
    let draft_model = env::var("TMOE_E2E_LLM_DRAFT").ok();
    let backend = match env::var("TMOE_E2E_LLM_BACKEND")
        .unwrap_or_else(|_| "llama_cpp".into())
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
        draft_model,
        spec_n_max: Some(16),
        api_key: env::var("TMOE_E2E_LLM_API_KEY").ok(),
    })
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_writes_fizzbuzz_via_worker_tool_call() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL is not set");
            return;
        }
    };
    let llm = OpenAiCompatClient::new(cfg).unwrap();

    let workdir = tempdir().unwrap();
    let root = workdir.path().to_path_buf();
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EditFileTool { root: root.clone() }));
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(RunCmdTool {
        root: root.clone(),
        blocklist: default_blocklist(),
    }));

    let task = ChatMessage::user(
        "Write a Rust function `fizzbuzz(n: u32) -> Vec<String>` that returns the FizzBuzz \
         strings for 1..=n. Save it to src/fizzbuzz.rs (no module declarations needed). \
         When done, output DONE on its own line.",
    );

    let out = single_agent_loop(
        AgentRole::Worker,
        WORKER_PROMPT,
        vec![task],
        &llm,
        &reg,
    )
    .await
    .expect("worker loop failed");

    assert!(
        !out.proposal.tool_calls.is_empty(),
        "real LLM produced no tool calls; raw text was:\n{}",
        out.proposal.raw_text
    );
    let edited = out
        .proposal
        .tool_calls
        .iter()
        .any(|c| c.name == "edit_file");
    assert!(
        edited,
        "real LLM did not call edit_file; tool calls: {:?}",
        out.proposal
            .tool_calls
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
    let path = root.join("src/fizzbuzz.rs");
    assert!(
        path.exists(),
        "expected file was not written: {} ; raw text:\n{}",
        path.display(),
        out.proposal.raw_text
    );
    let body = std::fs::read_to_string(&path).unwrap();
    let lower = body.to_lowercase();
    assert!(
        lower.contains("fn fizzbuzz") && lower.contains("fizz") && lower.contains("buzz"),
        "file does not look like fizzbuzz:\n{body}"
    );
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_builds_multifile_library_and_tests_pass() {
    // Worker に 3 ファイルからなる Cargo ライブラリを 1 ターンで作らせる:
    //   src/lib.rs              -- pub mod stack; pub use stack::Stack;
    //   src/stack.rs            -- pub struct Stack<T> { ... } + impl
    //   tests/stack_test.rs     -- 統合テスト
    // 完成後に `cargo test` をテスト側で実行し、LLM が書いたテストが実際に通ることを検証する。
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL is not set");
            return;
        }
    };
    let llm = OpenAiCompatClient::new(cfg).unwrap();

    let workdir = tempdir().unwrap();
    let root = workdir.path().to_path_buf();
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "tmoe_e2e_stack"
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

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EditFileTool { root: root.clone() }));
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(RunCmdTool {
        root: root.clone(),
        blocklist: default_blocklist(),
    }));

    let task = ChatMessage::user(
        r#"Create a small Rust library that provides a generic LIFO stack across multiple files.
Emit the files using THREE separate edit_file tool calls (each in its own ```json fenced block):

1. src/lib.rs
   - pub mod stack;
   - pub use stack::Stack;

2. src/stack.rs
   - pub struct Stack<T> { items: Vec<T> }
   - impl<T> Stack<T> with new() -> Self, push(&mut self, T), pop(&mut self) -> Option<T>,
     len(&self) -> usize, is_empty(&self) -> bool, peek(&self) -> Option<&T>.

3. tests/stack_test.rs
   - A cargo integration test using `tmoe_e2e_stack::Stack`. Cover:
     * a fresh stack is empty and len == 0,
     * push then peek returns the last value (compare with assert_eq!),
     * push three items, pop returns them in LIFO order, then is_empty.

Constraints:
- Output exactly three fenced ```json blocks, one per file, in the order above.
- Then a single line: DONE
- No prose, no extra commentary.
"#,
    );

    let out = single_agent_loop(
        AgentRole::Worker,
        WORKER_PROMPT,
        vec![task],
        &llm,
        &reg,
    )
    .await
    .expect("worker loop failed");

    let edited_paths: Vec<String> = out
        .proposal
        .tool_calls
        .iter()
        .filter(|c| c.name == "edit_file")
        .filter_map(|c| c.args.get("path").and_then(|p| p.as_str()).map(String::from))
        .collect();
    eprintln!("=== raw LLM output ({} chars) ===\n{}\n=== end raw ===", out.proposal.raw_text.len(), out.proposal.raw_text);
    eprintln!("edited paths: {:?}", edited_paths);
    let need = ["src/lib.rs", "src/stack.rs", "tests/stack_test.rs"];
    let missing: Vec<&str> = need
        .iter()
        .copied()
        .filter(|p| !edited_paths.iter().any(|x| x == p))
        .collect();
    assert!(
        missing.is_empty(),
        "missing files {:?}; called paths: {:?}",
        missing,
        edited_paths
    );
    let oks = out.tool_outputs.iter().filter(|r| r.is_ok()).count();
    assert!(
        oks >= 3,
        "expected at least 3 successful edit_file calls, got {oks}; outputs: {:?}",
        out.tool_outputs
            .iter()
            .map(|r| r.is_ok())
            .collect::<Vec<_>>()
    );

    // 物理ファイル存在確認。
    for rel in ["src/lib.rs", "src/stack.rs", "tests/stack_test.rs"] {
        let path = root.join(rel);
        assert!(path.exists(), "expected file missing: {}", path.display());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.trim().is_empty(), "file is empty: {rel}");
    }

    // LLM が書いた lib + test を実際にコンパイル + 実行する。これが通れば、
    // - lib.rs <-> stack.rs のモジュール解決
    // - tests/stack_test.rs から外部 crate として `tmoe_e2e_stack::Stack` が使える
    // - 統合テストの assert_eq! が成立する
    // を全て満たすことになる。
    let status = std::process::Command::new("cargo")
        .arg("test")
        .arg("--tests")
        .current_dir(&root)
        .status()
        .expect("cargo test failed to spawn");
    if !status.success() {
        eprintln!("--- src/lib.rs ---\n{}", std::fs::read_to_string(root.join("src/lib.rs")).unwrap_or_default());
        eprintln!("--- src/stack.rs ---\n{}", std::fs::read_to_string(root.join("src/stack.rs")).unwrap_or_default());
        eprintln!("--- tests/stack_test.rs ---\n{}", std::fs::read_to_string(root.join("tests/stack_test.rs")).unwrap_or_default());
        panic!("cargo test failed on LLM-generated multi-file code");
    }
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_llm_writes_then_cargo_check_passes() {
    // 「書いた」だけでなく Rust のコンパイルが通ることを確認する強めのバージョン。
    // 一時 Cargo プロジェクトを作って LLM に lib.rs の中身を書かせる。
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL is not set");
            return;
        }
    };
    let llm = OpenAiCompatClient::new(cfg).unwrap();

    let workdir = tempdir().unwrap();
    let root = workdir.path().to_path_buf();

    // Cargo プロジェクトの足場を用意。
    std::fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "tmoe_e2e_fizzbuzz"
version = "0.0.1"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "").unwrap();

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EditFileTool { root: root.clone() }));
    reg.register(Arc::new(ReadFileTool { root: root.clone() }));
    reg.register(Arc::new(RunCmdTool {
        root: root.clone(),
        blocklist: default_blocklist(),
    }));

    let task = ChatMessage::user(
        "Edit src/lib.rs so it defines `pub fn fizzbuzz(n: u32) -> Vec<String>` returning \
         the classic FizzBuzz sequence for 1..=n (\"Fizz\", \"Buzz\", \"FizzBuzz\", or the \
         number as a string otherwise). Output a single edit_file tool call, then DONE.",
    );

    let out = single_agent_loop(
        AgentRole::Worker,
        WORKER_PROMPT,
        vec![task],
        &llm,
        &reg,
    )
    .await
    .expect("worker loop failed");
    assert!(
        out.tool_outputs.iter().any(|r| r.is_ok()),
        "no successful tool execution; outputs: {:?}",
        out.tool_outputs
            .iter()
            .map(|r| r.is_ok())
            .collect::<Vec<_>>()
    );

    // cargo check で Rust 的に妥当か検証。
    let status = std::process::Command::new("cargo")
        .arg("check")
        .current_dir(&root)
        .status()
        .expect("cargo check failed to spawn");
    assert!(
        status.success(),
        "cargo check failed on LLM-generated code; lib.rs was:\n{}",
        std::fs::read_to_string(root.join("src/lib.rs")).unwrap_or_default()
    );
}
