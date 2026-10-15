//! `onefolder status --json`: the last run of every destination, in one
//! envelope (see [`super::Envelope`]) whose `data` is a [`Report`]:
//!
//! ```text
//! {"state_error":null|"…/state.json is not a valid state file: …",
//!  "jobs":[{
//!   "name":"documents","source":"/Users/bora/Documents",
//!   "source_volume":null | { as "volume" below },
//!   "run_lock":null | {"state":"running|stale","pid":123|null,
//!                      "started":"2026-10-13T09:41:02+11:00"|null,"path":".../runs/documents.lock"},
//!   "destinations":[{
//!     "id":"d-7f3a","type":"onedrive|local","display":"work:Backups/Documents",
//!     "path":"Backups/Documents","mode":"one-way|two-way",
//!     "account":"work"|null,"remote":"onefolder-work"|null,
//!     "remote_exists":true|false|null,
//!     "volume":null | {"state":"mounted|not-mounted|wrong-volume|read-only|missing|
//!                       not-a-folder|probe-failed|unsupported|no-drive|folder-missing",
//!                      "severity":"ok|warning|problem|skipped","message":"...",
//!                      "mount_point":"/Volumes/Backup"|null,"elsewhere":"/Volumes/Backup 1"|null},
//!     "last_run":null | { the state.json run record: started, finished, mode, dry_run,
//!                         outcome, added, updated, deleted, bytes, errors, log,
//!                         rclone_version, reason (when set) },
//!     "last_dry_run":null | { the same, dry_run true },
//!     "dry_run_newer":false,
//!     "log":"/absolute/path/of/the/last/run.jsonl"|null,
//!     "holds":["Old projects/2019"],
//!     "pending":null | {"state":"needs-resync|recovering|first-run|pair-changed",
//!                       "detail":"..."|null}
//!   }]
//! }]}
//! ```
//!
//! `remote_exists` is whether the account's rclone remote is in rclone's
//! config (a local lookup, never a network call; `accounts test` checks the
//! connection), null for a local destination or when rclone is not
//! available. `volume` is set for every local destination: the volume
//! check from DESIGN.md for a recorded drive (the same states and words as
//! `doctor`), `no-drive` or `folder-missing` for a folder with no drive
//! recorded. `pending` says what the next two-way run will do first.
//! `ok` in the envelope is true when the config and the state file both
//! load; with a `state_error` it is false (the exit code is still 0).

use serde::Serialize;

use crate::model::{Mode, RunRecord};

/// What `status` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Why `state.json` could not be read (corrupt, or written by a newer
    /// OneFolder), when it could not: the runs, holds and pending states
    /// below are then unknown, not absent.
    pub state_error: Option<String>,
    pub jobs: Vec<JobStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub name: String,
    pub source: String,
    /// For a source on an external drive.
    pub source_volume: Option<VolumeStatus>,
    /// The job's run lock, when there is one: a run in progress, or a
    /// stale lock the next run removes.
    pub run_lock: Option<crate::state::lock::Seen>,
    pub destinations: Vec<DestinationStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeStatus {
    /// `mounted`, a volume check error code, `no-drive` or `folder-missing`.
    pub state: String,
    /// `ok`, `warning`, `problem` or `skipped`.
    pub severity: String,
    /// In doctor's words: `not mounted (expects volume "Backup")`.
    pub message: String,
    /// The recorded mount point, when a drive is recorded.
    pub mount_point: Option<String>,
    /// Where the recorded drive is mounted instead, when it is.
    pub elsewhere: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pending {
    /// `needs-resync`, `recovering`, `first-run` or `pair-changed`.
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DestinationStatus {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub display: String,
    pub path: String,
    pub mode: Mode,
    pub account: Option<String>,
    pub remote: Option<String>,
    pub remote_exists: Option<bool>,
    pub volume: Option<VolumeStatus>,
    pub last_run: Option<RunRecord>,
    pub last_dry_run: Option<RunRecord>,
    pub dry_run_newer: bool,
    /// Absolute path of the last real run's log.
    pub log: Option<String>,
    pub holds: Vec<String>,
    pub pending: Option<Pending>,
}
