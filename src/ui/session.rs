//! The interactive session's terminal: `onefolder` with no arguments takes
//! raw mode and the alternate screen once, when the menu opens, and gives
//! them back once, when the menu is left (q, Esc, Ctrl-C, an error, a
//! signal). Every screen inside the session draws into that one terminal,
//! so the shell's earlier output never shows between two screens.
//!
//! - [`screen`] is how every screen gets its terminal: the session's when
//!   one is open, otherwise its own (as `ratatui::try_init` gives it),
//!   restored when the [`Screen`] guard drops. Direct subcommands
//!   (`onefolder run documents`, `onefolder jobs edit x`) work as before.
//! - Text a command prints inside the session ([`super::out`] and
//!   [`super::err`]) is kept on a [`Page`] rather than written to the
//!   shell. The page keeps the tones the painted text carries (the codes
//!   [`theme::paint`] writes are read back by [`theme::unpaint`]), so the
//!   words and colours are exactly those of the plain output.
//!   [`show_page`] shows it in a [`TextScreen`] to scroll, and any other
//!   key goes back, as "Press any key" did on the normal screen.
//! - The small inline prompts (`pick`, the label and yes/no questions)
//!   draw below the page on the session's terminal ([`pick_on_screen`],
//!   [`LineInput`]), with the page as the text above them.
//! - The summary blocks of every run are also kept in a transcript, which
//!   is printed on the normal screen when the session ends, so the
//!   shell's scrollback has the results and their Log: paths.

use std::io::{self, BufRead, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use super::accounts::{Picked, Picker};
use super::theme::{self, Tone};
use super::Segment;

/// True while the session is open.
static OPEN: AtomicBool = AtomicBool::new(false);
/// The session's terminal while no screen is using it.
static TERMINAL: Mutex<Option<DefaultTerminal>> = Mutex::new(None);
/// Set when something other than ratatui wrote to the screen (rclone's
/// sign-in): the next screen clears it before drawing.
static STALE: AtomicBool = AtomicBool::new(false);
/// What the session would have printed since the page was last shown.
static PAGE: Mutex<Page> = Mutex::new(Page::new());
/// The summary blocks of the session's runs, printed when it ends.
static TRANSCRIPT: Mutex<String> = Mutex::new(String::new());

/// True while raw mode and the alternate screen are on, whoever turned
/// them on: so the terminal is put back exactly once.
static UP: AtomicBool = AtomicBool::new(false);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// True while the interactive session is open.
pub fn is_open() -> bool {
    OPEN.load(Ordering::SeqCst)
}

/// The open session; ends it when dropped.
#[must_use = "the session ends when this is dropped"]
pub struct Session(());

/// Takes raw mode and the alternate screen for the whole session. The
/// terminal is restored, and the transcript printed, when the returned
/// [`Session`] drops, on Ctrl-C ([`super::quit_on_ctrl_c`]) and before a
/// signal (SIGINT, SIGTERM, SIGHUP) ends OneFolder: the signal dispatcher
/// is started here, so it runs the exit hook even before the first run.
pub fn start() -> Result<Session> {
    let terminal = ratatui::try_init().context("could not start the terminal interface")?;
    UP.store(true, Ordering::SeqCst);
    *lock(&TERMINAL) = Some(terminal);
    *lock(&PAGE) = Page::new();
    lock(&TRANSCRIPT).clear();
    OPEN.store(true, Ordering::SeqCst);
    crate::sync::reporter::set_exit_hook(Some(super::restore_terminal));
    crate::sync::reporter::install_signal_handler();
    Ok(Session(()))
}

impl Drop for Session {
    fn drop(&mut self) {
        end();
    }
}

/// Ends the session, once: puts the terminal back and prints the
/// transcript, followed by nothing else.
pub fn end() {
    if !OPEN.swap(false, Ordering::SeqCst) {
        return;
    }
    crate::sync::reporter::set_exit_hook(None);
    // Dropping the terminal shows the cursor it hid.
    drop(lock(&TERMINAL).take());
    leave();
    let transcript = std::mem::take(&mut *lock(&TRANSCRIPT));
    if !transcript.is_empty() {
        let _ = crate::jsonout::write_stdout(transcript.as_bytes());
    }
}

/// Leaves raw mode and the alternate screen if they are on.
pub(super) fn leave() {
    if UP.swap(false, Ordering::SeqCst) {
        ratatui::restore();
        let _ = crossterm::execute!(io::stdout(), crossterm::cursor::Show);
    }
}

// ------------------------------------------------------------- screen

/// Where a screen's terminal comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The session's terminal, given back when the screen closes.
    Session,
    /// A session is open but another screen has its terminal.
    Busy,
    /// No session: the screen takes the terminal itself and restores it.
    Own,
}

/// Decides where a screen's terminal comes from.
pub fn source(open: bool, free: bool) -> Source {
    match (open, free) {
        (true, true) => Source::Session,
        (true, false) => Source::Busy,
        (false, _) => Source::Own,
    }
}

/// What a screen gets from the session's slot: `Ok(Some)` the session's
/// terminal (the slot is left empty until it is given back), `Ok(None)`
/// no session, so the screen takes its own, and an error when another
/// screen has it, which is a bug: two screens never draw at once.
fn borrow<T>(open: bool, slot: &mut Option<T>) -> Result<Option<T>> {
    match source(open, slot.is_some()) {
        Source::Session => Ok(slot.take()),
        Source::Busy => Err(anyhow!(
            "another screen is already using the terminal (OneFolder bug: one screen opened inside another)"
        )),
        Source::Own => Ok(None),
    }
}

/// Gives a borrowed terminal back to an empty slot. A slot that is not
/// empty means the terminal was given back twice; the first one is kept.
fn give_back<T>(slot: &mut Option<T>, terminal: Option<T>) {
    debug_assert!(
        slot.is_none(),
        "the session's terminal was given back twice"
    );
    if slot.is_none() {
        *slot = terminal;
    }
}

/// A screen's hold on the terminal. Derefs to the ratatui terminal. On
/// drop, the session's terminal goes back to the session; a terminal of
/// its own is restored.
pub struct Screen {
    terminal: Option<DefaultTerminal>,
    own: bool,
}

/// The terminal for a screen: the session's, or one of its own.
pub fn screen() -> Result<Screen> {
    let borrowed = borrow(is_open(), &mut lock(&TERMINAL))?;
    match borrowed {
        Some(terminal) => {
            let mut screen = Screen {
                terminal: Some(terminal),
                own: false,
            };
            if STALE.swap(false, Ordering::SeqCst) {
                // On failure the guard gives the terminal back.
                screen.clear().context("could not clear the screen")?;
            }
            Ok(screen)
        }
        None => {
            let terminal = ratatui::try_init().context("could not start the terminal interface")?;
            UP.store(true, Ordering::SeqCst);
            // A signal that ends OneFolder while this screen is up (one
            // sent from outside) restores the terminal first.
            crate::sync::reporter::set_exit_hook(Some(super::restore_terminal));
            crate::sync::reporter::install_signal_handler();
            Ok(Screen {
                terminal: Some(terminal),
                own: true,
            })
        }
    }
}

impl std::ops::Deref for Screen {
    type Target = DefaultTerminal;
    fn deref(&self) -> &DefaultTerminal {
        self.terminal
            .as_ref()
            .expect("the screen holds its terminal")
    }
}

impl std::ops::DerefMut for Screen {
    fn deref_mut(&mut self) -> &mut DefaultTerminal {
        self.terminal
            .as_mut()
            .expect("the screen holds its terminal")
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let terminal = self.terminal.take();
        if self.own {
            crate::sync::reporter::set_exit_hook(None);
            drop(terminal);
            leave();
        } else if is_open() {
            give_back(&mut lock(&TERMINAL), terminal);
        }
    }
}

// --------------------------------------------------------------- page

/// Text printed inside the session, as lines of toned segments. The last
/// line is the one still being written (a prompt waiting for its answer),
/// and is empty after a newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    lines: Vec<Vec<Segment>>,
    /// The tone in force at the end of the text so far.
    tone: Tone,
}

impl Default for Page {
    fn default() -> Self {
        Page::new()
    }
}

impl Page {
    pub const fn new() -> Page {
        Page {
            lines: Vec::new(),
            tone: Tone::Plain,
        }
    }

    /// Adds text as it would have been printed: painted, with newlines.
    pub fn add(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n");
        let mut pieces = text.split('\n');
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        if let Some(first) = pieces.next() {
            let segments = theme::unpaint(first, &mut self.tone);
            if let Some(last) = self.lines.last_mut() {
                join(last, segments);
            }
        }
        for piece in pieces {
            let mut line = Vec::new();
            join(&mut line, theme::unpaint(piece, &mut self.tone));
            self.lines.push(line);
        }
    }

    /// The lines, without the empty line a final newline leaves.
    pub fn lines(&self) -> Vec<Vec<Segment>> {
        let mut lines = self.lines.clone();
        if lines
            .last()
            .is_some_and(|l| l.iter().all(|(t, _)| t.is_empty()))
        {
            lines.pop();
        }
        lines
    }

    /// The line still being written, such as `  Label: `.
    fn open_line(&self) -> Vec<Segment> {
        self.lines.last().cloned().unwrap_or_default()
    }

    /// The lines before the one still being written.
    fn closed_lines(&self) -> Vec<Vec<Segment>> {
        let n = self.lines.len().saturating_sub(1);
        self.lines[..n].to_vec()
    }
}

/// Appends segments to a line, joining a run of one tone.
fn join(line: &mut Vec<Segment>, segments: Vec<Segment>) {
    for (text, tone) in segments {
        if text.is_empty() {
            continue;
        }
        match line.last_mut() {
            Some((last, t)) if *t == tone => last.push_str(&text),
            _ => line.push((text, tone)),
        }
    }
}

/// Adds printed text to the session's page.
pub fn add(text: &str) {
    lock(&PAGE).add(text);
}

/// Adds a run's summary block to the transcript printed when the session
/// ends.
pub fn record(text: &str) {
    if is_open() {
        lock(&TRANSCRIPT).push_str(text);
    }
}

/// Shows what the session printed since the page was last shown, in a
/// [`TextScreen`], until a key other than a scroll key. Does nothing
/// outside a session or when nothing was printed.
pub fn show_page() -> Result<()> {
    if !is_open() {
        return Ok(());
    }
    let lines = std::mem::take(&mut *lock(&PAGE)).lines();
    if lines.is_empty() {
        return Ok(());
    }
    let mut text = TextScreen::new(lines);
    let mut terminal = screen()?;
    // Keys typed while the command worked (a second Enter at the menu)
    // must not close the output before it is seen.
    drain_keys()?;
    loop {
        terminal
            .draw(|f| text.render(f))
            .context("could not draw the output")?;
        if let Event::Key(key) = event::read().context("could not read a key")? {
            if super::is_ctrl_c(&key) {
                super::quit_on_ctrl_c();
            }
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if text.handle_key(key) {
                return Ok(());
            }
        }
    }
}

/// Throws away input that is waiting: keys typed while nothing was
/// listening.
fn drain_keys() -> Result<()> {
    while event::poll(std::time::Duration::ZERO).context("could not read a key")? {
        event::read().context("could not read a key")?;
    }
    Ok(())
}

/// Draws a frame saying `text` while a command works without a screen of
/// its own (Status, Doctor), so the menu does not look frozen. Does
/// nothing outside a session.
pub fn busy(text: &str) -> Result<()> {
    if !is_open() {
        return Ok(());
    }
    let mut terminal = screen()?;
    terminal
        .draw(|f| {
            let area = f.area().inner(Margin {
                horizontal: 2,
                vertical: 1,
            });
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    text.to_string(),
                    theme::style(Tone::Dim),
                ))),
                area,
            );
        })
        .context("could not draw the screen")?;
    Ok(())
}

// --------------------------------------------------------- text screen

/// Output to read, scrolled with the keys of the whole-list view. It
/// starts at the end, where the shell would have left it.
#[derive(Debug, Clone)]
pub struct TextScreen {
    lines: Vec<Vec<Segment>>,
    /// Rows scrolled from the top; past the end means the end.
    scroll: usize,
    /// The rows that fit, and the furthest the text scrolls, as last drawn.
    page: std::cell::Cell<usize>,
    max: std::cell::Cell<usize>,
}

impl TextScreen {
    pub fn new(lines: Vec<Vec<Segment>>) -> Self {
        TextScreen {
            lines,
            scroll: usize::MAX,
            page: std::cell::Cell::new(10),
            max: std::cell::Cell::new(usize::MAX),
        }
    }

    /// Applies one key. True when the key goes back.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let max = self.max.get();
        let at = self.scroll.min(max);
        let page = self.page.get().max(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.scroll = (at + 1).min(max),
            KeyCode::Up | KeyCode::Char('k') => self.scroll = at.saturating_sub(1),
            KeyCode::PageDown => self.scroll = (at + page).min(max),
            KeyCode::PageUp => self.scroll = at.saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = max,
            _ => return true,
        }
        false
    }

    /// The hint line.
    pub fn hints() -> String {
        format!(
            "esc back {d} {}{} pgup/pgdn scroll {d} g/G top/bottom",
            theme::UP,
            theme::DOWN,
            d = theme::DOT
        )
    }

    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.height == 0 {
            return;
        }
        // The text two columns in, as the menu and the lists sit; a blank
        // row and the keys under it.
        let inner = Rect {
            x: area.x + 2.min(area.width),
            width: area.width.saturating_sub(2),
            ..area
        };
        let body = Rect {
            height: area.height.saturating_sub(2).max(1),
            ..inner
        };
        let columns = usize::from(area.width).saturating_sub(2).max(1);
        let rows: Vec<Vec<Segment>> = self
            .lines
            .iter()
            .flat_map(|line| wrap_for_screen(line, columns))
            .collect();
        let height = usize::from(body.height);
        let max = rows.len().saturating_sub(height);
        self.max.set(max);
        self.page.set(height);
        let from = self.scroll.min(max);
        let shown: Vec<Line> = rows
            .into_iter()
            .skip(from)
            .take(height)
            .map(to_line)
            .collect();
        frame.render_widget(Paragraph::new(shown), body);
        if area.height > 2 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    Self::hints(),
                    theme::style(Tone::Dim),
                ))),
                Rect {
                    y: area.y + area.height - 1,
                    height: 1,
                    ..inner
                },
            );
        }
    }
}

/// Fits one line of output into `columns` for the text screen. A line
/// that fits is kept. A rule (`=====`) is cut to the width rather than
/// wrapped. Otherwise the line wraps at spaces, each new row indented four
/// columns past the line's own indent, except that a path (a long word
/// with a slash) or a word too long for any row is never moved or
/// indented: it carries on from where it is and continues from the first
/// column, as a terminal wraps it, so it can be copied in one piece.
pub fn wrap_for_screen(line: &[Segment], columns: usize) -> Vec<Vec<Segment>> {
    let text: String = line.iter().map(|(t, _)| t.as_str()).collect();
    let total = super::width(&text);
    if total <= columns {
        return vec![line.to_vec()];
    }
    let trimmed = text.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| matches!(c, '=' | '─' | '━' | '-')) {
        let mut out = Vec::new();
        let mut used = 0;
        for (t, tone) in line {
            let room = columns.saturating_sub(used);
            let piece = super::front(t, room);
            used += super::width(&piece);
            out.push((piece, *tone));
        }
        return vec![out];
    }
    let own = total - super::width(text.trim_start());
    let indent = (own + 4).min(columns / 2);
    // Words with the spaces before them, each in its tone.
    let mut words: Vec<(String, String, Tone)> = Vec::new();
    let mut lead = String::new();
    for (t, tone) in line {
        let mut word = String::new();
        for c in t.chars() {
            if c == ' ' {
                if !word.is_empty() {
                    words.push((std::mem::take(&mut lead), std::mem::take(&mut word), *tone));
                }
                lead.push(' ');
            } else {
                word.push(c);
            }
        }
        if !word.is_empty() {
            words.push((std::mem::take(&mut lead), word, *tone));
        }
    }
    let mut rows: Vec<Vec<Segment>> = Vec::new();
    let mut row: Vec<Segment> = Vec::new();
    let mut used = 0;
    for (spaces, word, tone) in words {
        let w = super::width(&word);
        let gap = super::width(&spaces);
        // A word too long for any row, or a path (a long word with a
        // slash), carries on from where it is.
        let long = w > columns.saturating_sub(indent) || (w >= 20 && word.contains('/'));
        if !long && used > 0 && used + gap + w > columns {
            rows.push(std::mem::take(&mut row));
            row.push((" ".repeat(indent), Tone::Plain));
            used = indent;
        } else if used + gap < columns || !long {
            if gap > 0 {
                row.push((spaces, Tone::Plain));
                used += gap;
            }
        } else {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        // A word wider than the room left carries on to the next rows,
        // from the first column.
        let mut rest = word;
        while used + super::width(&rest) > columns {
            let head = super::front(&rest, columns.saturating_sub(used).max(1));
            if head.is_empty() {
                break;
            }
            rest = rest[head.len()..].to_string();
            row.push((head, tone));
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        used += super::width(&rest);
        if !rest.is_empty() {
            row.push((rest, tone));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

fn to_line(segments: Vec<Segment>) -> Line<'static> {
    Line::from(
        segments
            .into_iter()
            .map(|(text, tone)| Span::styled(text, theme::style(tone)))
            .collect::<Vec<_>>(),
    )
}

/// Draws the page with `below` under it, keeping the end in view when it
/// does not all fit, as a terminal scrolls. `cursor` puts the terminal's
/// cursor that many columns into the last line.
fn render_console(
    frame: &mut Frame,
    page: Vec<Vec<Segment>>,
    below: Vec<Vec<Segment>>,
    cursor: Option<usize>,
) {
    let area = frame.area();
    let width = usize::from(area.width);
    let mut rows = super::fit_lines(page, Some(width));
    rows.extend(super::fit_lines(below, Some(width)));
    let height = usize::from(area.height);
    let skip = rows.len().saturating_sub(height);
    let shown: Vec<Line> = rows.into_iter().skip(skip).map(to_line).collect();
    let last = shown.len().saturating_sub(1);
    frame.render_widget(Paragraph::new(shown), area);
    if let Some(column) = cursor {
        let x = u16::try_from(column)
            .unwrap_or(u16::MAX)
            .min(area.width.saturating_sub(1));
        let y = u16::try_from(last).unwrap_or(0);
        frame.set_cursor_position((area.x + x, area.y + y));
    }
}

// --------------------------------------------------------------- pick

/// [`super::accounts::pick`] inside the session: the picker drawn under
/// the page with the same items, highlight and keys. The answer line the
/// inline picker leaves goes on the page.
pub fn pick_on_screen(mut picker: Picker) -> Result<Option<usize>> {
    let mut terminal = screen()?;
    let result = loop {
        let page = lock(&PAGE).lines();
        terminal
            .draw(|f| {
                picker.width = Some(usize::from(f.area().width));
                render_console(f, page, picker.segments(), None);
            })
            .context("could not draw the list")?;
        let Event::Key(key) = event::read().context("could not read a key")? else {
            continue;
        };
        if super::is_ctrl_c(&key) {
            super::quit_on_ctrl_c();
        }
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(done) = picker.handle_key(key) {
            break done;
        }
    };
    Ok(match result {
        Picked::Chosen(i) => {
            add(&picker.answer_line(i, theme::colour_on_stdout()));
            Some(i)
        }
        Picked::Cancelled => None,
    })
}

// ---------------------------------------------------------- line input

/// Reads one answer typed under the page, the page's last line (the
/// question) on the left of it. Enter gives the answer, which goes on the
/// page as a terminal would echo it; Esc gives `None`, as the end of
/// input would.
pub fn read_line() -> Result<Option<String>> {
    use tui_input::backend::crossterm::to_input_request;
    let mut input = tui_input::Input::default();
    let mut terminal = screen()?;
    loop {
        let (page, mut line) = {
            let page = lock(&PAGE);
            (page.closed_lines(), page.open_line())
        };
        let prompt: usize = line.iter().map(|(t, _)| super::width(t)).sum();
        line.push((input.value().to_string(), Tone::Plain));
        let cursor = prompt + input.visual_cursor();
        terminal
            .draw(|f| render_console(f, page, vec![line], Some(cursor)))
            .context("could not draw the question")?;
        let Event::Key(key) = event::read().context("could not read a key")? else {
            continue;
        };
        if super::is_ctrl_c(&key) {
            super::quit_on_ctrl_c();
        }
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Enter => {
                let answer = input.value().to_string();
                add(&format!("{answer}\n"));
                return Ok(Some(answer));
            }
            KeyCode::Esc => return Ok(None),
            _ => {
                if let Some(request) = to_input_request(&Event::Key(key)) {
                    input.handle(request);
                }
            }
        }
    }
}

/// The session's page as the input of a line-based prompt: each line read
/// is typed on the screen ([`read_line`]); Esc is the end of input.
#[derive(Debug, Default)]
pub struct LineInput {
    buffer: Vec<u8>,
    at: usize,
}

impl Read for LineInput {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for LineInput {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.at >= self.buffer.len() {
            self.buffer.clear();
            self.at = 0;
            if let Some(line) = read_line().map_err(io::Error::other)? {
                self.buffer = format!("{line}\n").into_bytes();
            }
        }
        Ok(&self.buffer[self.at..])
    }

    fn consume(&mut self, n: usize) {
        self.at = (self.at + n).min(self.buffer.len());
    }
}

/// The session's page as the output of a line-based prompt.
#[derive(Debug, Default)]
pub struct PageOutput;

impl Write for PageOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        add(&String::from_utf8_lossy(bytes));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ------------------------------------------------------------ hand over

/// The terminal lent to a child that talks to the user itself (rclone's
/// browser sign-in), inside the alternate screen: raw mode is off, the
/// screen is cleared and shows the page, and the child writes under it.
/// Dropping it takes the terminal back; the next screen redraws in full.
pub struct HandOver {
    terminal: Option<DefaultTerminal>,
}

/// Lends the session's terminal to a child. `None` outside a session,
/// where the terminal is the child's anyway.
pub fn hand_over() -> Result<Option<HandOver>> {
    if !is_open() {
        return Ok(None);
    }
    let terminal = borrow(true, &mut lock(&TERMINAL))?;
    // From here on, an error drops the guard, which gives the terminal back.
    let lent = HandOver { terminal };
    let _ = crossterm::terminal::disable_raw_mode();
    let mut out = io::stdout();
    crossterm::execute!(
        out,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        crossterm::cursor::Show
    )?;
    let text = super::paint_lines(&lock(&PAGE).lines(), theme::colour_on_stdout());
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(Some(lent))
}

impl Drop for HandOver {
    fn drop(&mut self) {
        if !is_open() {
            return;
        }
        let _ = crossterm::terminal::enable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), crossterm::cursor::Hide);
        // Keys typed while rclone had the terminal must not close the
        // result or move a list's cursor.
        let _ = drain_keys();
        STALE.store(true, Ordering::SeqCst);
        give_back(&mut lock(&TERMINAL), self.terminal.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn a_screen_reuses_the_session_terminal_or_takes_its_own() {
        assert_eq!(source(true, true), Source::Session);
        assert_eq!(source(true, false), Source::Busy);
        assert_eq!(source(false, true), Source::Own);
        assert_eq!(source(false, false), Source::Own);
        // No session is open in the tests: screens would take their own
        // terminal, and nothing is kept for a page or a transcript.
        assert!(!is_open());
        record("never kept");
        assert!(lock(&TRANSCRIPT).is_empty());
        // Ending a session that is not open does nothing, however often.
        end();
        end();
        assert!(!is_open());
    }

    #[test]
    fn a_busy_terminal_is_an_error_and_is_given_back_once() {
        // The session's terminal is free: the screen takes it.
        let mut slot = Some(7u8);
        assert_eq!(borrow(true, &mut slot).unwrap(), Some(7));
        assert_eq!(slot, None);
        // Another screen has it: an error that names the bug, slot untouched.
        let err = borrow(true, &mut slot).unwrap_err().to_string();
        assert!(err.contains("already using the terminal"), "{err}");
        assert_eq!(slot, None);
        // No session: the screen takes its own, whatever the slot holds.
        assert_eq!(borrow(false, &mut slot).unwrap(), None);
        // Given back into the empty slot.
        give_back(&mut slot, Some(7));
        assert_eq!(slot, Some(7));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "given back twice")]
    fn giving_the_terminal_back_twice_is_caught() {
        let mut slot = Some(1u8);
        give_back(&mut slot, Some(2));
    }

    #[test]
    fn rules_are_cut_and_paths_carry_on_from_the_first_column() {
        let text = |rows: &[Vec<Segment>]| -> Vec<String> {
            rows.iter()
                .map(|r| r.iter().map(|(t, _)| t.as_str()).collect())
                .collect()
        };
        let rule = vec![("=".repeat(70), Tone::Plain)];
        assert_eq!(text(&wrap_for_screen(&rule, 58)), vec!["=".repeat(58)]);
        let path = "/private/tmp/claude-501/scratchpad/m12pty/data/logs/demo/d-0001/2026-10-15T122500.jsonl";
        let log = vec![
            ("Log: ".to_string(), Tone::Plain),
            (path.to_string(), Tone::Dim),
        ];
        let rows = text(&wrap_for_screen(&log, 40));
        assert!(rows[0].starts_with("Log: /private/"), "{rows:?}");
        assert!(rows.iter().all(|r| crate::ui::width(r) <= 40), "{rows:?}");
        assert!(
            !rows[1].starts_with(' '),
            "no indent inside a path: {rows:?}"
        );
        assert_eq!(rows.concat(), format!("Log: {path}"), "copied in one piece");
        // Ordinary words still wrap with the continuation indented.
        let words = vec![(
            "  ✗ rclone stopped before the sign-in finished, so nothing changed.".to_string(),
            Tone::Plain,
        )];
        let rows = text(&wrap_for_screen(&words, 40));
        assert_eq!(rows[1], "      finished, so nothing changed.", "{rows:?}");
        // A short word after a path keeps to the rows.
        let line = vec![(format!("demo → {path}   one-way   complete"), Tone::Plain)];
        let rows = text(&wrap_for_screen(&line, 58));
        assert!(rows[0].starts_with("demo → /private/"), "{rows:?}");
        assert!(rows.iter().all(|r| crate::ui::width(r) <= 58), "{rows:?}");
        assert!(rows.concat().contains(path), "{rows:?}");
    }

    #[test]
    fn the_page_reads_painted_text_back_into_tones() {
        let mut page = Page::new();
        let p = |t: &str, tone| theme::paint(t, tone, true);
        page.add(&format!("{} Signed in.\n", p("→", Tone::Accent)));
        page.add(&format!("  {} ", p("Label:", Tone::Bold)));
        assert_eq!(
            page.open_line(),
            vec![
                ("  ".into(), Tone::Plain),
                ("Label:".into(), Tone::Bold),
                (" ".into(), Tone::Plain)
            ]
        );
        page.add("work\n");
        page.add(&p("two\nlines", Tone::Error));
        page.add("\n");
        assert_eq!(
            page.lines(),
            vec![
                vec![
                    ("→".into(), Tone::Accent),
                    (" Signed in.".into(), Tone::Plain)
                ],
                vec![
                    ("  ".into(), Tone::Plain),
                    ("Label:".into(), Tone::Bold),
                    (" work".into(), Tone::Plain)
                ],
                vec![("two".into(), Tone::Error)],
                vec![("lines".into(), Tone::Error)],
            ]
        );
        // Plain text and a closing newline leave no empty line behind.
        let mut plain = Page::new();
        plain.add("a\r\nb\n");
        assert_eq!(plain.lines().len(), 2);
        assert!(Page::new().lines().is_empty());
    }

    /// Fifty numbered lines with a summary-like block at the end.
    fn sample() -> Vec<Vec<Segment>> {
        let mut page = Page::new();
        for i in 1..=50 {
            page.add(&format!("line {i}\n"));
        }
        page.add(&format!(
            "{}\n{}   {}\n  Log: ~/.local/share/onefolder/logs/documents/d-7f3a/2026-10-15T10-00-00.jsonl\n{}\n",
            theme::rule_for(Some(80)),
            theme::paint("✓ documents → /Volumes/Backup/Documents", Tone::Bold, true),
            theme::paint("ok", Tone::Success, true),
            theme::rule_for(Some(80)),
        ));
        page.lines()
    }

    fn render(screen: &TextScreen, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
        terminal.draw(|f| screen.render(f)).unwrap();
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

    #[test]
    fn the_text_screen_starts_at_the_end_and_scrolls() {
        let mut screen = TextScreen::new(sample());
        let end = render(&screen, 80);
        assert!(end.contains("Log: ~/.local"), "{end}");
        assert!(end.contains("esc back · ↑↓ pgup/pgdn scroll · g/G top/bottom"));
        snapshot("text_screen_80", end);
        // 55 rows (the Log: path wraps once at 78 columns), 10 on screen:
        // the end is row 45.
        assert_eq!(screen.max.get(), 45);
        assert!(!screen.handle_key(key(KeyCode::Char('g'))));
        let top = render(&screen, 80);
        assert!(top.starts_with("\"  line 1 "), "{top}");
        assert!(!screen.handle_key(key(KeyCode::Down)));
        assert!(!screen.handle_key(key(KeyCode::Char('j'))));
        assert_eq!(screen.scroll, 2);
        assert!(!screen.handle_key(key(KeyCode::Char('k'))));
        assert_eq!(screen.scroll, 1);
        assert!(!screen.handle_key(key(KeyCode::PageDown)));
        assert_eq!(screen.scroll, 11);
        assert!(!screen.handle_key(key(KeyCode::PageUp)));
        assert!(!screen.handle_key(key(KeyCode::PageUp)));
        assert_eq!(screen.scroll, 0);
        assert!(!screen.handle_key(key(KeyCode::Up)), "stays at the top");
        assert_eq!(screen.scroll, 0);
        assert!(!screen.handle_key(key(KeyCode::Char('G'))));
        assert!(!screen.handle_key(key(KeyCode::Down)), "stays at the end");
        assert_eq!(screen.scroll, 45);
        // Any other key goes back, as "Press any key" did.
        for code in [
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Char('q'),
            KeyCode::Char('x'),
            KeyCode::Char(' '),
        ] {
            assert!(screen.clone().handle_key(key(code)), "{code:?}");
        }
    }

    #[test]
    fn the_text_screen_fits_long_lines_to_the_width() {
        let long = format!(
            "{} {}",
            theme::paint("✗", Tone::Error, true),
            "rclone stopped before the sign-in finished, and OneFolder removed the half-made rclone remote onefolder-work, so nothing changed."
        );
        let mut page = Page::new();
        page.add(&format!("{long}\nshort\n"));
        let screen = TextScreen::new(page.lines());
        let at_120 = render(&screen, 120);
        snapshot("text_screen_120", at_120);
        let at_80 = render(&screen, 80);
        assert!(
            at_80.contains("\n\"      half-made rclone remote"),
            "{at_80}"
        );
        // A resize redraws at the new width with the same scroll rule.
        assert_eq!(screen.max.get(), 0);
    }

    #[test]
    fn the_console_keeps_the_end_in_view() {
        let page: Vec<Vec<Segment>> = (1..=20)
            .map(|i| vec![(format!("line {i}"), Tone::Plain)])
            .collect();
        let mut picker = Picker::new(
            "Run which job?",
            vec![
                ("documents".into(), "~/Documents".into()),
                ("photos".into(), "~/Pictures".into()),
            ],
        );
        picker.width = Some(80);
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|f| render_console(f, page, picker.segments(), None))
            .unwrap();
        let text = terminal.backend().to_string();
        assert!(
            text.contains("line 20") && !text.contains("line 16"),
            "{text}"
        );
        assert!(
            text.contains("➤ documents") && text.contains("esc cancel"),
            "{text}"
        );
    }
}
