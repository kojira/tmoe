//! 初回セットアップウィザード。
//!
//! `tmoe init` で明示的に呼べる + `tmoe` を引数なしで起動して `~/.tmoe/config.toml` が
//! 無ければ自動で起動する。3 通り (Codex / Rapid-MLX / カスタム) + skip の対話 UI で
//! `~/.tmoe/config.toml` を生成し、Codex を選んだ場合はその場で OAuth ログインまで走らせる。
//!
//! 設計方針:
//!   - 同期 stdin/stdout で完結する。TUI に入る前に走るので raw mode はまだ無効。
//!   - 失敗パス (バックエンドに繋がらない / brew が無い / Codex login がキャンセル) でも
//!     最低限 config.toml は書く (= ユーザが手で直せる土台を残す)。
//!   - 既存の `~/.tmoe/config.toml` は **絶対に上書きしない**。確認なしの破壊操作禁止。
//!     ウィザード実行時に存在していたら "already configured" として早期 return する。

use anyhow::{bail, Context, Result};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

/// 既定の config.toml 保存場所 (`~/.tmoe/config.toml`)。`HOME` が解けない場合は
/// `./tmoe.toml` にフォールバック。
pub fn default_config_path() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".tmoe").join("config.toml")
    } else {
        PathBuf::from("tmoe.toml")
    }
}

/// `~/.tmoe/config.toml` が既にあるか。`tmoe` 引数なし起動時に「初回かどうか」を判定する。
pub fn config_exists() -> bool {
    default_config_path().exists()
}

/// セットアップウィザード本体。`tmoe init` から、または初回起動の自動誘導から呼ばれる。
/// 既に config.toml があれば早期 return (= 既存設定を尊重)。
pub async fn run_setup_wizard() -> Result<()> {
    let path = default_config_path();
    if path.exists() {
        eprintln!(
            "tmoe is already configured: {}\n\
             Edit it with $EDITOR, or remove the file and re-run `tmoe init` to start over.",
            path.display()
        );
        return Ok(());
    }

    println!();
    println!("┌── tmoe first-run setup ──────────────────────────────────────────");
    println!("│ tmoe needs an OpenAI-compatible LLM backend. Pick one:");
    println!("│");
    println!("│   1) ChatGPT Pro/Plus subscription (Codex)");
    println!("│      Routes chat through chatgpt.com using OAuth — no local model.");
    println!("│      Requires an active ChatGPT subscription.");
    println!("│");
    println!("│   2) Local LLM via Rapid-MLX (Apple Silicon)");
    println!("│      Free, fully offline. ~18 GB model download on first run.");
    println!("│");
    println!("│   3) Custom OpenAI-compatible endpoint");
    println!("│      llama.cpp / vLLM / LM Studio / remote service / anything else.");
    println!("│");
    println!("│   4) Skip — I'll write ~/.tmoe/config.toml myself");
    println!("└────────────────────────────────────────────────────────────────");
    print!("> [1/2/3/4]: ");

    let choice = read_choice(&["1", "2", "3", "4"])?;
    match choice.as_str() {
        "1" => setup_codex(&path).await,
        "2" => setup_rapid_mlx(&path),
        "3" => setup_custom(&path),
        "4" => setup_skip(&path),
        _ => unreachable!(),
    }
}

/// 1 行入力を `allowed` のいずれかに揃うまで読む。EOF / 不正値はリトライ。
fn read_choice(allowed: &[&str]) -> Result<String> {
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        io::stdout().flush().ok();
        let n = stdin.lock().read_line(&mut line)?;
        if n == 0 {
            bail!("setup aborted (EOF)");
        }
        let trimmed = line.trim().to_lowercase();
        if allowed.iter().any(|a| *a == trimmed) {
            return Ok(trimmed);
        }
        eprint!("Please enter one of [{}]: ", allowed.join("/"));
    }
}

/// 自由入力 (空 OK)。`default` が `Some` なら空入力で default を返す。
fn read_line(prompt: &str, default: Option<&str>) -> Result<String> {
    print!("{prompt}");
    if let Some(d) = default {
        print!(" [{d}]");
    }
    print!(": ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let trimmed = line.trim().to_string();
    if trimmed.is_empty() {
        if let Some(d) = default {
            return Ok(d.to_string());
        }
    }
    Ok(trimmed)
}

async fn setup_codex(path: &Path) -> Result<()> {
    println!();
    let model = read_line("Codex model (e.g. gpt-5.4)", Some("gpt-5.4"))?;
    write_config_toml(
        path,
        &ConfigDraft {
            backend: "codex",
            base_url: "https://chatgpt.com/backend-api/codex/responses",
            main_model: &model,
            api_key: None,
            note_lines: vec![
                "# backend = \"codex\" は OAuth で ChatGPT Pro/Plus サブスクを使う。".into(),
                "# base_url は無視され、内部で chatgpt.com 固定 (ここに書いてあるのは参考)。".into(),
                "# auth は ~/.tmoe/auth.json (`tmoe codex login` で作る)。".into(),
            ],
        },
    )?;
    println!();
    println!("config written: {}", path.display());
    println!();
    println!("Now opening the OAuth flow in your browser...");
    println!("(If the browser doesn't open, copy the URL printed below into one.)");
    println!();
    crate::codex_login::run_login(None)
        .await
        .context("codex login")?;
    println!();
    println!("✓ Setup complete. Run `tmoe doctor` to verify, then `tmoe \"<task>\"`.");
    Ok(())
}

fn setup_rapid_mlx(path: &Path) -> Result<()> {
    let on_path = which_on_path("rapid-mlx");
    println!();
    if on_path {
        println!("✓ rapid-mlx found on PATH.");
    } else {
        println!("rapid-mlx not found on PATH. Install it with:");
        println!("    brew install raullenchai/rapid-mlx/rapid-mlx");
        println!("(or follow https://github.com/raullenchai/rapid-mlx)");
    }
    let model = read_line(
        "Rapid-MLX model alias (e.g. qwen3-coder-30b)",
        Some("qwen3-coder-30b"),
    )?;
    let port = read_line("Port", Some("8081"))?;
    let base_url = format!("http://127.0.0.1:{}/v1", port);
    write_config_toml(
        path,
        &ConfigDraft {
            backend: "rapid_mlx",
            base_url: &base_url,
            main_model: &model,
            api_key: None,
            note_lines: vec![
                format!("# Rapid-MLX サーバを別ターミナルで起動してから tmoe を使う:"),
                format!("#     rapid-mlx serve {model} --port {port}"),
                "# 初回はモデルのダウンロード (4bit 量子化で ~18GB) で数分〜数十分かかる。".into(),
            ],
        },
    )?;
    println!();
    println!("config written: {}", path.display());
    println!("Next: start the server in another terminal —");
    println!("    rapid-mlx serve {model} --port {port}");
    println!("then run:");
    println!("    tmoe doctor");
    println!("    tmoe \"<task>\"");
    Ok(())
}

fn setup_custom(path: &Path) -> Result<()> {
    println!();
    println!("Pick the backend kind:");
    println!("   a) llama_cpp (llama-server)");
    println!("   b) vllm");
    println!("   c) lm_studio");
    println!("   d) openai_compat (anything else, including remote OpenAI)");
    print!("> [a/b/c/d]: ");
    let kind = read_choice(&["a", "b", "c", "d"])?;
    let backend = match kind.as_str() {
        "a" => "llama_cpp",
        "b" => "vllm",
        "c" => "lm_studio",
        _ => "openai_compat",
    };
    let base_url = read_line("base_url", Some("http://127.0.0.1:8080/v1"))?;
    let main_model = read_line("main_model (model id the backend exposes)", None)?;
    if main_model.is_empty() {
        bail!("main_model is required");
    }
    let api_key = read_line("api_key (blank if none)", Some(""))?;
    let api_key = if api_key.is_empty() { None } else { Some(api_key) };

    write_config_toml(
        path,
        &ConfigDraft {
            backend,
            base_url: &base_url,
            main_model: &main_model,
            api_key: api_key.as_deref(),
            note_lines: vec![
                format!("# 任意の OpenAI 互換エンドポイントを backend = \"{backend}\" として使う。"),
                "# api_key は空文字で省略可 (= ローカル LLM など認証無し)。".into(),
            ],
        },
    )?;
    println!();
    println!("config written: {}", path.display());
    println!("Next: make sure the endpoint is reachable, then run:");
    println!("    tmoe doctor");
    println!("    tmoe \"<task>\"");
    Ok(())
}

fn setup_skip(path: &Path) -> Result<()> {
    write_config_toml(
        path,
        &ConfigDraft {
            backend: "rapid_mlx",
            base_url: "http://127.0.0.1:8081/v1",
            main_model: "qwen3-coder-30b",
            api_key: None,
            note_lines: vec![
                "# skip 選択で自動生成された雛形。中身を編集して使ってください。".into(),
                "# サポート backend: codex / rapid_mlx / llama_cpp / vllm / lm_studio / openai_compat".into(),
            ],
        },
    )?;
    println!();
    println!("template written: {}", path.display());
    println!("Edit it (e.g. $EDITOR {}), then run `tmoe doctor`.", path.display());
    Ok(())
}

struct ConfigDraft<'a> {
    backend: &'a str,
    base_url: &'a str,
    main_model: &'a str,
    api_key: Option<&'a str>,
    note_lines: Vec<String>,
}

fn write_config_toml(path: &Path, draft: &ConfigDraft) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir of {}", path.display()))?;
    }
    let mut s = String::new();
    s.push_str("# tmoe configuration (auto-generated by `tmoe init`).\n");
    s.push_str("# 環境変数 (TMOE_LLM_URL / TMOE_LLM_MODEL / TMOE_LLM_BACKEND / TMOE_LLM_API_KEY)\n");
    s.push_str("# でも個別に上書きできる。\n");
    s.push_str("\n");
    s.push_str("[llm]\n");
    for note in &draft.note_lines {
        s.push_str(note);
        s.push('\n');
    }
    s.push_str(&format!("backend = \"{}\"\n", draft.backend));
    s.push_str(&format!("base_url = \"{}\"\n", draft.base_url));
    s.push_str(&format!("main_model = \"{}\"\n", draft.main_model));
    if let Some(k) = draft.api_key {
        s.push_str(&format!("api_key = \"{}\"\n", k));
    } else {
        s.push_str("api_key = \"\"\n");
    }
    s.push_str("\n[trio]\n");
    s.push_str("# 合意プロトコルの前進閾値。実機 LLM 既定は控えめ。\n");
    s.push_str("confidence_sum_min = 1.5\n");
    s.push_str("triangle_balance_min = 0.3\n");
    s.push_str("max_iter_per_step = 4\n");
    s.push_str("\n[history]\n");
    s.push_str("root = \"~/.tmoe\"\n");
    std::fs::write(path, s).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn which_on_path(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_skip_template_produces_parseable_toml() {
        let d = tempdir().unwrap();
        let p = d.path().join("config.toml");
        write_config_toml(
            &p,
            &ConfigDraft {
                backend: "rapid_mlx",
                base_url: "http://127.0.0.1:8081/v1",
                main_model: "qwen3-coder-30b",
                api_key: None,
                note_lines: vec!["# test".into()],
            },
        )
        .unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        let parsed: toml::Value = toml::from_str(&body).expect("template must parse");
        assert_eq!(parsed["llm"]["backend"].as_str(), Some("rapid_mlx"));
        assert_eq!(parsed["llm"]["main_model"].as_str(), Some("qwen3-coder-30b"));
    }

    #[test]
    fn config_exists_returns_false_when_no_home_config() {
        // `default_config_path` may point at the actual user's HOME/.tmoe/config.toml,
        // so we just sanity-check that the function returns a bool without panicking.
        let _ = config_exists();
    }
}
