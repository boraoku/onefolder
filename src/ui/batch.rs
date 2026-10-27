//! Several jobs, one after another: the screens around a batch.
//!
//! - The first row of the Jobs list and the last row of `Run which job?`
//!   (`Several jobs, one after another…`) lead to [`all_or_some`], then,
//!   for some, to the selection screen [`ChooseJobs`], whose ticks are
//!   remembered in the state file (`batch_unticked`).
//! - [`BatchLine`] is the dim line at the very top of every screen of a
//!   batch: which job of how many, and how the jobs so far ended. Without
//!   a terminal the plain output prints [`job_heading`] before each job
//!   instead.
//! - [`closing_lines`] is the batch's closing summary: a line per job, and
//!   under it a line per destination.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::accounts::{Inline, Picker};
use super::summary::{format_duration, outcome_mark, DestinationLine};
use super::theme::{self, Tone};
use super::Segment;

fn seg(text: impl Into<String>, tone: Tone) -> Segment {
    (text.into(), tone)
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The first row of the Jobs list and the last of the `Run which job?` list.
pub const SEVERAL: &str = "Several jobs, one after another…";

/// The reason a job is not run once Ctrl-C has stopped the batch.
pub const STOPPED: &str = "not run, the batch was stopped";

/// How a job of a batch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    /// Every destination ok.
    Ok,
    /// Some went to plan and some did not.
    Partial,
    /// Every destination failed.
    Failed,
    /// Ctrl-C stopped it part way.
    Stopped,
    /// It did not start, with the reason: `another run in progress`.
    Skipped(String),
    /// Ctrl-C stopped the batch before its turn.
    NotRun,
}

impl JobEnd {
    /// The glyph and its tone.
    pub fn glyph(&self) -> (&'static str, Tone) {
        match self {
            JobEnd::Ok => (theme::DONE, Tone::Success),
            JobEnd::Failed => (theme::FAILED, Tone::Error),
            _ => (theme::SKIPPED, Tone::Warning),
        }
    }

    /// The words of the closing summary: `ok`, `skipped · another run in
    /// progress`, `not run, the batch was stopped`.
    pub fn words(&self) -> String {
        match self {
            JobEnd::Ok => "ok".to_string(),
            JobEnd::Partial => "partial".to_string(),
            JobEnd::Failed => "failed".to_string(),
            JobEnd::Stopped => "stopped".to_string(),
            JobEnd::Skipped(reason) => format!("skipped {} {reason}", theme::DOT),
            JobEnd::NotRun => STOPPED.to_string(),
        }
    }
}

/// Where a batch has got to: every job in the order they run, how each
/// finished one ended, and which one is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchLine {
    /// The job running now, from 0.
    pub current: usize,
    pub jobs: Vec<(String, Option<JobEnd>)>,
}

impl BatchLine {
    pub fn new(names: &[String]) -> Self {
        BatchLine {
            current: 0,
            jobs: names.iter().map(|n| (n.clone(), None)).collect(),
        }
    }

    /// `Several jobs · job 2 of 3 · documents ✓ · records · photos next`,
    /// dim but for the glyphs. On a narrow terminal it is cut from the
    /// right, whole job names first, so the job counter stays.
    pub fn segments(&self, width: Option<usize>) -> Vec<Segment> {
        let d = theme::DOT;
        let head = format!(
            "Several jobs {d} job {} of {}",
            self.current + 1,
            self.jobs.len()
        );
        let mut pieces: Vec<Vec<Segment>> = Vec::new();
        for (i, (name, end)) in self.jobs.iter().enumerate() {
            let piece = match end {
                Some(end) => {
                    let (glyph, tone) = end.glyph();
                    vec![seg(format!("{name} "), Tone::Dim), seg(glyph, tone)]
                }
                None if i == self.current + 1 => vec![seg(format!("{name} next"), Tone::Dim)],
                None => vec![seg(name.clone(), Tone::Dim)],
            };
            pieces.push(piece);
        }
        let piece_w = |p: &Vec<Segment>| p.iter().map(|(t, _)| super::width(t)).sum::<usize>();
        let sep = super::width(&format!(" {d} "));
        let total =
            |pieces: &[Vec<Segment>]| -> usize { pieces.iter().map(|p| sep + piece_w(p)).sum() };
        let mut cut = false;
        if let Some(max) = width {
            while !pieces.is_empty()
                && super::width(&head) + total(&pieces) + if cut { 2 } else { 0 } > max
            {
                pieces.pop();
                cut = true;
            }
            if super::width(&head) + if cut { 2 } else { 0 } > max {
                return vec![seg(super::fit_text(&head, max), Tone::Dim)];
            }
        }
        let mut line = vec![seg(head, Tone::Dim)];
        for piece in pieces {
            line.push(seg(format!(" {d} "), Tone::Dim));
            line.extend(piece);
        }
        if cut {
            line.push(seg(" …", Tone::Dim));
        }
        line
    }

    /// The line as text with its newline, painted when `colour` is set.
    pub fn text(&self, colour: bool, width: Option<usize>) -> String {
        let line: String = self
            .segments(width)
            .iter()
            .map(|(t, tone)| theme::paint(t, *tone, colour))
            .collect();
        format!("{line}\n")
    }
}

/// Draws the batch line in the first row of `area` and gives back the
/// area under it and the blank row after it, where the screen's own title
/// starts. Without a batch, `area` as it is.
pub fn top(frame: &mut Frame, area: Rect, batch: Option<&BatchLine>) -> Rect {
    let Some(batch) = batch else {
        return area;
    };
    if area.height < 5 {
        return area;
    }
    let line = Line::from(
        batch
            .segments(Some(usize::from(area.width)))
            .into_iter()
            .map(|(t, tone)| Span::styled(t, theme::style(tone)))
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(line), Rect { height: 1, ..area });
    Rect {
        y: area.y + 2,
        height: area.height - 2,
        ..area
    }
}

/// What the plain output prints before each job of a batch:
/// `Job 2 of 3: records`.
pub fn job_heading(index: usize, of: usize, job: &str) -> String {
    format!("Job {index} of {of}: {job}")
}

// ------------------------------------------------------ all or some

/// The screen after `Several jobs, one after another…`: all the jobs, or
/// a choice of them. Item 0 runs them all, item 1 opens [`ChooseJobs`].
pub fn all_or_some(jobs: usize) -> Picker {
    let mut picker = Picker::new(
        "Run several jobs",
        vec![(format!("All {jobs} jobs"), String::new())],
    );
    picker.extra = Some("Choose jobs…".to_string());
    picker.note = Some(format!(
        "{jobs} jobs, run one after another in the order of the Jobs list."
    ));
    picker.esc = "back";
    picker
}

// --------------------------------------------------------- choosing

/// One job on the selection screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobChoice {
    pub name: String,
    /// Already shortened with `~`.
    pub source: String,
    pub destinations: usize,
    /// What the Jobs list would warn about: `drive not mounted`, `needs
    /// resync`, `never run`.
    pub note: Option<Segment>,
}

/// The selection screen: tick the jobs to run, then Enter. Space ticks
/// and unticks, `a` ticks all, `n` none; Enter starts once one is ticked;
/// Esc goes back.
#[derive(Debug, Clone)]
pub struct ChooseJobs {
    pub jobs: Vec<JobChoice>,
    pub ticked: Vec<bool>,
    pub cursor: usize,
    pub width: Option<usize>,
}

impl ChooseJobs {
    /// Every job ticked but the ones unticked last time.
    pub fn new(jobs: Vec<JobChoice>, unticked: &[String]) -> Self {
        let ticked = jobs.iter().map(|j| !unticked.contains(&j.name)).collect();
        ChooseJobs {
            jobs,
            ticked,
            cursor: 0,
            width: None,
        }
    }

    /// The names ticked, in config order.
    pub fn chosen(&self) -> Vec<String> {
        self.jobs
            .iter()
            .zip(&self.ticked)
            .filter(|(_, t)| **t)
            .map(|(j, _)| j.name.clone())
            .collect()
    }

    /// `3 jobs · 5 destinations will run, one after another`.
    pub fn counts(&self) -> String {
        let jobs = self.ticked.iter().filter(|t| **t).count();
        if jobs == 0 {
            return format!("No job ticked {} space ticks one", theme::DOT);
        }
        let dests: usize = self
            .jobs
            .iter()
            .zip(&self.ticked)
            .filter(|(_, t)| **t)
            .map(|(j, _)| j.destinations)
            .sum();
        format!(
            "{} {} {} will run, one after another",
            plural(jobs, "job"),
            theme::DOT,
            plural(dests, "destination")
        )
    }

    /// Applies one key: the names ticked once started, `Some(None)` to go
    /// back (Esc, q, Ctrl-C).
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Option<Vec<String>>> {
        let last = self.jobs.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(None),
            KeyCode::Esc | KeyCode::Char('q') => Some(None),
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(last);
                None
            }
            KeyCode::Char(' ') => {
                if let Some(t) = self.ticked.get_mut(self.cursor) {
                    *t = !*t;
                }
                None
            }
            KeyCode::Char('a') => {
                self.ticked.iter_mut().for_each(|t| *t = true);
                None
            }
            KeyCode::Char('n') => {
                self.ticked.iter_mut().for_each(|t| *t = false);
                None
            }
            KeyCode::Enter if self.ticked.iter().any(|t| *t) => Some(Some(self.chosen())),
            _ => None,
        }
    }

    /// The screen's lines: the title, a row per job, the counts and the
    /// keys, each fitted to the width.
    pub fn segments(&self) -> Vec<Vec<Segment>> {
        let max = self.width.map(|w| w.saturating_sub(1));
        let name_w = self
            .jobs
            .iter()
            .map(|j| super::width(&j.name))
            .max()
            .unwrap_or(0)
            .max(12)
            + 3;
        let dests: Vec<String> = self
            .jobs
            .iter()
            .map(|j| plural(j.destinations, "destination"))
            .collect();
        let dests_w = dests.iter().map(|d| super::width(d)).max().unwrap_or(0);
        let note_w = self
            .jobs
            .iter()
            .filter_map(|j| j.note.as_ref())
            .map(|(n, _)| super::width(n) + 3)
            .max()
            .unwrap_or(0);
        // The source column gives way first, then the note is cut.
        let source_max = self.width.map(|w| {
            w.saturating_sub(8 + name_w + 3 + dests_w + note_w + 1)
                .clamp(10, 40)
        });
        let shown: Vec<String> = self
            .jobs
            .iter()
            .map(|j| match source_max {
                Some(m) => super::shorten_path(&j.source, m),
                None => j.source.clone(),
            })
            .collect();
        let source_w = shown.iter().map(|s| super::width(s)).max().unwrap_or(0) + 3;
        let mut lines = vec![
            vec![seg("  ", Tone::Plain), seg("Choose jobs", Tone::Bold)],
            Vec::new(),
        ];
        for (i, job) in self.jobs.iter().enumerate() {
            let here = i == self.cursor;
            let box_ = if self.ticked[i] { "[x]" } else { "[ ]" };
            let mut line = if here {
                vec![
                    seg("  ", Tone::Plain),
                    seg(theme::CURSOR, Tone::Accent),
                    seg(format!(" {box_} "), Tone::Accent),
                    seg(super::pad(&job.name, name_w), Tone::Accent),
                ]
            } else {
                vec![
                    seg(format!("    {box_} "), Tone::Plain),
                    seg(super::pad(&job.name, name_w), Tone::Bold),
                ]
            };
            line.push(seg(super::pad(&shown[i], source_w), Tone::Plain));
            line.push(seg(dests[i].clone(), Tone::Dim));
            if let Some((note, tone)) = &job.note {
                line.push(seg(format!(" {} ", theme::DOT), Tone::Dim));
                line.push(seg(note.clone(), *tone));
            }
            lines.push(cut(line, max));
        }
        lines.push(Vec::new());
        lines.push(cut(
            vec![seg(format!("  {}", self.counts()), Tone::Plain)],
            max,
        ));
        lines.push(Vec::new());
        lines.push(cut(
            vec![seg(
                format!(
                    "  {}{} move {d} space tick {d} a all {d} n none {d} enter start {d} esc back",
                    theme::UP,
                    theme::DOWN,
                    d = theme::DOT
                ),
                Tone::Dim,
            )],
            max,
        ));
        lines
    }
}

impl Inline for ChooseJobs {
    type Out = Vec<String>;
    fn set_width(&mut self, width: Option<usize>) {
        self.width = width;
    }
    fn segments(&self) -> Vec<Vec<Segment>> {
        ChooseJobs::segments(self)
    }
    fn key(&mut self, key: KeyEvent) -> Option<Option<Vec<String>>> {
        self.handle_key(key)
    }
    /// `  Choose jobs: documents, records`.
    fn answer_line(&self, out: &Vec<String>, colour: bool) -> String {
        format!(
            "  {} {}\n",
            theme::paint("Choose jobs:", Tone::Bold, colour),
            out.join(", ")
        )
    }
}

/// Cuts a line of segments to `max` columns, the last piece ending in `…`.
fn cut(line: Vec<Segment>, max: Option<usize>) -> Vec<Segment> {
    let Some(max) = max else {
        return line;
    };
    let total: usize = line.iter().map(|(t, _)| super::width(t)).sum();
    if total <= max {
        return line;
    }
    let mut out = Vec::new();
    let mut used = 0;
    for (text, tone) in line {
        let w = super::width(&text);
        if used + w <= max.saturating_sub(1) {
            used += w;
            out.push((text, tone));
            continue;
        }
        let room = max.saturating_sub(used);
        out.push((super::fit_text(&text, room), tone));
        break;
    }
    out
}

// ---------------------------------------------------------- closing

/// One job in the batch's closing summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLine {
    pub name: String,
    pub end: JobEnd,
    /// The destinations that ran (none for a job skipped or not run).
    pub destinations: Vec<DestinationLine>,
}

/// The batch's closing summary, after the jobs' own: a line per job with
/// how it ended, under it a line per destination with its outcome and
/// reason, framed by rules. `width` is the terminal's: long destinations
/// are shortened in the middle and long reasons go under their line.
pub fn closing_lines(
    jobs: &[JobLine],
    dry_run: bool,
    elapsed_ms: u64,
    width: Option<usize>,
) -> Vec<Vec<Segment>> {
    let d = theme::DOT;
    let dests: usize = jobs.iter().map(|j| j.destinations.len()).sum();
    let mut title = format!(
        "Several jobs   {} {d} {}",
        plural(jobs.len(), "job"),
        plural(dests, "destination")
    );
    if dry_run {
        title.push_str(&format!(" {d} dry run"));
    }
    title.push_str(&format!(" {d} {}", format_duration(elapsed_ms)));
    // Fitted like the rules: never wider than the terminal, never
    // narrower than the narrowest one OneFolder draws for.
    let title = match width {
        Some(w) => super::fit_text(&title, w.max(theme::MIN_WIDTH)),
        None => title,
    };
    let mut lines = vec![
        vec![seg(theme::rule_for(width), Tone::Dim)],
        vec![seg(title, Tone::Bold)],
    ];
    let name_w = jobs
        .iter()
        .map(|j| super::width(&j.name))
        .max()
        .unwrap_or(0)
        + 3;
    // Room for the indent, the mode and a short outcome word.
    let max = width.map(|w| w.saturating_sub(4 + 10 + 3 + 14).max(20));
    let shown = |display: &str| match max {
        Some(m) => super::shorten_path(display, m),
        None => display.to_string(),
    };
    let column = jobs
        .iter()
        .flat_map(|j| &j.destinations)
        .map(|dest| super::width(&shown(&dest.display)))
        .max()
        .unwrap_or(0)
        + 3;
    let reason = |lines: &mut Vec<Vec<Segment>>, mut line: Vec<Segment>, used, text: &str| {
        match width {
            // A reason that does not fit goes under the line, wrapped.
            Some(w) if used + 3 + super::width(text) > w => {
                lines.push(line);
                for piece in super::wrap(text, w.saturating_sub(8).max(20)) {
                    lines.push(vec![seg(format!("        {piece}"), Tone::Plain)]);
                }
            }
            _ => {
                line.push(seg(format!(" {d} {text}"), Tone::Plain));
                lines.push(line);
            }
        }
    };
    for job in jobs {
        let (glyph, tone) = job.end.glyph();
        let mut line = vec![
            seg(format!("  {}", super::pad(&job.name, name_w)), Tone::Bold),
            seg(glyph, tone),
        ];
        match &job.end {
            JobEnd::Skipped(why) => {
                line.push(seg(" skipped", tone));
                let used = 2 + name_w + 2 + 7;
                reason(&mut lines, line, used, why);
            }
            end => {
                line.push(seg(format!(" {}", end.words()), tone));
                lines.push(line);
            }
        }
        for dest in &job.destinations {
            let (glyph, word, tone) = outcome_mark(&dest.outcome);
            let line = vec![
                seg(
                    format!(
                        "    {}{:<10}",
                        super::pad(&shown(&dest.display), column),
                        dest.mode.as_str()
                    ),
                    Tone::Plain,
                ),
                seg(glyph, tone),
                seg(format!(" {word}"), tone),
            ];
            let used = 4 + column + 10 + 2 + super::width(word);
            // A reason that only repeats the word (`not confirmed`) is left out.
            match dest.reason.as_deref().filter(|r| *r != word) {
                Some(why) => reason(&mut lines, line, used, why),
                None => lines.push(line),
            }
        }
    }
    lines.push(vec![seg(theme::rule_for(width), Tone::Dim)]);
    lines
}

/// The closing summary as text, ending with a newline.
pub fn render_closing(
    jobs: &[JobLine],
    dry_run: bool,
    elapsed_ms: u64,
    colour: bool,
    width: Option<usize>,
) -> String {
    let mut out = String::new();
    for line in closing_lines(jobs, dry_run, elapsed_ms, width) {
        let text: String = line
            .iter()
            .map(|(text, tone)| theme::paint(text, *tone, colour))
            .collect();
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

/// The line for a job of a batch that did not run, under its heading:
/// `◎ skipped · another run in progress`, `◎ not run, the batch was
/// stopped`.
pub fn not_run_line(end: &JobEnd, colour: bool) -> String {
    let (glyph, tone) = end.glyph();
    format!(
        "{}\n",
        theme::paint(&format!("{glyph} {}", end.words()), tone, colour)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Mode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn snapshot(name: &str, value: String) {
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!(name, value);
        });
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(lines: &[Vec<Segment>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.iter()
                    .map(|(t, _)| t.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Job 2 of 3, the first one done.
    fn second_of_three() -> BatchLine {
        let mut line = BatchLine::new(&["documents".into(), "records".into(), "photos".into()]);
        line.jobs[0].1 = Some(JobEnd::Ok);
        line.current = 1;
        line
    }

    #[test]
    fn the_batch_line_says_where_the_batch_is() {
        let line = second_of_three();
        assert_eq!(
            line.text(false, None),
            "Several jobs · job 2 of 3 · documents ✓ · records · photos next\n"
        );
        // Dim but for the glyph.
        let segments = line.segments(None);
        assert!(segments
            .iter()
            .all(|(t, tone)| *tone == Tone::Dim || t == theme::DONE));
        assert!(segments.contains(&(theme::DONE.to_string(), Tone::Success)));
        // Every end has its glyph; only the one after the current is next.
        let mut line = BatchLine::new(&["a".into(), "b".into(), "c".into(), "d".into()]);
        line.jobs[0].1 = Some(JobEnd::Failed);
        line.jobs[1].1 = Some(JobEnd::Skipped("another run in progress".into()));
        line.current = 2;
        assert_eq!(
            line.text(false, None),
            "Several jobs · job 3 of 4 · a ✗ · b ◎ · c · d next\n"
        );
        line.current = 3;
        line.jobs[2].1 = Some(JobEnd::NotRun);
        assert_eq!(
            line.text(false, None),
            "Several jobs · job 4 of 4 · a ✗ · b ◎ · c ◎ · d\n"
        );
    }

    #[test]
    fn a_narrow_batch_line_loses_job_names_first() {
        let line = second_of_three();
        let at = |w| line.text(false, Some(w)).trim_end().to_string();
        assert_eq!(
            at(80),
            "Several jobs · job 2 of 3 · documents ✓ · records · photos next"
        );
        assert_eq!(
            at(51),
            "Several jobs · job 2 of 3 · documents ✓ · records …"
        );
        assert_eq!(at(50), "Several jobs · job 2 of 3 · documents ✓ …");
        assert_eq!(at(38), "Several jobs · job 2 of 3 …");
        assert_eq!(at(20), "Several jobs…");
        for w in [80, 60, 51, 50, 40, 38, 30, 20] {
            assert!(crate::ui::width(&at(w)) <= w, "{w}: {}", at(w));
        }
    }

    /// The run screen of job 2 of 3, with the batch line on top, at 80
    /// and 40 columns.
    #[test]
    fn the_batch_line_tops_the_run_screen() {
        use crate::sync::reporter::{DestinationInfo, Step};
        let mut m = crate::ui::progress::ProgressModel::new("x");
        m.next_job("records", second_of_three());
        m.start(&DestinationInfo {
            job: "records".into(),
            id: "d-0002".into(),
            kind: "onedrive",
            display: "boramektup:Records".into(),
            mode: Mode::OneWay,
            index: 1,
            of: 2,
            dry_run: false,
        });
        m.step = Step::Copying;
        let render = |width| {
            let mut t = Terminal::new(TestBackend::new(width, 16)).unwrap();
            t.draw(|f| m.render(f)).unwrap();
            t.backend().to_string()
        };
        snapshot("batch_progress_80", render(80));
        snapshot("batch_progress_40", render(40));
    }

    #[test]
    fn all_or_some_offers_every_job_or_a_choice() {
        let mut p = all_or_some(4);
        snapshot("batch_all_or_some", p.lines(false).join("\n"));
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            Some(super::super::accounts::Picked::Chosen(0))
        );
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            Some(super::super::accounts::Picked::Chosen(1))
        );
        // The answer line is left for All only; Choose leads on.
        assert_eq!(p.answer_line(0, false), "  Run several jobs: All 4 jobs\n");
        assert_eq!(p.answer_line(1, false), "");
        assert_eq!(
            p.handle_key(key(KeyCode::Esc)),
            Some(super::super::accounts::Picked::Cancelled)
        );
    }

    fn choices() -> Vec<JobChoice> {
        let job = |name: &str, source: &str, destinations, note: Option<(&str, Tone)>| JobChoice {
            name: name.into(),
            source: source.into(),
            destinations,
            note: note.map(|(t, tone)| (t.to_string(), tone)),
        };
        vec![
            job("documents", "~/Documents", 2, None),
            job("engineering", "~/Engineering", 2, None),
            job(
                "records",
                "~/Records",
                2,
                Some(("needs resync", Tone::Error)),
            ),
            job(
                "photos",
                "~/Pictures",
                1,
                Some(("drive not mounted", Tone::Warning)),
            ),
        ]
    }

    #[test]
    fn choosing_jobs_ticks_counts_and_remembers() {
        // Ticked as last time: engineering was left out; a name no longer
        // in the config is ignored, and a job not known then is ticked.
        let mut c = ChooseJobs::new(choices(), &["engineering".into(), "gone".into()]);
        assert_eq!(c.ticked, [true, false, true, true]);
        assert_eq!(
            c.counts(),
            "3 jobs · 5 destinations will run, one after another"
        );
        c.handle_key(key(KeyCode::Down));
        c.width = Some(80);
        snapshot("batch_choose_80", text(&c.segments()));
        c.width = Some(40);
        for line in c.segments() {
            let w: usize = line.iter().map(|(t, _)| crate::ui::width(t)).sum();
            assert!(w < 40, "{}", text(&[line]));
        }
        // Space ticks the row under the cursor.
        assert_eq!(c.handle_key(key(KeyCode::Char(' '))), None);
        assert_eq!(c.ticked, [true, true, true, true]);
        assert_eq!(c.handle_key(key(KeyCode::Char(' '))), None);
        assert_eq!(c.ticked, [true, false, true, true]);
        // n none: Enter does nothing until one is ticked.
        c.handle_key(key(KeyCode::Char('n')));
        assert_eq!(c.counts(), "No job ticked · space ticks one");
        assert_eq!(c.handle_key(key(KeyCode::Enter)), None);
        c.handle_key(key(KeyCode::Char('j')));
        c.handle_key(key(KeyCode::Char('j')));
        c.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            c.counts(),
            "1 job · 1 destination will run, one after another"
        );
        // a all, then Enter starts them in config order.
        c.handle_key(key(KeyCode::Char('a')));
        assert_eq!(
            c.handle_key(key(KeyCode::Enter)),
            Some(Some(vec![
                "documents".into(),
                "engineering".into(),
                "records".into(),
                "photos".into()
            ]))
        );
        assert_eq!(
            Inline::answer_line(&c, &vec!["documents".into(), "photos".into()], false),
            "  Choose jobs: documents, photos\n"
        );
        // Esc goes back; so do q and Ctrl-C.
        assert_eq!(c.handle_key(key(KeyCode::Esc)), Some(None));
        assert_eq!(c.handle_key(key(KeyCode::Char('q'))), Some(None));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(c.handle_key(ctrl_c), Some(None));
        // All ticked on first use.
        assert!(ChooseJobs::new(choices(), &[]).ticked.iter().all(|t| *t));
    }

    fn closing_jobs() -> Vec<JobLine> {
        let dest = |display: &str, mode, outcome: &str, reason: Option<&str>| DestinationLine {
            display: display.into(),
            mode,
            outcome: outcome.into(),
            reason: reason.map(String::from),
        };
        vec![
            JobLine {
                name: "documents".into(),
                end: JobEnd::Ok,
                destinations: vec![
                    dest("work:Backups/Documents", Mode::OneWay, "ok", None),
                    dest("/Volumes/T7 Shield/Docs", Mode::TwoWay, "ok", None),
                ],
            },
            JobLine {
                name: "records".into(),
                end: JobEnd::Partial,
                destinations: vec![
                    dest("boramektup:Records", Mode::OneWay, "ok", None),
                    dest(
                        "/Volumes/T7 Shield/Records",
                        Mode::OneWay,
                        "failed",
                        Some("T7 Shield drive not mounted"),
                    ),
                ],
            },
            JobLine {
                name: "engineering".into(),
                end: JobEnd::Skipped("another run in progress".into()),
                destinations: Vec::new(),
            },
            JobLine {
                name: "photos".into(),
                end: JobEnd::NotRun,
                destinations: Vec::new(),
            },
        ]
    }

    #[test]
    fn the_closing_summary_has_a_line_per_job_and_destination() {
        let jobs = closing_jobs();
        snapshot(
            "batch_closing",
            render_closing(&jobs, false, 1_120_000, false, None),
        );
        let narrow = render_closing(&jobs, true, 1_120_000, false, Some(60));
        snapshot("batch_closing_60", narrow.clone());
        assert!(
            narrow.lines().all(|l| crate::ui::width(l) <= 60),
            "{narrow}"
        );
        assert!(narrow.contains("4 jobs · 4 destinations · dry run · 18m 40s"));
        // The title is fitted like the rules, down to the narrowest width.
        let tight = render_closing(&jobs, true, 1_120_000, false, Some(40));
        let title = tight.lines().nth(1).unwrap();
        assert!(crate::ui::width(title) <= 40, "{tight}");
        assert!(title.starts_with("Several jobs   4 jobs"), "{tight}");
    }

    #[test]
    fn a_job_that_did_not_run_says_why() {
        assert_eq!(
            not_run_line(&JobEnd::Skipped("another run in progress".into()), false),
            "◎ skipped · another run in progress\n"
        );
        assert_eq!(
            not_run_line(&JobEnd::NotRun, false),
            "◎ not run, the batch was stopped\n"
        );
        assert_eq!(job_heading(2, 3, "records"), "Job 2 of 3: records");
    }
}
