//! tmoe CLI/TUI エントリポイント。
//!
//! 入力中も裏で Trio が動き続ける (常駐 = 非ブロッキング)。
//! Concierge は 4 人目のエージェントではなく、ユーザー Z 軸推進力を平面に伝達する I/O チャネル。

mod app;
mod concierge;
mod config;
mod runtime;

use anyhow::{Context, Result};
use app::App;
use concierge::{key_to_thrust, translate};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::Widget;
use ratatui::Terminal;
use runtime::{run_feature, RunOptions, RuntimeEvent};
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use tmoe_core::{ThrustChannel, ThrustSender, UserThrust};
use tokio::sync::mpsc;

#[derive(Debug, Default)]
struct Args {
    show_version: bool,
    show_help: bool,
    headless: bool,
    no_worktree: bool,
    open_pr: bool,
    config_path: Option<PathBuf>,
    workdir: Option<PathBuf>,
    task: Option<String>,
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
            "--pr" => a.open_pr = true,
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
    if !positional.is_empty() {
        a.task = Some(positional.join(" "));
    }
    Ok(a)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TMOE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .ok();

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

    let cfg = config::Config::load(args.config_path.as_deref())?;
    let workdir = args
        .workdir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    match args.task.clone() {
        Some(task) if args.headless => {
            run_headless(task, cfg, workdir, !args.no_worktree, args.open_pr).await
        }
        Some(task) => run_tui(
            Some(task),
            cfg,
            workdir,
            RunFlags {
                use_worktree: !args.no_worktree,
                open_pr: args.open_pr,
            },
        ),
        None if args.headless => {
            anyhow::bail!("--headless requires a task argument: tmoe --headless \"<task>\"")
        }
        None => run_tui(
            None,
            cfg,
            workdir,
            RunFlags {
                use_worktree: !args.no_worktree,
                open_pr: args.open_pr,
            },
        ),
    }
}

fn print_help() {
    println!("tmoe — 3 + 1 (3 agents + user Z-axis) coding agent");
    println!();
    println!("USAGE:");
    println!("    tmoe [options] \"<task description>\"");
    println!("    tmoe                — start the TUI without a task");
    println!("    tmoe --version      — print version");
    println!();
    println!("OPTIONS:");
    println!("    --headless          run to completion without TUI (auto Go)");
    println!("    --no-worktree       do not carve a feature worktree (work in cwd)");
    println!("    --pr                after commit, open a draft PR via gh");
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

async fn run_headless(
    task: String,
    cfg: config::Config,
    workdir: PathBuf,
    use_worktree: bool,
    open_pr: bool,
) -> Result<()> {
    let (thrust_tx, thrust_rx) = ThrustChannel::new();
    thrust_tx
        .send(UserThrust::Go { strength: 1.0 })
        .map_err(|e| anyhow::anyhow!("send thrust: {e}"))?;
    drop(thrust_tx);

    let (event_tx, mut event_rx) = mpsc::channel::<RuntimeEvent>(64);

    let runner = tokio::spawn(async move {
        run_feature(
            cfg,
            RunOptions {
                task,
                workdir,
                use_worktree,
                open_pr,
                auto_go: true,
            },
            thrust_rx,
            Some(event_tx),
        )
        .await
    });

    while let Some(ev) = event_rx.recv().await {
        match ev {
            RuntimeEvent::Status(s) => eprintln!("[status] {s}"),
            RuntimeEvent::TrioLog(s) => eprintln!("[trio]   {s}"),
            RuntimeEvent::Warning(s) => eprintln!("[warn]   {s}"),
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

#[derive(Debug, Clone, Copy)]
struct RunFlags {
    use_worktree: bool,
    open_pr: bool,
}

fn run_tui(
    initial_task: Option<String>,
    cfg: config::Config,
    workdir: PathBuf,
    flags: RunFlags,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let res = tui_loop(&mut terminal, initial_task, cfg, workdir, flags);
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
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let (thrust_tx, thrust_rx) = ThrustChannel::new();
    let (event_tx, mut event_rx) = mpsc::channel::<RuntimeEvent>(64);

    let mut app = App::new();
    app.on_concierge("(tmoe) start typing. Enter to submit.".into());
    app.on_concierge("(tmoe) Ctrl-G=Go  Ctrl-P=Pause  Ctrl-K=Stop  Esc=Quit".into());

    let mut runtime_handle: Option<tokio::task::JoinHandle<Result<()>>> = None;
    let mut already_started = false;
    if let Some(task) = initial_task {
        already_started = true;
        let cfg_c = cfg.clone();
        let workdir_c = workdir.clone();
        let etx = event_tx.clone();
        runtime_handle = Some(runtime.spawn(async move {
            run_feature(
                cfg_c,
                RunOptions {
                    task,
                    workdir: workdir_c,
                    use_worktree: flags.use_worktree,
                    open_pr: flags.open_pr,
                    auto_go: false,
                },
                thrust_rx,
                Some(etx),
            )
            .await
        }));
        app.on_concierge("(tmoe) feature spawned. Press Ctrl-G to advance.".into());
    } else {
        let _ = thrust_rx;
        app.on_concierge("(tmoe) no task; provide one via the CLI: tmoe \"<task>\"".into());
    }

    loop {
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                RuntimeEvent::Status(s) => app.status = s,
                RuntimeEvent::TrioLog(s) => app.on_trio(s),
                RuntimeEvent::Warning(s) => app.on_warning(s),
                RuntimeEvent::Done { ok, message } => {
                    app.on_concierge(format!(
                        "(tmoe) {} {message}",
                        if ok { "✓ done:" } else { "✗ failed:" }
                    ));
                }
            }
        }

        terminal.draw(|f| {
            let area = f.area();
            (&app).render(area, f.buffer_mut());
        })?;

        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
                        let _ = thrust_tx.send(UserThrust::Stop);
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
                            send_thrust(&thrust_tx, &mut app, thrust, label);
                        }
                    }
                    (KeyCode::Char(c), _) => {
                        app.append_char(c);
                    }
                    (KeyCode::Backspace, _) => app.backspace(),
                    (KeyCode::Enter, _) => {
                        let line = app.take_input();
                        if !line.is_empty() {
                            let echo = format!("user> {line}");
                            app.on_concierge(echo);
                            let thrust = translate(&line);
                            let label = match &thrust {
                                UserThrust::Go { strength } => format!("(go strength={strength})"),
                                UserThrust::Pause => "(pause)".into(),
                                UserThrust::Stop => "(stop)".into(),
                                UserThrust::Redirect { instruction } => {
                                    format!("(redirect: {instruction})")
                                }
                            };
                            send_thrust(&thrust_tx, &mut app, thrust, &label);
                            if !already_started {
                                app.on_warning(
                                    "(tmoe) interactive task spawn from TUI not wired yet — \
                                     restart with: tmoe \"<task>\""
                                        .into(),
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if let Some(h) = &runtime_handle {
            if h.is_finished() {
                while let Ok(ev) = event_rx.try_recv() {
                    if let RuntimeEvent::TrioLog(s) | RuntimeEvent::Status(s) = ev {
                        app.on_trio(s);
                    }
                }
            }
        }
    }

    if let Some(h) = runtime_handle {
        let _ = runtime.block_on(async { h.await });
    }
    Ok(())
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
}
