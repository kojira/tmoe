//! Concierge: ユーザーの自然言語入力 + ホットキーを Z 軸推進シグナル `UserThrust` に
//! 翻訳する I/O チャネル。エージェントの一員ではない。
//!
//! 純粋関数で実装し、TUI 側は `translate` / `key_to_thrust` を呼ぶだけ。これにより
//! e2e テストはキーイベントをシミュレートして「Ctrl-P で park / Ctrl-G で再開」を
//! 決定的に検証できる。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
}
