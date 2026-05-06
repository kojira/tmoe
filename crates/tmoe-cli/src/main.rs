//! tmoe CLI/TUI エントリポイント。
//!
//! 入力中も裏で Trio が動き続ける (常駐 = 非ブロッキング)。
//! Concierge は 4 人目のエージェントではなく、ユーザー Z 軸推進力を平面に伝達する I/O チャネル。

mod app;
mod concierge;

use anyhow::Result;
use app::App;
use concierge::{key_to_thrust, translate};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::Widget;
use ratatui::Terminal;
use std::io;
use std::time::Duration;
use tmoe_core::{ThrustChannel, ThrustSender, UserThrust};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("tmoe {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    run_tui()
}

fn print_help() {
    println!("tmoe — 3 + 1 (3 agents + user Z-axis) coding agent");
    println!("");
    println!("USAGE:");
    println!("    tmoe                — start the TUI");
    println!("    tmoe --version      — print version");
    println!("    tmoe --help         — this message");
    println!("");
    println!("Hotkeys (TUI):");
    println!("    Ctrl-P              pause Trio (z_thrust=0)");
    println!("    Ctrl-K              stop the current feature");
    println!("    Ctrl-T              cycle feature tree");
    println!("    Enter               submit Concierge line");
    println!("    Ctrl-C / Esc        quit");
}

fn run_tui() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let res = run_loop(&mut terminal);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    res
}

fn run_loop<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) -> Result<()> {
    let mut app = App::new();
    let (tx, _rx) = ThrustChannel::new();
    app.on_concierge("(tmoe) start typing. Enter to submit.".into());
    app.on_concierge("(tmoe) Type 'go' to advance Trio, 'pause' to park, 'stop' to abort.".into());

    loop {
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
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => break,
                    _ if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(thrust) = key_to_thrust(k) {
                            let label = match &thrust {
                                UserThrust::Pause => "(Ctrl-P pause)",
                                UserThrust::Go { .. } => "(Ctrl-G go)",
                                UserThrust::Stop => "(Ctrl-K stop)",
                                UserThrust::Redirect { .. } => "(redirect)",
                            };
                            send_thrust(&tx, &mut app, thrust, label);
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
                            app.on_concierge(echo.clone());
                            let thrust = translate(&line);
                            let label = match &thrust {
                                UserThrust::Go { strength } => format!("(go strength={strength})"),
                                UserThrust::Pause => "(pause)".into(),
                                UserThrust::Stop => "(stop)".into(),
                                UserThrust::Redirect { instruction } => {
                                    format!("(redirect: {instruction})")
                                }
                            };
                            send_thrust(&tx, &mut app, thrust, &label);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn send_thrust(tx: &ThrustSender, app: &mut App, thrust: UserThrust, label: &str) {
    let _ = tx.send(thrust);
    app.on_trio(format!("Z-axis: {label}"));
}
