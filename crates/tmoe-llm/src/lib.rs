//! tmoe-llm: LLM 抽象レイヤ。
//!
//! バックエンド (llama.cpp / vLLM / LM Studio / Rapid-MLX / その他 OpenAI 互換) の差を吸収し、
//! `LlmClient` trait の単一インターフェースを上位に提供する。
//! 投機的推論はバックエンド能力検出により有効化され、未対応バックエンドでは no-op フォールバックする。

pub mod client;
pub mod codex;
pub mod error;
pub mod mock;
pub mod openai_compat;
pub mod types;

pub use client::LlmClient;
pub use codex::{
    build_authorize_url, default_auth_path, exchange_code_for_tokens,
    exchange_code_for_tokens_default, extract_account_id, extract_account_id_from_tokens,
    generate_state, load_codex_auth, parse_jwt_claims, refresh_access_token, save_codex_auth,
    token_response_to_auth, CodexAuth, IdTokenClaims, PkceCodes, TokenResponse,
    CLIENT_ID as CODEX_CLIENT_ID, CODEX_API_ENDPOINT, ISSUER as CODEX_ISSUER,
    OAUTH_PORT as CODEX_OAUTH_PORT, OAUTH_REDIRECT_PATH as CODEX_OAUTH_REDIRECT_PATH,
};
pub use error::{LlmError, Result};
pub use mock::{MockLlmClient, ScriptedTurn};
pub use openai_compat::{ClientDescription, HealthStatus, OpenAiCompatClient, OpenAiCompatConfig};
pub use types::{
    Backend, BackendCapabilities, ChatDelta, ChatMessage, ChatRequest, ChatResponse, Role, Usage,
};
