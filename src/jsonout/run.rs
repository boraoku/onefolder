//! `onefolder run --json`: newline-delimited JSON, one object per line, each
//! with an `event` field. Every event about a destination carries `job` and
//! `destination` (the destination's id).
//!
//! ```text
//! {"event":"start","job":"documents","destination":"d-91c0","type":"local",
//!  "display":"/Volumes/Backup/Documents","mode":"one-way","index":1,"of":2,"dry_run":false}
//! {"event":"step","job":..,"destination":..,"step":"checking|previewing|moving|copying|finishing"}
//! {"event":"preview","job":..,"destination":..,
//!  "new":{"files":37,"bytes":412600000,"paths":["first 10 paths"]},
//!  "changed":{"files":5,"bytes":18200000,"paths":[..]},
//!  "unchanged":981,"destination_only":12,"excludes":["node_modules/**"],
//!  "copy_hidden":false,"effective_excludes":[".DS_Store","._*",..,".*",".*/**","node_modules/**"],
//!  "moves":{"count":1204,"bytes":3100000000,"choice":"undecided|move|copy",
//!           "pairs":[{"from":"Projects/2025/a.txt","to":"Archive/2025/a.txt","size":12}],
//!           "groups":[{"from":"Projects/2025","to":"Archive/2025","files":1198,
//!                      "bytes":3000000000,"folder":true}]}}
//! {"event":"moves","job":..,"destination":..,"answer":"move|copy","by":"asked|flag|default|not-asked",
//!  "count":1204,"bytes":3100000000}
//! {"event":"progress","job":..,"destination":..,"step":"previewing|planning","elapsed":134,
//!  "checked":12408,"listed":31220|null,"to_copy":{"files":1204,"bytes":3200000000},
//!  "destination_listed":18330|null,"destination_done":false,"still_listing":false,
//!  "checking_moves":12 (only while new files are checked as possible moves)}
//! {"event":"progress","job":..,"destination":..,"step":"moving","elapsed":3,
//!  "moved":{"files":12,"of":1204}}
//! {"event":"stats","job":..,"destination":..,"bytes":..,"total_bytes":..,
//!  "transfers":..,"total_transfers":..,"checks":..,"total_checks":..,"errors":..,
//!  "speed":12800000.0,"eta":35.0|null,"elapsed":24.0,
//!  "transferring":[{"name":..,"bytes":..,"size":..,"percentage":62.0}]}
//! {"event":"file","job":..,"destination":..,"path":"a.txt","size":1200|null,
//!  "change":"added|updated|deleted","side":"destination|local"}
//! {"event":"error","job":..,"destination":..,"path":"locked.txt","message":"..."}
//! {"event":"stopping","job":..,"destination":..}
//! {"event":"problem","job":..,"destination":..,"message":"Backup drive not mounted\n  ..."}
//! {"event":"note","job":..,"destination":..,"message":"Nothing to copy."}
//! {"event":"skipped","job":..,"destination":..,"reason":"not confirmed, nothing copied"}
//! {"event":"summary","job":..,"destination":..,"summary":{ the summary block's fields:
//!  job, destination (as shown), destination_type, mode, outcome, dry_run, added, updated,
//!  deleted ({"local":n,"remote":n} each), unchanged, bytes, duration_ms,
//!  errors ([{"path","reason"}]), conflicts, destination_only, log, for one-way
//!  moved ({"files":n,"bytes":n}) when moved files were moved (or would be, in a
//!  dry run), and for two-way removed_folders when a folder answered Delete was
//!  removed with everything in it }}
//! {"event":"done","job":..,"ok":true,"exit_code":0,
//!  "destinations":[{"destination":"d-91c0","outcome":"ok|partial|failed|cancelled|skipped|declined",
//!                   "reason":"Backup drive not mounted"|null}]}
//! ```
//!
//! `done` lists every destination in the order they ran (config order),
//! with `exit_code` 0 when all are ok, 1 when any is partial, failed,
//! cancelled, skipped or declined, and 2 when all failed. A run refused
//! before it started (`refused`, then `done` with no destinations) also
//! exits 2, including `code: "locked"` when another run of the job holds
//! its lock.
//!
//! Two-way destinations add these (steps are also `planning`, `restoring`,
//! `syncing` and `resyncing`):
//!
//! ```text
//! {"event":"phase","job":..,"destination":..,"phase":"listing|checking|applying|updating|resyncing"}
//! {"event":"resync","job":..,"destination":..,"reason":"first-run|pair-changed|damaged|critical|requested",
//!  "detail":"..."|null,"destination_missing":false,
//!  "to_destination":{"files":12,"bytes":3400,"new":[..first 10],"changed":[..]},
//!  "to_local":{ the same },
//!  "kept":[{"path":"mod.txt","kept_as":"mod.conflict-2026-10-13-1.txt"}]}
//! {"event":"folder_prompt","job":..,"destination":..,"path":"Old projects/2019",
//!  "files":1284,"bytes":3400000000,"measured":true,"maybe_rename":false,
//!  "answer":"delete|keep|restore|cancel-run","by":"asked|flag|default"}
//! {"event":"plan","job":..,"destination":..,
//!  "to_destination":{ as in resync },"to_local":{ as in resync },
//!  "delete_destination":["every path"],"delete_local":["every path"],
//!  "conflicts":[{"path":"Notes/plan.md","kept_as":"Notes/plan.conflict-2026-10-13-1.md"|null}],
//!  "folders":[{"path":..,"answer":"keep","by":"default"}],
//!  "needs_confirmation":[{"kind":"deletions|all-changed","side":"destination|local",
//!                         "count":6,"total":10,"percent":60}],
//!  "lift_max_delete":false,"force":false,"excludes":[..],"copy_hidden":false,
//!  "effective_excludes":[..],"holds":[..]}
//! {"event":"conflict","job":..,"destination":..,"path":"Notes/plan.md",
//!  "kept_as":"Notes/plan.conflict-2026-10-13-1.md"|null}
//! ```
//!
//! `folder_prompt` comes once per deleted folder, with the answer that
//! applies: with `--json` the prompt is never shown, so it is the
//! `--folder-deletes` value (`by: flag`) or keep (`by: default`).
//!
//! A run refused before any destination starts (no terminal and no
//! `--yes`, an unknown job) prints one line and then `done`:
//!
//! ```text
//! {"event":"refused","code":"needs-yes","message":"..."}
//! ```
//!
//! `progress` comes at most once a second while a preview (`copy
//! --dry-run` and the destination listing) or a two-way plan (`bisync
//! --dry-run`, also before a merge) runs: the seconds since the step
//! started, files checked (`checks`), entries listed on both sides
//! (rclone's `listed`, null when the rclone in use does not say), files
//! the dry run would copy so far and their bytes, and for one-way the
//! destination files listed so far (null for two-way), whether that
//! listing is done, and `still_listing` once the dry run has finished
//! before it. `excludes` in `preview` and `plan` are the job's own
//! patterns (the plan adds the conflict pattern); `effective_excludes`
//! is every pattern rclone gets: the built-in list, the hidden patterns
//! (`.*`, `.*/**`) unless `copy_hidden`, then the job's (the plan adds
//! the holds and the conflict pattern).
//!
//! `file` events come only from the real copy; a `--dry-run` reports what
//! would happen in `preview` and in the `summary` (with `dry_run: true`).
//!
//! `moves` in `preview` lists files moved locally whose old copy is on the
//! destination under the old name (`count` 0 when there are none);
//! `choice` is `undecided` until answered, `move` or `copy` (uploaded
//! again, as `--moves copy`) once the flag or the default answers it. The
//! `moves` event says what was decided and how: with `--json` it is never
//! asked, so it is the `--moves` value (`by: flag`), move (`by: default`),
//! or, for `--moves ask` with no terminal to ask on, copy (`by:
//! not-asked`): an explicit ask never moves without a yes. While the files
//! move (step `moving`), `progress` carries how many have moved so far.
//! A move that fails is an `error` event for its new path, and the copy
//! uploads that file instead.
//!
//! Several jobs (`run documents records`, `run --all`) make a batch. Every
//! event of a batch except `refused` and `batch-done` carries `batch`, the job's place in it
//! (`index` from 1) and how many jobs; each job that runs streams the
//! events above and ends with its own `done`; a job that does not run
//! sends `job-skipped` instead; and `batch-done` closes the stream with
//! every job in config order, each with its destinations as `done` lists
//! them (a job that did not run lists each of its destinations as skipped,
//! with the reason), and the batch's exit code by the same rule over every
//! destination (2 also when no job ran a destination at all):
//!
//! ```text
//! {"event":"start","job":"documents",..,"batch":{"index":1,"of":3}}
//! {"event":"job-skipped","job":"photos","code":"locked|lock-failed|no-destinations|stopped",
//!  "reason":"another run in progress","batch":{"index":3,"of":3}}
//! {"event":"batch-done","ok":false,"exit_code":1,
//!  "jobs":[{"job":"documents","outcome":"ok|partial|failed|stopped|skipped",
//!           "reason":"another run in progress"|null,
//!           "destinations":[{"destination":"d-91c0","outcome":"ok","reason":null}]}]}
//! ```
//!
//! `stopped` is a job Ctrl-C stopped part way; the jobs after it are
//! skipped with `code: "stopped"` and reason `not run, the batch was
//! stopped`. A batch refused before any job ran (an unknown job, `--dest`
//! with several jobs) sends `refused` and a `batch-done` with no jobs,
//! neither with `batch`. One job given alone never has a `batch` field or
//! a `batch-done` event.

use std::io::Write;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::model::{FileError, Summary};
use crate::rclone::bisync::BisyncPhase;
use crate::rclone::events::Stats;
use crate::sync::oneway::{PlannedFile, Preview, PREVIEW_PATHS};
use crate::sync::plan::{Direction, Plan, ResyncOffer};
use crate::sync::reporter::{
    Answer, ConflictDone, DecidedBy, DeletedFolder, DestinationInfo, FileDone, FolderAnswer,
    Looking, MovesAnswer, Question, Reporter, Step,
};

/// The least time between two `progress` events: a second, less a little,
/// so a stats line that arrives a few milliseconds early is not dropped.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(900);

/// One destination's result in the closing `done` line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DestinationResult {
    pub destination: String,
    pub outcome: String,
    /// Why it did not go to plan ("Backup drive not mounted"), or null.
    pub reason: Option<String>,
}

/// Writes the NDJSON stream to a sink (stdout in use, a buffer in tests).
pub struct JsonReporter<W: Write> {
    out: W,
    job: String,
    destination: String,
    /// When the last `progress` event went out.
    last_progress: Option<Instant>,
    /// In a batch: the job's place in it, 1-based, and how many jobs.
    batch: Option<(usize, usize)>,
}

/// One job's result in the closing `batch-done` line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobResult {
    pub job: String,
    /// `ok`, `partial`, `failed`, `stopped` or `skipped`.
    pub outcome: String,
    /// Why it was skipped or not run, or null.
    pub reason: Option<String>,
    pub destinations: Vec<DestinationResult>,
}

impl JsonReporter<std::io::Stdout> {
    pub fn stdout(job: &str) -> Self {
        JsonReporter::new(std::io::stdout(), job)
    }
}

impl<W: Write> JsonReporter<W> {
    pub fn new(out: W, job: &str) -> Self {
        JsonReporter {
            out,
            job: job.to_string(),
            destination: String::new(),
            last_progress: None,
            batch: None,
        }
    }

    /// The next job of a batch: every event from now on carries
    /// `batch: {index, of}`.
    pub fn begin_job(&mut self, job: &str, index: usize, of: usize) {
        self.job = job.to_string();
        self.destination = String::new();
        self.last_progress = None;
        self.batch = Some((index, of));
    }

    /// A job of the batch that does not run: `job-skipped`, with why.
    pub fn job_skipped(&mut self, code: &str, reason: &str) {
        self.line(json!({
            "event": "job-skipped",
            "job": self.job,
            "code": code,
            "reason": reason,
        }));
    }

    /// The batch's closing line: every job and its destinations' results,
    /// and the batch's exit code. It carries no `batch` field.
    pub fn batch_done(&mut self, exit_code: u8, jobs: &[JobResult]) {
        self.batch = None;
        self.line(json!({
            "event": "batch-done",
            "ok": exit_code == 0,
            "exit_code": exit_code,
            "jobs": jobs,
        }));
    }

    #[cfg(test)]
    pub fn into_inner(self) -> W {
        self.out
    }

    /// Writes one line, with the job's place in the batch in a batch. A
    /// closed pipe must not stop the sync.
    fn line(&mut self, mut value: serde_json::Value) {
        if let (Some((index, of)), Some(map)) = (self.batch, value.as_object_mut()) {
            map.insert("batch".to_string(), json!({"index": index, "of": of}));
        }
        let _ = writeln!(self.out, "{value}");
        let _ = self.out.flush();
    }

    /// An event about the current destination.
    fn event(&mut self, event: &str, fields: serde_json::Value) {
        let mut value = json!({
            "event": event,
            "job": self.job,
            "destination": self.destination,
        });
        if let (Some(map), serde_json::Value::Object(extra)) = (value.as_object_mut(), fields) {
            map.extend(extra);
        }
        self.line(value);
    }

    /// The line for a run refused before it started.
    pub fn refused(&mut self, code: &str, message: &str) {
        self.line(json!({"event": "refused", "code": code, "message": message}));
    }

    /// The closing line.
    pub fn done(&mut self, exit_code: u8, destinations: &[DestinationResult]) {
        self.line(json!({
            "event": "done",
            "job": self.job,
            "ok": exit_code == 0,
            "exit_code": exit_code,
            "destinations": destinations,
        }));
    }
}

fn group(files: &[PlannedFile], bytes: u64) -> serde_json::Value {
    json!({
        "files": files.len(),
        "bytes": bytes,
        "paths": files.iter().take(PREVIEW_PATHS).map(|f| f.path.as_str()).collect::<Vec<_>>(),
    })
}

/// The preview's moved files: how many, their bytes, what happens to
/// them, and the first [`PREVIEW_PATHS`] files and groups.
fn moves(p: &Preview) -> serde_json::Value {
    json!({
        "count": p.moves.len(),
        "bytes": p.move_bytes,
        "choice": p.moves_choice,
        "pairs": p.moves.iter().take(PREVIEW_PATHS).map(|m| json!({
            "from": m.from, "to": m.to, "size": m.size,
        })).collect::<Vec<_>>(),
        "groups": p.move_groups.iter().take(PREVIEW_PATHS).map(|g| json!({
            "from": g.from, "to": g.to, "files": g.files, "bytes": g.bytes, "folder": g.folder,
        })).collect::<Vec<_>>(),
    })
}

fn direction(d: &Direction) -> serde_json::Value {
    json!({
        "files": d.files(),
        "bytes": d.bytes,
        "new": d.new.iter().take(PREVIEW_PATHS).map(|f| f.path.as_str()).collect::<Vec<_>>(),
        "changed": d.changed.iter().take(PREVIEW_PATHS).map(|f| f.path.as_str()).collect::<Vec<_>>(),
    })
}

impl<W: Write> Reporter for JsonReporter<W> {
    fn phase(&mut self, phase: BisyncPhase) {
        self.event("phase", json!({ "phase": phase }));
    }

    fn plan(&mut self, p: &Plan) {
        self.event(
            "plan",
            json!({
                "to_destination": direction(&p.to_remote),
                "to_local": direction(&p.to_local),
                "delete_destination": p.delete_remote,
                "delete_local": p.delete_local,
                "conflicts": p.conflicts,
                "folders": p.folders.iter().map(|d| json!({
                    "path": d.folder.path, "answer": d.answer, "by": d.by,
                })).collect::<Vec<_>>(),
                "needs_confirmation": p.big.iter().map(|b| json!({
                    "kind": b.kind, "side": b.side, "count": b.count,
                    "total": b.total, "percent": b.percent(),
                })).collect::<Vec<_>>(),
                "lift_max_delete": p.lift,
                "force": p.force,
                "excludes": p.excludes,
                "copy_hidden": p.copy_hidden,
                "effective_excludes": p.effective_excludes,
                "holds": p.holds,
            }),
        );
    }

    fn resync(&mut self, o: &ResyncOffer) {
        self.event(
            "resync",
            json!({
                "reason": o.reason,
                "detail": o.detail,
                "destination_missing": o.destination_missing,
                "to_destination": direction(&o.to_remote),
                "to_local": direction(&o.to_local),
                "kept": o.kept,
            }),
        );
    }

    /// Never asked: `--json` runs cannot answer. Answers no to be safe.
    fn confirm(&mut self, _question: &Question) -> Answer {
        Answer::No
    }

    fn folder_decided(&mut self, f: &DeletedFolder, answer: FolderAnswer, by: DecidedBy) {
        self.event(
            "folder_prompt",
            json!({
                "path": f.path,
                "files": f.files,
                "bytes": f.bytes,
                "measured": f.measured,
                "maybe_rename": f.maybe_rename,
                "answer": answer,
                "by": by,
            }),
        );
    }

    fn conflict(&mut self, c: &ConflictDone) {
        self.event("conflict", json!({ "path": c.path, "kept_as": c.kept_as }));
    }

    fn destination_started(&mut self, dest: &DestinationInfo) {
        self.destination = dest.id.clone();
        self.event(
            "start",
            json!({
                "type": dest.kind,
                "display": dest.display,
                "mode": dest.mode,
                "index": dest.index,
                "of": dest.of,
                "dry_run": dest.dry_run,
            }),
        );
    }

    fn step(&mut self, step: Step) {
        self.last_progress = None;
        self.event("step", json!({ "step": step }));
    }

    fn looking(&mut self, l: &Looking) {
        if self
            .last_progress
            .is_some_and(|t| t.elapsed() < PROGRESS_EVERY)
        {
            return;
        }
        self.last_progress = Some(Instant::now());
        if l.step == Step::Moving {
            self.event(
                "progress",
                json!({
                    "step": l.step,
                    "elapsed": l.elapsed_secs,
                    "moved": {"files": l.moved, "of": l.to_move},
                }),
            );
            return;
        }
        let mut fields = json!({
                "step": l.step,
                "elapsed": l.elapsed_secs,
                "checked": l.checked,
                "listed": l.listed,
                "to_copy": {"files": l.to_copy, "bytes": l.to_copy_bytes},
                "destination_listed": l.destination_listed,
                "destination_done": l.destination_done,
                "still_listing": l.still_listing(),
        });
        if let Some(n) = l.checking_moves {
            fields["checking_moves"] = json!(n);
        }
        self.event("progress", fields);
    }

    fn stats(&mut self, s: &Stats) {
        self.event(
            "stats",
            json!({
                "bytes": s.bytes,
                "total_bytes": s.total_bytes,
                "transfers": s.transfers,
                "total_transfers": s.total_transfers,
                "checks": s.checks,
                "total_checks": s.total_checks,
                "errors": s.errors,
                "speed": s.speed,
                "eta": s.eta,
                "elapsed": s.elapsed_time,
                "transferring": s.transferring,
            }),
        );
    }

    fn file(&mut self, f: &FileDone) {
        self.event(
            "file",
            json!({ "path": f.path, "size": f.size, "change": f.change, "side": f.side }),
        );
    }

    fn file_error(&mut self, e: &FileError) {
        self.event("error", json!({ "path": e.path, "message": e.reason }));
    }

    fn stopping(&mut self) {
        self.event("stopping", json!({}));
    }

    fn preview(&mut self, p: &Preview) {
        self.event(
            "preview",
            json!({
                "new": group(&p.new, p.new_bytes),
                "changed": group(&p.changed, p.changed_bytes),
                "unchanged": p.unchanged,
                "destination_only": p.destination_only,
                "excludes": p.excludes,
                "copy_hidden": p.copy_hidden,
                "effective_excludes": p.effective_excludes,
                "moves": moves(p),
            }),
        );
    }

    fn moves_decided(&mut self, p: &Preview, answer: MovesAnswer, by: DecidedBy) {
        self.event(
            "moves",
            json!({
                "answer": answer,
                "by": by,
                "count": p.moves.len(),
                "bytes": p.move_bytes,
            }),
        );
    }

    /// Never asked: `--json` runs need `--yes` or `--dry-run`. Answers no
    /// to be safe if it ever is.
    fn confirm_preview(&mut self, _preview: &Preview) -> Answer {
        Answer::No
    }

    fn problem(&mut self, message: &str) {
        self.event("problem", json!({ "message": message }));
    }

    fn note(&mut self, message: &str) {
        self.event("note", json!({ "message": message }));
    }

    fn destination_skipped(&mut self, dest: &DestinationInfo, reason: &str) {
        self.destination = dest.id.clone();
        self.event("skipped", json!({ "reason": reason }));
    }

    fn destination_finished(&mut self, summary: &Summary) {
        self.event("summary", json!({ "summary": summary }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mode, Outcome, SideCounts};
    use crate::rclone::events::InFlight;
    use crate::sync::reporter::{Change, Where};

    #[test]
    fn stream_shapes() {
        let mut r = JsonReporter::new(Vec::new(), "documents");
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
        r.step(Step::Previewing);
        let mut looking = Looking::new(Step::Previewing);
        looking.elapsed_secs = 2;
        looking.checked = 3;
        looking.listed = Some(9);
        looking.to_copy = 1;
        looking.to_copy_bytes = 6;
        looking.destination_listed = Some(4);
        r.looking(&looking);
        // At most one a second: the next one is dropped.
        looking.elapsed_secs = 3;
        r.looking(&looking);
        r.preview(&Preview {
            job: "documents".into(),
            destination_id: "d-91c0".into(),
            destination: dest.display.clone(),
            new: vec![PlannedFile {
                path: "new.txt".into(),
                size: 6,
                modtime_only: false,
            }],
            changed: vec![],
            new_bytes: 6,
            changed_bytes: 0,
            unchanged: 3,
            destination_only: 1,
            excludes: vec!["node_modules/**".into()],
            copy_hidden: false,
            effective_excludes: crate::rclone::effective_excludes(
                &["node_modules/**".into()],
                false,
            ),
            ..Preview::default()
        });
        r.step(Step::Copying);
        r.stats(&Stats {
            bytes: 3,
            total_bytes: 6,
            transfers: 0,
            total_transfers: 1,
            speed: 1.5,
            eta: Some(2.0),
            elapsed_time: 1.0,
            transferring: vec![InFlight {
                name: "new.txt".into(),
                bytes: 3,
                size: 6,
                percentage: 50.0,
            }],
            ..Stats::default()
        });
        r.file(&FileDone {
            path: "new.txt".into(),
            size: Some(6),
            change: Change::Added,
            side: Where::Destination,
        });
        r.file_error(&FileError {
            path: "locked.txt".into(),
            reason: "permission denied".into(),
        });
        r.stopping();
        r.problem("Backup drive not mounted\n  Plug it in.");
        r.note("Nothing to copy.");
        r.destination_finished(&Summary {
            job: "documents".into(),
            destination: dest.display.clone(),
            destination_type: "local".into(),
            mode: Mode::OneWay,
            outcome: Outcome::Partial,
            dry_run: false,
            added: SideCounts {
                local: 0,
                remote: 1,
            },
            updated: SideCounts::default(),
            deleted: SideCounts::default(),
            unchanged: Some(3),
            bytes: 6,
            duration_ms: 1200,
            errors: vec![FileError {
                path: "locked.txt".into(),
                reason: "permission denied".into(),
            }],
            conflicts: vec![],
            removed_folders: Vec::new(),
            moved: None,
            destination_only: Some(1),
            log: Some(
                "~/.local/share/onefolder/logs/documents/d-91c0/2026-10-13T094102.jsonl".into(),
            ),
        });
        let two = DestinationInfo {
            id: "d-7f3a".into(),
            kind: "onedrive",
            display: "work:Backups/Documents".into(),
            mode: Mode::TwoWay,
            index: 2,
            ..dest
        };
        r.destination_skipped(&two, "not confirmed, nothing synced");
        r.done(
            1,
            &[
                DestinationResult {
                    destination: "d-91c0".into(),
                    outcome: "partial".into(),
                    reason: None,
                },
                DestinationResult {
                    destination: "d-7f3a".into(),
                    outcome: "skipped".into(),
                    reason: Some("not confirmed, nothing synced".into()),
                },
            ],
        );
        r.refused("needs-yes", "add --yes");
        let text = String::from_utf8(r.into_inner()).unwrap();
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v["event"].is_string(), "{line}");
        }
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!("run_ndjson", text);
        });
    }

    /// The moved-file parts of the stream: the preview's `moves`, the
    /// decision, the moving step and its progress, a failed move, and the
    /// summary's `moved`.
    #[test]
    fn moves_shapes() {
        use crate::model::Moved;
        use crate::sync::moves::{MoveGroup, MovesChoice, PlannedMove};
        let mut r = JsonReporter::new(Vec::new(), "documents");
        let dest = DestinationInfo {
            job: "documents".into(),
            id: "d-7f3a".into(),
            kind: "onedrive",
            display: "work:Backups/Documents".into(),
            mode: Mode::OneWay,
            index: 1,
            of: 1,
            dry_run: false,
        };
        r.destination_started(&dest);
        let mut p = Preview {
            job: "documents".into(),
            destination_id: "d-7f3a".into(),
            destination: dest.display.clone(),
            destination_type: "onedrive".into(),
            move_groups: vec![
                MoveGroup {
                    from: "Projects/2025".into(),
                    to: "Archive/2025".into(),
                    files: 2,
                    bytes: 30,
                    folder: true,
                },
                MoveGroup {
                    from: "a.txt".into(),
                    to: "Notes/a.txt".into(),
                    files: 1,
                    bytes: 5,
                    folder: false,
                },
            ],
            moves: vec![
                PlannedMove {
                    from: "Projects/2025/x.pdf".into(),
                    to: "Archive/2025/x.pdf".into(),
                    size: 10,
                    group: 0,
                },
                PlannedMove {
                    from: "Projects/2025/y.pdf".into(),
                    to: "Archive/2025/y.pdf".into(),
                    size: 20,
                    group: 0,
                },
                PlannedMove {
                    from: "a.txt".into(),
                    to: "Notes/a.txt".into(),
                    size: 5,
                    group: 1,
                },
            ],
            move_bytes: 35,
            ..Preview::default()
        };
        p.choose_moves(MovesChoice::Move);
        r.preview(&p);
        r.moves_decided(&p, MovesAnswer::Move, DecidedBy::Default);
        r.step(Step::Moving);
        let mut l = Looking::new(Step::Moving);
        l.moved = 2;
        l.to_move = 3;
        l.elapsed_secs = 1;
        r.looking(&l);
        r.file_error(&FileError {
            path: "Notes/a.txt".into(),
            reason: "could not be moved from a.txt on OneDrive, so it is uploaded instead: \
                     access denied"
                .into(),
        });
        r.step(Step::Copying);
        let summary = Summary {
            job: "documents".into(),
            destination: dest.display.clone(),
            destination_type: "onedrive".into(),
            mode: Mode::OneWay,
            outcome: Outcome::Partial,
            dry_run: false,
            added: SideCounts {
                local: 0,
                remote: 1,
            },
            updated: SideCounts::default(),
            deleted: SideCounts::default(),
            unchanged: Some(0),
            bytes: 5,
            duration_ms: 900,
            errors: vec![],
            conflicts: vec![],
            removed_folders: Vec::new(),
            moved: Some(Moved {
                files: 2,
                bytes: 30,
            }),
            destination_only: Some(0),
            log: None,
        };
        r.destination_finished(&summary);
        let text = String::from_utf8(r.into_inner()).unwrap();
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["destination"], "d-7f3a", "{line}");
        }
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!("run_ndjson_moves", text);
        });
    }

    /// A figure a little under a second after the last one still goes
    /// out (stats lines come every second, give or take a few ms); one
    /// straight after it does not.
    #[test]
    fn progress_at_most_about_once_a_second() {
        let mut r = JsonReporter::new(Vec::new(), "documents");
        let l = Looking::new(Step::Planning);
        r.looking(&l);
        r.looking(&l);
        std::thread::sleep(Duration::from_millis(950));
        r.looking(&l);
        r.looking(&l);
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert_eq!(text.matches("\"progress\"").count(), 2, "{text}");
    }

    /// The events two-way runs add.
    #[test]
    fn two_way_shapes() {
        use crate::rclone::bisync::BisyncPhase;
        use crate::sync::plan::{Direction, Plan, PlannedConflict, ResyncOffer, ResyncReason};
        use crate::sync::reporter::{ConflictDone, DecidedBy, DeletedFolder, FolderAnswer};
        let mut r = JsonReporter::new(Vec::new(), "documents");
        let dest = DestinationInfo {
            job: "documents".into(),
            id: "d-7f3a".into(),
            kind: "onedrive",
            display: "work:Backups/Documents".into(),
            mode: Mode::TwoWay,
            index: 1,
            of: 1,
            dry_run: false,
        };
        r.destination_started(&dest);
        r.step(Step::Planning);
        r.phase(BisyncPhase::Listing);
        let up = Direction {
            new: vec![PlannedFile {
                path: "new.txt".into(),
                size: 6,
                modtime_only: false,
            }],
            changed: vec![],
            bytes: 6,
        };
        r.resync(&ResyncOffer {
            reason: Some(ResyncReason::FirstRun),
            to_remote: up.clone(),
            kept: vec![PlannedConflict {
                path: "mod.txt".into(),
                kept_as: Some("mod.conflict-2026-10-13-1.txt".into()),
            }],
            ..ResyncOffer::default()
        });
        let folder = DeletedFolder {
            path: "Old projects/2019".into(),
            files: 2,
            bytes: 20,
            measured: true,
            maybe_rename: false,
        };
        r.folder_decided(&folder, FolderAnswer::Keep, DecidedBy::Default);
        r.plan(&Plan {
            to_remote: up,
            delete_local: vec!["gone.txt".into()],
            conflicts: vec![PlannedConflict {
                path: "Notes/plan.md".into(),
                kept_as: Some("Notes/plan.conflict-2026-10-13-1.md".into()),
            }],
            holds: vec!["Archive".into()],
            ..Plan::default()
        });
        r.step(Step::Syncing);
        r.file(&FileDone {
            path: "gone.txt".into(),
            size: None,
            change: Change::Deleted,
            side: Where::Local,
        });
        r.conflict(&ConflictDone {
            path: "Notes/plan.md".into(),
            kept_as: Some("Notes/plan.conflict-2026-10-13-1.md".into()),
        });
        let text = String::from_utf8(r.into_inner()).unwrap();
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["destination"], "d-7f3a", "{line}");
        }
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!("run_ndjson_two_way", text);
        });
    }

    /// In a batch every event carries the job's place in it; a job that
    /// does not run is `job-skipped`; `batch-done` closes the stream.
    #[test]
    fn batch_events_carry_their_place() {
        let mut r = JsonReporter::new(Vec::new(), "documents");
        r.begin_job("documents", 1, 2);
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
        r.note("Nothing to copy.");
        let ok = DestinationResult {
            destination: "d-91c0".into(),
            outcome: "ok".into(),
            reason: None,
        };
        r.done(0, std::slice::from_ref(&ok));
        r.begin_job("records", 2, 2);
        r.job_skipped("locked", "another run in progress");
        r.batch_done(
            1,
            &[
                JobResult {
                    job: "documents".into(),
                    outcome: "ok".into(),
                    reason: None,
                    destinations: vec![ok],
                },
                JobResult {
                    job: "records".into(),
                    outcome: "skipped".into(),
                    reason: Some("another run in progress".into()),
                    destinations: vec![DestinationResult {
                        destination: "d-0002".into(),
                        outcome: "skipped".into(),
                        reason: Some("another run in progress".into()),
                    }],
                },
            ],
        );
        let text = String::from_utf8(r.into_inner()).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 5, "{text}");
        for l in &lines[..3] {
            assert_eq!(l["batch"], json!({"index": 1, "of": 2}), "{l}");
            assert_eq!(l["job"], "documents");
        }
        assert_eq!(lines[2]["event"], "done");
        assert_eq!(
            lines[3],
            json!({"event": "job-skipped", "job": "records", "code": "locked",
                   "reason": "another run in progress", "batch": {"index": 2, "of": 2}})
        );
        assert_eq!(
            lines[4],
            json!({"event": "batch-done", "ok": false, "exit_code": 1, "jobs": [
                {"job": "documents", "outcome": "ok", "reason": null,
                 "destinations": [{"destination": "d-91c0", "outcome": "ok", "reason": null}]},
                {"job": "records", "outcome": "skipped", "reason": "another run in progress",
                 "destinations": [{"destination": "d-0002", "outcome": "skipped",
                                   "reason": "another run in progress"}]},
            ]})
        );
        // A single job has no batch field.
        let mut r = JsonReporter::new(Vec::new(), "documents");
        r.done(0, &[]);
        let text = String::from_utf8(r.into_inner()).unwrap();
        assert!(!text.contains("batch"), "{text}");
    }
}
