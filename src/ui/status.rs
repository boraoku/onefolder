//! The text of `onefolder status`: each job, then each destination with its
//! last run, the drive or account, holds, what is pending, and the log.
//!
//! ```text
//! Status  1 job · 2 destinations
//!
//! documents   ~/Documents
//!   → work:Backups/Documents       two-way   ✓ ok 2h ago
//!       Last run   13 Oct 2026 09:41, took 2m 38s
//!       Changes    Added 14 to OneDrive, 2 to Mac · Updated 3 · Deleted 0 on
//!                  OneDrive, 1 on Mac · 184.3 MB · Errors 0
//!       Account    ✓ work · remote onefolder-work in rclone's config
//!       On hold    Old projects/2019
//!       Log        ~/.local/share/onefolder/logs/documents/d-7f3a/2026-10-13T094102.jsonl
//!   → /Volumes/Backup/Documents    one-way   ✗ failed 3d ago
//!       Last run   10 Oct 2026 14:02, took 0s
//!       Reason     Backup drive not mounted
//!       Drive      ◎ not mounted (expects volume "Backup")
//! ```
//!
//! [`status_lines`] decides the text and tone of every piece; the printed
//! text is drawn from it. Status is not interactive, so there is no screen.

use crate::jsonout::status::{DestinationStatus, JobStatus, Report, VolumeStatus};
use crate::model::{Mode, Outcome, RunRecord};
use crate::ui::jobs::{ago, unix_seconds};
use crate::ui::summary::{format_bytes, format_duration};
use crate::ui::theme::{self, Tone};

/// A run of text with one tone.
pub type Segment = (String, Tone);

fn seg(text: impl Into<String>, tone: Tone) -> Segment {
    (text.into(), tone)
}

/// The indent of a destination's detail lines and the label column.
const DETAIL_INDENT: usize = 6;
const LABEL_W: usize = 11;

/// Columns the status after the mode is given before paths are shortened.
const STATUS_ROOM: usize = 22;

/// Glyph, words and tone for an outcome: `✓ ok`, `✗ failed`...
fn outcome_mark(outcome: Outcome) -> (&'static str, &'static str, Tone) {
    match outcome {
        Outcome::Ok => (theme::DONE, "ok", Tone::Success),
        Outcome::Partial => (theme::SKIPPED, "partial", Tone::Warning),
        Outcome::Failed => (theme::FAILED, "failed", Tone::Error),
        Outcome::Cancelled => (theme::SKIPPED, "cancelled", Tone::Warning),
        Outcome::NeedsResync => (theme::FAILED, "needs resync", Tone::Error),
    }
}

fn severity_mark(severity: &str) -> (&'static str, Tone) {
    match severity {
        "ok" => (theme::DONE, Tone::Success),
        "warning" => (theme::SKIPPED, Tone::Warning),
        "problem" => (theme::FAILED, Tone::Error),
        _ => (theme::SKIPPED, Tone::Dim),
    }
}

/// `13 Oct 2026 09:41` from `2026-10-13T09:41:02+11:00`, in the time the
/// record was written in (the Mac's local time when it ran).
pub fn absolute(rfc3339: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let field = |range: std::ops::Range<usize>| rfc3339.get(range);
    let month = field(5..7)
        .and_then(|m| m.parse::<usize>().ok())
        .and_then(|m| MONTHS.get(m.wrapping_sub(1)));
    match (field(0..4), month, field(8..10), field(11..16)) {
        (Some(y), Some(m), Some(d), Some(hm)) => {
            format!("{} {m} {y} {hm}", d.trim_start_matches('0'))
        }
        _ => rfc3339.to_string(),
    }
}

/// `✓ ok 2h ago`, for a run record.
fn run_mark(r: &RunRecord, now: i64) -> (String, Tone) {
    let (glyph, word, tone) = outcome_mark(r.outcome);
    let when = unix_seconds(&r.finished)
        .map(|t| format!(" {}", ago(t, now)))
        .unwrap_or_default();
    (format!("{glyph} {word}{when}"), tone)
}

/// `Added: ...` counts of a run, in the summary block's words.
fn counts(r: &RunRecord, kind: &str) -> String {
    let remote = if kind == "onedrive" {
        "OneDrive"
    } else {
        "drive"
    };
    let d = theme::DOT;
    let changes = match r.mode {
        Mode::TwoWay => format!(
            "Added {} to {remote}, {} to Mac {d} Updated {} {d} Deleted {} on {remote}, {} on Mac",
            r.added.remote,
            r.added.local,
            r.updated.total(),
            r.deleted.remote,
            r.deleted.local
        ),
        Mode::OneWay => format!(
            "Added {} {d} Updated {}",
            r.added.total(),
            r.updated.total()
        ),
    };
    format!(
        "{changes} {d} {} {d} Errors {}",
        format_bytes(r.bytes),
        r.errors
    )
}

/// True when a run's counts are worth a line: always for a run that went
/// ahead, and for one that stopped only when it got somewhere (a run
/// refused at its checks has nothing but the reason to show).
fn has_counts(r: &RunRecord) -> bool {
    matches!(
        r.outcome,
        Outcome::Ok | Outcome::Partial | Outcome::Cancelled
    ) || r.bytes > 0
        || r.added.total() + r.updated.total() + r.deleted.total() > 0
}

use super::wrap;

/// Everything [`status_lines`] needs besides the report.
struct Layout<'a> {
    now: i64,
    width: Option<usize>,
    tilde: &'a dyn Fn(&str) -> String,
}

impl Layout<'_> {
    /// Room for a detail line's text.
    fn text_width(&self) -> Option<usize> {
        self.width
            .map(|w| w.saturating_sub(DETAIL_INDENT + LABEL_W).max(20))
    }

    fn fit(&self, path: &str, max: Option<usize>) -> String {
        match max {
            Some(m) => super::shorten_path(path, m),
            None => path.to_string(),
        }
    }

    /// A labelled detail line, wrapped under its label. `lead` comes before
    /// the text in its own tone (a glyph).
    fn detail(
        &self,
        out: &mut Vec<Vec<Segment>>,
        label: &str,
        lead: Option<Segment>,
        text: &str,
        tone: Tone,
    ) {
        let full_lead = lead.as_ref().map_or(0, |(t, _)| super::width(t) + 1);
        // A long lead (`✓ ok 10m ago`) on a narrow terminal is wrapped with
        // the text rather than indented under, which would squeeze the
        // text into a sliver.
        let inline = matches!(self.text_width(), Some(w) if full_lead > 3 && w < full_lead + 24);
        let lead_w = if inline { 0 } else { full_lead };
        let pieces = match self.text_width() {
            Some(w) if inline => {
                let first = lead.as_ref().map_or("", |(t, _)| t.as_str());
                let mut pieces = wrap(&format!("{first} {text}"), w.max(10));
                // The lead is drawn in its own tone below.
                if let Some(p) = pieces.first_mut() {
                    *p = p.strip_prefix(first).unwrap_or(p).trim_start().to_string();
                }
                pieces
            }
            Some(w) => wrap(text, w.saturating_sub(lead_w).max(10)),
            None => vec![text.to_string()],
        };
        for (i, piece) in pieces.into_iter().enumerate() {
            let mut line = Vec::new();
            if i == 0 {
                line.push(seg(
                    format!("{}{label:<LABEL_W$}", " ".repeat(DETAIL_INDENT)),
                    Tone::Dim,
                ));
                if let Some((glyph, t)) = &lead {
                    line.push(seg(glyph.clone(), *t));
                    line.push(seg(" ", Tone::Plain));
                }
            } else {
                line.push(seg(
                    " ".repeat(DETAIL_INDENT + LABEL_W + lead_w),
                    Tone::Plain,
                ));
            }
            line.push(seg(piece, tone));
            out.push(line);
        }
    }

    /// A labelled path, shortened in the middle rather than wrapped.
    fn path_detail(&self, out: &mut Vec<Vec<Segment>>, label: &str, path: &str) {
        let shown = self.fit(path, self.text_width());
        out.push(vec![
            seg(
                format!("{}{label:<LABEL_W$}", " ".repeat(DETAIL_INDENT)),
                Tone::Dim,
            ),
            seg(shown, Tone::Dim),
        ]);
    }

    fn volume(&self, out: &mut Vec<Vec<Segment>>, label: &str, v: &VolumeStatus) {
        let (glyph, tone) = severity_mark(&v.severity);
        self.detail(out, label, Some(seg(glyph, tone)), &v.message, Tone::Plain);
    }
}

/// The lines of `status`: a header, then each job and its destinations.
/// `width` is the terminal's (paths are shortened and details wrapped to
/// fit), or `None` to print everything in full. `tilde` shortens paths
/// under the home folder.
pub fn status_lines(
    report: &Report,
    now: i64,
    width: Option<usize>,
    tilde: &dyn Fn(&str) -> String,
) -> Vec<Vec<Segment>> {
    let layout = Layout { now, width, tilde };
    let dests: usize = report.jobs.iter().map(|j| j.destinations.len()).sum();
    let plural = |n: usize, w: &str| {
        if n == 1 {
            format!("1 {w}")
        } else {
            format!("{n} {w}s")
        }
    };
    let mut out = vec![
        vec![
            seg("Status", Tone::Bold),
            seg(
                format!(
                    "  {} {} {}",
                    plural(report.jobs.len(), "job"),
                    theme::DOT,
                    plural(dests, "destination")
                ),
                Tone::Dim,
            ),
        ],
        Vec::new(),
    ];
    if let Some(e) = &report.state_error {
        let text = format!(
            "{} {e} {} showing jobs without their last runs",
            theme::FAILED,
            theme::DOT
        );
        let pieces = match width {
            Some(w) => wrap(&text, w.saturating_sub(2).max(20)),
            None => vec![text],
        };
        for (i, piece) in pieces.into_iter().enumerate() {
            let indent = if i == 0 { "" } else { "  " };
            out.push(vec![seg(format!("{indent}{piece}"), Tone::Error)]);
        }
        out.push(Vec::new());
    }
    if report.jobs.is_empty() {
        out.push(vec![seg(
            format!(
                "  {} no jobs yet {} add one with: onefolder jobs add",
                theme::SKIPPED,
                theme::DOT
            ),
            Tone::Dim,
        )]);
        return out;
    }
    let natural = report
        .jobs
        .iter()
        .flat_map(|j| &j.destinations)
        .map(|d| super::width(&d.display))
        .max()
        .unwrap_or(0)
        .max(24);
    let target_w = width
        .map(|w| w.saturating_sub(4 + 2 + 10 + STATUS_ROOM).max(20))
        .map_or(natural, |m| natural.min(m))
        + 2;
    for (i, job) in report.jobs.iter().enumerate() {
        if i > 0 {
            out.push(Vec::new());
        }
        job_lines(&mut out, job, &layout, target_w);
    }
    out
}

fn job_lines(out: &mut Vec<Vec<Segment>>, job: &JobStatus, layout: &Layout, target_w: usize) {
    let name_w = super::width(&job.name).max(9) + 3;
    let source = (layout.tilde)(&job.source);
    let source_max = layout.width.map(|w| w.saturating_sub(name_w).max(10));
    out.push(vec![
        seg(super::pad(&job.name, name_w), Tone::Bold),
        seg(layout.fit(&source, source_max), Tone::Plain),
    ]);
    if let Some(v) = &job.source_volume {
        layout.volume(out, "Source", v);
    }
    if let Some(l) = &job.run_lock {
        let (glyph, tone) = if l.is_running() {
            (theme::ACTION, Tone::Accent)
        } else {
            (theme::SKIPPED, Tone::Warning)
        };
        let label = if l.is_running() {
            "Running"
        } else {
            "Run lock"
        };
        layout.detail(
            out,
            label,
            Some(seg(glyph, tone)),
            &l.words(absolute),
            Tone::Plain,
        );
    }
    if job.destinations.is_empty() {
        let text = format!(
            "{} no destinations yet {} add one with: onefolder jobs edit {}",
            theme::SKIPPED,
            theme::DOT,
            job.name
        );
        let pieces = match layout.width {
            Some(w) => wrap(&text, w.saturating_sub(4).max(20)),
            None => vec![text],
        };
        for (i, piece) in pieces.into_iter().enumerate() {
            let indent = if i == 0 { "  " } else { "    " };
            out.push(vec![seg(format!("{indent}{piece}"), Tone::Warning)]);
        }
    }
    for d in &job.destinations {
        destination_lines(out, d, layout, target_w);
    }
}

fn destination_lines(
    out: &mut Vec<Vec<Segment>>,
    d: &DestinationStatus,
    layout: &Layout,
    target_w: usize,
) {
    let (mark, tone) = match &d.last_run {
        Some(r) => run_mark(r, layout.now),
        None => ("never run".to_string(), Tone::Dim),
    };
    out.push(vec![
        seg(
            format!(
                "  {} {}",
                theme::ACTION,
                super::pad(&layout.fit(&d.display, Some(target_w - 2)), target_w)
            ),
            Tone::Plain,
        ),
        seg(format!("{:<10}", d.mode.as_str()), Tone::Plain),
        seg(mark, tone),
    ]);

    if let Some(r) = &d.last_run {
        let took = match (unix_seconds(&r.started), unix_seconds(&r.finished)) {
            (Some(a), Some(b)) if b >= a => {
                format!(", took {}", format_duration(((b - a) as u64) * 1000))
            }
            _ => String::new(),
        };
        layout.detail(
            out,
            "Last run",
            None,
            &format!("{}{took}", absolute(&r.started)),
            Tone::Plain,
        );
        if has_counts(r) {
            layout.detail(out, "Changes", None, &counts(r, &d.kind), Tone::Plain);
        }
        if let Some(reason) = &r.reason {
            layout.detail(out, "Reason", None, reason, Tone::Plain);
        }
    }

    match (&d.volume, &d.account) {
        (Some(v), _) => layout.volume(
            out,
            if v.mount_point.is_some() {
                "Drive"
            } else {
                "Folder"
            },
            v,
        ),
        (None, Some(account)) => {
            let d_ = theme::DOT;
            let (glyph, tone, text) = match (&d.remote, d.remote_exists) {
                (None, _) => (
                    theme::FAILED,
                    Tone::Error,
                    format!("{account} is not in the config {d_} add it with: onefolder accounts add {account}"),
                ),
                (Some(remote), Some(true)) => (
                    theme::DONE,
                    Tone::Success,
                    format!("{account} {d_} remote {remote} in rclone's config"),
                ),
                (Some(remote), Some(false)) => (
                    theme::FAILED,
                    Tone::Error,
                    format!(
                        "{account} {d_} remote {remote} missing from rclone's config {d_} run: onefolder accounts reconnect {account}"
                    ),
                ),
                (Some(remote), None) => (
                    theme::SKIPPED,
                    Tone::Dim,
                    format!("{account} {d_} remote {remote} not checked, rclone is not available"),
                ),
            };
            layout.detail(out, "Account", Some(seg(glyph, tone)), &text, Tone::Plain);
        }
        (None, None) => {}
    }

    if !d.holds.is_empty() {
        layout.detail(
            out,
            "On hold",
            None,
            &super::quoted_list(&d.holds),
            Tone::Plain,
        );
    }

    if let Some(p) = &d.pending {
        let (glyph, tone, text) = match p.state.as_str() {
            "needs-resync" => (
                theme::FAILED,
                Tone::Error,
                match &p.detail {
                    Some(why) => format!("needs a merge (resync) on the next run: {why}"),
                    None => "needs a merge (resync) on the next run".to_string(),
                },
            ),
            "recovering" => (
                theme::SKIPPED,
                Tone::Warning,
                format!(
                    "the next run recovers from the interrupted run on {}",
                    p.detail.as_deref().map(absolute).unwrap_or_default()
                ),
            ),
            "pair-changed" => (
                theme::SKIPPED,
                Tone::Warning,
                "the source or destination changed: the next run merges both sides".to_string(),
            ),
            _ => (
                theme::SKIPPED,
                Tone::Dim,
                "first two-way run: the next run merges both sides".to_string(),
            ),
        };
        layout.detail(
            out,
            "Pending",
            Some(seg(glyph, tone)),
            text.trim_end(),
            Tone::Plain,
        );
    }

    if d.dry_run_newer {
        if let Some(r) = &d.last_dry_run {
            let (mark, tone) = run_mark(r, layout.now);
            let newer = if d.last_run.is_some() {
                ", newer than the last run"
            } else {
                ""
            };
            layout.detail(
                out,
                "Dry run",
                Some(seg(mark, tone)),
                &format!("({}){newer}", absolute(&r.started)),
                Tone::Dim,
            );
        }
    }

    if let Some(log) = &d.log {
        layout.path_detail(out, "Log", &(layout.tilde)(log));
    }
}

/// The status as text, one line per row. With `colour` set, tones become
/// ANSI colours.
pub fn render_text(
    report: &Report,
    now: i64,
    colour: bool,
    width: Option<usize>,
    tilde: &dyn Fn(&str) -> String,
) -> String {
    let mut out = String::new();
    // Whatever the layout could not shorten is wrapped, so every line fits.
    let lines = super::fit_lines(status_lines(report, now, width, tilde), width);
    for line in lines {
        let text: String = line
            .iter()
            .map(|(t, tone)| theme::paint(t, *tone, colour))
            .collect();
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonout::status::Pending;
    use crate::model::SideCounts;

    /// 13 Oct 2026, 11:41 in Sydney.
    const NOW: i64 = 1_791_852_062;

    fn record(outcome: Outcome, mode: Mode, finished: &str) -> RunRecord {
        RunRecord {
            started: finished.replace(":40+", ":02+").replace("43:", "41:"),
            finished: finished.into(),
            mode,
            dry_run: false,
            outcome,
            added: SideCounts::default(),
            updated: SideCounts::default(),
            deleted: SideCounts::default(),
            bytes: 0,
            errors: 0,
            log: String::new(),
            rclone_version: "1.75.2".into(),
            reason: None,
        }
    }

    fn dest(id: &str, kind: &str, display: &str, mode: Mode) -> DestinationStatus {
        DestinationStatus {
            id: id.into(),
            kind: kind.into(),
            display: display.into(),
            path: display.into(),
            mode,
            account: None,
            remote: None,
            remote_exists: None,
            volume: None,
            last_run: None,
            last_dry_run: None,
            dry_run_newer: false,
            log: None,
            holds: Vec::new(),
            pending: None,
        }
    }

    fn onedrive(id: &str, path: &str, mode: Mode) -> DestinationStatus {
        DestinationStatus {
            account: Some("work".into()),
            remote: Some("onefolder-work".into()),
            remote_exists: Some(true),
            path: path.into(),
            ..dest(id, "onedrive", &format!("work:{path}"), mode)
        }
    }

    fn mounted(name: &str, mount: &str) -> VolumeStatus {
        VolumeStatus {
            state: "mounted".into(),
            severity: "ok".into(),
            message: format!("mounted · volume \"{name}\""),
            mount_point: Some(mount.into()),
            elsewhere: None,
        }
    }

    /// Every case the status shows: never run, ok, partial, failed,
    /// cancelled, needs resync, held folders and a newer dry run.
    pub(crate) fn sample() -> Report {
        // ok, two-way, with holds and a log.
        let mut ok = onedrive("d-7f3a", "Backups/Documents", Mode::TwoWay);
        let mut r = record(Outcome::Ok, Mode::TwoWay, "2026-10-13T09:43:40+11:00");
        r.added = SideCounts {
            local: 2,
            remote: 14,
        };
        r.updated = SideCounts {
            local: 0,
            remote: 3,
        };
        r.deleted = SideCounts {
            local: 1,
            remote: 0,
        };
        r.bytes = 184_329_113;
        r.log = "logs/documents/d-7f3a/2026-10-13T094102.jsonl".into();
        ok.last_run = Some(r);
        ok.log = Some(
            "/Users/bora/.local/share/onefolder/logs/documents/d-7f3a/2026-10-13T094102.jsonl"
                .into(),
        );
        ok.holds = vec!["Old projects/2019".into(), "Scratch".into()];

        // failed: the drive was not mounted; still not mounted; a newer dry run.
        let mut failed = dest("d-91c0", "local", "/Volumes/Backup/Documents", Mode::OneWay);
        let mut r = record(Outcome::Failed, Mode::OneWay, "2026-10-10T14:02:00+11:00");
        r.started = "2026-10-10T14:02:00+11:00".into();
        r.reason = Some("Backup drive not mounted".into());
        failed.last_run = Some(r);
        failed.volume = Some(VolumeStatus {
            state: "not-mounted".into(),
            severity: "warning".into(),
            message: "not mounted (expects volume \"Backup\")".into(),
            mount_point: Some("/Volumes/Backup".into()),
            elsewhere: None,
        });
        let mut dry = record(Outcome::Ok, Mode::OneWay, "2026-10-13T11:31:00+11:00");
        dry.started = "2026-10-13T11:30:40+11:00".into();
        dry.dry_run = true;
        failed.last_dry_run = Some(dry);
        failed.dry_run_newer = true;

        // partial, one-way, a long path on a drive mounted elsewhere.
        let mut partial = dest(
            "d-0b0b",
            "local",
            "/Volumes/Archive/Clients/Acme Corporation/Contracts/2026/Signed copies",
            Mode::OneWay,
        );
        let mut r = record(Outcome::Partial, Mode::OneWay, "2026-10-12T20:15:40+11:00");
        r.added = SideCounts {
            local: 0,
            remote: 37,
        };
        r.updated = SideCounts {
            local: 0,
            remote: 5,
        };
        r.bytes = 430_800_000;
        r.errors = 2;
        partial.last_run = Some(r);
        partial.volume = Some(VolumeStatus {
            state: "not-mounted".into(),
            severity: "problem".into(),
            message: "not mounted at /Volumes/Archive, the drive is at /Volumes/Archive 1 (expects volume \"Archive\")".into(),
            mount_point: Some("/Volumes/Archive".into()),
            elsewhere: Some("/Volumes/Archive 1".into()),
        });

        // never run, two-way: the first run will merge; the remote is missing.
        let mut never = onedrive("d-1111", "Notes", Mode::TwoWay);
        never.remote_exists = Some(false);
        never.pending = Some(Pending {
            state: "first-run".into(),
            detail: None,
        });

        // cancelled, two-way: the next run recovers.
        let mut cancelled = onedrive("d-2222", "Photos", Mode::TwoWay);
        let mut r = record(
            Outcome::Cancelled,
            Mode::TwoWay,
            "2026-10-13T08:43:40+11:00",
        );
        r.added = SideCounts {
            local: 0,
            remote: 4,
        };
        r.bytes = 12_000_000;
        cancelled.last_run = Some(r);
        cancelled.pending = Some(Pending {
            state: "recovering".into(),
            detail: Some("2026-10-13T08:41:02+11:00".into()),
        });

        // needs resync: bisync stopped on a critical error.
        let mut resync = dest("d-3333", "local", "/Volumes/Photos/Library", Mode::TwoWay);
        let mut r = record(
            Outcome::NeedsResync,
            Mode::TwoWay,
            "2026-10-11T07:43:40+11:00",
        );
        r.reason = Some("Bisync critical error".into());
        resync.last_run = Some(r);
        resync.volume = Some(mounted("Photos", "/Volumes/Photos"));
        resync.pending = Some(Pending {
            state: "needs-resync".into(),
            detail: Some(
                "bisync stopped on a critical error: path1 and path2 are out of sync".into(),
            ),
        });

        Report {
            state_error: None,
            jobs: vec![
                JobStatus {
                    name: "documents".into(),
                    source: "/Users/bora/Documents".into(),
                    source_volume: None,
                    run_lock: Some(crate::state::lock::Seen {
                        state: "running".into(),
                        pid: Some(4242),
                        started: Some("2026-10-13T11:30:00+11:00".into()),
                        path: "/Users/bora/.local/share/onefolder/runs/documents.lock".into(),
                    }),
                    destinations: vec![ok, failed, partial],
                },
                JobStatus {
                    name: "photos".into(),
                    source: "/Volumes/Photos SSD/Pictures".into(),
                    source_volume: Some(mounted("Photos SSD", "/Volumes/Photos SSD")),
                    run_lock: Some(crate::state::lock::Seen {
                        state: "stale".into(),
                        pid: Some(77),
                        started: None,
                        path: "/Users/bora/.local/share/onefolder/runs/photos.lock".into(),
                    }),
                    destinations: vec![never, cancelled, resync],
                },
                JobStatus {
                    name: "empty".into(),
                    source: "/Users/bora/Empty".into(),
                    source_volume: None,
                    run_lock: None,
                    destinations: Vec::new(),
                },
            ],
        }
    }

    fn tilde(p: &str) -> String {
        match p.strip_prefix("/Users/bora") {
            Some(rest) => format!("~{rest}"),
            None => p.to_string(),
        }
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
        let text = render_text(&sample(), NOW, false, Some(80), &tilde);
        for line in text.lines() {
            assert!(crate::ui::width(line) <= 80, "too wide: {line}");
        }
        snapshot("status_80", text);
    }

    #[test]
    fn renders_at_120_columns() {
        let text = render_text(&sample(), NOW, false, Some(120), &tilde);
        for line in text.lines() {
            assert!(crate::ui::width(line) <= 120, "too wide: {line}");
        }
        snapshot("status_120", text);
    }

    #[test]
    fn every_line_fits_at_40_columns() {
        let text = render_text(&sample(), NOW, false, Some(40), &tilde);
        for line in text.lines() {
            assert!(super::super::width(line) <= 40, "{line}");
        }
        assert!(
            !text.contains("184.3\n"),
            "a size stays with its unit: {text}"
        );
        snapshot("status_40", text);
    }

    #[test]
    fn renders_at_60_columns() {
        let text = render_text(&sample(), NOW, false, Some(60), &tilde);
        for line in text.lines() {
            assert!(crate::ui::width(line) <= 60, "too wide: {line}");
        }
        snapshot("status_60", text);
    }

    #[test]
    fn wide_characters_fit_and_line_up() {
        let mut report = sample();
        let job = &mut report.jobs[0];
        job.source = "/Users/bora/書類/プロジェクト".into();
        job.destinations[0].display = "work:バックアップ/書類/顧客/契約書/署名済み".into();
        job.destinations[1].display = "/Volumes/バックアップ/書類".into();
        job.destinations[0].holds = vec!["古いプロジェクト/2019".into()];
        for width in [60, 80] {
            let text = render_text(&report, NOW, false, Some(width), &tilde);
            for line in text.lines() {
                assert!(crate::ui::width(line) <= width, "too wide: {line}");
            }
            let starts: Vec<usize> = text
                .lines()
                .filter(|l| l.starts_with("  → "))
                .map(|l| crate::ui::width(&l[..l.find("-way").unwrap()]))
                .collect();
            assert!(starts.windows(2).all(|w| w[0] == w[1]), "{text}");
            snapshot(&format!("status_cjk_{width}"), text);
        }
    }

    #[test]
    fn says_what_each_case_means() {
        let text = render_text(&sample(), NOW, false, None, &tilde);
        for expected in [
            "Status  3 jobs · 6 destinations",
            "✓ ok 1h ago",
            "Last run   13 Oct 2026 09:41, took 2m 38s",
            "Added 14 to OneDrive, 2 to Mac · Updated 3 · Deleted 0 on OneDrive, 1 on Mac · 184.3 MB · Errors 0",
            "On hold    \"Old projects/2019\", \"Scratch\"",
            "✗ failed 2d ago",
            "Reason     Backup drive not mounted",
            "Drive      ◎ not mounted (expects volume \"Backup\")",
            "Dry run    ✓ ok 10m ago (13 Oct 2026 11:30), newer than the last run",
            "◎ partial 15h ago",
            "Errors 2",
            "the drive is at /Volumes/Archive 1",
            "never run",
            "remote onefolder-work missing from rclone's config",
            "Pending    ◎ first two-way run: the next run merges both sides",
            "◎ cancelled 2h ago",
            "the next run recovers from the interrupted run on 13 Oct 2026 08:41",
            "✗ needs resync 2d ago",
            "needs a merge (resync) on the next run",
            "      Source     ✓ mounted · volume \"Photos SSD\"",
            "no destinations yet",
            "Log        ~/.local/share/onefolder/logs/documents/d-7f3a/2026-10-13T094102.jsonl",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(!text.contains('\u{2014}'), "no em-dashes");
    }

    #[test]
    fn no_jobs() {
        let text = render_text(
            &Report {
                state_error: None,
                jobs: Vec::new(),
            },
            NOW,
            false,
            Some(80),
            &tilde,
        );
        assert!(text.contains("no jobs yet"), "{text}");
    }

    #[test]
    fn a_state_file_that_does_not_load_is_said() {
        let mut report = sample();
        report.state_error =
            Some("/d/state.json was written by a newer OneFolder (state version 2).".into());
        let text = render_text(&report, NOW, false, Some(80), &tilde);
        assert!(
            text.starts_with("Status  3 jobs · 6 destinations\n\n✗ /d/state.json was written by a newer OneFolder"),
            "{text}"
        );
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("showing jobs without their last runs"),
            "{text}"
        );
    }

    #[test]
    fn json_shape() {
        let json = crate::jsonout::Envelope::success("status", true, sample())
            .to_json()
            .unwrap()
            .replace(env!("CARGO_PKG_VERSION"), "[version]");
        snapshot("status_json", json);
    }

    #[test]
    fn absolute_times() {
        assert_eq!(absolute("2026-10-03T09:41:02+11:00"), "3 Oct 2026 09:41");
        assert_eq!(absolute("garbage"), "garbage");
    }
}
