//! Concierge: ユーザーの自然言語入力を Z 軸推進シグナル `UserThrust` に翻訳する
//! I/O チャネル。エージェントの一員ではない。
//!
//! Phase 6 では決定的なルールベース実装。Phase 後半で LLM へ置換可能。

use tmoe_core::UserThrust;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConciergeIntent {
    Go,
    Pause,
    Stop,
    Redirect,
}

pub fn classify_intent(input: &str) -> ConciergeIntent {
    let s = input.trim().to_lowercase();
    if s.is_empty() {
        return ConciergeIntent::Pause;
    }
    let stop_keys = ["stop", "中断", "やめて", "中止", "abort"];
    if stop_keys.iter().any(|k| s == *k || s.starts_with(k)) {
        return ConciergeIntent::Stop;
    }
    let pause_keys = ["pause", "待って", "wait", "停止", "ちょっと"];
    if pause_keys.iter().any(|k| s == *k || s.starts_with(k)) {
        return ConciergeIntent::Pause;
    }
    let go_keys = ["go", "進めて", "ok", "yes", "y", "進め", "続けて", "approve", "ack"];
    if go_keys.iter().any(|k| s == *k) {
        return ConciergeIntent::Go;
    }
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
        for k in ["stop", "中断して", "やめて", "abort now"] {
            assert_eq!(classify_intent(k), ConciergeIntent::Stop, "{k}");
        }
    }

    #[test]
    fn pause_keywords() {
        for k in ["pause", "待って", "ちょっと"] {
            assert_eq!(classify_intent(k), ConciergeIntent::Pause, "{k}");
        }
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
}
