//! `state.json`: what happened on each run, folders on hold, and bisync
//! bookkeeping. Machine-written and kept apart from the user's config, so a
//! run never rewrites a file the user edits by hand.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Paths;
use crate::model::RunRecord;

pub mod lock;
pub mod prune;

/// The state schema version this binary reads and writes.
pub const STATE_VERSION: u32 = 1;

/// Bisync bookkeeping for one two-way destination.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BisyncState {
    #[serde(default)]
    pub initialised: bool,
    /// RFC 3339 timestamp of the last `--resync`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_resync: Option<String>,
    /// The pair these listings belong to: the source path and the
    /// destination as shown (`work:Backups/Documents` or a local path).
    /// When either no longer matches the job, the listings describe another
    /// pair and the next two-way run must start with a merge (`--resync`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    /// Set when a real run stopped on a bisync critical error: what
    /// happened, in words. The next run starts with a resync and says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_resync: Option<String>,
}

impl BisyncState {
    /// True when the recorded pair is this one (or none was recorded).
    pub fn matches(&self, source: &str, destination: &str) -> bool {
        self.source.as_deref().is_none_or(|s| s == source)
            && self.destination.as_deref().is_none_or(|d| d == destination)
    }
}

/// The whole state file. Every map is keyed by `<job>/<destination-id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// The last real run of each destination.
    #[serde(default)]
    pub runs: BTreeMap<String, RunRecord>,
    /// The last `--dry-run` of each destination, kept apart so a dry run
    /// never hides the last real run (added in milestone 6; written only
    /// once there is one).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_dry_run: BTreeMap<String, RunRecord>,
    /// Folders kept on the destination but no longer synced.
    #[serde(default)]
    pub holds: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub bisync: BTreeMap<String, BisyncState>,
}

impl Default for State {
    fn default() -> Self {
        State {
            version: STATE_VERSION,
            runs: BTreeMap::new(),
            last_dry_run: BTreeMap::new(),
            holds: BTreeMap::new(),
            bisync: BTreeMap::new(),
        }
    }
}

impl State {
    /// The key used in every map: `<job>/<destination-id>`.
    pub fn key(job: &str, destination_id: &str) -> String {
        format!("{job}/{destination_id}")
    }

    /// Loads the state file. A missing file is an empty state.
    pub fn load(path: &Path) -> Result<State> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        let state: State = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a valid state file", path.display()))?;
        if state.version > STATE_VERSION {
            bail!(
                "{} was written by a newer OneFolder (state version {}). Update OneFolder, then run it again.",
                path.display(),
                state.version
            );
        }
        Ok(state)
    }

    /// Writes the state atomically: a temp file in the same directory, then a rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut text = serde_json::to_string_pretty(self).context("could not serialise state")?;
        text.push('\n');
        write_atomic(path, text.as_bytes())
            .with_context(|| format!("could not save {}", path.display()))
    }
}

/// Holds `<data>/state.lock` exclusively (`flock`) while alive: every
/// writer of `state.json` takes it across load, change and save, so two
/// OneFolder processes (runs of different jobs, a run and `jobs edit`)
/// never lose each other's changes. Released when dropped, and by the
/// system if the process dies.
#[derive(Debug)]
pub struct StateLock {
    _file: File,
}

impl State {
    /// Takes the state lock, waiting for any other writer to finish.
    pub fn lock(paths: &Paths) -> Result<StateLock> {
        fs::create_dir_all(&paths.data_dir)
            .with_context(|| format!("could not create {}", paths.data_dir.display()))?;
        let path = paths.data_dir.join("state.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("could not lock {}", path.display()))?;
        Ok(StateLock { _file: file })
    }

    /// Loads the state, lets `change` modify it, and saves it if it
    /// changed, all under the state lock. A state file that does not load
    /// is left alone and is the error.
    pub fn update<R>(paths: &Paths, change: impl FnOnce(&mut State) -> R) -> Result<R> {
        let _lock = State::lock(paths)?;
        let file = paths.state_file();
        let mut state = State::load(&file)?;
        let before = state.clone();
        let result = change(&mut state);
        if state != before {
            state.save(&file)?;
        }
        Ok(result)
    }
}

impl State {
    /// Records a finished run: a real run in `runs`, a dry run in
    /// `last_dry_run`.
    pub fn record_run(&mut self, job: &str, destination_id: &str, record: RunRecord) {
        let key = State::key(job, destination_id);
        if record.dry_run {
            self.last_dry_run.insert(key, record);
        } else {
            self.runs.insert(key, record);
        }
    }

    /// The last real run of one destination. (Records written before
    /// milestone 6 may hold a dry run in `runs`; that is not a real run.)
    pub fn last_run(&self, job: &str, destination_id: &str) -> Option<&RunRecord> {
        self.runs
            .get(&State::key(job, destination_id))
            .filter(|r| !r.dry_run)
    }

    /// The last dry run of one destination. Records written before
    /// milestone 6 kept a dry run in `runs`; such a record counts here too.
    pub fn last_dry_run(&self, job: &str, destination_id: &str) -> Option<&RunRecord> {
        let key = State::key(job, destination_id);
        self.last_dry_run
            .get(&key)
            .or_else(|| self.runs.get(&key).filter(|r| r.dry_run))
    }

    /// Drops every record of one destination (runs, dry runs, holds,
    /// bisync). Returns how many entries went.
    pub fn drop_destination(&mut self, job: &str, destination_id: &str) -> usize {
        let key = State::key(job, destination_id);
        usize::from(self.runs.remove(&key).is_some())
            + usize::from(self.last_dry_run.remove(&key).is_some())
            + usize::from(self.holds.remove(&key).is_some())
            + usize::from(self.bisync.remove(&key).is_some())
    }

    /// The folders on hold for one destination.
    pub fn holds_of(&self, job: &str, destination_id: &str) -> Vec<String> {
        self.holds
            .get(&State::key(job, destination_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Puts a folder on hold for one destination. False when it already was.
    pub fn add_hold(&mut self, job: &str, destination_id: &str, folder: &str) -> bool {
        let folder = clean_folder(folder);
        let list = self
            .holds
            .entry(State::key(job, destination_id))
            .or_default();
        if list.iter().any(|f| clean_folder(f) == folder) {
            return false;
        }
        list.push(folder);
        list.sort();
        true
    }

    /// Releases a held folder on every destination of a job that holds it.
    /// Returns the destination ids it was released from.
    pub fn release(&mut self, job: &str, folder: &str) -> Vec<String> {
        let folder = clean_folder(folder);
        let prefix = format!("{job}/");
        let mut released = Vec::new();
        for (key, list) in self.holds.iter_mut() {
            let Some(id) = key.strip_prefix(&prefix) else {
                continue;
            };
            let before = list.len();
            list.retain(|f| clean_folder(f) != folder);
            if list.len() != before {
                released.push(id.to_string());
            }
        }
        self.holds.retain(|_, list| !list.is_empty());
        released
    }
}

/// A held folder as stored: relative, without leading or trailing
/// slashes. Spaces are kept: they can belong to a folder's name.
pub fn clean_folder(folder: &str) -> String {
    folder.trim_matches('/').to_string()
}

impl State {
    /// Moves every `<old>/<id>` key in `runs`, `holds` and `bisync` to
    /// `<new>/<id>`, and rewrites each run record's log path from
    /// `logs/<old>/` to `logs/<new>/`. An entry already under the new name
    /// (left by a removed job of that name) is replaced, since the renamed
    /// job is the live one. Returns how many entries moved.
    pub fn rename_job(&mut self, old: &str, new: &str) -> usize {
        let old_logs = format!("logs/{old}/");
        for (key, run) in self.runs.iter_mut().chain(self.last_dry_run.iter_mut()) {
            if key.starts_with(&format!("{old}/")) {
                if let Some(rest) = run.log.strip_prefix(&old_logs) {
                    run.log = format!("logs/{new}/{rest}");
                }
            }
        }
        fn move_keys<V>(map: &mut BTreeMap<String, V>, old: &str, new: &str) -> usize {
            let prefix = format!("{old}/");
            let keys: Vec<String> = map
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect();
            for key in &keys {
                if let Some(value) = map.remove(key) {
                    map.insert(format!("{new}/{}", &key[prefix.len()..]), value);
                }
            }
            keys.len()
        }
        move_keys(&mut self.runs, old, new)
            + move_keys(&mut self.last_dry_run, old, new)
            + move_keys(&mut self.holds, old, new)
            + move_keys(&mut self.bisync, old, new)
    }

    /// Drops every entry of a job (a removed job). Returns how many went.
    pub fn drop_job(&mut self, job: &str) -> usize {
        let prefix = format!("{job}/");
        let count =
            |s: &State| s.runs.len() + s.last_dry_run.len() + s.holds.len() + s.bisync.len();
        let before = count(self);
        self.runs.retain(|k, _| !k.starts_with(&prefix));
        self.last_dry_run.retain(|k, _| !k.starts_with(&prefix));
        self.holds.retain(|k, _| !k.starts_with(&prefix));
        self.bisync.retain(|k, _| !k.starts_with(&prefix));
        before - count(self)
    }
}

/// A timestamp for names of folders moved aside: `20261013T094102`.
pub fn stamp() -> String {
    jiff::Zoned::now().strftime("%Y%m%dT%H%M%S").to_string()
}

/// Moves `dir` aside to `<dir>.<label>-<stamp>` (with a counter if that is
/// taken), so nothing is ever deleted. `None` when `dir` does not exist.
pub fn move_aside(dir: &Path, label: &str) -> io::Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = format!("{name}.{label}-{}", stamp());
    let mut target = dir.with_file_name(&base);
    let mut n = 2;
    while target.exists() {
        target = dir.with_file_name(format!("{base}-{n}"));
        n += 1;
    }
    fs::rename(dir, &target)?;
    Ok(Some(target))
}

/// What moving data directories did: plain notes for the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Moves {
    pub done: Vec<String>,
    /// Each thing that did not move, and why.
    pub failed: Vec<String>,
}

/// Moves a renamed job's run logs and bisync working directories
/// (`logs/<old>` and `bisync/<old>` under the data directory) to the new
/// name. Logs and bisync state are keyed by job name and destination id,
/// so this is what lets them survive a rename. `state.json` is the
/// caller's (see [`State::rename_job`]).
///
/// When `<new>` already exists (a removed job of that name left its data),
/// the destination folders are moved in one by one. A stale bisync folder
/// in the way is moved aside to `<id>.removed-<stamp>` so the live job's
/// listings win, as its state entries do; log files are merged, and a log
/// file whose name is taken stays where it is and is reported.
pub fn move_job_dirs(paths: &Paths, old: &str, new: &str) -> Moves {
    let mut moves = Moves::default();
    for (area, dir) in [("logs", paths.logs_dir()), ("bisync", paths.bisync_dir())] {
        let from = dir.join(old);
        let to = dir.join(new);
        if !from.is_dir() {
            continue;
        }
        if !to.exists() {
            match fs::rename(&from, &to) {
                Ok(()) => moves
                    .done
                    .push(format!("moved {area}/{old} to {area}/{new}")),
                Err(e) => moves
                    .failed
                    .push(format!("{area}/{old} did not move to {area}/{new}: {e}")),
            }
            continue;
        }
        let entries = match fs::read_dir(&from) {
            Ok(entries) => entries,
            Err(e) => {
                moves
                    .failed
                    .push(format!("could not read {area}/{old}: {e}"));
                continue;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let shown = name.to_string_lossy().into_owned();
            let target = to.join(&name);
            if target.exists() && area == "bisync" {
                match move_aside(&target, "removed") {
                    Ok(Some(aside)) => moves.done.push(format!(
                        "moved the stale bisync/{new}/{shown} aside to {}",
                        aside.display()
                    )),
                    Ok(None) => {}
                    Err(e) => {
                        moves.failed.push(format!(
                            "bisync/{old}/{shown} did not move: bisync/{new}/{shown} is in the way and could not be moved aside: {e}"
                        ));
                        continue;
                    }
                }
            }
            if target.exists() {
                merge_logs(
                    &entry.path(),
                    &target,
                    &format!("{area}/{old}/{shown}"),
                    &mut moves,
                );
                continue;
            }
            if let Err(e) = fs::rename(entry.path(), &target) {
                moves.failed.push(format!(
                    "{area}/{old}/{shown} did not move to {area}/{new}/{shown}: {e}"
                ));
            }
        }
        if fs::remove_dir(&from).is_ok() {
            moves
                .done
                .push(format!("moved {area}/{old} into {area}/{new}"));
        } else {
            moves
                .failed
                .push(format!("{area}/{old} still holds files that did not move"));
        }
    }
    moves
}

/// Moves the files of one log folder into another, leaving any whose name
/// is taken.
fn merge_logs(from: &Path, to: &Path, shown: &str, moves: &mut Moves) {
    let Ok(entries) = fs::read_dir(from) else {
        moves.failed.push(format!("could not read {shown}"));
        return;
    };
    for entry in entries.flatten() {
        let target = to.join(entry.file_name());
        if target.exists() || fs::rename(entry.path(), &target).is_err() {
            moves.failed.push(format!(
                "{shown}/{} did not move: {} is taken",
                entry.file_name().to_string_lossy(),
                target.display()
            ));
        }
    }
    let _ = fs::remove_dir(from);
}

/// Writes `bytes` to `path` atomically: the data goes to a temp file in the
/// same directory, is flushed to disk, then renamed over the target, and the
/// directory is flushed so the rename survives a crash. Readers see either
/// the old file or the new one, never half of each.
///
/// - A symlinked target (a `config.toml` kept in a dotfiles repo, say) is
///   resolved first, so the link stays and the file it points to is replaced.
/// - The original file's permissions are kept.
/// - Temp names carry the process id and a counter, so concurrent writers
///   never share a temp file.
/// - The parent directory is created when needed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let target = match fs::canonicalize(path) {
        Ok(real) => real,
        Err(e) if e.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e),
    };
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&dir)?;
    let original_permissions = fs::metadata(&target).ok().map(|m| m.permissions());

    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = dir.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        if let Some(permissions) = &original_permissions {
            file.set_permissions(permissions.clone())?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &target)?;
        sync_dir(&dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Flushes a directory entry change (the rename) to disk.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mode, Outcome, SideCounts};

    const DESIGN_SAMPLE: &str = r#"{
      "version": 1,
      "runs": {
        "documents/d-7f3a": {
          "started": "2026-10-13T09:41:02+11:00",
          "finished": "2026-10-13T09:43:40+11:00",
          "mode": "two-way", "dry_run": false,
          "outcome": "ok",
          "added": {"local": 2, "remote": 14}, "updated": {"local": 0, "remote": 3},
          "deleted": {"local": 1, "remote": 0},
          "bytes": 184329113, "errors": 0,
          "log": "logs/documents/d-7f3a/2026-10-13T094102.jsonl",
          "rclone_version": "1.75.2"
        }
      },
      "holds": {
        "documents/d-7f3a": ["Old projects/2019"]
      },
      "bisync": {
        "documents/d-7f3a": {"initialised": true, "last_resync": "2026-09-01T10:00:00+11:00"}
      }
    }"#;

    #[test]
    fn missing_file_is_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::load(&dir.path().join("state.json")).unwrap();
        assert_eq!(state, State::default());
        assert_eq!(state.version, 1);
    }

    #[test]
    fn design_sample_loads_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, DESIGN_SAMPLE).unwrap();

        let state = State::load(&path).unwrap();
        let key = State::key("documents", "d-7f3a");
        let run = &state.runs[&key];
        assert_eq!(run.mode, Mode::TwoWay);
        assert_eq!(run.outcome, Outcome::Ok);
        assert_eq!(
            run.added,
            SideCounts {
                local: 2,
                remote: 14
            }
        );
        assert_eq!(state.holds[&key], vec!["Old projects/2019".to_string()]);
        assert!(state.bisync[&key].initialised);

        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), state);
        // No temp files left behind.
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("state.json")]);
    }

    #[test]
    fn save_creates_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/state.json");
        State::default().save(&path).unwrap();
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_stays_a_symlink_and_keeps_its_mode() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/config.toml");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("config.toml");
        symlink(&real, &link).unwrap();

        write_atomic(&link, b"new").unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        let mode = fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn concurrent_writes_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::thread::scope(|s| {
            for i in 0..8 {
                let path = &path;
                s.spawn(move || write_atomic(path, format!("{i}").as_bytes()).unwrap());
            }
        });
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "temp files left behind");
    }

    #[test]
    fn newer_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, r#"{"version": 2}"#).unwrap();
        let err = State::load(&path).unwrap_err().to_string();
        assert!(err.contains("newer OneFolder"), "{err}");
    }

    fn paths(data: &Path) -> Paths {
        Paths {
            config_dir: data.join("config"),
            data_dir: data.to_path_buf(),
        }
    }

    fn sample_record() -> RunRecord {
        let state: State = serde_json::from_str(DESIGN_SAMPLE).unwrap();
        state.runs["documents/d-7f3a"].clone()
    }

    #[test]
    fn rename_moves_every_key_of_the_job_only() {
        let mut state: State = serde_json::from_str(DESIGN_SAMPLE).unwrap();
        state
            .runs
            .insert("documents-old/d-0001".into(), sample_record());
        state.runs.insert("docs/d-9999".into(), sample_record());
        let moved = state.rename_job("documents", "docs");
        assert_eq!(moved, 3);
        assert!(state.runs.contains_key("docs/d-7f3a"));
        assert!(
            state.runs.contains_key("docs/d-9999"),
            "others are untouched"
        );
        assert!(
            state.runs.contains_key("documents-old/d-0001"),
            "prefix match is on the whole name"
        );
        assert_eq!(
            state.holds["docs/d-7f3a"],
            vec!["Old projects/2019".to_string()]
        );
        assert!(state.bisync["docs/d-7f3a"].initialised);
        assert!(!state.holds.contains_key("documents/d-7f3a"));
        // The log path follows the folder.
        assert_eq!(
            state.runs["docs/d-7f3a"].log,
            "logs/docs/d-7f3a/2026-10-13T094102.jsonl"
        );
        assert_eq!(
            state.runs["documents-old/d-0001"].log, "logs/documents/d-7f3a/2026-10-13T094102.jsonl",
            "another job's record is not touched"
        );
    }

    #[test]
    fn drop_job_removes_only_that_job() {
        let mut state: State = serde_json::from_str(DESIGN_SAMPLE).unwrap();
        state.runs.insert("docs/d-0001".into(), sample_record());
        assert_eq!(state.drop_job("documents"), 3);
        assert!(state.runs.contains_key("docs/d-0001"));
        assert!(state.holds.is_empty() && state.bisync.is_empty());
    }

    #[test]
    fn bisync_pairs() {
        let b = BisyncState {
            initialised: true,
            last_resync: None,
            source: Some("/a".into()),
            destination: Some("work:A".into()),
            needs_resync: None,
        };
        assert!(b.matches("/a", "work:A"));
        assert!(!b.matches("/a", "work:B"));
        assert!(!b.matches("/b", "work:A"));
        assert!(
            BisyncState::default().matches("/x", "y"),
            "nothing recorded yet"
        );
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["destination"], "work:A");
    }

    #[test]
    fn holds_are_added_once_and_released_everywhere() {
        let mut state = State::default();
        assert!(state.add_hold("docs", "d-0001", "/Old projects/2019/"));
        assert!(!state.add_hold("docs", "d-0001", "Old projects/2019"));
        assert!(state.add_hold("docs", "d-0002", "Old projects/2019"));
        assert!(state.add_hold("docs", "d-0002", "Archive"));
        assert!(state.add_hold("docsx", "d-0001", "Old projects/2019"));
        assert_eq!(
            state.holds_of("docs", "d-0002"),
            ["Archive", "Old projects/2019"]
        );
        assert_eq!(
            state.release("docs", "Old projects/2019/"),
            ["d-0001", "d-0002"]
        );
        assert!(!state.holds.contains_key("docs/d-0001"), "empty lists go");
        assert_eq!(state.holds_of("docs", "d-0002"), ["Archive"]);
        assert_eq!(state.holds_of("docsx", "d-0001"), ["Old projects/2019"]);
        assert!(state.release("docs", "nothing").is_empty());
        // Spaces belong to the name.
        assert!(state.add_hold("docs", "d-0003", "Old "));
        assert!(state.add_hold("docs", "d-0003", "Old"));
        assert_eq!(state.holds_of("docs", "d-0003"), ["Old", "Old "]);
        assert_eq!(state.release("docs", "Old "), ["d-0003"]);
        assert_eq!(state.holds_of("docs", "d-0003"), ["Old"]);
    }

    #[test]
    fn rename_moves_logs_and_bisync() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        fs::create_dir_all(data.join("logs/documents/d-7f3a")).unwrap();
        fs::write(data.join("logs/documents/d-7f3a/run.jsonl"), "{}").unwrap();
        fs::create_dir_all(data.join("bisync/documents/d-7f3a")).unwrap();
        fs::write(data.join("bisync/documents/d-7f3a/x.lst"), "").unwrap();
        // Another job's data stays put.
        fs::create_dir_all(data.join("logs/photos/d-0001")).unwrap();

        let moves = move_job_dirs(&paths(data), "documents", "docs");
        assert_eq!(moves.done.len(), 2, "{moves:?}");
        assert!(moves.failed.is_empty(), "{moves:?}");
        assert!(data.join("logs/docs/d-7f3a/run.jsonl").exists());
        assert!(data.join("bisync/docs/d-7f3a/x.lst").exists());
        assert!(!data.join("logs/documents").exists());
        assert!(!data.join("bisync/documents").exists());
        assert!(data.join("logs/photos/d-0001").exists());
    }

    #[test]
    fn rename_into_a_removed_jobs_leftovers_lets_the_live_job_win() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        fs::create_dir_all(data.join("logs/old/d-0001")).unwrap();
        fs::write(data.join("logs/old/d-0001/a.jsonl"), "live").unwrap();
        fs::create_dir_all(data.join("bisync/old/d-0002")).unwrap();
        fs::write(data.join("bisync/old/d-0002/live.lst"), "").unwrap();
        // A removed job called "new" left data, with clashing ids.
        fs::create_dir_all(data.join("logs/new/d-0001")).unwrap();
        fs::write(data.join("logs/new/d-0001/b.jsonl"), "stale").unwrap();
        fs::write(data.join("logs/new/d-0001/a.jsonl"), "stale").unwrap();
        fs::create_dir_all(data.join("bisync/new/d-0002")).unwrap();
        fs::write(data.join("bisync/new/d-0002/stale.lst"), "").unwrap();

        let moves = move_job_dirs(&paths(data), "old", "new");
        // Bisync: the live listings are in place, the stale ones moved aside.
        assert!(data.join("bisync/new/d-0002/live.lst").exists());
        let aside: Vec<_> = fs::read_dir(data.join("bisync/new"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("d-0002.removed-"))
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(data
            .join("bisync/new")
            .join(&aside[0])
            .join("stale.lst")
            .exists());
        assert!(!data.join("bisync/old").exists());
        // Logs are merged; the clashing file is reported and left alone.
        assert_eq!(
            fs::read_to_string(data.join("logs/new/d-0001/b.jsonl")).unwrap(),
            "stale"
        );
        assert!(data.join("logs/old/d-0001/a.jsonl").exists());
        assert!(
            moves.failed.iter().any(|f| f.contains("a.jsonl")),
            "{moves:?}"
        );
    }

    #[test]
    fn move_aside_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            move_aside(&dir.path().join("none"), "removed").unwrap(),
            None
        );
        let a = dir.path().join("d-0001");
        fs::create_dir_all(&a).unwrap();
        let first = move_aside(&a, "replaced").unwrap().unwrap();
        fs::create_dir_all(&a).unwrap();
        let second = move_aside(&a, "replaced").unwrap().unwrap();
        assert_ne!(first, second);
        assert!(first.exists() && second.exists() && !a.exists());
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("d-0001.replaced-"));
    }

    #[test]
    fn rename_without_any_data_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            move_job_dirs(&paths(dir.path()), "a", "b"),
            Moves::default()
        );
    }

    #[test]
    fn a_dry_run_never_replaces_the_last_real_run() {
        let mut state = State::default();
        let real = sample_record();
        state.record_run("documents", "d-7f3a", real.clone());
        let dry = RunRecord {
            dry_run: true,
            started: "2026-10-14T08:00:00+11:00".into(),
            finished: "2026-10-14T08:00:05+11:00".into(),
            outcome: Outcome::Failed,
            reason: Some("Backup drive not mounted".into()),
            ..real.clone()
        };
        state.record_run("documents", "d-7f3a", dry.clone());
        assert_eq!(state.last_run("documents", "d-7f3a"), Some(&real));
        assert_eq!(state.last_dry_run("documents", "d-7f3a"), Some(&dry));
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["runs"]["documents/d-7f3a"]["dry_run"], false);
        assert_eq!(json["last_dry_run"]["documents/d-7f3a"]["dry_run"], true);
        assert_eq!(
            json["last_dry_run"]["documents/d-7f3a"]["reason"],
            "Backup drive not mounted"
        );
        // A later real run replaces the real one and leaves the dry run.
        let later = RunRecord {
            started: "2026-10-15T08:00:00+11:00".into(),
            ..real.clone()
        };
        state.record_run("documents", "d-7f3a", later.clone());
        assert_eq!(state.last_run("documents", "d-7f3a"), Some(&later));
        assert_eq!(state.last_dry_run("documents", "d-7f3a"), Some(&dry));
        // Renames and removals carry the dry run too.
        assert_eq!(state.rename_job("documents", "docs"), 2);
        assert!(state.last_dry_run("docs", "d-7f3a").is_some());
        assert_eq!(
            state.last_dry_run["docs/d-7f3a"].log,
            "logs/docs/d-7f3a/2026-10-13T094102.jsonl"
        );
        assert_eq!(state.drop_destination("docs", "d-7f3a"), 2);
        assert!(state.runs.is_empty() && state.last_dry_run.is_empty());
    }

    #[test]
    fn an_old_dry_run_record_in_runs_still_reads_as_a_dry_run() {
        let mut state = State::default();
        let dry = RunRecord {
            dry_run: true,
            ..sample_record()
        };
        state.runs.insert("documents/d-7f3a".into(), dry.clone());
        assert_eq!(state.last_dry_run("documents", "d-7f3a"), Some(&dry));
        assert_eq!(state.last_run("documents", "d-7f3a"), None);
        // No `last_dry_run` key is written while there is none.
        let json = serde_json::to_value(State::default()).unwrap();
        assert!(json.get("last_dry_run").is_none(), "{json}");
    }

    /// Many writers at once, each in its own thread with its own lock file
    /// handle (as separate processes have): no record is lost.
    #[test]
    fn concurrent_updates_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let writers = 16;
        let rounds = 10;
        std::thread::scope(|s| {
            for w in 0..writers {
                let paths = &paths;
                s.spawn(move || {
                    for r in 0..rounds {
                        State::update(paths, |state| {
                            state.record_run(
                                &format!("job{w}"),
                                &format!("d-{r:04}"),
                                sample_record(),
                            );
                            state.add_hold(&format!("job{w}"), "d-0000", &format!("held {r}"));
                        })
                        .unwrap();
                    }
                });
            }
        });
        let state = State::load(&paths.state_file()).unwrap();
        assert_eq!(state.runs.len(), writers * rounds);
        for w in 0..writers {
            assert_eq!(state.holds_of(&format!("job{w}"), "d-0000").len(), rounds);
        }
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "{not json").unwrap();
        assert!(State::load(&path).is_err());
    }
}
