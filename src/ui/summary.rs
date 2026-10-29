//! The bordered summary block printed after each destination runs.
//!
//! One function, [`summary_lines`], decides the text and the tone of every
//! piece. The plain renderer (used when stdout is not a terminal) and the
//! ratatui widget both draw from it, so they always say the same thing.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::model::{Mode, Outcome, Summary};
use crate::ui::prompt::thousands;
use crate::ui::theme::{self, Tone};

/// A run of text with one tone.
pub type Segment = (String, Tone);

fn seg(text: impl Into<String>, tone: Tone) -> Segment {
    (text.into(), tone)
}

/// The word shown after the mode on the title line.
pub fn status_word(summary: &Summary) -> (&'static str, Tone) {
    if summary.dry_run {
        return ("dry run", Tone::Warning);
    }
    match summary.outcome {
        Outcome::Ok => ("complete", Tone::Success),
        Outcome::Partial => ("partial", Tone::Warning),
        Outcome::Failed => ("failed", Tone::Error),
        Outcome::Cancelled => ("cancelled", Tone::Warning),
        Outcome::NeedsResync => ("needs resync", Tone::Error),
    }
}

/// The lines of the block, rules included. `width` is the terminal's:
/// the rules are then no wider than it (see [`theme::rule_for`]).
pub fn summary_lines(s: &Summary, width: Option<usize>) -> Vec<Vec<Segment>> {
    let remote = if s.destination_type == "onedrive" {
        "OneDrive"
    } else {
        "drive"
    };
    let mut lines = Vec::new();
    lines.push(vec![seg(theme::rule_for(width), Tone::Dim)]);

    let (word, tone) = status_word(s);
    lines.push(vec![
        seg(
            format!(
                "{} {} {}   {}   ",
                s.job,
                theme::ACTION,
                s.destination,
                s.mode.as_str()
            ),
            Tone::Bold,
        ),
        seg(word, tone),
    ]);

    let counts = match s.mode {
        Mode::TwoWay => {
            let w = crate::platform::words();
            format!(
                "Added: {} to {remote}, {} | Updated: {} | Deleted: {} on {remote}, {}",
                thousands(s.added.remote),
                w.to_local(thousands(s.added.local)),
                thousands(s.updated.total()),
                thousands(s.deleted.remote),
                w.on_local(thousands(s.deleted.local))
            )
        }
        Mode::OneWay => format!(
            "Added: {} | Updated: {} | Deleted: 0 (one-way never deletes)",
            thousands(s.added.total()),
            thousands(s.updated.total())
        ),
    };
    lines.push(vec![seg(counts, Tone::Plain)]);

    if let Some(m) = s.moved.filter(|m| m.files > 0) {
        lines.push(vec![seg(
            format!(
                "{}: {} {}, {} (on {}, no upload)",
                if s.dry_run { "Would move" } else { "Moved" },
                thousands(m.files),
                if m.files == 1 { "file" } else { "files" },
                format_bytes(m.bytes),
                if remote == "drive" {
                    "the drive"
                } else {
                    remote
                },
            ),
            Tone::Plain,
        )]);
    }

    let mut transfer = if s.dry_run {
        // Nothing moved: no time or rate to report.
        format!("Would transfer: {}", format_bytes(s.bytes))
    } else {
        format!(
            "Transferred: {} in {}",
            format_bytes(s.bytes),
            format_duration(s.duration_ms)
        )
    };
    if !s.dry_run && s.duration_ms >= 1000 && s.bytes > 0 {
        let rate = s.bytes as f64 / (s.duration_ms as f64 / 1000.0);
        transfer.push_str(&format!(" ({}/s)", format_bytes(rate.round() as u64)));
    }
    if let Some(unchanged) = s.unchanged {
        transfer.push_str(&format!(" | Unchanged: {}", thousands(unchanged)));
    }
    transfer.push_str(" | Errors: ");
    let error_tone = if s.errors.is_empty() {
        Tone::Plain
    } else {
        Tone::Error
    };
    lines.push(vec![
        seg(transfer, Tone::Plain),
        seg(thousands(s.errors.len() as u64), error_tone),
    ]);

    for e in &s.errors {
        // An error about the whole run (a drive not mounted, a missing
        // folder) has no path.
        let text = if e.path.is_empty() {
            format!(" {}", e.reason)
        } else {
            format!(" {}   {}", e.path, e.reason)
        };
        lines.push(vec![
            seg("  ", Tone::Plain),
            seg(theme::FAILED, Tone::Error),
            seg(text, Tone::Plain),
        ]);
    }

    if !s.conflicts.is_empty() {
        lines.push(vec![seg(
            format!(
                "Conflicts: {}, local copy kept, the {remote} copy renamed with a .conflict suffix",
                thousands(s.conflicts.len() as u64)
            ),
            Tone::Plain,
        )]);
        for c in &s.conflicts {
            lines.push(vec![
                seg("  ", Tone::Plain),
                seg(theme::SKIPPED, Tone::Warning),
                seg(format!(" {c}"), Tone::Plain),
            ]);
        }
    }

    if !s.removed_folders.is_empty() {
        lines.push(vec![seg(
            format!(
                "Removed on {}: {}, with the hidden and system files in {}",
                if remote == "drive" {
                    "the drive"
                } else {
                    remote
                },
                crate::ui::quoted_list(&s.removed_folders),
                if s.removed_folders.len() == 1 {
                    "it"
                } else {
                    "them"
                }
            ),
            Tone::Plain,
        )]);
    }

    if let Some(n) = s.destination_only.filter(|n| *n > 0) {
        let files = if n == 1 { "file" } else { "files" };
        lines.push(vec![seg(
            format!(
                "Left alone: {} {files} only on the destination (one-way does not mirror)",
                thousands(n)
            ),
            Tone::Dim,
        )]);
    }

    if let Some(log) = &s.log {
        lines.push(vec![seg(format!("Log: {log}"), Tone::Dim)]);
    }
    lines.push(vec![seg(theme::rule_for(width), Tone::Dim)]);
    lines
}

/// The block as text, one line per row, ending with a newline. With
/// `colour` set, tones become ANSI colours; `width` is the terminal's.
pub fn render_text(summary: &Summary, colour: bool, width: Option<usize>) -> String {
    let mut out = String::new();
    for line in summary_lines(summary, width) {
        for (text, tone) in line {
            out.push_str(&theme::paint(&text, tone, colour));
        }
        out.push('\n');
    }
    out
}

/// One destination's line in the job's closing summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationLine {
    /// As shown: `work:Backups/Documents` or a local path.
    pub display: String,
    pub mode: Mode,
    /// `ok`, `partial`, `failed`, `cancelled`, `needs-resync`, `skipped`
    /// or `declined`.
    pub outcome: String,
    /// Why it did not go to plan, when known.
    pub reason: Option<String>,
}

/// The glyph, word and tone of an outcome in the closing summary.
pub(crate) fn outcome_mark(outcome: &str) -> (&'static str, &'static str, Tone) {
    match outcome {
        "ok" => (theme::DONE, "ok", Tone::Success),
        "partial" => (theme::SKIPPED, "partial", Tone::Warning),
        "failed" => (theme::FAILED, "failed", Tone::Error),
        "cancelled" => (theme::SKIPPED, "cancelled", Tone::Warning),
        "needs-resync" => (theme::FAILED, "needs resync", Tone::Error),
        "declined" => (theme::SKIPPED, "not confirmed", Tone::Warning),
        super::partial::NOT_CHOSEN => (theme::DOT, "not chosen", Tone::Dim),
        _ => (theme::SKIPPED, "skipped", Tone::Warning),
    }
}

/// The job's closing summary, after every destination's block: one line
/// per destination, in the order they ran, with its outcome. In a partial
/// run the destinations left out ([`super::partial::NOT_CHOSEN`]) keep
/// their place, dimmed, and are not counted.
/// `width` is the terminal's: long destinations are then shortened in
/// the middle; `None` prints them in full.
pub fn job_summary_lines(
    job: &str,
    dry_run: bool,
    dests: &[DestinationLine],
    width: Option<usize>,
) -> Vec<Vec<Segment>> {
    let mut lines = vec![vec![seg(theme::rule_for(width), Tone::Dim)]];
    // Counted in a fixed order, good news first.
    let order = [
        "ok",
        "partial",
        "failed",
        "cancelled",
        "needs resync",
        "not confirmed",
        "skipped",
    ];
    let counts: Vec<(&str, usize)> = order
        .iter()
        .map(|w| {
            let n = dests
                .iter()
                .filter(|d| outcome_mark(&d.outcome).1 == *w)
                .count();
            (*w, n)
        })
        .filter(|(_, n)| *n > 0)
        .collect();
    let not_chosen = |d: &DestinationLine| d.outcome == super::partial::NOT_CHOSEN;
    let chosen = dests.iter().filter(|d| !not_chosen(d)).count();
    let partial = chosen < dests.len();
    let mut tail = String::new();
    if dry_run {
        tail.push_str(&format!(" {} dry run", theme::DOT));
    }
    for (word, n) in &counts {
        tail.push_str(&format!(" {} {n} {word}", theme::DOT));
    }
    let title = if partial {
        format!(
            "{job}   partial run {} {chosen} of {} destinations{tail}",
            theme::DOT,
            dests.len()
        )
    } else {
        format!(
            "{job}   {} destination{}{tail}",
            dests.len(),
            if dests.len() == 1 { "" } else { "s" }
        )
    };
    // Fitted like the rules: never wider than the terminal, never
    // narrower than the narrowest one OneFolder draws for. A partial
    // run's title gets shorter first (`records · 1 of 3 · dry run · 1
    // ok`), so its counts survive.
    let title = match width.map(|w| w.max(theme::MIN_WIDTH)) {
        Some(w) if partial && super::width(&title) > w => super::fit_text(
            &format!("{job} {} {chosen} of {}{tail}", theme::DOT, dests.len()),
            w,
        ),
        Some(w) => super::fit_text(&title, w),
        None => title,
    };
    lines.push(vec![seg(title, Tone::Bold)]);
    // Room for the indent, the glyph, the mode and a short outcome word.
    let max = width.map(|w| w.saturating_sub(4 + 3 + 10 + 14).max(20));
    let shown: Vec<String> = dests
        .iter()
        .map(|d| match max {
            Some(m) => super::shorten_path(&d.display, m),
            None => d.display.clone(),
        })
        .collect();
    let column = shown.iter().map(|s| super::width(s)).max().unwrap_or(0) + 3;
    for (d, display) in dests.iter().zip(&shown) {
        let (glyph, word, tone) = outcome_mark(&d.outcome);
        // A destination left out is dim all along its line.
        let plain = if not_chosen(d) {
            Tone::Dim
        } else {
            Tone::Plain
        };
        let mut line = vec![
            seg("  ", Tone::Plain),
            seg(glyph, tone),
            seg(
                format!(" {}{:<10}", super::pad(display, column), d.mode.as_str()),
                plain,
            ),
            seg(word, tone),
        ];
        let used = 4 + column + 10 + super::width(word);
        // A reason that only repeats the word (`not confirmed`) is left out.
        let reason = d.reason.as_deref().filter(|r| *r != word);
        match (reason, width) {
            // A reason that does not fit goes under the line, wrapped.
            (Some(reason), Some(w)) if used + 3 + super::width(reason) > w => {
                lines.push(line);
                for piece in super::wrap(reason, w.saturating_sub(6).max(20)) {
                    lines.push(vec![seg(format!("      {piece}"), plain)]);
                }
            }
            (Some(reason), _) => {
                line.push(seg(format!(" {} {reason}", theme::DOT), plain));
                lines.push(line);
            }
            (None, _) => lines.push(line),
        }
    }
    lines.push(vec![seg(theme::rule_for(width), Tone::Dim)]);
    lines
}

/// The closing summary as text, ending with a newline.
pub fn render_job_summary(
    job: &str,
    dry_run: bool,
    dests: &[DestinationLine],
    colour: bool,
    width: Option<usize>,
) -> String {
    let mut out = String::new();
    for line in job_summary_lines(job, dry_run, dests, width) {
        let text: String = line
            .iter()
            .map(|(text, tone)| theme::paint(text, *tone, colour))
            .collect();
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

/// The block as plain text with no colour.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "status prints its own lines; kept for the tests and a later status screen"
    )
)]
pub fn render_plain(summary: &Summary) -> String {
    render_text(summary, false, None)
}

/// The block as a ratatui widget. The run screen prints its summaries on
/// the normal screen once it closes, so they stay in the scrollback; the
/// widget is for the status screen.
pub struct SummaryBlock<'a>(pub &'a Summary);

impl SummaryBlock<'_> {
    pub fn lines(&self) -> Vec<Line<'static>> {
        summary_lines(self.0, None)
            .into_iter()
            .map(|segments| {
                Line::from(
                    segments
                        .into_iter()
                        .map(|(text, tone)| Span::styled(text, theme::style(tone)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    /// Rows the block needs.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "kept for a later status screen")
    )]
    pub fn height(&self) -> u16 {
        u16::try_from(summary_lines(self.0, None).len()).unwrap_or(u16::MAX)
    }
}

impl Widget for SummaryBlock<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(self.lines()).render(area, buf);
    }
}

/// Sizes in decimal units: `512 B`, `248 KB`, `312.4 MB`, `3.4 GB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    if bytes < 1_000_000 {
        return format!("{} KB", (bytes as f64 / 1000.0).round() as u64);
    }
    let mut value = bytes as f64 / 1_000_000.0;
    let mut unit = 0;
    while value >= 999.95 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Durations as `42s`, `1m 38s` or `2h 5m`.
pub fn format_duration(ms: u64) -> String {
    let secs = (ms + 500) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileError, SideCounts};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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

    /// The summary from the mockup in DESIGN.md.
    fn two_way() -> Summary {
        Summary {
            job: "documents".into(),
            destination: "work:Backups/Documents".into(),
            destination_type: "onedrive".into(),
            mode: Mode::TwoWay,
            outcome: Outcome::Ok,
            dry_run: false,
            added: SideCounts {
                local: 2,
                remote: 14,
            },
            updated: SideCounts {
                local: 0,
                remote: 3,
            },
            deleted: SideCounts {
                local: 1,
                remote: 0,
            },
            unchanged: None,
            bytes: 312_400_000,
            duration_ms: 98_000,
            errors: vec![FileError {
                path: "Projects/2024/old.mov".into(),
                reason: "OneDrive: file too large (250 GiB limit)".into(),
            }],
            conflicts: vec![],
            removed_folders: Vec::new(),
            moved: None,
            destination_only: None,
            log: Some(
                "~/.local/share/onefolder/logs/documents/d-7f3a/2026-10-13T094102.jsonl".into(),
            ),
        }
    }

    fn one_way() -> Summary {
        Summary {
            job: "documents".into(),
            destination: "/Volumes/Backup/Documents".into(),
            destination_type: "local".into(),
            mode: Mode::OneWay,
            outcome: Outcome::Ok,
            dry_run: true,
            added: SideCounts {
                local: 0,
                remote: 37,
            },
            updated: SideCounts {
                local: 0,
                remote: 5,
            },
            deleted: SideCounts::default(),
            unchanged: Some(981),
            bytes: 430_800_000,
            duration_ms: 62_000,
            errors: vec![],
            conflicts: vec![],
            removed_folders: Vec::new(),
            moved: None,
            destination_only: Some(12),
            log: Some(
                "~/.local/share/onefolder/logs/documents/d-91c0/2026-10-13T094500.jsonl".into(),
            ),
        }
    }

    #[test]
    fn mockup_text_matches_design() {
        let expected = "\
======================================================================
documents → work:Backups/Documents   two-way   complete
Added: 14 to OneDrive, 2 to Mac | Updated: 3 | Deleted: 0 on OneDrive, 1 on Mac
Transferred: 312.4 MB in 1m 38s (3.2 MB/s) | Errors: 1
  ✗ Projects/2024/old.mov   OneDrive: file too large (250 GiB limit)
Log: ~/.local/share/onefolder/logs/documents/d-7f3a/2026-10-13T094102.jsonl
======================================================================
";
        assert_eq!(render_plain(&two_way()), expected);
    }

    /// On Linux and WSL the counts on this side say "here".
    #[test]
    fn linux_counts_say_here() {
        let _linux = crate::platform::testing::describe(crate::platform::Os::Linux);
        let w = crate::platform::words();
        let text = render_plain(&two_way());
        assert!(
            text.contains(&format!(
                "Added: 14 to OneDrive, {} | Updated: 3 | Deleted: 0 on OneDrive, {}\n",
                w.to_local(2),
                w.on_local(1)
            )),
            "{text}"
        );
        assert!(
            text.contains(
                "Added: 14 to OneDrive, 2 here | Updated: 3 | Deleted: 0 on OneDrive, 1 here\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn moved_files_have_a_line() {
        let mut s = one_way();
        s.dry_run = false;
        s.moved = Some(crate::model::Moved {
            files: 1204,
            bytes: 3_100_000_000,
        });
        let text = render_text(&s, false, None);
        assert!(
            text.contains("Moved: 1,204 files, 3.1 GB (on the drive, no upload)\n"),
            "{text}"
        );
        // The deleted count is still 0: a move deletes nothing.
        assert!(
            text.contains("Deleted: 0 (one-way never deletes)"),
            "{text}"
        );
        s.destination_type = "onedrive".into();
        s.dry_run = true;
        s.moved = Some(crate::model::Moved {
            files: 1,
            bytes: 12,
        });
        let text = render_text(&s, false, None);
        assert!(
            text.contains("Would move: 1 file, 12 B (on OneDrive, no upload)\n"),
            "{text}"
        );
        s.moved = Some(crate::model::Moved::default());
        assert!(!render_text(&s, false, None).contains("move"));
    }

    #[test]
    fn removed_folders_are_named() {
        let mut s = two_way();
        s.removed_folders = vec!["Old projects/2019".into()];
        let text = render_plain(&s);
        assert!(
            text.contains(
                "\nRemoved on OneDrive: \"Old projects/2019\", with the hidden and system files in it\n"
            ),
            "{text}"
        );
        s.destination_type = "local".into();
        s.removed_folders.push("Drafts".into());
        assert!(render_plain(&s).contains(
            "Removed on the drive: \"Old projects/2019\", \"Drafts\", with the hidden and system files in them"
        ));
    }

    #[test]
    fn plain_two_way() {
        snapshot!("summary_plain_two_way", render_plain(&two_way()));
    }

    #[test]
    fn plain_one_way_dry_run() {
        snapshot!("summary_plain_one_way_dry_run", render_plain(&one_way()));
    }

    #[test]
    fn plain_conflicts_and_failure() {
        let mut s = two_way();
        s.outcome = Outcome::Partial;
        s.conflicts = vec!["Notes/plan.md".into()];
        snapshot!("summary_plain_conflicts", render_plain(&s));
    }

    fn render_tui(summary: &Summary, width: u16) -> String {
        let block = SummaryBlock(summary);
        let mut terminal = Terminal::new(TestBackend::new(width, block.height())).unwrap();
        terminal
            .draw(|f| f.render_widget(SummaryBlock(summary), f.area()))
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn widget_two_way_80() {
        snapshot!("summary_tui_two_way_80", render_tui(&two_way(), 80));
    }

    #[test]
    fn widget_two_way_120() {
        snapshot!("summary_tui_two_way_120", render_tui(&two_way(), 120));
    }

    #[test]
    fn widget_one_way_120() {
        snapshot!("summary_tui_one_way_120", render_tui(&one_way(), 120));
    }

    #[test]
    fn widget_one_way_80() {
        snapshot!("summary_tui_one_way_80", render_tui(&one_way(), 80));
    }

    #[test]
    fn widget_lines_match_plain_text() {
        let s = two_way();
        let from_widget: Vec<String> = SummaryBlock(&s)
            .lines()
            .iter()
            .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .collect();
        let plain: Vec<String> = render_plain(&s).lines().map(String::from).collect();
        assert_eq!(from_widget, plain);
    }

    #[test]
    fn job_summary_lists_every_destination_in_order() {
        let dests = vec![
            DestinationLine {
                display: "work:Backups/Documents".into(),
                mode: Mode::TwoWay,
                outcome: "ok".into(),
                reason: None,
            },
            DestinationLine {
                display: "/Volumes/Backup/Documents".into(),
                mode: Mode::OneWay,
                outcome: "failed".into(),
                reason: Some("Backup drive not mounted".into()),
            },
            DestinationLine {
                display: "personal:Docs".into(),
                mode: Mode::OneWay,
                outcome: "skipped".into(),
                reason: Some("not run, the run was stopped".into()),
            },
        ];
        snapshot!(
            "job_summary",
            render_job_summary("documents", false, &dests, false, None)
        );
        let dry = render_job_summary("documents", true, &dests[..1], false, None);
        assert!(
            dry.contains("documents   1 destination · dry run · 1 ok"),
            "{dry}"
        );
    }

    /// A partial run: the destinations left out keep their place, dimmed,
    /// with when they last ran, and are not counted.
    #[test]
    fn a_partial_runs_summary_lists_the_destinations_left_out() {
        let line = |display: &str, mode, outcome: &str, reason: Option<&str>| DestinationLine {
            display: display.into(),
            mode,
            outcome: outcome.into(),
            reason: reason.map(String::from),
        };
        let dests = vec![
            line("boramektup:Records", Mode::OneWay, "ok", None),
            line(
                "/Volumes/T7 Shield/Records",
                Mode::OneWay,
                crate::ui::partial::NOT_CHOSEN,
                Some("last run 3d ago"),
            ),
            line("work:Archive/Records", Mode::TwoWay, "ok", None),
        ];
        let text = render_job_summary("records", false, &dests, false, None);
        snapshot!("job_summary_partial", text.clone());
        assert!(
            text.contains("records   partial run · 2 of 3 destinations · 2 ok\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "  · /Volumes/T7 Shield/Records   one-way   not chosen · last run 3d ago"
            ),
            "{text}"
        );
        // Dim all along, and the rest as ever.
        let lines = job_summary_lines("records", false, &dests, None);
        assert!(lines[3][1..].iter().all(|(_, tone)| *tone == Tone::Dim));
        assert!(lines[2].contains(&("ok".to_string(), Tone::Success)));
        // One chosen, in a dry run, at a narrow width.
        let one = vec![
            dests[0].clone(),
            line(
                "/Volumes/T7 Shield/Records",
                Mode::OneWay,
                crate::ui::partial::NOT_CHOSEN,
                Some("never run"),
            ),
            line(
                "work:Archive/Records",
                Mode::TwoWay,
                crate::ui::partial::NOT_CHOSEN,
                Some("last run yesterday"),
            ),
        ];
        let text = render_job_summary("records", true, &one, false, None);
        assert!(
            text.contains("records   partial run · 1 of 3 destinations · dry run · 1 ok\n"),
            "{text}"
        );
        let text = render_job_summary("records", true, &one, false, Some(50));
        assert!(text.lines().all(|l| crate::ui::width(l) <= 50), "{text}");
        assert!(
            text.contains("\nrecords · 1 of 3 · dry run · 1 ok\n"),
            "the shorter title keeps the counts: {text}"
        );
        snapshot!("job_summary_partial_50", text);
    }

    #[test]
    fn job_summary_fits_narrow_and_wide_character_terminals() {
        let dests = vec![
            DestinationLine {
                display: "work:バックアップ/書類/顧客/契約書/署名済み".into(),
                mode: Mode::TwoWay,
                outcome: "ok".into(),
                reason: None,
            },
            DestinationLine {
                display: "/Volumes/Backup/Archive/Clients/Acme Corporation/Contracts".into(),
                mode: Mode::OneWay,
                outcome: "failed".into(),
                reason: Some("OneDrive account spare is missing from rclone's config".into()),
            },
            DestinationLine {
                display: "/Volumes/バックアップ/書類".into(),
                mode: Mode::OneWay,
                outcome: "failed".into(),
                reason: Some("バックアップ drive not mounted".into()),
            },
        ];
        let text = render_job_summary("documents", false, &dests, false, Some(60));
        for line in text.lines() {
            assert!(crate::ui::width(line) <= 70, "too wide: {line}");
            if !line.starts_with('=') {
                assert!(crate::ui::width(line) <= 60, "too wide: {line}");
            }
        }
        assert!(text.contains("work:"), "the account is kept: {text}");
        snapshot!("job_summary_60", text);
    }

    #[test]
    fn bytes_and_durations() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(14_000), "14 KB");
        assert_eq!(format_bytes(248_000), "248 KB");
        assert_eq!(format_bytes(1_200_000), "1.2 MB");
        assert_eq!(format_bytes(312_400_000), "312.4 MB");
        assert_eq!(format_bytes(3_400_000_000), "3.4 GB");
        assert_eq!(format_bytes(1_200_000_000_000), "1.2 TB");
        assert_eq!(format_bytes(999_990_000), "1.0 GB");
        assert_eq!(format_duration(400), "0s");
        assert_eq!(format_duration(35_000), "35s");
        assert_eq!(format_duration(98_000), "1m 38s");
        assert_eq!(format_duration(7_500_000), "2h 5m");
    }

    #[test]
    fn a_reason_that_repeats_the_word_is_left_out() {
        let dests = [DestinationLine {
            display: "work:Docs".into(),
            mode: Mode::OneWay,
            outcome: "declined".into(),
            reason: Some("not confirmed".into()),
        }];
        let text = render_job_summary("documents", false, &dests, false, None);
        assert!(text.contains("one-way   not confirmed\n"), "{text}");
        assert!(!text.contains("· not confirmed"), "{text}");
    }
}
