//! OpenAI 互換バックエンドクライアント。
//!
//! llama.cpp llama-server / vLLM / LM Studio / Rapid-MLX / その他 OpenAI 互換 HTTP サーバーを
//! 単一の実装で扱う。`draft_model` が設定されており、バックエンドが投機推論を受け付ける場合のみ
//! リクエストに `speculative_decoding` 拡張パラメータを混ぜる。未対応バックエンドでは静かに
//! `main_model` 単独で動作する (no-op フォールバック)。

use crate::client::LlmClient;
use crate::error::{LlmError, Result};
use crate::types::{Backend, BackendCapabilities, ChatDelta, ChatRequest, ChatResponse, Usage};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::task::{Context, Poll};
use url::Url;

#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    pub backend: Backend,
    pub base_url: Url,
    pub main_model: String,
    pub draft_model: Option<String>,
    pub spec_n_max: Option<u32>,
    pub api_key: Option<String>,
    /// 1 リクエストあたりの最大待機時間 (秒)。None なら 120 秒。
    /// LLM のサーバ側ハングや極端に重い prompt に対する safety net。
    pub request_timeout_secs: Option<u64>,
    /// 一過性エラー (timeout / 接続切断 / 5xx) のときの再試行回数。0 なら無効。
    /// 既定は 3 (= 250ms → 500ms → 1s の指数バックオフ)。
    pub retry_max_attempts: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatClient {
    config: OpenAiCompatConfig,
    capabilities: BackendCapabilities,
    http: reqwest::Client,
}

/// `OpenAiCompatClient::health_check` の結果。
#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub url: Url,
    pub ok: bool,
    pub status_code: u16,
    /// `GET /v1/models` のレスポンスに、設定中の main_model 名が含まれていたか。
    /// 一部バックエンドは ID を別表記で返すので false でも正常運転は可能。
    pub main_model_visible: bool,
    pub main_model: String,
}

#[derive(Debug, Clone)]
pub struct ClientDescription {
    pub backend: String,
    pub base_url: String,
    pub main_model: String,
    pub draft_model: Option<String>,
    pub speculative_enabled: bool,
}

impl OpenAiCompatClient {
    /// 設定からクライアントを構築。投機推論対応はバックエンド種別から推定する
    /// (本物の能力検出は probe() で行える)。
    pub fn new(config: OpenAiCompatConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(LlmError::from)?;
        let capabilities = BackendCapabilities {
            supports_speculative: backend_supports_speculative(config.backend)
                && config.draft_model.is_some(),
        };
        Ok(Self { config, capabilities, http })
    }

    pub fn with_capabilities(mut self, caps: BackendCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// OpenAI 互換の `<base_url>/models` を GET し、200 が返るかどうかを確認する。
    /// `tmoe doctor` と runtime の preflight で使う。Trio を起動する前に LLM の生死を
    /// 短い HTTP one-shot で確認することで、初見ユーザーが reqwest スタックトレースに
    /// 直面するのを避ける。
    pub async fn health_check(&self) -> Result<HealthStatus> {
        let mut url = self.config.base_url.clone();
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        let url = url.join("models").map_err(LlmError::from)?;
        let mut builder = self.http.get(url.clone()).timeout(std::time::Duration::from_secs(3));
        if let Some(k) = &self.config.api_key {
            builder = builder.bearer_auth(k);
        }
        let resp = builder.send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let mut model_visible = false;
        if status.is_success() {
            // 設定されている main_model が GET /v1/models のレスポンスに含まれていれば
            // 「ロード済み」の確度が上がる。含まれていなくても 200 なら HEALTH 自体は OK。
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                    model_visible = arr.iter().any(|m| {
                        m.get("id")
                            .and_then(|i| i.as_str())
                            .map(|id| id == self.config.main_model)
                            .unwrap_or(false)
                    });
                }
            }
        }
        Ok(HealthStatus {
            url,
            ok: status.is_success(),
            status_code: status.as_u16(),
            main_model_visible: model_visible,
            main_model: self.config.main_model.clone(),
        })
    }

    /// 設定中の `(backend, base_url, main_model, draft_model)` を読み出す。doctor 用。
    pub fn describe(&self) -> ClientDescription {
        ClientDescription {
            backend: format!("{:?}", self.config.backend),
            base_url: self.config.base_url.to_string(),
            main_model: self.config.main_model.clone(),
            draft_model: self.config.draft_model.clone(),
            speculative_enabled: self.capabilities.supports_speculative,
        }
    }

    fn chat_url(&self) -> Result<Url> {
        // base_url は通常 .../v1 で終わる。chat/completions を後置する。
        let mut joined = self.config.base_url.clone();
        if !joined.path().ends_with('/') {
            joined.set_path(&format!("{}/", joined.path()));
        }
        joined.join("chat/completions").map_err(LlmError::from)
    }

    fn build_request_body(&self, req: &ChatRequest, stream: bool) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.config.main_model,
            "messages": req.messages,
            "stream": stream,
        });
        if let Some(t) = req.temperature { body["temperature"] = t.into(); }
        if let Some(m) = req.max_tokens { body["max_tokens"] = m.into(); }
        if let Some(s) = &req.stop { body["stop"] = serde_json::to_value(s).unwrap(); }
        if let Some(s) = req.seed { body["seed"] = s.into(); }

        // 投機推論対応バックエンドにのみ draft_model を送る。
        if self.capabilities.supports_speculative {
            if let Some(draft) = &self.config.draft_model {
                body["draft_model"] = draft.clone().into();
                if let Some(n) = self.config.spec_n_max {
                    body["spec_n_max"] = n.into();
                }
            }
        }
        body
    }
}

fn backend_supports_speculative(backend: Backend) -> bool {
    match backend {
        Backend::LlamaCpp | Backend::Vllm | Backend::LmStudio => true,
        // Rapid-MLX は 2026-05 時点で投機推論未実装。
        Backend::RapidMlx => false,
        // 一般 OpenAI 互換はバックエンド次第のため既定 false。実環境で probe() してから上書き。
        Backend::OpenAiCompat => false,
    }
}

impl OpenAiCompatClient {
    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.config.request_timeout_secs.unwrap_or(120))
    }
    fn retry_max(&self) -> u32 {
        self.config.retry_max_attempts.unwrap_or(3)
    }

    /// 与えられた non-stream POST を **timeout + 指数バックオフ retry** で 1 度に包む。
    /// 一過性エラーのみ retry: タイムアウト / 接続失敗 / 5xx。4xx と JSON parse エラーは即時失敗。
    async fn chat_with_retry(&self, req: ChatRequest) -> Result<ChatResponse> {
        let url = self.chat_url()?;
        let body = self.build_request_body(&req, false);
        let mut attempt: u32 = 0;
        let max = self.retry_max();
        loop {
            attempt += 1;
            let mut builder = self.http.post(url.clone()).json(&body).timeout(self.timeout());
            if let Some(k) = &self.config.api_key {
                builder = builder.bearer_auth(k);
            }
            let send = builder.send().await;
            let resp = match send {
                Ok(r) => r,
                Err(e) => {
                    if is_transient_send_error(&e) && attempt <= max {
                        tracing::warn!("LLM transient send error (attempt {attempt}/{max}): {e}");
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(LlmError::from(e));
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let body_text = resp.text().await.unwrap_or_default();
                if is_transient_status(status.as_u16()) && attempt <= max {
                    tracing::warn!(
                        "LLM transient HTTP {} (attempt {attempt}/{max}): {}",
                        status.as_u16(),
                        body_text.chars().take(200).collect::<String>()
                    );
                    backoff(attempt).await;
                    continue;
                }
                return Err(LlmError::BadStatus { status: status.as_u16(), body: body_text });
            }
            let parsed: OpenAiChatResponse = resp.json().await.map_err(LlmError::from)?;
            return Ok(parsed.into_chat_response());
        }
    }
}

fn is_transient_send_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}
fn is_transient_status(code: u16) -> bool {
    // 408 Request Timeout / 429 Too Many Requests / 5xx は再試行価値あり。
    code == 408 || code == 429 || (500..=599).contains(&code)
}
async fn backoff(attempt: u32) {
    // 1: 250ms, 2: 500ms, 3: 1s, 4: 2s ... cap 8s
    let ms = 250u64.saturating_mul(1u64 << (attempt.saturating_sub(1)));
    let ms = ms.min(8_000);
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.chat_with_retry(req).await
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatDelta>>> {
        // Stream は本体到達後の中断は安全に巻き戻せないので、retry は **接続確立まで** に限定する。
        let url = self.chat_url()?;
        let body = self.build_request_body(&req, true);
        let mut attempt: u32 = 0;
        let max = self.retry_max();
        loop {
            attempt += 1;
            let mut builder = self.http.post(url.clone()).json(&body).timeout(self.timeout());
            if let Some(k) = &self.config.api_key {
                builder = builder.bearer_auth(k);
            }
            let resp = match builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    if is_transient_send_error(&e) && attempt <= max {
                        tracing::warn!("LLM stream transient send error (attempt {attempt}/{max}): {e}");
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(LlmError::from(e));
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let body_text = resp.text().await.unwrap_or_default();
                if is_transient_status(status.as_u16()) && attempt <= max {
                    tracing::warn!(
                        "LLM stream transient HTTP {} (attempt {attempt}/{max})",
                        status.as_u16()
                    );
                    backoff(attempt).await;
                    continue;
                }
                return Err(LlmError::BadStatus { status: status.as_u16(), body: body_text });
            }
            let bytes_stream = resp.bytes_stream().map(|r| r.map_err(LlmError::from));
            return Ok(SseDeltaStream::new(bytes_stream).boxed());
        }
    }

    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn name(&self) -> &str {
        &self.config.main_model
    }
}

// --- SSE 解析 -----------------------------------------------------------

#[derive(Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}
#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}
#[derive(Deserialize)]
struct OpenAiChoiceMessage {
    #[serde(default, rename = "role")]
    _role: Option<String>,
    #[serde(default)]
    content: Option<String>,
}
#[derive(Deserialize, Serialize, Clone, Copy, Default)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

impl OpenAiChatResponse {
    fn into_chat_response(self) -> ChatResponse {
        let (content, finish_reason) = self
            .choices
            .into_iter()
            .next()
            .map(|c| (c.message.content.unwrap_or_default(), c.finish_reason))
            .unwrap_or_default();
        ChatResponse {
            content,
            finish_reason,
            usage: self.usage.map(|u| Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            }),
        }
    }
}

#[derive(Deserialize)]
struct OpenAiStreamChunk {
    choices: Vec<OpenAiStreamChoice>,
}
#[derive(Deserialize)]
struct OpenAiStreamChoice {
    #[serde(default)]
    delta: OpenAiStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}
#[derive(Deserialize, Default)]
struct OpenAiStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

/// reqwest の bytes_stream() を SSE event 単位にパースして ChatDelta 列に変換する。
struct SseDeltaStream<S> {
    inner: S,
    buf: Vec<u8>,
    done: bool,
}

impl<S> SseDeltaStream<S> {
    fn new(inner: S) -> Self {
        Self { inner, buf: Vec::with_capacity(4096), done: false }
    }
}

impl<S> Stream for SseDeltaStream<S>
where
    S: Stream<Item = Result<Bytes>> + Unpin + Send + 'static,
{
    type Item = Result<ChatDelta>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        loop {
            // 既にバッファに完結したイベントがあれば取り出す。
            if let Some((event_end, body)) = take_next_event(&self.buf) {
                self.buf.drain(..event_end);
                if let Some(delta) = decode_sse_event(&body) {
                    if matches!(delta, ChatDelta::Done { .. }) {
                        self.done = true;
                    }
                    return Poll::Ready(Some(Ok(delta)));
                }
                // skip と続行
                continue;
            }
            // バッファ不足。次のチャンクを取りに行く。
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(b))) => {
                    self.buf.extend_from_slice(&b);
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    // ストリーム終端。残りバッファを 1 つの不完全イベントとして処理して終わる。
                    if !self.buf.is_empty() {
                        let body = std::mem::take(&mut self.buf);
                        if let Some(delta) = decode_sse_event(&body) {
                            self.done = true;
                            return Poll::Ready(Some(Ok(delta)));
                        }
                    }
                    self.done = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// SSE はイベント間を空行で区切る。`buf` から最初の完結イベントの終端位置と本体を返す。
fn take_next_event(buf: &[u8]) -> Option<(usize, Vec<u8>)> {
    // \n\n または \r\n\r\n を境界とする。
    let n = buf.len();
    let mut i = 0;
    while i < n {
        if i + 1 < n && buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i + 2, buf[..i].to_vec()));
        }
        if i + 3 < n && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i + 4, buf[..i].to_vec()));
        }
        i += 1;
    }
    None
}

/// 1 イベントの本体 (複数行) から OpenAI 互換の data 行を抽出してデコードする。
fn decode_sse_event(body: &[u8]) -> Option<ChatDelta> {
    let text = match std::str::from_utf8(body) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(payload) = line.strip_prefix("data:") {
            data_lines.push(payload.trim_start());
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    let payload = data_lines.join("\n");
    if payload == "[DONE]" {
        return Some(ChatDelta::Done { finish_reason: None });
    }
    let chunk: OpenAiStreamChunk = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(_) => return None,
    };
    if let Some(c) = chunk.choices.into_iter().next() {
        if let Some(reason) = c.finish_reason.clone() {
            // 最終チャンクは Done 扱いにする。
            return Some(ChatDelta::Done { finish_reason: Some(reason) });
        }
        if let Some(content) = c.delta.content {
            return Some(ChatDelta::Token(content));
        }
        // role のみのデルタは無視。
        let _ = c.delta.role;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    fn cfg(backend: Backend, draft: Option<&str>) -> OpenAiCompatConfig {
        OpenAiCompatConfig {
            backend,
            base_url: Url::parse("http://127.0.0.1:8080/v1").unwrap(),
            main_model: "main".to_string(),
            draft_model: draft.map(|s| s.to_string()),
            spec_n_max: Some(16),
            api_key: None,
            request_timeout_secs: None,
            retry_max_attempts: None,
        }
    }

    #[test]
    fn capabilities_off_for_rapid_mlx() {
        let c = OpenAiCompatClient::new(cfg(Backend::RapidMlx, Some("draft"))).unwrap();
        assert!(!c.capabilities().supports_speculative);
    }

    #[test]
    fn capabilities_on_for_llama_cpp_when_draft_set() {
        let c = OpenAiCompatClient::new(cfg(Backend::LlamaCpp, Some("draft"))).unwrap();
        assert!(c.capabilities().supports_speculative);
    }

    #[test]
    fn capabilities_off_when_draft_missing() {
        let c = OpenAiCompatClient::new(cfg(Backend::LlamaCpp, None)).unwrap();
        assert!(!c.capabilities().supports_speculative);
    }

    #[test]
    fn body_includes_draft_only_when_capable() {
        let supportive = OpenAiCompatClient::new(cfg(Backend::LlamaCpp, Some("draft-1"))).unwrap();
        let incapable = OpenAiCompatClient::new(cfg(Backend::RapidMlx, Some("draft-1"))).unwrap();
        let req = ChatRequest {
            messages: vec![ChatMessage::user("hi")],
            ..Default::default()
        };
        let supportive_body = supportive.build_request_body(&req, false);
        let incapable_body = incapable.build_request_body(&req, false);
        assert_eq!(supportive_body["draft_model"], "draft-1");
        assert!(incapable_body.get("draft_model").is_none());
    }

    #[test]
    fn chat_url_appends_completions() {
        let c = OpenAiCompatClient::new(cfg(Backend::LlamaCpp, None)).unwrap();
        let url = c.chat_url().unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:8080/v1/chat/completions");
    }

    #[test]
    fn sse_decode_token_then_done() {
        let token_event = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let done_event = b"data: [DONE]\n\n";
        let mut full = Vec::new();
        full.extend_from_slice(token_event);
        full.extend_from_slice(done_event);

        let (n1, ev1) = take_next_event(&full).unwrap();
        let d1 = decode_sse_event(&ev1).unwrap();
        assert!(matches!(d1, ChatDelta::Token(ref s) if s == "hi"));
        let rest = &full[n1..];
        let (_, ev2) = take_next_event(rest).unwrap();
        let d2 = decode_sse_event(&ev2).unwrap();
        assert!(matches!(d2, ChatDelta::Done { .. }));
    }

    #[test]
    fn sse_decode_finish_reason_chunk() {
        let event = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let (_, ev) = take_next_event(event).unwrap();
        let d = decode_sse_event(&ev).unwrap();
        match d {
            ChatDelta::Done { finish_reason } => assert_eq!(finish_reason.as_deref(), Some("stop")),
            _ => panic!("expected Done"),
        }
    }
}
