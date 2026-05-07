//! tmoe-cli の lib 面: TUI バイナリで使う内部モジュールを公開する。
//!
//! `[[bin]]` だけだと integration test が個別モジュールを掴めないので、
//! 同じ crate に lib も載せて test/再利用に開く。
//! バイナリ実体は `src/main.rs` 側で `use tmoe_cli::...` する。

pub mod agents_md;
pub mod app;
pub mod concierge;
pub mod config;
pub mod history_tool;
pub mod question_tool;
pub mod runtime;
pub mod skill_tool;
pub mod source_tool;
