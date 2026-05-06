//! tmoe-llm: LLM 抽象レイヤ。
//!
//! バックエンド (llama.cpp / vLLM / LM Studio / Rapid-MLX / その他 OpenAI 互換) の差を吸収し、
//! `LlmClient` trait の単一インターフェースを上位に提供する。
//! 投機的推論はバックエンド能力検出により有効化され、未対応バックエンドでは no-op フォールバックする。

pub mod client;
pub mod error;
pub mod mock;
pub mod openai_compat;
pub mod types;

pub use client::LlmClient;
pub use error::{LlmError, Result};
pub use mock::{MockLlmClient, ScriptedTurn};
pub use openai_compat::{OpenAiCompatClient, OpenAiCompatConfig};
pub use types::{
    Backend, BackendCapabilities, ChatDelta, ChatMessage, ChatRequest, ChatResponse, Role, Usage,
};
