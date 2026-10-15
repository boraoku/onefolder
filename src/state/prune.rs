//! Run log pruning, only when asked: `onefolder doctor --prune-logs`.
//! Logs are never deleted automatically (DESIGN.md decision 2: logs are
//! always kept).
//!
//! Run logs live at `logs/<job>/<destination-id>/<timestamp>.jsonl` under
//! the data directory, with an `.excludes` file of the same name beside a
//! log when the run passed its excludes in a file. A log is a candidate
//! when it is older than [`MAX_AGE_DAYS`], or beyond the newest
//! [`KEEP_PER_DESTINATION`] of its destination. A candidate is still kept,
//! and the report says why, when:
//!
//! - it is the log of its destination's last run or last dry run (so
//!   `status` never points at a missing file);
//! - it records a conflict (bisync's conflict lines, or OneFolder's own
//!   rename of a conflict copy), the only record of where a losing copy
//!   went;
//! - its job or destination is no longer in the config: OneFolder cannot
//!   tell whether the user still wants it.
//!
//! An `.excludes` file whose log is gone is removed too. A log's time is
//! the timestamp in its name (the local time the run started), or the
//! file's modification time when the name has none.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{Config, Paths};
use crate::rclone::events::{self, Event};
use crate::state::State;

/// Logs older than this many days are candidates.
pub const MAX_AGE_DAYS: i64 = 90;
/// Logs beyond the newest this many of a destination are candidates.
pub const KEEP_PER_DESTINATION: usize = 50;

/// The jobs and destinations the config has, as `<job>` and
/// `<job>/<destination-id>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Known {
    pub jobs: HashSet<String>,
    pub destinations: HashSet<String>,
}

impl Known {
    pub fn from_config(config: &Config) -> Known {
        Known {
            jobs: config.jobs.iter().map(|j| j.name.clone()).collect(),
            destinations: config
                .jobs
                .iter()
                .flat_map(|j| {
                    j.destinations
                        .iter()
                        .map(move |d| State::key(&j.name, d.id()))
                })
                .collect(),
        }
    }
}

/// One run log found under `logs/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    pub path: PathBuf,
    /// `<job>/<destination-id>`, as the state file keys it.
    pub key: String,
    /// Unix seconds: from the name, else the modification time.
    pub time: Option<i64>,
    /// The log and its `.excludes` companion, if any.
    pub bytes: u64,
    /// The companion file written beside the log, when there is one.
    pub companion: Option<PathBuf>,
}

/// Why a file goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// Older than [`MAX_AGE_DAYS`].
    Old,
    /// Beyond the newest [`KEEP_PER_DESTINATION`] of its destination.
    TooMany,
    /// An `.excludes` file whose log is gone.
    OrphanExcludes,
}

impl Reason {
    pub fn words(self) -> String {
        match self {
            Reason::Old => format!("older than {MAX_AGE_DAYS} days"),
            Reason::TooMany => format!("beyond the newest {KEEP_PER_DESTINATION}"),
            Reason::OrphanExcludes => "excludes file whose log is gone".to_string(),
        }
    }
}

/// Why a log that is old enough, or one too many, stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kept {
    /// The log of the destination's last run or last dry run.
    LastRun,
    /// It records a conflict.
    Conflicts,
    /// Its job is no longer in the config.
    JobRemoved,
    /// Its destination is no longer in the job.
    DestinationRemoved,
}

impl Kept {
    pub fn words(self) -> &'static str {
        match self {
            Kept::LastRun => "kept, the last run's log",
            Kept::Conflicts => "kept, records a conflict",
            Kept::JobRemoved => "kept, job removed",
            Kept::DestinationRemoved => "kept, destination removed",
        }
    }
}

/// A file that goes (or would go, in a dry run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pruned {
    pub path: String,
    /// `<job>/<destination-id>`.
    pub destination: String,
    pub bytes: u64,
    pub reason: Reason,
    /// RFC 3339, when known.
    pub time: Option<String>,
}

/// A candidate that stays, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub path: String,
    pub destination: String,
    pub bytes: u64,
    /// Why it was a candidate.
    pub candidate: Reason,
    /// Why it stays.
    pub kept: Kept,
}

/// What a pruning pass did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub dry_run: bool,
    pub removed: Vec<Pruned>,
    pub removed_bytes: u64,
    /// Candidates kept, with the reason.
    pub skipped: Vec<Skipped>,
    /// Logs left in place, the skipped ones included.
    pub kept: u64,
    pub kept_bytes: u64,
    /// Files that could not be removed, with the reason.
    pub errors: Vec<String>,
}

/// What `doctor` says about the logs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// Run logs (`.jsonl` files).
    pub files: u64,
    /// Every file under `logs/`, companions included.
    pub bytes: u64,
    /// RFC 3339 times of the oldest and newest log, when known.
    pub oldest: Option<String>,
    pub newest: Option<String>,
    /// Files `doctor --prune-logs` would remove now.
    pub prunable: u64,
}

/// `2026-10-13T094102` or `2026-10-13T094102-2` (a second log started in
/// the same second) in the local time zone, as Unix seconds.
pub fn time_from_name(stem: &str) -> Option<i64> {
    let stamp = stem.get(..17)?;
    let rest = &stem[17..];
    if !(rest.is_empty() || rest.starts_with('-')) {
        return None;
    }
    let civil = jiff::civil::DateTime::strptime("%Y-%m-%dT%H%M%S", stamp).ok()?;
    let zoned = civil.to_zoned(jiff::tz::TimeZone::system()).ok()?;
    Some(zoned.timestamp().as_second())
}

fn modified(path: &Path) -> Option<i64> {
    let time = fs::metadata(path).ok()?.modified().ok()?;
    let secs = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(secs).ok()
}

fn rfc3339(seconds: i64) -> Option<String> {
    let ts = jiff::Timestamp::from_second(seconds).ok()?;
    Some(
        ts.to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%dT%H:%M:%S%:z")
            .to_string(),
    )
}

fn names_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect();
    out.sort();
    out
}

/// What is under `logs/`: every run log, the `.excludes` files with no log
/// (with their destination), and the bytes of everything.
struct Scan {
    logs: Vec<LogFile>,
    orphans: Vec<(PathBuf, String, u64)>,
    bytes: u64,
}

fn scan(paths: &Paths) -> Scan {
    let root = paths.logs_dir();
    let mut logs = Vec::new();
    let mut orphans = Vec::new();
    for (job, job_dir) in names_in(&root) {
        if !job_dir.is_dir() {
            continue;
        }
        for (dest, dest_dir) in names_in(&job_dir) {
            if !dest_dir.is_dir() {
                continue;
            }
            let key = format!("{job}/{dest}");
            for (name, path) in names_in(&dest_dir) {
                if let Some(stem) = name.strip_suffix(".excludes") {
                    if path.is_file() && !dest_dir.join(format!("{stem}.jsonl")).exists() {
                        let bytes = fs::metadata(&path).map_or(0, |m| m.len());
                        orphans.push((path, key.clone(), bytes));
                    }
                    continue;
                }
                let Some(stem) = name.strip_suffix(".jsonl") else {
                    continue;
                };
                if !path.is_file() {
                    continue;
                }
                let companion = dest_dir.join(format!("{stem}.excludes"));
                let companion = companion.is_file().then_some(companion);
                let bytes = fs::metadata(&path).map_or(0, |m| m.len())
                    + companion
                        .as_ref()
                        .and_then(|c| fs::metadata(c).ok())
                        .map_or(0, |m| m.len());
                logs.push(LogFile {
                    time: time_from_name(stem).or_else(|| modified(&path)),
                    path,
                    key: key.clone(),
                    bytes,
                    companion,
                });
            }
        }
    }
    Scan {
        logs,
        orphans,
        bytes: dir_bytes(&root),
    }
}

/// Total size of the files under `dir`; zero when it does not exist.
pub fn dir_bytes(dir: &Path) -> u64 {
    let mut bytes = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => bytes += entry.metadata().map_or(0, |m| m.len()),
                _ => {}
            }
        }
    }
    bytes
}

/// The logs the state file points at: each destination's last run and
/// last dry run. Paths are relative to the data directory, or absolute.
pub fn referenced(paths: &Paths, state: &State) -> HashSet<PathBuf> {
    state
        .runs
        .values()
        .chain(state.last_dry_run.values())
        .filter(|r| !r.log.is_empty())
        .map(|r| {
            let p = Path::new(&r.log);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                paths.data_dir.join(p)
            }
        })
        .collect()
}

/// True when a log records a conflict: one of bisync's conflict lines, or
/// OneFolder's own `moveto` of a destination copy to its conflict name.
/// A log that cannot be read counts as one, so it is kept.
pub fn records_conflict(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return true;
    };
    text.lines().any(|line| {
        if line.contains("\"onefolder\":\"command\"") {
            return serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v["command"].as_str().map(|c| c.contains(" moveto ")))
                .unwrap_or(false);
        }
        matches!(events::decode(line), Event::Conflict { .. })
    })
}

/// The candidates by the thresholds alone: older than [`MAX_AGE_DAYS`] or
/// beyond the newest [`KEEP_PER_DESTINATION`] of each destination. A log
/// with no known time is never too old.
pub fn candidates(logs: &[LogFile], now: i64) -> Vec<(&LogFile, Reason)> {
    let cutoff = now - MAX_AGE_DAYS * 86_400;
    let mut by_dest: BTreeMap<&str, Vec<&LogFile>> = BTreeMap::new();
    for log in logs {
        by_dest.entry(log.key.as_str()).or_default().push(log);
    }
    let mut out = Vec::new();
    for list in by_dest.values_mut() {
        // Newest first; the path breaks ties (`-2` sorts after its first).
        list.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| b.path.cmp(&a.path)));
        for (rank, log) in list.iter().enumerate() {
            if log.time.is_some_and(|t| t < cutoff) {
                out.push((*log, Reason::Old));
            } else if rank >= KEEP_PER_DESTINATION {
                out.push((*log, Reason::TooMany));
            }
        }
    }
    out.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    out
}

/// Why a candidate stays, if it does.
fn exception(log: &LogFile, keep: &HashSet<PathBuf>, known: &Known) -> Option<Kept> {
    let job = log.key.split('/').next().unwrap_or(&log.key);
    if !known.jobs.contains(job) {
        return Some(Kept::JobRemoved);
    }
    if !known.destinations.contains(&log.key) {
        return Some(Kept::DestinationRemoved);
    }
    if keep.contains(&log.path) {
        return Some(Kept::LastRun);
    }
    if records_conflict(&log.path) {
        return Some(Kept::Conflicts);
    }
    None
}

/// What `doctor` reports about the logs.
pub fn usage(paths: &Paths, state: &State, known: &Known, now: i64) -> Usage {
    let outcome = prune(paths, state, known, now, true);
    let s = scan(paths);
    let times = s.logs.iter().filter_map(|l| l.time);
    Usage {
        files: s.logs.len() as u64,
        bytes: s.bytes,
        oldest: times.clone().min().and_then(rfc3339),
        newest: times.max().and_then(rfc3339),
        prunable: outcome.removed.len() as u64,
    }
}

/// Removes the candidates that have no exception, and `.excludes` files
/// whose log is gone (in a configured destination); nothing in a dry run.
/// A file already gone is not an error.
pub fn prune(paths: &Paths, state: &State, known: &Known, now: i64, dry_run: bool) -> Outcome {
    let s = scan(paths);
    let keep = referenced(paths, state);
    let mut out = Outcome {
        dry_run,
        ..Outcome::default()
    };
    let mut going: Vec<(&LogFile, Reason)> = Vec::new();
    for (log, reason) in candidates(&s.logs, now) {
        match exception(log, &keep, known) {
            Some(kept) => out.skipped.push(Skipped {
                path: log.path.display().to_string(),
                destination: log.key.clone(),
                bytes: log.bytes,
                candidate: reason,
                kept,
            }),
            None => going.push((log, reason)),
        }
    }
    let gone: HashSet<&Path> = going.iter().map(|(l, _)| l.path.as_path()).collect();
    for log in &s.logs {
        if !gone.contains(log.path.as_path()) {
            out.kept += 1;
            out.kept_bytes += log.bytes;
        }
    }
    let remove = |files: Vec<&PathBuf>, out: &mut Outcome| -> bool {
        if dry_run {
            return true;
        }
        let mut ok = true;
        for file in files {
            match fs::remove_file(file) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    out.errors.push(format!("{}: {e}", file.display()));
                    ok = false;
                }
            }
        }
        ok
    };
    for (log, reason) in going {
        let files: Vec<&PathBuf> = std::iter::once(&log.path)
            .chain(log.companion.as_ref())
            .collect();
        if !remove(files, &mut out) {
            out.kept += 1;
            out.kept_bytes += log.bytes;
            continue;
        }
        out.removed_bytes += log.bytes;
        out.removed.push(Pruned {
            path: log.path.display().to_string(),
            destination: log.key.clone(),
            bytes: log.bytes,
            reason,
            time: log.time.and_then(rfc3339),
        });
    }
    for (path, key, bytes) in &s.orphans {
        if !known.destinations.contains(key) {
            continue;
        }
        if !remove(vec![path], &mut out) {
            continue;
        }
        out.removed_bytes += bytes;
        out.removed.push(Pruned {
            path: path.display().to_string(),
            destination: key.clone(),
            bytes: *bytes,
            reason: Reason::OrphanExcludes,
            time: modified(path).and_then(rfc3339),
        });
    }
    out.removed.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mode, Outcome as RunOutcome, RunRecord, SideCounts};

    fn paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
        }
    }

    /// A log named for `days` days before `now`, `n` minutes earlier.
    fn stamp(now: i64, days: i64, n: i64) -> String {
        let t = jiff::Timestamp::from_second(now - days * 86_400 - n * 60).unwrap();
        t.to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%dT%H%M%S")
            .to_string()
    }

    fn write_log(p: &Paths, key: &str, name: &str, text: &str) -> PathBuf {
        let dir = p.logs_dir().join(key);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.jsonl"));
        fs::write(&path, text).unwrap();
        path
    }

    fn record(log: &str, dry_run: bool) -> RunRecord {
        RunRecord {
            started: "2026-10-13T09:41:02+11:00".into(),
            finished: "2026-10-13T09:43:40+11:00".into(),
            mode: Mode::OneWay,
            dry_run,
            outcome: RunOutcome::Ok,
            added: SideCounts::default(),
            updated: SideCounts::default(),
            deleted: SideCounts::default(),
            bytes: 0,
            errors: 0,
            log: log.into(),
            rclone_version: "1.75.2".into(),
            reason: None,
        }
    }

    fn known(keys: &[&str]) -> Known {
        Known {
            jobs: keys
                .iter()
                .map(|k| k.split('/').next().unwrap().to_string())
                .collect(),
            destinations: keys.iter().map(|k| k.to_string()).collect(),
        }
    }

    const NOW: i64 = 1_791_852_062;
    const PLAIN: &str = "{\"level\":\"notice\",\"msg\":\"nothing\"}\n";

    #[test]
    fn names_give_the_time() {
        let name = stamp(NOW, 0, 0);
        assert_eq!(time_from_name(&name), Some(NOW));
        assert_eq!(time_from_name(&format!("{name}-2")), Some(NOW));
        assert_eq!(time_from_name(&format!("{name}x")), None);
        assert_eq!(time_from_name("a"), None);
        assert_eq!(time_from_name("2026-13-40T999999"), None);
    }

    #[test]
    fn old_logs_and_logs_beyond_fifty_go_with_the_exceptions_kept() {
        let root = tempfile::tempdir().unwrap();
        let p = paths(root.path());
        // documents/d-7f3a: 60 recent logs, one a minute apart.
        let mut recent = Vec::new();
        for n in 0..60 {
            recent.push(write_log(&p, "documents/d-7f3a", &stamp(NOW, 1, n), PLAIN));
        }
        // documents/d-91c0: an old log referenced by the last run, an old
        // one, one at 89 days, an old one with an excludes file, and an old
        // one that records a conflict.
        let old_kept = write_log(&p, "documents/d-91c0", &stamp(NOW, 200, 0), PLAIN);
        let old = write_log(&p, "documents/d-91c0", &stamp(NOW, 91, 0), PLAIN);
        let young = write_log(&p, "documents/d-91c0", &stamp(NOW, 89, 0), PLAIN);
        let with_excludes = write_log(&p, "documents/d-91c0", &stamp(NOW, 95, 0), PLAIN);
        fs::write(with_excludes.with_extension("excludes"), "a\nb\n").unwrap();
        let conflict = write_log(
            &p,
            "documents/d-91c0",
            &stamp(NOW, 120, 0),
            "{\"onefolder\":\"command\",\"command\":\"/opt/homebrew/bin/rclone moveto a b\"}\n",
        );
        // An excludes file whose log is gone goes too.
        let orphan = p
            .logs_dir()
            .join(format!("documents/d-91c0/{}.excludes", stamp(NOW, 2, 0)));
        fs::write(&orphan, "x\n").unwrap();
        // A file that is not a log is never touched.
        fs::write(p.logs_dir().join("documents/d-91c0/notes.txt"), "x").unwrap();
        // A removed job and a removed destination: kept.
        let gone_job = write_log(&p, "photos/d-0001", &stamp(NOW, 300, 0), PLAIN);
        let gone_dest = write_log(&p, "documents/d-dead", &stamp(NOW, 300, 0), PLAIN);
        let gone_orphan = p.logs_dir().join("photos/d-0001/x.excludes");
        fs::write(&gone_orphan, "x\n").unwrap();

        let mut state = State::default();
        let rel = old_kept
            .strip_prefix(&p.data_dir)
            .unwrap()
            .display()
            .to_string();
        state.record_run("documents", "d-91c0", record(&rel, false));
        // The oldest of the 60 is the last dry run: it stays too.
        let dry = recent[59].clone();
        state.record_run(
            "documents",
            "d-7f3a",
            record(&dry.display().to_string(), true),
        );
        let k = known(&["documents/d-7f3a", "documents/d-91c0"]);

        let usage = usage(&p, &state, &k, NOW);
        assert_eq!(usage.files, 67);
        assert_eq!(usage.prunable, 9 + 2 + 1);
        assert!(usage.oldest.is_some() && usage.newest.is_some());

        let dry_run = prune(&p, &state, &k, NOW, true);
        assert_eq!(dry_run.removed.len(), 12);
        assert!(old.exists(), "a dry run removes nothing");

        let done = prune(&p, &state, &k, NOW, false);
        assert_eq!(done.removed.len(), 12);
        assert!(done.errors.is_empty());
        // The newest 50 stay, the referenced 60th stays, 51st to 59th go.
        for (n, path) in recent.iter().enumerate() {
            let expect = n < KEEP_PER_DESTINATION || n == 59;
            assert_eq!(path.exists(), expect, "log {n}");
        }
        assert!(old_kept.exists(), "referenced by the last run");
        assert!(conflict.exists(), "records a conflict");
        assert!(!old.exists());
        assert!(young.exists());
        assert!(!with_excludes.exists());
        assert!(!with_excludes.with_extension("excludes").exists());
        assert!(!orphan.exists());
        assert!(p.logs_dir().join("documents/d-91c0/notes.txt").exists());
        assert!(gone_job.exists() && gone_dest.exists() && gone_orphan.exists());
        let why: Vec<(String, Kept)> = done
            .skipped
            .iter()
            .map(|s| (s.destination.clone(), s.kept))
            .collect();
        assert!(
            why.contains(&("documents/d-7f3a".into(), Kept::LastRun)),
            "{why:?}"
        );
        assert!(why.contains(&("documents/d-91c0".into(), Kept::LastRun)));
        assert!(why.contains(&("documents/d-91c0".into(), Kept::Conflicts)));
        assert!(why.contains(&("photos/d-0001".into(), Kept::JobRemoved)));
        assert!(why.contains(&("documents/d-dead".into(), Kept::DestinationRemoved)));
        assert_eq!(Kept::JobRemoved.words(), "kept, job removed");
        // Nothing left to do.
        assert!(prune(&p, &state, &k, NOW, false).removed.is_empty());
    }

    #[test]
    fn a_log_without_a_time_in_its_name_uses_its_modification_time() {
        let root = tempfile::tempdir().unwrap();
        let p = paths(root.path());
        let path = write_log(&p, "notes/d-0001", "manual", PLAIN);
        let old = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs((NOW - 100 * 86_400) as u64);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let out = prune(&p, &State::default(), &known(&["notes/d-0001"]), NOW, false);
        assert_eq!(out.removed.len(), 1);
        assert_eq!(out.removed[0].reason, Reason::Old);
        assert!(!path.exists());
    }

    #[test]
    fn conflicts_are_found_in_bisync_lines_and_renames() {
        let root = tempfile::tempdir().unwrap();
        let bisync = root.path().join("bisync.jsonl");
        fs::copy(
            crate::rclone::fixture_path("bisync-own-conflict.jsonl"),
            &bisync,
        )
        .unwrap();
        assert!(records_conflict(&bisync));
        let plain = root.path().join("copy.jsonl");
        fs::copy(crate::rclone::fixture_path("copy-real.jsonl"), &plain).unwrap();
        assert!(!records_conflict(&plain));
        assert!(records_conflict(&root.path().join("missing.jsonl")));
    }
}
