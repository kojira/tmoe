//! tmoe 実行時設定。
//!
//! 優先順位:
//!   1. `--config <path>` 引数で明示されたファイル
//!   2. `~/.tmoe/config.toml`
//!   3. リポジトリ同梱の `config/tmoe.toml.example` (見つかれば)
//!   4. 環境変数 (TMOE_LLM_URL / TMOE_LLM_MODEL / TMOE_LLM_BACKEND ...)
//!
//! どれも無ければ Rapid-MLX のローカル既定値 (127.0.0.1:8081, qwen3-coder-30b) を使う。
//! これは「ローカル LLM を前提にした自作コーディングエージェント」という設計の最後の砦。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tmoe_llm::{Backend, OpenAiCompatConfig};
use url::Url;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawConfig {
    #[serde(default)]
    pub llm: RawLlm,
    #[serde(default)]
    pub trio: RawTrio,
    #[serde(default)]
    pub history: RawHistory,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawLlm {
    pub backend: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub main_model: Option<String>,
    pub draft_model: Option<String>,
    pub spec_n_max: Option<u32>,
    pub request_timeout_secs: Option<u64>,
    pub retry_max_attempts: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawTrio {
    pub confidence_sum_min: Option<f32>,
    pub triangle_balance_min: Option<f32>,
    pub max_iter_per_step: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawHistory {
    pub root: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub llm: OpenAiCompatConfig,
    pub trio: TrioCfg,
    pub history_root: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub struct TrioCfg {
    pub confidence_sum_min: f32,
    pub triangle_balance_min: f32,
    pub max_iter_per_step: u32,
}

impl Default for TrioCfg {
    fn default() -> Self {
        Self {
            // 実機 LLM は中立 0.7 を返しがちなので CLI 既定は控えめに。
            confidence_sum_min: 1.5,
            triangle_balance_min: 0.3,
            max_iter_per_step: 4,
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let raw = if let Some(p) = explicit {
            read_toml(p)?
        } else if let Some(home) = dirs::home_dir() {
            let p = home.join(".tmoe").join("config.toml");
            if p.exists() {
                read_toml(&p)?
            } else {
                RawConfig::default()
            }
        } else {
            RawConfig::default()
        };
        Self::from_raw(raw)
    }

    pub fn from_raw(raw: RawConfig) -> Result<Self> {
        let backend_str = raw
            .llm
            .backend
            .or_else(|| std::env::var("TMOE_LLM_BACKEND").ok())
            .unwrap_or_else(|| "rapid_mlx".into());
        let backend = match backend_str.as_str() {
            "vllm" => Backend::Vllm,
            "lm_studio" => Backend::LmStudio,
            "rapid_mlx" => Backend::RapidMlx,
            "openai_compat" => Backend::OpenAiCompat,
            "llama_cpp" => Backend::LlamaCpp,
            other => anyhow::bail!("unknown llm.backend: {other}"),
        };
        let base_str = raw
            .llm
            .base_url
            .or_else(|| std::env::var("TMOE_LLM_URL").ok())
            .unwrap_or_else(|| "http://127.0.0.1:8081/v1".into());
        let base_url = Url::parse(&base_str).context("llm.base_url must be a valid URL")?;
        let main_model = raw
            .llm
            .main_model
            .or_else(|| std::env::var("TMOE_LLM_MODEL").ok())
            .unwrap_or_else(|| "qwen3-coder-30b".into());
        let api_key = raw.llm.api_key.filter(|s| !s.is_empty()).or_else(|| {
            std::env::var("TMOE_LLM_API_KEY").ok().filter(|s| !s.is_empty())
        });
        let llm = OpenAiCompatConfig {
            backend,
            base_url,
            main_model,
            draft_model: raw
                .llm
                .draft_model
                .filter(|s| !s.is_empty())
                .or_else(|| std::env::var("TMOE_LLM_DRAFT").ok().filter(|s| !s.is_empty())),
            spec_n_max: raw.llm.spec_n_max.or(Some(16)),
            api_key,
            request_timeout_secs: raw.llm.request_timeout_secs,
            retry_max_attempts: raw.llm.retry_max_attempts,
        };
        let trio = TrioCfg {
            confidence_sum_min: raw.trio.confidence_sum_min.unwrap_or(1.5),
            triangle_balance_min: raw.trio.triangle_balance_min.unwrap_or(0.3),
            max_iter_per_step: raw.trio.max_iter_per_step.unwrap_or(4),
        };
        let history_root = raw
            .history
            .root
            .map(expand_tilde)
            .or_else(|| dirs::home_dir().map(|h| h.join(".tmoe")))
            .unwrap_or_else(|| PathBuf::from(".tmoe"));
        Ok(Self { llm, trio, history_root })
    }
}

fn read_toml(p: &Path) -> Result<RawConfig> {
    let text = std::fs::read_to_string(p)
        .with_context(|| format!("read config: {}", p.display()))?;
    let raw: RawConfig = toml::from_str(&text)
        .with_context(|| format!("parse config TOML: {}", p.display()))?;
    Ok(raw)
}

fn expand_tilde(s: String) -> PathBuf {
    if let Some(stripped) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_rapid_mlx_local() {
        let cfg = Config::from_raw(RawConfig::default()).unwrap();
        assert_eq!(cfg.llm.main_model, "qwen3-coder-30b");
        assert_eq!(cfg.llm.base_url.as_str(), "http://127.0.0.1:8081/v1");
        assert!(matches!(cfg.llm.backend, Backend::RapidMlx));
    }

    #[test]
    fn parses_full_toml() {
        let raw: RawConfig = toml::from_str(
            r#"
[llm]
backend = "llama_cpp"
base_url = "http://127.0.0.1:8080/v1"
main_model = "qwen2.5-coder-32b"
draft_model = "qwen2.5-coder-0.5b"
spec_n_max = 8

[trio]
confidence_sum_min = 2.0
triangle_balance_min = 0.5
max_iter_per_step = 6

[history]
root = "/tmp/tmoe-test"
"#,
        )
        .unwrap();
        let cfg = Config::from_raw(raw).unwrap();
        assert!(matches!(cfg.llm.backend, Backend::LlamaCpp));
        assert_eq!(cfg.llm.draft_model.as_deref(), Some("qwen2.5-coder-0.5b"));
        assert_eq!(cfg.trio.max_iter_per_step, 6);
        assert_eq!(cfg.history_root, PathBuf::from("/tmp/tmoe-test"));
    }

    #[test]
    fn rejects_unknown_backend() {
        let mut raw = RawConfig::default();
        raw.llm.backend = Some("totally-bogus".into());
        let err = Config::from_raw(raw).unwrap_err();
        assert!(err.to_string().contains("unknown llm.backend"));
    }
}
