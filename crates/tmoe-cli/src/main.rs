//! tmoe CLI/TUI エントリポイント。
//!
//! 入力中も裏で Trio が動き続ける (常駐 = 非ブロッキング)。
//! Concierge は 4 人目のエージェントではなく、ユーザー Z 軸推進力を平面に伝達する I/O チャネル。

use anyhow::{Context, Result};
use tmoe_cli::app::App;
use tmoe_cli::concierge::{classify_route, key_to_thrust, translate, Route};
use tmoe_cli::config;
use tmoe_cli::runtime::{
    doctor, history_list, history_show, merge_feature, run_feature, RunOptions, RuntimeEvent,
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::Widget;
use ratatui::Terminal;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tmoe_core::{ThrustChannel, ThrustSender, UserThrust};
use tokio::sync::mpsc;

#[derive(Debug, Default)]
struct Args {
    show_version: bool,
    show_help: bool,
    headless: bool,
    no_worktree: bool,
    cleanup_worktree: bool,
    open_pr: bool,
    config_path: Option<PathBuf>,
    workdir: Option<PathBuf>,
    max_rounds: Option<u32>,
    task: Option<String>,
    subcommand_doctor: bool,
    subcommand_history: Option<HistorySub>,
    subcommand_merge: Option<String>,
    subcommand_codex_login: bool,
    subcommand_init: bool,
    resume_feature_id: Option<String>,
}

fn parse_args(argv: &[String]) -> Result<Args> {
    let mut a = Args::default();
    let mut i = 1;
    let mut positional: Vec<String> = Vec::new();
    while i < argv.len() {
        let s = &argv[i];
        match s.as_str() {
            "--version" | "-V" => a.show_version = true,
            "--help" | "-h" => a.show_help = true,
            "--headless" => a.headless = true,
            "--no-worktree" => a.no_worktree = true,
            "--cleanup-worktree" => a.cleanup_worktree = true,
            "--pr" => a.open_pr = true,
            "--max-rounds" => {
                i += 1;
                let s = argv.get(i).context("--max-rounds needs a number")?;
                a.max_rounds = Some(s.parse().context("--max-rounds must be u32")?);
            }
            "--config" => {
                i += 1;
                a.config_path = Some(PathBuf::from(
                    argv.get(i).context("--config needs a path")?,
                ));
            }
            "--workdir" => {
                i += 1;
                a.workdir = Some(PathBuf::from(
                    argv.get(i).context("--workdir needs a path")?,
                ));
            }
            other if other.starts_with("--") => {
                anyhow::bail!("unknown flag: {other}");
            }
            _ => positional.push(s.clone()),
        }
        i += 1;
    }
    // 互換性: 1 つ目の positional が "ask" の場合は subcommand verb として読み捨て、
    // 残りをタスク文字列にする。DESIGN.md は `tmoe ask "..."` と書いているため、その表記でも
    // 動くようにしておく (素の `tmoe "<task>"` も従来通り動く)。
    if positional.first().map(|s| s.as_str()) == Some("ask") {
        positional.remove(0);
    }
    // `tmoe doctor` は専用 subcommand。task としては扱わない。
    if positional.first().map(|s| s.as_str()) == Some("doctor") {
        a.subcommand_doctor = true;
        positional.remove(0);
    }
    // `tmoe merge <feature_id>`
    if positional.first().map(|s| s.as_str()) == Some("merge") {
        positional.remove(0);
        let id = positional
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("`tmoe merge` needs <feature_id>"))?;
        positional.remove(0);
        a.subcommand_merge = Some(id);
    }
    // `tmoe resume <feature_id> [follow-up text...]`
    if positional.first().map(|s| s.as_str()) == Some("resume") {
        positional.remove(0);
        let id = positional
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("`tmoe resume` needs <feature_id>"))?;
        positional.remove(0);
        a.resume_feature_id = Some(id);
    }
    // `tmoe init` — 初回セットアップウィザード (= ~/.tmoe/config.toml を生成)。
    if positional.first().map(|s| s.as_str()) == Some("init") {
        positional.remove(0);
        a.subcommand_init = true;
    }
    // `tmoe codex login` — ChatGPT Pro/Plus サブスクの OAuth をその場で完了させる。
    if positional.first().map(|s| s.as_str()) == Some("codex") {
        positional.remove(0);
        match positional.first().map(|s| s.as_str()) {
            Some("login") => {
                positional.remove(0);
                a.subcommand_codex_login = true;
            }
            Some(other) => anyhow::bail!("unknown codex subcommand: {other}"),
            None => anyhow::bail!("usage: tmoe codex login"),
        }
    }
    // `tmoe history list` / `tmoe history show <id>`
    if positional.first().map(|s| s.as_str()) == Some("history") {
        positional.remove(0);
        a.subcommand_history = Some(match positional.first().map(|s| s.as_str()) {
            Some("list") => {
                positional.remove(0);
                HistorySub::List
            }
            Some("show") => {
                positional.remove(0);
                let id = positional
                    .first()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("`tmoe history show` needs <feature_id>"))?;
                positional.remove(0);
                HistorySub::Show(id)
            }
            Some(other) => anyhow::bail!("unknown history subcommand: {other}"),
            None => HistorySub::List,
        });
    }
    if !positional.is_empty() {
        a.task = Some(positional.join(" "));
    }
    Ok(a)
}

#[derive(Debug, Clone)]
enum HistorySub {
    List,
    Show(String),
}

#[tokio::main]
async fn main() -> Result<()> {
    // tracing は run_headless / run_tui の各エントリで初期化する。
    // TUI 中に stderr へ流すと ratatui の描画の上に書き殴られて表示が壊れるので、
    // モードごとに writer を切り替える必要がある (TUI 時はファイル、headless は stderr)。

    let argv: Vec<String> = std::env::args().collect();
    let args = parse_args(&argv)?;

    if args.show_version {
        println!("tmoe {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.show_help {
        print_help();
        return Ok(());
    }

    let mut cfg = config::Config::load(args.config_path.as_deref())?;
    let workdir = args
        .workdir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // 非 TUI な subcommand は stderr に tracing を出してよい (TUI と違って ratatui 描画なし)。
    if args.subcommand_doctor
        || args.subcommand_codex_login
        || args.subcommand_init
        || args.subcommand_history.is_some()
        || args.subcommand_merge.is_some()
    {
        init_tracing_stderr();
    }

    if args.subcommand_init {
        tmoe_cli::setup::run_setup_wizard().await?;
        return Ok(());
    }
    if args.subcommand_doctor {
        let ok = doctor(&cfg).await?;
        std::process::exit(if ok { 0 } else { 1 });
    }
    if args.subcommand_codex_login {
        tmoe_cli::codex_login::run_login(None).await?;
        return Ok(());
    }
    if let Some(sub) = args.subcommand_history.clone() {
        match sub {
            HistorySub::List => history_list(&cfg)?,
            HistorySub::Show(id) => history_show(&cfg, &id)?,
        }
        return Ok(());
    }
    if let Some(id) = args.subcommand_merge.clone() {
        merge_feature(&cfg, &workdir, &id)?;
        return Ok(());
    }

    // ここから先は LLM が必要な経路 (TUI / headless / resume)。`~/.tmoe/config.toml` が
    // 存在しない初回起動を救う:
    //   - --config 明示指定 (= args.config_path) があればそれを尊重 (ユーザが分かってる)
    //   - そうでなく TUI に入る予定なら setup ウィザードを自動起動して config を作る
    //   - そうでなく headless で task が来てるなら、tmoe init を案内して exit する
    if args.config_path.is_none() && !tmoe_cli::setup::config_exists() {
        if args.headless {
            anyhow::bail!(
                "tmoe is not configured yet. Run `tmoe init` first to choose a backend, \
                 or set TMOE_LLM_URL / TMOE_LLM_MODEL / TMOE_LLM_BACKEND env vars."
            );
        }
        eprintln!(
            "tmoe is not configured yet. Running `tmoe init` first..."
        );
        tmoe_cli::setup::run_setup_wizard().await?;
        // ウィザード後は config を読み直す。Codex login が成功してれば即 TUI 起動に進める。
        // Skip / Rapid-MLX セットアップだけだった場合も同様に進める (LLM が立ってなければ
        // preflight が落ちて friendly hint を出す)。
        cfg = config::Config::load(args.config_path.as_deref())?;
    }

    let flags = RunFlags {
        use_worktree: !args.no_worktree,
        cleanup_worktree: args.cleanup_worktree,
        open_pr: args.open_pr,
        max_rounds: args.max_rounds.unwrap_or(4),
        resume_feature_id: args.resume_feature_id.clone(),
    };
    // resume の場合、task が空でも OK (前回の続き)。デフォルト follow-up を入れる。
    let effective_task = match (args.task.clone(), &args.resume_feature_id) {
        (Some(t), _) => Some(t),
        (None, Some(_)) => Some("Continue from where you left off.".to_string()),
        (None, None) => None,
    };
    match effective_task {
        Some(task) if args.headless => run_headless(task, cfg, workdir, flags).await,
        Some(task) => run_tui(Some(task), cfg, workdir, flags),
        None if args.headless => {
            anyhow::bail!("--headless requires a task argument: tmoe --headless \"<task>\"")
        }
        None => run_tui(None, cfg, workdir, flags),
    }
}

/// Concierge ペイン内に表示する短いヘルプ。`tmoe --help` (CLI 側 `print_help`) は
/// シェル起動時用なので別に長文。こっちは TUI で「何が打てるんだっけ」を即返す用。
fn concierge_help_lines(log_path: &Path) -> Vec<String> {
    vec![
        "(tmoe) ── help ──".into(),
        "(tmoe)   <free text>     start a new feature with that as the task".into(),
        "(tmoe)   help / ?        show this help".into(),
        "(tmoe)   quit / exit     leave the TUI (same as Esc / Ctrl-C)".into(),
        "(tmoe) ── hotkeys ──".into(),
        "(tmoe)   Ctrl-G          z_thrust=1.0 (resume the active feature)".into(),
        "(tmoe)   Ctrl-P          z_thrust=0   (pause the active feature)".into(),
        "(tmoe)   Ctrl-K          stop the active feature".into(),
        "(tmoe)   Esc / Ctrl-C    quit the TUI (sends Stop first)".into(),
        format!("(tmoe) logs -> {}", log_path.display()),
    ]
}

fn print_help() {
    println!("tmoe — 3 + 1 (3 agents + user Z-axis) coding agent");
    println!();
    println!("USAGE:");
    println!("    tmoe [options] \"<task description>\"");
    println!("    tmoe ask \"<task>\"  — same as above (DESIGN-doc form)");
    println!("    tmoe init           — first-run setup wizard (writes ~/.tmoe/config.toml)");
    println!("    tmoe doctor         — diagnose config + LLM reachability + optional bins");
    println!("    tmoe history list   — list past features stored in ~/.tmoe");
    println!("    tmoe history show <feature_id>");
    println!("    tmoe resume <feature_id> [follow-up text...]");
    println!("    tmoe merge <feature_id>  — git merge --no-ff tmoe/feature/<id>");
    println!("    tmoe codex login    — log in to ChatGPT Pro/Plus to use the Codex backend");
    println!("    tmoe                — start the TUI without a task");
    println!("    tmoe --version      — print version");
    println!();
    println!("OPTIONS:");
    println!("    --headless          run to completion without TUI (auto Go)");
    println!("    --no-worktree       do not carve a feature worktree (work in cwd)");
    println!("    --cleanup-worktree  prune the feature worktree after success");
    println!("    --pr                after commit, open a draft PR via gh");
    println!("    --max-rounds N      max Trio rounds per session (default 4); redirect/park count as 1");
    println!("    --config <path>     use this TOML config (else ~/.tmoe/config.toml)");
    println!("    --workdir <path>    treat this dir as the workspace root (else cwd)");
    println!();
    println!("HOTKEYS (TUI):");
    println!("    Ctrl-P              pause Trio (z_thrust=0)");
    println!("    Ctrl-G              resume Trio (z_thrust=1.0)");
    println!("    Ctrl-K              stop the current feature");
    println!("    Enter               submit Concierge line");
    println!("    Ctrl-C / Esc        quit");
}

/// stderr に tracing を出す (= --headless / 非 TUI コマンド向け)。重複初期化は no-op。
fn init_tracing_stderr() {
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TMOE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

/// `<history_root>/tmoe.log` に tracing を流す (= TUI 起動中)。stderr に出すと ratatui の
/// 描画の上に書き殴られて表示が壊れるのでファイルに逃がす。失敗したら sink にフォールバック。
fn init_tracing_file(history_root: &Path) -> PathBuf {
    let log_path = history_root.join("tmoe.log");
    let _ = std::fs::create_dir_all(history_root);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            let _ = tracing_subscriber::fmt()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_env("TMOE_LOG")
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .try_init();
        }
        Err(_) => {
            let _ = tracing_subscriber::fmt()
                .with_writer(io::sink)
                .try_init();
        }
    }
    log_path
}

async fn run_headless(
    task: String,
    cfg: config::Config,
    workdir: PathBuf,
    flags: RunFlags,
) -> Result<()> {
    init_tracing_stderr();
    let (thrust_tx, thrust_rx) = ThrustChannel::new();
    thrust_tx
        .send(UserThrust::Go { strength: 1.0 })
        .map_err(|e| anyhow::anyhow!("send thrust: {e}"))?;
    drop(thrust_tx);

    let (event_tx, mut event_rx) = mpsc::channel::<RuntimeEvent>(64);

    let runner = tokio::spawn(async move {
        let mut opts = RunOptions::new(task, workdir);
        opts.use_worktree = flags.use_worktree;
        opts.cleanup_worktree = flags.cleanup_worktree;
        opts.open_pr = flags.open_pr;
        opts.max_rounds = flags.max_rounds;
        opts.resume_feature_id = flags.resume_feature_id.clone();
        run_feature(cfg, opts, thrust_rx, Some(event_tx)).await
    });

    while let Some(ev) = event_rx.recv().await {
        match ev {
            RuntimeEvent::Status(s) => eprintln!("[status] {s}"),
            RuntimeEvent::TrioLog(s) => eprintln!("[trio]   {s}"),
            RuntimeEvent::Warning(s) => eprintln!("[warn]   {s}"),
            RuntimeEvent::ConciergeReply(s) => eprintln!("[chat]   {s}"),
            // Routed は TUI loop 用の制御シグナルで headless では用が無い (= headless は
            // 既に "task として走れ" の前提で run_feature を回している)。無視する。
            RuntimeEvent::Routed { .. } => {}
            RuntimeEvent::Done { ok, message } => {
                eprintln!("[done]   ok={ok} {message}");
                break;
            }
        }
    }
    runner
        .await
        .map_err(|e| anyhow::anyhow!("runtime task join: {e}"))??;
    Ok(())
}

#[derive(Debug, Clone)]
struct RunFlags {
    use_worktree: bool,
    cleanup_worktree: bool,
    open_pr: bool,
    max_rounds: u32,
    resume_feature_id: Option<String>,
}

fn run_tui(
    initial_task: Option<String>,
    cfg: config::Config,
    workdir: PathBuf,
    flags: RunFlags,
) -> Result<()> {
    let log_path = init_tracing_file(&cfg.history_root);
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let res = tui_loop(&mut terminal, initial_task, cfg, workdir, flags, log_path);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    res
}

fn tui_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    initial_task: Option<String>,
    cfg: config::Config,
    workdir: PathBuf,
    flags: RunFlags,
    log_path: PathBuf,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let (event_tx, mut event_rx) = mpsc::channel::<RuntimeEvent>(64);

    let mut app = App::new();
    app.on_concierge("(tmoe) Type a task and press Enter to start.".into());
    app.on_concierge("(tmoe) Built-in commands: help / quit".into());
    app.on_concierge("(tmoe) Hotkeys: Ctrl-G=Go  Ctrl-P=Pause  Ctrl-K=Stop  Esc=Quit".into());
    app.on_concierge(format!("(tmoe) logs -> {}", log_path.display()));

    // 現在アクティブなセッションへの thrust 送信口。None ならアイドル (タスク未投入)。
    let mut current_thrust_tx: Option<ThrustSender> = None;
    let mut runtime_handle: Option<tokio::task::JoinHandle<Result<()>>> = None;

    // 初期タスクがあれば即起動。
    if let Some(task) = initial_task {
        let (tx, rx) = ThrustChannel::new();
        // 初回の Z 軸推進を自動投入 (= ヘッドレス相当の即発進)。Concierge 経由で
        // 後から Pause / Stop / Redirect すれば変更できる。
        let _ = tx.send(UserThrust::Go { strength: 1.0 });
        spawn_session(
            &runtime,
            cfg.clone(),
            workdir.clone(),
            flags.clone(),
            task,
            rx,
            event_tx.clone(),
            &mut runtime_handle,
            &mut app,
        );
        current_thrust_tx = Some(tx);
    }

    loop {
        // ランタイムイベントを drain して TUI 状態に反映。
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                RuntimeEvent::Status(s) => app.status = s,
                RuntimeEvent::TrioLog(s) => app.on_trio(s),
                RuntimeEvent::Warning(s) => app.on_warning(s),
                RuntimeEvent::ConciergeReply(s) => {
                    app.on_concierge(format!("(tmoe) {s}"));
                }
                RuntimeEvent::Routed { task, user_input } => {
                    if task {
                        // classifier が "task" と判定した → アイドルなら新規セッション spawn。
                        // (current_thrust_tx が Some なら既に走っているはずで、再 spawn は不要)
                        if current_thrust_tx.is_none() {
                            let (tx, rx) = ThrustChannel::new();
                            let _ = tx.send(UserThrust::Go { strength: 1.0 });
                            spawn_session(
                                &runtime,
                                cfg.clone(),
                                workdir.clone(),
                                flags.clone(),
                                user_input,
                                rx,
                                event_tx.clone(),
                                &mut runtime_handle,
                                &mut app,
                            );
                            current_thrust_tx = Some(tx);
                        }
                    }
                    // chat の場合: ConciergeReply 側で表示済みなので何もしない。
                }
                RuntimeEvent::Done { ok, message } => {
                    app.on_concierge(format!(
                        "(tmoe) {} {message}",
                        if ok { "✓ done:" } else { "✗ failed:" }
                    ));
                    // セッション終了 → アイドルに戻す。次の Concierge 入力で新規セッションを spawn できる。
                    current_thrust_tx = None;
                }
            }
        }

        // ランタイム task の生死を確認 (Done を受け取らずに panic した場合の保険)。
        if let Some(h) = &runtime_handle {
            if h.is_finished() {
                runtime_handle = None;
            }
        }

        terminal.draw(|f| {
            let area = f.area();
            (&app).render(area, f.buffer_mut());
            // 入力プロンプトの末尾にカーソルを置く。これをやらないと macOS Terminal の
            // 日本語 IME が pre-edit テキストを「最後にカーソルがあった場所」に描いてしまい、
            // 結果として alternate screen が予期せずスクロールして TUI 全体が崩れる。
            let (cx, cy) = app.cursor_position(area);
            f.set_cursor_position(ratatui::layout::Position::new(cx, cy));
        })?;

        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
                        if let Some(tx) = &current_thrust_tx {
                            let _ = tx.send(UserThrust::Stop);
                        }
                        break;
                    }
                    _ if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(thrust) = key_to_thrust(k) {
                            let label = match &thrust {
                                UserThrust::Pause => "(Ctrl-P pause)",
                                UserThrust::Go { .. } => "(Ctrl-G go)",
                                UserThrust::Stop => "(Ctrl-K stop)",
                                UserThrust::Redirect { .. } => "(redirect)",
                            };
                            if let Some(tx) = &current_thrust_tx {
                                send_thrust(tx, &mut app, thrust, label);
                            } else {
                                app.on_concierge(
                                    "(tmoe) no active session; type a task and press Enter".into(),
                                );
                            }
                        }
                    }
                    (KeyCode::Char(c), _) => {
                        app.append_char(c);
                    }
                    (KeyCode::Backspace, _) => app.backspace(),
                    (KeyCode::Enter, _) => {
                        let line = app.take_input();
                        if line.is_empty() {
                            // 空 Enter: 何もしない。
                        } else if matches!(
                            line.trim().to_lowercase().as_str(),
                            "help" | "?" | "/help" | ":help" | "h"
                        ) {
                            // ヘルプ語の入力は task ではなく Concierge 内で処理する。
                            // 入っているのが既存セッション中でもアイドル中でも同じ挙動。
                            app.on_concierge(format!("user> {line}"));
                            for line in concierge_help_lines(&log_path) {
                                app.on_concierge(line);
                            }
                        } else if matches!(
                            line.trim().to_lowercase().as_str(),
                            "quit" | "exit" | ":q" | ":quit" | "q"
                        ) {
                            // 明示的な終了コマンド。Esc / Ctrl-C と同等。アクティブ session があれば
                            // Stop シグナルだけ送って TUI を畳む (タスクの後始末は runtime 側がやる)。
                            if let Some(tx) = &current_thrust_tx {
                                let _ = tx.send(UserThrust::Stop);
                            }
                            break;
                        } else if current_thrust_tx.is_none() {
                            // **アイドル時の Enter は LLM classifier に分類させる**。
                            // - chat (= 挨拶 / メタ質問 / 雑談) → ConciergeReply で左ペインに返事
                            // - task (= 実装依頼) → Routed { task: true } を投げて main loop で
                            //   spawn_session が走る
                            // 判定は CONCIERGE_SYSTEM プロンプトに任せる。文字列マッチは禁止。
                            // 失敗時は task に倒れる (= 実装依頼を取り逃がさない安全側)。
                            app.on_concierge(format!("user> {line}"));
                            app.on_concierge("(tmoe) thinking…".into());
                            let cfg_for_classify = cfg.clone();
                            let event_tx_for_classify = event_tx.clone();
                            let line_for_classify = line.clone();
                            runtime.spawn(async move {
                                let client = match tmoe_llm::OpenAiCompatClient::new(
                                    cfg_for_classify.llm.clone(),
                                ) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        let _ = event_tx_for_classify
                                            .send(RuntimeEvent::Warning(format!(
                                                "classify_route: build llm client: {e}"
                                            )))
                                            .await;
                                        // LLM すら立てられない場合は task に倒す。
                                        let _ = event_tx_for_classify
                                            .send(RuntimeEvent::Routed {
                                                task: true,
                                                user_input: line_for_classify,
                                            })
                                            .await;
                                        return;
                                    }
                                };
                                let route = classify_route(&client, &line_for_classify).await;
                                match route {
                                    Route::Chat { reply } => {
                                        let _ = event_tx_for_classify
                                            .send(RuntimeEvent::ConciergeReply(reply))
                                            .await;
                                        // chat は session を spawn しないので current_thrust_tx
                                        // は None のまま。Routed { task: false } を流して main loop
                                        // 側に「もう何もしなくていい」を伝える (= log だけ)。
                                        let _ = event_tx_for_classify
                                            .send(RuntimeEvent::Routed {
                                                task: false,
                                                user_input: line_for_classify,
                                            })
                                            .await;
                                    }
                                    Route::Task => {
                                        let _ = event_tx_for_classify
                                            .send(RuntimeEvent::Routed {
                                                task: true,
                                                user_input: line_for_classify,
                                            })
                                            .await;
                                    }
                                }
                            });
                        } else {
                            // セッション稼動中: 通常の thrust ルートへ。
                            app.on_concierge(format!("user> {line}"));
                            let thrust = translate(&line);
                            let label = match &thrust {
                                UserThrust::Go { strength } => format!("(go strength={strength})"),
                                UserThrust::Pause => "(pause)".into(),
                                UserThrust::Stop => "(stop)".into(),
                                UserThrust::Redirect { instruction } => {
                                    format!("(redirect: {instruction})")
                                }
                            };
                            if let Some(tx) = &current_thrust_tx {
                                send_thrust(tx, &mut app, thrust, &label);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(h) = runtime_handle {
        let _ = runtime.block_on(async { h.await });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_session(
    runtime: &tokio::runtime::Runtime,
    cfg: config::Config,
    workdir: PathBuf,
    flags: RunFlags,
    task: String,
    thrust_rx: tmoe_core::ThrustReceiver,
    event_tx: mpsc::Sender<RuntimeEvent>,
    runtime_handle: &mut Option<tokio::task::JoinHandle<Result<()>>>,
    app: &mut App,
) {
    *runtime_handle = Some(runtime.spawn(async move {
        let mut opts = RunOptions::new(task, workdir);
        opts.use_worktree = flags.use_worktree;
        opts.cleanup_worktree = flags.cleanup_worktree;
        opts.open_pr = flags.open_pr;
        opts.max_rounds = flags.max_rounds;
        opts.resume_feature_id = flags.resume_feature_id.clone();
        run_feature(cfg, opts, thrust_rx, Some(event_tx)).await
    }));
    app.on_concierge("(tmoe) feature spawned.".into());
}

fn send_thrust(tx: &ThrustSender, app: &mut App, thrust: UserThrust, label: &str) {
    let _ = tx.send(thrust);
    app.on_trio(format!("Z-axis: {label}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        std::iter::once("tmoe".to_string())
            .chain(parts.iter().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn parses_positional_task() {
        let a = parse_args(&argv(&["implement gcd in src/math.rs"])).unwrap();
        assert_eq!(a.task.as_deref(), Some("implement gcd in src/math.rs"));
        assert!(!a.headless);
    }

    #[test]
    fn parses_flags_and_task() {
        let a = parse_args(&argv(&[
            "--headless",
            "--no-worktree",
            "--pr",
            "rename old to new",
        ]))
        .unwrap();
        assert!(a.headless);
        assert!(a.no_worktree);
        assert!(a.open_pr);
        assert_eq!(a.task.as_deref(), Some("rename old to new"));
    }

    #[test]
    fn rejects_unknown_flag() {
        let err = parse_args(&argv(&["--bogus"])).unwrap_err();
        assert!(err.to_string().contains("unknown flag"));
    }

    #[test]
    fn config_path_consumes_arg() {
        let a = parse_args(&argv(&["--config", "/tmp/cfg.toml", "task"])).unwrap();
        assert_eq!(a.config_path, Some(PathBuf::from("/tmp/cfg.toml")));
        assert_eq!(a.task.as_deref(), Some("task"));
    }

    #[test]
    fn version_short() {
        let a = parse_args(&argv(&["-V"])).unwrap();
        assert!(a.show_version);
    }

    #[test]
    fn ask_subcommand_alias() {
        // DESIGN.md は `tmoe ask "..."` と書いているので、ask verb を読み捨てて同等にする。
        let a = parse_args(&argv(&["ask", "rename foo to bar"])).unwrap();
        assert_eq!(a.task.as_deref(), Some("rename foo to bar"));
    }
}
