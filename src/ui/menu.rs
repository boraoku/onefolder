//! The interactive main menu shown when `onefolder` runs with no arguments.
//!
//! ```text
//! OneFolder
//!
//! ⚙ rclone 1.75.2 · 2 accounts · 3 jobs · last run 2h ago
//!
//! ➤ Jobs        3 jobs · last: documents ✓ 2h ago · 1 drive not mounted
//!   Accounts    2 accounts · all in rclone's config
//!   Status      needs a look: notes
//!   Doctor      checks rclone, accounts and drives · 48 logs, 12.4 MB
//!   Quit
//!
//! ↑↓ j/k move · enter select · ? keys · q quit
//! ```
//!
//! Each item carries a short status, worked out without the network (the
//! doctor's account checks are not run). Enter opens the item; the caller
//! comes back here once its output has been read. `?` shows the keys.
//! Jobs runs jobs too: from its list, one or several one after another.

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::ui::theme::{self, Tone};

/// What the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Jobs,
    Accounts,
    Status,
    Doctor,
    Quit,
}

impl Choice {
    pub const ALL: [Choice; 5] = [
        Choice::Jobs,
        Choice::Accounts,
        Choice::Status,
        Choice::Doctor,
        Choice::Quit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Choice::Jobs => "Jobs",
            Choice::Accounts => "Accounts",
            Choice::Status => "Status",
            Choice::Doctor => "Doctor",
            Choice::Quit => "Quit",
        }
    }
}

/// What the menu says: the line under the title and one short status per
/// item (in [`Choice::ALL`] order; an empty text says nothing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuStatus {
    pub header: String,
    pub items: Vec<(String, Tone)>,
    /// Shorter statuses for an item (same order as `items`; missing or
    /// empty for none), longest first. On a narrow screen the first that
    /// fits is shown; when none does, the last is cut with `…`.
    pub shorter: Vec<Vec<(String, Tone)>>,
}

/// The keys, for the `?` overlay. Shared with the lists the menu opens.
pub const KEYS: [(&str, &str); 11] = [
    ("↑↓  j/k", "move"),
    ("g  G", "first, last"),
    ("enter", "open the item; you come back here after"),
    ("/", "filter the jobs or accounts list"),
    ("a  e  d", "add, edit, remove in a list"),
    ("j  k", "in the Jobs list: move the job down, up"),
    ("t  r", "test, reconnect an account"),
    ("y  n", "answer a question"),
    ("?", "show or hide these keys"),
    ("q  esc", "quit (in a list: go back)"),
    ("ctrl-c", "quit OneFolder from any screen"),
];

/// Menu state: the status, the cursor and whether the keys are shown.
#[derive(Debug, Clone)]
pub struct Menu {
    pub status: MenuStatus,
    pub cursor: usize,
    pub help: bool,
}

impl Menu {
    pub fn new(status: MenuStatus) -> Self {
        Menu {
            status,
            cursor: 0,
            help: false,
        }
    }

    /// Applies one key. Returns the choice once the user has made one.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Choice> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Choice::Quit);
        }
        if self.help {
            // Any key closes the keys; q and Esc do not also quit.
            self.help = false;
            return None;
        }
        let last = Choice::ALL.len() - 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = last,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Enter => return Some(Choice::ALL[self.cursor]),
            KeyCode::Char('q') | KeyCode::Esc => return Some(Choice::Quit),
            _ => {}
        }
        None
    }

    /// The menu's lines; with a `width`, the status line and each item's
    /// status are shortened with `…` to fit.
    pub fn lines(&self, width: Option<usize>) -> Vec<Line<'static>> {
        let fit = |text: &str, room: usize| match width {
            Some(w) => super::fit_text(text, w.saturating_sub(room)),
            None => text.to_string(),
        };
        let mut lines = vec![
            Line::from(Span::styled("OneFolder", theme::style(Tone::Bold))),
            Line::default(),
            Line::from(Span::styled(
                fit(&format!("{} {}", theme::GEAR, self.status.header), 0),
                theme::style(Tone::Dim),
            )),
            Line::default(),
        ];
        let label_w = Choice::ALL
            .iter()
            .map(|c| c.label().len())
            .max()
            .unwrap_or(0)
            + 4;
        for (i, choice) in Choice::ALL.iter().enumerate() {
            let label = format!("{:<label_w$}", choice.label());
            let mut spans = if i == self.cursor {
                vec![
                    Span::styled(theme::CURSOR, theme::style(Tone::Accent)),
                    Span::raw(" "),
                    Span::styled(label, theme::style(Tone::Accent)),
                ]
            } else {
                vec![Span::raw("  "), Span::raw(label)]
            };
            if let Some((text, tone)) =
                self.item_status(i, width.map(|w| w.saturating_sub(2 + label_w)))
            {
                let tone = if tone == Tone::Plain { Tone::Dim } else { tone };
                spans.push(Span::styled(fit(&text, 2 + label_w), theme::style(tone)));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            fit(
                &format!(
                    "{}{} j/k move {dot} enter select {dot} ? keys {dot} q quit",
                    theme::UP,
                    theme::DOWN,
                    dot = theme::DOT
                ),
                0,
            ),
            theme::style(Tone::Dim),
        )));
        lines
    }

    /// Item `i`'s status: the full one, or with a `room` the first of its
    /// shorter ones that fits (the shortest when none does). `None` when
    /// the item says nothing.
    fn item_status(&self, i: usize, room: Option<usize>) -> Option<(String, Tone)> {
        let full = self.status.items.get(i).filter(|(t, _)| !t.is_empty())?;
        let Some(room) = room else {
            return Some(full.clone());
        };
        let shorter = self.status.shorter.get(i).map_or(&[][..], Vec::as_slice);
        let fits = std::iter::once(full)
            .chain(shorter)
            .find(|(t, _)| super::width(t) <= room);
        Some(fits.or(shorter.last()).unwrap_or(full).clone())
    }

    pub fn render(&self, frame: &mut Frame) {
        let area: Rect = frame.area().inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        frame.render_widget(
            Paragraph::new(self.lines(Some(usize::from(area.width)))),
            area,
        );
        if self.help {
            render_keys(frame, "Keys", &KEYS);
        }
    }
}

/// Draws a framed list of keys over the middle of the screen.
pub fn render_keys(frame: &mut Frame, title: &str, keys: &[(&str, &str)]) {
    use ratatui::widgets::{Block, Borders, Clear};
    let key_w = keys.iter().map(|(k, _)| super::width(k)).max().unwrap_or(0) + 3;
    let text_w = keys.iter().map(|(_, t)| super::width(t)).max().unwrap_or(0);
    let full = frame.area();
    let width = u16::try_from(key_w + text_w + 4)
        .unwrap_or(u16::MAX)
        .min(full.width);
    let height = u16::try_from(keys.len() + 4)
        .unwrap_or(u16::MAX)
        .min(full.height);
    let area = Rect {
        x: full.x + (full.width - width) / 2,
        y: full.y + (full.height - height) / 2,
        width,
        height,
    };
    let mut lines: Vec<Line> = keys
        .iter()
        .map(|(k, t)| {
            Line::from(vec![
                Span::styled(super::pad(k, key_w), theme::style(Tone::Accent)),
                Span::raw(t.to_string()),
            ])
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "any key closes this",
        theme::style(Tone::Dim),
    )));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(theme::BORDERS)
                .title(format!(" {title} ")),
        ),
        area,
    );
}

/// Shows the menu with the cursor at `cursor` until the user chooses.
/// Gives back the choice and where the cursor was. The terminal is back
/// to normal when this returns.
pub fn run(status: MenuStatus, cursor: usize) -> Result<(Choice, usize)> {
    let mut terminal = super::screen()?;
    let mut menu = Menu::new(status);
    menu.cursor = cursor.min(Choice::ALL.len() - 1);
    loop {
        terminal
            .draw(|f| menu.render(f))
            .context("could not draw the menu")?;
        if let Event::Key(key) = event::read().context("could not read a key")? {
            if super::is_ctrl_c(&key) {
                super::quit_on_ctrl_c();
            }
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(choice) = menu.handle_key(key) {
                return Ok((choice, menu.cursor));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn status() -> MenuStatus {
        MenuStatus {
            header: "rclone 1.75.2 · 2 accounts · 3 jobs · last run 2h ago".into(),
            items: vec![
                (
                    "3 jobs · last: documents ✓ 2h ago · 1 drive not mounted".into(),
                    Tone::Warning,
                ),
                ("2 accounts · all in rclone's config".into(), Tone::Plain),
                ("needs a look: notes".into(), Tone::Warning),
                (
                    "checks rclone, accounts and drives · 48 logs, 12.4 MB · 3 to prune".into(),
                    Tone::Plain,
                ),
                (String::new(), Tone::Plain),
            ],
            shorter: vec![vec![
                ("3 jobs · 1 drive not mounted".into(), Tone::Warning),
                ("3 jobs".into(), Tone::Plain),
            ]],
        }
    }

    #[test]
    fn arrows_and_vim_keys_move() {
        let mut m = Menu::new(MenuStatus::default());
        assert_eq!(m.handle_key(key(KeyCode::Down)), None);
        assert_eq!(m.handle_key(key(KeyCode::Char('j'))), None);
        assert_eq!(m.cursor, 2);
        m.handle_key(key(KeyCode::Char('k')));
        m.handle_key(key(KeyCode::Up));
        m.handle_key(key(KeyCode::Up));
        assert_eq!(m.cursor, 0);
        for _ in 0..10 {
            m.handle_key(key(KeyCode::Down));
        }
        assert_eq!(m.cursor, Choice::ALL.len() - 1);
        m.handle_key(key(KeyCode::Char('g')));
        assert_eq!(m.cursor, 0);
        m.handle_key(key(KeyCode::Char('G')));
        assert_eq!(m.cursor, 4);
        assert_eq!(Choice::ALL[0], Choice::Jobs, "Jobs is the top item");
        m.cursor = 3;
        assert_eq!(m.handle_key(key(KeyCode::Enter)), Some(Choice::Doctor));
    }

    #[test]
    fn q_esc_and_ctrl_c_quit() {
        let mut m = Menu::new(MenuStatus::default());
        assert_eq!(m.handle_key(key(KeyCode::Char('q'))), Some(Choice::Quit));
        assert_eq!(m.handle_key(key(KeyCode::Esc)), Some(Choice::Quit));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(m.handle_key(ctrl_c), Some(Choice::Quit));
    }

    #[test]
    fn question_mark_shows_the_keys_and_any_key_hides_them() {
        let mut m = Menu::new(status());
        assert_eq!(m.handle_key(key(KeyCode::Char('?'))), None);
        assert!(m.help);
        // Esc closes the keys rather than quitting.
        assert_eq!(m.handle_key(key(KeyCode::Esc)), None);
        assert!(!m.help);
        m.handle_key(key(KeyCode::Char('?')));
        snapshot("menu_keys_80", render(&m, 80));
    }

    #[test]
    fn renders_at_80_columns() {
        let m = Menu::new(status());
        let text = render(&m, 80);
        assert!(!text.contains("Run a job"), "{text}");
        assert!(
            text.contains("➤ Jobs        3 jobs · last: documents ✓ 2h ago · 1 drive not mounted"),
            "{text}"
        );
        snapshot("menu_80", text);
    }

    #[test]
    fn renders_at_40_columns_without_cutting_words() {
        let mut m = Menu::new(status());
        m.cursor = 1;
        let text = render(&m, 40);
        // The Jobs item keeps its count; others are shortened at a part
        // or a word, never inside a word.
        assert!(text.contains("  Jobs        3 jobs  "), "{text}");
        assert!(!text.contains("last:"), "{text}");
        assert!(!text.contains("rclone's co…"), "{text}");
        snapshot("menu_40", text);
    }

    #[test]
    fn the_jobs_item_drops_the_last_run_first_then_the_drives() {
        let m = Menu::new(status());
        let jobs_line = |width: usize| {
            m.lines(Some(width))[4]
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert!(jobs_line(80).ends_with("2h ago · 1 drive not mounted"));
        assert!(jobs_line(50).ends_with("Jobs        3 jobs · 1 drive not mounted"));
        assert!(jobs_line(30).ends_with("Jobs        3 jobs"));
    }

    #[test]
    fn renders_at_120_columns() {
        let mut m = Menu::new(status());
        m.cursor = 1;
        snapshot("menu_120", render(&m, 120));
    }

    fn render(m: &Menu, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
        terminal.draw(|f| m.render(f)).unwrap();
        terminal.backend().to_string()
    }

    fn snapshot(name: &str, value: String) {
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!(name, value);
        });
    }
}
