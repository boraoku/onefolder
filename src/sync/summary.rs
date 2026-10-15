//! Per-destination accumulation of rclone's events into the counts, bytes
//! and errors of a [`crate::model::Summary`], and the outcome rules.

use std::collections::{HashMap, HashSet};

use crate::model::{FileError, Outcome, SideCounts};
use crate::rclone::bisync::{self, BisyncPhase, QueuedAction, Side};
use crate::rclone::events::{Event, PlanVerb, Stats, TransferKind};
use crate::rclone::exec::Finished;

use super::oneway::Preview;
use super::plan::{loser_name, DryRun};
use super::reporter::{Change, ConflictDone, FileDone, Where};

/// What one event meant, for the reporter.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    Stats(Stats),
    File(FileDone),
    Error(FileError),
    /// bisync moved to another phase.
    Phase(BisyncPhase),
    Nothing,
}

/// Collects a real copy's events.
#[derive(Debug, Default)]
pub struct Accumulator {
    /// The preview's verdict per path.
    plan: HashMap<String, Change>,
    /// Paths the destination listing had, for files the preview did not see.
    listed: HashSet<String>,
    /// `listed` in NFC, so a decomposed name still matches.
    listed_nfc: HashSet<String>,
    done: HashSet<String>,
    pub added: u64,
    pub updated: u64,
    /// One entry per failed file, in the order they first failed.
    errors: Vec<FileError>,
    /// The last error that was not about one file.
    general: Option<String>,
    /// Prefixes dropped from messages (see [`Accumulator::with_roots`]).
    roots: Vec<String>,
    last: Option<Stats>,
}

impl Accumulator {
    pub fn new(preview: &Preview, listed: HashSet<String>) -> Accumulator {
        let mut plan = HashMap::new();
        for f in &preview.new {
            plan.insert(f.path.clone(), Change::Added);
        }
        for f in &preview.changed {
            plan.insert(f.path.clone(), Change::Updated);
        }
        Accumulator {
            plan,
            listed_nfc: super::nfc_set(&listed),
            listed,
            ..Accumulator::default()
        }
    }

    /// Takes one event into account and says what it meant.
    pub fn apply(&mut self, event: &Event) -> Applied {
        match event {
            Event::Stats(s) => {
                self.last = Some(s.clone());
                Applied::Stats(s.clone())
            }
            Event::Transferred(t) => {
                // A retry that succeeds clears the file's error.
                self.errors.retain(|e| e.path != t.object);
                if !self.done.insert(t.object.clone()) {
                    return Applied::Nothing;
                }
                let change = match self.plan.get(&t.object) {
                    Some(c) => *c,
                    None => match t.kind {
                        TransferKind::New => Change::Added,
                        TransferKind::Replaced | TransferKind::ModTime => Change::Updated,
                        TransferKind::ServerSide | TransferKind::Other => {
                            if self.listed.contains(&t.object)
                                || self.listed_nfc.contains(&super::nfc(&t.object))
                            {
                                Change::Updated
                            } else {
                                Change::Added
                            }
                        }
                    },
                };
                match change {
                    Change::Added => self.added += 1,
                    Change::Updated => self.updated += 1,
                    Change::Deleted => {}
                }
                Applied::File(FileDone {
                    path: t.object.clone(),
                    size: t.size,
                    change,
                    side: Where::Destination,
                })
            }
            Event::Error(e) => match e.file() {
                Some(path) => {
                    // Each file once, with its first reason: retries tend to
                    // repeat it or add a vaguer one ("errno -1").
                    if self.errors.iter().any(|k| k.path == path) {
                        return Applied::Nothing;
                    }
                    let error = FileError {
                        path: path.to_string(),
                        reason: self.reason(&e.message),
                    };
                    self.errors.push(error.clone());
                    Applied::Error(error)
                }
                // rclone's "Attempt 1/3 failed with 2 errors and: ..." only
                // sums up the per-file errors already counted.
                None if is_attempt_summary(&e.message) => Applied::Nothing,
                None => {
                    self.general = Some(self.reason(&e.message));
                    Applied::Nothing
                }
            },
            _ => Applied::Nothing,
        }
    }

    /// Shortens rclone's paths in messages: the source and destination
    /// roots are dropped, so a reason names the file as the summary does.
    pub fn with_roots(mut self, roots: &[&str]) -> Self {
        self.roots = roots
            .iter()
            .filter(|r| !r.is_empty())
            .map(|r| format!("{}/", r.trim_end_matches('/')))
            .collect();
        self
    }

    fn reason(&self, message: &str) -> String {
        let mut text = reason(message);
        for root in &self.roots {
            text = text.replace(root.as_str(), "");
        }
        text
    }

    /// Bytes transferred, from the last stats line.
    pub fn bytes(&self) -> u64 {
        self.last.as_ref().map_or(0, |s| s.bytes)
    }

    /// The last error that was not about one file (a missing folder, a
    /// critical error), if any.
    pub fn general_error(&self) -> Option<&str> {
        self.general.as_deref()
    }

    pub fn file_errors(&self) -> &[FileError] {
        &self.errors
    }

    /// The errors to list in the summary: the per-file ones, or, when the
    /// run failed without any, rclone's general reason.
    pub fn summary_errors(&self, finished: &Finished) -> Vec<FileError> {
        let mut errors = self.errors.clone();
        if errors.is_empty() && !finished.cancelled && !finished.exit.is_success() {
            let reason = self
                .general
                .clone()
                .or_else(|| self.last.as_ref().and_then(|s| s.last_error.clone()))
                .unwrap_or_else(|| format!("rclone {}", finished.exit.describe()));
            errors.push(FileError {
                path: String::new(),
                reason,
            });
        }
        errors
    }

    pub fn outcome(&self, finished: &Finished) -> Outcome {
        outcome(finished, self.errors.len(), self.added + self.updated)
    }
}

/// Collects a bisync run's events: the one-way [`Accumulator`] keeps the
/// stats, the errors and the outcome rules; this adds the side each file
/// changed on (from `Do queued copies to - PathN`), deletions, and the
/// conflicts with the name each losing copy got on the destination.
#[derive(Debug, Default)]
pub struct TwoWayTally {
    pub acc: Accumulator,
    on: Option<Side>,
    /// From the plan: files new on each side, deleted locally, in conflict.
    new1: HashSet<String>,
    new2: HashSet<String>,
    deleted1: HashSet<String>,
    conflicted: HashSet<String>,
    /// A resync: every copy is new, except these overwrites on the destination.
    resync: bool,
    overwrites: HashSet<String>,
    done: HashSet<(Side, String)>,
    pub added: SideCounts,
    pub updated: SideCounts,
    pub deleted: SideCounts,
    pub conflicts: Vec<ConflictDone>,
    /// Every abort or error message, for [`bisync::classify`].
    pub messages: Vec<String>,
    /// The source and destination as rclone prints them.
    roots: Vec<String>,
    /// Conflicts OneFolder set aside before the run: their upload is a change.
    planned_conflicts: HashSet<String>,
}

impl TwoWayTally {
    /// For a normal run, labelled with its plan.
    pub fn new(plan: &DryRun) -> TwoWayTally {
        let set = |side, verb| -> HashSet<String> {
            plan.lines
                .iter()
                .filter(|l| l.side == side && l.verb == verb)
                .map(|l| l.path.clone())
                .collect()
        };
        TwoWayTally {
            new1: set(Side::Path1, PlanVerb::New),
            new2: set(Side::Path2, PlanVerb::New),
            deleted1: set(Side::Path1, PlanVerb::Deleted),
            ..TwoWayTally::default()
        }
    }

    /// For a resync, which only copies: `overwrites` are the destination
    /// files it replaces (counted as updated).
    pub fn resync(overwrites: HashSet<String>) -> TwoWayTally {
        TwoWayTally {
            resync: true,
            overwrites,
            ..TwoWayTally::default()
        }
    }

    pub fn with_roots(mut self, roots: &[&str]) -> Self {
        self.acc = std::mem::take(&mut self.acc).with_roots(roots);
        self.roots = roots.iter().map(|r| r.to_string()).collect();
        self
    }

    /// The conflicts set aside before the run, whose uploads count as updates.
    pub fn with_conflicts(mut self, paths: impl IntoIterator<Item = String>) -> Self {
        self.planned_conflicts = paths.into_iter().collect();
        self
    }

    pub fn apply(&mut self, event: &Event) -> Applied {
        match event {
            Event::Phase { phase } => Applied::Phase(*phase),
            Event::Applying { to } => {
                self.on = Some(*to);
                Applied::Nothing
            }
            Event::Fatal { message } => {
                self.messages.push(message.clone());
                Applied::Nothing
            }
            Event::Conflict { path } => {
                self.conflicted.insert(path.clone());
                self.conflicts.push(ConflictDone {
                    path: path.clone(),
                    kept_as: None,
                });
                Applied::Nothing
            }
            Event::NoConflict { path } => {
                self.conflicted.remove(path);
                self.conflicts.retain(|c| &c.path != path);
                Applied::Nothing
            }
            Event::Queued(q)
                if matches!(q.action, QueuedAction::Copy | QueuedAction::Rename)
                    && (bisync::is_conflict_copy(&q.path)
                        || bisync::is_race_conflict_copy(&q.path)) =>
            {
                if let Some(c) = self.conflicts.last_mut() {
                    if c.kept_as.is_none() {
                        let roots: Vec<&str> = self.roots.iter().map(String::as_str).collect();
                        c.kept_as = Some(loser_name(&c.path, &q.path, &roots));
                    }
                }
                Applied::Nothing
            }
            Event::Error(e) => {
                self.messages.push(e.message.clone());
                self.acc.apply(event)
            }
            Event::Stats(_) => self.acc.apply(event),
            Event::Transferred(t) => {
                // The one-way accumulator keeps its error bookkeeping.
                self.acc.apply(event);
                let side = self.on.unwrap_or(Side::Path2);
                if !self.done.insert((side, t.object.clone())) {
                    return Applied::Nothing;
                }
                let path = t.object.as_str();
                let (change, counts_side) = match side {
                    Side::Path2 => {
                        if !self.resync && self.conflicted.contains(path) {
                            return Applied::Nothing;
                        }
                        let new = if self.resync {
                            !self.overwrites.contains(path)
                        } else {
                            self.new1.contains(path) && !self.planned_conflicts.contains(path)
                        };
                        (
                            if new { Change::Added } else { Change::Updated },
                            Where::Destination,
                        )
                    }
                    Side::Path1 => {
                        // A copy bisync made for a conflict it met in the run
                        // comes here too, as a new file.
                        let new = self.resync
                            || self.new2.contains(path)
                            || self.deleted1.contains(path)
                            || bisync::is_race_conflict_copy(path);
                        (
                            if new { Change::Added } else { Change::Updated },
                            Where::Local,
                        )
                    }
                };
                let change = if t.kind == TransferKind::ModTime {
                    Change::Updated
                } else {
                    change
                };
                let counts = match change {
                    Change::Added => &mut self.added,
                    _ => &mut self.updated,
                };
                match counts_side {
                    Where::Destination => counts.remote += 1,
                    Where::Local => counts.local += 1,
                }
                Applied::File(FileDone {
                    path: t.object.clone(),
                    size: t.size,
                    change,
                    side: counts_side,
                })
            }
            Event::Deleted { object } => {
                let side = self.on.unwrap_or(Side::Path2);
                if !self.done.insert((side, format!("deleted:{object}"))) {
                    return Applied::Nothing;
                }
                let at = match side {
                    Side::Path1 => {
                        self.deleted.local += 1;
                        Where::Local
                    }
                    Side::Path2 => {
                        self.deleted.remote += 1;
                        Where::Destination
                    }
                };
                Applied::File(FileDone {
                    path: object.clone(),
                    size: None,
                    change: Change::Deleted,
                    side: at,
                })
            }
            _ => Applied::Nothing,
        }
    }

    pub fn outcome(&self, finished: &Finished) -> Outcome {
        outcome(
            finished,
            self.acc.file_errors().len(),
            self.added.total() + self.updated.total() + self.deleted.total(),
        )
    }
}

/// The outcome of one rclone run:
///
/// - Ctrl-C: cancelled, whatever the exit code;
/// - success: ok, or partial if some files still failed;
/// - failed files, retryable errors or a transfer limit, with files failed
///   or some transferred: partial;
/// - anything else (usage error, missing folder, fatal error): failed.
pub fn outcome(finished: &Finished, file_errors: usize, transferred: u64) -> Outcome {
    if finished.cancelled {
        Outcome::Cancelled
    } else if finished.exit.is_success() {
        if file_errors > 0 {
            Outcome::Partial
        } else {
            Outcome::Ok
        }
    } else if finished.exit.may_be_partial() && (file_errors > 0 || transferred > 0) {
        Outcome::Partial
    } else {
        Outcome::Failed
    }
}

/// True for rclone's per-attempt summary line.
fn is_attempt_summary(message: &str) -> bool {
    message.starts_with("Attempt ") && message.contains(" failed with ")
}

/// rclone's message without its `Failed to copy: ` wrapper.
fn reason(message: &str) -> String {
    let text = message.trim();
    for prefix in ["Failed to copy: ", "Failed to set modification time: "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rclone::events::{self, ErrorLine, Transferred};
    use crate::rclone::exec::ExitKind;
    use crate::sync::oneway::PlannedFile;

    fn finished(code: i32, cancelled: bool) -> Finished {
        Finished {
            exit: ExitKind::from_code(Some(code)),
            code: Some(code),
            cancelled,
            killed: false,
        }
    }

    fn preview() -> Preview {
        let f = |p: &str, size| PlannedFile {
            path: p.into(),
            size,
            modtime_only: false,
        };
        Preview {
            new: vec![f("new.txt", 6), f("Projects/2026/brief.docx", 6)],
            changed: vec![
                f("Projects/2026/budget.xlsx", 10),
                PlannedFile {
                    path: "touched.txt".into(),
                    size: 0,
                    modtime_only: true,
                },
            ],
            ..Preview::default()
        }
    }

    fn fixture(name: &str) -> Vec<Event> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rclone")
            .join(name);
        events::decode_all(&std::fs::read_to_string(path).unwrap())
    }

    #[test]
    fn server_side_copies_take_the_preview_labels() {
        let mut acc = Accumulator::new(&preview(), HashSet::new());
        for e in fixture("copy-real.jsonl") {
            acc.apply(&e);
        }
        assert_eq!((acc.added, acc.updated), (2, 2));
        assert_eq!(acc.bytes(), 22);
        assert_eq!(acc.outcome(&finished(0, false)), Outcome::Ok);
        assert!(acc.summary_errors(&finished(0, false)).is_empty());
    }

    #[test]
    fn without_a_plan_the_kind_and_listing_decide() {
        let listed: HashSet<String> = ["Projects/2026/budget.xlsx".to_string()].into();
        let mut acc = Accumulator::new(&Preview::default(), listed.clone());
        for e in fixture("copy-upload.jsonl") {
            acc.apply(&e);
        }
        // new.txt and brief.docx are new; budget replaced; touched only re-timed.
        assert_eq!((acc.added, acc.updated), (2, 2));
        let mut acc = Accumulator::new(&Preview::default(), listed);
        for e in fixture("copy-real.jsonl") {
            acc.apply(&e);
        }
        // Server-side copies: the listing knows budget.xlsx; touched.txt is a
        // modification time, so an update.
        assert_eq!((acc.added, acc.updated), (2, 2));
    }

    #[test]
    fn retried_errors_count_once_per_file() {
        let mut acc = Accumulator::new(&Preview::default(), HashSet::new());
        let applied: Vec<Applied> = fixture("copy-file-error.jsonl")
            .iter()
            .map(|e| acc.apply(e))
            .collect();
        let reported = applied
            .iter()
            .filter(|a| matches!(a, Applied::Error(_)))
            .count();
        assert_eq!(reported, 1);
        assert_eq!(acc.file_errors().len(), 1);
        assert_eq!(acc.file_errors()[0].path, "locked.txt");
        assert!(acc.file_errors()[0]
            .reason
            .starts_with("couldn't copy from /fixture/d/src/"));
        // With the roots known, the reason names the file as the summary does.
        let mut acc = Accumulator::new(&Preview::default(), HashSet::new())
            .with_roots(&["/fixture/d/src", "/fixture/d/dst/"]);
        for e in fixture("copy-file-error.jsonl") {
            acc.apply(&e);
        }
        assert_eq!(
            acc.file_errors()[0].reason,
            "couldn't copy from locked.txt to locked.txt.00000000.partial: errno -1"
        );
        assert_eq!(acc.added, 1);
        assert_eq!(acc.outcome(&finished(1, false)), Outcome::Partial);
        assert_eq!(
            acc.general_error(),
            None,
            "attempt summaries are not general errors"
        );
    }

    #[test]
    fn a_retry_that_succeeds_clears_the_error() {
        let mut acc = Accumulator::new(&Preview::default(), HashSet::new());
        acc.apply(&Event::Error(ErrorLine {
            object: Some("a".into()),
            message: "Failed to copy: flaky".into(),
            fatal: false,
        }));
        acc.apply(&Event::Transferred(Transferred {
            kind: TransferKind::New,
            object: "a".into(),
            size: Some(1),
            message: "Copied (new)".into(),
        }));
        assert!(acc.file_errors().is_empty());
        assert_eq!(acc.added, 1);
    }

    #[test]
    fn a_failed_run_lists_rclones_reason() {
        let mut acc = Accumulator::new(&Preview::default(), HashSet::new());
        for e in fixture("copy-missing-source.jsonl") {
            acc.apply(&e);
        }
        let f = finished(3, false);
        assert_eq!(acc.outcome(&f), Outcome::Failed);
        let errors = acc.summary_errors(&f);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "");
        assert!(
            errors[0].reason.contains("directory not found"),
            "{errors:?}"
        );
        assert!(acc.general_error().is_some());
    }

    #[test]
    fn interrupted_runs_are_cancelled() {
        let mut acc = Accumulator::new(&Preview::default(), HashSet::new());
        for e in fixture("copy-sigint.jsonl") {
            acc.apply(&e);
        }
        let f = Finished {
            exit: ExitKind::Signalled,
            code: Some(130),
            cancelled: true,
            killed: false,
        };
        assert_eq!(acc.outcome(&f), Outcome::Cancelled);
        assert!(acc.summary_errors(&f).is_empty());
        assert_eq!(acc.added, 1);
        assert!(acc.bytes() > 0);
    }

    #[test]
    fn two_way_tally_of_the_recorded_run() {
        // The run after OneFolder set the destination's Notes/plan.md
        // aside: bisync meets no conflict and uploads the local copy.
        let plan = DryRun::collect(&fixture("bisync-plan.jsonl"));
        let mut tally = TwoWayTally::new(&plan).with_conflicts(["Notes/plan.md".to_string()]);
        let mut files = Vec::new();
        for e in fixture("bisync-sync.jsonl") {
            if let Applied::File(f) = tally.apply(&e) {
                files.push((f.side, f.change, f.path));
            }
        }
        assert_eq!(
            tally.added,
            SideCounts {
                local: 1,
                remote: 1
            }
        );
        assert_eq!(
            tally.updated,
            SideCounts {
                local: 1,
                remote: 2
            },
            "mod.txt and the conflict's winning upload"
        );
        assert_eq!(
            tally.deleted,
            SideCounts {
                local: 1,
                remote: 5
            }
        );
        assert!(files.contains(&(Where::Local, Change::Added, "newremote.txt".into())));
        assert!(files.contains(&(Where::Destination, Change::Deleted, "sub/f1.txt".into())));
        assert!(files.contains(&(Where::Destination, Change::Updated, "Notes/plan.md".into())));
        assert!(tally.conflicts.is_empty(), "{:?}", tally.conflicts);
        assert_eq!(tally.outcome(&finished(0, false)), Outcome::Ok);
        assert!(tally.messages.is_empty());
    }

    /// bisync's own conflict handling (a file changed after the plan):
    /// reported with the name it gave, taken off the known root.
    #[test]
    fn a_conflict_bisync_handles_itself_is_reported() {
        let mut tally = TwoWayTally::default().with_roots(&["/fixture/ba/src", "/fixture/ba/dst"]);
        for e in fixture("bisync-race-conflict.jsonl") {
            tally.apply(&e);
        }
        assert_eq!(
            tally.conflicts,
            [ConflictDone {
                path: "Notes/plan.md".into(),
                kept_as: Some("Notes/plan.race-conflict-2026-10-13-1.md".into())
            }]
        );
    }

    #[test]
    fn resync_tally_counts_merges() {
        let overwrites: HashSet<String> = ["mod.txt".to_string()].into();
        let mut tally = TwoWayTally::resync(overwrites);
        for e in fixture("bisync-resync.jsonl") {
            tally.apply(&e);
        }
        assert_eq!(tally.added.local, 1, "only-on-destination.txt");
        assert_eq!(tally.updated.remote, 1, "mod.txt");
        assert!(tally.added.remote > 10);
    }

    #[test]
    fn an_abort_leaves_its_messages() {
        let mut tally = TwoWayTally::default();
        for e in fixture("bisync-too-many-deletes.jsonl") {
            tally.apply(&e);
        }
        assert!(matches!(
            bisync::classify(tally.messages.iter().map(String::as_str)),
            Some(bisync::Abort::TooManyDeletes { deleted: 6, .. })
        ));
    }

    #[test]
    fn outcome_rules() {
        assert_eq!(outcome(&finished(0, false), 0, 0), Outcome::Ok);
        assert_eq!(outcome(&finished(9, false), 0, 0), Outcome::Ok);
        assert_eq!(outcome(&finished(0, false), 1, 3), Outcome::Partial);
        assert_eq!(outcome(&finished(1, false), 1, 0), Outcome::Partial);
        assert_eq!(outcome(&finished(1, false), 0, 0), Outcome::Failed);
        assert_eq!(outcome(&finished(5, false), 0, 2), Outcome::Partial);
        assert_eq!(outcome(&finished(8, false), 0, 2), Outcome::Partial);
        for code in [2, 3, 4, 7, 42] {
            assert_eq!(outcome(&finished(code, false), 1, 5), Outcome::Failed);
        }
        assert_eq!(outcome(&finished(0, true), 0, 0), Outcome::Cancelled);
        assert_eq!(outcome(&finished(3, true), 2, 0), Outcome::Cancelled);
    }

    #[test]
    fn reasons_lose_the_wrapper() {
        assert_eq!(reason("Failed to copy: disk full"), "disk full");
        assert_eq!(reason(" plain "), "plain");
    }
}
