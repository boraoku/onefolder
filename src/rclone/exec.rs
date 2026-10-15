//! The only place that starts processes.
//!
//! Three entry points: [`run_capture`] runs a command to completion and
//! collects its output (`rclone version`, `rclone about`, `rclone lsjson`,
//! and `diskutil info -plist` for the volume checks); [`run_attached`]
//! hands the terminal to the command (rclone's browser sign-in) and waits
//! for it; [`run_streaming`] runs a transfer command (`rclone copy`),
//! decodes its JSON stderr line by line on a task, tees every raw line into
//! the per-run log ([`RunLog`]) and maps its exit code ([`ExitKind`]).
//! [`interrupt_guard`] decides what Ctrl-C does while any of them runs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::events::{self, Event};

/// Output of a finished process.
#[derive(Debug, Clone)]
pub struct Captured {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("could not start {program}: {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("lost track of {program}: {source}")]
    Wait {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Children started by the capture helpers ([`run_capture`],
/// [`run_capture_input`], [`run_capture_lines`]) and not yet waited for,
/// by process id. When OneFolder ends early ([`exit_now`], or a signal
/// with no [`InterruptGuard`] alive), the exit hook kills them, since the
/// futures that would kill them on drop never get dropped: a folder
/// browser's `rclone size` would otherwise carry on after OneFolder has
/// gone. A transfer ([`run_streaming`]), rclone's sign-in
/// ([`run_attached`]) and a move ([`run_capture_detached`], which must not
/// be cut in half) are not registered, so their handling is as it was.
static CHILDREN: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// A child's place in [`CHILDREN`], given up when dropped: after the child
/// has been waited for, or when its future is dropped (which kills it).
struct Registered(Option<u32>);

impl Registered {
    fn new(pid: Option<u32>) -> Registered {
        if let (Some(pid), Ok(mut list)) = (pid, CHILDREN.lock()) {
            list.push(pid);
        }
        Registered(pid)
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        if let (Some(pid), Ok(mut list)) = (self.0, CHILDREN.lock()) {
            list.retain(|p| *p != pid);
        }
    }
}

/// The process ids of the registered children (for tests).
#[cfg(test)]
fn registered() -> Vec<u32> {
    CHILDREN.lock().map(|l| l.clone()).unwrap_or_default()
}

/// Kills every registered child at once (SIGKILL), for the exit hook.
fn kill_registered() {
    let pids = CHILDREN
        .lock()
        .map(|mut list| std::mem::take(&mut *list))
        .unwrap_or_default();
    #[cfg(unix)]
    for pid in pids {
        if let Ok(pid) = i32::try_from(pid) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    #[cfg(not(unix))]
    drop(pids);
}

/// Runs `program` with `args`, stdin closed, and waits for it to exit.
/// Output is decoded as UTF-8, replacing invalid sequences. Dropping the
/// future kills the process; so does OneFolder ending early.
pub async fn run_capture(program: &Path, args: &[&str]) -> Result<Captured, ExecError> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ExecError::Spawn {
            program: program.to_path_buf(),
            source,
        })?;
    let registered = Registered::new(child.id());
    let output = child
        .wait_with_output()
        .await
        .map_err(|source| ExecError::Wait {
            program: program.to_path_buf(),
            source,
        })?;
    drop(registered);
    Ok(Captured {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Runs `program` with `args`, writes `input` to its standard input and
/// closes it, and waits for it to exit (`rclone hashsum --files-from-raw
/// -`, which reads its list of files there). Dropping the future kills
/// the process.
pub async fn run_capture_input(
    program: &Path,
    args: &[&str],
    input: &[u8],
) -> Result<Captured, ExecError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ExecError::Spawn {
            program: program.to_path_buf(),
            source,
        })?;
    let registered = Registered::new(child.id());
    let stdin = child.stdin.take();
    let write = async {
        if let Some(mut stdin) = stdin {
            // A process that exits without reading all of it is not an
            // error here; its exit status says what happened.
            let _ = tokio::io::AsyncWriteExt::write_all(&mut stdin, input).await;
        }
    };
    let (_, output) = tokio::join!(write, child.wait_with_output());
    drop(registered);
    let output = output.map_err(|source| ExecError::Wait {
        program: program.to_path_buf(),
        source,
    })?;
    Ok(Captured {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Runs `program` with `args` to completion in its own process group, so
/// a Ctrl-C typed in the terminal does not reach it: for a step that must
/// not be cut in half (one `rclone moveto`), whose caller stops between
/// steps instead. Stdin closed; dropping the future still kills it.
pub async fn run_capture_detached(program: &Path, args: &[String]) -> Result<Captured, ExecError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let output = command.output().await.map_err(|source| ExecError::Spawn {
        program: program.to_path_buf(),
        source,
    })?;
    Ok(Captured {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Runs `program` with `args`, stdin closed, and waits for it to exit, as
/// [`run_capture`] does, but hands each line of standard output to
/// `on_line` as it arrives (a long `rclone lsjson -R` prints one entry per
/// line, so its caller can count them while it runs). The whole output is
/// still collected and returned. A last line without a line break is
/// handed over too. Dropping the future kills the process.
pub async fn run_capture_lines(
    program: &Path,
    args: &[&str],
    on_line: &mut (dyn FnMut(&str) + Send),
) -> Result<Captured, ExecError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ExecError::Spawn {
            program: program.to_path_buf(),
            source,
        })?;
    let registered = Registered::new(child.id());
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let read_out = async {
        let mut text = Vec::new();
        if let Some(stdout) = stdout {
            read_lines(stdout, &mut text, on_line).await;
        }
        text
    };
    // Read stderr at the same time, so a full pipe can never stall rclone.
    let read_err = async {
        let mut text = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut text).await;
        }
        text
    };
    let (out, err) = tokio::join!(read_out, read_err);
    let status = child.wait().await;
    drop(registered);
    let status = status.map_err(|source| ExecError::Wait {
        program: program.to_path_buf(),
        source,
    })?;
    Ok(Captured {
        status,
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

/// Reads `reader` to the end, keeping every byte in `text` and handing
/// each line (without its line break) to `on_line`, the last one even
/// when it has no line break.
pub async fn read_lines<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    text: &mut Vec<u8>,
    on_line: &mut (dyn FnMut(&str) + Send),
) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        text.extend_from_slice(&buf);
        let line = String::from_utf8_lossy(&buf);
        on_line(line.trim_end_matches(['\n', '\r']));
    }
}

/// Where an attached command's standard output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachedOutput {
    /// Straight to the terminal, like the rest of the command's output.
    Inherit,
    /// To our standard error, so our own standard output stays clean (`--json`).
    Stderr,
}

/// How an attached command ended.
#[derive(Debug, Clone, Copy)]
pub struct Attached {
    pub status: ExitStatus,
    /// True when the user pressed Ctrl-C while it ran.
    pub interrupted: bool,
}

/// How long an attached command may take to exit after Ctrl-C before it is killed.
pub const INTERRUPT_GRACE: Duration = Duration::from_secs(10);

/// Runs `program` with the terminal attached (stdin and stderr inherited,
/// stdout as `output` says) and waits for it to exit.
///
/// The caller holds an [`InterruptGuard`] for the whole operation, so
/// Ctrl-C reaches the child directly (it shares the terminal's process
/// group) while this process keeps running. After the first Ctrl-C the
/// child gets [`INTERRUPT_GRACE`] to exit before it is killed; a second
/// Ctrl-C kills it at once. When stdin is a terminal its settings are saved
/// before and restored after, so a child stopped in the middle of a
/// hidden-input prompt cannot leave echo off.
pub async fn run_attached(
    program: &Path,
    args: &[&str],
    output: AttachedOutput,
    interrupts: &mut InterruptGuard,
) -> Result<Attached, ExecError> {
    let stdout = match output {
        AttachedOutput::Inherit => Stdio::inherit(),
        AttachedOutput::Stderr => stderr_handle(),
    };
    let saved = TerminalSettings::save().await;
    interrupts.mark_seen();
    let before = interrupts.count();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(stdout)
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ExecError::Spawn {
            program: program.to_path_buf(),
            source,
        })?;

    let waited = tokio::select! {
        status = child.wait() => status,
        () = interrupts.next() => {
            tokio::select! {
                status = child.wait() => status,
                // A second Ctrl-C, or no exit within the grace period: stop it.
                () = interrupts.next() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
                () = tokio::time::sleep(INTERRUPT_GRACE) => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            }
        }
    };
    if let Some(saved) = saved {
        saved.restore().await;
    }
    let status = waited.map_err(|source| ExecError::Wait {
        program: program.to_path_buf(),
        source,
    })?;
    Ok(Attached {
        status,
        interrupted: interrupts.count() != before,
    })
}

/// Number of guards alive. While it is zero, Ctrl-C ends the process.
static GUARDS: AtomicUsize = AtomicUsize::new(0);

/// Counts Ctrl-C presses once the dispatcher is running.
static PRESSES: OnceLock<Option<watch::Sender<u64>>> = OnceLock::new();

/// The Ctrl-C dispatcher, started on first use. It owns SIGINT for the rest
/// of the process: with no [`InterruptGuard`] alive it exits with status
/// 130, as the default disposition would; with one alive it only counts the
/// press and wakes the guards. So the state outside a guard is always the
/// familiar one, whatever ran before. `None` when the handler could not be
/// installed, in which case Ctrl-C keeps its default effect.
fn presses() -> Option<&'static watch::Sender<u64>> {
    PRESSES
        .get_or_init(|| {
            let (tx, _) = watch::channel(0u64);
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
            let thread_tx = tx.clone();
            let spawned = std::thread::Builder::new()
                .name("onefolder-ctrl-c".into())
                .spawn(move || dispatch(thread_tx, ready_tx));
            let ready = spawned.is_ok() && ready_rx.recv().unwrap_or(false);
            ready.then_some(tx)
        })
        .as_ref()
}

fn dispatch(tx: watch::Sender<u64>, ready: std::sync::mpsc::Sender<bool>) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        let _ = ready.send(false);
        return;
    };
    runtime.block_on(async move {
        let Some(mut signals) = Signals::install() else {
            let _ = ready.send(false);
            return;
        };
        let _ = ready.send(true);
        while let Some(code) = signals.recv().await {
            if GUARDS.load(Ordering::SeqCst) == 0 {
                run_exit_hook();
                std::process::exit(code);
            }
            tx.send_modify(|n| *n += 1);
        }
    });
}

/// The signals the dispatcher owns: SIGINT, and SIGTERM and SIGHUP, so a
/// `kill` or a closed terminal stops rclone cleanly and the run is still
/// recorded. `recv` gives the exit status the default disposition would
/// have meant (128 plus the signal number).
#[cfg(unix)]
struct Signals {
    int: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn install() -> Option<Signals> {
        use tokio::signal::unix::{signal, SignalKind};
        Some(Signals {
            int: signal(SignalKind::interrupt()).ok()?,
            term: signal(SignalKind::terminate()).ok()?,
            hup: signal(SignalKind::hangup()).ok()?,
        })
    }

    async fn recv(&mut self) -> Option<i32> {
        tokio::select! {
            s = self.int.recv() => s.map(|()| 130),
            s = self.term.recv() => s.map(|()| 143),
            s = self.hup.recv() => s.map(|()| 129),
        }
    }
}

#[cfg(not(unix))]
struct Signals;

#[cfg(not(unix))]
impl Signals {
    fn install() -> Option<Signals> {
        Some(Signals)
    }

    async fn recv(&mut self) -> Option<i32> {
        tokio::signal::ctrl_c().await.ok().map(|()| 130)
    }
}

/// What to run before the dispatcher ends the process (no guard alive):
/// the screen that owns the terminal registers its restore here, so a
/// signal never leaves raw mode or the alternate screen behind.
static EXIT_HOOK: std::sync::Mutex<Option<fn()>> = std::sync::Mutex::new(None);

/// Registers (or with `None` clears) the hook run before a signal ends the process.
pub fn set_exit_hook(hook: Option<fn()>) {
    if let Ok(mut slot) = EXIT_HOOK.lock() {
        *slot = hook;
    }
}

/// Ends OneFolder at once with `code`, after the same clean-up a signal
/// gets (the screen's restore hook, the run lock): for Ctrl-C pressed on a
/// screen, which reads it as a key.
pub fn exit_now(code: i32) -> ! {
    run_exit_hook();
    std::process::exit(code)
}

fn run_exit_hook() {
    kill_registered();
    let hook = EXIT_HOOK.lock().ok().and_then(|slot| *slot);
    if let Some(hook) = hook {
        hook();
    }
    let files = EXIT_REMOVE
        .lock()
        .map(|mut list| std::mem::take(&mut *list))
        .unwrap_or_default();
    for file in files {
        let _ = std::fs::remove_file(file);
    }
}

/// Files to delete before a signal ends the process: the run lock, which
/// a normal exit removes when it is dropped.
static EXIT_REMOVE: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Asks for `path` to be deleted if a signal ends OneFolder before the
/// owner removes it itself (then call [`forget_on_exit`]).
pub fn remove_on_exit(path: &Path) {
    if let Ok(mut list) = EXIT_REMOVE.lock() {
        list.push(path.to_path_buf());
    }
}

/// Undoes [`remove_on_exit`] for a file its owner has removed.
pub fn forget_on_exit(path: &Path) {
    if let Ok(mut list) = EXIT_REMOVE.lock() {
        list.retain(|p| p != path);
    }
}

/// While alive, Ctrl-C does not end OneFolder: it reaches whatever child
/// shares the terminal, and the guard sees it. Hold one across everything
/// that belongs to one interruptible operation (a sign-in, its check and
/// its clean-up), so a Ctrl-C at any point is reported as a cancellation.
/// Dropping the last guard puts back "Ctrl-C quits".
pub struct InterruptGuard {
    rx: Option<watch::Receiver<u64>>,
    start: u64,
}

/// Starts the signal dispatcher now rather than at the first
/// [`InterruptGuard`], so SIGINT, SIGTERM and SIGHUP run the exit hook
/// (which restores the terminal) before they end OneFolder, also before
/// any run has started. A screen that takes the terminal calls it.
pub fn install_signal_handler() {
    let _ = presses();
}

/// Takes the Ctrl-C guard (see [`InterruptGuard`]).
pub fn interrupt_guard() -> InterruptGuard {
    GUARDS.fetch_add(1, Ordering::SeqCst);
    let rx = presses().map(watch::Sender::subscribe);
    let start = rx.as_ref().map(|r| *r.borrow()).unwrap_or(0);
    InterruptGuard { rx, start }
}

impl InterruptGuard {
    fn count(&self) -> u64 {
        self.rx.as_ref().map(|r| *r.borrow()).unwrap_or(0)
    }

    /// Forgets presses so far, so [`InterruptGuard::next`] waits for a new one.
    fn mark_seen(&mut self) {
        if let Some(rx) = &mut self.rx {
            rx.borrow_and_update();
        }
    }

    /// True once Ctrl-C has been pressed since the guard was taken.
    pub fn interrupted(&self) -> bool {
        self.count() != self.start
    }

    /// Runs `f` with Ctrl-C back to quitting OneFolder, as at any prompt,
    /// unless another guard is alive. Used around a question asked before
    /// anything has run, so Ctrl-C there does not wait for an answer.
    pub fn unguarded<R>(&mut self, f: impl FnOnce() -> R) -> R {
        struct Rearm;
        impl Drop for Rearm {
            fn drop(&mut self) {
                GUARDS.fetch_add(1, Ordering::SeqCst);
            }
        }
        GUARDS.fetch_sub(1, Ordering::SeqCst);
        let _rearm = Rearm;
        f()
    }

    /// Waits for the next Ctrl-C. Never returns if none can be seen.
    pub async fn next(&mut self) {
        match &mut self.rx {
            Some(rx) => {
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending::<()>().await,
        }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        GUARDS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// rclone's exit codes, as its documentation lists them (confirmed on
/// 1.75.2: an unknown flag exits 2, a per-file error 1, a missing source 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitKind {
    /// 0
    Success,
    /// 1: an error not otherwise categorised, such as files that failed.
    Uncategorised,
    /// 2: a syntax or usage error, which is a OneFolder bug.
    Usage,
    /// 3
    DirectoryNotFound,
    /// 4
    FileNotFound,
    /// 5: temporary errors that more retries might fix.
    Temporary,
    /// 6: less serious errors.
    LessSerious,
    /// 7: fatal errors that retries will not fix (account suspended...).
    Fatal,
    /// 8: `--max-transfer` reached.
    TransferLimit,
    /// 9: success, nothing transferred (only with `--error-on-no-transfer`).
    NoFilesTransferred,
    /// 10: `--max-duration` reached.
    DurationLimit,
    /// Stopped by a signal (130 is rclone's own exit after SIGINT).
    Signalled,
    /// Any other code.
    Other,
}

impl ExitKind {
    /// The kind for an exit code; `None` means the process died of a signal.
    pub fn from_code(code: Option<i32>) -> ExitKind {
        match code {
            Some(0) => ExitKind::Success,
            Some(1) => ExitKind::Uncategorised,
            Some(2) => ExitKind::Usage,
            Some(3) => ExitKind::DirectoryNotFound,
            Some(4) => ExitKind::FileNotFound,
            Some(5) => ExitKind::Temporary,
            Some(6) => ExitKind::LessSerious,
            Some(7) => ExitKind::Fatal,
            Some(8) => ExitKind::TransferLimit,
            Some(9) => ExitKind::NoFilesTransferred,
            Some(10) => ExitKind::DurationLimit,
            Some(130) | Some(143) | None => ExitKind::Signalled,
            Some(_) => ExitKind::Other,
        }
    }

    /// True for the codes that mean the command did its work.
    pub fn is_success(self) -> bool {
        matches!(self, ExitKind::Success | ExitKind::NoFilesTransferred)
    }

    /// True for the codes after which some files may have gone across:
    /// failed files, retryable errors, and the transfer limits.
    pub fn may_be_partial(self) -> bool {
        matches!(
            self,
            ExitKind::Uncategorised
                | ExitKind::Temporary
                | ExitKind::LessSerious
                | ExitKind::TransferLimit
                | ExitKind::DurationLimit
        )
    }

    /// A few words for the user.
    pub fn describe(self) -> &'static str {
        match self {
            ExitKind::Success | ExitKind::NoFilesTransferred => "finished",
            ExitKind::Uncategorised => "finished with errors",
            ExitKind::Usage => "rejected its arguments (a OneFolder bug, please report it)",
            ExitKind::DirectoryNotFound => "could not find a folder",
            ExitKind::FileNotFound => "could not find a file",
            ExitKind::Temporary => "stopped on errors that a later run may get past",
            ExitKind::LessSerious => "finished with minor errors",
            ExitKind::Fatal => "stopped on a fatal error",
            ExitKind::TransferLimit => "reached its transfer limit",
            ExitKind::DurationLimit => "reached its time limit",
            ExitKind::Signalled => "was stopped",
            ExitKind::Other => "exited with an unexpected code",
        }
    }
}

/// How a streamed command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    pub exit: ExitKind,
    /// The raw exit code, when there is one.
    pub code: Option<i32>,
    /// True when Ctrl-C was pressed while it ran.
    pub cancelled: bool,
    /// True when it had to be killed (a second Ctrl-C, or no exit within
    /// [`INTERRUPT_GRACE`] of the first).
    pub killed: bool,
}

/// What [`run_streaming`] reports while the command runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// A decoded stderr line.
    Event(Event),
    /// Ctrl-C was pressed: rclone has been sent SIGINT.
    Stopping,
    /// rclone is being killed.
    Killing,
}

/// The per-run log: `logs/<job>/<dest-id>/<timestamp>.jsonl` under the
/// data directory. Every raw stderr line is appended as it arrives, so a
/// crash mid-run still leaves a usable log; each command starts with a
/// JSON line naming the command line that ran.
#[derive(Debug)]
pub struct RunLog {
    file: File,
    path: PathBuf,
}

impl RunLog {
    /// Creates a new log file in `dir` named `<stamp>.jsonl` (or
    /// `<stamp>-2.jsonl` and so on when that name is taken), creating the
    /// directories first.
    pub fn create(dir: &Path, stamp: &str) -> io::Result<RunLog> {
        fs::create_dir_all(dir)?;
        let mut n = 1;
        loop {
            let name = if n == 1 {
                format!("{stamp}.jsonl")
            } else {
                format!("{stamp}-{n}.jsonl")
            };
            let path = dir.join(name);
            match OpenOptions::new().append(true).create_new(true).open(&path) {
                Ok(file) => return Ok(RunLog { file, path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one JSON object of OneFolder's own as a line (the command
    /// line, a refusal, a note). Logging is best effort: a full disk must
    /// not stop a sync.
    pub fn note(&mut self, value: &serde_json::Value) {
        let _ = writeln!(self.file, "{value}");
    }

    /// Records the command line about to run.
    pub fn command(&mut self, program: &Path, args: &[String]) {
        let line = std::iter::once(program.display().to_string())
            .chain(args.iter().map(|a| shell_quote(a)))
            .collect::<Vec<_>>()
            .join(" ");
        self.note(&serde_json::json!({
            "onefolder": "command",
            "command": line,
            "time": jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string(),
        }));
    }

    fn handle(&self) -> Option<File> {
        self.file.try_clone().ok()
    }
}

/// Quotes an argument for the log's command line when it needs it.
fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// How long the stderr reader may run on after the process exits (a
/// grandchild holding the pipe open must not hang the run).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Runs `program` with `args`, stdin and stdout closed and stderr read
/// line by line on a task: each line is appended to `log`, decoded, and
/// handed to `on` in order. Returns once the process has exited and its
/// stderr is drained.
///
/// The process gets its own process group, so a Ctrl-C typed in the
/// terminal reaches only OneFolder, which decides: the first press (seen
/// through `interrupts`, which the caller holds for the whole run) sends
/// rclone SIGINT and reports [`Progress::Stopping`]; a second press, or no
/// exit within [`INTERRUPT_GRACE`], kills it.
pub async fn run_streaming(
    program: &Path,
    args: &[String],
    log: Option<&mut RunLog>,
    interrupts: &mut InterruptGuard,
    on: &mut dyn FnMut(Progress),
) -> Result<Finished, ExecError> {
    run_streaming_with_grace(program, args, log, interrupts, INTERRUPT_GRACE, on).await
}

/// How long `rclone bisync` may take to stop after Ctrl-C before it is
/// killed. bisync answers SIGINT with a graceful shutdown: it lets the
/// transfers in flight finish for up to 30 s, then cancels them, saves its
/// listings and removes its lock (confirmed on 1.75.2). Killing it sooner
/// would leave the lock behind; a second Ctrl-C still kills at once.
pub const BISYNC_INTERRUPT_GRACE: Duration = Duration::from_secs(45);

/// [`run_streaming`] with its own grace period after the first Ctrl-C.
pub async fn run_streaming_with_grace(
    program: &Path,
    args: &[String],
    log: Option<&mut RunLog>,
    interrupts: &mut InterruptGuard,
    grace: Duration,
    on: &mut dyn FnMut(Progress),
) -> Result<Finished, ExecError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut log_file = None;
    if let Some(log) = log {
        log.command(program, args);
        log_file = log.handle();
    }
    interrupts.mark_seen();
    let mut child = command.spawn().map_err(|source| ExecError::Spawn {
        program: program.to_path_buf(),
        source,
    })?;

    let (tx, mut rx) = mpsc::channel::<Event>(256);
    let reader = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match lines.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if let Some(file) = log_file.as_mut() {
                    let _ = file.write_all(&buf);
                    if !buf.ends_with(b"\n") {
                        let _ = file.write_all(b"\n");
                    }
                }
                let line = String::from_utf8_lossy(&buf);
                if line.trim().is_empty() {
                    continue;
                }
                if tx.send(events::decode(&line)).await.is_err() {
                    break;
                }
            }
        })
    });
    let mut open = reader.is_some();

    let mut status: Option<ExitStatus> = None;
    let mut presses = 0u32;
    let mut killed = false;
    let mut kill_at: Option<Instant> = None;
    let mut drain_until: Option<Instant> = None;
    loop {
        if status.is_some() && !open {
            break;
        }
        let deadline = match (status.is_some(), kill_at, drain_until) {
            (false, Some(at), _) => Some(at),
            (true, _, Some(at)) => Some(at),
            _ => None,
        };
        tokio::select! {
            // Queued lines first, so none is lost to the drain deadline.
            biased;
            event = rx.recv(), if open => match event {
                Some(event) => on(Progress::Event(event)),
                None => open = false,
            },
            waited = child.wait(), if status.is_none() => {
                status = Some(waited.map_err(|source| ExecError::Wait {
                    program: program.to_path_buf(),
                    source,
                })?);
                drain_until = Some(Instant::now() + DRAIN_GRACE);
            }
            () = interrupts.next(), if status.is_none() => {
                presses += 1;
                if presses == 1 {
                    if !send_interrupt(&child) {
                        let _ = child.start_kill();
                        killed = true;
                    }
                    kill_at = Some(Instant::now() + grace);
                    on(Progress::Stopping);
                } else if !killed {
                    let _ = child.start_kill();
                    killed = true;
                    on(Progress::Killing);
                }
            }
            () = sleep_until(deadline) => {
                if status.is_none() {
                    // No exit within the grace period after SIGINT.
                    let _ = child.start_kill();
                    killed = true;
                    kill_at = None;
                    on(Progress::Killing);
                } else {
                    // The process is gone but something holds its stderr.
                    open = false;
                }
            }
        }
    }
    if let Some(reader) = reader {
        reader.abort();
    }
    let status = status.ok_or_else(|| ExecError::Wait {
        program: program.to_path_buf(),
        source: io::Error::other("the process was not waited for"),
    })?;
    let code = status.code();
    Ok(Finished {
        exit: ExitKind::from_code(code),
        code,
        // Only presses seen while rclone ran; one after it exited is the
        // caller's to notice (the guard remembers it).
        cancelled: presses > 0,
        killed,
    })
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

/// Sends SIGINT to the child. False when it could not be sent.
#[cfg(unix)]
fn send_interrupt(child: &tokio::process::Child) -> bool {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) else {
        return false;
    };
    kill(Pid::from_raw(pid), Signal::SIGINT).is_ok()
}

#[cfg(not(unix))]
fn send_interrupt(_child: &tokio::process::Child) -> bool {
    false
}

/// Counts as a Ctrl-C press, for screens that read Ctrl-C as a key (raw
/// mode turns the terminal's own SIGINT off). Every live
/// [`InterruptGuard`] sees it.
pub fn press_interrupt() {
    if let Some(tx) = presses() {
        tx.send_modify(|n| *n += 1);
    }
}

/// Tests that press Ctrl-C (a real SIGINT, or [`press_interrupt`]) wake
/// every live guard in the test process, so every test that holds a guard
/// takes its turn with this lock.
#[cfg(test)]
pub static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A duplicate of our standard error, for a child's standard output.
#[cfg(unix)]
fn stderr_handle() -> Stdio {
    use std::os::fd::AsFd;
    match std::io::stderr().as_fd().try_clone_to_owned() {
        Ok(fd) => Stdio::from(fd),
        Err(_) => Stdio::null(),
    }
}

#[cfg(not(unix))]
fn stderr_handle() -> Stdio {
    Stdio::null()
}

/// Absolute path, so a different `stty` earlier on PATH is never run.
const STTY: &str = "/bin/stty";

/// The terminal's settings as `stty -g` prints them.
struct TerminalSettings(String);

impl TerminalSettings {
    /// Saves the settings of the terminal on stdin, if stdin is one.
    async fn save() -> Option<TerminalSettings> {
        if !cfg!(unix) || !std::io::stdin().is_terminal() {
            return None;
        }
        let out = Command::new(STTY)
            .arg("-g")
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        let text = text.trim();
        (out.status.success() && !text.is_empty()).then(|| TerminalSettings(text.to_string()))
    }

    /// Puts the saved settings back. Best effort: a failure leaves the
    /// terminal as the child left it.
    async fn restore(self) {
        let _ = Command::new(STTY)
            .arg(&self.0)
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    use super::SERIAL;

    fn sh(script: &str) -> Vec<&str> {
        vec!["-c", script]
    }

    /// A capture's child is registered while it runs, for the exit hook to
    /// kill, and forgotten once it is waited for or its future is dropped.
    #[tokio::test]
    async fn captured_children_are_registered_while_they_run() {
        let task = tokio::spawn(async {
            run_capture(Path::new("/bin/sh"), &sh("echo $$; exec sleep 30")).await
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            // The newest registration is ours once one appears that is a sleep.
            let found = registered().into_iter().find(|pid| {
                std::process::Command::new("/bin/ps")
                    .args(["-p", &pid.to_string(), "-o", "command="])
                    .output()
                    .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("sleep 30"))
            });
            if let Some(pid) = found {
                break pid;
            }
            assert!(Instant::now() < deadline, "never registered");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        task.abort();
        let _ = task.await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while registered().contains(&pid) {
            assert!(Instant::now() < deadline, "still registered after the drop");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // A capture that finishes is forgotten too.
        let out = run_capture(Path::new("/bin/sh"), &sh("echo $$"))
            .await
            .unwrap();
        let pid: u32 = out.stdout.trim().parse().unwrap();
        assert!(!registered().contains(&pid));
    }

    #[tokio::test]
    async fn attached_reports_the_exit_status() {
        let _turn = SERIAL.lock().await;
        let mut guard = interrupt_guard();
        let ok = run_attached(
            Path::new("/bin/sh"),
            &sh("exit 0"),
            AttachedOutput::Stderr,
            &mut guard,
        )
        .await
        .unwrap();
        assert!(ok.status.success());
        assert!(!ok.interrupted);
        let failed = run_attached(
            Path::new("/bin/sh"),
            &sh("exit 3"),
            AttachedOutput::Stderr,
            &mut guard,
        )
        .await
        .unwrap();
        assert_eq!(failed.status.code(), Some(3));
    }

    #[tokio::test]
    async fn attached_spawn_failure_is_an_error() {
        let err = run_attached(
            Path::new("/nowhere/rclone"),
            &[],
            AttachedOutput::Inherit,
            &mut interrupt_guard(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ExecError::Spawn { .. }));
    }

    /// A Ctrl-C while a guard is held is counted, not fatal, and the
    /// attached child is reported as interrupted.
    #[tokio::test]
    async fn ctrl_c_under_a_guard_is_seen() {
        let _turn = SERIAL.lock().await;
        let mut guard = interrupt_guard();
        assert!(!guard.interrupted());
        // The child sends SIGINT to this process, as the terminal would.
        let script = format!("kill -INT {}; sleep 0.5; exit 0", std::process::id());
        let got = run_attached(
            Path::new("/bin/sh"),
            &sh(&script),
            AttachedOutput::Stderr,
            &mut guard,
        )
        .await
        .unwrap();
        assert!(got.interrupted);
        assert!(guard.interrupted());
    }
    fn script(text: &str) -> Vec<String> {
        vec!["-c".to_string(), text.to_string()]
    }

    /// Runs a shell script with `run_streaming`, collecting what it reports.
    async fn stream(
        text: &str,
        log: Option<&mut RunLog>,
        guard: &mut InterruptGuard,
    ) -> (Finished, Vec<Progress>) {
        let mut seen = Vec::new();
        let finished = run_streaming(Path::new("/bin/sh"), &script(text), log, guard, &mut |p| {
            seen.push(p)
        })
        .await
        .unwrap();
        (finished, seen)
    }

    #[tokio::test]
    async fn streaming_decodes_logs_and_maps_the_exit_code() {
        let _turn = SERIAL.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let mut log = RunLog::create(&dir.path().join("logs/job/d-0001"), "stamp").unwrap();
        let mut guard = interrupt_guard();
        let line = r#"{"level":"info","msg":"Copied (new)","object":"a.txt","size":1}"#;
        let (finished, seen) = stream(
            &format!("echo '{line}' >&2; printf 'not json' >&2; exit 3"),
            Some(&mut log),
            &mut guard,
        )
        .await;
        assert_eq!(finished.exit, ExitKind::DirectoryNotFound);
        assert_eq!(finished.code, Some(3));
        assert!(!finished.cancelled && !finished.killed);
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(matches!(&seen[0], Progress::Event(Event::Transferred(t)) if t.object == "a.txt"));
        assert_eq!(
            seen[1],
            Progress::Event(Event::Unparsed {
                line: "not json".into()
            })
        );
        // The log: the command line first, then every raw line as it came.
        let text = fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["onefolder"], "command");
        assert!(first["command"]
            .as_str()
            .unwrap()
            .starts_with("/bin/sh -c 'echo"));
        assert_eq!(lines[1], line);
        assert_eq!(lines[2], "not json");
        // A second log in the same second gets its own name.
        let again = RunLog::create(&dir.path().join("logs/job/d-0001"), "stamp").unwrap();
        assert!(again.path().ends_with("stamp-2.jsonl"));
    }

    #[tokio::test]
    async fn first_ctrl_c_asks_the_child_to_stop() {
        let _turn = SERIAL.lock().await;
        let mut guard = interrupt_guard();
        let presser = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            press_interrupt();
        });
        let (finished, seen) = stream(
            "trap 'echo stopped >&2; exit 7' INT; while true; do sleep 0.05; done",
            None,
            &mut guard,
        )
        .await;
        presser.await.unwrap();
        assert!(finished.cancelled && !finished.killed, "{finished:?}");
        assert_eq!(finished.exit, ExitKind::Fatal);
        assert_eq!(seen[0], Progress::Stopping);
        assert!(seen.contains(&Progress::Event(Event::Unparsed {
            line: "stopped".into()
        })));
    }

    #[tokio::test]
    async fn second_ctrl_c_kills() {
        let _turn = SERIAL.lock().await;
        let mut guard = interrupt_guard();
        let presser = tokio::spawn(async {
            for _ in 0..2 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                press_interrupt();
            }
        });
        let (finished, seen) = stream(
            "trap '' INT; while true; do sleep 0.05; done",
            None,
            &mut guard,
        )
        .await;
        presser.await.unwrap();
        assert!(finished.cancelled && finished.killed, "{finished:?}");
        assert_eq!(finished.exit, ExitKind::Signalled);
        assert_eq!(seen, [Progress::Stopping, Progress::Killing]);
    }

    #[test]
    fn exit_codes_follow_rclones_documentation() {
        let expected = [
            (Some(0), ExitKind::Success),
            (Some(1), ExitKind::Uncategorised),
            (Some(2), ExitKind::Usage),
            (Some(3), ExitKind::DirectoryNotFound),
            (Some(4), ExitKind::FileNotFound),
            (Some(5), ExitKind::Temporary),
            (Some(6), ExitKind::LessSerious),
            (Some(7), ExitKind::Fatal),
            (Some(8), ExitKind::TransferLimit),
            (Some(9), ExitKind::NoFilesTransferred),
            (Some(10), ExitKind::DurationLimit),
            (Some(130), ExitKind::Signalled),
            (None, ExitKind::Signalled),
            (Some(42), ExitKind::Other),
        ];
        for (code, kind) in expected {
            assert_eq!(ExitKind::from_code(code), kind, "{code:?}");
        }
        assert!(ExitKind::Success.is_success() && ExitKind::NoFilesTransferred.is_success());
        assert!(!ExitKind::Uncategorised.is_success());
        for k in [
            ExitKind::Uncategorised,
            ExitKind::Temporary,
            ExitKind::LessSerious,
            ExitKind::TransferLimit,
            ExitKind::DurationLimit,
        ] {
            assert!(k.may_be_partial(), "{k:?}");
        }
        for k in [
            ExitKind::Usage,
            ExitKind::DirectoryNotFound,
            ExitKind::FileNotFound,
            ExitKind::Fatal,
            ExitKind::Signalled,
        ] {
            assert!(!k.may_be_partial(), "{k:?}");
        }
        // The recorded fixtures' exit codes, from real rclone runs.
        let text = fs::read_to_string(crate::rclone::fixture_path("exit-codes.txt")).unwrap();
        let codes: std::collections::HashMap<&str, i32> = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_once(' '))
            .map(|(n, c)| (n, c.parse().unwrap()))
            .collect();
        assert_eq!(
            ExitKind::from_code(Some(codes["copy-real"])),
            ExitKind::Success
        );
        assert_eq!(
            ExitKind::from_code(Some(codes["copy-file-error"])),
            ExitKind::Uncategorised
        );
        assert_eq!(
            ExitKind::from_code(Some(codes["copy-missing-source"])),
            ExitKind::DirectoryNotFound
        );
        assert_eq!(
            ExitKind::from_code(Some(codes["copy-sigint"])),
            ExitKind::Signalled
        );
        assert_eq!(
            ExitKind::from_code(Some(codes["copy-usage"])),
            ExitKind::Usage
        );
    }

    #[test]
    fn command_lines_are_quoted_for_the_log() {
        assert_eq!(shell_quote("--exclude"), "--exclude");
        assert_eq!(
            shell_quote("onefolder-work:Backups/Docs"),
            "onefolder-work:Backups/Docs"
        );
        assert_eq!(shell_quote("*.tmp"), "'*.tmp'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
