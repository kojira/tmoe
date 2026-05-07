//! `tmoe codex login` のローカル HTTP callback サーバを socket 経由で叩く統合テスト。
//!
//! 本物の OpenAI には届かないように **callback で `error=test` を返してオフ** する経路だけ
//! 検証する。これで `parse_callback_request_line` → state チェック → HTML 応答 → エラー
//! 伝播 までを 1 本の socket round-trip で踏み抜けることを確かめる。
//!
//! ポート 1455 (`CODEX_OAUTH_PORT`) を使うので、同マシンで他の opencode/tmoe が立ってると
//! 衝突する。CI で衝突する場合は ignore する。

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
#[ignore = "binds local TCP port 1455 — run manually"]
async fn login_callback_with_error_param_returns_400_and_propagates_error() {
    // run_login をバックグラウンドで起動。テスト中はブラウザを起動しない (= 実 OAuth に飛ばさない)。
    std::env::set_var("TMOE_CODEX_NO_BROWSER", "1");
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    let login_handle = tokio::spawn(async move {
        tmoe_cli::codex_login::run_login(Some(auth_path)).await
    });

    // listener が bind するまで少し待つ。
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut sock = TcpStream::connect(("127.0.0.1", 1455))
        .await
        .expect("connect to login server");
    let req = "GET /auth/callback?error=test_only HTTP/1.1\r\nHost: localhost:1455\r\n\r\n";
    sock.write_all(req.as_bytes()).await.unwrap();

    let mut buf = [0u8; 4096];
    let n = sock.read(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf[..n]);
    assert!(
        resp.starts_with("HTTP/1.1 400"),
        "expected 400 from error path, got: {resp}"
    );
    assert!(resp.contains("Authorization failed"), "{resp}");

    let outcome = login_handle.await.unwrap();
    let err = outcome.expect_err("login should fail when callback carries error param");
    let msg = format!("{err:#}");
    assert!(msg.contains("OAuth error from issuer"), "{msg}");
}
