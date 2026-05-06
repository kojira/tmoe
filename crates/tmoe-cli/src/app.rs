//! TUI 状態と描画ロジック (ratatui)。
//!
//! 4 ペイン構成:
//!   ① Concierge 対話 (左)        — Z 軸推進入力
//!   ② Trio ライブログ (右上)      — Worker / Supervisor / Observer の発話
//!   ③ Observer 警告 (右下)       — ループ・記憶ずれ・要件逸脱
//!   ④ 機能ツリー (下部)           — feature 一覧
//!
//! `App` は表示専用状態。Trio との接続は別タスク (Phase 7 / e2e で組合せ)。

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Widget};

#[derive(Debug, Default, Clone)]
pub struct App {
    pub concierge: Vec<String>,
    pub trio_log: Vec<String>,
    pub observer_warnings: Vec<String>,
    pub features: Vec<String>,
    pub input_buffer: String,
    pub status: String,
}

impl App {
    pub fn new() -> Self {
        Self {
            status: "tmoe ready — 3 + 1 mode".into(),
            ..Default::default()
        }
    }

    pub fn on_concierge(&mut self, line: String) {
        self.concierge.push(line);
    }
    pub fn on_trio(&mut self, line: String) {
        self.trio_log.push(line);
    }
    pub fn on_warning(&mut self, line: String) {
        self.observer_warnings.push(line);
    }
    pub fn set_features(&mut self, items: Vec<String>) {
        self.features = items;
    }
    pub fn append_char(&mut self, c: char) {
        self.input_buffer.push(c);
    }
    pub fn backspace(&mut self) {
        self.input_buffer.pop();
    }
    pub fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input_buffer)
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(7), Constraint::Length(3)])
            .split(area);
        let middle = outer[0];
        let bottom_tree = outer[1];
        let status_bar = outer[2];

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(middle);

        let concierge_pane = cols[0];
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(cols[1]);
        let trio_pane = right[0];
        let warn_pane = right[1];

        // Concierge ペイン: 履歴 + 入力行。
        let mut conc_lines: Vec<Line> = self.concierge.iter().map(|s| Line::from(s.as_str())).collect();
        conc_lines.push(Line::from("> "));
        conc_lines.push(Line::from(self.input_buffer.as_str()));
        Paragraph::new(conc_lines)
            .block(Block::default().borders(Borders::ALL).title("Concierge — Z 軸推進入力"))
            .render(concierge_pane, buf);

        // Trio ライブログ。
        let trio_items: Vec<ListItem> = self
            .trio_log
            .iter()
            .map(|s| ListItem::new(s.as_str()))
            .collect();
        List::new(trio_items)
            .block(Block::default().borders(Borders::ALL).title("Trio (Worker/Supervisor/Observer)"))
            .render(trio_pane, buf);

        // Observer 警告。
        let warn_items: Vec<ListItem> = self
            .observer_warnings
            .iter()
            .map(|s| ListItem::new(s.as_str()))
            .collect();
        List::new(warn_items)
            .block(Block::default().borders(Borders::ALL).title("Observer warnings"))
            .render(warn_pane, buf);

        // 機能ツリー。
        let f_items: Vec<ListItem> = self.features.iter().map(|s| ListItem::new(s.as_str())).collect();
        List::new(f_items)
            .block(Block::default().borders(Borders::ALL).title("Features"))
            .render(bottom_tree, buf);

        // Status bar.
        Paragraph::new(self.status.as_str())
            .style(Style::default().add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL))
            .render(status_bar, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn renders_all_four_panes_with_titles() {
        let mut app = App::new();
        app.on_concierge("user: go".into());
        app.on_trio("worker: implementing".into());
        app.on_warning("loop suspected".into());
        app.set_features(vec!["[in_progress] add gcd".into()]);
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| (&app).render(f.area(), f.buffer_mut())).unwrap();
        let buffer = term.backend().buffer().clone();
        let dump = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(dump.contains("Concierge"));
        assert!(dump.contains("Trio"));
        assert!(dump.contains("Observer"));
        assert!(dump.contains("Features"));
        assert!(dump.contains("3 + 1"));
    }

    #[test]
    fn input_buffer_handling() {
        let mut app = App::new();
        for c in "hello".chars() {
            app.append_char(c);
        }
        app.backspace();
        assert_eq!(app.input_buffer, "hell");
        let taken = app.take_input();
        assert_eq!(taken, "hell");
        assert!(app.input_buffer.is_empty());
    }
}
