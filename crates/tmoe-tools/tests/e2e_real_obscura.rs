//! 実 Obscura バイナリに対する gated e2e。
//!
//! 環境変数 `TMOE_E2E_OBSCURA_BIN` がセットされた時のみ走る。
//!
//! ```sh
//! curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/obscura-aarch64-macos.tar.gz
//! tar xzf obscura-aarch64-macos.tar.gz
//! TMOE_E2E_OBSCURA_BIN=$(pwd)/obscura cargo test -p tmoe-tools --test e2e_real_obscura -- --ignored
//! ```

use std::sync::Arc;
use tmoe_tools::{
    permission::PermissionProfile, registry::ToolRegistry, tool::ToolCall, WebFetchTool,
    WebSearchTool,
};

fn obscura_bin() -> Option<String> {
    std::env::var("TMOE_E2E_OBSCURA_BIN").ok()
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_OBSCURA_BIN; run with --ignored"]
async fn real_obscura_fetches_example_dot_com() {
    let bin = match obscura_bin() {
        Some(b) => b,
        None => {
            eprintln!("skipping: TMOE_E2E_OBSCURA_BIN not set");
            return;
        }
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(WebFetchTool::with_bin(bin)));
    let out = reg
        .invoke(
            &ToolCall {
                name: "web_fetch".into(),
                args: serde_json::json!({"url": "https://example.com"}),
            },
            &PermissionProfile::worker(),
        )
        .await
        .expect("web_fetch failed");
    eprintln!("--- example.com text dump ({} bytes) ---", out.stdout.len());
    eprintln!("{}", &out.stdout.chars().take(800).collect::<String>());
    let lower = out.stdout.to_lowercase();
    assert!(
        lower.contains("example domain") || lower.contains("for use in"),
        "expected example.com canonical text in output; got first 200 chars:\n{}",
        out.stdout.chars().take(200).collect::<String>()
    );
}

#[tokio::test]
#[ignore = "gated by TMOE_E2E_OBSCURA_BIN; run with --ignored"]
async fn real_obscura_search_returns_results_text() {
    let bin = match obscura_bin() {
        Some(b) => b,
        None => {
            eprintln!("skipping: TMOE_E2E_OBSCURA_BIN not set");
            return;
        }
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(WebSearchTool::with_bin(bin)));
    let out = reg
        .invoke(
            &ToolCall {
                name: "web_search".into(),
                args: serde_json::json!({
                    "query": "rust pageindex agentic rag",
                    "engine": "duckduckgo"
                }),
            },
            &PermissionProfile::worker(),
        )
        .await
        .expect("web_search failed");
    eprintln!("--- duckduckgo search ({} bytes) ---", out.stdout.len());
    eprintln!("{}", out.stdout.chars().take(1500).collect::<String>());
    assert!(
        !out.stdout.trim().is_empty(),
        "duckduckgo search returned empty body"
    );
    let lower = out.stdout.to_lowercase();
    // クエリ語のいずれかが結果テキストに現れていれば検索結果として妥当 (DDG の表示形式の揺れを許容)。
    let needles = ["pageindex", "agentic", "rag", "rust"];
    let hits = needles.iter().filter(|n| lower.contains(*n)).count();
    assert!(
        hits >= 2,
        "search result should mention at least two query keywords; hits={hits} of {:?}",
        needles
    );
}
