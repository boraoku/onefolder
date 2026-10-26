//! The accounts screens: the list, the test results, and the small prompts
//! `accounts add`, `remove` and `reconnect` use on a terminal.
//!
//! The output is plain text in Mole's calm style rather than a full-screen
//! interface, because `accounts add` hands the terminal to rclone's own
//! sign-in flow straight after the prompts. The one picker draws inline in
//! raw mode and cleans up after itself. The interactive list behind
//! `onefolder accounts` (and the menu's Accounts item) is a [`Screen`]:
//! the user picks an account and an action, the screen closes, and the
//! action runs on the normal terminal. Inside the interactive session the
//! prompts and the picker draw on the session's screen instead, under the
//! text printed so far (see `ui::session`).

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, queue, terminal};

use crate::jsonout::accounts::{AccountEntry, Health, TestEntry};
use crate::model::DriveType;
use crate::rclone::remotes::Quota;
use crate::ui::summary::format_bytes;
use crate::ui::theme::{self, Tone};

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// How each drive type is described to the user.
pub fn drive_type_words(drive_type: DriveType) -> &'static str {
    match drive_type {
        DriveType::Personal => "OneDrive for home: outlook.com, hotmail.com or live.com",
        DriveType::Business => "OneDrive for work or school: Microsoft 365",
    }
}

/// `338.8 GB used of 1.1 TB · 760.7 GB free`, or as much of it as rclone reported.
pub fn format_quota(q: &Quota) -> Option<String> {
    let used = match (q.used, q.total) {
        (Some(used), Some(total)) => Some(format!(
            "{} used of {}",
            format_bytes(used),
            format_bytes(total)
        )),
        (Some(used), None) => Some(format!("{} used", format_bytes(used))),
        (None, Some(total)) => Some(format!("{} in total", format_bytes(total))),
        (None, None) => None,
    };
    let free = q.free.map(|f| format!("{} free", format_bytes(f)));
    match (used, free) {
        (Some(u), Some(f)) => Some(format!("{u} {} {f}", theme::DOT)),
        (Some(one), None) | (None, Some(one)) => Some(one),
        (None, None) => None,
    }
}

/// `13 Oct 2026` from an RFC 3339 date-time; anything unexpected is shown as it is.
pub fn format_added(added: Option<&str>) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Some(text) = added else {
        return "no date recorded".into();
    };
    let parsed = text.get(0..10).and_then(|date| {
        let mut parts = date.split('-');
        let year: u32 = parts.next()?.parse().ok()?;
        let month: usize = parts.next()?.parse().ok()?;
        let day: u32 = parts.next()?.parse().ok()?;
        let name = MONTHS.get(month.checked_sub(1)?)?;
        Some(format!("{day} {name} {year}"))
    });
    parsed.unwrap_or_else(|| text.to_string())
}

/// Column width for labels: at least 9, plus a gap of 3.
fn label_width<'a>(labels: impl Iterator<Item = &'a str>) -> usize {
    labels.map(|l| l.chars().count()).max().unwrap_or(0).max(9) + 3
}

/// The `accounts list` text.
pub fn render_list(entries: &[AccountEntry], rclone_problem: Option<&str>, colour: bool) -> String {
    let p = |text: &str, tone: Tone| theme::paint(text, tone, colour);
    let mut out = format!(
        "{}  {}\n\n",
        p("Accounts", Tone::Bold),
        p(&plural(entries.len(), "account"), Tone::Dim)
    );
    if entries.is_empty() {
        out.push_str(&format!(
            "  {} none yet {} add one with: onefolder accounts add\n",
            p(theme::SKIPPED, Tone::Dim),
            theme::DOT
        ));
        return out;
    }
    let width = label_width(entries.iter().map(|e| e.label.as_str()));
    let rwidth = entries
        .iter()
        .map(|e| e.remote.chars().count())
        .max()
        .unwrap_or(0)
        + 3;
    for e in entries {
        let mut added = match e.added.as_deref() {
            Some(date) => format!("added {}", format_added(Some(date))),
            None => format_added(None),
        };
        if e.delta {
            added.push_str(&format!(" {} delta listing", theme::DOT));
        }
        out.push_str(&format!(
            "  {:<width$}{:<11}{:<rwidth$}{}",
            e.label,
            e.drive_type.as_str(),
            e.remote,
            p(&added, Tone::Dim)
        ));
        if e.remote_exists == Some(false) {
            out.push_str(&format!(
                "   {}\n      {}\n",
                p(
                    &format!("{} remote missing from rclone's config", theme::FAILED),
                    Tone::Error
                ),
                p(
                    &format!("run: onefolder accounts reconnect {}", e.label),
                    Tone::Dim
                )
            ));
        } else {
            out.push('\n');
        }
    }
    if let Some(problem) = rclone_problem {
        out.push_str(&format!(
            "\n  {} {}\n",
            p(theme::SKIPPED, Tone::Warning),
            p(
                &format!("could not ask rclone which remotes exist: {problem}"),
                Tone::Warning
            )
        ));
    }
    out.push('\n');
    out.push_str(&p(
        &format!(
            "onefolder accounts test {0} add {0} reconnect <label> {0} remove <label>",
            theme::DOT
        ),
        Tone::Dim,
    ));
    out.push('\n');
    out
}

/// What a test result says after the label and drive type.
pub fn health_words(entry: &TestEntry) -> String {
    match entry.status {
        Health::Connected => {
            let mut words = "connected".to_string();
            if let Some(q) = entry.quota.as_ref().and_then(format_quota) {
                words.push_str(&format!(" {} {q}", theme::DOT));
            }
            words
        }
        Health::TokenExpired => "token expired".into(),
        Health::RemoteMissing => "remote missing from rclone's config".into(),
        Health::Timeout => format!(
            "no answer {} {}",
            theme::DOT,
            entry
                .message
                .as_deref()
                .unwrap_or("check your network connection")
        ),
        Health::Failed => format!(
            "not reachable: {}",
            entry.message.as_deref().unwrap_or("rclone gave no reason")
        ),
    }
}

/// The `accounts test` text.
pub fn render_test(entries: &[TestEntry], colour: bool) -> String {
    let p = |text: &str, tone: Tone| theme::paint(text, tone, colour);
    let mut out = format!("{}\n\n", p("Test accounts", Tone::Bold));
    if entries.is_empty() {
        out.push_str(&format!(
            "  {} none yet {} add one with: onefolder accounts add\n",
            p(theme::SKIPPED, Tone::Dim),
            theme::DOT
        ));
        return out;
    }
    let width = label_width(entries.iter().map(|e| e.label.as_str()));
    for e in entries {
        let (glyph, tone) = if e.ok {
            (theme::DONE, Tone::Success)
        } else {
            (theme::FAILED, Tone::Error)
        };
        out.push_str(&format!(
            "  {} {:<width$}{:<11}{}\n",
            p(glyph, tone),
            e.label,
            e.drive_type.as_str(),
            health_words(e)
        ));
        if let Some(hint) = &e.hint {
            out.push_str(&format!(
                "      {}\n",
                p(&format!("run: {hint}"), Tone::Dim)
            ));
        }
    }
    let connected = entries.iter().filter(|e| e.ok).count();
    let tone = if connected == entries.len() {
        Tone::Success
    } else {
        Tone::Error
    };
    out.push('\n');
    out.push_str(&p(
        &format!(
            "{connected} of {} connected",
            plural(entries.len(), "account")
        ),
        tone,
    ));
    out.push('\n');
    out
}

/// Asks for a label until `validate` accepts one. An empty answer or the
/// end of input cancels.
pub fn ask_label(validate: impl Fn(&str) -> Result<(), String>) -> Result<Option<String>> {
    if super::session::is_open() {
        return ask_label_with(
            &mut super::session::LineInput::default(),
            &mut super::session::PageOutput,
            theme::colour_on_stdout(),
            validate,
        );
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    ask_label_with(
        &mut input,
        &mut io::stdout(),
        theme::colour_on_stdout(),
        validate,
    )
}

/// [`ask_label`] over any input and output, for tests.
pub fn ask_label_with(
    input: &mut impl BufRead,
    output: &mut impl Write,
    colour: bool,
    validate: impl Fn(&str) -> Result<(), String>,
) -> Result<Option<String>> {
    let p = |text: &str, tone: Tone| theme::paint(text, tone, colour);
    writeln!(
        output,
        "  {}",
        p(
            "A short name for the account, such as work or personal: lowercase letters, digits and hyphens.",
            Tone::Dim
        )
    )?;
    loop {
        write!(output, "  {} ", p("Label:", Tone::Bold))?;
        output.flush()?;
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .context("could not read the label")?
            == 0
        {
            writeln!(output)?;
            return Ok(None);
        }
        let label = line.trim();
        if label.is_empty() {
            return Ok(None);
        }
        match validate(label) {
            Ok(()) => return Ok(Some(label.to_string())),
            Err(reason) => writeln!(
                output,
                "  {} {}",
                p(theme::FAILED, Tone::Error),
                p(&reason, Tone::Error)
            )?,
        }
    }
}

/// Asks a yes/no question. Only `y` or `yes` (any case) says yes; an empty
/// answer gives `default`.
pub fn confirm(question: &str, default: bool) -> Result<bool> {
    if super::session::is_open() {
        return confirm_with(
            &mut super::session::LineInput::default(),
            &mut super::session::PageOutput,
            question,
            default,
        );
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    confirm_with(&mut input, &mut io::stdout(), question, default)
}

/// [`confirm`] over any input and output, for tests.
pub fn confirm_with(
    input: &mut impl BufRead,
    output: &mut impl Write,
    question: &str,
    default: bool,
) -> Result<bool> {
    let choices = if default { "[Y/n]" } else { "[y/N]" };
    write!(output, "  {question} {choices} ")?;
    output.flush()?;
    let mut line = String::new();
    if input
        .read_line(&mut line)
        .context("could not read the answer")?
        == 0
    {
        writeln!(output)?;
        return Ok(false);
    }
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

/// A short vertical list to choose one item from.
#[derive(Debug, Clone)]
pub struct Picker {
    pub title: String,
    /// Each item: the value shown in the first column, and a description.
    pub items: Vec<(String, String)>,
    /// A row of its own after the items, outside their columns, such as
    /// `Several jobs, one after another…`; chosen as `items.len()`.
    pub extra: Option<String>,
    /// A line under the title saying what the choice is about, set off by
    /// blank lines.
    pub note: Option<String>,
    /// What Esc does, for the keys line: `cancel`, or `back` where Esc goes
    /// back to an earlier list.
    pub esc: &'static str,
    pub cursor: usize,
    /// The terminal's width: every line is fitted to it, so the inline
    /// redraw (which counts lines) never meets a wrapped one.
    pub width: Option<usize>,
}

/// What a key did to the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picked {
    Chosen(usize),
    Cancelled,
}

impl Picker {
    pub fn new(title: &str, items: Vec<(String, String)>) -> Self {
        Picker {
            title: title.to_string(),
            items,
            extra: None,
            note: None,
            esc: "cancel",
            cursor: 0,
            width: None,
        }
    }

    /// Rows to choose from: the items and the extra row.
    fn rows(&self) -> usize {
        self.items.len() + usize::from(self.extra.is_some())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Picked> {
        let last = self.rows().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(last);
                None
            }
            KeyCode::Enter if self.rows() > 0 => Some(Picked::Chosen(self.cursor)),
            KeyCode::Esc | KeyCode::Char('q') => Some(Picked::Cancelled),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Picked::Cancelled)
            }
            _ => None,
        }
    }

    /// The picker's lines as toned segments: the title, the note, the
    /// items with the cursor on one, the extra row, and the keys.
    pub fn segments(&self) -> Vec<Vec<super::Segment>> {
        let width = self
            .items
            .iter()
            .map(|(v, _)| v.chars().count())
            .max()
            .unwrap_or(0)
            + 3;
        // Room for the detail once the indent, cursor and value are taken;
        // a line never reaches the last column, where terminals wrap.
        let room = self.width.map(|w| w.saturating_sub(5 + width));
        let fit = |text: &str, max: Option<usize>| match max {
            Some(m) => super::fit_text(text, m),
            None => text.to_string(),
        };
        let line_max = self.width.map(|w| w.saturating_sub(3));
        let s = |text: &str, tone: Tone| (text.to_string(), tone);
        let mut lines = vec![vec![
            s("  ", Tone::Plain),
            s(&fit(&self.title, line_max), Tone::Bold),
        ]];
        if let Some(note) = &self.note {
            lines.push(Vec::new());
            lines.push(vec![s(
                &fit(
                    &format!("  {note}"),
                    self.width.map(|w| w.saturating_sub(1)),
                ),
                Tone::Plain,
            )]);
            lines.push(Vec::new());
        }
        for (i, (value, detail)) in self.items.iter().enumerate() {
            let padded = fit(&format!("{value:<width$}"), line_max);
            let detail = fit(detail, room);
            if i == self.cursor {
                lines.push(vec![
                    s("  ", Tone::Plain),
                    s(theme::CURSOR, Tone::Accent),
                    s(" ", Tone::Plain),
                    s(&padded, Tone::Accent),
                    s(&detail, Tone::Dim),
                ]);
            } else {
                lines.push(vec![
                    s(&format!("    {padded}"), Tone::Plain),
                    s(&detail, Tone::Dim),
                ]);
            }
        }
        if let Some(extra) = &self.extra {
            let text = fit(extra, line_max);
            if self.cursor == self.items.len() {
                lines.push(vec![
                    s("  ", Tone::Plain),
                    s(theme::CURSOR, Tone::Accent),
                    s(" ", Tone::Plain),
                    s(&text, Tone::Accent),
                ]);
            } else {
                lines.push(vec![s(&format!("    {text}"), Tone::Plain)]);
            }
        }
        if self.note.is_some() {
            lines.push(Vec::new());
        }
        lines.push(vec![
            s("  ", Tone::Plain),
            s(
                &fit(
                    &format!(
                        "{}{} j/k move {dot} enter select {dot} esc {}",
                        theme::UP,
                        theme::DOWN,
                        self.esc,
                        dot = theme::DOT
                    ),
                    line_max,
                ),
                Tone::Dim,
            ),
        ]);
        lines
    }

    /// The picker's lines as text, painted when `colour` is set.
    #[cfg(test)]
    pub fn lines(&self, colour: bool) -> Vec<String> {
        painted(&self.segments(), colour)
    }

    /// The line left in place of the list once item `i` is chosen:
    /// `  Run which job?: documents`, with its newline. None for the extra
    /// row, which leads to another list that leaves its own.
    pub fn answer_line(&self, i: usize, colour: bool) -> String {
        if self.extra.is_some() && i == self.items.len() {
            return String::new();
        }
        let value = self.items.get(i).map(|(v, _)| v.as_str()).unwrap_or("");
        format!(
            "  {} {}\n",
            theme::paint(&format!("{}:", self.title), Tone::Bold, colour),
            value
        )
    }
}

/// Toned lines as text, painted when `colour` is set.
fn painted(lines: &[Vec<super::Segment>], colour: bool) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|(text, tone)| theme::paint(text, *tone, colour))
                .collect()
        })
        .collect()
}

/// A small list drawn inline under the text printed so far, on the shell
/// or under the session's page: the [`Picker`], and the batch's job
/// chooser (`ui::batch::ChooseJobs`).
pub trait Inline {
    /// What choosing gives.
    type Out;
    /// Fits every line to the terminal's width.
    fn set_width(&mut self, width: Option<usize>);
    /// The lines as toned segments.
    fn segments(&self) -> Vec<Vec<super::Segment>>;
    /// Applies one key: `Some(Some(out))` once chosen, `Some(None)` when
    /// cancelled (Esc, q or Ctrl-C), `None` to carry on.
    fn key(&mut self, key: KeyEvent) -> Option<Option<Self::Out>>;
    /// The line left in place of the list once chosen, with its newline.
    fn answer_line(&self, out: &Self::Out, colour: bool) -> String;
}

impl Inline for Picker {
    type Out = usize;
    fn set_width(&mut self, width: Option<usize>) {
        self.width = width;
    }
    fn segments(&self) -> Vec<Vec<super::Segment>> {
        Picker::segments(self)
    }
    fn key(&mut self, key: KeyEvent) -> Option<Option<usize>> {
        self.handle_key(key).map(|p| match p {
            Picked::Chosen(i) => Some(i),
            Picked::Cancelled => None,
        })
    }
    fn answer_line(&self, out: &usize, colour: bool) -> String {
        Picker::answer_line(self, *out, colour)
    }
}

/// Leaves raw mode and shows the cursor again on every exit path.
struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), cursor::Show);
    }
}

/// Shows `picker` inline on the terminal until the user chooses or
/// cancels. Afterwards the list is replaced by one line with the answer.
pub fn pick(picker: Picker) -> Result<Option<usize>> {
    show_inline(picker)
}

/// Shows an [`Inline`] list on the terminal until the user chooses or
/// cancels. Afterwards the list is replaced by one line with the answer.
/// Inside the session it draws under the page instead.
pub fn show_inline<T: Inline>(mut list: T) -> Result<Option<T::Out>> {
    if super::session::is_open() {
        return super::session::inline_on_screen(list);
    }
    let colour = theme::colour_on_stdout();
    list.set_width(terminal::size().ok().map(|(w, _)| usize::from(w)));
    let mut out = io::stdout();
    terminal::enable_raw_mode().context("could not read keys from the terminal")?;
    let _guard = RawGuard;
    queue!(out, cursor::Hide)?;
    let mut drawn = 0u16;
    let result = loop {
        if drawn > 0 {
            queue!(
                out,
                cursor::MoveUp(drawn),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )?;
        }
        let lines = painted(&list.segments(), colour);
        for line in &lines {
            queue!(
                out,
                crossterm::style::Print(line),
                crossterm::style::Print("\r\n")
            )?;
        }
        out.flush()?;
        drawn = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let Event::Key(key) = event::read().context("could not read a key")? else {
            continue;
        };
        if super::is_ctrl_c(&key) {
            // Clears the list first, so the shell prompt starts clean.
            let _ = queue!(
                out,
                cursor::MoveUp(drawn),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            );
            let _ = out.flush();
            drop(_guard);
            super::quit_on_ctrl_c();
        }
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(done) = list.key(key) {
            break done;
        }
    };
    queue!(
        out,
        cursor::MoveUp(drawn),
        cursor::MoveToColumn(0),
        terminal::Clear(terminal::ClearType::FromCursorDown)
    )?;
    match result {
        Some(chosen) => {
            let line = list.answer_line(&chosen, colour);
            queue!(out, crossterm::style::Print(line.replace('\n', "\r\n")))?;
            out.flush()?;
            Ok(Some(chosen))
        }
        None => {
            out.flush()?;
            Ok(None)
        }
    }
}

/// Asks for the drive type with a picker.
pub fn pick_drive_type() -> Result<Option<DriveType>> {
    let options = [DriveType::Personal, DriveType::Business];
    let picker = Picker::new(
        "Drive type",
        options
            .iter()
            .map(|d| (d.as_str().to_string(), drive_type_words(*d).to_string()))
            .collect(),
    );
    Ok(pick(picker)?.and_then(|i| options.get(i).copied()))
}

/// Asks whether to turn on OneDrive delta listing. Off is first and is
/// the default.
pub fn pick_delta() -> Result<Option<bool>> {
    let picker = Picker::new(
        "Delta listing",
        vec![
            (
                "off".into(),
                "the default: lists only the synced folder".into(),
            ),
            (
                "on".into(),
                "lists from the drive root with --fast-list; for folders near the root of a large drive"
                    .into(),
            ),
        ],
    );
    Ok(pick(picker)?.map(|i| i == 1))
}

/// Asks which account to act on.
pub fn pick_account(title: &str, accounts: &[(String, DriveType)]) -> Result<Option<String>> {
    let picker = Picker::new(
        title,
        accounts
            .iter()
            .map(|(label, d)| (label.clone(), d.as_str().to_string()))
            .collect(),
    );
    Ok(pick(picker)?.and_then(|i| accounts.get(i).map(|(l, _)| l.clone())))
}

/// What the user chose on the accounts screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Add,
    Test(String),
    TestAll,
    Reconnect(String),
    /// Already confirmed on the screen.
    Remove(String),
    Back,
}

/// The keys of the accounts list, for its `?` overlay.
pub const KEYS: [(&str, &str); 12] = [
    ("↑↓  j/k", "move"),
    ("g  G", "first, last"),
    ("enter  t", "test the account"),
    ("T", "test every account"),
    ("a", "add an account"),
    ("r", "sign in again (reconnect)"),
    ("d", "remove the account (asks first)"),
    ("/", "filter by label, type or remote"),
    ("?", "show or hide these keys"),
    ("esc", "clear the filter, or go back"),
    ("q", "go back"),
    ("ctrl-c", "quit OneFolder"),
];

/// The interactive accounts list.
#[derive(Debug, Clone)]
pub struct Screen {
    pub entries: Vec<AccountEntry>,
    pub cursor: usize,
    /// Label waiting for a y/n to remove.
    pub confirm: Option<String>,
    pub message: Option<(String, Tone)>,
    pub filter: String,
    pub filtering: bool,
    pub help: bool,
}

impl Screen {
    pub fn new(entries: Vec<AccountEntry>) -> Screen {
        Screen {
            entries,
            cursor: 0,
            confirm: None,
            message: None,
            filter: String::new(),
            filtering: false,
            help: false,
        }
    }

    /// Indexes of the accounts the filter lets through.
    pub fn visible(&self) -> Vec<usize> {
        use super::jobs::matches_filter;
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches_filter(&e.label, &self.filter)
                    || matches_filter(e.drive_type.as_str(), &self.filter)
                    || matches_filter(&e.remote, &self.filter)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn selected(&self) -> Option<String> {
        self.visible()
            .get(self.cursor)
            .and_then(|i| self.entries.get(*i))
            .map(|e| e.label.clone())
    }

    /// Replaces the accounts, keeping the cursor on `select` (or the same
    /// label) when it is still there.
    pub fn set_entries(&mut self, entries: Vec<AccountEntry>, select: Option<&str>) {
        let keep = select.map(str::to_string).or_else(|| self.selected());
        self.entries = entries;
        let visible = self.visible();
        self.cursor = keep
            .and_then(|l| {
                visible
                    .iter()
                    .position(|i| self.entries.get(*i).is_some_and(|e| e.label == l))
            })
            .unwrap_or(self.cursor)
            .min(visible.len().saturating_sub(1));
        self.confirm = None;
    }

    /// Applies one key. Returns an action once the user has chosen one.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Action::Back);
        }
        if self.help {
            self.help = false;
            return None;
        }
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.cursor = 0;
            return None;
        }
        if let Some(label) = self.confirm.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                return Some(Action::Remove(label));
            }
            self.message = Some(("Kept it.".to_string(), Tone::Dim));
            return None;
        }
        let last = self.visible().len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = last,
            KeyCode::Char('/') => {
                self.filtering = true;
                self.message = None;
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('a') => return Some(Action::Add),
            KeyCode::Char('T') if !self.entries.is_empty() => return Some(Action::TestAll),
            KeyCode::Enter | KeyCode::Char('t') => return self.selected().map(Action::Test),
            KeyCode::Char('r') => return self.selected().map(Action::Reconnect),
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(label) = self.selected() {
                    self.message = Some((
                        format!(
                            "Remove {label} and its rclone remote? Job destinations that use it \
                             are removed too; files stay where they are. y remove {} any other \
                             key keeps it",
                            theme::DOT
                        ),
                        Tone::Warning,
                    ));
                    self.confirm = Some(label);
                }
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.cursor = 0;
            }
            KeyCode::Char('q') | KeyCode::Esc => return Some(Action::Back),
            _ => {}
        }
        None
    }

    pub fn lines(&self) -> Vec<ratatui::text::Line<'static>> {
        use ratatui::text::{Line, Span};
        let mut head = vec![
            Span::styled("Accounts", theme::style(Tone::Bold)),
            Span::styled(
                format!("  {}", plural(self.entries.len(), "account")),
                theme::style(Tone::Dim),
            ),
        ];
        if self.filtering || !self.filter.is_empty() {
            let cursor = if self.filtering { "▏" } else { "" };
            head.push(Span::styled(
                format!(" {} /{}{cursor}", theme::DOT, self.filter),
                theme::style(Tone::Accent),
            ));
        }
        let mut lines = vec![Line::from(head), Line::default()];
        if self.entries.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  {} none yet {} a adds one", theme::SKIPPED, theme::DOT),
                theme::style(Tone::Dim),
            )));
            return lines;
        }
        let visible = self.visible();
        if visible.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} no account matches {} esc clears the filter",
                    theme::SKIPPED,
                    theme::DOT
                ),
                theme::style(Tone::Dim),
            )));
            return lines;
        }
        let width = label_width(self.entries.iter().map(|e| e.label.as_str()));
        let rwidth = self
            .entries
            .iter()
            .map(|e| e.remote.chars().count())
            .max()
            .unwrap_or(0)
            + 3;
        for (row, i) in visible.iter().enumerate() {
            let Some(e) = self.entries.get(*i) else {
                continue;
            };
            let selected = row == self.cursor;
            let mut spans = if selected {
                vec![
                    Span::styled(theme::CURSOR, theme::style(Tone::Accent)),
                    Span::raw(" "),
                    Span::styled(format!("{:<width$}", e.label), theme::style(Tone::Accent)),
                ]
            } else {
                vec![
                    Span::raw("  "),
                    Span::styled(format!("{:<width$}", e.label), theme::style(Tone::Bold)),
                ]
            };
            spans.push(Span::raw(format!(
                "{:<11}{:<rwidth$}",
                e.drive_type.as_str(),
                e.remote
            )));
            if e.remote_exists == Some(false) {
                spans.push(Span::styled(
                    format!("{} remote missing, press r", theme::FAILED),
                    theme::style(Tone::Error),
                ));
            } else {
                let mut detail = match e.added.as_deref() {
                    Some(date) => format!("added {}", format_added(Some(date))),
                    None => format_added(None),
                };
                if e.delta {
                    detail.push_str(&format!(" {} delta", theme::DOT));
                }
                spans.push(Span::styled(detail, theme::style(Tone::Dim)));
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    fn hints(&self) -> String {
        let d = theme::DOT;
        if self.filtering {
            format!("type to filter {d} enter keep {d} esc clear")
        } else if self.entries.is_empty() {
            format!("a add {d} ? keys {d} q back")
        } else {
            format!(
                "enter test {d} a add {d} r reconnect {d} d remove {d} / filter {d} ? keys {d} q back"
            )
        }
    }

    pub fn render(&self, frame: &mut ratatui::Frame) {
        use ratatui::layout::{Margin, Rect};
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Paragraph, Wrap};
        let area: Rect = frame.area().inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        if area.height < 4 {
            return;
        }
        let message_height = self
            .message
            .as_ref()
            .map(|(m, _)| {
                let w = usize::from(area.width.max(1));
                u16::try_from(super::width(m).div_ceil(w)).unwrap_or(1) + 1
            })
            .unwrap_or(0)
            .min(area.height.saturating_sub(3));
        let body_height = area.height.saturating_sub(2 + message_height);
        let scroll =
            (u16::try_from(self.cursor).unwrap_or(u16::MAX) + 3).saturating_sub(body_height);
        frame.render_widget(
            Paragraph::new(self.lines()).scroll((scroll, 0)),
            Rect {
                height: body_height,
                ..area
            },
        );
        if let Some((message, tone)) = &self.message {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    message.clone(),
                    theme::style(*tone),
                )))
                .wrap(Wrap { trim: true }),
                Rect {
                    y: area.y + area.height - 1 - message_height,
                    height: message_height,
                    ..area
                },
            );
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                self.hints(),
                theme::style(Tone::Dim),
            ))),
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
        );
        if self.help {
            super::menu::render_keys(frame, "Accounts keys", &KEYS);
        }
    }
}

/// Shows the accounts screen until the user chooses an action. The
/// terminal is back to normal when this returns.
pub fn run_screen(screen: &mut Screen) -> Result<Action> {
    let mut terminal = super::screen()?;
    loop {
        terminal
            .draw(|f| screen.render(f))
            .context("could not draw the accounts list")?;
        if let Event::Key(key) = event::read().context("could not read a key")? {
            if super::is_ctrl_c(&key) {
                super::quit_on_ctrl_c();
            }
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(action) = screen.handle_key(key) {
                return Ok(action);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! snapshot {
        ($name:expr, $value:expr) => {
            insta::with_settings!({
                snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
                prepend_module_to_snapshot => false,
            }, {
                insta::assert_snapshot!($name, $value);
            });
        };
    }

    fn entry(label: &str, drive_type: DriveType, exists: Option<bool>) -> AccountEntry {
        AccountEntry {
            label: label.into(),
            remote: format!("onefolder-{label}"),
            drive_type,
            delta: false,
            added: Some("2026-10-13T09:40:00+11:00".into()),
            remote_exists: exists,
        }
    }

    fn quota() -> Quota {
        Quota {
            total: Some(1_099_511_627_776),
            used: Some(338_812_420_413),
            free: Some(760_694_187_747),
            trashed: None,
        }
    }

    #[test]
    fn list_text() {
        let mut home = entry("home", DriveType::Personal, Some(true));
        home.delta = true;
        home.added = None;
        let entries = vec![
            entry("work", DriveType::Business, Some(true)),
            home,
            entry("old-laptop", DriveType::Personal, Some(false)),
        ];
        snapshot!("accounts_list", render_list(&entries, None, false));
        snapshot!("accounts_list_empty", render_list(&[], None, false));
        let text = render_list(&entries[..1], Some("rclone listremotes timed out"), false);
        assert!(text.contains("could not ask rclone which remotes exist"));
    }

    #[test]
    fn test_text() {
        let ok = TestEntry {
            label: "work".into(),
            remote: "onefolder-work".into(),
            drive_type: DriveType::Business,
            ok: true,
            status: Health::Connected,
            quota: Some(quota()),
            message: None,
            hint: None,
        };
        let expired = TestEntry {
            label: "personal".into(),
            remote: "onefolder-personal".into(),
            drive_type: DriveType::Personal,
            ok: false,
            status: Health::TokenExpired,
            quota: None,
            message: Some("couldn't fetch token".into()),
            hint: Some("onefolder accounts reconnect personal".into()),
        };
        let failed = TestEntry {
            label: "shared".into(),
            remote: "onefolder-shared".into(),
            drive_type: DriveType::Business,
            ok: false,
            status: Health::Failed,
            quota: None,
            message: Some("about call failed: directory not found".into()),
            hint: None,
        };
        snapshot!(
            "accounts_test",
            render_test(&[ok.clone(), expired, failed], false)
        );
        assert!(render_test(&[ok], false).ends_with("1 of 1 account connected\n"));
    }

    #[test]
    fn quota_and_dates() {
        assert_eq!(
            format_quota(&quota()).as_deref(),
            Some("338.8 GB used of 1.1 TB · 760.7 GB free")
        );
        assert_eq!(
            format_quota(&Quota {
                free: Some(2_000_000),
                ..Quota::default()
            })
            .as_deref(),
            Some("2.0 MB free")
        );
        assert_eq!(format_quota(&Quota::default()), None);
        assert_eq!(
            format_added(Some("2026-10-13T09:40:00+11:00")),
            "13 Oct 2026"
        );
        assert_eq!(format_added(Some("2026-01-02T00:00:00Z")), "2 Jan 2026");
        assert_eq!(format_added(Some("yesterday")), "yesterday");
        assert_eq!(format_added(None), "no date recorded");
    }

    #[test]
    fn label_prompt_retries_until_valid() {
        let mut input = io::Cursor::new("Work\nwork\n");
        let mut output = Vec::new();
        let got = ask_label_with(&mut input, &mut output, false, |l| {
            if l == "work" {
                Ok(())
            } else {
                Err("labels are lowercase".into())
            }
        })
        .unwrap();
        assert_eq!(got.as_deref(), Some("work"));
        let shown = String::from_utf8(output).unwrap();
        assert!(shown.contains("  ✗ labels are lowercase\n"), "{shown}");
        assert_eq!(shown.matches("Label:").count(), 2);

        let mut empty = io::Cursor::new("\n");
        assert_eq!(
            ask_label_with(&mut empty, &mut Vec::new(), false, |_| Ok(())).unwrap(),
            None
        );
        let mut eof = io::Cursor::new("");
        assert_eq!(
            ask_label_with(&mut eof, &mut Vec::new(), false, |_| Ok(())).unwrap(),
            None
        );
    }

    #[test]
    fn confirm_answers() {
        let ask = |text: &str, default: bool| {
            confirm_with(&mut io::Cursor::new(text), &mut Vec::new(), "Go?", default).unwrap()
        };
        assert!(ask("y\n", false));
        assert!(ask("YES\n", false));
        assert!(!ask("\n", false));
        assert!(ask("\n", true));
        assert!(!ask("nope\n", true));
        assert!(!ask("", true));
    }

    #[test]
    fn picker_keys_and_lines() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut picker = Picker::new(
            "Drive type",
            vec![
                (
                    "personal".into(),
                    drive_type_words(DriveType::Personal).into(),
                ),
                (
                    "business".into(),
                    drive_type_words(DriveType::Business).into(),
                ),
            ],
        );
        assert_eq!(picker.handle_key(key(KeyCode::Char('j'))), None);
        assert_eq!(picker.handle_key(key(KeyCode::Down)), None);
        assert_eq!(picker.cursor, 1);
        snapshot!("accounts_drive_type_picker", picker.lines(false).join("\n"));
        // Fitted to a narrow terminal, so the inline redraw counts right.
        picker.width = Some(40);
        for line in picker.lines(false) {
            assert!(crate::ui::width(&line) < 40, "{line}");
        }
        picker.width = None;
        assert_eq!(picker.handle_key(key(KeyCode::Up)), None);
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            Some(Picked::Chosen(0))
        );
        assert_eq!(
            picker.handle_key(key(KeyCode::Esc)),
            Some(Picked::Cancelled)
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(picker.handle_key(ctrl_c), Some(Picked::Cancelled));
    }

    #[test]
    fn the_accounts_screen_filters_and_acts() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut home = entry("home", DriveType::Personal, Some(true));
        home.delta = true;
        let mut s = Screen::new(vec![
            entry("work", DriveType::Business, Some(true)),
            home,
            entry("old-laptop", DriveType::Personal, Some(false)),
        ]);
        let render = |s: &Screen, width| {
            let mut t = Terminal::new(TestBackend::new(width, 14)).unwrap();
            t.draw(|f| s.render(f)).unwrap();
            t.backend().to_string()
        };
        snapshot!("accounts_screen_80", render(&s, 80));
        snapshot!("accounts_screen_120", render(&s, 120));
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Action::Test("work".into()))
        );
        s.handle_key(key(KeyCode::Char('/')));
        for c in "personal".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        s.handle_key(key(KeyCode::Enter));
        assert_eq!(s.visible(), vec![1, 2]);
        s.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            s.handle_key(key(KeyCode::Char('r'))),
            Some(Action::Reconnect("old-laptop".into()))
        );
        // Remove asks first.
        assert_eq!(s.handle_key(key(KeyCode::Char('d'))), None);
        assert_eq!(
            s.handle_key(key(KeyCode::Char('y'))),
            Some(Action::Remove("old-laptop".into()))
        );
        s.handle_key(key(KeyCode::Char('d')));
        assert_eq!(s.handle_key(key(KeyCode::Char('n'))), None);
        assert_eq!(s.handle_key(key(KeyCode::Char('T'))), Some(Action::TestAll));
        assert_eq!(s.handle_key(key(KeyCode::Char('a'))), Some(Action::Add));
        // Esc clears the filter, then goes back.
        assert_eq!(s.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(s.visible().len(), 3);
        s.handle_key(key(KeyCode::Char('?')));
        assert!(s.help);
        assert_eq!(s.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(s.handle_key(key(KeyCode::Esc)), Some(Action::Back));
        // Keeps the selection by label.
        s.cursor = 2;
        let mut fewer = s.entries.clone();
        fewer.remove(0);
        s.set_entries(fewer, None);
        assert_eq!(s.selected().as_deref(), Some("old-laptop"));
    }

    #[test]
    fn the_run_picker_ends_with_several_jobs() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut picker = Picker::new(
            "Run which job?",
            [
                ("documents", "~/Documents"),
                ("engineering", "~/Engineering"),
                ("records", "~/Records"),
                ("photos", "~/Pictures"),
            ]
            .iter()
            .map(|(n, s)| (n.to_string(), s.to_string()))
            .collect(),
        );
        picker.extra = Some(crate::ui::batch::SEVERAL.into());
        picker.esc = "back";
        for _ in 0..10 {
            picker.handle_key(key(KeyCode::Down));
        }
        assert_eq!(picker.cursor, 4, "the last row is the extra one");
        snapshot!("run_picker_several", picker.lines(false).join("\n"));
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            Some(Picked::Chosen(4))
        );
        // It leads to another list, so it leaves no answer line.
        assert_eq!(picker.answer_line(4, false), "");
        assert_eq!(
            picker.answer_line(1, false),
            "  Run which job?: engineering\n"
        );
        picker.width = Some(30);
        for line in picker.lines(false) {
            assert!(crate::ui::width(&line) < 30, "{line}");
        }
    }
}
