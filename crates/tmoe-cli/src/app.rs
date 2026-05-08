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
        // unicode-width で half-width / full-width / 結合文字を正しく扱う。
        // 0x1100 比較の旧実装は半角カタカナを 2 列と数えてしまい、IME pre-edit が
        // 右の Trio ペインのボーダーを上書きしていた。
        let input_visible_width: u16 = self
            .input_buffer
            .chars()
            .map(|c| c.width().unwrap_or(0) as u16)
            .sum();
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

        // Concierge ペイン: 上 N-2 行が履歴、下 2 行は固定で "> " + input_buffer。
        // 履歴が pane を埋めない時は **空行で padding** することで、input 行を必ず
        // pane の内寸最終 row に固定する。これで `cursor_position` が返す y
        // (= pane.y + height - 2) と input 行が常に一致し、macOS Terminal の IME
        // pre-edit が「履歴の上には > プロンプトだけ、ずっと下に未確定文字」のように
        // 分離して描かれる症状を防ぐ。
        let conc_keep = conc_n.saturating_sub(2);
        let conc_visible = App::tail(&self.concierge, conc_keep);
        let mut conc_lines: Vec<Line> = conc_visible.iter().map(|s| Line::from(s.as_str())).collect();
        while conc_lines.len() < conc_keep {
            conc_lines.push(Line::from(""));
        }
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
        // ASCII 2 文字 = pane.x + 1 + 2 = 3
        // 全角 2 文字 = pane.x + 1 + 4 = 5
        assert_eq!(x_full.saturating_sub(x_ascii), 2);

        app.input_buffer = "ｱｲ".into();
        let (x_half_kana, _) = app.cursor_position(area);
        // 半角カナ 2 文字は ASCII 2 文字と同じ 2 セル分。旧実装ならここが 4 になる。
        assert_eq!(x_half_kana, x_ascii);
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
    fn input_row_at_pane_bottom_when_history_short() {
        // 履歴が pane を埋めない時でも、入力行は pane 内寸の最終 row に固定されているはず。
        // = cursor_position が指す y 行に input_buffer の文字が描画されている。
        let mut app = App::new();
        app.input_buffer = "abc".into();
        let area = Rect::new(0, 0, 80, 24);
        let (_cx, cy, buf) = render_and_check_input_row(&app, area);
        // cy 行の左 pane 内側 (col 1) から "abc" が読めるはず。
        // ratatui の Buffer は 1 セル 1 文字 (wide char は 2 セルだがここは ASCII)。
        let line: String = (1..4).map(|x| buf[(x, cy)].symbol().to_string()).collect();
        assert_eq!(line, "abc", "input row should be at cursor y; got dump '{line}'");
    }

    #[test]
    fn input_row_at_pane_bottom_when_history_full() {
        // 履歴が pane を超えても、入力行は pane 内寸の最終 row に固定されているはず。
        let mut app = App::new();
        for i in 0..200 {
            app.on_concierge(format!("line_{i:03}"));
        }
        app.input_buffer = "xyz".into();
        let area = Rect::new(0, 0, 80, 24);
        let (_cx, cy, buf) = render_and_check_input_row(&app, area);
        let line: String = (1..4).map(|x| buf[(x, cy)].symbol().to_string()).collect();
        assert_eq!(line, "xyz");
    }

    #[test]
    fn input_row_at_pane_bottom_with_full_width_chars() {
        // 全角入力時もカーソル y と描画行が一致する。
        let mut app = App::new();
        app.input_buffer = "あい".into();
        let area = Rect::new(0, 0, 80, 24);
        let (_cx, cy, buf) = render_and_check_input_row(&app, area);
        // ratatui は wide char を 1 つの cell に格納し、次の cell は空文字 (placeholder)。
        // col 1 に "あ", col 3 に "い"。
        assert_eq!(buf[(1, cy)].symbol(), "あ");
        assert_eq!(buf[(3, cy)].symbol(), "い");
    }

    #[test]
    fn cursor_position_clamps_inside_pane_when_input_overflows() {
        // 入力幅がペイン幅を超えても、cursor は右端の手前に張り付く (= pre-edit が
        // 右ペインのボーダーに食い込まない最後の防壁)。
        let mut app = App::new();
        let area = Rect::new(0, 0, 80, 24);
        app.input_buffer = "あ".repeat(200); // 400 セル相当
        let (x, _) = app.cursor_position(area);
        // 左 pane は area の 45% なので width ≈ 36。x は pane.x + (width - 2) 以下に収まる。
        let pane_right_max = 36u16; // 80 * 0.45 - 2 ≈ 34, 余裕で含む上限
        assert!(x <= pane_right_max, "cursor x={x} should be clamped under {pane_right_max}");
    }
}
