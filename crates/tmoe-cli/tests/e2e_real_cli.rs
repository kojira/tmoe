//! Real-LLM CLI binary smoke e2e.
//!
//! `tmoe --headless "<task>"` を実 LLM (Rapid-MLX) 越しにビルド済みバイナリで起動し、
//! 1 機能を最後まで完走させる。これが通って初めて「個別 crate は動くが結線層が空」と
//! いう旧状態が解消されたと言える。
//!
//! 検証点:
//! - `tmoe` バイナリが Trio + History + Tools + ViewProvider + Worktree を実際に結線して動く
//! - workdir 内に Worker のツール呼び出しでファイルが書き込まれる
//! - `--no-worktree` オプションで worktree を切らずに直接書き込めるパスも回る
//! - 終了コードは 0、stderr に `[done]   ok=true` が乗る

use std::env;
use std::path::PathBuf;
use std::process::Command;
use tempfile::tempdir;

fn cargo_bin_tmoe() -> PathBuf {
    // Cargo がテスト実行時に `CARGO_BIN_EXE_tmoe` を設定する。
    // これは現バイナリを示す絶対パス。テスト前に `cargo build -p tmoe-cli` を要する。
    PathBuf::from(env!("CARGO_BIN_EXE_tmoe"))
}

#[test]
#[ignore = "gated by TMOE_E2E_LLM_URL; run with --ignored"]
fn real_cli_headless_writes_file_against_rapid_mlx() {
    let url = match env::var("TMOE_E2E_LLM_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("skipping: TMOE_E2E_LLM_URL not set");
            return;
        }
    };
    let model = env::var("TMOE_E2E_LLM_MODEL").unwrap_or_else(|_| "qwen3-coder-30b".into());

    let workdir = tempdir().unwrap();
    let bin = cargo_bin_tmoe();

    // git リポジトリではないので worktree は自動的にスキップされるが、念のため明示。
    let task = "Create src/lib.rs containing exactly: pub fn add(a: i64, b: i64) -> i64 { a + b }\n\
                Emit ONE edit_file tool call (one ```json block) and then DONE on its own line.";

    let out = Command::new(&bin)
        .arg("--headless")
        .arg("--no-worktree")
        .arg("--workdir")
        .arg(workdir.path())
        .arg(task)
        .env("TMOE_LLM_URL", &url)
        .env("TMOE_LLM_MODEL", &model)
        .env("TMOE_LLM_BACKEND", "rapid_mlx")
        // 履歴を一時ディレクトリに隔離 (~/.tmoe を汚さない)。
        .env("HOME", workdir.path())
        .output()
        .expect("spawn tmoe");

    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    eprintln!("--- tmoe stderr ---\n{stderr}");
    eprintln!("--- tmoe stdout ---\n{stdout}");
    assert!(out.status.success(), "tmoe exited non-zero");

    // [done] ok=true がイベントログに乗っていること。
    assert!(stderr.contains("[done]   ok=true"), "stderr missing done=true: {stderr}");

    // Worker のツール呼び出しでファイルが実体として書かれていること。
    let lib_rs = workdir.path().join("src/lib.rs");
    assert!(lib_rs.exists(), "src/lib.rs missing — Worker tool call did not land");
    let body = std::fs::read_to_string(&lib_rs).unwrap();
    assert!(body.contains("pub fn add"), "lib.rs body lacks pub fn add: {body}");

    // History が ~/.tmoe (= HOME=workdir) 配下に作られていること。
    let hist_db = workdir.path().join(".tmoe").join("db.sqlite");
    assert!(hist_db.exists(), "history db not created at {}", hist_db.display());
}
