//! Two-way sync: `rclone bisync` with the local source as Path1 and the
//! destination as Path2. For one destination (DESIGN.md, Two-way sync):
//!
//! 1. validate as one-way, then the sync state: the bisync record must
//!    describe this pair, the working directory must have its listings,
//!    and a lock file must belong to a running process (a stale one is
//!    offered for removal);
//! 2. a first run (or one whose state was lost): explain, ask, then
//!    `bisync --resync --resync-mode path1`, record, and stop. Destination
//!    copies that differ from the local file are renamed there with the
//!    conflict suffix first, because `--resync` would overwrite them;
//! 3. plan: `bisync --dry-run --force`;
//! 4. deleted local folders: the folder prompt, or `--folder-deletes`;
//! 5. the preview and its confirmation (`--yes` skips it, except when more
//!    than half a side would be deleted); `--dry-run` stops here;
//! 6. holds saved, folders restored, then the real `bisync`, with
//!    `--max-delete 100` only after a confirmed preview showing those
//!    deletions;
//! 7. conflict copies taken off the local side (bisync copies them to both
//!    sides; they belong on the destination only), the summary and the
//!    record. A bisync critical error marks the destination needs-resync.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::tilde;
use crate::model::{Destination, FileError, Mode, Outcome, Summary};
use crate::rclone::bisync::{self, Abort, Call, Lock, Pair, Side};
use crate::rclone::events::Event;
use crate::rclone::exec::{self, ExecError, Finished, InterruptGuard, Progress, RunLog};
use crate::rclone::listing::{self, ListError};
use crate::rclone::{self as rc};
use crate::state::{BisyncState, State};

use super::folders;
use super::oneway::{
    self, elapsed, excludes_of, first_line, note, now, rfc3339, Ended, Recorder, Run,
};
use super::plan::{
    conflict_name, DryRun, FolderDecision, Plan, PlannedConflict, ResyncOffer, ResyncReason,
};
use super::reporter::{
    Answer, ConflictDone, DecidedBy, DeletedFolder, DestinationInfo, FolderAnswer, FolderPolicy,
    Looking, Question, Reporter, Step,
};
use super::summary::{Applied, TwoWayTally};

/// "OneDrive" or "the drive", as the summary and prompts say it.
pub fn place(destination_type: &str) -> &'static str {
    if destination_type == "onedrive" {
        "OneDrive"
    } else {
        "the drive"
    }
}

/// Everything one destination's run carries between steps.
struct Flow<'a> {
    run: &'a Run<'a>,
    dest: &'a Destination,
    info: &'a DestinationInfo,
    ctx: Recorder<'a>,
    log: Option<RunLog>,
    /// A local path or `remote:path`.
    target: String,
    fast_list: bool,
    workdir: PathBuf,
}

impl Flow<'_> {
    fn source(&self) -> &str {
        &self.run.job.source
    }

    fn key(&self) -> String {
        State::key(&self.run.job.name, self.dest.id())
    }

    fn log_shown(&self) -> String {
        self.ctx
            .log
            .as_deref()
            .map(tilde)
            .unwrap_or_else(|| "the run log".into())
    }

    /// Ends the destination with the conflicts already set aside listed,
    /// so a stop between the renames and the sync still reports them.
    fn stopped(
        &mut self,
        outcome: Outcome,
        message: Option<&str>,
        duration: u64,
        conflicts: &[ConflictDone],
        reporter: &mut dyn Reporter,
    ) -> Ended {
        let mut summary = match message {
            Some(m) => {
                note(&mut self.log, "refused", m);
                reporter.problem(m);
                self.ctx.failed(first_line(m), duration)
            }
            None => self.ctx.summary(outcome, duration),
        };
        summary.conflicts = conflict_lines(conflicts);
        self.ctx.finish(summary, reporter)
    }

    fn cancelled(&self, duration: u64, reporter: &mut dyn Reporter) -> Ended {
        let summary = self.ctx.summary(Outcome::Cancelled, duration);
        self.ctx.finish(summary, reporter)
    }

    fn fail(&mut self, message: &str, duration: u64, reporter: &mut dyn Reporter) -> Ended {
        note(&mut self.log, "refused", message);
        reporter.problem(message);
        let summary = self.ctx.failed(first_line(message), duration);
        self.ctx.finish(summary, reporter)
    }

    /// The bisync arguments for this pair with these excludes, always as
    /// repeated `--exclude` flags: bisync never gets a filters file.
    fn args(&self, excludes: &[String], call: Call) -> Vec<String> {
        let pair = Pair {
            path1: self.source(),
            path2: &self.target,
            workdir: &self.workdir,
            excludes,
            fast_list: self.fast_list,
        };
        bisync::args(&pair, call)
    }

    /// Runs one bisync (or other streamed rclone) call. With `progress`
    /// the files, stats and errors go to the reporter; phases and Ctrl-C
    /// always do. Every event is kept in `events` when given. With
    /// `planning` (when the planning step started), each stats line goes
    /// to the reporter as live figures instead.
    #[allow(clippy::too_many_arguments)]
    async fn stream(
        &mut self,
        args: &[String],
        tally: &mut TwoWayTally,
        mut events: Option<&mut Vec<Event>>,
        progress: bool,
        planning: Option<Instant>,
        reporter: &mut dyn Reporter,
        interrupts: &mut InterruptGuard,
    ) -> Result<Finished, ExecError> {
        let mut looking = Looking::new(Step::Planning);
        let mut on = |p: Progress| match p {
            Progress::Event(e) => {
                let applied = tally.apply(&e);
                if let Some(events) = events.as_deref_mut() {
                    events.push(e);
                }
                match applied {
                    Applied::Phase(phase) => reporter.phase(phase),
                    Applied::Stats(s) if progress => reporter.stats(&s),
                    Applied::Stats(s) => {
                        if let Some(since) = planning {
                            looking.take_stats(&s);
                            looking.set_elapsed(since.elapsed());
                            reporter.looking(&looking);
                        }
                    }
                    Applied::File(f) if progress => reporter.file(&f),
                    Applied::Error(err) if progress => reporter.file_error(&err),
                    _ => {}
                }
            }
            Progress::Stopping => reporter.stopping(),
            Progress::Killing => {}
        };
        exec::run_streaming_with_grace(
            &self.run.rclone.path,
            args,
            self.log.as_mut(),
            interrupts,
            exec::BISYNC_INTERRUPT_GRACE,
            &mut on,
        )
        .await
    }

    /// After bisync was killed (no exit within the grace period), its lock
    /// is left behind with a process that is gone: ours to remove.
    async fn clear_own_lock(&mut self, finished: &Finished) {
        if !finished.killed {
            return;
        }
        if let Lock::Stale { path, .. } = bisync::check_lock(&self.workdir).await {
            if bisync::remove_lock(&path).is_ok() {
                note(
                    &mut self.log,
                    "lock-removed",
                    &format!("removed {} left by the stopped bisync", path.display()),
                );
            }
        }
    }

    /// The user-facing words for a bisync abort (DESIGN.md: error handling
    /// in words, not rclone output). A headline, then detail lines.
    fn explain(&self, abort: &Abort) -> String {
        let job = &self.run.job.name;
        let id = self.dest.id();
        let place = place(self.info.kind);
        let side_name = |side: Option<Side>| match side {
            Some(Side::Path1) => "this Mac".to_string(),
            _ => place.to_string(),
        };
        let resync = format!("onefolder run {job} --dest {id} --resync");
        match abort {
            Abort::NeedsResync { message } => format!(
                "The sync state for this destination was lost or damaged\n  \
                 bisync said: {}\n  The log is {}. The next run starts with a merge of both \
                 sides (nothing is deleted by a merge), or run it now with: {resync}",
                first_line(message),
                self.log_shown()
            ),
            Abort::TooManyDeletes {
                side,
                percent,
                deleted,
                total,
            } => format!(
                "bisync stopped before deleting {deleted} of {total} files and folders ({})\n  \
                 They were deleted on {} and would be deleted on {} too, which is more than \
                 bisync's {percent}% safety limit. Run onefolder run {job} on a terminal to see \
                 the plan and confirm it.",
                super::plan::percent_text(*deleted, *total),
                side_name(*side),
                side_name(side.map(Side::other)),
            ),
            Abort::AllChanged { side } => format!(
                "bisync stopped because every file on {} changed\n  Run onefolder run {job} on a \
                 terminal to see the plan and confirm it.",
                side_name(*side)
            ),
            Abort::Locked { lock } => format!(
                "Another OneFolder or rclone run is syncing this destination\n  Its lock is {}. \
                 Wait for it to finish, then run again.",
                lock.as_deref()
                    .map_or_else(|| self.log_shown(), |l| tilde(Path::new(l)))
            ),
            Abort::FiltersChanged => {
                format!("bisync wants a merge because its filters changed\n  Run: {resync}")
            }
            Abort::EmptySide { side } => {
                let (what, path) = match side {
                    Some(Side::Path1) => ("source folder", tilde(Path::new(self.source()))),
                    _ => ("destination folder", self.info.display.clone()),
                };
                format!(
                    "The {what} looks empty\n  {path} has no files, but the last run left files \
                     there. If it is on a drive, check that the drive is plugged in and mounted, \
                     then run again. If the folder really is empty now, merge both sides again \
                     with: {resync} (nothing is deleted by a merge)."
                )
            }
            Abort::NamesTooLong => "The folder paths are too long for bisync\n  rclone names its \
                 listing files after both folder paths, and together they pass macOS's limit of \
                 255 characters for a file name. Use a shorter source or destination folder."
                .to_string(),
        }
    }
}

/// Runs one two-way destination from validation to the run record.
pub async fn run_destination(
    run: &Run<'_>,
    dest: &Destination,
    info: &DestinationInfo,
    reporter: &mut dyn Reporter,
    interrupts: &mut InterruptGuard,
) -> Ended {
    reporter.destination_started(info);
    let mut ctx = Recorder::new(run, dest, info, now());
    let log = match ctx.open_log() {
        Ok(log) => Some(log),
        Err(e) => {
            reporter.problem(&format!(
                "Could not create the run log, carrying on without it\n  {e}"
            ));
            None
        }
    };
    ctx.log = log.as_ref().map(|l| l.path().to_path_buf());
    let workdir = run.paths.bisync_dir().join(&run.job.name).join(dest.id());
    let mut flow = Flow {
        run,
        dest,
        info,
        ctx,
        log,
        target: String::new(),
        fast_list: false,
        workdir,
    };

    // 1. Validate.
    reporter.step(Step::Checking);
    match oneway::validate(run, dest).await {
        Ok(t) => {
            flow.target = t.target;
            flow.fast_list = t.fast_list;
        }
        Err(_) if interrupts.interrupted() => return flow.cancelled(0, reporter),
        Err(message) => return flow.fail(&message, 0, reporter),
    }
    if interrupts.interrupted() {
        return flow.cancelled(0, reporter);
    }
    if let Err(e) = fs::create_dir_all(&flow.workdir) {
        let message = format!(
            "Could not create the sync state folder\n  {}: {e}",
            tilde(&flow.workdir)
        );
        return flow.fail(&message, 0, reporter);
    }
    let state = match State::load(&run.paths.state_file()) {
        Ok(state) => state,
        Err(e) => {
            let message = format!("Could not read the sync state\n  {e:#}");
            return flow.fail(&message, 0, reporter);
        }
    };
    let key = flow.key();
    let record = state.bisync.get(&key).cloned();
    let holds = state.holds_of(&run.job.name, dest.id());

    match bisync::check_lock(&flow.workdir).await {
        Lock::Free => {}
        Lock::Held { pid, path } => {
            let message = format!(
                "Another OneFolder or rclone run is syncing this destination\n  rclone process \
                 {pid} holds the lock {}. Wait for it to finish, then run again. If no sync of \
                 this destination is running, delete that file and run again.",
                tilde(&path)
            );
            return flow.fail(&message, 0, reporter);
        }
        Lock::Unreadable { path } => {
            let message = format!(
                "A lock file is in the way\n  {} does not say which process made it. If no \
                 other sync of this destination is running, delete it and run again.",
                tilde(&path)
            );
            return flow.fail(&message, 0, reporter);
        }
        Lock::Stale { pid, path } => {
            let shown = tilde(&path);
            let remove = if run.interactive {
                interrupts
                    .unguarded(|| reporter.confirm(&Question::StaleLock { pid, path: &shown }))
                    == Answer::Yes
            } else {
                run.yes
            };
            if !remove {
                let message = format!(
                    "A stale lock is in the way\n  Process {pid} left {shown}, and no rclone \
                     with that id is running it now. Run again on a terminal to remove it, add \
                     --yes, or delete that file."
                );
                return flow.fail(&message, 0, reporter);
            }
            if let Err(e) = bisync::remove_lock(&path) {
                let message = format!("Could not remove the stale lock\n  {shown}: {e}");
                return flow.fail(&message, 0, reporter);
            }
            reporter.note(&format!(
                "Removed a stale lock left by process {pid}, which is no longer syncing ({shown})."
            ));
        }
    }
    if let Some(last) = state.runs.get(&key) {
        if last.mode == Mode::TwoWay && last.outcome == Outcome::Cancelled && !last.dry_run {
            reporter.note(&format!(
                "Recovering from the interrupted run on {}.",
                short_date(&last.started)
            ));
        }
    }

    // 2. A first run, or one whose sync state is gone.
    let source = run.job.source.clone();
    let shown = dest.display();
    let reason = if run.resync {
        Some((ResyncReason::Requested, None))
    } else {
        match &record {
            None => Some((ResyncReason::FirstRun, None)),
            Some(b) if !b.initialised => Some((ResyncReason::FirstRun, None)),
            Some(b) if !b.matches(&source, &shown) => Some((ResyncReason::PairChanged, None)),
            Some(b) if b.needs_resync.is_some() => {
                Some((ResyncReason::Critical, b.needs_resync.clone()))
            }
            Some(_) if !bisync::has_listings(&flow.workdir) => Some((
                ResyncReason::Damaged,
                Some(format!(
                    "The listings in {} are missing.",
                    tilde(&flow.workdir)
                )),
            )),
            Some(_) => None,
        }
    };
    if let Some((reason, detail)) = reason {
        return resync(&mut flow, reason, detail, &holds, reporter, interrupts).await;
    }

    // 3. Plan.
    reporter.step(Step::Planning);
    let clock = Instant::now();
    let excludes = bisync::excludes(&excludes_of(run.job), &holds);
    let args = flow.args(&excludes, Call::Plan);
    let mut events = Vec::new();
    let mut dry_tally = TwoWayTally::default();
    let finished = match flow
        .stream(
            &args,
            &mut dry_tally,
            Some(&mut events),
            false,
            Some(clock),
            reporter,
            interrupts,
        )
        .await
    {
        Ok(f) => f,
        Err(e) => return flow.fail(&format!("Could not run rclone\n  {e}"), 0, reporter),
    };
    flow.clear_own_lock(&finished).await;
    if finished.cancelled || interrupts.interrupted() {
        return flow.cancelled(elapsed(clock), reporter);
    }
    let abort = bisync::classify(dry_tally.messages.iter().map(String::as_str));
    match &abort {
        Some(Abort::NeedsResync { message }) => {
            let detail = format!("bisync said: {}", first_line(message));
            return resync(
                &mut flow,
                ResyncReason::Damaged,
                Some(detail),
                &holds,
                reporter,
                interrupts,
            )
            .await;
        }
        Some(Abort::FiltersChanged) => {
            return resync(
                &mut flow,
                ResyncReason::Damaged,
                None,
                &holds,
                reporter,
                interrupts,
            )
            .await;
        }
        Some(other) => {
            let message = flow.explain(other);
            return flow.fail(&message, elapsed(clock), reporter);
        }
        None if !finished.exit.is_success() => {
            let reason = dry_tally
                .acc
                .summary_errors(&finished)
                .first()
                .map(|e| e.reason.clone())
                .unwrap_or_else(|| format!("rclone {}", finished.exit.describe()));
            let message = format!("The two-way plan could not be made\n  {reason}");
            return flow.fail(&message, elapsed(clock), reporter);
        }
        None => {}
    }
    // Fail closed: without the previous listings a deleted folder cannot be
    // told from deleted files, and would go without a prompt.
    let listings = (
        bisync::read_listing(&flow.workdir, Side::Path1),
        bisync::read_listing(&flow.workdir, Side::Path2),
    );
    let (prior1, prior2) = match listings {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            let detail = format!(
                "The last listing could not be read ({e}), so a deleted folder could not be \
                 told from deleted files. Nothing was synced."
            );
            return resync(
                &mut flow,
                ResyncReason::Damaged,
                Some(detail),
                &holds,
                reporter,
                interrupts,
            )
            .await;
        }
    };
    let dry = DryRun::collect(&events);

    // 4. Deleted local folders.
    let src = Path::new(&source);
    let new_local = dry
        .new_local()
        .iter()
        .filter(|p| src.join(p).is_file())
        .count() as u64;
    let found = folders::detect(&dry.lines, &prior1, new_local, |rel| {
        fs::symlink_metadata(src.join(rel)).is_ok()
    });
    let mut deleted = Vec::new();
    // Sized as the sync sees it: what the excludes leave out is not counted.
    let sync_excludes = excludes_of(run.job);
    for f in found {
        let filters = rc::folder_filter_args(&sync_excludes, &f.path);
        let size = listing::size(run.rclone, &flow.target, &filters).await;
        let (files, bytes, measured) = match size {
            Ok(s) => (s.count, u64::try_from(s.bytes).unwrap_or(0), true),
            Err(_) => (f.listed_files, f.listed_bytes, false),
        };
        deleted.push(DeletedFolder {
            path: f.path,
            files,
            bytes,
            measured,
            maybe_rename: f.maybe_rename,
        });
    }
    if interrupts.interrupted() {
        return flow.cancelled(elapsed(clock), reporter);
    }
    let (answers, by) = match run.folders {
        _ if deleted.is_empty() => (Vec::new(), DecidedBy::Default),
        // A dry run lists the folders and says it would ask; the plan
        // counts them as kept, so nothing is shown as deleted.
        FolderPolicy::Ask if run.interactive && run.dry_run => {
            (vec![FolderAnswer::Keep; deleted.len()], DecidedBy::NotAsked)
        }
        FolderPolicy::Ask if run.interactive => {
            let kind = info.kind;
            let mut answers = interrupts.unguarded(|| reporter.ask_folders(kind, &deleted));
            answers.resize(deleted.len(), FolderAnswer::Keep);
            (answers, DecidedBy::Asked)
        }
        FolderPolicy::Answer(a) => (vec![a; deleted.len()], DecidedBy::Flag),
        FolderPolicy::Ask | FolderPolicy::DefaultKeep => {
            (vec![FolderAnswer::Keep; deleted.len()], DecidedBy::Default)
        }
    };
    if answers.contains(&FolderAnswer::CancelRun) {
        note(
            &mut flow.log,
            "declined",
            "the run was stopped at the folder prompt",
        );
        super::reporter::request_stop();
        reporter.destination_skipped(info, "not run, the run was stopped");
        return Ended::Declined;
    }
    let decisions: Vec<FolderDecision> = deleted
        .into_iter()
        .zip(answers)
        .map(|(folder, answer)| {
            reporter.folder_decided(&folder, answer, by);
            FolderDecision { folder, answer, by }
        })
        .collect();

    // 5. Preview.
    let mut plan = Plan::build(&dry, &prior1, &prior2, decisions);
    plan.job = run.job.name.clone();
    plan.destination_id = dest.id().to_string();
    plan.destination = shown.clone();
    plan.destination_type = info.kind.to_string();
    plan.excludes = run.job.excludes.clone();
    plan.excludes.push(bisync::CONFLICT_PATTERN.to_string());
    plan.copy_hidden = run.job.copy_hidden;
    plan.effective_excludes = excludes.clone();
    plan.holds = holds.clone();
    if let Err(message) = name_conflicts(&flow, &mut plan.conflicts).await {
        return flow.fail(&message, elapsed(clock), reporter);
    }
    reporter.plan(&plan);
    if run.dry_run {
        let mut summary = flow.ctx.summary(Outcome::Ok, elapsed(clock));
        fill_from_plan(&mut summary, &plan);
        return flow.ctx.finish(summary, reporter);
    }
    if plan.is_empty() {
        reporter.note("Nothing to sync.");
        let summary = flow.ctx.summary(Outcome::Ok, elapsed(clock));
        return flow.ctx.finish(summary, reporter);
    }
    if plan.needs_confirmation() {
        if !run.interactive {
            let big = &plan.big[0];
            let side = match big.side {
                super::reporter::Where::Local => "this Mac",
                super::reporter::Where::Destination => place(info.kind),
            };
            let what = match big.kind {
                super::plan::BigKind::Deletions => format!(
                    "{} of {} files and folders ({}) would be deleted on {side}",
                    big.count,
                    big.total,
                    big.percent_text()
                ),
                super::plan::BigKind::AllChanged => {
                    format!(
                        "every file changed, and all {} would be copied to {side}",
                        big.count
                    )
                }
            };
            let reason = format!(
                "not run: {what}, which needs confirming on a terminal (onefolder run {})",
                run.job.name
            );
            note(&mut flow.log, "declined", &reason);
            reporter.destination_skipped(info, &reason);
            return Ended::Declined;
        }
        if interrupts.unguarded(|| reporter.confirm(&Question::Plan(&plan))) == Answer::No {
            return declined(&mut flow, reporter, interrupts);
        }
    } else if !run.yes
        && interrupts.unguarded(|| reporter.confirm(&Question::Plan(&plan))) == Answer::No
    {
        return declined(&mut flow, reporter, interrupts);
    }
    if interrupts.interrupted() {
        return flow.cancelled(0, reporter);
    }

    // 6. Holds, restores, then the real run.
    let clock = Instant::now();
    let mut held = holds.clone();
    let kept: Vec<String> = plan
        .folders
        .iter()
        .filter(|d| d.answer == FolderAnswer::Keep)
        .map(|d| d.folder.path.clone())
        .collect();
    if let Err(e) = save_holds(&flow, &kept) {
        let message = format!(
            "Could not save the folders to keep\n  {e:#}. Nothing was synced, so nothing was \
             deleted."
        );
        return flow.fail(&message, 0, reporter);
    }
    held.extend(kept);
    for d in plan
        .folders
        .iter()
        .filter(|d| d.answer == FolderAnswer::Restore)
    {
        reporter.step(Step::Restoring);
        // Copied root to root, with the folder picked out by filters, so
        // the excludes mean what they mean in the sync: nothing excluded
        // comes back. Conflict copies stay on the destination too.
        let mut restore_excludes = excludes_of(run.job);
        restore_excludes.push(bisync::CONFLICT_PATTERN.to_string());
        let mut args = rc::copy_args(&flow.target, &source, &[], None, flow.fast_list, false);
        args.extend(rc::folder_filter_args(&restore_excludes, &d.folder.path));
        let mut tally = TwoWayTally::resync(HashSet::new());
        let restored = flow
            .stream(&args, &mut tally, None, false, None, reporter, interrupts)
            .await;
        if interrupts.interrupted() {
            return flow.cancelled(elapsed(clock), reporter);
        }
        let ok = matches!(&restored, Ok(f) if f.exit.is_success());
        if ok {
            reporter.note(&format!(
                "Restored \"{}\" from {}.",
                d.folder.path,
                place(info.kind)
            ));
            continue;
        }
        // Never let bisync delete a folder that was meant to come back.
        if let Err(e) = save_holds(&flow, std::slice::from_ref(&d.folder.path)) {
            let message = format!(
                "Could not restore \"{}\" and could not keep it either\n  {e:#}. Nothing \
                 was synced, so nothing was deleted.",
                d.folder.path
            );
            return flow.fail(&message, elapsed(clock), reporter);
        }
        held.push(d.folder.path.clone());
        reporter.problem(&format!(
            "Could not restore \"{}\"\n  It stays on {} and is kept out of the sync instead; \
             release it with: onefolder jobs release {} \"{}\"",
            d.folder.path,
            place(info.kind),
            run.job.name,
            d.folder.path
        ));
    }

    // Each conflict's destination copy is set aside under the name checked
    // free on both sides; bisync then sees it changed here and gone there,
    // and uploads the local copy. bisync's own numbering is not used: it
    // cannot see earlier conflict copies (they are excluded), so it would
    // reuse -1 and replace one.
    let mut set_aside: Vec<ConflictDone> = Vec::new();
    for c in &plan.conflicts {
        let Some(kept_as) = &c.kept_as else { continue };
        let args = rc::moveto_args(
            &listing::join(&flow.target, &c.path),
            &listing::join(&flow.target, kept_as),
        );
        let mut t = TwoWayTally::default();
        let moved = flow
            .stream(&args, &mut t, None, false, None, reporter, interrupts)
            .await;
        if interrupts.interrupted() {
            return flow.stopped(
                Outcome::Cancelled,
                None,
                elapsed(clock),
                &set_aside,
                reporter,
            );
        }
        if !matches!(&moved, Ok(f) if f.exit.is_success()) {
            let message = format!(
                "Could not keep {}'s copy of {}\n  Renaming it to {kept_as} failed, so nothing \
                 was synced.",
                place(info.kind),
                c.path
            );
            return flow.stopped(
                Outcome::Failed,
                Some(&message),
                elapsed(clock),
                &set_aside,
                reporter,
            );
        }
        set_aside.push(ConflictDone {
            path: c.path.clone(),
            kept_as: Some(kept_as.clone()),
        });
    }

    reporter.step(Step::Syncing);
    let excludes = bisync::excludes(&excludes_of(run.job), &held);
    let target = flow.target.clone();
    let roots = [source.as_str(), target.as_str()];
    let (mut lift, mut force) = (plan.lift, plan.force);
    let mut retried = false;
    let (finished, tally, abort) = loop {
        let args = flow.args(&excludes, Call::Sync { lift, force });
        let mut tally = TwoWayTally::new(&dry)
            .with_roots(&roots)
            .with_conflicts(set_aside.iter().map(|c| c.path.clone()));
        let finished = match flow
            .stream(&args, &mut tally, None, true, None, reporter, interrupts)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                let message = format!("Could not run rclone\n  {e}");
                return flow.fail(&message, elapsed(clock), reporter);
            }
        };
        flow.clear_own_lock(&finished).await;
        let abort = bisync::classify(tally.messages.iter().map(String::as_str));
        // A safety limit after the preview (should not happen, but is
        // caught): offer to run again with it lifted.
        let limit = matches!(
            abort,
            Some(Abort::TooManyDeletes { .. } | Abort::AllChanged { .. })
        );
        if limit && !finished.cancelled && !retried && run.interactive {
            if let Some(a) = &abort {
                reporter.problem(&flow.explain(a));
                if interrupts.unguarded(|| reporter.confirm(&Question::Retry(a))) == Answer::Yes {
                    retried = true;
                    lift = true;
                    force |= matches!(a, Abort::AllChanged { .. });
                    continue;
                }
            }
        }
        break (finished, tally, abort);
    };

    // Folders the user chose to delete: bisync removed what it syncs, and
    // left what the excludes keep out of its sight (.DS_Store, hidden
    // files), so the folder would still be there. Remove the rest, only
    // after a clean run.
    let mut removed = Vec::new();
    if abort.is_none()
        && !finished.cancelled
        && finished.exit.is_success()
        && !interrupts.interrupted()
    {
        for d in plan
            .folders
            .iter()
            .filter(|d| d.answer == FolderAnswer::Delete)
        {
            if remove_folder_leftovers(&mut flow, &d.folder.path, reporter).await {
                removed.push(d.folder.path.clone());
            }
        }
    }
    reporter.step(Step::Finishing);

    // 7. Conflicts, summary and record. bisync meets a conflict itself
    // only when a file changed after the plan; those are reported too.
    let mut done = set_aside;
    done.extend(tally.conflicts.iter().cloned());
    for c in &done {
        reporter.conflict(c);
    }
    let conflicts = conflict_lines(&done);
    let mut outcome = tally.outcome(&finished);
    let mut errors = tally.acc.summary_errors(&finished);
    if let (Some(a), false) = (&abort, finished.cancelled) {
        let message = flow.explain(a);
        reporter.problem(&message);
        errors.retain(|e| !e.path.is_empty());
        errors.insert(
            0,
            FileError {
                path: String::new(),
                reason: first_line(&message).to_string(),
            },
        );
        if matches!(a, Abort::NeedsResync { .. } | Abort::FiltersChanged) {
            outcome = Outcome::NeedsResync;
            let what = format!(
                "The last two-way run stopped on a bisync critical error ({}).",
                flow.log_shown()
            );
            if let Err(e) = update_bisync(&flow, |b| b.needs_resync = Some(what)) {
                reporter.problem(&format!("Could not record that a merge is needed\n  {e:#}"));
            }
        } else if outcome == Outcome::Ok {
            outcome = Outcome::Failed;
        }
    }
    let mut summary = flow.ctx.summary(outcome, elapsed(clock));
    summary.added = tally.added;
    summary.updated = tally.updated;
    summary.deleted = tally.deleted;
    summary.bytes = tally.acc.bytes();
    summary.errors = errors;
    summary.conflicts = conflicts;
    summary.removed_folders = removed;
    flow.ctx.finish(summary, reporter)
}

/// Removes what is left of a folder the user chose to delete, on the
/// destination, after bisync deleted what it syncs: the excluded files in
/// it (hidden files, `.DS_Store` and the like, the job's own excludes),
/// with `rclone purge` and no filter. True when the folder was removed.
///
/// Only when the folder is still gone from this Mac, and only when what is
/// left is all excluded: a file the sync handles (one that arrived after
/// the plan, or a conflict copy) keeps the folder, and the user is told.
async fn remove_folder_leftovers(
    flow: &mut Flow<'_>,
    folder: &str,
    reporter: &mut dyn Reporter,
) -> bool {
    let place = place(flow.info.kind);
    let quoted = format!("\"{folder}\"");
    if fs::symlink_metadata(Path::new(flow.source()).join(folder)).is_ok() {
        reporter.note(&format!(
            "{quoted} is back on this Mac, so what is left of it on {place} stays."
        ));
        return false;
    }
    let rclone = flow.run.rclone;
    let path = listing::join(&flow.target, folder);
    match listing::exists(rclone, &path).await {
        Ok(true) => {}
        Ok(false) => return false,
        Err(e) => {
            reporter.note(&format!(
                "Could not check what is left of {quoted} on {place}: {e}"
            ));
            return false;
        }
    }
    let filters = rc::folder_filter_args(&excludes_of(flow.run.job), folder);
    match listing::list_filtered(rclone, &flow.target, &filters, flow.fast_list).await {
        Ok(left) if left.is_empty() => {}
        Ok(left) => {
            note(
                &mut flow.log,
                "folder-kept",
                &format!("{folder}: {} synced files left, not removed", left.len()),
            );
            let files = if left.len() == 1 { "file" } else { "files" };
            reporter.note(&format!(
                "{quoted} still has {} {files} on {place} that the sync handles (such as a \
                 conflict copy, or a file added after the plan), so the folder stays there.",
                left.len()
            ));
            return false;
        }
        Err(e) => {
            reporter.note(&format!(
                "Could not check what is left of {quoted} on {place}: {e}"
            ));
            return false;
        }
    }
    match listing::purge(rclone, &path).await {
        Ok(()) | Err(ListError::NotFound { .. }) => {
            note(
                &mut flow.log,
                "folder-removed",
                &format!("removed {path} with everything left in it (rclone purge)"),
            );
            reporter.note(&format!(
                "Removed {quoted} on {place}, including the hidden and system files inside it."
            ));
            true
        }
        Err(e) => {
            note(
                &mut flow.log,
                "folder-remove-failed",
                &format!("{path}: {e}"),
            );
            reporter.note(&format!(
                "Could not remove what is left of {quoted} on {place}: {e}"
            ));
            false
        }
    }
}

fn declined(flow: &mut Flow, reporter: &mut dyn Reporter, interrupts: &InterruptGuard) -> Ended {
    note(&mut flow.log, "declined", "the plan was not confirmed");
    let reason = if interrupts.interrupted() {
        "not run, the run was stopped"
    } else {
        "not confirmed, nothing synced"
    };
    reporter.destination_skipped(flow.info, reason);
    Ended::Declined
}

/// The summary of a dry run: what would happen.
fn fill_from_plan(summary: &mut Summary, plan: &Plan) {
    summary.added.remote = plan.to_remote.new.len() as u64;
    summary.updated.remote = plan.to_remote.changed.len() as u64;
    summary.added.local = plan.to_local.new.len() as u64;
    summary.updated.local = plan.to_local.changed.len() as u64;
    summary.deleted.remote = plan.delete_remote.len() as u64;
    summary.deleted.local = plan.delete_local.len() as u64;
    summary.bytes = plan.to_remote.bytes + plan.to_local.bytes;
    summary.conflicts = plan
        .conflicts
        .iter()
        .map(|c| conflict_line(&c.path, c.kept_as.as_deref()))
        .collect();
}

fn conflict_lines(conflicts: &[ConflictDone]) -> Vec<String> {
    conflicts
        .iter()
        .map(|c| conflict_line(&c.path, c.kept_as.as_deref()))
        .collect()
}

/// Names each conflict's destination copy: `name.conflict-<date>-<n>.ext`
/// with the first `n` free on both sides, checked with unfiltered listings
/// (the destination's folder through rclone, this Mac on disk), and not
/// given to another conflict of this run.
async fn name_conflicts(flow: &Flow<'_>, conflicts: &mut [PlannedConflict]) -> Result<(), String> {
    let date = now().strftime("%Y-%m-%d").to_string();
    let src = Path::new(flow.source());
    let mut remote: HashMap<String, HashSet<String>> = HashMap::new();
    let mut chosen: HashSet<String> = HashSet::new();
    for c in conflicts.iter_mut() {
        let dir = c.path.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        if !remote.contains_key(&dir) {
            let names = listing::names_in(flow.run.rclone, &listing::join(&flow.target, &dir))
                .await
                .map_err(|e| {
                    format!("Could not look for a free name for a conflict copy\n  {e}")
                })?;
            let full = names
                .into_iter()
                .map(|n| {
                    if dir.is_empty() {
                        n
                    } else {
                        format!("{dir}/{n}")
                    }
                })
                .collect();
            remote.insert(dir.clone(), full);
        }
        let there = remote.get(&dir);
        let name = conflict_name(&c.path, &date, |n| {
            there.is_some_and(|t| t.contains(n))
                || chosen.contains(n)
                || fs::symlink_metadata(src.join(n)).is_ok()
        });
        chosen.insert(name.clone());
        c.kept_as = Some(name);
    }
    Ok(())
}

/// One conflict in the summary block: the file and the name its losing
/// copy has on the destination.
fn conflict_line(path: &str, kept_as: Option<&str>) -> String {
    match kept_as {
        Some(k) => format!("{path} → {k}"),
        None => path.to_string(),
    }
}

/// Adds folders to this destination's holds in `state.json`.
fn save_holds(flow: &Flow, folders: &[String]) -> anyhow::Result<()> {
    if folders.is_empty() {
        return Ok(());
    }
    State::update(flow.run.paths, |state| {
        for f in folders {
            state.add_hold(&flow.run.job.name, flow.dest.id(), f);
        }
    })
}

/// Changes this destination's bisync record in `state.json`.
fn update_bisync(flow: &Flow, change: impl FnOnce(&mut BisyncState)) -> anyhow::Result<()> {
    let key = flow.key();
    State::update(flow.run.paths, |state| {
        change(state.bisync.entry(key).or_default())
    })
}

/// `13 Oct` from `2026-10-13T09:41:02+11:00`, the date as recorded.
fn short_date(rfc3339: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let day: Option<u32> = rfc3339.get(8..10).and_then(|d| d.parse().ok());
    let month: Option<usize> = rfc3339.get(5..7).and_then(|m| m.parse().ok());
    match (day, month.and_then(|m| MONTHS.get(m.wrapping_sub(1)))) {
        (Some(d), Some(m)) => format!("{d} {m}"),
        _ => rfc3339.to_string(),
    }
}

/// Step 2: explain, ask, merge, record, stop.
async fn resync(
    flow: &mut Flow<'_>,
    reason: ResyncReason,
    detail: Option<String>,
    holds: &[String],
    reporter: &mut dyn Reporter,
    interrupts: &mut InterruptGuard,
) -> Ended {
    let run = flow.run;
    reporter.step(Step::Planning);
    let clock = Instant::now();
    let missing = match flow.dest {
        Destination::Local { path, .. } => !Path::new(path).is_dir(),
        Destination::Onedrive { .. } => match listing::exists(run.rclone, &flow.target).await {
            Ok(exists) => !exists,
            Err(e) => {
                let message = format!("Could not look at the destination\n  {e}");
                return flow.fail(&message, 0, reporter);
            }
        },
    };
    let mut offer = ResyncOffer {
        job: run.job.name.clone(),
        destination_id: flow.dest.id().to_string(),
        destination: flow.info.display.clone(),
        destination_type: flow.info.kind.to_string(),
        reason: Some(reason),
        detail,
        destination_missing: missing,
        dry_run: run.dry_run,
        ..ResyncOffer::default()
    };
    let excludes = bisync::excludes(&excludes_of(run.job), holds);
    if !missing {
        let args = flow.args(&excludes, Call::Resync { dry_run: true });
        let mut events = Vec::new();
        let mut tally = TwoWayTally::default();
        let finished = match flow
            .stream(
                &args,
                &mut tally,
                Some(&mut events),
                false,
                Some(clock),
                reporter,
                interrupts,
            )
            .await
        {
            Ok(f) => f,
            Err(e) => return flow.fail(&format!("Could not run rclone\n  {e}"), 0, reporter),
        };
        flow.clear_own_lock(&finished).await;
        if finished.cancelled || interrupts.interrupted() {
            return flow.cancelled(elapsed(clock), reporter);
        }
        if !finished.exit.is_success() {
            let message = match bisync::classify(tally.messages.iter().map(String::as_str)) {
                Some(a) => flow.explain(&a),
                None => format!(
                    "The merge could not be planned\n  {}",
                    tally.acc.summary_errors(&finished).first().map_or_else(
                        || finished.exit.describe().to_string(),
                        |e| e.reason.clone()
                    )
                ),
            };
            return flow.fail(&message, elapsed(clock), reporter);
        }
        let overwrites_possible = events
            .iter()
            .any(|e| matches!(e, Event::Applying { to: Side::Path2 }));
        // Unfiltered: earlier conflict copies must count when a free
        // name is chosen for a new one.
        let mut listed = HashSet::new();
        if overwrites_possible {
            match listing::list_files(
                run.rclone,
                &flow.target,
                &[],
                None,
                flow.fast_list,
                false,
                None,
            )
            .await
            {
                Ok(entries) => listed = entries.into_iter().map(|e| e.path).collect(),
                Err(ListError::NotFound { .. }) => {}
                Err(e) => {
                    let message = format!("Could not list the destination\n  {e}");
                    return flow.fail(&message, elapsed(clock), reporter);
                }
            }
        }
        let today = now().strftime("%Y-%m-%d").to_string();
        let src = Path::new(&run.job.source);
        offer.fill(&events, &listed, &today, |n| {
            fs::symlink_metadata(src.join(n)).is_ok()
        });
    } else {
        // Nothing there yet: everything here goes, counted from a listing
        // of this Mac with the same excludes.
        let exclude_file = flow.ctx.exclude_file(flow.log.as_ref(), &excludes);
        match listing::list_files(
            run.rclone,
            &run.job.source,
            &excludes,
            exclude_file.as_deref(),
            false,
            false,
            None,
        )
        .await
        {
            Ok(entries) => {
                for e in entries {
                    let size = u64::try_from(e.size).unwrap_or(0);
                    offer.to_remote.bytes += size;
                    offer.to_remote.new.push(super::oneway::PlannedFile {
                        path: e.path,
                        size,
                        modtime_only: false,
                    });
                }
            }
            Err(e) => {
                let message = format!("Could not list the source\n  {e}");
                return flow.fail(&message, elapsed(clock), reporter);
            }
        }
    }
    reporter.resync(&offer);
    if run.dry_run {
        let mut summary = flow.ctx.summary(Outcome::Ok, elapsed(clock));
        summary.added.local = offer.to_local.new.len() as u64;
        summary.added.remote = offer.to_remote.new.len() as u64;
        summary.updated.remote = offer.to_remote.changed.len() as u64;
        summary.bytes = offer.to_local.bytes + offer.to_remote.bytes;
        summary.conflicts = offer
            .kept
            .iter()
            .map(|k| conflict_line(&k.path, k.kept_as.as_deref()))
            .collect();
        return flow.ctx.finish(summary, reporter);
    }
    if !run.yes
        && interrupts.unguarded(|| reporter.confirm(&Question::Resync(&offer))) == Answer::No
    {
        note(&mut flow.log, "declined", "the merge was not confirmed");
        let why = if interrupts.interrupted() {
            "not run, the run was stopped"
        } else {
            "not confirmed, nothing synced"
        };
        reporter.destination_skipped(flow.info, why);
        return Ended::Declined;
    }
    if interrupts.interrupted() {
        return flow.cancelled(0, reporter);
    }
    let clock = Instant::now();
    if missing {
        let made = match flow.dest {
            Destination::Local { path, .. } => {
                fs::create_dir(path).map_err(|e| format!("{path}: {e}"))
            }
            Destination::Onedrive { .. } => listing::mkdir(run.rclone, &flow.target)
                .await
                .map_err(|e| e.to_string()),
        };
        if let Err(e) = made {
            let message = format!("Could not create the destination folder\n  {e}");
            return flow.fail(&message, 0, reporter);
        }
        reporter.note(&format!("Created {}.", flow.info.display));
    }

    // The destination's differing copies are set aside under conflict
    // names first: --resync --resync-mode path1 overwrites them.
    let mut set_aside: Vec<ConflictDone> = Vec::new();
    for k in &offer.kept {
        let Some(kept_as) = &k.kept_as else { continue };
        let args = rc::moveto_args(
            &listing::join(&flow.target, &k.path),
            &listing::join(&flow.target, kept_as),
        );
        let mut tally = TwoWayTally::default();
        let moved = flow
            .stream(&args, &mut tally, None, false, None, reporter, interrupts)
            .await;
        if interrupts.interrupted() {
            return flow.stopped(
                Outcome::Cancelled,
                None,
                elapsed(clock),
                &set_aside,
                reporter,
            );
        }
        if !matches!(&moved, Ok(f) if f.exit.is_success()) {
            let message = format!(
                "Could not keep the destination's copy of {}\n  Renaming it to {kept_as} \
                 failed, so the merge did not run (it would have replaced that copy).",
                k.path
            );
            return flow.stopped(
                Outcome::Failed,
                Some(&message),
                elapsed(clock),
                &set_aside,
                reporter,
            );
        }
        set_aside.push(ConflictDone {
            path: k.path.clone(),
            kept_as: Some(kept_as.clone()),
        });
    }

    reporter.step(Step::Resyncing);
    let overwrites: HashSet<String> = offer.kept.iter().map(|k| k.path.clone()).collect();
    let target = flow.target.clone();
    let roots = [run.job.source.as_str(), target.as_str()];
    let mut tally = TwoWayTally::resync(overwrites).with_roots(&roots);
    let args = flow.args(&excludes, Call::Resync { dry_run: false });
    let finished = match flow
        .stream(&args, &mut tally, None, true, None, reporter, interrupts)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            let message = format!("Could not run rclone\n  {e}");
            return flow.fail(&message, elapsed(clock), reporter);
        }
    };
    flow.clear_own_lock(&finished).await;
    reporter.step(Step::Finishing);
    let mut outcome = tally.outcome(&finished);
    let mut errors = tally.acc.summary_errors(&finished);
    if let (Some(a), false) = (
        bisync::classify(tally.messages.iter().map(String::as_str)),
        finished.cancelled,
    ) {
        let message = flow.explain(&a);
        reporter.problem(&message);
        errors.retain(|e| !e.path.is_empty());
        errors.insert(
            0,
            FileError {
                path: String::new(),
                reason: first_line(&message).to_string(),
            },
        );
        if outcome == Outcome::Ok {
            outcome = Outcome::Failed;
        }
    }
    if outcome == Outcome::Ok || outcome == Outcome::Partial {
        let source = run.job.source.clone();
        let display = flow.dest.display();
        let stamp = rfc3339(&now());
        if let Err(e) = update_bisync(flow, |b| {
            b.initialised = true;
            b.last_resync = Some(stamp);
            b.source = Some(source);
            b.destination = Some(display);
            b.needs_resync = None;
        }) {
            reporter.problem(&format!("Could not record the sync state\n  {e:#}"));
        }
    }
    let mut summary = flow.ctx.summary(outcome, elapsed(clock));
    summary.added = tally.added;
    summary.updated = tally.updated;
    summary.bytes = tally.acc.bytes();
    summary.errors = errors;
    summary.conflicts = offer
        .kept
        .iter()
        .map(|k| conflict_line(&k.path, k.kept_as.as_deref()))
        .collect();
    flow.ctx.finish(summary, reporter)
}

#[cfg(test)]
mod tests;
