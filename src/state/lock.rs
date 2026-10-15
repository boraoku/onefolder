//! The per-job run lock: `runs/<job>.lock` under the data directory, so two
//! OneFolder runs of the same job never work on its destinations at once.
//!
//! The file is created with `O_EXCL` and holds the process id and when the
//! run started. It is removed when the [`RunLock`] is dropped, and by the
//! signal dispatcher when SIGINT, SIGTERM or SIGHUP end the process outright
//! (see `rclone::exec::remove_on_exit`). A lock whose process is gone, is no
//! longer OneFolder (the id was reused), or was taken before the computer last
//! started is stale and is removed with a note.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::Paths;
use crate::rclone::bisync;
use crate::rclone::exec;

/// What the lock file says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: i32,
    /// RFC 3339, when the run started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
}

/// A held run lock. Dropping it removes the file.
#[derive(Debug)]
pub struct RunLock {
    path: PathBuf,
}

impl RunLock {
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        // Only our own lock: a file someone replaced is left alone.
        let ours = read(&self.path).is_some_and(|i| i.pid == std::process::id() as i32);
        if ours {
            let _ = fs::remove_file(&self.path);
        }
        exec::forget_on_exit(&self.path);
    }
}

/// Why the lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another live OneFolder run of this job holds it.
    Busy { pid: i32, path: PathBuf },
    /// The lock file could not be created or read.
    Io { path: PathBuf, error: io::Error },
}

/// What taking the lock did, besides taking it: a stale lock that was
/// removed, worth a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub pid: i32,
    pub path: PathBuf,
    /// The process id is alive, but it is another program now (the id was
    /// reused) or it started before the computer did: no longer a OneFolder run.
    pub pid_reused: bool,
}

impl Removed {
    /// The note for the user.
    pub fn note(&self) -> String {
        let why = if self.pid_reused {
            "which is no longer a OneFolder run"
        } else {
            "which is no longer running"
        };
        format!(
            "Removed a stale run lock left by process {}, {why}",
            self.pid
        )
    }
}

/// The lock file of a job.
pub fn lock_path(paths: &Paths, job: &str) -> PathBuf {
    paths.runs_dir().join(format!("{job}.lock"))
}

fn read(path: &Path) -> Option<LockInfo> {
    let text = fs::read_to_string(path).ok()?;
    parse(&text)
}

/// Reads a lock file's text: the JSON this module writes, or a bare
/// process id.
pub fn parse(text: &str) -> Option<LockInfo> {
    let text = text.trim();
    serde_json::from_str::<LockInfo>(text)
        .ok()
        .or_else(|| {
            text.parse::<i32>()
                .ok()
                .map(|pid| LockInfo { pid, started: None })
        })
        .filter(|i| i.pid > 0)
}

/// Whether a lock still belongs to a running OneFolder. `alive` is whether
/// the process exists, `command` its name and `boot` the computer's start in
/// Unix seconds, each when known. A process id is reused after a restart,
/// so a lock taken before the start, or one whose process is not
/// OneFolder, is stale whatever `kill` says.
pub fn is_live(info: &LockInfo, alive: bool, command: Option<&str>, boot: Option<i64>) -> bool {
    if !alive {
        return false;
    }
    let started = info
        .started
        .as_deref()
        .and_then(|s| s.parse::<jiff::Timestamp>().ok())
        .map(|t| t.as_second());
    if let (Some(started), Some(boot)) = (started, boot) {
        if started < boot {
            return false;
        }
    }
    command.is_none_or(|c| c.to_ascii_lowercase().contains("onefolder"))
}

/// Whether the lock's process is a live OneFolder run, and whether its
/// process id is at least alive.
async fn live(info: &LockInfo) -> (bool, bool) {
    let alive = bisync::pid_alive(info.pid);
    let (command, boot) = if alive {
        (
            bisync::process_name(info.pid).await,
            bisync::boot_time().await,
        )
    } else {
        (None, None)
    };
    (is_live(info, alive, command.as_deref(), boot), alive)
}

/// A job's run lock as `status` and `doctor` report it, looked at
/// without being taken or removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Seen {
    /// `running` (a live OneFolder run holds it) or `stale` (the next run
    /// removes it).
    pub state: String,
    /// The process id in the lock; `None` when the file does not say.
    pub pid: Option<i32>,
    /// RFC 3339, when the run started, if recorded.
    pub started: Option<String>,
    pub path: String,
}

impl Seen {
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }

    /// `a run is in progress (process 123, started 13 Oct 2026 09:41)`, or
    /// the stale lock in words.
    pub fn words(&self, when: impl Fn(&str) -> String) -> String {
        let pid = self.pid.map_or_else(
            || "an unknown process".to_string(),
            |p| format!("process {p}"),
        );
        let started = self
            .started
            .as_deref()
            .map(|s| format!(", started {}", when(s)))
            .unwrap_or_default();
        if self.is_running() {
            format!("a run is in progress ({pid}{started})")
        } else {
            format!("stale run lock left by {pid}{started}; the next run removes it")
        }
    }
}

/// Looks at a job's run lock: `None` when there is none.
pub async fn inspect(paths: &Paths, job: &str) -> Option<Seen> {
    let path = lock_path(paths, job);
    let text = fs::read_to_string(&path).ok()?;
    let held = parse(&text);
    let running = match &held {
        Some(info) => live(info).await.0,
        None => false,
    };
    Some(Seen {
        state: if running { "running" } else { "stale" }.to_string(),
        pid: held.as_ref().map(|h| h.pid),
        started: held.and_then(|h| h.started),
        path: path.display().to_string(),
    })
}

/// Creates the lock file with its content in one step: the content goes
/// to a temp file, which is then hard-linked to the lock's name. A link
/// fails if the name exists, so the lock never exists half-written (an
/// empty lock file would read as stale and could be removed by another
/// starter).
fn create(path: &Path, info: &LockInfo) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        let text = serde_json::to_string(info).map_err(io::Error::other)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&tmp, path)
    })();
    let _ = fs::remove_file(&tmp);
    result
}

/// Takes the run lock of `job`. A stale lock is removed and reported in
/// the second value; a live one is [`LockError::Busy`].
pub async fn acquire(paths: &Paths, job: &str) -> Result<(RunLock, Option<Removed>), LockError> {
    let path = lock_path(paths, job);
    let io_error = |error| LockError::Io {
        path: path.clone(),
        error,
    };
    fs::create_dir_all(paths.runs_dir()).map_err(io_error)?;
    let info = LockInfo {
        pid: std::process::id() as i32,
        // Local time with its offset, as run records are written, so it
        // shows as the time the user saw.
        started: Some(
            jiff::Zoned::now()
                .strftime("%Y-%m-%dT%H:%M:%S%:z")
                .to_string(),
        ),
    };
    let mut removed = None;
    // Twice at most: once, and once more after removing a stale lock.
    for _ in 0..2 {
        match create(&path, &info) {
            Ok(()) => {
                exec::remove_on_exit(&path);
                return Ok((RunLock { path }, removed));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error(e)),
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            // Gone in between: its owner finished. Try again.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_error(e)),
        };
        // An unreadable lock (edited by hand, or a disk fault) holds no
        // process; the lock is never written partly (see `create`).
        let held = parse(&text);
        let mut pid_reused = false;
        if let Some(held) = &held {
            let (is_live, alive) = live(held).await;
            if is_live {
                return Err(LockError::Busy {
                    pid: held.pid,
                    path: path.clone(),
                });
            }
            pid_reused = alive;
        }
        // Remove it only if it is still the file just read, so a run that
        // took the lock in the meantime keeps it.
        if fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_error(e)),
            }
            removed = Some(Removed {
                pid: held.map_or(0, |h| h.pid),
                path: path.clone(),
                pid_reused,
            });
        }
    }
    // Someone else took it between the removal and the retry.
    match read(&path) {
        Some(held) => Err(LockError::Busy {
            pid: held.pid,
            path,
        }),
        None => Err(io_error(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the lock file appeared again while it was being taken",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
        }
    }

    #[test]
    fn lock_files_read_back() {
        let info = LockInfo {
            pid: 4242,
            started: Some("2026-10-13T00:00:00Z".into()),
        };
        let text = serde_json::to_string(&info).unwrap();
        assert_eq!(parse(&text), Some(info));
        assert_eq!(parse(" 77\n").map(|i| i.pid), Some(77));
        assert_eq!(parse("0"), None);
        assert_eq!(parse("-3"), None);
        assert_eq!(parse("garbage"), None);
    }

    #[test]
    fn liveness() {
        let info = LockInfo {
            pid: 10,
            started: Some("2026-10-13T00:00:00Z".into()),
        };
        let started = 1_791_849_600; // 2026-10-13T00:00:00Z
        assert!(!is_live(&info, false, Some("onefolder"), None));
        assert!(is_live(
            &info,
            true,
            Some("/opt/homebrew/bin/onefolder"),
            None
        ));
        assert!(is_live(&info, true, None, Some(started - 60)));
        assert!(
            !is_live(&info, true, Some("/usr/sbin/cron"), None),
            "the id was reused by another program"
        );
        assert!(
            !is_live(&info, true, Some("onefolder"), Some(started + 60)),
            "taken before the computer started"
        );
    }

    #[tokio::test]
    async fn a_live_lock_refuses_and_a_dropped_one_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let (lock, removed) = acquire(&paths, "documents").await.unwrap();
        assert_eq!(removed, None);
        let path = lock_path(&paths, "documents");
        assert_eq!(lock.path(), path);
        assert_eq!(read(&path).map(|i| i.pid), Some(std::process::id() as i32));
        // This test process is alive and its name contains "onefolder".
        match acquire(&paths, "documents").await {
            Err(LockError::Busy { pid, .. }) => assert_eq!(pid, std::process::id() as i32),
            other => panic!("expected busy, got {other:?}"),
        }
        // Another job has its own lock.
        let (other, _) = acquire(&paths, "photos").await.unwrap();
        drop(other);
        drop(lock);
        assert!(!path.exists());
        let (again, removed) = acquire(&paths, "documents").await.unwrap();
        assert_eq!(removed, None);
        drop(again);
    }

    #[tokio::test]
    async fn a_stale_lock_is_removed_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        fs::create_dir_all(paths.runs_dir()).unwrap();
        let path = lock_path(&paths, "documents");
        // A process id that does not exist (above the macOS maximum).
        fs::write(
            &path,
            r#"{"pid":99999999,"started":"2026-10-13T00:00:00Z"}"#,
        )
        .unwrap();
        let (lock, removed) = acquire(&paths, "documents").await.unwrap();
        assert_eq!(
            removed,
            Some(Removed {
                pid: 99_999_999,
                path: path.clone(),
                pid_reused: false,
            })
        );
        assert_eq!(read(&path).map(|i| i.pid), Some(std::process::id() as i32));
        drop(lock);
        // An unreadable lock holds no process either.
        fs::write(&path, "").unwrap();
        let (lock, removed) = acquire(&paths, "documents").await.unwrap();
        assert_eq!(removed.map(|r| r.pid), Some(0));
        drop(lock);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_lock_whose_id_is_another_program_is_stale_and_said_so() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        fs::create_dir_all(paths.runs_dir()).unwrap();
        let path = lock_path(&paths, "documents");
        // Process 1 (launchd) is alive and is not OneFolder.
        fs::write(&path, r#"{"pid":1,"started":"2026-10-13T00:00:00Z"}"#).unwrap();
        let (lock, removed) = acquire(&paths, "documents").await.unwrap();
        let removed = removed.unwrap();
        assert!(removed.pid_reused);
        assert_eq!(
            removed.note(),
            "Removed a stale run lock left by process 1, which is no longer a OneFolder run"
        );
        drop(lock);
    }

    #[test]
    fn the_lock_is_created_whole_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.lock");
        let info = LockInfo {
            pid: 42,
            started: None,
        };
        create(&path, &info).unwrap();
        assert_eq!(read(&path), Some(info.clone()));
        let err = create(&path, &info).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "no temp files left");
    }

    #[tokio::test]
    async fn status_and_doctor_see_a_live_or_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        assert_eq!(inspect(&paths, "documents").await, None);
        let (lock, _) = acquire(&paths, "documents").await.unwrap();
        let seen = inspect(&paths, "documents").await.unwrap();
        assert!(seen.is_running(), "{seen:?}");
        assert_eq!(seen.pid, Some(std::process::id() as i32));
        // Local time with its offset, which status shows as written.
        let started = seen.started.clone().unwrap();
        assert!(
            matches!(started.as_bytes().get(19), Some(b'+' | b'-')),
            "{started}"
        );
        assert!(started.parse::<jiff::Timestamp>().is_ok(), "{started}");
        assert!(seen
            .words(|s| s.to_string())
            .starts_with("a run is in progress (process "));
        drop(lock);
        fs::create_dir_all(paths.runs_dir()).unwrap();
        fs::write(
            lock_path(&paths, "documents"),
            r#"{"pid":99999999,"started":"2026-10-13T00:00:00Z"}"#,
        )
        .unwrap();
        let seen = inspect(&paths, "documents").await.unwrap();
        assert_eq!(seen.state, "stale");
        assert_eq!(
            seen.words(|_| "13 Oct".into()),
            "stale run lock left by process 99999999, started 13 Oct; the next run removes it"
        );
        assert!(lock_path(&paths, "documents").exists(), "only looked at");
    }

    #[test]
    fn a_replaced_lock_is_not_removed_by_the_old_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.lock");
        fs::write(&path, r#"{"pid":1}"#).unwrap();
        drop(RunLock { path: path.clone() });
        assert!(path.exists());
    }
}
