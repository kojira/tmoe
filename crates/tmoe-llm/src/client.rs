use crate::error::Result;
use crate::types::{BackendCapabilities, ChatDelta, ChatRequest, ChatResponse};
use async_trait::async_trait;
use futures::stream::BoxStream;

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// 抽象 LLM の単発 chat。
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse>;

    /// SSE ストリーム。バックエンドが対応しなければ chat() 結果を 1 デルタで返す既定実装でも可。
    async fn chat_stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatDelta>>>;

    /// バックエンド能力。投機推論可否などを返す。
    fn capabilities(&self) -> &BackendCapabilities;

    /// LLM の論理名。ログ・デバッグ用。
    fn name(&self) -> &str;
}
