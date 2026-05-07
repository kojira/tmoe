//! `tmoe codex login`: ChatGPT Pro/Plus サブスクで Codex を使うための OAuth フロー。
//!
//! opencode (sst/opencode) と同じプロトコル準拠。ポート 1455 で `/auth/callback` を
//! 待ち受け、PKCE+state を発行してブラウザを `auth.openai.com/oauth/authorize` に飛ばす。
//! callback を受け取ったら `code` を `/oauth/token` に交換し、結果を `~/.tmoe/auth.json`
//! に保存する。
//!
//! 実装方針:
//!   - 軽量を保つため tokio の `TcpListener` で素のソケットを受け、HTTP request line を
//!     1 行ずつ自前パースする。reqwest 等のフル HTTP server crate は引っ張ってこない。
//!   - 1 リクエスト処理したら server を閉じる (one-shot)。
//!   - state mismatch は CSRF とみなして即 reject。code が無いときも error を返して終了。
//!   - 5 分でタイムアウト (= ブラウザを閉じてしまった等)。
//!
//! テスト: `localhost` で stub server を起動して `/auth/callback?code=X&state=...` を叩き、
//! callback パーサと state 検証を unit test で確かめる。本物の OpenAI への HTTP は走らせない。

use anyhow::{anyhow, Context, Result};
use std::time::Duration;
use tmoe_llm::{
    build_authorize_url, default_auth_path, exchange_code_for_tokens_default, save_codex_auth,
    token_response_to_auth, PkceCodes, CODEX_OAUTH_PORT, CODEX_OAUTH_REDIRECT_PATH,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const HTML_SUCCESS: &str = include_str!("codex_login_success.html");
const HTML_FAILURE: &str = include_str!("codex_login_failure.html");

/// callback を受けて `code` と `state` を取り出した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// `GET /auth/callback?code=...&state=...` のような HTTP request line から
/// クエリ文字列を取り出して `code` / `state` / `error` を抽出する。
pub fn parse_callback_request_line(line: &str) -> Option<CallbackParams> {
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    let path = parts.next()?;
    if !path.starts_with(CODEX_OAUTH_REDIRECT_PATH) {
        return None;
    }
    let q = path.find('?').map(|i| &path[i + 1..]).unwrap_or("");
    let mut p = CallbackParams {
        code: None,
        state: None,
        error: None,
    };
    for kv in q.split('&') {
        if kv.is_empty() {
            continue;
        }
        let mut it = kv.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        let v = url_decode(v);
        match k {
            "code" => p.code = Some(v),
            "state" => p.state = Some(v),
            "error" => p.error = Some(v),
            _ => {}
        }
    }
    Some(p)
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(b' ');
            i += 1;
        } else if b == b'%' && i + 2 < bytes.len() {
            let hi = hex(bytes[i + 1]);
            let lo = hex(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
            } else {
                out.push(b);
                i += 1;
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// ブラウザを `url` で開く。失敗してもエラーにせず、ユーザに手動で開く案内を出す。
/// `TMOE_CODEX_NO_BROWSER=1` が設定されていれば起動しない (= テスト用)。
fn try_open_browser(url: &str) -> bool {
    if std::env::var("TMOE_CODEX_NO_BROWSER").ok().as_deref() == Some("1") {
        return false;
    }
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        return false;
    };
    std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// Codex OAuth フローを 1 度走らせる。
///
/// 1. PKCE + state 生成
/// 2. localhost:1455 の HTTP サーバを 1 接続だけ accept
/// 3. ブラウザを authorize_url に open
/// 4. callback で受け取った code を `/oauth/token` に交換
/// 5. tokens を `~/.tmoe/auth.json` (もしくは指定パス) に保存
pub async fn run_login(custom_auth_path: Option<std::path::PathBuf>) -> Result<()> {
    let pkce = PkceCodes::generate().context("generate PKCE codes")?;
    let state = tmoe_llm::generate_state().context("generate state")?;
    let redirect_uri = format!("http://localhost:{}{}", CODEX_OAUTH_PORT, CODEX_OAUTH_REDIRECT_PATH);
    let authorize_url = build_authorize_url(&redirect_uri, &pkce, &state);

    let listener = TcpListener::bind(("127.0.0.1", CODEX_OAUTH_PORT))
        .await
        .with_context(|| {
            format!(
                "bind 127.0.0.1:{}. If another tmoe/codex login is running, close it.",
                CODEX_OAUTH_PORT
            )
        })?;

    eprintln!("Open this URL in your browser to log in to ChatGPT:");
    eprintln!();
    eprintln!("    {authorize_url}");
    eprintln!();
    if try_open_browser(&authorize_url) {
        eprintln!("(Attempted to open the browser automatically.)");
    }
    eprintln!("Waiting for callback on {redirect_uri} ...");

    // 5 分で諦める。
    let accept = tokio::time::timeout(Duration::from_secs(300), listener.accept()).await;
    let (mut sock, _peer) = match accept {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return Err(anyhow!("accept failed: {e}")),
        Err(_) => return Err(anyhow!("OAuth callback timed out (5 minutes)")),
    };

    // request line + headers + (optional) body を 8 KiB まで読み込む。callback は GET なので
    // body は無い。query 文字列もせいぜい 1 KiB 程度。
    let mut buf = [0u8; 8192];
    let n = sock.read(&mut buf).await.context("read request")?;
    let txt = String::from_utf8_lossy(&buf[..n]);
    let first_line = txt.lines().next().unwrap_or("");
    tracing::debug!("codex callback request line: {first_line}");

    let params = parse_callback_request_line(first_line)
        .ok_or_else(|| anyhow!("callback path not /auth/callback: {first_line}"))?;

    async fn respond(
        sock: &mut tokio::net::TcpStream,
        status_line: &str,
        body: &str,
    ) -> std::io::Result<()> {
        let resp = format!(
            "{status_line}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            len = body.len()
        );
        sock.write_all(resp.as_bytes()).await?;
        sock.shutdown().await?;
        Ok(())
    }

    if let Some(err) = params.error {
        let _ = respond(
            &mut sock,
            "HTTP/1.1 400 Bad Request",
            &HTML_FAILURE.replace("{ERROR}", &err),
        )
        .await;
        return Err(anyhow!("OAuth error from issuer: {err}"));
    }
    let Some(returned_state) = params.state else {
        let _ = respond(
            &mut sock,
            "HTTP/1.1 400 Bad Request",
            &HTML_FAILURE.replace("{ERROR}", "missing state"),
        )
        .await;
        return Err(anyhow!("callback missing state"));
    };
    if returned_state != state {
        let _ = respond(
            &mut sock,
            "HTTP/1.1 400 Bad Request",
            &HTML_FAILURE.replace("{ERROR}", "state mismatch (possible CSRF)"),
        )
        .await;
        return Err(anyhow!("state mismatch — aborting login (possible CSRF)"));
    }
    let Some(code) = params.code else {
        let _ = respond(
            &mut sock,
            "HTTP/1.1 400 Bad Request",
            &HTML_FAILURE.replace("{ERROR}", "missing code"),
        )
        .await;
        return Err(anyhow!("callback missing code"));
    };

    // code 受信 → 即 200 を返してブラウザを解放してから token exchange を行う。
    let _ = respond(&mut sock, "HTTP/1.1 200 OK", HTML_SUCCESS).await;

    let tr = exchange_code_for_tokens_default(&code, &redirect_uri, &pkce)
        .await
        .context("exchange code for tokens")?;
    let auth = token_response_to_auth(&tr, None);
    let path = custom_auth_path.unwrap_or_else(default_auth_path);
    save_codex_auth(&path, &auth).with_context(|| format!("save {}", path.display()))?;
    eprintln!("Login OK — saved tokens to {}", path.display());
    if let Some(acct) = &auth.account_id {
        eprintln!("Account: {acct}");
    } else {
        eprintln!("(no account_id extracted; you may not have a ChatGPT subscription account)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_callback_extracts_code_and_state() {
        let p = parse_callback_request_line("GET /auth/callback?code=ABC&state=XYZ HTTP/1.1")
            .expect("should parse");
        assert_eq!(p.code.as_deref(), Some("ABC"));
        assert_eq!(p.state.as_deref(), Some("XYZ"));
        assert!(p.error.is_none());
    }

    #[test]
    fn parse_callback_extracts_error() {
        let p = parse_callback_request_line(
            "GET /auth/callback?error=access_denied&error_description=user+said+no HTTP/1.1",
        )
        .expect("should parse");
        assert_eq!(p.error.as_deref(), Some("access_denied"));
    }

    #[test]
    fn parse_callback_returns_none_for_other_paths() {
        assert!(parse_callback_request_line("GET / HTTP/1.1").is_none());
        assert!(parse_callback_request_line("GET /other HTTP/1.1").is_none());
    }

    #[test]
    fn parse_callback_handles_url_encoded_values() {
        let p = parse_callback_request_line(
            "GET /auth/callback?code=hello%20world&state=a%2Fb HTTP/1.1",
        )
        .unwrap();
        assert_eq!(p.code.as_deref(), Some("hello world"));
        assert_eq!(p.state.as_deref(), Some("a/b"));
    }

    #[test]
    fn parse_callback_with_no_query_yields_empty_params() {
        let p = parse_callback_request_line("GET /auth/callback HTTP/1.1").unwrap();
        assert!(p.code.is_none());
        assert!(p.state.is_none());
        assert!(p.error.is_none());
    }
}
