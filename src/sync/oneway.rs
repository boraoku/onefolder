//! One-way sync: additive `rclone copy` from the local source to a
//! destination. Nothing on the destination is ever deleted or renamed.
//!
//! For one destination (DESIGN.md, One-way sync):
//!
//! 1. validate: the source exists and is readable (and is on its recorded
//!    drive); a local destination passes the volume check; a OneDrive
//!    destination answers `rclone about`;
//! 2. preview: `rclone copy --dry-run` and, in parallel, `rclone lsjson -R
//!    --files-only` of the destination (with hashes for OneDrive) and of
//!    the source; a would-be copy whose path the destination lacks is new,
//!    otherwise changed; new files whose content is already on the
//!    destination under a name the source no longer has are moves
//!    ([`super::moves`]);
//! 3. show the preview and ask (about the moves first, when there are
//!    any), unless `--yes`; `--dry-run` stops here;
//! 4. the moves on the destination, then `rclone copy`, labelling each
//!    file with the preview's verdict;
//! 5. the summary, and the run record in `state.json`, whatever happened.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{tilde, Paths};
use crate::model::{
    Account, Destination, FileError, Job, Moved, Outcome, RunRecord, SideCounts, Summary,
};
use crate::rclone::detect::Rclone;
use crate::rclone::events::{Event, Stats};
use crate::rclone::exec::{self, Finished, InterruptGuard, Progress, RunLog};
use crate::rclone::listing::{self, Entry, ListError};
use crate::rclone::remotes::{self, FailureKind};
use crate::rclone::{self as rc};
use crate::state::State;
use crate::volume::{self, VolumeError};

use super::moves::{self as mv, MoveGroup, MovesChoice, PlannedMove};
use super::reporter::{
    Answer, DecidedBy, DestinationInfo, Expected, FolderPolicy, Looking, MovesAnswer, MovesPolicy,
    Question, Reporter, Step,
};
use super::summary::{Accumulator, Applied};

/// How many paths of each kind the preview lists.
pub const PREVIEW_PATHS: usize = 10;

/// How often the live figures move once the dry run has finished and the
/// destination listing has not (rclone's stats lines set the pace before).
pub const LOOKING_EVERY: Duration = Duration::from_secs(1);

/// The excludes in force for a job: the built-in list, the hidden
/// patterns unless the job copies hidden files, then the job's own
/// ([`rc::effective_excludes`]).
pub fn excludes_of(job: &Job) -> Vec<String> {
    rc::effective_excludes(&job.excludes, job.copy_hidden)
}

/// A file the copy would send.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PlannedFile {
    pub path: String,
    /// Bytes to send: 0 when only the modification time differs.
    pub size: u64,
    /// Same content, different time: rclone only sets the time where the
    /// backend allows it (local drives); OneDrive gets the file again.
    pub modtime_only: bool,
}

/// What a one-way copy would do, from the dry run and the destination listing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Preview {
    pub job: String,
    pub destination_id: String,
    /// As shown: `work:Backups/Documents` or a local path.
    pub destination: String,
    pub new: Vec<PlannedFile>,
    pub changed: Vec<PlannedFile>,
    pub new_bytes: u64,
    pub changed_bytes: u64,
    /// Files on both sides that are the same.
    pub unchanged: u64,
    /// Files the dry run compared on both sides (its `checks`), changed or
    /// not: what the real copy checks too.
    pub checked: u64,
    /// Files only on the destination, which one-way leaves alone.
    pub destination_only: u64,
    /// The job's own exclude patterns.
    pub excludes: Vec<String>,
    /// The job copies hidden files (`copy_hidden`).
    pub copy_hidden: bool,
    /// Every pattern in force: the built-in list, the hidden patterns
    /// unless `copy_hidden`, then the job's own.
    pub effective_excludes: Vec<String>,
    /// `onedrive` or `local`, for the wording ("on OneDrive", "on the
    /// drive").
    pub destination_type: String,
    /// Files moved locally whose old copy is on the destination under the
    /// old name, sorted by `to` ([`super::moves`]). Unless they are to be
    /// uploaded again ([`MovesChoice::Upload`]), they are not in `new`.
    pub moves: Vec<PlannedMove>,
    pub move_bytes: u64,
    /// The moves as the Moved list shows them and as they run: whole
    /// folders first, then single files.
    pub move_groups: Vec<MoveGroup>,
    /// Whether the moves happen, are uploaded again, or are still to be
    /// asked about.
    pub moves_choice: MovesChoice,
    /// A `--dry-run`: the wording says what would happen.
    pub dry_run: bool,
}

impl Preview {
    /// Classifies the dry run's would-be copies against the destination
    /// listing. `stats` is the dry run's last stats line: its `checks` are
    /// the files found on both sides, so the unchanged ones are the checks
    /// that will not be copied, and the destination-only ones are the
    /// listed files that were not checked.
    pub fn build(
        dry_run: &[Event],
        stats: Option<&Stats>,
        listing: &HashSet<String>,
        listed_files: u64,
    ) -> Preview {
        let mut preview = Preview::default();
        let mut seen = HashSet::new();
        let listed_nfc = super::nfc_set(listing);
        for event in dry_run {
            let Event::Skipped(s) = event else { continue };
            if s.is_dir || !seen.insert(s.object.clone()) {
                continue;
            }
            let size = s.size.unwrap_or(0);
            match s.action.as_str() {
                "copy" => {
                    let file = PlannedFile {
                        path: s.object.clone(),
                        size,
                        modtime_only: false,
                    };
                    if listing.contains(&s.object) || listed_nfc.contains(&super::nfc(&s.object)) {
                        preview.changed_bytes += size;
                        preview.changed.push(file);
                    } else {
                        preview.new_bytes += size;
                        preview.new.push(file);
                    }
                }
                "update modification time" => preview.changed.push(PlannedFile {
                    path: s.object.clone(),
                    size: 0,
                    modtime_only: true,
                }),
                _ => {}
            }
        }
        let checks = stats.map_or(0, |s| s.checks);
        preview.checked = checks;
        preview.unchanged = checks.saturating_sub(preview.changed.len() as u64);
        preview.destination_only = listed_files.saturating_sub(checks);
        preview
    }

    pub fn total_bytes(&self) -> u64 {
        self.new_bytes + self.changed_bytes
    }

    /// Takes the moves found: their files leave `new`, since they need no
    /// upload if the moves go ahead ([`Preview::choose_moves`] puts them
    /// back when they are uploaded again).
    pub fn apply_moves(&mut self, groups: Vec<MoveGroup>, moves: Vec<PlannedMove>) {
        let moved: HashSet<&str> = moves.iter().map(|m| m.to.as_str()).collect();
        self.new.retain(|f| !moved.contains(f.path.as_str()));
        self.move_bytes = moves.iter().map(|m| m.size).sum();
        self.new_bytes = self.new_bytes.saturating_sub(self.move_bytes);
        self.moves = moves;
        self.move_groups = groups;
        self.moves_choice = MovesChoice::Undecided;
    }

    /// Records what happens to the moved files. Uploading them again puts
    /// them back in `new` (and its bytes); any other choice keeps them out.
    pub fn choose_moves(&mut self, choice: MovesChoice) {
        let was_upload = self.moves_choice == MovesChoice::Upload;
        let upload = choice == MovesChoice::Upload;
        if upload && !was_upload {
            self.new.extend(self.moves.iter().map(|m| PlannedFile {
                path: m.to.clone(),
                size: m.size,
                modtime_only: false,
            }));
            self.new_bytes += self.move_bytes;
        } else if was_upload && !upload {
            let moved: HashSet<&str> = self.moves.iter().map(|m| m.to.as_str()).collect();
            self.new.retain(|f| !moved.contains(f.path.as_str()));
            self.new_bytes = self.new_bytes.saturating_sub(self.move_bytes);
        }
        self.moves_choice = choice;
    }

    /// The moves will run (or would, for a dry run that did not ask).
    pub fn moving(&self) -> bool {
        !self.moves.is_empty() && self.moves_choice != MovesChoice::Upload
    }

    /// What the real copy should end at once `moved_files` (of
    /// `moved_bytes`) have moved: rclone counts a check for every file on
    /// both sides, changed or not, in the dry run and the copy alike, so a
    /// moved file, now on the destination under its new name, is one more
    /// check; it sends every new and changed file except those whose time
    /// alone is set, and a move that failed is uploaded instead (checked
    /// against rclone 1.75.2).
    pub fn expected(&self, moved_files: u64, moved_bytes: u64) -> Expected {
        let changed = self.changed.iter().filter(|f| !f.modtime_only).count();
        let mut transfers = (self.new.len() + changed) as u64;
        let mut bytes = self.total_bytes();
        if self.moves_choice == MovesChoice::Move {
            transfers += (self.moves.len() as u64).saturating_sub(moved_files);
            bytes += self.move_bytes.saturating_sub(moved_bytes);
        }
        Expected {
            checks: self.checked + moved_files,
            transfers,
            bytes,
        }
    }
}

/// Everything a run of one job needs.
pub struct Run<'a> {
    pub rclone: &'a Rclone,
    pub paths: &'a Paths,
    pub accounts: &'a [Account],
    pub job: &'a Job,
    pub dry_run: bool,
    /// Skip the preview confirmation.
    pub yes: bool,
    /// Questions can be asked (a terminal, not `--json`).
    pub interactive: bool,
    /// Two-way: how the folder-deletion prompt is answered.
    pub folders: FolderPolicy,
    /// Two-way: start with a resync (`--resync`).
    pub resync: bool,
    /// One-way: how the question about moved files is answered (`--moves`).
    pub moves: MovesPolicy,
}

/// How a destination's run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// It ran, or failed trying; the summary says which.
    Finished(Box<Summary>),
    /// The user said no to the preview. Nothing was copied or recorded.
    Declined,
}

/// Where rclone copies to.
pub(super) struct Target {
    /// A local path or `remote:path`.
    pub target: String,
    /// `--fast-list`, for a OneDrive destination only.
    pub fast_list: bool,
}

/// Runs one one-way destination from validation to the run record.
pub async fn run_destination(
    run: &Run<'_>,
    dest: &Destination,
    info: &DestinationInfo,
    reporter: &mut dyn Reporter,
    interrupts: &mut InterruptGuard,
) -> Ended {
    let started = now();
    reporter.destination_started(info);
    let mut ctx = Recorder::new(run, dest, info, started);
    let mut log = match ctx.open_log() {
        Ok(log) => Some(log),
        Err(e) => {
            reporter.problem(&format!(
                "Could not create the run log, carrying on without it\n  {e}"
            ));
            None
        }
    };
    if let Some(log) = &log {
        ctx.log = Some(log.path().to_path_buf());
    }

    // 1. Validate.
    reporter.step(Step::Checking);
    let target = match validate(run, dest).await {
        Ok(target) => target,
        // Ctrl-C while `rclone about` or diskutil ran can make them fail:
        // that is a cancelled run, not a failed one.
        Err(_) if interrupts.interrupted() => {
            let summary = ctx.summary(Outcome::Cancelled, 0);
            return ctx.finish(summary, reporter);
        }
        Err(message) => {
            note(&mut log, "refused", &message);
            reporter.problem(&message);
            let summary = ctx.failed(first_line(&message), 0);
            return ctx.finish(summary, reporter);
        }
    };
    // Ctrl-C during the checks (an `rclone about` can take seconds).
    if interrupts.interrupted() {
        let summary = ctx.summary(Outcome::Cancelled, 0);
        return ctx.finish(summary, reporter);
    }

    // 2. Preview: the dry run, the destination listing and the source
    // listing, side by side.
    reporter.step(Step::Previewing);
    let clock = Instant::now();
    let excludes = &excludes_of(run.job);
    let exclude_file = ctx.exclude_file(log.as_ref(), excludes);
    let source = run.job.source.clone();
    let onedrive = matches!(dest, Destination::Onedrive { .. });
    let args = rc::copy_args(
        &source,
        &target.target,
        excludes,
        exclude_file.as_deref(),
        target.fast_list,
        true,
    );
    let mut dry_events = Vec::new();
    // The listings run beside the dry run, the destination's counting the
    // files it has read so far. They are dropped (and their rclone killed)
    // as soon as the run is stopped, so Ctrl-C never waits for them. A
    // OneDrive listing carries each file's QuickXorHash, which OneDrive
    // keeps with the file, for move detection; a local one never does.
    let destination_listed = AtomicU64::new(0);
    let listing_done = AtomicBool::new(false);
    let listing = listing::list_files(
        run.rclone,
        &target.target,
        excludes,
        exclude_file.as_deref(),
        target.fast_list,
        onedrive,
        Some(&destination_listed),
    );
    tokio::pin!(listing);
    // The source, as the copy sees it: what is only on the destination,
    // exactly, and the source's modification times for move detection.
    let source_listing = listing::list_files(
        run.rclone,
        &source,
        excludes,
        exclude_file.as_deref(),
        false,
        false,
        None,
    );
    tokio::pin!(source_listing);
    let mut listed = None;
    let mut source_listed = None;
    let mut looking = Looking::new(Step::Previewing);
    let copied = {
        // Each stats line (about once a second) brings the live figures.
        let mut on = |p: Progress| match p {
            Progress::Event(e) => {
                if let Event::Stats(s) = &e {
                    looking.take_stats(s);
                    looking.set_elapsed(clock.elapsed());
                    looking.destination_listed = Some(destination_listed.load(Ordering::Relaxed));
                    looking.destination_done = listing_done.load(Ordering::Relaxed);
                    reporter.looking(&looking);
                }
                dry_events.push(e);
            }
            Progress::Stopping => reporter.stopping(),
            Progress::Killing => {}
        };
        let dry = exec::run_streaming(&run.rclone.path, &args, log.as_mut(), interrupts, &mut on);
        tokio::pin!(dry);
        loop {
            tokio::select! {
                result = &mut dry => break result,
                l = &mut listing, if listed.is_none() => {
                    listing_done.store(true, Ordering::Relaxed);
                    listed = Some(l);
                }
                l = &mut source_listing, if source_listed.is_none() => source_listed = Some(l),
            }
        }
    };
    let finished = match copied {
        Ok(f) => f,
        Err(e) => {
            let message = format!("Could not run rclone\n  {e}");
            reporter.problem(&message);
            let summary = ctx.failed(first_line(&message), elapsed(clock));
            return ctx.finish(summary, reporter);
        }
    };
    if finished.cancelled || interrupts.interrupted() {
        let summary = ctx.summary(Outcome::Cancelled, elapsed(clock));
        return ctx.finish(summary, reporter);
    }
    if listed.is_none() || source_listed.is_none() {
        // The dry run is done and a listing is not: keep the figures
        // moving, so a long listing never looks stuck.
        looking.dry_run_done = true;
        let mut tick = tokio::time::interval(LOOKING_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while listed.is_none() || source_listed.is_none() {
            tokio::select! {
                l = &mut listing, if listed.is_none() => {
                    listing_done.store(true, Ordering::Relaxed);
                    listed = Some(l);
                }
                l = &mut source_listing, if source_listed.is_none() => source_listed = Some(l),
                _ = tick.tick() => {
                    looking.set_elapsed(clock.elapsed());
                    looking.destination_listed =
                        Some(destination_listed.load(Ordering::Relaxed));
                    looking.destination_done = listing_done.load(Ordering::Relaxed);
                    reporter.looking(&looking);
                }
                () = interrupts.next() => {
                    let summary = ctx.summary(Outcome::Cancelled, elapsed(clock));
                    return ctx.finish(summary, reporter);
                }
            }
        }
    }
    let (Some(listed), Some(source_listed)) = (listed, source_listed) else {
        unreachable!("both listings are in");
    };
    // A dry run can fail on single files (an unreadable file whose time
    // changed is hashed). Those are the copy's per-file errors to come, so
    // the run carries on; anything else stops this destination.
    let roots = [source.as_str(), target.target.as_str()];
    let mut dry = Accumulator::default().with_roots(&roots);
    for e in &dry_events {
        dry.apply(e);
    }
    let file_errors_only = finished.exit.may_be_partial()
        && !dry.file_errors().is_empty()
        && dry.general_error().is_none();
    if !finished.exit.is_success() && !file_errors_only {
        let acc = dry;
        let mut summary = ctx.summary(Outcome::Failed, elapsed(clock));
        summary.errors = acc.summary_errors(&finished);
        if summary.errors.is_empty() {
            summary.errors.push(FileError {
                path: String::new(),
                reason: format!("the preview {}", finished.exit.describe()),
            });
        }
        return ctx.finish(summary, reporter);
    }
    let entries: Vec<Entry> = match listed {
        Ok(entries) => entries,
        // A destination that does not exist yet is empty: everything is new.
        Err(ListError::NotFound { .. }) => Vec::new(),
        Err(e) => {
            let message = format!("Could not list the destination\n  {e}");
            note(&mut log, "list-failed", &message);
            reporter.problem(&message);
            let summary = ctx.failed(
                first_line(&message).to_string() + ": " + &e.to_string(),
                elapsed(clock),
            );
            return ctx.finish(summary, reporter);
        }
    };
    let listing: HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
    // How names compare between the two sides, for move detection: a
    // OneDrive destination ignores case; a local folder does on macOS, and
    // on Linux only on a FAT, exFAT or NTFS drive or a Windows drive.
    let case = mv::Case::between(
        mv::Case::of(volume::ignores_case(Path::new(&source)).await),
        if onedrive {
            mv::Case::Insensitive
        } else {
            mv::Case::of(volume::ignores_case(Path::new(&target.target)).await)
        },
    );
    let stats = dry_events.iter().rev().find_map(|e| match e {
        Event::Stats(s) => Some(s),
        _ => None,
    });
    let mut preview = Preview::build(&dry_events, stats, &listing, entries.len() as u64);
    preview.job = run.job.name.clone();
    preview.destination_id = dest.id().to_string();
    preview.destination = dest.display();
    preview.excludes = run.job.excludes.clone();
    preview.copy_hidden = run.job.copy_hidden;
    preview.effective_excludes = excludes.clone();
    preview.destination_type = info.kind.to_string();
    preview.dry_run = run.dry_run;

    // Files moved locally whose old copy is still on the destination.
    match source_listed {
        Ok(source_entries) => {
            // Compared composed, and ignoring case where either side
            // does (`case`): there `Report.pdf` renamed to `report.pdf` is
            // the same file, not one left alone; where neither does (two
            // ext4 folders) it is a move.
            let source_keys: HashSet<String> =
                source_entries.iter().map(|e| case.key(&e.path)).collect();
            let dest_only: Vec<Entry> = entries
                .iter()
                .filter(|e| !source_keys.contains(&case.key(&e.path)))
                .cloned()
                .collect();
            preview.destination_only = dest_only.len() as u64;
            let dest_paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
            // Hashing a large candidate can take a while: keep the figures
            // moving and say what is being checked.
            let possible = mv::candidates(&preview.new, &dest_only).0.len() as u64;
            looking.dry_run_done = true;
            looking.destination_done = true;
            looking.checking_moves = (possible > 0).then_some(possible);
            if possible > 0 {
                looking.set_elapsed(clock.elapsed());
                reporter.looking(&looking);
            }
            let found = {
                let finding = find_moves(
                    run.rclone,
                    case,
                    &source,
                    &target.target,
                    onedrive,
                    &preview.new,
                    &source_entries,
                    &dest_only,
                    &dest_paths,
                );
                tokio::pin!(finding);
                let mut tick = tokio::time::interval(LOOKING_EVERY);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                tick.reset();
                loop {
                    tokio::select! {
                        found = &mut finding => break found,
                        _ = tick.tick(), if possible > 0 => {
                            looking.set_elapsed(clock.elapsed());
                            reporter.looking(&looking);
                        }
                        () = interrupts.next() => {
                            let summary = ctx.summary(Outcome::Cancelled, elapsed(clock));
                            return ctx.finish(summary, reporter);
                        }
                    }
                }
            };
            {
                match found {
                    Ok(found) => {
                        if !found.unhashed.is_empty() {
                            note_value(
                                &mut log,
                                serde_json::json!({
                                    "onefolder": "moves-not-checked",
                                    "message": "these files could not be hashed, so they are uploaded",
                                    "files": found.unhashed.len(),
                                    "paths": found.unhashed.iter().take(20).collect::<Vec<_>>(),
                                }),
                            );
                        }
                        let (groups, moves) = (found.groups, found.moves);
                        if !moves.is_empty() {
                            note_value(
                                &mut log,
                                serde_json::json!({
                                    "onefolder": "moves-found",
                                    "files": moves.len(),
                                    "bytes": moves.iter().map(|m| m.size).sum::<u64>(),
                                    "groups": groups.len(),
                                }),
                            );
                        }
                        preview.apply_moves(groups, moves);
                    }
                    Err(e) => note(&mut log, "moves-not-checked", &e),
                }
            }
        }
        Err(e) => note(
            &mut log,
            "moves-not-checked",
            &format!("could not list the source: {e}"),
        ),
    }
    // The answer is known before the preview when the flag or the default
    // gives it, so the preview already says what happens to moved files.
    if !preview.moves.is_empty() {
        if let MovesPolicy::Answer(answer, _) = run.moves {
            preview.choose_moves(choice_of(answer));
        }
    }
    reporter.preview(&preview);
    if let (false, MovesPolicy::Answer(answer, by)) = (preview.moves.is_empty(), run.moves) {
        decided(&mut log, &preview, answer, by);
        reporter.moves_decided(&preview, answer, by);
    }

    // 3. Ask, or stop after the preview.
    if run.dry_run {
        let outcome = if dry.file_errors().is_empty() {
            Outcome::Ok
        } else {
            Outcome::Partial
        };
        let mut summary = ctx.summary(outcome, elapsed(clock));
        summary.errors = dry.file_errors().to_vec();
        summary.added.remote = preview.new.len() as u64;
        summary.updated.remote = preview.changed.len() as u64;
        summary.bytes = preview.total_bytes();
        summary.unchanged = Some(preview.unchanged);
        let would_move = if preview.moving() {
            preview.moves.len() as u64
        } else {
            0
        };
        summary.destination_only = Some(preview.destination_only.saturating_sub(would_move));
        if would_move > 0 {
            summary.moved = Some(Moved {
                files: would_move,
                bytes: preview.move_bytes,
            });
        }
        return ctx.finish(summary, reporter);
    }
    // Nothing to send or move: no question, no copy.
    if preview.new.is_empty()
        && preview.changed.is_empty()
        && preview.moves.is_empty()
        && dry.file_errors().is_empty()
    {
        reporter.note("Nothing to copy.");
        let mut summary = ctx.summary(Outcome::Ok, elapsed(clock));
        summary.unchanged = Some(preview.unchanged);
        summary.destination_only = Some(preview.destination_only);
        return ctx.finish(summary, reporter);
    }
    // Ctrl-C at a question quits, as at any prompt: nothing has run yet.
    // The run screen reads Ctrl-C as a key instead, answers no and stops
    // the run.
    let declined = |log: &mut Option<RunLog>,
                    reporter: &mut dyn Reporter,
                    interrupts: &InterruptGuard,
                    what: &str| {
        note(log, "declined", what);
        let reason = if interrupts.interrupted() {
            "not run, the run was stopped"
        } else {
            "not confirmed, nothing copied"
        };
        reporter.destination_skipped(info, reason);
        Ended::Declined
    };
    if !preview.moves.is_empty() && run.moves == MovesPolicy::Ask {
        let answer = interrupts.unguarded(|| reporter.confirm(&Question::Moves(&preview)));
        if interrupts.interrupted() {
            return declined(
                &mut log,
                reporter,
                interrupts,
                "the run was stopped at the question about moved files",
            );
        }
        let answer = match answer {
            Answer::Yes | Answer::YesToRest => MovesAnswer::Move,
            Answer::No => MovesAnswer::Copy,
        };
        preview.choose_moves(choice_of(answer));
        decided(&mut log, &preview, answer, DecidedBy::Asked);
        reporter.moves_decided(&preview, answer, DecidedBy::Asked);
    }
    if !run.yes {
        let answer = interrupts.unguarded(|| reporter.confirm_preview(&preview));
        if answer == Answer::No {
            return declined(
                &mut log,
                reporter,
                interrupts,
                "the preview was not confirmed",
            );
        }
    }

    // 4. The moves, then the real copy, unless Ctrl-C came while the
    // preview was listed.
    if interrupts.interrupted() {
        let summary = ctx.summary(Outcome::Cancelled, 0);
        return ctx.finish(summary, reporter);
    }
    let clock = Instant::now();
    let mut moved = MovesDone::default();
    if preview.moves_choice == MovesChoice::Move && !preview.moves.is_empty() {
        reporter.step(Step::Moving);
        moved = run_moves(
            run.rclone, &target, &preview, case, &mut log, interrupts, reporter,
        )
        .await;
        if interrupts.interrupted() {
            let mut summary = ctx.summary(Outcome::Cancelled, elapsed(clock));
            summary.moved = moved.counts();
            summary.errors = moved.errors;
            summary.unchanged = Some(preview.unchanged);
            summary.destination_only = Some(preview.destination_only.saturating_sub(moved.files));
            return ctx.finish(summary, reporter);
        }
    }
    reporter.step(Step::Copying);
    reporter.expected(&preview.expected(moved.files, moved.bytes));
    let args = rc::copy_args(
        &source,
        &target.target,
        excludes,
        exclude_file.as_deref(),
        target.fast_list,
        false,
    );
    let mut acc = Accumulator::new(&preview, listing).with_roots(&roots);
    let result = {
        let mut on = |p: Progress| match p {
            Progress::Event(e) => match acc.apply(&e) {
                Applied::Stats(s) => reporter.stats(&s),
                Applied::File(f) => reporter.file(&f),
                Applied::Error(err) => reporter.file_error(&err),
                Applied::Phase(_) | Applied::Nothing => {}
            },
            Progress::Stopping => reporter.stopping(),
            Progress::Killing => {}
        };
        exec::run_streaming(&run.rclone.path, &args, log.as_mut(), interrupts, &mut on).await
    };
    reporter.step(Step::Finishing);

    // 5. Summarise and record.
    let finished: Finished = match result {
        Ok(f) => f,
        Err(e) => {
            let message = format!("Could not run rclone\n  {e}");
            reporter.problem(&message);
            let mut summary = ctx.failed(first_line(&message), elapsed(clock));
            summary.moved = moved.counts();
            summary.errors.extend(moved.errors);
            return ctx.finish(summary, reporter);
        }
    };
    let mut outcome = acc.outcome(&finished);
    if outcome == Outcome::Ok && !moved.errors.is_empty() {
        outcome = Outcome::Partial;
    }
    let mut summary = ctx.summary(outcome, elapsed(clock));
    summary.added.remote = acc.added;
    summary.updated.remote = acc.updated;
    summary.bytes = acc.bytes();
    // From the preview: rclone's checks count again on every retry.
    summary.unchanged = Some(preview.unchanged);
    summary.destination_only = Some(preview.destination_only.saturating_sub(moved.files));
    summary.moved = moved.counts();
    let mut errors = moved.errors;
    errors.extend(acc.summary_errors(&finished));
    summary.errors = errors;
    ctx.finish(summary, reporter)
}

/// How many moves run at once: a handful, so a OneDrive account is not
/// flooded with requests and each one is logged before and after.
pub const MOVE_CONCURRENCY: usize = 4;

fn choice_of(answer: MovesAnswer) -> MovesChoice {
    match answer {
        MovesAnswer::Move => MovesChoice::Move,
        MovesAnswer::Copy => MovesChoice::Upload,
    }
}

/// Logs what was decided about the moved files.
fn decided(log: &mut Option<RunLog>, preview: &Preview, answer: MovesAnswer, by: DecidedBy) {
    let mut value = serde_json::json!({
        "onefolder": "moves-decided",
        "answer": answer,
        "by": by,
        "files": preview.moves.len(),
        "bytes": preview.move_bytes,
    });
    if by == DecidedBy::NotAsked {
        value["message"] = serde_json::json!(
            "--moves ask, and there is no terminal to ask on: the moved files are uploaded \
             again, nothing is moved"
        );
    }
    note_value(log, value);
}

/// Finds the files moved locally whose old copy is on the destination
/// (see [`super::moves`]): candidates by size, then the QuickXorHash of
/// just those files on both sides (`rclone hashsum quickxor` on the
/// source; on the destination the OneDrive listing's hashes, or `rclone
/// hashsum` for a local folder), then pairs and groups. An error means
/// the check could not be made; the files are then simply uploaded.
#[allow(clippy::too_many_arguments)]
async fn find_moves(
    rclone: &Rclone,
    case: mv::Case,
    source: &str,
    target: &str,
    onedrive: bool,
    new: &[PlannedFile],
    source_entries: &[Entry],
    dest_only: &[Entry],
    dest_paths: &[String],
) -> Result<Found, String> {
    let (source_paths, dest_candidates) = mv::candidates(new, dest_only);
    if source_paths.is_empty() {
        return Ok(Found::default());
    }
    let source_hashes = listing::hashsum(rclone, source, &source_paths);
    let dest_hashes = async {
        if onedrive {
            Ok(dest_only
                .iter()
                .filter_map(|e| Some((e.path.clone(), e.quickxor()?.to_string())))
                .collect())
        } else {
            listing::hashsum(rclone, target, &dest_candidates).await
        }
    };
    let (source_hashes, dest_hashes) = tokio::join!(source_hashes, dest_hashes);
    let source_hashes = source_hashes.map_err(|e| format!("could not hash the source: {e}"))?;
    let dest_hashes = dest_hashes.map_err(|e| format!("could not hash the destination: {e}"))?;
    let times: std::collections::HashMap<String, String> = source_entries
        .iter()
        .map(|e| (case.key(&e.path), e.mod_time.clone()))
        .collect();
    let unhashed: Vec<String> = source_paths
        .iter()
        .filter(|p| !source_hashes.contains_key(*p))
        .cloned()
        .collect();
    let pairs = mv::pair(case, new, &times, dest_only, &source_hashes, &dest_hashes);
    let (groups, moves) = mv::group(case, &pairs, dest_paths);
    Ok(Found {
        groups,
        moves,
        unhashed,
    })
}

/// What [`find_moves`] found, and the candidates of the source it could
/// not hash (uploaded as usual; logged so it is not silent).
#[derive(Debug, Default)]
struct Found {
    groups: Vec<MoveGroup>,
    moves: Vec<PlannedMove>,
    unhashed: Vec<String>,
}

/// What the moves did.
#[derive(Debug, Default)]
struct MovesDone {
    files: u64,
    bytes: u64,
    /// One per group that failed, by its new path.
    errors: Vec<FileError>,
}

impl MovesDone {
    /// For the summary: only when something moved.
    fn counts(&self) -> Option<Moved> {
        (self.files > 0).then_some(Moved {
            files: self.files,
            bytes: self.bytes,
        })
    }
}

/// One `rclone moveto`, for a group or for one file of a folder group that
/// could not move as a whole.
#[derive(Debug, Clone)]
struct MoveJob {
    from: String,
    to: String,
    files: u64,
    bytes: u64,
    folder: bool,
}

/// Moves the files on the destination: folder moves first (each checked
/// first: nothing else in the old folder, unfiltered, and no new folder
/// yet; otherwise its files move one by one), then single files,
/// [`MOVE_CONCURRENCY`] at a time. Each move is logged before and after.
/// A failed move is a per-file error, and the copy after it uploads the
/// file instead. Ctrl-C stops between moves: those running finish (each
/// is in its own process group, so the key does not cut one in half), a
/// second Ctrl-C stops them too.
async fn run_moves(
    rclone: &Rclone,
    target: &Target,
    preview: &Preview,
    case: mv::Case,
    log: &mut Option<RunLog>,
    interrupts: &mut InterruptGuard,
    reporter: &mut dyn Reporter,
) -> MovesDone {
    let mut done = MovesDone::default();
    let clock = Instant::now();
    let mut looking = Looking::new(Step::Moving);
    looking.to_move = preview.moves.len() as u64;
    reporter.looking(&looking);
    let place = super::twoway::place(&preview.destination_type);

    let mut folders = Vec::new();
    let mut singles = Vec::new();
    for (i, g) in preview.move_groups.iter().enumerate() {
        let job = MoveJob {
            from: g.from.clone(),
            to: g.to.clone(),
            files: g.files,
            bytes: g.bytes,
            folder: g.folder,
        };
        if !g.folder {
            singles.push(job);
            continue;
        }
        if interrupts.interrupted() {
            return done;
        }
        let inside: Vec<String> = preview
            .moves
            .iter()
            .filter(|m| m.group == i)
            .map(|m| relative_to(&m.from, &g.from))
            .collect();
        match folder_ready(rclone, target, g, &inside, case).await {
            Ok(()) => folders.push(job),
            Err(why) => {
                note_value(
                    log,
                    serde_json::json!({
                        "onefolder": "move-folder-split",
                        "from": g.from, "to": g.to, "files": g.files, "reason": why,
                    }),
                );
                singles.extend(
                    preview
                        .moves
                        .iter()
                        .filter(|m| m.group == i)
                        .map(|m| MoveJob {
                            from: m.from.clone(),
                            to: m.to.clone(),
                            files: 1,
                            bytes: m.size,
                            folder: false,
                        }),
                );
            }
        }
    }

    let mut presses = 0;
    for phase in [folders, singles] {
        // Each new parent folder is made first, one at a time: moves into
        // the same new folder side by side would each try to create it,
        // and OneDrive can refuse the second ("name already exists").
        let mut parents: Vec<String> = phase
            .iter()
            .filter_map(|j| j.to.rsplit_once('/').map(|(parent, _)| parent.to_string()))
            .collect();
        parents.sort();
        parents.dedup();
        for parent in parents {
            if presses > 0 || interrupts.interrupted() {
                break;
            }
            let folder = listing::join(&target.target, &parent);
            if let Err(e) = listing::mkdir(rclone, &folder).await {
                // The move tries again on its own; a failure shows there.
                note_value(
                    log,
                    serde_json::json!({
                        "onefolder": "mkdir-failed", "folder": parent, "message": e.to_string(),
                    }),
                );
            }
        }
        if interrupts.interrupted() {
            break;
        }
        let mut queue = phase.into_iter();
        let mut set: tokio::task::JoinSet<(MoveJob, Result<exec::Captured, exec::ExecError>)> =
            tokio::task::JoinSet::new();
        loop {
            while presses == 0 && set.len() < MOVE_CONCURRENCY {
                let Some(job) = queue.next() else { break };
                let args = rc::moveto_args(
                    &listing::join(&target.target, &job.from),
                    &listing::join(&target.target, &job.to),
                );
                note_value(log, move_note("move", &job, None));
                if let Some(log) = log.as_mut() {
                    log.command(Path::new(&rclone.path), &args);
                }
                let program = PathBuf::from(&rclone.path);
                set.spawn(async move {
                    let result = exec::run_capture_detached(&program, &args).await;
                    (job, result)
                });
            }
            if set.is_empty() {
                break;
            }
            tokio::select! {
                joined = set.join_next() => {
                    let Some(Ok((job, result))) = joined else { continue };
                    let failure = match &result {
                        Ok(out) if out.status.success() => None,
                        Ok(out) => Some(move_failure(&out.stderr)),
                        Err(e) => Some(e.to_string()),
                    };
                    match failure {
                        None => {
                            note_value(log, move_note("moved", &job, None));
                            done.files += job.files;
                            done.bytes += job.bytes;
                        }
                        Some(reason) => {
                            note_value(log, move_note("move-failed", &job, Some(&reason)));
                            let error = move_error(&job, &reason, place);
                            reporter.file_error(&error);
                            done.errors.push(error);
                        }
                    }
                    looking.moved = done.files;
                    looking.set_elapsed(clock.elapsed());
                    reporter.looking(&looking);
                }
                () = interrupts.next() => {
                    presses += 1;
                    if presses == 1 {
                        reporter.stopping();
                    } else {
                        set.abort_all();
                    }
                }
            }
        }
        if presses > 0 || interrupts.interrupted() {
            break;
        }
    }
    done
}

/// `Old/sub/a.txt` relative to `Old`: `sub/a.txt`.
fn relative_to(path: &str, folder: &str) -> String {
    path.strip_prefix(folder)
        .map(|rest| rest.trim_start_matches('/'))
        .unwrap_or(path)
        .to_string()
}

/// A folder move goes ahead only when the old folder holds exactly the
/// files being moved (`inside`, relative to it), counting the files the
/// sync leaves out (a move takes the whole folder), and the new folder
/// does not exist yet. Paths compare as the moves were found (composed,
/// ignoring case where `case` says so), so a file added since the preview
/// stops the folder move.
async fn folder_ready(
    rclone: &Rclone,
    target: &Target,
    g: &MoveGroup,
    inside: &[String],
    case: mv::Case,
) -> Result<(), String> {
    let old = listing::join(&target.target, &g.from);
    match listing::list_filtered(rclone, &old, &[], target.fast_list).await {
        Ok(entries) => {
            let there: HashSet<String> = entries.iter().map(|e| case.key(&e.path)).collect();
            let moving: HashSet<String> = inside.iter().map(|p| case.key(p)).collect();
            if there != moving {
                let extra = there.difference(&moving).count();
                let missing = moving.difference(&there).count();
                return Err(format!(
                    "the old folder holds {} files: {extra} not among the moved files, \
                     {missing} of those no longer there",
                    entries.len()
                ));
            }
        }
        Err(e) => return Err(format!("could not list the old folder: {e}")),
    }
    match listing::exists(rclone, &listing::join(&target.target, &g.to)).await {
        Ok(false) => Ok(()),
        Ok(true) => Err("the new folder exists already".to_string()),
        Err(e) => Err(format!("could not check the new folder: {e}")),
    }
}

fn move_note(what: &str, job: &MoveJob, reason: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "onefolder": what,
        "from": job.from,
        "to": job.to,
        "size": job.bytes,
        "files": job.files,
        "group": if job.folder { "folder" } else { "file" },
    });
    if let Some(r) = reason {
        v["message"] = serde_json::Value::String(r.to_string());
    }
    v
}

/// The per-file error for a move that failed: what happened, in words,
/// and that the copy uploads the file (or the folder's files) instead.
fn move_error(job: &MoveJob, reason: &str, place: &str) -> FileError {
    let why = if reason.contains("is a directory") {
        format!("a folder with that name is already on {place}")
    } else {
        reason.to_string()
    };
    let (path, what) = if job.folder {
        (format!("{}/", job.to), "its files")
    } else {
        (job.to.clone(), "it")
    };
    FileError {
        path,
        reason: format!(
            "not moved from {}{} on {place} ({why}), so the copy uploads {what} instead",
            job.from,
            if job.folder { "/" } else { "" },
        ),
    }
}

/// rclone's reason for a failed `moveto`, from its JSON log on stderr: the
/// first error that is not the "Attempt 1/3 failed" summary, or else that
/// summary without its "Attempt ... and:" lead.
fn move_failure(stderr: &str) -> String {
    let messages: Vec<String> = stderr
        .lines()
        .filter_map(|l| match crate::rclone::events::decode(l) {
            Event::Error(e) => Some(e.message.trim().to_string()),
            _ => None,
        })
        .collect();
    if let Some(m) = messages.iter().find(|m| !m.starts_with("Attempt ")) {
        return m.clone();
    }
    if let Some(m) = messages.first() {
        return m
            .split_once(" and: ")
            .map_or(m.as_str(), |(_, rest)| rest)
            .to_string();
    }
    crate::rclone::remotes::clean_message(stderr)
}

/// The source exists, is a folder and can be read.
fn source_folder(source: &Path, job: &str) -> Result<(), String> {
    match fs::metadata(source) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "Source is not a folder\n  {} is a file.",
                source.display()
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "Source folder missing\n  {} does not exist. Edit the job with: onefolder jobs edit {job}",
                source.display(),
            ))
        }
        Err(e) => return Err(format!("Source not readable\n  {}: {e}", source.display())),
    }
    if let Err(e) = fs::read_dir(source) {
        return Err(format!("Source not readable\n  {}: {e}", source.display()));
    }
    Ok(())
}

/// A local destination with no drive recorded: a folder, or missing
/// inside an existing parent (the copy creates it).
fn plain_destination(p: &Path, job: &str) -> Result<(), String> {
    let path = p.display();
    match fs::metadata(p) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(format!("Destination is not a folder\n  {path} is a file.")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if p.parent().is_some_and(Path::is_dir) {
                Ok(())
            } else {
                Err(format!(
                    "Destination folder missing\n  Neither {path} nor its parent folder exists. \
                     Create it, or edit the job with: onefolder jobs edit {job}"
                ))
            }
        }
        Err(e) => Err(format!("Destination not readable\n  {path}: {e}")),
    }
}

/// Checks the source and the destination before anything runs. The error
/// is a headline and indented detail lines, ready to print after a `✗`.
pub(super) async fn validate(run: &Run<'_>, dest: &Destination) -> Result<Target, String> {
    let source = Path::new(&run.job.source);
    if let Some(identity) = run.job.source_identity() {
        volume::check(source, &identity, false)
            .await
            .map_err(|e| e.message(&run.job.name))?;
    }
    // A hung network mount blocks stat and read_dir: give up after the
    // same limit as the drive checks.
    let (src, job) = (source.to_path_buf(), run.job.name.clone());
    volume::guarded(source, volume::DEADLINE, move || source_folder(&src, &job))
        .await
        .map_err(|e| e.message(&run.job.name))??;

    match dest {
        Destination::Local { path, .. } => {
            let p = Path::new(path);
            match dest.identity() {
                // A missing folder on the verified drive is created by the copy.
                Some(identity) => match volume::check(p, &identity, true).await {
                    Ok(()) => {}
                    // The drive is the right one but the folder is not there
                    // yet: check where the copy would create it.
                    Err(VolumeError::Missing { .. }) => volume::check_creatable(p, &identity)
                        .await
                        .map_err(|e| e.message(&run.job.name))?,
                    Err(e) => return Err(e.message(&run.job.name)),
                },
                // A folder that leads onto a drive (a symlink into
                // /Volumes) with no drive recorded could be an empty folder
                // left by an unplugged drive: refuse until it is recorded.
                None => match volume::leads_where(p).await {
                    (_, _, Some(e)) => return Err(e.message(&run.job.name)),
                    (true, real, None) => {
                        return Err(format!(
                            "No drive recorded for this destination\n  {path} leads to {} on a \
                             drive, but no drive was recorded for it, so OneFolder cannot tell \
                             the drive from an empty folder. Record it with: onefolder jobs edit \
                             {} --rerecord {}",
                            real.display(),
                            run.job.name,
                            dest.id()
                        ))
                    }
                    (false, _, None) => {
                        let (owned, job) = (p.to_path_buf(), run.job.name.clone());
                        volume::guarded(p, volume::DEADLINE, move || {
                            plain_destination(&owned, &job)
                        })
                        .await
                        .map_err(|e| e.message(&run.job.name))??;
                    }
                },
            }
            Ok(Target {
                target: path.clone(),
                fast_list: false,
            })
        }
        Destination::Onedrive { account, path, .. } => {
            let Some(acct) = run.accounts.iter().find(|a| &a.label == account) else {
                return Err(format!(
                    "Account {account} not found\n  Add it with: onefolder accounts add {account}"
                ));
            };
            if let Err(e) = remotes::about(run.rclone, &acct.remote, remotes::ABOUT_TIMEOUT).await {
                return Err(match e.kind() {
                    FailureKind::TokenExpired => format!(
                        "OneDrive account {account} needs signing in again\n  Run: onefolder accounts reconnect {account}"
                    ),
                    FailureKind::RemoteMissing => format!(
                        "OneDrive account {account} is missing from rclone's config\n  Run: onefolder accounts reconnect {account}"
                    ),
                    _ => format!("Could not reach OneDrive account {account}\n  {e}"),
                });
            }
            Ok(Target {
                target: format!("{}:{path}", acct.remote),
                fast_list: true,
            })
        }
    }
}

/// Builds the summary and the run record for one destination.
pub(super) struct Recorder<'a> {
    run: &'a Run<'a>,
    dest: &'a Destination,
    info: &'a DestinationInfo,
    pub started: jiff::Zoned,
    pub log: Option<PathBuf>,
}

impl<'a> Recorder<'a> {
    pub fn new(
        run: &'a Run<'a>,
        dest: &'a Destination,
        info: &'a DestinationInfo,
        started: jiff::Zoned,
    ) -> Self {
        Recorder {
            run,
            dest,
            info,
            started,
            log: None,
        }
    }

    fn log_dir(&self) -> PathBuf {
        self.run
            .paths
            .logs_dir()
            .join(&self.run.job.name)
            .join(self.dest.id())
    }

    pub fn open_log(&self) -> io::Result<RunLog> {
        RunLog::create(
            &self.log_dir(),
            &self.started.strftime("%Y-%m-%dT%H%M%S").to_string(),
        )
    }

    /// Writes the excludes to a file beside the log when there are more
    /// than [`rc::MAX_EXCLUDE_FLAGS`] of them (DESIGN.md), for `copy` and
    /// `lsjson`. `None` means repeated `--exclude` flags. Never a
    /// `--filters-file`, and never used for bisync, which always gets
    /// flags.
    pub fn exclude_file(&self, log: Option<&RunLog>, excludes: &[String]) -> Option<PathBuf> {
        if excludes.len() <= rc::MAX_EXCLUDE_FLAGS || !rc::excludes_fit_a_file(excludes) {
            return None;
        }
        let path = log?.path().with_extension("excludes");
        let mut text = excludes.join("\n");
        text.push('\n');
        fs::write(&path, text).ok()?;
        Some(path)
    }

    pub fn summary(&self, outcome: Outcome, duration_ms: u64) -> Summary {
        Summary {
            job: self.run.job.name.clone(),
            destination: self.info.display.clone(),
            destination_type: self.info.kind.to_string(),
            mode: self.info.mode,
            outcome,
            dry_run: self.run.dry_run,
            added: SideCounts::default(),
            updated: SideCounts::default(),
            deleted: SideCounts::default(),
            unchanged: None,
            bytes: 0,
            duration_ms,
            errors: Vec::new(),
            conflicts: Vec::new(),
            removed_folders: Vec::new(),
            moved: None,
            destination_only: None,
            log: self.log.as_deref().map(tilde),
        }
    }

    pub fn failed(&self, reason: impl Into<String>, duration_ms: u64) -> Summary {
        let mut s = self.summary(Outcome::Failed, duration_ms);
        s.errors.push(FileError {
            path: String::new(),
            reason: reason.into(),
        });
        s
    }

    /// Records the run in `state.json`, then hands the summary to the reporter.
    pub fn finish(&self, summary: Summary, reporter: &mut dyn Reporter) -> Ended {
        let record = RunRecord {
            started: rfc3339(&self.started),
            finished: rfc3339(&now()),
            mode: summary.mode,
            dry_run: summary.dry_run,
            outcome: summary.outcome,
            added: summary.added,
            updated: summary.updated,
            deleted: summary.deleted,
            bytes: summary.bytes,
            errors: summary.errors.len() as u64,
            log: self
                .log
                .as_deref()
                .map(|p| relative_log(&self.run.paths.data_dir, p))
                .unwrap_or_default(),
            rclone_version: self.run.rclone.version.to_string(),
            reason: reason_of(&summary),
        };
        if let Err(e) = save_record(self.run.paths, &self.run.job.name, self.dest.id(), record) {
            reporter.problem(&format!("Could not record this run\n  {e:#}"));
        }
        reporter.destination_finished(&summary);
        Ended::Finished(Box::new(summary))
    }
}

/// Why a run did not go to plan, for the run record: the first error
/// about the whole run (one without a path), such as "Backup drive not
/// mounted". `None` for a clean run, or one whose only errors were files.
pub fn reason_of(summary: &Summary) -> Option<String> {
    if summary.outcome == Outcome::Ok {
        return None;
    }
    summary
        .errors
        .iter()
        .find(|e| e.path.is_empty() && !e.reason.trim().is_empty())
        .map(|e| e.reason.trim().to_string())
}

/// Adds the record to `state.json` under `<job>/<dest-id>`: a real run in
/// `runs`, a dry run in `last_dry_run`, so a dry run never hides the last
/// real run. A state file that does not load is left alone rather than
/// overwritten.
pub fn save_record(
    paths: &Paths,
    job: &str,
    dest_id: &str,
    record: RunRecord,
) -> anyhow::Result<()> {
    State::update(paths, |state| state.record_run(job, dest_id, record))
}

/// The log path as the state file stores it: relative to the data directory.
fn relative_log(data_dir: &Path, log: &Path) -> String {
    log.strip_prefix(data_dir)
        .unwrap_or(log)
        .to_string_lossy()
        .into_owned()
}

pub(super) fn note(log: &mut Option<RunLog>, what: &str, message: &str) {
    note_value(
        log,
        serde_json::json!({"onefolder": what, "message": message}),
    );
}

/// Writes a line of OneFolder's own to the run log, if there is one.
pub(super) fn note_value(log: &mut Option<RunLog>, value: serde_json::Value) {
    if let Some(log) = log {
        log.note(&value);
    }
}

pub(super) fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or(message).trim()
}

pub(super) fn elapsed(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn now() -> jiff::Zoned {
    jiff::Zoned::now()
}

/// `2026-10-13T09:41:02+11:00`.
pub fn rfc3339(time: &jiff::Zoned) -> String {
    time.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rclone::events;

    fn fixture(name: &str) -> String {
        let path = crate::rclone::fixture_path(name);
        std::fs::read_to_string(path).unwrap()
    }

    fn last_stats(events: &[Event]) -> Option<&Stats> {
        events.iter().rev().find_map(|e| match e {
            Event::Stats(s) => Some(s),
            _ => None,
        })
    }

    #[test]
    fn preview_from_the_recorded_dry_run_and_listing() {
        let events = events::decode_all(&fixture("copy-dry-run.jsonl"));
        let entries = listing::parse_lsjson(&fixture("lsjson.json")).unwrap();
        let set: HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
        let p = Preview::build(&events, last_stats(&events), &set, entries.len() as u64);
        let mut new: Vec<&str> = p.new.iter().map(|f| f.path.as_str()).collect();
        new.sort();
        assert_eq!(new, ["Projects/2026/brief.docx", "new.txt"]);
        let mut changed: Vec<(&str, bool)> = p
            .changed
            .iter()
            .map(|f| (f.path.as_str(), f.modtime_only))
            .collect();
        changed.sort();
        assert_eq!(
            changed,
            [("Projects/2026/budget.xlsx", false), ("touched.txt", true)]
        );
        assert_eq!((p.new_bytes, p.changed_bytes), (12, 10));
        assert_eq!(p.unchanged, 1, "same.txt");
        assert_eq!(p.destination_only, 1, "only-on-destination.txt");
        assert_eq!(p.total_bytes(), 22);
    }

    #[test]
    fn preview_classification_over_a_synthetic_plan() {
        let skip = |action: &str, object: &str, size: u64, is_dir: bool| {
            Event::Skipped(events::Skipped {
                action: action.into(),
                object: object.into(),
                size: Some(size),
                is_dir,
            })
        };
        let plan = vec![
            skip("copy", "a/new.bin", 100, false),
            skip("copy", "a/old.bin", 50, false),
            skip("copy", "a/old.bin", 50, false), // repeated: counted once
            skip("update modification time", "t.txt", 9, false),
            skip("set directory modification time", "a", 96, true),
            skip("delete", "gone.txt", 3, false),
        ];
        let stats = Stats {
            checks: 5,
            ..Stats::default()
        };
        let listing: HashSet<String> = ["a/old.bin", "t.txt", "x", "y", "z", "only"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = Preview::build(&plan, Some(&stats), &listing, 6);
        assert_eq!(p.new.len(), 1);
        assert_eq!(p.changed.len(), 2);
        assert_eq!((p.new_bytes, p.changed_bytes), (100, 50));
        assert_eq!(p.unchanged, 3);
        assert_eq!(p.destination_only, 1);
        // An empty destination: everything is new, nothing is left alone.
        let p = Preview::build(&plan[..1], None, &HashSet::new(), 0);
        assert_eq!((p.new.len(), p.unchanged, p.destination_only), (1, 0, 0));
        // A decomposed name (NFD, as macOS can give it) against a composed
        // listing entry (NFC, as OneDrive gives it) is the same file.
        let nfd = "Re\u{301}sume\u{301}.docx";
        let nfc = "R\u{e9}sum\u{e9}.docx";
        let listing: HashSet<String> = [nfc.to_string()].into();
        let p = Preview::build(&[skip("copy", nfd, 10, false)], None, &listing, 1);
        assert_eq!((p.new.len(), p.changed.len()), (0, 1));
        assert_eq!(
            p.changed[0].path, nfd,
            "the path itself is kept as rclone gave it"
        );
        let listing: HashSet<String> = [nfd.to_string()].into();
        let p = Preview::build(&[skip("copy", nfc, 10, false)], None, &listing, 1);
        assert_eq!(p.changed.len(), 1);
    }

    /// The whole flow against the real rclone, with the scripted reporter:
    /// a "no" to the preview copies and records nothing; a "yes" copies.
    /// Skipped when rclone is not installed (the CI unit job has none; the
    /// integration job covers the flow end to end).
    #[tokio::test]
    async fn scripted_run_declines_then_copies() {
        use crate::model::{Destination, Mode};
        use crate::sync::reporter::Scripted;
        let Ok(rclone) = crate::rclone::detect::detect().await else {
            eprintln!("skipped: rclone is not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a.txt"), "a").unwrap();
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
        };
        let dest = Destination::local("d-0001".into(), dst.display().to_string());
        let mut job = Job::new("documents", src.display().to_string());
        job.destinations.push(dest.clone());
        let run = Run {
            rclone: &rclone,
            paths: &paths,
            accounts: &[],
            job: &job,
            dry_run: false,
            yes: false,
            interactive: false,
            folders: FolderPolicy::DefaultKeep,
            resync: false,
            moves: MovesPolicy::default(),
        };
        let info = DestinationInfo {
            job: "documents".into(),
            id: "d-0001".into(),
            kind: "local",
            display: dest.display(),
            mode: Mode::OneWay,
            index: 1,
            of: 1,
            dry_run: false,
        };
        let _turn = exec::SERIAL.lock().await;
        let mut guard = exec::interrupt_guard();

        let mut r = Scripted {
            answers: vec![Answer::No],
            ..Scripted::default()
        };
        let ended = run_destination(&run, &dest, &info, &mut r, &mut guard).await;
        assert_eq!(ended, Ended::Declined, "{:#?}", r.log);
        assert!(!dst.join("a.txt").exists());
        assert!(
            !paths.state_file().exists(),
            "a declined run is not recorded"
        );
        assert_eq!(r.previews[0].new.len(), 1);

        let mut r = Scripted::default();
        let Ended::Finished(summary) =
            run_destination(&run, &dest, &info, &mut r, &mut guard).await
        else {
            panic!("expected a summary: {:#?}", r.log);
        };
        assert_eq!(summary.outcome, Outcome::Ok);
        assert_eq!(summary.added.remote, 1);
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "a");
        // Stats lines come on rclone's clock; their place is not the point.
        let log: Vec<&str> = r
            .log
            .iter()
            .map(String::as_str)
            .filter(|l| *l != "stats")
            .collect();
        assert_eq!(
            log,
            [
                "start d-0001 1/1",
                "step Checking",
                "step Previewing",
                "looking",
                "preview new 1 changed 0",
                "confirm Yes",
                "step Copying",
                "file Destination Added a.txt",
                "step Finishing",
                "finished ok",
            ]
        );
        // The dry run's last stats line gave the live figures: one file of
        // one byte to copy, and the (empty) destination listed.
        let last = r.looking.last().expect("live figures during the preview");
        assert_eq!(last.step, Step::Previewing);
        assert_eq!((last.to_copy, last.to_copy_bytes), (1, 1));
        assert_eq!(last.destination_listed.map(|n| n <= 1), Some(true));
        // The copy's totals, from the preview, are where rclone ended.
        let end = r.last_stats.as_ref().expect("stats from the copy");
        assert_eq!(
            r.expected,
            [Expected {
                checks: end.total_checks,
                transfers: end.total_transfers,
                bytes: end.total_bytes,
            }]
        );
        assert_eq!((end.total_transfers, end.total_bytes), (1, 1));
        let state = State::load(&paths.state_file()).unwrap();
        assert_eq!(state.runs["documents/d-0001"].outcome, Outcome::Ok);
    }

    /// Moved files against the real rclone: a renamed folder and a moved
    /// file are found, asked about, and moved on a local destination; the
    /// copy after the moves sends nothing, and the next run finds every
    /// file unchanged. Skipped when rclone is not installed.
    #[tokio::test]
    async fn scripted_run_moves_instead_of_uploading() {
        use crate::model::{Destination, Mode};
        use crate::sync::reporter::Scripted;
        let Ok(rclone) = crate::rclone::detect::detect().await else {
            eprintln!("skipped: rclone is not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(src.join("Old")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a.txt"), "aaa").unwrap();
        fs::write(src.join("Old/x.txt"), "x1").unwrap();
        fs::write(src.join("Old/y.txt"), "y22").unwrap();
        fs::write(src.join("keep.txt"), "keep").unwrap();
        let seeded = std::process::Command::new(&rclone.path)
            .args([
                "copy",
                &src.display().to_string(),
                &dst.display().to_string(),
            ])
            .status()
            .unwrap();
        assert!(seeded.success());
        fs::create_dir_all(src.join("Docs")).unwrap();
        fs::rename(src.join("a.txt"), src.join("Docs/a.txt")).unwrap();
        fs::rename(src.join("Old"), src.join("New")).unwrap();

        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
        };
        let dest = Destination::local("d-0001".into(), dst.display().to_string());
        let mut job = Job::new("documents", src.display().to_string());
        job.destinations.push(dest.clone());
        let run = Run {
            rclone: &rclone,
            paths: &paths,
            accounts: &[],
            job: &job,
            dry_run: false,
            yes: false,
            interactive: true,
            folders: FolderPolicy::DefaultKeep,
            resync: false,
            moves: MovesPolicy::Ask,
        };
        let info = DestinationInfo {
            job: "documents".into(),
            id: "d-0001".into(),
            kind: "local",
            display: dest.display(),
            mode: Mode::OneWay,
            index: 1,
            of: 1,
            dry_run: false,
        };
        let _turn = exec::SERIAL.lock().await;
        let mut guard = exec::interrupt_guard();
        let mut r = Scripted::default();
        let Ended::Finished(summary) =
            run_destination(&run, &dest, &info, &mut r, &mut guard).await
        else {
            panic!("expected a summary: {:#?}", r.log);
        };
        let p = &r.previews[0];
        assert!(p.new.is_empty(), "{:?}", p.new);
        let groups: Vec<(&str, &str, u64, bool)> = p
            .move_groups
            .iter()
            .map(|g| (g.from.as_str(), g.to.as_str(), g.files, g.folder))
            .collect();
        assert_eq!(
            groups,
            [("Old", "New", 2, true), ("a.txt", "Docs/a.txt", 1, false)]
        );
        let log: Vec<&str> = r
            .log
            .iter()
            .map(String::as_str)
            .filter(|l| !matches!(*l, "stats" | "looking"))
            .collect();
        assert_eq!(
            log,
            [
                "start d-0001 1/1",
                "step Checking",
                "step Previewing",
                "preview new 0 changed 0",
                "confirm moves Yes",
                "moves 3 Move Asked",
                "confirm Yes",
                "step Moving",
                "step Copying",
                "step Finishing",
                "finished ok",
            ]
        );
        assert_eq!(summary.added.remote, 0);
        assert_eq!(summary.bytes, 0, "nothing uploaded");
        assert_eq!(summary.moved, Some(Moved { files: 3, bytes: 8 }));
        // The moved files are checked by the copy, not sent: the totals
        // from the preview are where rclone ended.
        let end = r.last_stats.as_ref().expect("stats from the copy");
        assert_eq!(
            r.expected,
            [Expected {
                checks: 4,
                transfers: 0,
                bytes: 0,
            }]
        );
        assert_eq!(
            (end.total_checks, end.total_transfers, end.total_bytes),
            (4, 0, 0)
        );
        assert_eq!(summary.destination_only, Some(0));
        for (rel, text) in [
            ("Docs/a.txt", "aaa"),
            ("New/x.txt", "x1"),
            ("New/y.txt", "y22"),
        ] {
            assert_eq!(fs::read_to_string(dst.join(rel)).unwrap(), text, "{rel}");
        }
        assert!(!dst.join("a.txt").exists());
        assert!(!dst.join("Old").exists(), "the folder moved as a whole");
        // The run log records each move before and after.
        let log_text = fs::read_to_string(summary_log(&paths, &summary)).unwrap();
        assert!(
            log_text.contains(r#""onefolder":"move","from":"Old","to":"New""#),
            "{log_text}"
        );
        assert!(log_text.contains(r#""onefolder":"moved","from":"a.txt","to":"Docs/a.txt""#));

        // The next run: everything unchanged, nothing to move or copy.
        let mut r = Scripted::default();
        let Ended::Finished(summary) =
            run_destination(&run, &dest, &info, &mut r, &mut guard).await
        else {
            panic!("expected a summary: {:#?}", r.log);
        };
        assert_eq!(summary.unchanged, Some(4), "{:#?}", r.log);
        assert!(r.previews[0].moves.is_empty());
        assert!(r.log.iter().any(|l| l == "note Nothing to copy."));
    }

    /// Ctrl-C part way through the moves: the moves under way finish, no
    /// more start, the run is cancelled with what moved, and the next run
    /// finds the rest as moves. Skipped when rclone is not installed.
    #[tokio::test]
    async fn ctrl_c_during_moves_then_the_rest_next_time() {
        use crate::model::{Destination, Mode};
        use crate::sync::reporter::Scripted;
        let Ok(rclone) = crate::rclone::detect::detect().await else {
            eprintln!("skipped: rclone is not installed");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        for i in 0..12 {
            fs::write(src.join(format!("a{i}.txt")), format!("content {i}")).unwrap();
        }
        let seeded = std::process::Command::new(&rclone.path)
            .args([
                "copy",
                &src.display().to_string(),
                &dst.display().to_string(),
            ])
            .status()
            .unwrap();
        assert!(seeded.success());
        for i in 0..12 {
            let folder = src.join(format!("d{i}"));
            fs::create_dir_all(&folder).unwrap();
            fs::rename(
                src.join(format!("a{i}.txt")),
                folder.join(format!("a{i}.txt")),
            )
            .unwrap();
        }
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
        };
        let dest = Destination::local("d-0001".into(), dst.display().to_string());
        let mut job = Job::new("documents", src.display().to_string());
        job.destinations.push(dest.clone());
        let run = Run {
            rclone: &rclone,
            paths: &paths,
            accounts: &[],
            job: &job,
            dry_run: false,
            yes: true,
            interactive: false,
            folders: FolderPolicy::DefaultKeep,
            resync: false,
            moves: MovesPolicy::default(),
        };
        let info = DestinationInfo {
            job: "documents".into(),
            id: "d-0001".into(),
            kind: "local",
            display: dest.display(),
            mode: Mode::OneWay,
            index: 1,
            of: 1,
            dry_run: false,
        };
        let _turn = exec::SERIAL.lock().await;
        let mut guard = exec::interrupt_guard();
        let mut r = Scripted {
            press_at_moved: Some(1),
            ..Scripted::default()
        };
        let Ended::Finished(first) = run_destination(&run, &dest, &info, &mut r, &mut guard).await
        else {
            panic!("expected a summary: {:#?}", r.log);
        };
        assert_eq!(first.outcome, Outcome::Cancelled, "{:#?}", r.log);
        assert!(r.log.iter().any(|l| l == "stopping"), "{:#?}", r.log);
        assert!(
            !r.log.iter().any(|l| l == "step Copying"),
            "no copy after Ctrl-C"
        );
        let moved = first.moved.expect("some moved").files;
        assert!((1..12).contains(&moved), "stopped part way: {moved} moved");
        let at_new = (0..12)
            .filter(|i| dst.join(format!("d{i}/a{i}.txt")).exists())
            .count() as u64;
        assert_eq!(at_new, moved, "each move is whole: done or not started");

        // The next run (a new guard, as a new process would have).
        drop(guard);
        let mut guard = exec::interrupt_guard();
        let mut r = Scripted::default();
        let Ended::Finished(second) = run_destination(&run, &dest, &info, &mut r, &mut guard).await
        else {
            panic!("expected a summary: {:#?}", r.log);
        };
        assert_eq!(second.outcome, Outcome::Ok, "{:#?}", r.log);
        assert_eq!(r.previews[0].moves.len() as u64, 12 - moved);
        assert_eq!(second.moved.map(|m| m.files), Some(12 - moved));
        assert_eq!(second.added.remote, 0);
        for i in 0..12 {
            assert!(dst.join(format!("d{i}/a{i}.txt")).exists());
            assert!(!dst.join(format!("a{i}.txt")).exists());
        }
    }

    /// The run log of a summary, on disk.
    fn summary_log(paths: &Paths, summary: &Summary) -> PathBuf {
        let shown = summary.log.as_deref().unwrap();
        let rel = shown.rsplit("logs/").next().unwrap();
        paths.logs_dir().join(rel)
    }

    #[test]
    fn moves_leave_new_and_come_back_when_uploaded_again() {
        let f = |p: &str, size| PlannedFile {
            path: p.into(),
            size,
            modtime_only: false,
        };
        let mut p = Preview {
            new: vec![f("New/a.txt", 3), f("fresh.txt", 5)],
            new_bytes: 8,
            ..Preview::default()
        };
        p.apply_moves(
            vec![MoveGroup {
                from: "Old/a.txt".into(),
                to: "New/a.txt".into(),
                files: 1,
                bytes: 3,
                folder: false,
            }],
            vec![PlannedMove {
                from: "Old/a.txt".into(),
                to: "New/a.txt".into(),
                size: 3,
                group: 0,
            }],
        );
        assert_eq!((p.new.len(), p.new_bytes, p.move_bytes), (1, 5, 3));
        assert_eq!(p.moves_choice, MovesChoice::Undecided);
        assert!(p.moving(), "a dry run that did not ask would move them");
        p.choose_moves(MovesChoice::Upload);
        assert_eq!((p.new.len(), p.new_bytes), (2, 8));
        assert!(!p.moving());
        p.choose_moves(MovesChoice::Upload);
        assert_eq!(p.new.len(), 2, "said twice, added once");
        p.choose_moves(MovesChoice::Move);
        assert_eq!((p.new.len(), p.new_bytes), (1, 5));
        assert_eq!(p.total_bytes(), 5);
    }

    #[test]
    fn copy_totals_from_the_preview_and_the_moves() {
        let f = |p: &str, size, modtime_only| PlannedFile {
            path: p.into(),
            size,
            modtime_only,
        };
        let mut p = Preview {
            new: vec![f("New/a.txt", 3, false), f("fresh.txt", 5, false)],
            new_bytes: 8,
            changed: vec![f("report.pdf", 7, false), f("time.txt", 0, true)],
            changed_bytes: 7,
            checked: 40,
            ..Preview::default()
        };
        let total = |e: Expected| (e.checks, e.transfers, e.bytes);
        // No moves: the dry run's checks, and the new and changed files
        // except the one whose time alone is set.
        assert_eq!(total(p.expected(0, 0)), (40, 3, 15));
        p.apply_moves(
            vec![MoveGroup {
                from: "Old/a.txt".into(),
                to: "New/a.txt".into(),
                files: 1,
                bytes: 3,
                folder: false,
            }],
            vec![PlannedMove {
                from: "Old/a.txt".into(),
                to: "New/a.txt".into(),
                size: 3,
                group: 0,
            }],
        );
        p.choose_moves(MovesChoice::Move);
        // Moved: one more check, nothing to send for it.
        assert_eq!(total(p.expected(1, 3)), (41, 2, 12));
        // The move failed: the copy uploads it.
        assert_eq!(total(p.expected(0, 0)), (40, 3, 15));
        // Uploaded again: as with no moves.
        p.choose_moves(MovesChoice::Upload);
        assert_eq!(total(p.expected(0, 0)), (40, 3, 15));
    }

    #[test]
    fn move_failures_read_like_rclone_said() {
        let stderr = r#"{"level":"error","msg":"Server side directory move failed: access denied","object":"x","objectType":"*local.Fs"}
{"level":"error","msg":"Attempt 1/3 failed with 1 errors and: access denied"}
"#;
        assert_eq!(
            move_failure(stderr),
            "Server side directory move failed: access denied"
        );
        assert_eq!(move_failure("plain words\n"), "plain words");
        let only_attempt = r#"{"level":"error","msg":"Attempt 1/1 failed with 1 errors and: is a directory not a file"}"#;
        assert_eq!(move_failure(only_attempt), "is a directory not a file");
        let job = MoveJob {
            from: "Docs/report.pdf".into(),
            to: "Archive/report.pdf".into(),
            files: 1,
            bytes: 3,
            folder: false,
        };
        assert_eq!(
            move_error(&job, "is a directory not a file", "OneDrive").reason,
            "not moved from Docs/report.pdf on OneDrive (a folder with that name is already \
             on OneDrive), so the copy uploads it instead"
        );
        let folder = MoveJob {
            folder: true,
            from: "Old".into(),
            to: "New".into(),
            ..job
        };
        let e = move_error(&folder, "access denied", "the drive");
        assert_eq!(e.path, "New/");
        assert_eq!(
            e.reason,
            "not moved from Old/ on the drive (access denied), so the copy uploads its files instead"
        );
        assert_eq!(relative_to("Old/sub/a.txt", "Old"), "sub/a.txt");
    }

    #[test]
    fn log_paths_are_relative_to_the_data_directory() {
        assert_eq!(
            relative_log(
                Path::new("/d"),
                Path::new("/d/logs/documents/d-7f3a/2026-10-13T094102.jsonl")
            ),
            "logs/documents/d-7f3a/2026-10-13T094102.jsonl"
        );
        assert_eq!(relative_log(Path::new("/d"), Path::new("/x/y")), "/x/y");
    }

    #[test]
    fn timestamps_read_back() {
        let t = now();
        let text = rfc3339(&t);
        let secs = crate::ui::jobs::unix_seconds(&text).unwrap();
        assert_eq!(secs, t.timestamp().as_second());
    }

    #[test]
    fn run_record_round_trips_through_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let record = RunRecord {
            started: "2026-10-13T09:41:02+11:00".into(),
            finished: "2026-10-13T09:43:40+11:00".into(),
            mode: crate::model::Mode::OneWay,
            dry_run: false,
            outcome: Outcome::Partial,
            added: SideCounts {
                local: 0,
                remote: 12,
            },
            updated: SideCounts {
                local: 0,
                remote: 3,
            },
            deleted: SideCounts::default(),
            bytes: 4096,
            errors: 1,
            log: "logs/documents/d-91c0/2026-10-13T094102.jsonl".into(),
            rclone_version: "1.75.2".into(),
            reason: None,
        };
        save_record(&paths, "documents", "d-91c0", record.clone()).unwrap();
        let state = State::load(&paths.state_file()).unwrap();
        assert_eq!(state.runs["documents/d-91c0"], record);
        // A state file that does not load is not overwritten.
        fs::write(paths.state_file(), "{broken").unwrap();
        assert!(save_record(&paths, "documents", "d-91c0", record).is_err());
        assert_eq!(fs::read_to_string(paths.state_file()).unwrap(), "{broken");
    }
}
