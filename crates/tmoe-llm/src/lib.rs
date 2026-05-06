//! tmoe-llm: LLM 抽象レイヤ。
//!
//! バックエンド (llama.cpp / vLLM / LM Studio / Rapid-MLX / その他 OpenAI 互換) の差を吸収し、
//! `LlmClient` trait の単一インターフェースを上位に提供する。
//! 投機的推論はバックエンド能力検出により有効化され、未対応バックエンドでは no-op フォールバックする。

pub mod types;
