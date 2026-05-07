//! TUI 状態と描画ロジック (ratatui)。
//!
//! 4 ペイン構成:
//!   ① Concierge 対話 (左)        — Z 軸推進入力
//!   ② Trio ライブログ (右上)      — Worker / Supervisor / Observer の発話
//!   ③ Observer 警告 (右下)       — ループ・記憶ずれ・要件逸脱
//!   ④ 機能ツリー (下部)           — feature 一覧
//!
//! `App` は表示専用状態。Trio との接続は `main.rs::tui_loop` が tokio::spawn で行い、
//! `RuntimeEvent` を mpsc 経由で受信して各ペインに反映する。

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Widget};

/// TUI ペインに保持するログの最大行数。これを超えると古い行が破棄される (= スクロールバック上限)。
/// 1 行 ≈ 200 文字想定で、`500 * 4 = 2000` ペイン × 200B ≈ 400KB 程度に収まる。
pub const LOG_CAP: usize = 500;

#[derive(Debug, Default, Clone)]
pub struct App {
    pub concierge: Vec<String>,
    pub trio_log: Vec<String>,
    pub observer_warnings: Vec<String>,
    pub features: Vec<String>,
    pub input_buffer: String,
    pub status: String,
    /// 各ログの上限。テスト用に上書き可能。
    pub log_cap: usize,
}

impl App {
    /// 入力プロンプトの末尾 (= IME pre-edit / 通常の caret) を置くべき画面座標を計算する。
    /// `area` は描画ターゲット全体 (= `Frame::area()`)。レイアウトは `render` と同じ計算。
    /// macOS Terminal の IME は OS 側のカーソル位置に pre-edit を描くので、これを設定し
    /// 忘れると pre-edit が想定外の場所に出て画面が崩れる。
    pub fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(7), Constraint::Length(3)])
            .split(area);
        let middle = outer[0];
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(middle);
        let concierge_pane = cols[0];
        // Concierge ペインの中、input_buffer の末尾位置:
        //   x = pane.x + 1 (left border) + display width of input_buffer
        //   y = pane.y + pane.height - 2 (bottom border above)
        let input_visible_width = self.input_buffer.chars().fold(0u16, |acc, c| {
            // CJK 全角文字は 2 列、それ以外は 1 列で概算 (unicode-width の正確版を入れない代わりの近似)。
            let w = if c as u32 >= 0x1100 { 2 } else { 1 };
            acc.saturating_add(w)
        });
        let x = concierge_pane.x.saturating_add(1).saturating_add(input_visible_width);
        let y = concierge_pane
            .y
            .saturating_add(concierge_pane.height.saturating_sub(2));
        (
            x.min(concierge_pane.x.saturating_add(concierge_pane.width.saturating_sub(2))),
            y,
        )
    }

    pub fn new() -> Self {
        Self {
            status: "tmoe ready — 3 + 1 mode".into(),
            log_cap: LOG_CAP,
            ..Default::default()
        }
    }

    fn push_bounded(buf: &mut Vec<String>, cap: usize, line: String) {
        buf.push(line);
        // バウンド超過時は **先頭を切る** (= 古いログから捨てる)。drain は O(N) になるが
        // pop は最新を捨ててしまうので逆。VecDeque の方が pop_front が O(1) だが、
        // ratatui の List は &[ListItem] なので連続スライスが必要。少数行ずつ落とす実装で十分。
        if buf.len() > cap {
            let excess = buf.len() - cap;
            buf.drain(0..excess);
        }
    }

    pub fn on_concierge(&mut self, line: String) {
        Self::push_bounded(&mut self.concierge, self.log_cap, line);
    }
    pub fn on_trio(&mut self, line: String) {
        Self::push_bounded(&mut self.trio_log, self.log_cap, line);
    }
    pub fn on_warning(&mut self, line: String) {
        Self::push_bounded(&mut self.observer_warnings, self.log_cap, line);
    }
    pub fn set_features(&mut self, items: Vec<String>) {
        self.features = items;
        if self.features.len() > self.log_cap {
            let excess = self.features.len() - self.log_cap;
            self.features.drain(0..excess);
        }
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

    /// ペイン高さに合わせて末尾 N 行のスライスを返す。新しい行が下に来る。
    pub fn tail<'a>(buf: &'a [String], n: usize) -> &'a [String] {
        if buf.len() <= n {
            buf
        } else {
            &buf[buf.len() - n..]
        }
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

        // 各ペインに収まる末尾 N 行だけ render する (= スクロールバック: 新しい行が見える)。
        // pane height はボーダーを除いた内寄り高さに依存するので、保守的に
        // `pane.height - 2` をターゲットにする (タイトル + 下線で 2 行消費)。
        let pane_h = |area: Rect| area.height.saturating_sub(2) as usize;
        let conc_n = pane_h(concierge_pane);
        let trio_n = pane_h(trio_pane);
        let warn_n = pane_h(warn_pane);
        let feat_n = pane_h(bottom_tree);

        // Concierge ペイン: 履歴 (末尾 N-2 行) + 入力プロンプト 2 行。
        let conc_keep = conc_n.saturating_sub(2);
        let conc_visible = App::tail(&self.concierge, conc_keep);
        let mut conc_lines: Vec<Line> = conc_visible.iter().map(|s| Line::from(s.as_str())).collect();
        conc_lines.push(Line::from("> "));
        conc_lines.push(Line::from(self.input_buffer.as_str()));
        Paragraph::new(conc_lines)
            .block(Block::default().borders(Borders::ALL).title("Concierge — Z 軸推進入力"))
            .render(concierge_pane, buf);

        // Trio ライブログ (末尾 N 行)。
        let trio_visible = App::tail(&self.trio_log, trio_n);
        let trio_items: Vec<ListItem> =
            trio_visible.iter().map(|s| ListItem::new(s.as_str())).collect();
        List::new(trio_items)
            .block(Block::default().borders(Borders::ALL).title("Trio (Worker/Supervisor/Observer)"))
            .render(trio_pane, buf);

        // Observer 警告 (末尾 N 行)。
        let warn_visible = App::tail(&self.observer_warnings, warn_n);
        let warn_items: Vec<ListItem> =
            warn_visible.iter().map(|s| ListItem::new(s.as_str())).collect();
        List::new(warn_items)
            .block(Block::default().borders(Borders::ALL).title("Observer warnings"))
            .render(warn_pane, buf);

        // 機能ツリー (末尾 N 行)。
        let feat_visible = App::tail(&self.features, feat_n);
        let f_items: Vec<ListItem> = feat_visible.iter().map(|s| ListItem::new(s.as_str())).collect();
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
    fn log_is_bounded_by_cap_and_keeps_newest() {
        let mut app = App::new();
        app.log_cap = 3;
        for i in 0..10 {
            app.on_trio(format!("line {i}"));
        }
        assert_eq!(app.trio_log.len(), 3);
        assert_eq!(app.trio_log[0], "line 7");
        assert_eq!(app.trio_log[2], "line 9");
    }

    #[test]
    fn rendered_pane_shows_newest_lines_when_log_overflows_pane_height() {
        let mut app = App::new();
        // ペインに収まる以上の行を投入する。`tail` で末尾だけ render される設計の検証。
        for i in 0..40 {
            app.on_trio(format!("trio_msg_{i}"));
        }
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| (&app).render(f.area(), f.buffer_mut())).unwrap();
        let buffer = term.backend().buffer().clone();
        let dump: String = buffer.content.iter().map(|c| c.symbol()).collect();
        // 最新行は必ず表示される。
        assert!(dump.contains("trio_msg_39"), "newest line missing");
        // 古すぎる行は窓から外れて見えない (ペイン高さは ~10-15 行なので 0 番台は出ない)。
        assert!(!dump.contains("trio_msg_0\n") && !dump.contains("trio_msg_5\n"),
            "old line should not be in the visible pane");
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
