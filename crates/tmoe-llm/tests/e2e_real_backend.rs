//! Gated real-backend smoke test.
//!
//! 環境変数 `TMOE_E2E_LLM_URL` がセットされた時のみ実行する。例:
//! ```
//! TMOE_E2E_LLM_URL=http://127.0.0.1:8080/v1 \
//! TMOE_E2E_LLM_MODEL=qwen2.5-coder-32b-instruct \
//! cargo test -p tmoe-llm --test e2e_real_backend -- --ignored
//! ```
//!
//! Rapid-MLX (Apple Silicon) 等の OpenAI 互換サーバーで動くことを検証する。

use futures::StreamExt;
use std::env;
use tmoe_llm::{
    Backend, ChatDelta, ChatMessage, ChatRequest, LlmClient, OpenAiCompatClient, OpenAiCompatConfig,
};
use url::Url;

fn config_from_env() -> Option<OpenAiCompatConfig> {
    let base = env::var("TMOE_E2E_LLM_URL").ok()?;
    let main_model =
        env::var("TMOE_E2E_LLM_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());
    let draft_model = env::var("TMOE_E2E_LLM_DRAFT").ok();
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
        draft_model,
        spec_n_max: Some(16),
        api_key: env::var("TMOE_E2E_LLM_API_KEY").ok(),
            request_timeout_secs: None,
            retry_max_attempts: None,
    })
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_backend_chat_returns_nonempty_text() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL is not set");
            return;
        }
    };
    let client = OpenAiCompatClient::new(cfg).unwrap();
    let resp = client
        .chat(ChatRequest {
            messages: vec![
                ChatMessage::system("You are a terse assistant. Reply with a single short word."),
                ChatMessage::user("Say 'pong' and nothing else."),
            ],
            max_tokens: Some(16),
            temperature: Some(0.0),
            ..Default::default()
        })
        .await
        .expect("real backend chat failed");
    let lower = resp.content.to_lowercase();
    assert!(
        lower.contains("pong"),
        "expected 'pong' in response, got: {:?}",
        resp.content
    );
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
async fn real_backend_chat_stream_emits_tokens_then_done() {
    let cfg = match config_from_env() {
        Some(c) => c,
        None => {
            eprintln!("skipping: TMOE_E2E_LLM_URL is not set");
            return;
        }
    };
    let client = OpenAiCompatClient::new(cfg).unwrap();
    let mut stream = client
        .chat_stream(ChatRequest {
            messages: vec![ChatMessage::user("Count: one, two, three.")],
            max_tokens: Some(32),
            temperature: Some(0.0),
            ..Default::default()
        })
        .await
        .expect("stream open failed");
    let mut text = String::new();
    let mut got_done = false;
    while let Some(d) = stream.next().await {
        match d.expect("stream item error") {
            ChatDelta::Token(t) => text.push_str(&t),
            ChatDelta::Done { .. } => {
                got_done = true;
                break;
            }
        }
    }
    assert!(!text.is_empty(), "stream produced no tokens");
    assert!(got_done, "stream never emitted Done");
}
