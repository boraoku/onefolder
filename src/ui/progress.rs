//! The run screen: the destination, the phase, the overall bar, the last
//! files and the errors, redrawn from the runner's events at most ten times
//! a second. [`TuiReporter`] is the [`Reporter`] behind it: the screen runs
//! on its own thread, which owns the terminal; the runner sends it messages
//! and, for the preview, waits for the answer.
//!
//! When the run is over the screen closes and the summary blocks (and, for
//! `--dry-run`, the previews) are printed on the normal screen, so they
//! stay in the terminal's scrollback.
//!
//! A batch of jobs keeps one screen from its first job to its last: each
//! job starts with [`TuiReporter::begin_job`], every screen (the run, the
//! questions, the whole lists) carries the batch line on top, and the text
//! printed afterwards starts each job's part with the batch line as that
//! job left it. Inside the interactive session they
//! are shown on the session's text screen instead, and the summary blocks
//! are printed when the session ends (see `ui::session`).

use std::collections::VecDeque;
use std::io::IsTerminal;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event as KeyInput, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::model::{FileError, Summary};
use crate::rclone::bisync::BisyncPhase;
use crate::rclone::events::Stats;
use crate::sync::oneway::Preview;
use crate::sync::plan::{Plan, ResyncOffer};
use crate::sync::reporter::{
    self, Answer, Change, ConflictDone, DecidedBy, DeletedFolder, DestinationInfo, Expected,
    FileDone, FolderAnswer, Looking, MovesAnswer, Question, Reporter, Step, Where,
};
use crate::sync::twoway::place;
use crate::ui::batch::{self as several, BatchLine, JobEnd};
use crate::ui::prompt::{self, FolderPrompt, PreviewPrompt};
use crate::ui::summary::{self as block, format_bytes, format_duration};
use crate::ui::theme::{self, Tone};

/// How many finished files the screen keeps.
pub const RECENT: usize = 8;

/// A finished line in the file list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recent {
    Done(FileDone),
    Failed(FileError),
}

/// What the run screen shows.
#[derive(Debug, Clone)]
pub struct ProgressModel {
    pub job: String,
    pub dest: Option<DestinationInfo>,
    pub step: Step,
    /// bisync's phase within a two-way step.
    pub bisync: Option<BisyncPhase>,
    pub stats: Option<Stats>,
    /// The totals the copy or sync should end at, from its preview or plan.
    pub expected: Option<Expected>,
    /// Live figures of a preview or a plan.
    pub looking: Option<Looking>,
    pub recent: VecDeque<Recent>,
    pub errors: u64,
    pub stopping: bool,
    /// Lines from earlier destinations and problems, newest last.
    pub notes: Vec<(String, Tone)>,
    /// Advances on every redraw, for the spinner.
    pub tick: usize,
    /// Where the batch has got to, on the top line; `None` for one job.
    pub batch: Option<BatchLine>,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl ProgressModel {
    pub fn new(job: &str) -> Self {
        ProgressModel {
            job: job.to_string(),
            dest: None,
            step: Step::Checking,
            bisync: None,
            stats: None,
            expected: None,
            looking: None,
            recent: VecDeque::new(),
            errors: 0,
            stopping: false,
            notes: Vec::new(),
            tick: 0,
            batch: None,
        }
    }

    /// The next job of a batch: its title, no destination yet and no
    /// notes from the job before.
    pub fn next_job(&mut self, job: &str, batch: BatchLine) {
        *self = ProgressModel {
            tick: self.tick,
            batch: Some(batch),
            ..ProgressModel::new(job)
        };
    }

    pub fn start(&mut self, dest: &DestinationInfo) {
        self.dest = Some(dest.clone());
        self.step = Step::Checking;
        self.bisync = None;
        self.stats = None;
        self.expected = None;
        self.looking = None;
        self.recent.clear();
        self.errors = 0;
        self.stopping = false;
    }

    fn push(&mut self, r: Recent) {
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(r);
    }

    pub fn file(&mut self, f: &FileDone) {
        self.push(Recent::Done(f.clone()));
    }

    pub fn file_error(&mut self, e: &FileError) {
        self.errors += 1;
        self.push(Recent::Failed(e.clone()));
    }

    /// The phase as the design names it: for the copy, Listing until the
    /// first stats line, Comparing while files are checked and none sent,
    /// then Transferring.
    pub fn phase(&self) -> &'static str {
        if self.stopping {
            return "Stopping rclone... press Ctrl-C again to stop at once";
        }
        let transferring = |stats: &Option<Stats>| {
            stats
                .as_ref()
                .is_some_and(|s| s.transfers > 0 || s.bytes > 0 || !s.transferring.is_empty())
        };
        match self.step {
            Step::Checking => "Checking",
            Step::Previewing => "Previewing",
            Step::Moving => "Moving files",
            Step::Finishing => "Finishing",
            Step::Planning => "Planning",
            Step::Restoring => "Restoring folders",
            Step::Syncing | Step::Resyncing => match self.bisync {
                Some(BisyncPhase::Applying) if transferring(&self.stats) => "Transferring",
                Some(phase) => phase.label(),
                None if self.step == Step::Resyncing => "Merging",
                None => "Syncing",
            },
            Step::Copying => match &self.stats {
                None => "Listing",
                Some(s) if s.transfers == 0 && s.bytes == 0 && s.transferring.is_empty() => {
                    "Comparing"
                }
                Some(_) => "Transferring",
            },
        }
    }

    /// rclone's stats with the totals raised to the expected ones, where
    /// the preview or the plan gave them.
    fn shown_stats(&self) -> Option<Stats> {
        let s = self.stats.as_ref()?;
        Some(self.expected.map_or_else(|| s.clone(), |e| e.raise(s)))
    }

    /// The lines of the screen above the key hints, for a body `width` wide.
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width);
        let stats = self.shown_stats();
        let mut lines = vec![
            // `Run records · partial, 2 of 3 destinations` is cut with `…`
            // on a narrow screen rather than at its edge.
            Line::from(Span::styled(
                super::fit_text(&format!("Run {}", self.job), width),
                theme::style(Tone::Bold),
            )),
            Line::default(),
        ];
        for (text, tone) in &self.notes {
            lines.push(Line::from(Span::styled(text.clone(), theme::style(*tone))));
        }
        if !self.notes.is_empty() {
            lines.push(Line::default());
        }
        let Some(dest) = &self.dest else {
            return lines;
        };
        let of = (dest.of > 1).then(|| format!("destination {} of {}", dest.index, dest.of));
        // The rest of the header takes its room first; a long path is
        // shortened in the middle with `…` rather than cut at the edge.
        let rest = 2
            + 3
            + super::width(dest.mode.as_str())
            + 3
            + of.as_deref().map_or(0, super::width)
            + if dest.dry_run { 10 } else { 0 };
        let shown = super::shorten_path(&dest.display, width.saturating_sub(rest).max(12));
        let mut header = vec![
            Span::styled(theme::ACTION, theme::style(Tone::Accent)),
            Span::styled(format!(" {shown}   "), theme::style(Tone::Bold)),
            Span::raw(format!("{}   ", dest.mode.as_str())),
        ];
        if let Some(of) = of {
            header.push(Span::styled(of, theme::style(Tone::Dim)));
        }
        if dest.dry_run {
            header.push(Span::styled("   dry run", theme::style(Tone::Warning)));
        }
        lines.push(Line::from(header));
        lines.push(Line::default());

        // Phase and counts.
        let phase = self.phase();
        let spinner = SPINNER[self.tick % SPINNER.len()];
        let streaming = matches!(
            self.step,
            Step::Copying | Step::Syncing | Step::Resyncing | Step::Restoring
        );
        let counts = match (&stats, streaming) {
            _ if self.stopping => String::new(),
            (Some(s), true) if phase == "Transferring" => {
                format!(
                    " {} {} of {} files",
                    theme::DOT,
                    prompt::thousands(s.transfers),
                    prompt::thousands(s.total_transfers)
                )
            }
            (Some(s), true) if self.step == Step::Copying => {
                format!(
                    " {} {} of {} checked",
                    theme::DOT,
                    prompt::thousands(s.checks),
                    prompt::thousands(s.total_checks)
                )
            }
            _ => String::new(),
        };
        let phase_tone = if self.stopping {
            Tone::Warning
        } else {
            Tone::Plain
        };
        // A preview or a plan: the time so far after the step's name, and
        // the live figures under it; no bar, since the totals are unknown.
        let looking = self
            .looking
            .filter(|l| l.step == self.step && !self.stopping);
        let counts = match &looking {
            Some(l) => format!(
                " {} {}",
                theme::DOT,
                format_duration(l.elapsed_secs.saturating_mul(1000))
            ),
            None => counts,
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(phase.to_string(), theme::style(phase_tone)),
            Span::styled(counts, theme::style(Tone::Dim)),
        ]));
        if let Some(l) = &looking {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(spinner.to_string(), theme::style(Tone::Accent)),
                Span::styled(format!(" {}", looking_figures(l)), theme::style(Tone::Dim)),
            ]));
            if let Some(text) = looking_destination(l) {
                lines.push(Line::from(Span::styled(
                    format!("    {text}"),
                    theme::style(Tone::Dim),
                )));
            }
            return lines;
        }

        // The bar, or a spinner until the total is known.
        let bar_width = width.saturating_sub(36).clamp(10, 34);
        match &stats {
            Some(s) if s.total_bytes > 0 => {
                let fraction = s.bytes as f64 / s.total_bytes as f64;
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(theme::bar(fraction, bar_width), theme::style(Tone::Accent)),
                    Span::raw(format!(
                        " {:>3}%   {} / {}",
                        (fraction * 100.0).floor().clamp(0.0, 100.0) as u64,
                        format_bytes(s.bytes),
                        format_bytes(s.total_bytes)
                    )),
                ]));
            }
            _ => {
                let files = stats
                    .as_ref()
                    .map(|s| format!("{} files", prompt::thousands(s.checks + s.transfers)))
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(spinner.to_string(), theme::style(Tone::Accent)),
                    Span::styled(format!(" {files}"), theme::style(Tone::Dim)),
                ]));
            }
        }
        if let Some(s) = &stats {
            let eta = s
                .eta
                .filter(|e| e.is_finite() && *e >= 0.0)
                .map(|e| format_duration((e * 1000.0) as u64))
                .unwrap_or_else(|| "-".into());
            let d = theme::DOT;
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "  {}/s {d} ETA {eta} {d} errors ",
                        format_bytes(s.speed.max(0.0) as u64)
                    ),
                    theme::style(Tone::Dim),
                ),
                Span::styled(
                    self.errors.max(s.errors).to_string(),
                    theme::style(if self.errors.max(s.errors) > 0 {
                        Tone::Error
                    } else {
                        Tone::Dim
                    }),
                ),
            ]));
        }
        lines.push(Line::default());

        // The last files, then the ones in flight.
        // Sizes line up in one column, as in the mockup, however wide the screen.
        let path_width = width.saturating_sub(16).clamp(20, 46);
        for r in &self.recent {
            let kind = self.dest.as_ref().map_or("local", |d| d.kind);
            lines.push(match r {
                Recent::Done(f) if f.change == Change::Deleted => {
                    let side = match f.side {
                        Where::Destination => place(kind),
                        Where::Local => crate::platform::words().this_computer,
                    };
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(theme::SKIPPED, theme::style(Tone::Warning)),
                        Span::raw(format!(" {}", fit(&f.path, path_width))),
                        Span::styled(format!(" deleted on {side}"), theme::style(Tone::Dim)),
                    ])
                }
                Recent::Done(f) => {
                    // Only the modification time moved: no bytes went across.
                    let size = f.size.map_or_else(|| "time".to_string(), format_bytes);
                    let (glyph, tone, from) = match f.side {
                        Where::Destination => (theme::DONE, Tone::Success, String::new()),
                        Where::Local => (
                            theme::DOWN,
                            Tone::Accent,
                            format!("   from {}", place(kind)),
                        ),
                    };
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(glyph, theme::style(tone)),
                        Span::raw(format!(" {}", fit(&f.path, path_width))),
                        Span::styled(format!(" {size:>9}{from}"), theme::style(Tone::Dim)),
                    ])
                }
                Recent::Failed(e) => Line::from(vec![
                    Span::raw("  "),
                    Span::styled(theme::FAILED, theme::style(Tone::Error)),
                    Span::raw(format!(" {}   ", e.path)),
                    Span::styled(e.reason.clone(), theme::style(Tone::Error)),
                ]),
            });
        }
        if let Some(s) = &self.stats {
            for f in &s.transferring {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(theme::UP, theme::style(Tone::Accent)),
                    Span::raw(format!(" {}", fit(&f.name, path_width))),
                    Span::styled(
                        format!(" {:>8}%", f.percentage.clamp(0.0, 100.0) as u64),
                        theme::style(Tone::Dim),
                    ),
                ]));
            }
        }
        lines
    }

    fn hints(&self) -> String {
        if self.stopping {
            "ctrl-c again to stop at once".to_string()
        } else {
            "ctrl-c stop".to_string()
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let area: Rect = frame.area().inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        let area = several::top(frame, area, self.batch.as_ref());
        if area.height < 3 {
            return;
        }
        let body = Rect {
            height: area.height - 2,
            ..area
        };
        let lines = self.lines(area.width);
        // Keep the newest lines in view.
        let scroll = (lines.len() as u16).saturating_sub(body.height);
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
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
    }
}

/// The live figures of a preview or a plan, on one line:
/// `checked 12,408 · to copy 1,204 files, 3.2 GB`. rclone's `listed` (every
/// entry on both sides, folders too) is left out: it read as a second,
/// larger count of the same files. The JSON `progress` event keeps it.
pub fn looking_figures(l: &Looking) -> String {
    let d = theme::DOT;
    if l.step == Step::Moving {
        return format!(
            "moved {} of {} {}",
            prompt::thousands(l.moved),
            prompt::thousands(l.to_move),
            if l.to_move == 1 { "file" } else { "files" }
        );
    }
    let mut parts = vec![format!("checked {}", prompt::thousands(l.checked))];
    parts.push(format!(
        "to copy {} {}, {}",
        prompt::thousands(l.to_copy),
        if l.to_copy == 1 { "file" } else { "files" },
        format_bytes(l.to_copy_bytes)
    ));
    if let Some(n) = l.checking_moves {
        parts.push(format!(
            "checking {} possible {}",
            prompt::thousands(n),
            if n == 1 { "move" } else { "moves" }
        ));
    }
    parts.join(&format!(" {d} "))
}

/// How far the one-way destination listing has got, or `None` for a
/// two-way plan: `destination 18,330 files listed so far`, `still listing
/// the destination · 48,120 files so far` once the dry run has finished
/// first, and `destination listed · 52,004 files` when it is done.
pub fn looking_destination(l: &Looking) -> Option<String> {
    let n = l.destination_listed?;
    let files = format!(
        "{} {}",
        prompt::thousands(n),
        if n == 1 { "file" } else { "files" }
    );
    Some(if l.destination_done {
        format!("destination listed {} {files}", theme::DOT)
    } else if l.still_listing() {
        format!(
            "still listing the destination {} {files} so far",
            theme::DOT
        )
    } else {
        format!("destination {files} listed so far")
    })
}

/// Pads or shortens a path to exactly `width` display columns, keeping its
/// end. Wide characters count two columns and combining marks none, so a
/// column after the path stays put for any name macOS hands back.
pub(crate) fn fit(path: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if super::width(path) <= width {
        return super::pad(path, width);
    }
    let keep = super::back(path, width.saturating_sub(1));
    super::pad(&format!("…{keep}"), width)
}

/// Messages from the runner to the screen thread.
enum Msg {
    Job(String, BatchLine),
    Start(DestinationInfo),
    Step(Step),
    Stats(Stats),
    Expected(Expected),
    Looking(Looking),
    File(FileDone),
    FileError(FileError),
    Stopping,
    Note(String, Tone),
    Phase(BisyncPhase),
    Ask(Box<PreviewPrompt>, mpsc::Sender<Answer>),
    Folders(Box<FolderPrompt>, mpsc::Sender<Vec<FolderAnswer>>),
    Close,
}

/// The question on screen, and where its answer goes.
enum Active {
    YesNo(Box<PreviewPrompt>, mpsc::Sender<Answer>),
    Folders(Box<FolderPrompt>, mpsc::Sender<Vec<FolderAnswer>>),
}

/// What is printed once the screen has closed.
enum After {
    Problem(String),
    Skipped(String),
    Preview(Box<Preview>),
    Text(String),
    Summary(Box<Summary>),
    /// The job's closing summary, for a job with more than one destination.
    Closing(String),
    /// The batch line as a job of a batch left it, before that job's part.
    Batch(BatchLine),
    /// A remark about the batch, such as a stale run lock removed.
    Remark(String),
}

/// The run screen as a [`Reporter`].
pub struct TuiReporter {
    tx: mpsc::Sender<Msg>,
    thread: Option<JoinHandle<Result<()>>>,
    after: Vec<After>,
    dry_run: bool,
    title: String,
    /// The current destination's type: `onedrive` or `local`.
    kind: &'static str,
    /// Where the batch has got to; `None` for one job.
    batch: Option<BatchLine>,
    /// Where the current job's part of `after` starts, in a batch.
    job_start: usize,
    /// A confirmation of the batch was answered `a`.
    yes_rest: bool,
}

impl TuiReporter {
    /// Opens the run screen on its own thread. Fails when the terminal
    /// cannot be taken over; the caller then falls back to plain output.
    pub fn start(job: &str, dry_run: bool) -> Result<TuiReporter> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(anyhow!("the run screen needs a terminal"));
        }
        let (tx, rx) = mpsc::channel::<Msg>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let model = ProgressModel::new(job);
        let title = format!("Run {job}");
        let thread = std::thread::Builder::new()
            .name("onefolder-run-screen".into())
            .spawn(move || screen(model, title, rx, ready_tx))
            .context("could not start the run screen")?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(TuiReporter {
                tx,
                thread: Some(thread),
                after: Vec::new(),
                dry_run,
                title: format!("Run {job}"),
                kind: "local",
                batch: None,
                job_start: 0,
                yes_rest: false,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(anyhow!(e))
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("the run screen stopped before it started"))
            }
        }
    }

    fn send(&self, msg: Msg) {
        // A screen that has gone (it failed, or panicked and restored the
        // terminal) must not stop the run.
        let _ = self.tx.send(msg);
    }

    /// Closes the screen, then prints the summaries and anything else worth
    /// keeping on the normal screen, and `closing` (the job's summary) last.
    /// Inside the interactive session the same text goes on the session's
    /// page to read on a screen, and the summary blocks and `closing` are
    /// also kept for the transcript printed when the session ends.
    pub fn finish(mut self, closing: Option<String>) -> Result<()> {
        if let Some(text) = closing {
            self.after.push(After::Closing(text));
        }
        self.close();
        super::out(&after_text(&self.after, theme::colour_on_stdout()))
    }

    /// The next job of a batch takes the screen: its title, and the batch
    /// line (with this job as the current one) on every screen until the
    /// next.
    pub fn begin_job(&mut self, job: &str, batch: &BatchLine) {
        self.title = format!("Run {job}");
        self.batch = Some(batch.clone());
        self.job_start = self.after.len();
        self.send(Msg::Job(job.to_string(), batch.clone()));
    }

    /// The job is over: its closing summary (for more than one
    /// destination) ends its part of the text printed afterwards, and the
    /// batch line as it now stands (with this job's end) starts it.
    pub fn end_job(&mut self, closing: Option<String>, batch: &BatchLine) {
        if let Some(text) = closing {
            self.after.push(After::Closing(text));
        }
        let at = self.job_start.min(self.after.len());
        self.after.insert(at, After::Batch(batch.clone()));
        self.batch = Some(batch.clone());
    }

    /// A job of the batch that does not run (skipped, or the batch was
    /// stopped): one line on the screen and in the text afterwards.
    pub fn job_not_run(&mut self, job: &str, end: &JobEnd, batch: &BatchLine) {
        self.begin_job(job, batch);
        let (glyph, tone) = end.glyph();
        let line = format!("{glyph} {}", end.words());
        self.send(Msg::Note(line.clone(), tone));
        self.after.push(After::Skipped(line));
        let mut after = batch.clone();
        if let Some(j) = after.jobs.get_mut(after.current) {
            j.1 = Some(end.clone());
        }
        self.end_job(None, &after);
    }

    /// A remark during a batch, such as a stale run lock removed: on the
    /// screen, since the shell's text is hidden under it, and in the
    /// current job's part of the text printed afterwards.
    pub fn remark(&mut self, text: &str) {
        let line = format!("{} {text}", theme::ACTION);
        self.send(Msg::Note(line.clone(), Tone::Dim));
        self.after.push(After::Remark(line));
    }

    /// Waits for a yes/no screen's answer. `a` (yes to the rest of the
    /// batch) is passed on as yes, and kept for [`Reporter::yes_to_rest`].
    fn ask(&mut self, screen: PreviewPrompt) -> Answer {
        let (tx, rx) = mpsc::channel();
        self.send(Msg::Ask(Box::new(screen), tx));
        match self.wait(rx, Answer::No) {
            Answer::YesToRest => {
                self.yes_rest = true;
                Answer::Yes
            }
            answer => answer,
        }
    }

    fn close(&mut self) {
        self.send(Msg::Close);
        if let Some(thread) = self.thread.take() {
            if let Ok(Err(e)) = thread.join() {
                super::err(&format!("{} {e:#}\n", theme::FAILED));
            }
        }
    }
}

/// The text printed once the screen has closed, in the order it was kept.
/// The summary blocks, problems, skipped lines, remarks and batch lines go
/// in the session's transcript too.
fn after_text(after: &[After], colour: bool) -> String {
    let mut out = String::new();
    for a in after {
        match a {
            After::Problem(m) => {
                let mut lines = m.lines();
                let mut text = String::new();
                if let Some(first) = lines.next() {
                    text.push_str(&format!(
                        "{} {first}\n",
                        theme::paint(theme::FAILED, Tone::Error, colour)
                    ));
                }
                for l in lines {
                    text.push_str(&format!("{l}\n"));
                }
                super::session::record(&text);
                out.push_str(&text);
            }
            After::Skipped(line) => {
                let text = format!("{}\n", theme::paint(line, Tone::Warning, colour));
                super::session::record(&text);
                out.push_str(&text);
            }
            After::Preview(p) => {
                out.push_str(&prompt::render_text(p, colour));
                out.push('\n');
            }
            After::Text(text) => {
                out.push_str(text);
                out.push('\n');
            }
            After::Summary(s) => {
                let text = block::render_text(s, colour, super::terminal_width());
                super::session::record(&format!("{text}\n"));
                out.push_str(&text);
                out.push('\n');
            }
            After::Closing(text) => {
                super::session::record(&format!("{text}\n"));
                out.push_str(text);
                out.push('\n');
            }
            After::Remark(line) => {
                let text = format!("{}\n", theme::paint(line, Tone::Dim, colour));
                super::session::record(&text);
                out.push_str(&text);
            }
            After::Batch(line) => {
                let text = line.text(colour, super::terminal_width());
                // One blank line before it, never two.
                let text = if out.is_empty() || out.ends_with("\n\n") {
                    text
                } else {
                    format!("\n{text}")
                };
                super::session::record(&text);
                out.push_str(&text);
            }
        }
    }
    out
}

impl Drop for TuiReporter {
    fn drop(&mut self) {
        self.close();
    }
}

impl TuiReporter {
    /// Waits for the screen's answer to a question.
    fn wait<T>(&self, rx: mpsc::Receiver<T>, fallback: T) -> T {
        // The screen thread answers; waiting blocks this thread, so tell
        // the runtime when it can move other work elsewhere.
        let wait = || rx.recv().unwrap_or(fallback);
        match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
            Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(wait),
            _ => wait(),
        }
    }
}

impl Reporter for TuiReporter {
    fn destination_started(&mut self, dest: &DestinationInfo) {
        self.kind = dest.kind;
        self.send(Msg::Start(dest.clone()));
    }

    fn phase(&mut self, phase: BisyncPhase) {
        self.send(Msg::Phase(phase));
    }

    fn plan(&mut self, plan: &Plan) {
        if self.dry_run {
            self.after.push(After::Text(prompt::render_plan_text(
                plan,
                theme::colour_on_stdout(),
            )));
        }
    }

    /// The merge explanation is always printed once the screen closes,
    /// also when `--yes` answered the question without showing it.
    fn resync(&mut self, offer: &ResyncOffer) {
        self.after.push(After::Text(prompt::render_resync_text(
            offer,
            theme::colour_on_stdout(),
        )));
        self.send(Msg::Note(
            "  Merging both sides (nothing is deleted by a merge).".to_string(),
            Tone::Dim,
        ));
    }

    fn confirm(&mut self, question: &Question) -> Answer {
        if let Question::Moves(p) = question {
            let screen = PreviewPrompt::moves(self.title.clone(), p).in_batch(self.batch.clone());
            return self.ask(screen);
        }
        // In a batch, `a` answers the plan and the merge offer, the
        // confirmations `--yes` skips; never the safety questions.
        let rest = self.batch.is_some() && prompt::offers_rest(question);
        let (mut body, q, default) = prompt::question(question, self.kind);
        let mut full = Vec::new();
        let verb = match question {
            Question::Plan(p) => {
                let (lines, lists) = prompt::plan_screen(p);
                body.splice(0..0, lines);
                full = lists;
                "sync"
            }
            Question::Resync(o) => {
                body.splice(0..0, prompt::resync_lines(o));
                "merge"
            }
            Question::StaleLock { .. } => "remove it",
            Question::Retry(_) => "run again",
            Question::Moves(_) => "move",
        };
        let mut screen = PreviewPrompt::with_lines(self.title.clone(), body, q, default, verb)
            .with_full_lists(full)
            .in_batch(self.batch.clone());
        if rest {
            screen = screen.offer_rest();
        }
        self.ask(screen)
    }

    fn ask_folders(&mut self, kind: &str, folders: &[DeletedFolder]) -> Vec<FolderAnswer> {
        let mut screen = FolderPrompt::new(self.title.clone(), kind, folders.to_vec());
        screen.batch = self.batch.clone();
        let (tx, rx) = mpsc::channel();
        self.send(Msg::Folders(Box::new(screen), tx));
        self.wait(rx, vec![FolderAnswer::CancelRun; folders.len()])
    }

    fn moves_decided(&mut self, preview: &Preview, answer: MovesAnswer, by: DecidedBy) {
        self.send(Msg::Note(
            format!(
                "  {} {}",
                theme::ACTION,
                prompt::moves_decision(preview, answer, by)
            ),
            Tone::Dim,
        ));
    }

    fn folder_decided(&mut self, folder: &DeletedFolder, answer: FolderAnswer, by: DecidedBy) {
        self.send(Msg::Note(
            format!(
                "  {} {}: {}",
                theme::ACTION,
                crate::ui::quoted(&folder.path),
                prompt::folder_decision(answer, by, self.kind)
            ),
            Tone::Dim,
        ));
    }

    fn conflict(&mut self, conflict: &ConflictDone) {
        let kept = conflict
            .kept_as
            .as_deref()
            .map(|k| format!(", {} keeps its copy as {k}", place(self.kind)))
            .unwrap_or_default();
        self.send(Msg::Note(
            format!(
                "  {} {}: the local copy won{kept}",
                theme::SKIPPED,
                conflict.path
            ),
            Tone::Warning,
        ));
    }

    fn step(&mut self, step: Step) {
        self.send(Msg::Step(step));
    }

    fn stats(&mut self, stats: &Stats) {
        self.send(Msg::Stats(stats.clone()));
    }

    fn looking(&mut self, looking: &Looking) {
        self.send(Msg::Looking(*looking));
    }

    fn expected(&mut self, expected: &Expected) {
        self.send(Msg::Expected(*expected));
    }

    fn file(&mut self, file: &FileDone) {
        self.send(Msg::File(file.clone()));
    }

    fn file_error(&mut self, error: &FileError) {
        self.send(Msg::FileError(error.clone()));
    }

    fn stopping(&mut self) {
        self.send(Msg::Stopping);
    }

    fn preview(&mut self, preview: &Preview) {
        if self.dry_run {
            self.after.push(After::Preview(Box::new(preview.clone())));
        }
    }

    fn confirm_preview(&mut self, preview: &Preview) -> Answer {
        let mut screen =
            PreviewPrompt::new(self.title.clone(), preview.clone()).in_batch(self.batch.clone());
        if self.batch.is_some() {
            screen = screen.offer_rest();
        }
        self.ask(screen)
    }

    fn problem(&mut self, message: &str) {
        let first = message.lines().next().unwrap_or(message);
        self.send(Msg::Note(format!("{} {first}", theme::FAILED), Tone::Error));
        self.after.push(After::Problem(message.to_string()));
    }

    fn note(&mut self, message: &str) {
        self.send(Msg::Note(format!("  {message}"), Tone::Dim));
    }

    fn destination_skipped(&mut self, dest: &DestinationInfo, reason: &str) {
        let line = format!("{} {} {reason}", theme::SKIPPED, dest.display);
        self.send(Msg::Note(line.clone(), Tone::Warning));
        self.after.push(After::Skipped(line));
    }

    fn destination_finished(&mut self, summary: &Summary) {
        let (word, tone) = block::status_word(summary);
        let glyph = match tone {
            Tone::Success => theme::DONE,
            Tone::Error => theme::FAILED,
            _ => theme::SKIPPED,
        };
        self.send(Msg::Note(
            format!("{glyph} {}   {word}", summary.destination),
            tone,
        ));
        self.after.push(After::Summary(Box::new(summary.clone())));
    }

    fn yes_to_rest(&self) -> bool {
        self.yes_rest
    }
}

/// The screen thread: owns the terminal until told to close.
fn screen(
    mut model: ProgressModel,
    _title: String,
    rx: mpsc::Receiver<Msg>,
    ready: mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    // The session's terminal, or one of its own. Either way a signal that
    // ends OneFolder while this screen is up (one sent from outside during
    // the preview question) restores the terminal first.
    let mut terminal = match super::screen() {
        Ok(t) => t,
        Err(e) => {
            let _ = ready.send(Err(format!("could not start the run screen: {e:#}")));
            return Ok(());
        }
    };
    let _ = ready.send(Ok(()));
    let mut prompt: Option<Active> = None;
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let frame = Duration::from_millis(100);
    loop {
        // Messages first, all that are waiting.
        loop {
            match rx.try_recv() {
                Ok(Msg::Close) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                Ok(msg) => {
                    dirty = true;
                    match msg {
                        Msg::Job(job, batch) => model.next_job(&job, batch),
                        Msg::Start(d) => model.start(&d),
                        Msg::Step(s) => {
                            model.step = s;
                            model.bisync = None;
                            model.stats = None;
                            model.expected = None;
                            model.looking = None;
                        }
                        Msg::Phase(p) => model.bisync = Some(p),
                        Msg::Stats(s) => model.stats = Some(s),
                        Msg::Expected(e) => model.expected = Some(e),
                        Msg::Looking(l) => model.looking = Some(l),
                        Msg::File(f) => model.file(&f),
                        Msg::FileError(e) => model.file_error(&e),
                        Msg::Stopping => model.stopping = true,
                        Msg::Note(text, tone) => model.notes.push((text, tone)),
                        Msg::Ask(p, reply) => prompt = Some(Active::YesNo(p, reply)),
                        Msg::Folders(p, reply) => prompt = Some(Active::Folders(p, reply)),
                        Msg::Close => {}
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if dirty && last_draw.elapsed() >= frame {
            model.tick = model.tick.wrapping_add(1);
            terminal
                .draw(|f| match &prompt {
                    Some(Active::YesNo(p, _)) => p.render(f),
                    Some(Active::Folders(p, _)) => p.render(f),
                    None => model.render(f),
                })
                .context("could not draw the run screen")?;
            last_draw = Instant::now();
            dirty = false;
        }
        if event::poll(Duration::from_millis(50)).context("could not read a key")? {
            let KeyInput::Key(key) = event::read().context("could not read a key")? else {
                dirty = true;
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            dirty = true;
            let ctrl_c =
                key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
            if let Some(active) = &mut prompt {
                // Ctrl-C at the question stops the whole run, as in plain
                // mode: this destination is not copied and the rest are skipped.
                if ctrl_c {
                    reporter::request_stop();
                }
                let done = match active {
                    Active::YesNo(p, reply) => p.handle_key(key).map(|a| {
                        let _ = reply.send(a);
                    }),
                    Active::Folders(p, reply) => p.handle_key(key).map(|a| {
                        let _ = reply.send(a);
                    }),
                };
                if done.is_some() {
                    prompt = None;
                }
                continue;
            }
            if ctrl_c {
                model.stopping = true;
                reporter::request_stop();
            }
        } else if model.step != Step::Finishing {
            // Keep the spinner turning.
            dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Mode;
    use crate::rclone::events::InFlight;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The run from the design's mockup, as a sequence of events.
    fn mockup() -> ProgressModel {
        let mut m = ProgressModel::new("documents");
        m.start(&DestinationInfo {
            job: "documents".into(),
            id: "d-7f3a".into(),
            kind: "onedrive",
            display: "work:Backups/Documents".into(),
            mode: Mode::OneWay,
            index: 1,
            of: 2,
            dry_run: false,
        });
        m.step = Step::Copying;
        for (path, size) in [
            ("Projects/2026/brief.docx", 1_200_000),
            ("Projects/2026/budget.xlsx", 248_000),
        ] {
            m.file(&FileDone {
                path: path.into(),
                size: Some(size),
                change: Change::Added,
                side: Where::Destination,
            });
        }
        m.file_error(&FileError {
            path: "Projects/2024/old.mov".into(),
            reason: "OneDrive: file too large (250 GiB limit)".into(),
        });
        m.stats = Some(Stats {
            bytes: 312_400_000,
            total_bytes: 760_100_000,
            transfers: 42,
            total_transfers: 118,
            checks: 981,
            total_checks: 981,
            errors: 1,
            eta: Some(35.0),
            speed: 12_800_000.0,
            elapsed_time: 24.0,
            transferring: vec![InFlight {
                name: "Projects/2026/deck.pptx".into(),
                bytes: 6_200_000,
                size: 10_000_000,
                percentage: 62.0,
            }],
            ..Stats::default()
        });
        m
    }

    fn render(model: &ProgressModel, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 18)).unwrap();
        terminal.draw(|f| model.render(f)).unwrap();
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
        snapshot("progress_80", render(&mockup(), 80));
    }

    #[test]
    fn a_long_destination_is_shortened_with_an_ellipsis() {
        let mut m = mockup();
        if let Some(d) = m.dest.as_mut() {
            d.display =
                "/private/tmp/claude-501/-Users-bora-Apps-onefolder/56475fcb/scratchpad/m12pty/dst"
                    .into();
        }
        let text = render(&m, 80);
        let header = text
            .lines()
            .find(|l| l.contains("one-way"))
            .unwrap_or_default();
        assert!(
            header.contains('…') && header.contains("m12pty/dst"),
            "{text}"
        );
        assert!(header.contains("destination 1 of 2"), "{text}");
    }

    #[test]
    fn renders_at_120_columns() {
        snapshot("progress_120", render(&mockup(), 120));
    }

    #[test]
    fn stopping_and_listing() {
        let mut m = mockup();
        m.stopping = true;
        m.notes.push((
            "✓ /Volumes/Backup/Documents   complete".into(),
            Tone::Success,
        ));
        snapshot("progress_stopping_80", render(&m, 80));
        let mut m = mockup();
        m.stats = None;
        m.recent.clear();
        assert_eq!(m.phase(), "Listing");
        m.stats = Some(Stats {
            checks: 10,
            total_checks: 40,
            ..Stats::default()
        });
        assert_eq!(m.phase(), "Comparing");
        m.step = Step::Previewing;
        assert_eq!(m.phase(), "Previewing");
    }

    #[test]
    fn two_way_sync_screen() {
        let mut m = mockup();
        if let Some(d) = &mut m.dest {
            d.mode = Mode::TwoWay;
        }
        m.step = Step::Syncing;
        m.bisync = Some(BisyncPhase::Applying);
        m.file(&FileDone {
            path: "Shared/meeting-notes.md".into(),
            size: Some(14_000),
            change: Change::Added,
            side: Where::Local,
        });
        m.file(&FileDone {
            path: "Drafts/old.txt".into(),
            size: None,
            change: Change::Deleted,
            side: Where::Destination,
        });
        assert_eq!(m.phase(), "Transferring");
        snapshot("progress_two_way_80", render(&m, 80));
        snapshot("progress_two_way_120", render(&m, 120));
        m.stats = None;
        m.bisync = Some(BisyncPhase::Updating);
        assert_eq!(m.phase(), "Updating listings");
        m.step = Step::Planning;
        assert_eq!(m.phase(), "Planning");
    }

    /// A long preview: the time so far, the figures, and how far the
    /// destination listing has got; no bar.
    fn previewing() -> ProgressModel {
        let mut m = mockup();
        m.step = Step::Previewing;
        m.stats = None;
        m.recent.clear();
        let mut l = Looking::new(Step::Previewing);
        l.elapsed_secs = 134;
        l.checked = 12_408;
        l.listed = Some(31_220);
        l.to_copy = 1_204;
        l.to_copy_bytes = 3_200_000_000;
        l.destination_listed = Some(18_330);
        m.looking = Some(l);
        m
    }

    #[test]
    fn preview_shows_live_figures() {
        let m = previewing();
        snapshot("progress_previewing_80", render(&m, 80));
        // The dry run finished first: the listing is still going.
        let mut m = previewing();
        if let Some(l) = &mut m.looking {
            l.elapsed_secs = 1_203;
            l.dry_run_done = true;
            l.destination_listed = Some(48_120);
        }
        snapshot("progress_previewing_listing_80", render(&m, 80));
        // Two-way planning: no destination listing line.
        let mut m = previewing();
        m.step = Step::Planning;
        if let Some(l) = &mut m.looking {
            l.step = Step::Planning;
            l.destination_listed = None;
        }
        let text = render(&m, 80);
        assert!(text.contains("Planning · 2m 14s"), "{text}");
        assert!(text.contains("checked 12,408"), "{text}");
        assert!(!text.contains("destination 18"), "{text}");
        // Figures from another step are not shown.
        m.step = Step::Syncing;
        assert!(!render(&m, 80).contains("checked 12,408"));
    }

    #[test]
    fn moving_shows_files_moved() {
        let mut m = previewing();
        m.step = Step::Moving;
        let mut l = Looking::new(Step::Moving);
        l.moved = 12;
        l.to_move = 1_204;
        l.elapsed_secs = 3;
        m.looking = Some(l);
        let text = render(&m, 80);
        assert!(text.contains("Moving files · 3s"), "{text}");
        assert!(text.contains("moved 12 of 1,204 files"), "{text}");
        assert!(!text.contains("listed"), "{text}");
        l.to_move = 1;
        l.moved = 1;
        assert_eq!(looking_figures(&l), "moved 1 of 1 file");
        // Checking possible moves while previewing.
        let mut l = Looking::new(Step::Previewing);
        l.checking_moves = Some(12);
        assert_eq!(
            looking_figures(&l),
            "checked 0 · to copy 0 files, 0 B · checking 12 possible moves"
        );
        l.checking_moves = Some(1);
        assert!(looking_figures(&l).ends_with("checking 1 possible move"));
    }

    #[test]
    fn live_figure_wording() {
        let mut l = Looking::new(Step::Previewing);
        assert_eq!(looking_figures(&l), "checked 0 · to copy 0 files, 0 B");
        assert_eq!(looking_destination(&l), None);
        l.checked = 1_234_567;
        l.listed = Some(2_000);
        l.to_copy = 1;
        l.to_copy_bytes = 2_048;
        assert_eq!(
            looking_figures(&l),
            format!(
                "checked 1,234,567 · to copy 1 file, {}",
                format_bytes(2_048)
            )
        );
        l.destination_listed = Some(1);
        assert_eq!(
            looking_destination(&l).unwrap(),
            "destination 1 file listed so far"
        );
        l.dry_run_done = true;
        l.destination_listed = Some(48_120);
        assert!(l.still_listing());
        assert_eq!(
            looking_destination(&l).unwrap(),
            "still listing the destination · 48,120 files so far"
        );
        l.destination_done = true;
        assert!(!l.still_listing());
        assert_eq!(
            looking_destination(&l).unwrap(),
            "destination listed · 48,120 files"
        );
    }

    /// The copy's totals from the preview win over rclone's while rclone
    /// is still finding files, and counts get thousands separators.
    #[test]
    fn copy_totals_come_from_the_preview() {
        let mut m = mockup();
        m.recent.clear();
        m.stats = Some(Stats {
            checks: 2_726,
            total_checks: 2_726,
            ..Stats::default()
        });
        let text = render(&m, 80);
        assert!(
            text.contains("Comparing · 2,726 of 2,726 checked"),
            "{text}"
        );
        m.expected = Some(Expected {
            checks: 33_388,
            transfers: 20,
            bytes: 12_100_000,
        });
        let text = render(&m, 80);
        assert!(
            text.contains("Comparing · 2,726 of 33,388 checked"),
            "{text}"
        );
        // The bar is there before the first file goes.
        assert!(text.contains("0%   0 B / 12.1 MB"), "{text}");
        m.stats = Some(Stats {
            checks: 33_388,
            total_checks: 33_388,
            transfers: 8,
            total_transfers: 8,
            bytes: 6_050_000,
            total_bytes: 11_000_000,
            speed: 1_000_000.0,
            eta: Some(5.0),
            ..Stats::default()
        });
        let text = render(&m, 80);
        assert!(text.contains("Transferring · 8 of 20 files"), "{text}");
        assert!(text.contains("50%   6.0 MB / 12.1 MB"), "{text}");
        // The ETA counts from the larger total.
        assert!(text.contains("ETA 6s"), "{text}");
        // More than the preview said: rclone's figures, never `21 of 20`.
        m.stats = Some(Stats {
            transfers: 21,
            total_transfers: 21,
            bytes: 12_300_000,
            total_bytes: 12_300_000,
            ..Stats::default()
        });
        let text = render(&m, 80);
        assert!(text.contains("21 of 21 files"), "{text}");
        assert!(text.contains("12.3 MB / 12.3 MB"), "{text}");
        // Thousands in the file counts too.
        m.expected = None;
        m.stats = Some(Stats {
            checks: 31_220,
            transfers: 1_204,
            total_transfers: 31_220,
            bytes: 1,
            ..Stats::default()
        });
        let text = render(&m, 80);
        assert!(text.contains("1,204 of 31,220 files"), "{text}");
        assert!(text.contains("32,424 files"), "{text}");
    }

    #[test]
    fn keeps_the_last_eight_files() {
        let mut m = ProgressModel::new("x");
        for i in 0..20 {
            m.file(&FileDone {
                path: format!("f{i}"),
                size: Some(1),
                change: Change::Updated,
                side: Where::Destination,
            });
        }
        assert_eq!(m.recent.len(), RECENT);
        assert_eq!(
            m.recent.front(),
            Some(&Recent::Done(FileDone {
                path: "f12".into(),
                size: Some(1),
                change: Change::Updated,
                side: Where::Destination,
            }))
        );
    }

    #[test]
    fn long_paths_keep_their_end() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("a/very/long/name.txt", 10), "…/name.txt");
        assert_eq!(fit("a/very/long/name.txt", 10).chars().count(), 10);
        // Display columns, not characters: wide names and decomposed accents.
        let wide = fit("日本語のフォルダ/写真ファイル番号_1.jpg", 20);
        assert_eq!(crate::ui::width(&wide), 20, "{wide}");
        assert!(
            wide.starts_with('…') && wide.trim_end().ends_with("_1.jpg"),
            "{wide}"
        );
        // An odd column left after a wide character is padded.
        assert_eq!(crate::ui::width(&fit("日本語のフォルダ", 6)), 6);
        let nfd = fit("Cafe\u{301}/re\u{301}sume\u{301}.docx", 20);
        assert_eq!(crate::ui::width(&nfd), 20);
        assert!(nfd.starts_with("Cafe\u{301}/"));
        assert_eq!(fit("abc", 0), "");
    }

    /// A run screen with no thread behind it, for what is printed after.
    fn offline() -> TuiReporter {
        let (tx, _) = mpsc::channel();
        TuiReporter {
            tx,
            thread: None,
            after: Vec::new(),
            dry_run: false,
            title: "Run x".into(),
            kind: "local",
            batch: None,
            job_start: 0,
            yes_rest: false,
        }
    }

    /// Each job's part of the text after a batch starts with the batch
    /// line as the job left it; a remark (a stale run lock removed) stays
    /// in its job's part.
    #[test]
    fn a_batch_prints_each_jobs_part_under_its_line() {
        let mut line = BatchLine::new(&["documents".into(), "photos".into()]);
        let mut r = offline();
        r.begin_job("documents", &line);
        r.after.push(After::Text("documents block".into()));
        line.jobs[0].1 = Some(JobEnd::Ok);
        r.end_job(None, &line);
        line.current = 1;
        r.begin_job("photos", &line);
        r.remark("Removed a stale run lock left by process 99998, which is no longer running (x).");
        r.after.push(After::Text("photos block".into()));
        line.jobs[1].1 = Some(JobEnd::Ok);
        r.end_job(Some("photos closing".into()), &line);
        let text = after_text(&r.after, false);
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "Several jobs · job 1 of 2 · documents ✓ · photos next",
                "documents block",
                "",
                "Several jobs · job 2 of 2 · documents ✓ · photos ✓",
                "→ Removed a stale run lock left by process 99998, which is no longer running (x).",
                "photos block",
                "photos closing",
            ]
        );
    }

    /// A partial run says so in its title, cut with `…` when narrow.
    #[test]
    fn a_partial_runs_title() {
        let m = ProgressModel::new(&crate::ui::partial::title("records", 2, 3));
        let title = |width| {
            m.lines(width)[0]
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(title(80), "Run records · partial, 2 of 3 destinations");
        let narrow = title(30);
        assert!(crate::ui::width(&narrow) <= 30, "{narrow}");
        assert!(narrow.starts_with("Run records · partial") && narrow.ends_with('…'));
    }
}
