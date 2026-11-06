//! Windows only, against a real rclone: Ctrl-C during a run stops rclone
//! cleanly. They run only when `ONEFOLDER_INTEGRATION=1` is set and rclone
//! is on PATH (the Windows smoke job in CI):
//!
//! ```powershell
//! $env:ONEFOLDER_INTEGRATION = 1; cargo test --test windows -- --nocapture
//! ```
//!
//! OneFolder is started in a console process group of its own and sent
//! Ctrl-Break, which it reads as Ctrl-C (a script cannot send Ctrl-C to
//! another group). It must then ask rclone, in its own group, to stop with
//! Ctrl-Break, and rclone must stop well before OneFolder's grace period
//! ends, after which it would be killed: 10 seconds for a one-way run, 45
//! for two-way. rclone is slowed with `RCLONE_BWLIMIT`, so the run is in
//! the middle of a transfer when the key is pressed.
#![cfg(windows)]

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;
use windows_sys::Win32::System::Console::{
    AllocConsole, GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT,
};

/// `CREATE_NEW_PROCESS_GROUP`.
const NEW_GROUP: u32 = 0x0000_0200;
/// The file still being copied when Ctrl-C comes: 32 MB at 256 KB/s.
const BIG: usize = 32 << 20;

fn enabled() -> bool {
    std::env::var_os("ONEFOLDER_INTEGRATION").is_some_and(|v| v == "1")
}

struct World {
    dir: TempDir,
}

impl World {
    fn new() -> World {
        let w = World {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(w.path("src")).unwrap();
        fs::create_dir_all(w.path("dst")).unwrap();
        fs::write(w.path("src/small.txt"), "small").unwrap();
        w
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn onefolder(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_onefolder"));
        cmd.env("ONEFOLDER_CONFIG_DIR", self.path("config"))
            .env("ONEFOLDER_DATA_DIR", self.path("data"))
            .env("NO_COLOR", "1")
            .env_remove("RCLONE_CONFIG")
            .env_remove("RCLONE_BWLIMIT")
            .stdin(Stdio::null());
        cmd
    }

    /// Adds the job `documents` with one local destination in `mode`.
    fn add_job(&self, mode: &str) {
        let out = self
            .onefolder()
            .args(["jobs", "add", "documents", "--source"])
            .arg(self.path("src"))
            .arg("--local")
            .arg(format!("{}:{mode}", self.path("dst").display()))
            .output()
            .unwrap();
        assert!(out.status.success(), "jobs add: {}", text(&out.stderr));
    }

    /// A run at full speed, which must succeed.
    fn run_ok(&self, what: &str) {
        let out = self
            .onefolder()
            .args(["run", "documents", "--yes"])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{what}:\n{}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
    }

    /// The one destination's last run, from the state file.
    fn last_run(&self) -> Value {
        let state: Value =
            serde_json::from_str(&fs::read_to_string(self.path("data/state.json")).unwrap())
                .unwrap();
        state["runs"]
            .as_object()
            .and_then(|runs| runs.values().next().cloned())
            .expect("a run recorded")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// True once rclone has started writing `big.bin` (as a partial file).
fn big_started(dst: &Path) -> bool {
    fs::read_dir(dst).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("big.bin"))
    })
}

/// Starts a slow run of `documents` in a process group of its own, waits
/// until `big.bin` is arriving, presses Ctrl-Break and waits for the end.
/// Gives the exit code, how long the stop took, and what was printed.
fn interrupted_run(w: &World) -> (Option<i32>, Duration, String) {
    // SAFETY: plain call. It fails harmlessly when this process already
    // has a console; either way OneFolder, started next, shares it, which
    // Ctrl-Break needs.
    unsafe { AllocConsole() };
    fs::write(w.path("src/big.bin"), vec![7u8; BIG]).unwrap();
    let out = fs::File::create(w.path("out.txt")).unwrap();
    let mut run: Child = w
        .onefolder()
        .args(["run", "documents", "--yes"])
        .env("RCLONE_BWLIMIT", "256k")
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .creation_flags(NEW_GROUP)
        .spawn()
        .unwrap();
    let printed = || fs::read_to_string(w.path("out.txt")).unwrap_or_default();
    let start = Instant::now();
    while !big_started(&w.path("dst")) {
        if let Some(status) = run.try_wait().unwrap() {
            panic!("the run ended before Ctrl-C ({status}):\n{}", printed());
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = run.kill();
            panic!("big.bin never started arriving:\n{}", printed());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_secs(2));

    // SAFETY: plain call.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, run.id()) };
    assert_ne!(
        sent,
        0,
        "Ctrl-Break could not be sent: {}",
        std::io::Error::last_os_error()
    );
    let pressed = Instant::now();
    let code = loop {
        if let Some(status) = run.try_wait().unwrap() {
            break status.code();
        }
        if pressed.elapsed() > Duration::from_secs(120) {
            let _ = run.kill();
            panic!("OneFolder did not stop after Ctrl-C:\n{}", printed());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let took = pressed.elapsed();
    let printed = printed();
    println!("--- stopped after {took:?} with {code:?}:\n{printed}");
    (code, took, printed)
}

#[test]
fn ctrl_c_stops_a_one_way_copy_cleanly() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.add_job("one-way");
    w.run_ok("first run");

    let (code, took, printed) = interrupted_run(&w);
    assert_eq!(code, Some(1), "a cancelled run:\n{printed}");
    assert_eq!(w.last_run()["outcome"], "cancelled", "{printed}");
    assert!(
        took < Duration::from_secs(8),
        "rclone took {took:?} to stop, so it was killed when the 10 s grace \
         ended rather than stopped by Ctrl-Break:\n{printed}"
    );

    w.run_ok("the run after");
    assert_eq!(
        fs::metadata(w.path("dst/big.bin")).unwrap().len(),
        BIG as u64
    );
}

#[test]
fn ctrl_c_stops_a_two_way_sync_cleanly() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.add_job("two-way");
    w.run_ok("first run");

    let (code, took, printed) = interrupted_run(&w);
    assert_eq!(code, Some(1), "a cancelled run:\n{printed}");
    let record = w.last_run();
    assert_eq!(record["outcome"], "cancelled", "{printed}");
    assert!(
        took < Duration::from_secs(42),
        "bisync took {took:?} to stop, so it was killed when the 45 s grace \
         ended rather than stopped by Ctrl-Break:\n{printed}"
    );
    // A bisync that was killed leaves its lock, which OneFolder removes and
    // notes in the run's log; one that stopped cleanly removes it itself.
    let log = fs::read_to_string(w.path("data").join(record["log"].as_str().unwrap())).unwrap();
    assert!(
        !log.contains("lock-removed"),
        "bisync was killed, not stopped:\n{log}"
    );
    let locks: Vec<PathBuf> = walk(&w.path("data/bisync"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "lck"))
        .collect();
    assert!(locks.is_empty(), "locks left: {locks:?}");

    // The next run picks up where it stopped, with no resync.
    w.run_ok("the run after");
    assert_eq!(
        fs::metadata(w.path("dst/big.bin")).unwrap().len(),
        BIG as u64
    );
}

/// Every file under `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}
