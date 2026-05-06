//! 決定的なテスト用 LLM。スクリプトされた応答テーブルを与えると
//! `chat()` 呼び出しごとに 1 つずつ返す。e2e テストや合意ループのスナップショット検証に使う。

use crate::client::LlmClient;
use crate::error::{LlmError, Result};
use crate::types::{BackendCapabilities, ChatDelta, ChatRequest, ChatResponse};
use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use std::sync::{Arc, Mutex};

/// スクリプト 1 件分。content と任意 finish_reason。
#[derive(Debug, Clone)]
pub struct ScriptedTurn {
    pub content: String,
    pub finish_reason: Option<String>,
}

impl ScriptedTurn {
    pub fn new(content: impl Into<String>) -> Self {
        Self { content: content.into(), finish_reason: Some("stop".into()) }
    }
}

#[derive(Debug, Default)]
struct MockState {
    script: Vec<ScriptedTurn>,
    cursor: usize,
    calls: Vec<ChatRequest>,
}

#[derive(Debug, Clone)]
pub struct MockLlmClient {
    name: String,
    capabilities: BackendCapabilities,
    state: Arc<Mutex<MockState>>,
}

impl MockLlmClient {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            capabilities: BackendCapabilities::default(),
            state: Arc::new(Mutex::new(MockState::default())),
        }
    }

    pub fn with_speculative(mut self, supports: bool) -> Self {
        self.capabilities.supports_speculative = supports;
        self
    }

    pub fn push(&self, turn: ScriptedTurn) {
        self.state.lock().unwrap().script.push(turn);
    }

    pub fn extend<I: IntoIterator<Item = ScriptedTurn>>(&self, turns: I) {
        self.state.lock().unwrap().script.extend(turns);
    }

    pub fn calls(&self) -> Vec<ChatRequest> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn remaining(&self) -> usize {
        let s = self.state.lock().unwrap();
        s.script.len().saturating_sub(s.cursor)
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(req);
        if s.cursor >= s.script.len() {
            return Err(LlmError::MockExhausted);
        }
        let turn = s.script[s.cursor].clone();
        s.cursor += 1;
        Ok(ChatResponse {
            content: turn.content,
            finish_reason: turn.finish_reason,
            usage: None,
        })
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatDelta>>> {
        let resp = self.chat(req).await?;
        let tokens: Vec<Result<ChatDelta>> = resp
            .content
            .chars()
            .map(|c| Ok(ChatDelta::Token(c.to_string())))
            .chain(std::iter::once(Ok(ChatDelta::Done { finish_reason: resp.finish_reason })))
            .collect();
        Ok(stream::iter(tokens).boxed())
    }

    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    #[tokio::test]
    async fn script_consumed_in_order() {
        let m = MockLlmClient::new("worker");
        m.extend([ScriptedTurn::new("a"), ScriptedTurn::new("b")]);

        let r1 = m.chat(ChatRequest { messages: vec![ChatMessage::user("?")], ..Default::default() })
            .await
            .unwrap();
        assert_eq!(r1.content, "a");
        let r2 = m.chat(ChatRequest { messages: vec![ChatMessage::user("?")], ..Default::default() })
            .await
            .unwrap();
        assert_eq!(r2.content, "b");
        let r3 = m.chat(ChatRequest::default()).await;
        assert!(matches!(r3, Err(LlmError::MockExhausted)));
    }

    #[tokio::test]
    async fn calls_recorded() {
        let m = MockLlmClient::new("worker");
        m.push(ScriptedTurn::new("x"));
        let req = ChatRequest { messages: vec![ChatMessage::user("hello")], ..Default::default() };
        m.chat(req.clone()).await.unwrap();
        let recorded = m.calls();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], req);
    }

    #[tokio::test]
    async fn stream_fanouts_per_char_then_done() {
        let m = MockLlmClient::new("worker");
        m.push(ScriptedTurn::new("hi"));
        let mut stream = m.chat_stream(ChatRequest::default()).await.unwrap();
        let mut tokens = Vec::new();
        while let Some(d) = stream.next().await {
            tokens.push(d.unwrap());
        }
        assert_eq!(tokens.len(), 3);
        assert!(matches!(tokens[0], ChatDelta::Token(ref s) if s == "h"));
        assert!(matches!(tokens[1], ChatDelta::Token(ref s) if s == "i"));
        assert!(matches!(tokens[2], ChatDelta::Done { .. }));
    }
}
