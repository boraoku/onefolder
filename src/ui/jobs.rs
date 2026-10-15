//! The jobs list: printed by `onefolder jobs list`, and the interactive
//! screen behind `onefolder jobs` (and the menu's Jobs entry) where
//! a, e and d add, edit and remove.
//!
//! [`list_lines`] decides the text and tone of every piece, so the printed
//! list and the screen always say the same thing.

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::model::{Mode, Outcome};
use crate::ui::theme::{self, Tone};

/// A run of text with one tone.
pub type Segment = (String, Tone);

/// What the list knows about the last run of a destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastRun {
    pub outcome: Outcome,
    pub dry_run: bool,
    /// When it finished, in seconds since the Unix epoch, if readable.
    pub finished: Option<i64>,
}

/// One destination as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationView {
    pub target: String,
    pub mode: Mode,
    /// False when the recorded drive is not mounted right now.
    pub mounted: bool,
    /// The recorded drive's mount point did not answer (a hung network drive).
    pub not_answering: bool,
    /// The last real run.
    pub last_run: Option<LastRun>,
    /// The last dry run, shown only when it is newer than the last real run.
    pub last_dry_run: Option<LastRun>,
    /// Folders on hold for this destination.
    pub holds: usize,
}

/// One job as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobView {
    pub name: String,
    /// Already shortened with `~`.
    pub source: String,
    /// The job copies hidden files; shown after the source only when on,
    /// since leaving them out is the default.
    pub copy_hidden: bool,
    pub destinations: Vec<DestinationView>,
}

/// The note after the source of a job that copies hidden files.
pub const HIDDEN_NOTE: &str = "hidden files copied";

/// `Jobs  3 jobs · 4 destinations`.
pub fn header(jobs: &[JobView]) -> Vec<Segment> {
    let dests: usize = jobs.iter().map(|j| j.destinations.len()).sum();
    vec![
        ("Jobs".to_string(), Tone::Bold),
        (
            format!(
                "  {} {} {}",
                plural(jobs.len(), "job"),
                theme::DOT,
                plural(dests, "destination")
            ),
            Tone::Dim,
        ),
    ]
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The status column of a destination.
pub fn status(d: &DestinationView, now: i64) -> Segment {
    let mut text;
    let tone;
    if d.not_answering {
        text = format!("{} not answering", theme::SKIPPED);
        tone = Tone::Warning;
    } else if !d.mounted {
        text = format!("{} not mounted", theme::SKIPPED);
        tone = Tone::Warning;
    } else if let Some(run) = &d.last_run {
        let when = run.finished.map(|t| ago(t, now)).unwrap_or_default();
        let (glyph, word, t) = match run.outcome {
            Outcome::Ok => (theme::DONE, "", Tone::Success),
            Outcome::Partial => (theme::SKIPPED, "partial ", Tone::Warning),
            Outcome::Cancelled => (theme::SKIPPED, "cancelled ", Tone::Warning),
            Outcome::Failed => (theme::FAILED, "failed ", Tone::Error),
            Outcome::NeedsResync => (theme::FAILED, "needs resync ", Tone::Error),
        };
        let dry = if run.dry_run { "dry run " } else { "" };
        text = format!("{glyph} {dry}{word}{when}").trim_end().to_string();
        tone = t;
    } else {
        text = "never run".to_string();
        tone = Tone::Dim;
    }
    if d.mounted {
        if let Some(dry) = newer_dry_run(d) {
            let when = dry.finished.map(|t| ago(t, now)).unwrap_or_default();
            text.push_str(&format!(" {} dry run {when}", theme::DOT));
            text = text.trim_end().to_string();
        }
    }
    if d.holds > 0 {
        text.push_str(&format!(" {} {} held", theme::DOT, d.holds));
    }
    (text, tone)
}

/// The last dry run when it is newer than the last real run (or there is
/// no real run yet).
pub fn newer_dry_run(d: &DestinationView) -> Option<&LastRun> {
    let dry = d.last_dry_run.as_ref()?;
    match &d.last_run {
        None => Some(dry),
        Some(real) => match (dry.finished, real.finished) {
            (Some(a), Some(b)) if a > b => Some(dry),
            (Some(_), None) => Some(dry),
            _ => None,
        },
    }
}

/// Columns the status after the mode is given before paths are shortened.
const STATUS_ROOM: usize = 26;

/// The lines under the header: one per job, one per destination. `cursor`
/// marks the selected job with `➤`; `None` prints the plain list. With a
/// `width`, long sources and destinations are shortened in the middle
/// (keeping their last two components) so each line fits.
pub fn list_lines(
    jobs: &[JobView],
    cursor: Option<usize>,
    now: i64,
    width: Option<usize>,
) -> Vec<Vec<Segment>> {
    if jobs.is_empty() {
        return vec![vec![(
            format!(
                "  {} none yet {} add one with: onefolder jobs add",
                theme::SKIPPED,
                theme::DOT
            ),
            Tone::Dim,
        )]];
    }
    let name_w = jobs
        .iter()
        .map(|j| super::width(&j.name))
        .max()
        .unwrap_or(0)
        .max(12)
        + 3;
    // The destination column: as wide as the longest, but within the
    // width once the indent, the mode and some status are taken.
    let target_max = width.map(|w| w.saturating_sub(8 + 2 + 10 + STATUS_ROOM).max(16));
    let natural = jobs
        .iter()
        .flat_map(|j| &j.destinations)
        .map(|d| super::width(&d.target))
        .max()
        .unwrap_or(0)
        .max(30);
    let target_w = target_max.map_or(natural, |m| natural.min(m)) + 2;
    let source_max = width.map(|w| w.saturating_sub(2 + name_w).max(10));
    let fit = |text: &str, max: Option<usize>| match max {
        Some(m) => super::shorten_path(text, m),
        None => text.to_string(),
    };
    let mut lines = Vec::new();
    for (i, job) in jobs.iter().enumerate() {
        let selected = cursor == Some(i);
        let mut line = if selected {
            vec![(format!("{} ", theme::CURSOR), Tone::Accent)]
        } else {
            vec![("  ".to_string(), Tone::Plain)]
        };
        line.push((
            super::pad(&job.name, name_w),
            if selected { Tone::Accent } else { Tone::Bold },
        ));
        let note = job
            .copy_hidden
            .then(|| format!("  {} {HIDDEN_NOTE}", theme::DOT));
        let room = source_max.map(|m| {
            m.saturating_sub(note.as_deref().map_or(0, super::width))
                .max(10)
        });
        line.push((fit(&job.source, room), Tone::Plain));
        if let Some(note) = note {
            line.push((note, Tone::Dim));
        }
        lines.push(line);
        if job.destinations.is_empty() {
            lines.push(vec![(
                format!("      {} no destinations yet", theme::SKIPPED),
                Tone::Warning,
            )]);
        }
        for d in &job.destinations {
            lines.push(vec![
                (
                    format!(
                        "      {} {}",
                        theme::ACTION,
                        super::pad(&fit(&d.target, Some(target_w - 2)), target_w)
                    ),
                    Tone::Plain,
                ),
                (format!("{:<10}", d.mode.as_str()), Tone::Plain),
                status(d, now),
            ]);
        }
    }
    lines
}

/// The printed list, for `onefolder jobs list` and a non-terminal stdout.
/// `width` is the terminal's, or `None` to print paths in full.
pub fn render_text(jobs: &[JobView], now: i64, colour: bool, width: Option<usize>) -> String {
    let mut out = String::new();
    let mut push = |segments: &[Segment]| {
        let line: String = segments
            .iter()
            .map(|(text, tone)| theme::paint(text, *tone, colour))
            .collect();
        out.push_str(line.trim_end());
        out.push('\n');
    };
    push(&header(jobs));
    push(&[]);
    for line in list_lines(jobs, None, now, width) {
        push(&line);
    }
    out
}

/// "2h ago", "yesterday", "3d ago", from two Unix times in seconds.
pub fn ago(then: i64, now: i64) -> String {
    let secs = (now - then).max(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=172_799 => "yesterday".to_string(),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Seconds since the Unix epoch for an RFC 3339 timestamp such as
/// `2026-10-13T09:41:02+11:00`. `None` when it cannot be read.
pub fn unix_seconds(text: &str) -> Option<i64> {
    let dt: toml_edit::Datetime = text.parse().ok()?;
    let date = dt.date?;
    let time = dt.time?;
    let offset_minutes = match dt.offset? {
        toml_edit::Offset::Z => 0,
        toml_edit::Offset::Custom { minutes } => i64::from(minutes),
    };
    // Days from civil, after Howard Hinnant's algorithm.
    let (y, m, d) = (
        i64::from(date.year),
        i64::from(date.month),
        i64::from(date.day),
    );
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400
        + i64::from(time.hour) * 3600
        + i64::from(time.minute) * 60
        + i64::from(time.second.unwrap_or(0));
    Some(secs - offset_minutes * 60)
}

/// What the user chose on the interactive screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Add,
    Edit(String),
    /// Already confirmed on the screen.
    Remove(String),
    Run(String),
    Back,
}

/// The interactive list's state.
#[derive(Debug, Clone)]
pub struct Screen {
    pub jobs: Vec<JobView>,
    pub cursor: usize,
    /// Index of the job waiting for a y/n to remove.
    pub confirm: Option<usize>,
    /// A line above the key hints: the result of the last action.
    pub message: Option<(String, Tone)>,
    pub now: i64,
    /// Text the list is filtered by (`/`): a job shows when its name, its
    /// source or one of its destinations contains it, ignoring case.
    pub filter: String,
    /// The filter is being typed.
    pub filtering: bool,
    /// The keys are shown (`?`).
    pub help: bool,
}

/// The keys of the jobs list, for its `?` overlay.
pub const KEYS: [(&str, &str); 11] = [
    ("↑↓  j/k", "move"),
    ("g  G", "first, last"),
    ("enter", "run the job"),
    ("a", "add a job"),
    ("e", "edit the job"),
    ("d", "remove the job (asks first)"),
    ("/", "filter by name, folder or destination"),
    ("?", "show or hide these keys"),
    ("esc", "clear the filter, or go back"),
    ("q", "go back"),
    ("ctrl-c", "quit OneFolder"),
];

/// True when `text` contains `filter`, ignoring case.
pub fn matches_filter(text: &str, filter: &str) -> bool {
    filter.is_empty() || text.to_lowercase().contains(&filter.to_lowercase())
}

impl Screen {
    pub fn new(jobs: Vec<JobView>, now: i64) -> Screen {
        Screen {
            jobs,
            cursor: 0,
            confirm: None,
            message: None,
            now,
            filter: String::new(),
            filtering: false,
            help: false,
        }
    }

    /// Indexes of the jobs the filter lets through.
    pub fn visible(&self) -> Vec<usize> {
        self.jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| {
                matches_filter(&j.name, &self.filter)
                    || matches_filter(&j.source, &self.filter)
                    || j.destinations
                        .iter()
                        .any(|d| matches_filter(&d.target, &self.filter))
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn shown(&self) -> Vec<JobView> {
        self.visible()
            .into_iter()
            .filter_map(|i| self.jobs.get(i).cloned())
            .collect()
    }

    /// Replaces the jobs, keeping the cursor on the same name when it is still there.
    pub fn set_jobs(&mut self, jobs: Vec<JobView>, select: Option<&str>) {
        let keep = select.map(str::to_string).or_else(|| self.selected());
        self.jobs = jobs;
        let shown = self.shown();
        self.cursor = keep
            .and_then(|name| shown.iter().position(|j| j.name == name))
            .unwrap_or(self.cursor)
            .min(shown.len().saturating_sub(1));
        self.confirm = None;
    }

    /// The job under the cursor, among those shown.
    fn selected(&self) -> Option<String> {
        self.visible()
            .get(self.cursor)
            .and_then(|i| self.jobs.get(*i))
            .map(|j| j.name.clone())
    }

    /// A key while the filter is typed: letters add to it, Backspace
    /// takes one off, Enter keeps it, Esc clears it.
    fn filter_key(&mut self, key: KeyEvent) {
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
            self.filter_key(key);
            return None;
        }
        if let Some(i) = self.confirm.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                return self.jobs.get(i).map(|j| Action::Remove(j.name.clone()));
            }
            self.message = Some(("Kept it.".to_string(), Tone::Dim));
            return None;
        }
        let last = self.visible().len().saturating_sub(1);
        match key.code {
            KeyCode::Char('/') => {
                self.filtering = true;
                self.message = None;
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.cursor = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = last,
            KeyCode::Char('a') => return Some(Action::Add),
            KeyCode::Char('e') => return self.selected().map(Action::Edit),
            KeyCode::Enter => return self.selected().map(Action::Run),
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(name) = self.selected() {
                    self.confirm = self.jobs.iter().position(|j| j.name == name);
                    self.message = Some((
                        format!(
                            "Remove job {name}? Files and run logs are left where they are. y remove {} any other key keeps it",
                            theme::DOT
                        ),
                        Tone::Warning,
                    ));
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => return Some(Action::Back),
            _ => {}
        }
        None
    }

    pub fn lines(&self, width: Option<usize>) -> Vec<Line<'static>> {
        let to_line = |segments: Vec<Segment>| {
            Line::from(
                segments
                    .into_iter()
                    .map(|(text, tone)| Span::styled(text, theme::style(tone)))
                    .collect::<Vec<_>>(),
            )
        };
        let mut head = header(&self.jobs);
        if self.filtering || !self.filter.is_empty() {
            let cursor = if self.filtering { "▏" } else { "" };
            head.push((
                format!(" {} /{}{cursor}", theme::DOT, self.filter),
                Tone::Accent,
            ));
        }
        let mut lines = vec![to_line(head), Line::default()];
        let shown = self.shown();
        if shown.is_empty() && !self.jobs.is_empty() {
            lines.push(to_line(vec![(
                format!(
                    "  {} no job matches {} esc clears the filter",
                    theme::SKIPPED,
                    theme::DOT
                ),
                Tone::Dim,
            )]));
            return lines;
        }
        lines.extend(
            list_lines(&shown, Some(self.cursor), self.now, width)
                .into_iter()
                .map(to_line),
        );
        lines
    }

    fn hints(&self) -> String {
        let d = theme::DOT;
        if self.filtering {
            format!("type to filter {d} enter keep {d} esc clear")
        } else if self.jobs.is_empty() {
            format!("a add {d} ? keys {d} q back")
        } else {
            format!(
                "enter run {d} a add {d} e edit {d} d remove {d} / filter {d} ? keys {d} q back"
            )
        }
    }

    pub fn render(&self, frame: &mut Frame) {
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
            .map(|(m, _)| m.lines().count() as u16 + 1)
            .unwrap_or(0);
        let body_height = area.height.saturating_sub(2 + message_height);
        // Keep the selected job on screen.
        let lines = self.lines(Some(usize::from(area.width)));
        let selected_line = lines
            .iter()
            .position(|l| {
                l.spans
                    .first()
                    .is_some_and(|s| s.content.starts_with(theme::CURSOR))
            })
            .unwrap_or(0) as u16;
        let scroll = (selected_line + 3).saturating_sub(body_height);
        frame.render_widget(
            Paragraph::new(lines).scroll((scroll, 0)),
            Rect {
                height: body_height,
                ..area
            },
        );
        if let Some((message, tone)) = &self.message {
            let msg: Vec<Line> = message
                .lines()
                .map(|l| Line::from(Span::styled(l.to_string(), theme::style(*tone))))
                .collect();
            frame.render_widget(
                Paragraph::new(msg),
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
            super::menu::render_keys(frame, "Jobs keys", &KEYS);
        }
    }
}

/// Shows the screen until the user chooses an action. The terminal is
/// back to normal when this returns.
pub fn run(screen: &mut Screen) -> Result<Action> {
    let mut terminal = super::screen()?;
    loop {
        terminal
            .draw(|f| screen.render(f))
            .context("could not draw the jobs list")?;
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// 13 Oct 2026, 11:41 in Sydney.
    const NOW: i64 = 1_791_852_062;

    fn mockup() -> Vec<JobView> {
        let ok = |hours: i64| {
            Some(LastRun {
                outcome: Outcome::Ok,
                dry_run: false,
                finished: Some(NOW - hours * 3600),
            })
        };
        vec![
            JobView {
                name: "documents".into(),
                source: "~/Documents".into(),
                copy_hidden: false,
                destinations: vec![
                    DestinationView {
                        target: "work:Backups/Documents".into(),
                        mode: Mode::TwoWay,
                        mounted: true,
                        not_answering: false,
                        last_run: ok(2),
                        last_dry_run: None,
                        holds: 0,
                    },
                    DestinationView {
                        target: "/Volumes/Backup/Documents".into(),
                        mode: Mode::OneWay,
                        mounted: false,
                        not_answering: false,
                        last_run: ok(30),
                        last_dry_run: None,
                        holds: 0,
                    },
                ],
            },
            JobView {
                name: "photos".into(),
                source: "~/Pictures/Photos".into(),
                copy_hidden: false,
                destinations: vec![DestinationView {
                    target: "personal:Photos".into(),
                    mode: Mode::OneWay,
                    mounted: true,
                    not_answering: false,
                    last_run: ok(26),
                    last_dry_run: None,
                    holds: 0,
                }],
            },
            JobView {
                name: "notes".into(),
                source: "~/Notes".into(),
                copy_hidden: false,
                destinations: vec![DestinationView {
                    target: "work:Notes".into(),
                    mode: Mode::TwoWay,
                    mounted: true,
                    not_answering: false,
                    last_run: Some(LastRun {
                        outcome: Outcome::Failed,
                        dry_run: false,
                        finished: Some(NOW - 3 * 86_400),
                    }),
                    last_dry_run: None,
                    holds: 2,
                }],
            },
        ]
    }

    #[test]
    fn plain_text_matches_the_mockup() {
        let text = render_text(&mockup(), NOW, false, None);
        assert_eq!(
            text,
            "Jobs  3 jobs · 4 destinations\n\
             \n\
             \x20 documents      ~/Documents\n\
             \x20     → work:Backups/Documents          two-way   ✓ 2h ago\n\
             \x20     → /Volumes/Backup/Documents       one-way   ◎ not mounted\n\
             \x20 photos         ~/Pictures/Photos\n\
             \x20     → personal:Photos                 one-way   ✓ yesterday\n\
             \x20 notes          ~/Notes\n\
             \x20     → work:Notes                      two-way   ✗ failed 3d ago · 2 held\n"
        );
    }

    #[test]
    fn a_job_that_copies_hidden_files_says_so() {
        let mut jobs = mockup();
        jobs[1].copy_hidden = true;
        let text = render_text(&jobs, NOW, false, None);
        assert!(
            text.contains("\n  photos         ~/Pictures/Photos  · hidden files copied\n"),
            "{text}"
        );
        assert!(!text.contains("~/Documents  ·"), "{text}");
        jobs[1].source = "~/Pictures/Photos/A very long folder name/Another one/2026".into();
        for width in [60, 80] {
            let text = render_text(&jobs, NOW, false, Some(width));
            assert_fits(&text, width);
            assert!(text.contains("· hidden files copied"), "{text}");
        }
    }

    #[test]
    fn long_paths_are_shortened_in_the_middle_to_fit() {
        let mut jobs = mockup();
        jobs[0].source =
            "~/Library/Mobile Documents/com~apple~CloudDocs/Projects/Clients/2026".into();
        jobs[0].destinations[0].target =
            "work:Backups/Very long folder name/Another long folder/Documents/2026".into();
        jobs[0].destinations[1].target =
            "/Volumes/Backup/Archive/Deeply/Nested/Folder/Structure/Documents".into();
        for width in [60, 80, 120] {
            let text = render_text(&jobs, NOW, false, Some(width));
            assert_fits(&text, width);
            // The last two components survive; at 120 columns all fits.
            if width == 60 {
                assert!(text.contains("→ work:…"), "the account is kept: {text}");
                assert!(text.contains("…/Clients/2026"), "{text}");
            } else if width == 80 {
                assert!(text.contains("…/Documents/2026"), "{text}");
                assert!(text.contains("…/Structure/Documents"), "{text}");
                assert!(text.contains("…/Clients/2026"), "{text}");
            } else {
                assert!(!text.contains('…'), "{text}");
            }
            // The columns still line up: every mode starts at the same place.
            let starts: Vec<usize> = text
                .lines()
                .filter(|l| l.contains(" → "))
                .map(|l| {
                    let byte = l.find("-way").unwrap();
                    crate::ui::width(&l[..byte])
                })
                .collect();
            assert!(starts.windows(2).all(|w| w[0] == w[1]), "{text}");
            snapshot(&format!("jobs_long_paths_{width}"), text);
        }
        // Without a width (a pipe) nothing is shortened.
        let full = render_text(&jobs, NOW, false, None);
        assert!(full.contains("Deeply/Nested/Folder"), "{full}");
    }

    fn assert_fits(text: &str, width: usize) {
        for line in text.lines() {
            let w = crate::ui::width(line);
            assert!(w <= width, "{w} > {width}: {line}");
        }
    }

    /// Wide characters take two columns: the columns still line up and
    /// every line fits.
    #[test]
    fn wide_characters_line_up() {
        let mut jobs = mockup();
        jobs[0].name = "documents".into();
        jobs[0].source = "~/書類/プロジェクト/二〇二六年".into();
        jobs[0].destinations[0].target = "work:バックアップ/書類/顧客/契約書/署名済み".into();
        jobs[0].destinations[1].target = "/Volumes/バックアップ/書類".into();
        for width in [60, 80] {
            let text = render_text(&jobs, NOW, false, Some(width));
            assert_fits(&text, width);
            let starts: Vec<usize> = text
                .lines()
                .filter(|l| l.contains(" → "))
                .map(|l| crate::ui::width(&l[..l.find("-way").unwrap()]))
                .collect();
            assert!(starts.windows(2).all(|w| w[0] == w[1]), "{text}");
            snapshot(&format!("jobs_cjk_{width}"), text);
        }
    }

    #[test]
    fn a_newer_dry_run_is_mentioned() {
        let mut jobs = mockup();
        let d = &mut jobs[1].destinations[0];
        d.last_dry_run = Some(LastRun {
            outcome: Outcome::Ok,
            dry_run: true,
            finished: Some(NOW - 600),
        });
        let text = render_text(&jobs, NOW, false, None);
        assert!(
            text.contains(
                "personal:Photos                 one-way   ✓ yesterday · dry run 10m ago"
            ),
            "{text}"
        );
        // An older dry run is not.
        jobs[1].destinations[0]
            .last_dry_run
            .as_mut()
            .unwrap()
            .finished = Some(NOW - 40 * 3600);
        let text = render_text(&jobs, NOW, false, None);
        assert!(!text.contains("dry run"), "{text}");
        // Never run for real.
        jobs[1].destinations[0].last_run = None;
        let text = render_text(&jobs, NOW, false, None);
        assert!(text.contains("never run · dry run yesterday"), "{text}");
    }

    #[test]
    fn empty_list_says_how_to_add() {
        let text = render_text(&[], NOW, false, None);
        assert!(
            text.starts_with("Jobs  0 jobs · 0 destinations\n\n"),
            "{text}"
        );
        assert!(text.contains("onefolder jobs add"), "{text}");
    }

    #[test]
    fn times() {
        assert_eq!(ago(NOW - 5, NOW), "just now");
        assert_eq!(ago(NOW - 300, NOW), "5m ago");
        assert_eq!(ago(NOW - 2 * 3600, NOW), "2h ago");
        assert_eq!(ago(NOW - 30 * 3600, NOW), "yesterday");
        assert_eq!(ago(NOW - 3 * 86_400, NOW), "3d ago");
        assert_eq!(ago(NOW + 50, NOW), "just now", "clock skew");
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2026-10-13T11:41:02+11:00"), Some(NOW));
        assert_eq!(unix_seconds("2026-10-13T00:41:02Z"), Some(NOW));
        assert_eq!(unix_seconds("2000-02-29T12:00:00-05:30"), Some(951_845_400));
        assert_eq!(unix_seconds("2026-10-13"), None);
        assert_eq!(unix_seconds("yesterday"), None);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn keys() {
        let mut s = Screen::new(mockup(), NOW);
        s.handle_key(key(KeyCode::Down));
        s.handle_key(key(KeyCode::Char('j')));
        s.handle_key(key(KeyCode::Char('j')));
        assert_eq!(s.cursor, 2);
        s.handle_key(key(KeyCode::Char('k')));
        s.handle_key(key(KeyCode::Up));
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.cursor, 0);
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Action::Run("documents".into()))
        );
        assert_eq!(
            s.handle_key(key(KeyCode::Char('e'))),
            Some(Action::Edit("documents".into()))
        );
        assert_eq!(s.handle_key(key(KeyCode::Char('a'))), Some(Action::Add));
        // Remove asks first; anything but y keeps the job.
        assert_eq!(s.handle_key(key(KeyCode::Char('d'))), None);
        assert_eq!(s.handle_key(key(KeyCode::Char('n'))), None);
        assert_eq!(s.confirm, None);
        s.handle_key(key(KeyCode::Char('d')));
        assert_eq!(
            s.handle_key(key(KeyCode::Char('y'))),
            Some(Action::Remove("documents".into()))
        );
        assert_eq!(s.handle_key(key(KeyCode::Char('q'))), Some(Action::Back));
        assert_eq!(s.handle_key(key(KeyCode::Esc)), Some(Action::Back));
    }

    #[test]
    fn slash_filters_the_list() {
        let mut s = Screen::new(mockup(), NOW);
        s.handle_key(key(KeyCode::Char('/')));
        assert!(s.filtering);
        for c in "PHO".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(s.visible(), vec![1]);
        // While typing, letters are text, not commands.
        assert_eq!(s.filter, "PHO");
        s.handle_key(key(KeyCode::Enter));
        assert!(!s.filtering);
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Action::Run("photos".into()))
        );
        snapshot("jobs_filtered_80", render(&s, 80));
        // A destination matches too.
        s.filter = "backup".into();
        assert_eq!(s.visible(), vec![0]);
        // Esc clears the filter first, then goes back.
        assert_eq!(s.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(s.visible(), vec![0, 1, 2]);
        assert_eq!(s.handle_key(key(KeyCode::Esc)), Some(Action::Back));
        // Nothing matches.
        s.filter = "zzz".into();
        assert!(render(&s, 80).contains("no job matches"));
        assert_eq!(s.handle_key(key(KeyCode::Char('e'))), None);
    }

    #[test]
    fn question_mark_shows_the_keys() {
        let mut s = Screen::new(mockup(), NOW);
        s.handle_key(key(KeyCode::Char('?')));
        assert!(s.help);
        snapshot("jobs_keys_80", render(&s, 80));
        assert_eq!(
            s.handle_key(key(KeyCode::Char('q'))),
            None,
            "q closes the keys"
        );
        assert!(!s.help);
    }

    #[test]
    fn set_jobs_keeps_the_selection() {
        let mut s = Screen::new(mockup(), NOW);
        s.cursor = 2;
        let mut jobs = mockup();
        jobs.remove(0);
        s.set_jobs(jobs, None);
        assert_eq!(s.cursor, 1);
        assert_eq!(s.jobs[s.cursor].name, "notes");
        s.set_jobs(Vec::new(), None);
        assert_eq!(s.cursor, 0);
        assert_eq!(s.handle_key(key(KeyCode::Char('e'))), None);
    }

    fn render(screen: &Screen, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
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
    fn renders_at_80_columns() {
        let s = Screen::new(mockup(), NOW);
        snapshot("jobs_80", render(&s, 80));
    }

    #[test]
    fn renders_at_120_columns_with_a_message() {
        let mut s = Screen::new(mockup(), NOW);
        s.cursor = 1;
        s.handle_key(key(KeyCode::Char('d')));
        snapshot("jobs_120", render(&s, 120));
    }
}
