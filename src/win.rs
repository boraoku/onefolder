//! The Windows system calls OneFolder makes itself, where Unix has a
//! signal, `/proc` or `ps`: whether a process is running and what it is
//! called (for the run lock and rclone's bisync lock), when the computer
//! started, stopping a child, and the console's Ctrl-Break, which is how a
//! child in its own process group is asked to stop as SIGINT asks on Unix.

use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

/// `CREATE_NEW_PROCESS_GROUP`, for `Command::creation_flags`: the child
/// gets a process group of its own, so a Ctrl-C typed in the console
/// reaches only OneFolder, which decides what the child is told, as
/// `process_group(0)` does on Unix.
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// A handle closed when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned open by the call that made it and
        // is closed only here.
        unsafe { CloseHandle(self.0) };
    }
}

/// The executable name of the running process with this id (`rclone.exe`),
/// from a snapshot of every process. A snapshot names processes of other
/// users and protected ones too, which opening the process could not.
/// `None` when no process has the id.
pub fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: plain call; a failure is INVALID_HANDLE_VALUE.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return None;
    }
    let snapshot = Handle(snapshot);
    // SAFETY: PROCESSENTRY32W is plain data, valid when zeroed; dwSize is
    // set before the first call as the API requires.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `snapshot` is open and `entry` is writable and sized.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        if entry.th32ProcessID == pid {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            // Some entries carry a path; the name is its last part.
            let name = Path::new(&name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(name);
            return Some(name);
        }
        // SAFETY: as for Process32FirstW.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    None
}

/// True when a process with this id is running.
pub fn pid_alive(pid: u32) -> bool {
    process_name(pid).is_some()
}

/// When the computer started, in Unix seconds: now less the time since
/// the start. With Fast Startup a shutdown is a hibernation, which does
/// not count as a start; the process name still tells a reused id apart.
pub fn boot_time() -> Option<i64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    // SAFETY: plain call with no arguments.
    let up_ms = unsafe { GetTickCount64() };
    i64::try_from(now.checked_sub(up_ms / 1000)?).ok()
}

/// A job object holding a child and every process it starts, so the
/// whole tree is waited for and ended together: a run is over only when
/// nothing it started is left, a kill ends them all, and closing the job
/// (dropping this) ends whatever is still in it, as `kill_on_drop` does
/// for the child alone. rclone itself starts no processes; a launcher in
/// front of it would (see `rclone::detect`, which runs the real rclone
/// behind Scoop's and Chocolatey's).
pub struct Job(Handle);

impl Job {
    /// Puts the process behind `process` (a child's handle) in a new job.
    /// `None` when Windows refuses; the child then runs as before.
    pub fn assign(process: std::os::windows::io::RawHandle) -> Option<Job> {
        // SAFETY: plain call; no security attributes and no name.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return None;
        }
        let job = Job(Handle(job));
        // SAFETY: the limit information is plain data, valid when zeroed.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `job` is open and `limits` is the size passed.
        let set = unsafe {
            SetInformationJobObject(
                (job.0).0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        // SAFETY: both handles are open; the process handle stays the
        // child's own.
        let assigned = unsafe { AssignProcessToJobObject((job.0).0, process as HANDLE) };
        (set != 0 && assigned != 0).then_some(job)
    }

    /// How many processes in the job are still running.
    pub fn active(&self) -> u32 {
        // SAFETY: the accounting information is plain data, valid when zeroed.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the job is open and `info` is the size passed.
        let ok = unsafe {
            QueryInformationJobObject(
                (self.0).0,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            0
        } else {
            info.ActiveProcesses
        }
    }

    /// Ends every process in the job at once.
    pub fn terminate(&self) {
        // SAFETY: the job is open.
        unsafe { TerminateJobObject((self.0).0, 1) };
    }
}

/// Ends a process at once, as SIGKILL does. False when it could not.
pub fn terminate(pid: u32) -> bool {
    // SAFETY: plain call; a failure is a null handle.
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        return false;
    }
    let process = Handle(process);
    // SAFETY: `process` is open with PROCESS_TERMINATE access.
    unsafe { TerminateProcess(process.0, 1) != 0 }
}

/// Sends Ctrl-Break to the process group led by `pid` (a child started
/// with [`CREATE_NEW_PROCESS_GROUP`]): Go programs such as rclone read it
/// as an interrupt and stop cleanly, as SIGINT makes them on Unix. False
/// when it could not be sent (OneFolder has no console, say).
pub fn ctrl_break(pid: u32) -> bool {
    // SAFETY: plain call.
    unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 }
}
