//! The `Reporter` trait: progress out, prompt answers in. It keeps `sync`
//! free of any UI dependency: the run screen (`ui::progress`), the plain
//! text output (`ui::plain`) and the `--json` stream (`jsonout::run`) each
//! implement it, and tests use [`Scripted`], which records what it was told
//! and answers prompts from a script.

use serde::Serialize;

use crate::model::{FileError, Mode, Summary};
use crate::rclone::bisync::{Abort, BisyncPhase};
use crate::rclone::events::Stats;

use super::oneway::Preview;
use super::plan::{Plan, ResyncOffer};

/// The destination a run is working on, and where it sits in the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DestinationInfo {
    pub job: String,
    pub id: String,
    /// `onedrive` or `local`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// As the user sees it: `work:Backups/Documents` or a local path.
    pub display: String,
    pub mode: Mode,
    /// 1-based position in the job's list of destinations being run.
    pub index: usize,
    pub of: usize,
    pub dry_run: bool,
}

/// The runner's coarse steps for one destination. The run screen refines
/// `Copying` into Listing, Comparing and Transferring from the stats, and
/// `Planning`, `Syncing` and `Resyncing` from bisync's phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// Source, drive and account checks.
    Checking,
    /// `copy --dry-run` and the destination listing.
    Previewing,
    /// One-way: moving files on the destination that were moved locally,
    /// before the copy.
    Moving,
    /// The real `copy`.
    Copying,
    /// Two-way: the `bisync --dry-run` that lists the plan.
    Planning,
    /// Two-way: copying folders back from the destination before the sync.
    Restoring,
    /// Two-way: the real `bisync`.
    Syncing,
    /// Two-way: a first run or a recovery (`bisync --resync`).
    Resyncing,
    /// The process has exited; the summary and state are being written.
    Finishing,
}

/// Live figures while a preview (`copy --dry-run` and the destination
/// listing) or a two-way plan (`bisync --dry-run`) runs, so a long listing
/// never looks stuck. Sent about once a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Looking {
    /// [`Step::Previewing`] or [`Step::Planning`].
    pub step: Step,
    /// Seconds since the step started, to the nearest second.
    pub elapsed_secs: u64,
    /// The same in milliseconds, for throttling.
    pub elapsed_ms: u64,
    /// Files compared on both sides so far (rclone's `checks`).
    pub checked: u64,
    /// Entries rclone has listed so far, source and destination together
    /// (its `listed`), when the rclone in use says.
    pub listed: Option<u64>,
    /// Files the dry run would copy so far, and their bytes.
    pub to_copy: u64,
    pub to_copy_bytes: u64,
    /// One-way: files the destination listing has read so far; `None` for
    /// a two-way plan, which lists both sides itself.
    pub destination_listed: Option<u64>,
    /// One-way: the destination listing has finished.
    pub destination_done: bool,
    /// One-way: the dry run has finished (the listing may still run).
    pub dry_run_done: bool,
    /// [`Step::Moving`]: files moved on the destination so far, of
    /// `to_move`.
    pub moved: u64,
    pub to_move: u64,
    /// One-way preview: the listings are done and this many new files are
    /// being checked as possible moves (hashing).
    pub checking_moves: Option<u64>,
}

impl Looking {
    pub fn new(step: Step) -> Looking {
        Looking {
            step,
            elapsed_secs: 0,
            elapsed_ms: 0,
            checked: 0,
            listed: None,
            to_copy: 0,
            to_copy_bytes: 0,
            destination_listed: None,
            destination_done: false,
            dry_run_done: false,
            moved: 0,
            to_move: 0,
            checking_moves: None,
        }
    }

    /// Sets the time since the step started.
    pub fn set_elapsed(&mut self, elapsed: std::time::Duration) {
        self.elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        // Rounded, so a figure a few milliseconds early reads as the
        // second it belongs to.
        self.elapsed_secs = self.elapsed_ms.saturating_add(500) / 1000;
    }

    /// Takes the counts from a stats line of the dry run.
    pub fn take_stats(&mut self, s: &Stats) {
        self.checked = s.checks;
        self.listed = s.listed;
        // A dry run counts each would-be copy as a transfer of its size.
        self.to_copy = s.transfers;
        self.to_copy_bytes = s.bytes;
    }

    /// The dry run is done and the destination listing is not: the screen
    /// says so, so the wait is explained.
    pub fn still_listing(&self) -> bool {
        self.dry_run_done && !self.destination_done && self.destination_listed.is_some()
    }
}

/// What happened to a file, as the plan classified it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Change {
    /// The file was not on that side.
    Added,
    /// That side had an older or different copy.
    Updated,
    /// The file was deleted on that side (two-way).
    Deleted,
}

/// Which side a file changed on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Where {
    /// The destination: OneDrive or the drive.
    #[default]
    Destination,
    /// This Mac (the job's source).
    Local,
}

/// One file finished during the real run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileDone {
    pub path: String,
    /// Bytes copied; `None` when only the modification time was set, or
    /// for a deletion.
    pub size: Option<u64>,
    pub change: Change,
    /// The side that changed: always the destination for one-way.
    pub side: Where,
}

/// The answer to a yes/no question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
}

/// A folder deleted locally, for the two-way folder-deletion prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeletedFolder {
    /// Relative to the job's source.
    pub path: String,
    /// On the destination now (`rclone size`), or as last listed when that
    /// could not be measured (`measured` false).
    pub files: u64,
    pub bytes: u64,
    pub measured: bool,
    /// The plan also adds as many files elsewhere, so it may be a rename.
    pub maybe_rename: bool,
}

/// The answer to the folder-deletion prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FolderAnswer {
    /// Leave the deletion in the plan: bisync removes it on the destination.
    Delete,
    /// Keep it on the destination and stop syncing it (a hold).
    Keep,
    /// Copy it back from the destination before the sync.
    Restore,
    /// Stop the run.
    CancelRun,
}

/// How a folder answer was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecidedBy {
    /// The user answered the prompt.
    Asked,
    /// `--folder-deletes` said so.
    Flag,
    /// No terminal (or `--json`): the safe default, keep.
    Default,
    /// A `--dry-run` on a terminal: the prompt would be asked, and was not.
    NotAsked,
}

/// `--folder-deletes`: how the folder-deletion prompt is answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FolderPolicy {
    /// Ask for each folder (a terminal without the flag).
    #[default]
    Ask,
    /// The same answer for every folder, from the flag.
    Answer(FolderAnswer),
    /// No terminal and no flag: keep, so an unattended run never deletes a
    /// folder on the destination.
    DefaultKeep,
}

/// What to do with files moved locally whose old copy is on the
/// destination: move that copy, or upload the file again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MovesAnswer {
    /// Move the copy on the destination (no upload).
    Move,
    /// Upload the file again under its new name; the old copy stays.
    Copy,
}

/// `--moves`: how the move question of a one-way run is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovesPolicy {
    /// Ask (a terminal, without `--yes`).
    Ask,
    /// The answer, and how it was reached: the flag, or the default
    /// (move) with `--yes` or without a terminal.
    Answer(MovesAnswer, DecidedBy),
}

impl Default for MovesPolicy {
    fn default() -> Self {
        MovesPolicy::Answer(MovesAnswer::Move, DecidedBy::Default)
    }
}

/// A yes/no question a run asks.
#[derive(Debug, Clone, Copy)]
pub enum Question<'a> {
    /// One-way: move the files found moved on the destination instead of
    /// uploading them again? Yes moves, no uploads them again.
    Moves(&'a Preview),
    /// Go ahead with this plan?
    Plan(&'a Plan),
    /// Merge both sides (a first run, or a recovery)?
    Resync(&'a ResyncOffer),
    /// The lock file's process is gone: remove it?
    StaleLock { pid: i32, path: &'a str },
    /// bisync stopped on a safety limit after the preview: run again with
    /// it lifted?
    Retry(&'a Abort),
}

/// A conflict after the real run: the local copy won, and the
/// destination's copy is kept there under another name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConflictDone {
    pub path: String,
    /// The losing copy's name on the destination, when bisync said it.
    pub kept_as: Option<String>,
}

/// Where the runner sends progress and asks its questions.
pub trait Reporter {
    /// A destination is starting.
    fn destination_started(&mut self, dest: &DestinationInfo);

    /// The runner moved to another step.
    fn step(&mut self, step: Step);

    /// bisync moved to another phase. Only the run screen and the JSON
    /// stream show these.
    fn phase(&mut self, phase: BisyncPhase) {
        let _ = phase;
    }

    /// A stats line from rclone (about once a second).
    fn stats(&mut self, stats: &Stats);

    /// Live figures while a preview or a plan runs (about once a second).
    fn looking(&mut self, looking: &Looking);

    /// A file finished copying, or was deleted.
    fn file(&mut self, file: &FileDone);

    /// A file failed (each file once, however many retries).
    fn file_error(&mut self, error: &FileError);

    /// Ctrl-C was seen: rclone has been asked to stop.
    fn stopping(&mut self);

    /// The preview is ready. Always called, before any question, so the
    /// plain and JSON outputs show it even with `--yes` or `--dry-run`.
    fn preview(&mut self, preview: &Preview);

    /// Asks whether to go ahead with the previewed copy.
    fn confirm_preview(&mut self, preview: &Preview) -> Answer;

    /// The two-way plan is ready (folder decisions included). Always
    /// called, before any question, as `preview` is.
    fn plan(&mut self, plan: &Plan);

    /// A first run or a recovery: what the merge will do. Always called
    /// before the question.
    fn resync(&mut self, offer: &ResyncOffer);

    /// Asks a yes/no question.
    fn confirm(&mut self, question: &Question) -> Answer;

    /// Asks what to do about folders deleted locally, one answer per
    /// folder. `destination_type` is `onedrive` or `local`. The default
    /// keeps them all, the safe unattended answer.
    fn ask_folders(
        &mut self,
        destination_type: &str,
        folders: &[DeletedFolder],
    ) -> Vec<FolderAnswer> {
        let _ = destination_type;
        vec![FolderAnswer::Keep; folders.len()]
    }

    /// What was decided about the moved files of a one-way preview, and
    /// how. Called once a preview with moves has its answer (not on a
    /// `--dry-run` that would ask).
    fn moves_decided(&mut self, preview: &Preview, answer: MovesAnswer, by: DecidedBy) {
        let _ = (preview, answer, by);
    }

    /// What was decided for a deleted folder, and how.
    fn folder_decided(&mut self, folder: &DeletedFolder, answer: FolderAnswer, by: DecidedBy);

    /// A conflict the real run resolved.
    fn conflict(&mut self, conflict: &ConflictDone);

    /// A problem that stops this destination before rclone runs, such as an
    /// unplugged drive. `message` may span lines.
    fn problem(&mut self, message: &str);

    /// A plain remark about this destination, such as "Nothing to copy."
    fn note(&mut self, message: &str);

    /// The destination was not run, with the reason.
    fn destination_skipped(&mut self, dest: &DestinationInfo, reason: &str);

    /// The destination is done; `summary` is what the summary block shows.
    fn destination_finished(&mut self, summary: &Summary);
}

/// Asks the run to stop, as a Ctrl-C would: for screens that read Ctrl-C
/// as a key, since raw mode turns the terminal's own SIGINT off.
pub fn request_stop() {
    crate::rclone::exec::press_interrupt();
}

/// Makes sure signals are caught from now on, so the exit hook runs
/// before one ends OneFolder (see `rclone::exec::install_signal_handler`).
pub fn install_signal_handler() {
    crate::rclone::exec::install_signal_handler();
}

/// Registers what to run before a signal ends OneFolder outright (when no
/// run is guarding against it): a screen restores the terminal here.
pub fn set_exit_hook(hook: Option<fn()>) {
    crate::rclone::exec::set_exit_hook(hook);
}

/// A reporter for tests: records every call as a line of text and answers
/// questions from a script.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct Scripted {
    pub log: Vec<String>,
    /// Answers to give, in order; `Yes` once they run out.
    pub answers: Vec<Answer>,
    /// Folder answers to give, in order; `Keep` once they run out.
    pub folder_answers: Vec<FolderAnswer>,
    pub previews: Vec<Preview>,
    pub plans: Vec<Plan>,
    pub summaries: Vec<Summary>,
    /// Every live figure sent while a preview or a plan ran.
    pub looking: Vec<Looking>,
    /// Presses Ctrl-C (once) when the moves have moved this many files,
    /// to stop a run part way through its moves.
    pub press_at_moved: Option<u64>,
}

#[cfg(test)]
impl Scripted {
    fn answer(&mut self) -> Answer {
        if self.answers.is_empty() {
            Answer::Yes
        } else {
            self.answers.remove(0)
        }
    }
}

#[cfg(test)]
impl Reporter for Scripted {
    fn destination_started(&mut self, dest: &DestinationInfo) {
        self.log
            .push(format!("start {} {}/{}", dest.id, dest.index, dest.of));
    }
    fn step(&mut self, step: Step) {
        self.log.push(format!("step {step:?}"));
    }
    fn stats(&mut self, _stats: &Stats) {
        if self.log.last().map(String::as_str) != Some("stats") {
            self.log.push("stats".into());
        }
    }
    fn looking(&mut self, looking: &Looking) {
        if self.log.last().map(String::as_str) != Some("looking") {
            self.log.push("looking".into());
        }
        self.looking.push(*looking);
        if let Some(n) = self.press_at_moved {
            if looking.step == Step::Moving && looking.moved >= n {
                self.press_at_moved = None;
                crate::rclone::exec::press_interrupt();
            }
        }
    }
    fn file(&mut self, file: &FileDone) {
        self.log.push(format!(
            "file {:?} {:?} {}",
            file.side, file.change, file.path
        ));
    }
    fn file_error(&mut self, error: &FileError) {
        self.log.push(format!("error {}", error.path));
    }
    fn stopping(&mut self) {
        self.log.push("stopping".into());
    }
    fn preview(&mut self, preview: &Preview) {
        self.log.push(format!(
            "preview new {} changed {}",
            preview.new.len(),
            preview.changed.len()
        ));
        self.previews.push(preview.clone());
    }
    fn confirm_preview(&mut self, _preview: &Preview) -> Answer {
        let answer = self.answer();
        self.log.push(format!("confirm {answer:?}"));
        answer
    }
    fn plan(&mut self, plan: &Plan) {
        self.log.push(format!(
            "plan up {} down {} delete-remote {} delete-local {} conflicts {}",
            plan.to_remote.files(),
            plan.to_local.files(),
            plan.delete_remote.len(),
            plan.delete_local.len(),
            plan.conflicts.len()
        ));
        self.plans.push(plan.clone());
    }
    fn resync(&mut self, offer: &ResyncOffer) {
        self.log.push(format!("resync {:?}", offer.reason));
    }
    fn confirm(&mut self, question: &Question) -> Answer {
        let what = match question {
            Question::Moves(_) => "moves",
            Question::Plan(_) => "plan",
            Question::Resync(_) => "resync",
            Question::StaleLock { .. } => "stale-lock",
            Question::Retry(_) => "retry",
        };
        let answer = self.answer();
        self.log.push(format!("confirm {what} {answer:?}"));
        answer
    }
    fn moves_decided(&mut self, preview: &Preview, answer: MovesAnswer, by: DecidedBy) {
        self.log
            .push(format!("moves {} {answer:?} {by:?}", preview.moves.len()));
    }
    fn ask_folders(&mut self, _kind: &str, folders: &[DeletedFolder]) -> Vec<FolderAnswer> {
        folders
            .iter()
            .map(|_| {
                if self.folder_answers.is_empty() {
                    FolderAnswer::Keep
                } else {
                    self.folder_answers.remove(0)
                }
            })
            .collect()
    }
    fn folder_decided(&mut self, folder: &DeletedFolder, answer: FolderAnswer, by: DecidedBy) {
        self.log
            .push(format!("folder {} {answer:?} {by:?}", folder.path));
    }
    fn conflict(&mut self, conflict: &ConflictDone) {
        self.log.push(format!(
            "conflict {} {}",
            conflict.path,
            conflict.kept_as.as_deref().unwrap_or("?")
        ));
    }
    fn problem(&mut self, message: &str) {
        self.log.push(format!("problem {message}"));
    }
    fn note(&mut self, message: &str) {
        self.log.push(format!("note {message}"));
    }
    fn destination_skipped(&mut self, dest: &DestinationInfo, reason: &str) {
        self.log.push(format!("skipped {} {reason}", dest.id));
    }
    fn destination_finished(&mut self, summary: &Summary) {
        self.log
            .push(format!("finished {}", summary.outcome.as_str()));
        self.summaries.push(summary.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_prompt_defaults_to_keep() {
        struct Silent;
        impl Reporter for Silent {
            fn destination_started(&mut self, _: &DestinationInfo) {}
            fn step(&mut self, _: Step) {}
            fn stats(&mut self, _: &Stats) {}
            fn looking(&mut self, _: &Looking) {}
            fn file(&mut self, _: &FileDone) {}
            fn file_error(&mut self, _: &FileError) {}
            fn stopping(&mut self) {}
            fn preview(&mut self, _: &Preview) {}
            fn confirm_preview(&mut self, _: &Preview) -> Answer {
                Answer::No
            }
            fn plan(&mut self, _: &Plan) {}
            fn resync(&mut self, _: &ResyncOffer) {}
            fn confirm(&mut self, _: &Question) -> Answer {
                Answer::No
            }
            fn folder_decided(&mut self, _: &DeletedFolder, _: FolderAnswer, _: DecidedBy) {}
            fn conflict(&mut self, _: &ConflictDone) {}
            fn problem(&mut self, _: &str) {}
            fn note(&mut self, _: &str) {}
            fn destination_skipped(&mut self, _: &DestinationInfo, _: &str) {}
            fn destination_finished(&mut self, _: &Summary) {}
        }
        let folder = DeletedFolder {
            path: "Old projects/2019".into(),
            files: 1284,
            bytes: 3_400_000_000,
            measured: true,
            maybe_rename: false,
        };
        assert_eq!(
            Silent.ask_folders("onedrive", &[folder.clone(), folder]),
            [FolderAnswer::Keep, FolderAnswer::Keep]
        );
    }

    #[test]
    fn scripted_answers_in_order_then_yes() {
        let mut r = Scripted {
            answers: vec![Answer::No],
            ..Scripted::default()
        };
        let p = Preview::default();
        assert_eq!(r.confirm_preview(&p), Answer::No);
        assert_eq!(r.confirm_preview(&p), Answer::Yes);
    }
}
