//! Concierge: ユーザーの自然言語入力 + ホットキーを Z 軸推進シグナル `UserThrust` に
//! 翻訳する I/O チャネル。エージェントの一員ではない。
//!
//! 純粋関数で実装し、TUI 側は `translate` / `key_to_thrust` を呼ぶだけ。これにより
//! e2e テストはキーイベントをシミュレートして「Ctrl-P で park / Ctrl-G で再開」を
//! 決定的に検証できる。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tmoe_core::UserThrust;
use tmoe_llm::{ChatMessage, ChatRequest, LlmClient};

/// アイドル時 Enter のルーティング判定結果。`classify_route` の戻り値。
///
/// **task vs chat の判定は LLM が決める** (= 文字列マッチ禁止)。CLI ビルトインコマンド
/// (`help` / `quit` / `exit`) は呼び出し側で先に短絡されるので、`classify_route` には
/// 来ない前提。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// ファイル変更を伴う作業 → 通常の Trio セッション spawn。
    Task,
    /// 雑談・挨拶・メタ質問 → そのまま左ペインに `reply` を表示して終了。
    Chat { reply: String },
}

/// LLM の生応答テキストから `Route` を取り出す純粋関数。HTTP コールは含まない (= test 容易)。
///
/// 想定する正解フォーマット:
///   `{"route":"task"}`
///   `{"route":"chat","reply":"<text>"}`
///
/// パースに失敗した場合や `route` の値が不明だった場合は **task に倒す** (= 安全側)。
/// chat 誤分類は被害甚大 (= 実装依頼を放置) だが、task 誤分類は worktree carve + 空提案で
/// 第二防壁が回収できるため。
///
/// LLM が markdown フェンスや prose を混ぜてくることを許容するため、最初の `{` から最後の `}`
/// までを取り出してから serde_json に渡す (= history_tool::parse_pick / trio::parse_vote と
/// 同じ leniency)。
pub fn parse_route(text: &str) -> Route {
    let trimmed = text.trim();
    let start = match trimmed.find('{') {
        Some(i) => i,
        None => return Route::Task,
    };
    let end = match trimmed.rfind('}') {
        Some(i) if i > start => i,
        _ => return Route::Task,
    };
    let payload = &trimmed[start..=end];

    #[derive(serde::Deserialize)]
    struct Raw {
        route: Option<String>,
        #[serde(default)]
        reply: Option<String>,
    }
    let raw: Raw = match serde_json::from_str(payload) {
        Ok(r) => r,
        Err(_) => return Route::Task,
    };
    match raw.route.as_deref() {
        Some("chat") => Route::Chat {
            reply: raw.reply.unwrap_or_default().trim().to_string(),
        },
        Some("task") => Route::Task,
        // 不明な route 値は安全側で task。
        _ => Route::Task,
    }
}

/// アイドル時の Enter 入力を LLM に分類させる。判定基準は `tmoe_prompts::CONCIERGE_SYSTEM`
/// に書いてある (= 概念で書く、キーワードリスト禁止)。
///
/// 失敗時 (LLM HTTP エラー / parse 不能) は `Route::Task` に倒す (= 安全側)。呼び出し側は
/// この戻り値だけを見て分岐すれば良く、エラー処理は不要。
pub async fn classify_route(llm: &dyn LlmClient, input: &str) -> Route {
    let req = ChatRequest {
        messages: vec![
            ChatMessage::system(tmoe_prompts::CONCIERGE_SYSTEM),
            ChatMessage::user(input.to_string()),
        ],
        max_tokens: Some(256),
        temperature: Some(0.2),
        ..Default::default()
    };
    match llm.chat(req).await {
        Ok(resp) => parse_route(&resp.content),
        Err(e) => {
            tracing::warn!("classify_route LLM error: {e}; falling back to task");
            Route::Task
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConciergeIntent {
    Go,
    Pause,
    Stop,
    Redirect,
}

/// 入力テキストから ConciergeIntent を分類する。
///
/// 旧実装は `starts_with` を使っていたため "stop using mocks" を Stop と誤分類していた。
/// 新実装は **first-token equality** に切り替える: 制御語は単独 (または短い丁寧語の連結) の
/// ときだけ制御扱い。3 トークン以上ある自然文はすべて Redirect として Worker にメッセージとして
/// 投入する。
pub fn classify_intent(input: &str) -> ConciergeIntent {
    let s = input.trim().to_lowercase();
    if s.is_empty() {
        return ConciergeIntent::Pause;
    }

    // ASCII 空白と全角空白の両方で分割。日本語句点・カンマ末尾は剥がす。
    let stripped: String = s
        .trim_end_matches(|c: char| matches!(c, '.' | '、' | ',' | '。' | '!' | '！' | '?' | '？'))
        .to_string();
    let tokens: Vec<&str> = stripped
        .split(|c: char| c.is_whitespace() || c == '\u{3000}')
        .filter(|t| !t.is_empty())
        .collect();
    let first = match tokens.first() {
        Some(t) => *t,
        None => return ConciergeIntent::Pause,
    };

    // 制御語。第 1 トークンが完全一致すれば intent 確定。
    let stop_keys = ["stop", "中断", "やめて", "中止", "abort", "やめる", "中断して"];
    let pause_keys = ["pause", "wait", "停止", "ちょっと", "待って", "待機"];
    let go_keys = [
        "go", "ok", "okay", "yes", "y", "approve", "ack", "ack.",
        "進めて", "進め", "続けて", "了解", "ok.", "yes.",
    ];

    let is_short = tokens.len() <= 2; // "stop please" / "go now" 程度は許容、それ以上は Redirect。

    if is_short {
        if stop_keys.iter().any(|k| *k == first) {
            return ConciergeIntent::Stop;
        }
        if pause_keys.iter().any(|k| *k == first) {
            return ConciergeIntent::Pause;
        }
        if go_keys.iter().any(|k| *k == first) {
            return ConciergeIntent::Go;
        }
    }
    // それ以外 (=自然文) はすべて Redirect として Worker に渡す。
    ConciergeIntent::Redirect
}

pub fn translate(input: &str) -> UserThrust {
    match classify_intent(input) {
        ConciergeIntent::Go => UserThrust::Go { strength: 1.0 },
        ConciergeIntent::Pause => UserThrust::Pause,
        ConciergeIntent::Stop => UserThrust::Stop,
        ConciergeIntent::Redirect => UserThrust::Redirect {
            instruction: input.trim().to_string(),
        },
    }
}

/// 制御キーを `UserThrust` に直接翻訳する。テキスト入力経路 (`translate`) の代替で
/// あり、ユーザーは入力中でも Ctrl-P / Ctrl-G / Ctrl-K で Trio に介入できる。
///
/// マッピング:
///   Ctrl-P  -> Pause     (動作中の Trio を park)
///   Ctrl-G  -> Go        (park された Trio を進める)
///   Ctrl-K  -> Stop      (現在の feature を中断)
pub fn key_to_thrust(key: KeyEvent) -> Option<UserThrust> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Char('p') | KeyCode::Char('P') => Some(UserThrust::Pause),
        KeyCode::Char('g') | KeyCode::Char('G') => Some(UserThrust::Go { strength: 1.0 }),
        KeyCode::Char('k') | KeyCode::Char('K') => Some(UserThrust::Stop),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_pause() {
        assert_eq!(classify_intent(""), ConciergeIntent::Pause);
        assert_eq!(classify_intent("   "), ConciergeIntent::Pause);
    }

    #[test]
    fn go_keywords() {
        for k in ["go", "進めて", "ok", "yes", "ACK"] {
            assert_eq!(classify_intent(k), ConciergeIntent::Go, "{k}");
        }
    }

    #[test]
    fn stop_keywords() {
        for k in ["stop", "中断して", "やめて", "abort"] {
            assert_eq!(classify_intent(k), ConciergeIntent::Stop, "{k}");
        }
    }

    #[test]
    fn pause_keywords() {
        for k in ["pause", "待って", "ちょっと"] {
            assert_eq!(classify_intent(k), ConciergeIntent::Pause, "{k}");
        }
    }

    /// 自然文に制御語が **含まれる** ケース: redirect として Worker に渡るべき。
    /// 旧実装の `starts_with` で誤分類されていたケースを回帰防止する。
    #[test]
    fn natural_sentence_with_control_word_prefix_is_redirect() {
        for s in [
            "stop using mocks, switch to real LLM",
            "stop calling the wrong file",
            "wait until tests pass before merging",
            "pause the cargo build and try a smaller crate first",
            "go to src/util.rs and rename gcd to euclid_gcd",
            "abort the current PR draft and re-create it from scratch",
        ] {
            assert!(
                matches!(classify_intent(s), ConciergeIntent::Redirect),
                "should be Redirect: {s}"
            );
        }
    }

    /// 短い丁寧語接尾は許容: "stop please" / "go now" / "ok."
    #[test]
    fn short_polite_forms_are_still_control_intent() {
        assert_eq!(classify_intent("stop please"), ConciergeIntent::Stop);
        assert_eq!(classify_intent("go now"), ConciergeIntent::Go);
        assert_eq!(classify_intent("ok."), ConciergeIntent::Go);
    }

    #[test]
    fn anything_else_is_redirect() {
        let t = translate("hello.rs ではなく greet.rs にして");
        match t {
            UserThrust::Redirect { instruction } => {
                assert!(instruction.contains("greet.rs"));
            }
            _ => panic!("expected Redirect"),
        }
    }

    #[test]
    fn translate_to_thrust_variants() {
        assert!(matches!(translate("go"), UserThrust::Go { .. }));
        assert!(matches!(translate("pause"), UserThrust::Pause));
        assert!(matches!(translate("stop"), UserThrust::Stop));
        assert!(matches!(translate("change something"), UserThrust::Redirect { .. }));
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn plain(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn ctrl_p_pauses() {
        assert!(matches!(key_to_thrust(ctrl('p')), Some(UserThrust::Pause)));
        assert!(matches!(key_to_thrust(ctrl('P')), Some(UserThrust::Pause)));
    }

    #[test]
    fn ctrl_g_goes() {
        assert!(matches!(
            key_to_thrust(ctrl('g')),
            Some(UserThrust::Go { .. })
        ));
    }

    #[test]
    fn ctrl_k_stops() {
        assert!(matches!(key_to_thrust(ctrl('k')), Some(UserThrust::Stop)));
    }

    #[test]
    fn plain_letter_does_not_thrust() {
        assert!(key_to_thrust(plain('p')).is_none());
        assert!(key_to_thrust(plain('g')).is_none());
    }

    #[test]
    fn unrelated_ctrl_keys_ignored() {
        assert!(key_to_thrust(ctrl('a')).is_none());
        assert!(key_to_thrust(ctrl('z')).is_none());
    }

    // ---- LLM-driven route classifier (idle-time Enter only) ----

    #[test]
    fn parse_route_task_minimal() {
        assert_eq!(parse_route(r#"{"route":"task"}"#), Route::Task);
    }

    #[test]
    fn parse_route_chat_with_reply() {
        match parse_route(r#"{"route":"chat","reply":"hello"}"#) {
            Route::Chat { reply } => assert_eq!(reply, "hello"),
            other => panic!("expected chat, got {other:?}"),
        }
    }

    #[test]
    fn parse_route_chat_japanese_reply_preserved() {
        match parse_route(r#"{"route":"chat","reply":"こんにちは！"}"#) {
            Route::Chat { reply } => assert_eq!(reply, "こんにちは！"),
            other => panic!("expected chat, got {other:?}"),
        }
    }

    /// LLM が markdown フェンスを巻いてきても剥がして parse できる (= history_tool::parse_pick
    /// と同じ leniency)。
    #[test]
    fn parse_route_strips_markdown_fences() {
        let body = "```json\n{\"route\":\"task\"}\n```";
        assert_eq!(parse_route(body), Route::Task);
    }

    /// JSON parse 不能 → task に倒す (安全側)。
    #[test]
    fn parse_route_unparseable_falls_back_to_task() {
        assert_eq!(parse_route("not json"), Route::Task);
        assert_eq!(parse_route(r#"{"route":"#), Route::Task);
        assert_eq!(parse_route(""), Route::Task);
    }

    /// 不明な route 値も task に倒す。
    #[test]
    fn parse_route_unknown_route_value_falls_back_to_task() {
        assert_eq!(parse_route(r#"{"route":"weird"}"#), Route::Task);
        assert_eq!(parse_route(r#"{"foo":"bar"}"#), Route::Task);
    }

    #[tokio::test]
    async fn classify_route_with_mock_chat() {
        use tmoe_llm::{MockLlmClient, ScriptedTurn};
        let llm = MockLlmClient::new("classifier");
        llm.push(ScriptedTurn::new(r#"{"route":"chat","reply":"hi"}"#));
        match classify_route(&llm, "hello").await {
            Route::Chat { reply } => assert_eq!(reply, "hi"),
            other => panic!("expected chat, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn classify_route_with_mock_task() {
        use tmoe_llm::{MockLlmClient, ScriptedTurn};
        let llm = MockLlmClient::new("classifier");
        llm.push(ScriptedTurn::new(r#"{"route":"task"}"#));
        assert_eq!(
            classify_route(&llm, "add hello() to src/main.rs").await,
            Route::Task
        );
    }

    /// LLM 呼び出しが失敗 (MockExhausted) → fallback で Task に倒れる。
    #[tokio::test]
    async fn classify_route_falls_back_to_task_on_llm_error() {
        use tmoe_llm::MockLlmClient;
        let llm = MockLlmClient::new("classifier"); // script 空 = エラー
        assert_eq!(classify_route(&llm, "anything").await, Route::Task);
    }
}
