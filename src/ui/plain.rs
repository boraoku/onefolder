//! The plain-text [`Reporter`], for runs whose output is not a terminal:
//! one line per file, a stats line every few seconds, the preview, and the
//! summary block. Colour only when stdout is a terminal. In a batch of
//! jobs, `Job 2 of 3: records` comes before each job.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use crate::model::{FileError, Summary};
use crate::rclone::events::Stats;
use crate::sync::folders::LIST_PROMPT_OVER;
use crate::sync::oneway::Preview;
use crate::sync::plan::{Plan, ResyncOffer};
use crate::sync::reporter::{
    Answer, Change, ConflictDone, DecidedBy, DeletedFolder, DestinationInfo, Expected, FileDone,
    FolderAnswer, Looking, MovesAnswer, Question, Reporter, Step, Where,
};
use crate::sync::twoway::place;
use crate::ui::prompt;
use crate::ui::summary::{self as block, format_bytes, format_duration};
use crate::ui::theme::{self, Tone};

/// How often a stats line is printed.
pub const STATS_EVERY: Duration = Duration::from_secs(5);

/// How often a line of live figures is printed while a preview or a plan
/// runs, in milliseconds: often enough that a launchd log shows the run is
/// alive, rarely enough not to flood it. A quick preview prints none.
pub const LOOKING_EVERY_MS: u64 = 10_000;

/// How early a line of live figures may come and still be printed (the
/// figures arrive about once a second, give or take a few milliseconds).
pub const LOOKING_EARLY_MS: u64 = 100;

/// Writes plain lines to a sink (stdout in use, a buffer in tests).
pub struct PlainReporter<W: Write> {
    out: W,
    colour: bool,
    job: String,
    titled: bool,
    last_stats: Option<Instant>,
    /// The elapsed time (ms) at which the next line of live figures is due.
    next_looking: u64,
    /// The totals the copy or sync should end at, from its preview or plan.
    expected: Option<Expected>,
    /// The current destination's type: `onedrive` or `local`.
    kind: &'static str,
    /// The terminal's width, for the summary's rules; `None` for a pipe.
    width: Option<usize>,
    /// The output so far ends with a blank line.
    blank: bool,
    /// The output so far ends with a newline.
    newline: bool,
    /// The question about moved files was asked for this destination, so
    /// the confirmation first says what the answer changed.
    moves_asked: bool,
    /// Part of a batch of jobs: the preview and plan confirmations offer
    /// `a`, yes to the rest of the batch.
    batch: bool,
    /// A confirmation of the batch was answered `a`.
    yes_rest: bool,
    /// Where answers are read from: stdin, or a script in tests.
    input: Option<Box<dyn BufRead + Send>>,
}

impl PlainReporter<std::io::Stdout> {
    pub fn stdout(job: &str) -> Self {
        let mut r = PlainReporter::new(std::io::stdout(), job, theme::colour_on_stdout());
        r.width = super::terminal_width();
        r
    }
}

impl<W: Write> PlainReporter<W> {
    pub fn new(out: W, job: &str, colour: bool) -> Self {
        PlainReporter {
            out,
            colour,
            job: job.to_string(),
            titled: false,
            last_stats: None,
            next_looking: LOOKING_EVERY_MS,
            expected: None,
            kind: "local",
            width: None,
            blank: true,
            newline: true,
            moves_asked: false,
            batch: false,
            yes_rest: false,
            input: None,
        }
    }

    /// Reads answers from `input` instead of stdin.
    #[cfg(test)]
    pub fn answering(mut self, input: &str) -> Self {
        self.input = Some(Box::new(std::io::Cursor::new(input.as_bytes().to_vec())));
        self
    }

    /// The next job of a batch: `Job 2 of 3: records`, after one blank
    /// line (never two), then the job's output as a run of it alone.
    pub fn begin_job(&mut self, index: usize, of: usize, job: &str) {
        self.batch = true;
        self.job = job.to_string();
        self.titled = false;
        let gap = if self.blank { "" } else { "\n" };
        let heading = self.p(&crate::ui::batch::job_heading(index, of, job), Tone::Bold);
        self.write(&format!("{gap}{heading}\n"));
    }

    /// A job of the batch that does not run, under its heading:
    /// `◎ skipped · another run in progress`.
    pub fn job_not_run(&mut self, end: &crate::ui::batch::JobEnd) {
        let line = crate::ui::batch::not_run_line(end, self.colour);
        self.write(&line);
    }

    /// Asks a question that a batch may answer with `a`.
    fn ask(&mut self, question: &str, default: Answer, rest: bool) -> Answer {
        let q = if rest {
            format!(
                "{}{}",
                self.p(&prompt::rest_question(question), Tone::Accent),
                self.p(&format!("   {} ", prompt::REST_NOTE), Tone::Dim)
            )
        } else {
            self.p(question, Tone::Accent)
        };
        self.write(&q);
        let answer = match self.read_line() {
            None => Answer::No,
            Some(line) if rest => prompt::parse_rest_answer(&line, default),
            Some(line) => prompt::parse_answer_or(&line, default),
        };
        if answer == Answer::YesToRest {
            self.yes_rest = true;
            return Answer::Yes;
        }
        answer
    }

    /// Writes the job's closing summary, after one blank line (never two).
    pub fn closing(&mut self, text: &str) {
        let gap = if self.blank { "" } else { "\n" };
        self.write(&format!("{gap}{text}"));
    }

    #[cfg(test)]
    pub fn into_inner(self) -> W {
        self.out
    }

    fn p(&self, text: &str, tone: Tone) -> String {
        theme::paint(text, tone, self.colour)
    }

    /// Writes text; a closed pipe or a full disk must not stop the run.
    fn write(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.blank = text.ends_with("\n\n")
            || (self.newline && text.starts_with('\n') && text.trim_start_matches('\n').is_empty());
        self.newline = text.ends_with('\n');
        let _ = self.out.write_all(text.as_bytes());
        let _ = self.out.flush();
    }

    /// Reads a line from stdin; `None` at end of input.
    fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        let got = match &mut self.input {
            Some(input) => input.read_line(&mut line),
            None => {
                let mut read = || std::io::stdin().lock().read_line(&mut line);
                match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
                    Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
                        tokio::task::block_in_place(read)
                    }
                    _ => read(),
                }
            }
        };
        match got {
            Ok(0) | Err(_) => {
                self.write("\n");
                None
            }
            Ok(_) => Some(line),
        }
    }

    /// Writes text without its first line (the header that
    /// `destination_started` already printed).
    fn body(&mut self, text: &str) {
        let rest = text.split_once('\n').map_or("", |(_, r)| r).to_string();
        self.write(&rest);
    }

    fn title(&mut self) {
        if !self.titled {
            self.titled = true;
            let t = self.p(&format!("Run {}", self.job), Tone::Bold);
            self.write(&format!("{t}\n\n"));
        }
    }
}

/// `Previewing 2m 10s · checked 12,408 · to copy 1,204 files, 3.2 GB ·
/// destination 18,330 files listed so far`.
pub fn looking_line(l: &Looking) -> String {
    let d = theme::DOT;
    let step = match l.step {
        Step::Planning => "Planning",
        Step::Moving => "Moving files",
        _ => "Previewing",
    };
    let mut line = format!(
        "{step} {} {d} {}",
        format_duration(l.elapsed_secs.saturating_mul(1000)),
        super::progress::looking_figures(l)
    );
    if let Some(text) = super::progress::looking_destination(l) {
        line.push_str(&format!(" {d} {text}"));
    }
    line
}

/// `41% · 312.4 MB / 760.1 MB · 12.8 MB/s · ETA 35s · 42 of 118 files`.
pub fn stats_line(s: &Stats) -> String {
    let d = theme::DOT;
    let mut parts = Vec::new();
    if s.total_bytes > 0 {
        let pct = (s.bytes as f64 / s.total_bytes as f64 * 100.0)
            .floor()
            .clamp(0.0, 100.0);
        parts.push(format!(
            "{pct}% {d} {} / {}",
            format_bytes(s.bytes),
            format_bytes(s.total_bytes)
        ));
    }
    parts.push(format!("{}/s", format_bytes(s.speed.max(0.0) as u64)));
    if let Some(eta) = s.eta.filter(|e| e.is_finite() && *e >= 0.0) {
        parts.push(format!("ETA {}", format_duration((eta * 1000.0) as u64)));
    }
    parts.push(format!(
        "{} of {} files",
        prompt::thousands(s.transfers),
        prompt::thousands(s.total_transfers)
    ));
    if s.errors > 0 {
        parts.push(format!("errors {}", prompt::thousands(s.errors)));
    }
    parts.join(&format!(" {d} "))
}

impl<W: Write> Reporter for PlainReporter<W> {
    fn destination_started(&mut self, dest: &DestinationInfo) {
        self.title();
        self.kind = dest.kind;
        let mut line = format!(
            "{} {}   {}",
            self.p(theme::ACTION, Tone::Accent),
            self.p(&dest.display, Tone::Bold),
            dest.mode.as_str()
        );
        if dest.of > 1 {
            line.push_str(&self.p(
                &format!("   destination {} of {}", dest.index, dest.of),
                Tone::Dim,
            ));
        }
        if dest.dry_run {
            line.push_str(&self.p("   dry run", Tone::Warning));
        }
        self.write(&format!("{line}\n"));
        self.last_stats = None;
        self.expected = None;
        self.moves_asked = false;
    }

    fn step(&mut self, step: Step) {
        self.expected = None;
        let text = match step {
            Step::Previewing => "  Previewing...",
            Step::Moving => "  Moving files...",
            Step::Copying => "  Copying...",
            Step::Planning => "  Planning...",
            Step::Restoring => "  Restoring...",
            Step::Syncing => "  Syncing...",
            Step::Resyncing => "  Merging...",
            Step::Checking | Step::Finishing => return,
        };
        self.next_looking = LOOKING_EVERY_MS;
        let t = self.p(text, Tone::Dim);
        self.write(&format!("{t}\n"));
    }

    fn stats(&mut self, stats: &Stats) {
        let due = self.last_stats.is_none_or(|t| t.elapsed() >= STATS_EVERY);
        if !due {
            return;
        }
        self.last_stats = Some(Instant::now());
        let shown = self.expected.map(|e| e.raise(stats));
        let line = stats_line(shown.as_ref().unwrap_or(stats));
        let t = self.p(&format!("  {line}"), Tone::Dim);
        self.write(&format!("{t}\n"));
    }

    fn looking(&mut self, looking: &Looking) {
        if looking.elapsed_ms + LOOKING_EARLY_MS < self.next_looking {
            return;
        }
        self.next_looking = looking.elapsed_ms + LOOKING_EVERY_MS;
        let t = self.p(&format!("  {}", looking_line(looking)), Tone::Dim);
        self.write(&format!("{t}\n"));
    }

    fn expected(&mut self, expected: &Expected) {
        self.expected = Some(*expected);
    }

    fn file(&mut self, file: &FileDone) {
        let side = match file.side {
            Where::Destination => place(self.kind),
            Where::Local => crate::platform::words().this_computer,
        };
        let what = match (file.change, file.size) {
            (Change::Deleted, _) => format!("deleted on {side}"),
            (_, None) => "time updated".to_string(),
            (Change::Added, Some(b)) => format!("{} added", format_bytes(b)),
            (Change::Updated, Some(b)) => format!("{} updated", format_bytes(b)),
        };
        let what = if file.side == Where::Local && file.change != Change::Deleted {
            format!("{what} on {}", crate::platform::words().this_computer)
        } else {
            what
        };
        let glyph = if file.side == Where::Local && file.change != Change::Deleted {
            self.p(theme::DOWN, Tone::Accent)
        } else {
            self.p(theme::DONE, Tone::Success)
        };
        let line = format!("  {glyph} {}   {}\n", file.path, self.p(&what, Tone::Dim));
        self.write(&line);
    }

    fn file_error(&mut self, error: &FileError) {
        let line = format!(
            "  {} {}   {}\n",
            self.p(theme::FAILED, Tone::Error),
            error.path,
            error.reason
        );
        self.write(&line);
    }

    fn stopping(&mut self) {
        let t = self.p(
            "  Stopping rclone... press Ctrl-C again to stop at once",
            Tone::Warning,
        );
        self.write(&format!("{t}\n"));
    }

    fn preview(&mut self, preview: &Preview) {
        let mut text = prompt::render_text(preview, self.colour);
        // The header line is already printed by destination_started.
        if let Some((_, rest)) = text.split_once('\n') {
            text = rest.to_string();
        }
        self.write(&text);
    }

    fn confirm_preview(&mut self, preview: &Preview) -> Answer {
        if self.moves_asked && !preview.moves.is_empty() {
            let text = prompt::lines_text(&prompt::moves_answer_lines(preview), self.colour);
            self.write(&format!("\n{text}"));
        }
        self.ask(prompt::QUESTION, Answer::Yes, self.batch)
    }

    fn plan(&mut self, plan: &Plan) {
        let text = prompt::render_plan_text(plan, self.colour);
        self.body(&text);
    }

    fn resync(&mut self, offer: &ResyncOffer) {
        let text = prompt::render_resync_text(offer, self.colour);
        self.body(&text);
    }

    fn confirm(&mut self, question: &Question) -> Answer {
        if matches!(question, Question::Moves(_)) {
            self.moves_asked = true;
        }
        let (lines, q, default) = prompt::question(question, self.kind);
        let text = prompt::lines_text(&lines, self.colour);
        self.write(&text);
        // In a batch, `a` answers the plan and the merge offer, the
        // confirmations `--yes` skips; never the safety questions.
        let rest = self.batch && prompt::offers_rest(question);
        self.ask(&q, default, rest)
    }

    /// Said only when not asked: an answer typed at the question needs no
    /// repeating.
    fn moves_decided(&mut self, preview: &Preview, answer: MovesAnswer, by: DecidedBy) {
        if by == DecidedBy::Asked {
            return;
        }
        let text = self.p(
            &format!(
                "  {} {}",
                theme::ACTION,
                prompt::moves_decision(preview, answer, by)
            ),
            Tone::Dim,
        );
        self.write(&format!("{text}\n"));
    }

    fn ask_folders(&mut self, kind: &str, folders: &[DeletedFolder]) -> Vec<FolderAnswer> {
        let labels = prompt::choice_labels(kind);
        let keys = ["d", "k", "r"];
        if folders.len() <= LIST_PROMPT_OVER {
            let mut answers = Vec::new();
            for (i, f) in folders.iter().enumerate() {
                let mut text = prompt::lines_text(
                    &prompt::folder_lines(kind, f, i + 1, folders.len()),
                    self.colour,
                );
                for (k, l) in keys.iter().zip(&labels) {
                    text.push_str(&format!("    {k}  {l}\n"));
                }
                self.write(&format!("\n{text}"));
                let answer = loop {
                    let q = self.p("  Answer [d/k/r, enter keeps]: ", Tone::Accent);
                    self.write(&q);
                    match self.read_line() {
                        None => break FolderAnswer::Keep,
                        Some(line) => {
                            if let Some(a) = prompt::parse_folder_answer(&line) {
                                break a;
                            }
                        }
                    }
                };
                answers.push(answer);
            }
            return answers;
        }
        let place = place(kind);
        let mut text = format!(
            "\n{} {} folders deleted locally\n\n  Not on {} any more. What should happen to them on {place}?\n\n",
            self.p(theme::CURSOR, Tone::Accent),
            folders.len(),
            crate::platform::words().your_computer
        );
        for (i, f) in folders.iter().enumerate() {
            text.push_str(&format!(
                "  {:>3}. {}   {} files, {}\n",
                i + 1,
                f.path,
                prompt::thousands(f.files),
                format_bytes(f.bytes)
            ));
        }
        for (k, l) in keys.iter().zip(&labels) {
            text.push_str(&format!(
                "    {k}  {}\n",
                l.replace(" it ", " them ")
                    .replace("this folder", "these folders")
            ));
        }
        self.write(&text);
        let all = loop {
            let q = self.p("  Answer for all [d/k/r, enter keeps]: ", Tone::Accent);
            self.write(&q);
            match self.read_line() {
                None => break FolderAnswer::Keep,
                Some(line) => {
                    if let Some(a) = prompt::parse_folder_answer(&line) {
                        break a;
                    }
                }
            }
        };
        let mut answers = vec![all; folders.len()];
        loop {
            let q = self.p(
                "  Any folder different? Type its number and d, k or r, such as 3k 7r (enter for none): ",
                Tone::Accent,
            );
            self.write(&q);
            let Some(line) = self.read_line() else { break };
            match prompt::parse_overrides(&line, folders.len()) {
                Some(list) => {
                    for (i, a) in list {
                        answers[i] = a;
                    }
                    break;
                }
                None => continue,
            }
        }
        answers
    }

    fn folder_decided(&mut self, folder: &DeletedFolder, answer: FolderAnswer, by: DecidedBy) {
        let _ = (folder, answer, by);
    }

    fn conflict(&mut self, conflict: &ConflictDone) {
        let kept = conflict
            .kept_as
            .as_deref()
            .map(|k| format!("; {} keeps its copy as {k}", place(self.kind)))
            .unwrap_or_default();
        let line = format!(
            "  {} {}   {}\n",
            self.p(theme::SKIPPED, Tone::Warning),
            conflict.path,
            self.p(
                &format!("changed on both sides, the local copy won{kept}"),
                Tone::Dim
            )
        );
        self.write(&line);
    }

    fn problem(&mut self, message: &str) {
        let mut lines = message.lines();
        let mut text = String::new();
        if let Some(first) = lines.next() {
            text.push_str(&format!("{} {first}\n", self.p(theme::FAILED, Tone::Error)));
        }
        for l in lines {
            text.push_str(&format!("{l}\n"));
        }
        self.write(&text);
    }

    fn note(&mut self, message: &str) {
        self.write(&format!("  {message}\n"));
    }

    fn destination_skipped(&mut self, dest: &DestinationInfo, reason: &str) {
        self.title();
        let line = format!(
            "{} {} {reason}\n",
            self.p(theme::SKIPPED, Tone::Warning),
            dest.display
        );
        self.write(&line);
    }

    fn destination_finished(&mut self, summary: &Summary) {
        let text = block::render_text(summary, self.colour, self.width);
        self.write(&format!("\n{text}\n"));
    }

    fn yes_to_rest(&self) -> bool {
        self.yes_rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Mode;

    #[test]
    fn one_blank_line_before_the_closing_summary() {
        let mut r = PlainReporter::new(Vec::new(), "documents", false);
        r.write("block\n\n");
        r.closing("closing\n");
        r.write("◎ skipped\n");
        r.closing("closing\n");
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert_eq!(text, "block\n\nclosing\n◎ skipped\n\nclosing\n");
    }

    #[test]
    fn live_figures_every_ten_seconds() {
        let mut r = PlainReporter::new(Vec::new(), "documents", false);
        r.step(Step::Previewing);
        let mut l = Looking::new(Step::Previewing);
        l.checked = 500;
        l.listed = Some(1_200);
        l.to_copy = 3;
        l.to_copy_bytes = 3_000;
        l.destination_listed = Some(700);
        // A line a second for 25 seconds: one at 10 s and one at 20 s.
        // The figures come a little early or late, as they do.
        for secs in 0..=25u64 {
            let jitter = if secs % 2 == 0 { -30 } else { 40 };
            let ms = (secs * 1000) as i64 + jitter;
            l.set_elapsed(Duration::from_millis(ms.max(0) as u64));
            r.looking(&l);
        }
        // A late line (a stall) prints at once, then the next is 10 s on.
        l.set_elapsed(Duration::from_secs(47));
        r.looking(&l);
        l.set_elapsed(Duration::from_secs(52));
        r.looking(&l);
        l.dry_run_done = true;
        l.set_elapsed(Duration::from_secs(57));
        r.looking(&l);
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert_eq!(
            text,
            "  Previewing...\n\
             \x20 Previewing 10s · checked 500 · to copy 3 files, 3 KB · destination 700 files listed so far\n\
             \x20 Previewing 20s · checked 500 · to copy 3 files, 3 KB · destination 700 files listed so far\n\
             \x20 Previewing 47s · checked 500 · to copy 3 files, 3 KB · destination 700 files listed so far\n\
             \x20 Previewing 57s · checked 500 · to copy 3 files, 3 KB · still listing the destination · 700 files so far\n"
        );
        // A quick preview prints none, and a new step starts the count again.
        let mut r = PlainReporter::new(Vec::new(), "documents", false);
        r.step(Step::Planning);
        let mut l = Looking::new(Step::Planning);
        l.set_elapsed(Duration::from_secs(3));
        r.looking(&l);
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert_eq!(text, "  Planning...\n");
    }

    #[test]
    fn moving_files_in_plain_text() {
        use crate::sync::moves::MovesChoice;
        let mut r = PlainReporter::new(Vec::new(), "documents", false);
        let mut p = crate::ui::prompt::tests::moves_mockup();
        p.choose_moves(MovesChoice::Move);
        // Decided by the flag or the default: said once, under the preview.
        r.moves_decided(&p, MovesAnswer::Move, DecidedBy::Flag);
        // Typed at the question: not repeated.
        r.moves_decided(&p, MovesAnswer::Copy, DecidedBy::Asked);
        r.step(Step::Moving);
        let mut l = Looking::new(Step::Moving);
        l.to_move = 1209;
        l.moved = 412;
        l.set_elapsed(Duration::from_secs(10));
        r.looking(&l);
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert_eq!(
            text,
            "  → 1,209 moved files: will be moved on OneDrive, not uploaded again   (--moves)\n\
             \x20 Moving files...\n\
             \x20 Moving files 10s · moved 412 of 1,209 files\n"
        );
    }

    #[test]
    fn plain_lines() {
        let mut r = PlainReporter::new(Vec::new(), "documents", false);
        let dest = DestinationInfo {
            job: "documents".into(),
            id: "d-91c0".into(),
            kind: "local",
            display: "/Volumes/Backup/Documents".into(),
            mode: Mode::OneWay,
            index: 1,
            of: 2,
            dry_run: false,
        };
        r.destination_started(&dest);
        r.step(Step::Copying);
        r.stats(&Stats {
            bytes: 500,
            total_bytes: 1000,
            transfers: 1,
            total_transfers: 2,
            speed: 250.0,
            eta: Some(2.0),
            ..Stats::default()
        });
        // The next one comes too soon to print.
        r.stats(&Stats::default());
        r.file(&FileDone {
            path: "a.txt".into(),
            size: Some(1200),
            change: Change::Added,
            side: Where::Destination,
        });
        r.file(&FileDone {
            path: "b.txt".into(),
            size: None,
            change: Change::Updated,
            side: Where::Destination,
        });
        r.file_error(&FileError {
            path: "c.txt".into(),
            reason: "permission denied".into(),
        });
        r.stopping();
        r.problem("Backup drive not mounted\n  Plug it in.");
        r.destination_skipped(&dest, "two-way sync arrives in milestone 5");
        // The copy's totals from the preview: larger than rclone's so far,
        // so they win; with thousands separators.
        r.expected(&Expected {
            checks: 0,
            transfers: 1_204,
            bytes: 4_000,
        });
        r.last_stats = None;
        r.stats(&Stats {
            bytes: 1000,
            total_bytes: 1000,
            transfers: 3,
            total_transfers: 3,
            speed: 1000.0,
            ..Stats::default()
        });
        // A new step forgets them.
        r.step(Step::Copying);
        r.last_stats = None;
        r.stats(&Stats {
            transfers: 3,
            total_transfers: 3,
            ..Stats::default()
        });
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert_eq!(
            text,
            "Run documents\n\n\
             → /Volumes/Backup/Documents   one-way   destination 1 of 2\n\
             \x20 Copying...\n\
             \x20 50% · 500 B / 1 KB · 250 B/s · ETA 2s · 1 of 2 files\n\
             \x20 ✓ a.txt   1 KB added\n\
             \x20 ✓ b.txt   time updated\n\
             \x20 ✗ c.txt   permission denied\n\
             \x20 Stopping rclone... press Ctrl-C again to stop at once\n\
             ✗ Backup drive not mounted\n\
             \x20 Plug it in.\n\
             ◎ /Volumes/Backup/Documents two-way sync arrives in milestone 5\n\
             \x20 25% · 1 KB / 4 KB · 1 KB/s · ETA 3s · 3 of 1,204 files\n\
             \x20 Copying...\n\
             \x20 0 B/s · 3 of 3 files\n"
        );
    }

    #[test]
    fn each_job_of_a_batch_has_its_heading() {
        use crate::ui::batch::JobEnd;
        let mut r = PlainReporter::new(Vec::new(), "documents", false);
        r.begin_job(1, 3, "documents");
        r.destination_started(&DestinationInfo {
            job: "documents".into(),
            id: "d-91c0".into(),
            kind: "local",
            display: "/Volumes/Backup/Documents".into(),
            mode: Mode::OneWay,
            index: 1,
            of: 1,
            dry_run: false,
        });
        r.closing("closing\n");
        r.begin_job(2, 3, "records");
        r.job_not_run(&JobEnd::Skipped("another run in progress".into()));
        r.begin_job(3, 3, "photos");
        r.job_not_run(&JobEnd::NotRun);
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert_eq!(
            text,
            "Job 1 of 3: documents\nRun documents\n\n→ /Volumes/Backup/Documents   one-way\n\
             \nclosing\n\nJob 2 of 3: records\n◎ skipped · another run in progress\n\
             \nJob 3 of 3: photos\n◎ not run, the batch was stopped\n"
        );
    }

    /// In a batch the preview asks `[Y/n/a]`; `a` is yes, and kept as yes
    /// to the rest. The moved-files question has no `a`.
    #[test]
    fn a_batch_question_takes_a() {
        let mut r = PlainReporter::new(Vec::new(), "documents", false).answering("a\nall\na\n");
        r.begin_job(1, 2, "documents");
        assert!(!r.yes_to_rest());
        assert_eq!(r.confirm_preview(&Preview::default()), Answer::Yes);
        assert!(r.yes_to_rest());
        // Not offered on the moved-files question: `a` is no there.
        let mut r2 = PlainReporter::new(Vec::new(), "documents", false).answering("a\n");
        r2.begin_job(1, 2, "documents");
        assert_eq!(
            r2.confirm(&Question::Moves(&Preview::default())),
            Answer::No
        );
        assert!(!r2.yes_to_rest());
        let text = String::from_utf8(r2.into_inner()).unwrap();
        assert!(!text.contains("[Y/n/a]"), "{text}");
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert!(
            text.ends_with(
                "  Continue? [Y/n/a]   n skips this destination · a yes to the rest of the batch "
            ),
            "{text}"
        );
        // A single job: `a` is no, and the question is as before.
        let mut r = PlainReporter::new(Vec::new(), "documents", false).answering("a\n");
        assert_eq!(r.confirm_preview(&Preview::default()), Answer::No);
        assert!(!r.yes_to_rest());
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert_eq!(text, prompt::QUESTION);
    }
}
