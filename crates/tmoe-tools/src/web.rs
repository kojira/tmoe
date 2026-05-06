//! Web ビルトインスキル — Obscura ヘッドレスブラウザ (h4ckf0r0day/obscura) を背後に使う。
//!
//! Obscura は Rust 製の CDP 互換ヘッドレスブラウザで、`obscura fetch <URL> --dump text`
//! のように呼び出すと JS レンダリング済みのプレーンテキストを返す。tmoe の Worker は
//! web_fetch / web_search を `Permission::Read` として呼べる。
//!
//! - `web_fetch(url)`        : 単一 URL をマークダウン化して返す
//! - `web_search(query)`     : DuckDuckGo HTML 版を Obscura で取得し検索結果のマークダウンを返す
//!
//! 実バイナリのパスは `TMOE_OBSCURA_BIN` 環境変数で上書き可能。デフォルトは `obscura`
//! (PATH 依存)。

use crate::permission::Permission;
use crate::tool::{Tool, ToolError, ToolOutput, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_TIMEOUT_SECS: u64 = 30;

fn default_obscura_bin() -> OsString {
    std::env::var_os("TMOE_OBSCURA_BIN").unwrap_or_else(|| OsString::from("obscura"))
}

fn validate_url(url: &str) -> Result<(), ToolError> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(ToolError::Args(format!("URL must be http(s)://: {url}")));
    }
    if url.contains('\n') || url.contains(' ') {
        return Err(ToolError::Args(format!("URL contains whitespace: {url}")));
    }
    Ok(())
}

async fn run_obscura(bin: &OsStr, args: &[&str]) -> ToolResult {
    let exec = Command::new(bin).args(args).output();
    let result = timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS), exec).await;
    let output = match result {
        Err(_) => return Err(ToolError::Exec(format!("obscura timed out after {DEFAULT_TIMEOUT_SECS}s"))),
        Ok(Err(e)) => {
            let hint = if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "obscura binary not found ({:?}). Install h4ckf0r0day/obscura and put it in PATH, \
                     or set TMOE_OBSCURA_BIN to its absolute path.",
                    bin
                )
            } else {
                format!("failed to spawn obscura: {e}")
            };
            return Err(ToolError::Exec(hint));
        }
        Ok(Ok(o)) => o,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(ToolError::Exec(format!(
            "obscura exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(ToolOutput::text(stdout))
}

// --- web_fetch -----------------------------------------------------------

#[derive(Default)]
pub struct WebFetchTool {
    /// 上書き: テストや特殊環境で obscura のパスを直接指定する場合に使う。
    /// None の場合は `TMOE_OBSCURA_BIN` 環境変数か `obscura` (PATH) にフォールバック。
    pub bin_override: Option<OsString>,
}

impl WebFetchTool {
    pub fn new() -> Self { Self { bin_override: None } }
    pub fn with_bin(bin: impl Into<OsString>) -> Self {
        Self { bin_override: Some(bin.into()) }
    }
    fn resolved_bin(&self) -> OsString {
        self.bin_override.clone().unwrap_or_else(default_obscura_bin)
    }
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str { "web_fetch" }
    fn requires(&self) -> Permission { Permission::Read }

    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: FetchArgs = serde_json::from_value(args.clone())?;
        validate_url(&a.url)?;
        run_obscura(&self.resolved_bin(), &["fetch", &a.url, "--dump", "text"]).await
    }
}

// --- web_search ----------------------------------------------------------

#[derive(Default)]
pub struct WebSearchTool {
    pub bin_override: Option<OsString>,
}

impl WebSearchTool {
    pub fn new() -> Self { Self { bin_override: None } }
    pub fn with_bin(bin: impl Into<OsString>) -> Self {
        Self { bin_override: Some(bin.into()) }
    }
    fn resolved_bin(&self) -> OsString {
        self.bin_override.clone().unwrap_or_else(default_obscura_bin)
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    engine: Option<String>,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str { "web_search" }
    fn requires(&self) -> Permission { Permission::Read }

    async fn call(&self, args: &serde_json::Value) -> ToolResult {
        let a: SearchArgs = serde_json::from_value(args.clone())?;
        if a.query.trim().is_empty() {
            return Err(ToolError::Args("query is empty".into()));
        }
        let engine = a.engine.as_deref().unwrap_or("duckduckgo");
        let url = match engine {
            "duckduckgo" | "ddg" => format!(
                "https://html.duckduckgo.com/html/?q={}",
                percent_encode(&a.query)
            ),
            "bing" => format!("https://www.bing.com/search?q={}", percent_encode(&a.query)),
            other => return Err(ToolError::Args(format!("unsupported engine: {other}"))),
        };
        validate_url(&url)?;
        run_obscura(&self.resolved_bin(), &["fetch", &url, "--dump", "text"]).await
    }
}

/// 最小限のパーセントエンコード (RFC 3986 unreserved + space → %20)。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        let is_unreserved = byte.is_ascii_alphanumeric()
            || byte == b'-'
            || byte == b'_'
            || byte == b'.'
            || byte == b'~';
        if is_unreserved {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionProfile;
    use crate::registry::ToolRegistry;
    use crate::tool::ToolCall;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn write_fake_obscura(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("obscura");
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// Fake obscura: 引数を受け取り、固定の markdown を stdout に書く shell スクリプト。
    /// 引数を <log_path> に書き出すので、CLI 引数の検査もできる。
    fn fake_emitter_script(log_path: &std::path::Path) -> String {
        format!(
            r##"#!/bin/sh
printf '%s\n' "$*" > "{log}"
echo "# fake markdown"
echo ""
echo "URL: $2"
echo "DUMP: $4"
"##,
            log = log_path.display()
        )
    }

    fn fake_failing_script() -> String {
        r##"#!/bin/sh
echo "boom" 1>&2
exit 7
"##
        .to_string()
    }

    #[tokio::test]
    async fn web_fetch_calls_obscura_with_markdown_dump() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("obscura.args");
        let bin = write_fake_obscura(dir.path(), &fake_emitter_script(&log_path));

        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(WebFetchTool::with_bin(&bin)));

        let out = reg
            .invoke(
                &ToolCall {
                    name: "web_fetch".into(),
                    args: serde_json::json!({"url": "https://example.com"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(out.stdout.contains("fake markdown"));
        assert!(out.stdout.contains("URL: https://example.com"));
        assert!(out.stdout.contains("DUMP: text"));

        let recorded = std::fs::read_to_string(&log_path).unwrap();
        assert!(recorded.contains("fetch"), "args were: {recorded}");
        assert!(recorded.contains("https://example.com"));
        assert!(recorded.contains("--dump"));
        assert!(recorded.contains("text"));
    }

    #[tokio::test]
    async fn web_fetch_rejects_non_http_url() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(WebFetchTool::with_bin("/nonexistent/obscura")));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "web_fetch".into(),
                    args: serde_json::json!({"url": "file:///etc/passwd"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Args(_)));
    }

    #[tokio::test]
    async fn web_search_builds_duckduckgo_url() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("obscura.args");
        let bin = write_fake_obscura(dir.path(), &fake_emitter_script(&log_path));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(WebSearchTool::with_bin(&bin)));
        let out = reg
            .invoke(
                &ToolCall {
                    name: "web_search".into(),
                    args: serde_json::json!({"query": "rust pageindex agentic rag"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap();
        assert!(
            out.stdout.contains(
                "URL: https://html.duckduckgo.com/html/?q=rust%20pageindex%20agentic%20rag"
            ),
            "stdout was:\n{}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn web_search_rejects_unknown_engine() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(WebSearchTool::with_bin("/nonexistent/obscura")));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "web_search".into(),
                    args: serde_json::json!({"query": "x", "engine": "nope"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Args(_)));
    }

    #[tokio::test]
    async fn web_fetch_propagates_obscura_failure() {
        let dir = tempdir().unwrap();
        let bin = write_fake_obscura(dir.path(), &fake_failing_script());
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(WebFetchTool::with_bin(&bin)));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "web_fetch".into(),
                    args: serde_json::json!({"url": "https://example.com"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Exec(msg) => {
                assert!(msg.contains("obscura exited"), "msg={msg}");
                assert!(msg.contains("boom"), "msg={msg}");
            }
            other => panic!("expected Exec error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_obscura_returns_helpful_error() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(WebFetchTool::with_bin("/nonexistent/path/to/obscura-xyz")));
        let err = reg
            .invoke(
                &ToolCall {
                    name: "web_fetch".into(),
                    args: serde_json::json!({"url": "https://example.com"}),
                },
                &PermissionProfile::worker(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Exec(msg) => {
                assert!(
                    msg.contains("not found") || msg.contains("No such file"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Exec error, got {other:?}"),
        }
    }

    #[test]
    fn percent_encode_keeps_unreserved() {
        assert_eq!(percent_encode("abc.-_~"), "abc.-_~");
        assert_eq!(percent_encode("rust pageindex"), "rust%20pageindex");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
    }
}
