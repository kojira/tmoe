//! LLM 抽象レイヤで使う共通型。Phase 1 で具体化する。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    LlamaCpp,
    Vllm,
    LmStudio,
    RapidMlx,
    OpenAiCompat,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub supports_speculative: bool,
}
