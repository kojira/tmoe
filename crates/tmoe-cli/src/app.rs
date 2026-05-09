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
use unicode_width::UnicodeWidthChar;

/// TUI ペインに保持するログの最大行数。これを超えると古い行が破棄される (= スクロールバック上限)。
/// 1 行 ≈ 200 文字想定で、`500 * 4 = 2000` ペイン × 200B ≈ 400KB 程度に収まる。
pub const LOG_CAP: usize = 500;

/// Esc / Ctrl-C 2 段確認の判定結果。`App::on_quit_key` が返す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuitDecision {
    /// 通常処理を続けてよい (pending 状態でなく、quit キーでもない)。
    Pass,
    /// 1 回目の Esc/Ctrl-C を受けた。警告だけ出して保留。
    Pending,
    /// pending 中に他のキーが来たので取消。キー自体は飲み込む。
    Cancel,
    /// 確認完了。本当に TUI を抜ける。
    Quit,
}

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
    /// Esc / Ctrl-C を 1 回押した状態。次のキーが Esc / Ctrl-C / y なら本当に終了、
    /// それ以外なら取り消し。誤操作で session 中断を防ぐための 2 段確認。
    pub quit_pending: bool,
}

impl App {
    /// 入力プロンプトの末尾 (= IME pre-edit / 通常の caret) を置くべき画面座標を計算する。
    /// `area` は描画ターゲット全体 (= `Frame::area()`)。レイアウトは `render` と同じ計算。
    /// macOS Terminal の IME は OS 側のカーソル位置に pre-edit を描くので、これを設定し
    /// 忘れると pre-edit が想定外の場所に出て画面が崩れる。
    pub fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let input_bar = Self::input_bar_rect(area);
        // 入力行は画面最下端の 1 行 (border 無し)。
        //   x = input_bar.x + 2 ("> " prompt) + display width of input_buffer
        //   y = input_bar.y
        const PROMPT_WIDTH: u16 = 2; // "> "
        let input_visible_width: u16 = self
            .input_buffer
            .chars()
            .map(|c| c.width().unwrap_or(0) as u16)
            .sum();
        let x = input_bar
            .x
            .saturating_add(PROMPT_WIDTH)
            .saturating_add(input_visible_width);
        let y = input_bar.y;
        (
            x.min(input_bar.x.saturating_add(input_bar.width.saturating_sub(1))),
            y,
        )
    }

    /// 全体 area からレイアウトを切って、入力行 (画面最下 1 行) の Rect を返す。
    /// `cursor_position` と `render` で同じ計算を使うために共通化。
    fn input_bar_rect(area: Rect) -> Rect {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),    // middle (concierge / trio / observer)
                Constraint::Length(7), // features
                Constraint::Length(1), // status (border 無し)
                Constraint::Length(1), // input bar (border 無し、画面最下端)
            ])
            .split(area);
        outer[3]
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

    /// Esc / Ctrl-C 2 段確認の状態機械。
    /// `is_quit_key`: 今押されたキーが Esc または Ctrl-C か。
    /// `is_yes_key`:  今押されたキーが 'y' / 'Y' (確認用) か。
    /// 結果に従って main loop が分岐する。`Pending` / `Cancel` の時は警告メッセージも
    /// concierge ペインに追記しておく (= UX の一貫性のため、状態と表示を一緒に変える)。
    pub fn on_quit_key(&mut self, is_quit_key: bool, is_yes_key: bool) -> QuitDecision {
        if self.quit_pending {
            if is_quit_key || is_yes_key {
                // 2 回目: 本決定。
                QuitDecision::Quit
            } else {
                // pending 中に他のキー: 取消。キー自体は呑み込む。
                self.quit_pending = false;
                self.on_concierge("(tmoe) quit canceled.".into());
                QuitDecision::Cancel
            }
        } else if is_quit_key {
            // 1 回目: 警告だけ出して保留。
            self.quit_pending = true;
            self.on_concierge(
                "(tmoe) press Esc / Ctrl-C / y again to quit, any other key to cancel."
                    .into(),
            );
            QuitDecision::Pending
        } else {
            // 通常入力。状態は据え置きで loop の通常処理へ流す。
            QuitDecision::Pass
        }
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
        // 縦 4 段:
        //   ① middle (Concierge + Trio + Observer の 3 ペイン)
        //   ② features (枠あり、~7 行)
        //   ③ status (border 無し 1 行)
        //   ④ input bar (border 無し 1 行、画面最下端)
        // 入力欄を最下端に独立させる狙い: IME pre-edit が右ペインの border / 内側に
        // 干渉するのを物理的に防ぐ (右ペインと同じ y 行に入力が来ないので衝突しない)。
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(7),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        let middle = outer[0];
        let bottom_tree = outer[1];
        let status_bar = outer[2];
        let input_bar = outer[3];

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

        // Concierge ペイン: 入力 UI を持たず、ログ末尾 N 行のみ表示。
        let conc_visible = App::tail(&self.concierge, conc_n);
        let conc_lines: Vec<Line> = conc_visible.iter().map(|s| Line::from(s.as_str())).collect();
        Paragraph::new(conc_lines)
            .block(Block::default().borders(Borders::ALL).title("Concierge"))
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

        // Status bar (border 無し 1 行)。
        Paragraph::new(self.status.as_str())
            .style(Style::default().add_modifier(Modifier::BOLD))
            .render(status_bar, buf);

        // 入力欄 (border 無し 1 行、画面最下端)。`> ` プロンプトの右に input_buffer を直書き。
        // border が無いので IME pre-edit がこの行で右にはみ出ても、隣接ペインの border 文字を
        // 上書きしない (= 右ペインの中段にある縦線位置とは y がずれるので衝突しない)。
        Paragraph::new(format!("> {}", self.input_buffer))
            .render(input_bar, buf);
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

    #[test]
    fn cursor_position_uses_unicode_width_for_full_width_chars() {
        // 日本語 (full-width) 2 文字は 4 セル分。半角 ASCII 2 文字は 2 セル分。
        // 旧実装は半角カタカナ ｱ (U+FF71) を 2 セルと誤算していたが、unicode-width 経由で
        // 正しく 1 セルになるので、IME pre-edit が右ペインを侵食しない。
        let mut app = App::new();
        let area = Rect::new(0, 0, 80, 24);
        app.input_buffer = "ab".into();
        let (x_ascii, _) = app.cursor_position(area);
        app.input_buffer = "あい".into();
        let (x_full, _) = app.cursor_position(area);
        assert_eq!(x_full.saturating_sub(x_ascii), 2);

        app.input_buffer = "ｱｲ".into();
        let (x_half_kana, _) = app.cursor_position(area);
        assert_eq!(x_half_kana, x_ascii);
    }

    #[test]
    fn cursor_position_includes_prompt_offset() {
        // input_buffer が空でも、`> ` プロンプト分 (= 2 cell) だけ画面左端から右にずれる。
        let app = App::new();
        let area = Rect::new(0, 0, 80, 24);
        let (x, _) = app.cursor_position(area);
        // input bar は border 無しで area.x = 0 から始まる。"> " 2 cell。
        assert_eq!(x, 2);
    }

    #[test]
    fn cursor_y_is_screen_bottom_row() {
        // 入力行は **画面の最下行** に独立させてある (= 中段ペインの下端ではない)。
        let app = App::new();
        let area = Rect::new(0, 0, 80, 24);
        let (_, y) = app.cursor_position(area);
        // 24 行画面の最下端 = row 23 (0-indexed)
        assert_eq!(y, area.height - 1);
    }

    #[test]
    fn on_quit_key_first_press_is_pending_not_quit() {
        let mut app = App::new();
        let d = app.on_quit_key(true, false);
        assert_eq!(d, QuitDecision::Pending);
        assert!(app.quit_pending);
        // 警告が左ペインに足されているはず (= ユーザに「もう 1 回押せ」を見せる)。
        assert!(app
            .concierge
            .iter()
            .any(|s| s.contains("press Esc") && s.contains("any other key to cancel")));
    }

    #[test]
    fn on_quit_key_second_quit_press_decides_quit() {
        let mut app = App::new();
        let _ = app.on_quit_key(true, false); // 1 回目
        let d = app.on_quit_key(true, false); // 2 回目
        assert_eq!(d, QuitDecision::Quit);
        // Quit に進む時は flag は据え置きで OK (loop が break するので参照されない)。
    }

    #[test]
    fn on_quit_key_pending_then_yes_decides_quit() {
        // 「Esc 押してから 'y' で確定」フローも quit に到達することを確認。
        let mut app = App::new();
        let _ = app.on_quit_key(true, false);
        let d = app.on_quit_key(false, true);
        assert_eq!(d, QuitDecision::Quit);
    }

    #[test]
    fn on_quit_key_pending_then_other_key_cancels() {
        let mut app = App::new();
        let _ = app.on_quit_key(true, false);
        let d = app.on_quit_key(false, false);
        assert_eq!(d, QuitDecision::Cancel);
        assert!(!app.quit_pending, "cancel should clear pending flag");
        assert!(app
            .concierge
            .iter()
            .any(|s| s.contains("quit canceled")));
    }

    #[test]
    fn on_quit_key_idle_pass_through() {
        // pending 状態でない時に通常キーが来たら何もしない (= Pass で main loop の通常処理へ)。
        let mut app = App::new();
        let d = app.on_quit_key(false, false);
        assert_eq!(d, QuitDecision::Pass);
        assert!(!app.quit_pending);
    }

    #[test]
    fn on_quit_key_cancel_then_quit_requires_two_presses_again() {
        // 1 回 cancel した後に Esc を押しても即終了せず、また 1 回目扱い (= 1 段確認に戻る)。
        let mut app = App::new();
        let _ = app.on_quit_key(true, false); // pending
        let _ = app.on_quit_key(false, false); // cancel
        let d = app.on_quit_key(true, false); // また Esc
        assert_eq!(d, QuitDecision::Pending, "after cancel, Esc should re-arm pending, not quit");
    }

    /// 入力行とカーソル位置が整合するかの検証ヘルパ。`f.area()` ではなく `area: Rect` を
    /// 直接渡す形にして、render と cursor_position を同じ Rect で呼ぶ。
    fn render_and_check_input_row(app: &App, area: Rect) -> (u16, u16, ratatui::buffer::Buffer) {
        let mut buf = ratatui::buffer::Buffer::empty(area);
        app.render(area, &mut buf);
        let (cx, cy) = app.cursor_position(area);
        (cx, cy, buf)
    }

    #[test]
    fn input_row_at_screen_bottom_when_history_short() {
        // 履歴量に関わらず、入力行は **画面の最下行** に独立。Concierge pane の中ではない。
        let mut app = App::new();
        app.input_buffer = "abc".into();
        let area = Rect::new(0, 0, 80, 24);
        let (_cx, cy, buf) = render_and_check_input_row(&app, area);
        assert_eq!(cy, area.height - 1, "input must be on the last screen row");
        // col 0 から "> abc" が読めるはず (border 無し)。
        let line: String = (0..5).map(|x| buf[(x, cy)].symbol().to_string()).collect();
        assert_eq!(line, "> abc");
    }

    #[test]
    fn input_row_at_screen_bottom_when_history_full() {
        // 履歴が膨れても、入力行の位置は変わらず最下行。
        let mut app = App::new();
        for i in 0..200 {
            app.on_concierge(format!("line_{i:03}"));
        }
        app.input_buffer = "xyz".into();
        let area = Rect::new(0, 0, 80, 24);
        let (_cx, cy, buf) = render_and_check_input_row(&app, area);
        assert_eq!(cy, area.height - 1);
        let line: String = (0..5).map(|x| buf[(x, cy)].symbol().to_string()).collect();
        assert_eq!(line, "> xyz");
    }

    #[test]
    fn input_row_at_screen_bottom_with_full_width_chars() {
        // 全角入力時も画面最下行に "> あい"。
        let mut app = App::new();
        app.input_buffer = "あい".into();
        let area = Rect::new(0, 0, 80, 24);
        let (_cx, cy, buf) = render_and_check_input_row(&app, area);
        assert_eq!(cy, area.height - 1);
        // col 0 = ">", col 1 = " ", col 2 = "あ", col 4 = "い"
        assert_eq!(buf[(0, cy)].symbol(), ">");
        assert_eq!(buf[(2, cy)].symbol(), "あ");
        assert_eq!(buf[(4, cy)].symbol(), "い");
    }

    #[test]
    fn cursor_y_matches_drawn_input_row() {
        // cursor_position が返す y と、Paragraph で描画された "> " の y が一致することを
        // ピクセル単位で確認する。
        let mut app = App::new();
        app.input_buffer = "abc".into();
        let area = Rect::new(0, 0, 80, 24);
        let (cx, cy, buf) = render_and_check_input_row(&app, area);
        let drawn_y = (0..area.height)
            .find(|&y| buf[(0, y)].symbol() == ">")
            .expect("'>' should be drawn on the input bar");
        assert_eq!(cy, drawn_y);
        // x = "> "(2) + "abc"(3) = 5
        assert_eq!(cx, 5);
    }

    #[test]
    fn input_bar_has_no_border() {
        // 入力欄は border 無し。col 0 が "> " のスペースで埋まり、`│` ボーダー文字が出ない。
        let app = App::new();
        let area = Rect::new(0, 0, 80, 24);
        let (_cx, cy, buf) = render_and_check_input_row(&app, area);
        assert_ne!(buf[(0, cy)].symbol(), "│", "input bar should not start with a border char");
        assert_ne!(buf[(0, cy)].symbol(), "┌", "input bar should not have a border char");
    }

    #[test]
    fn cursor_position_clamps_inside_screen_when_input_overflows() {
        // 入力幅が画面幅を超えても、cursor は画面右端の手前に張り付く。
        // 入力欄は最下行で画面全幅なので、clamp 上限は画面幅 - 1。
        let mut app = App::new();
        let area = Rect::new(0, 0, 80, 24);
        app.input_buffer = "あ".repeat(200); // 400 セル相当
        let (x, _) = app.cursor_position(area);
        assert!(x <= area.width - 1, "cursor x={x} should be clamped under {}", area.width - 1);
    }
}
