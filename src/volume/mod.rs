//! Volume identity for local paths: proves a path is on the drive that was
//! recorded at setup, so an unplugged drive is never mistaken for an empty
//! folder. The interface takes a path and returns an [`Identity`]; the
//! macOS implementation (statfs and `diskutil info -plist`) lives in
//! `macos.rs` and the Linux one (`/proc/self/mountinfo`, `lsblk`, and
//! PowerShell for Windows drives under WSL) in `linux.rs`, each behind
//! `cfg(target_os)`; other platforms get a stub that says the check is not
//! supported there. Where drives are mounted and which mount points are
//! the system disk come from [`crate::platform`].
//!
//! [`identify`] runs at setup (`jobs add`, `jobs edit`); [`check`] runs the
//! four checks from DESIGN.md before a run and in `doctor`.

// Compiled on every Unix so its parsers are tested on a Mac too; only
// Linux uses it.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;

use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

use crate::platform as host;

/// Where the volume holding a path is mounted, from `statfs` (macOS) or
/// the mount table (Linux).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// `f_mntonname`: `/Volumes/Backup`, `/` or `/System/Volumes/Data`;
    /// on Linux `/media/bora/Backup` or `/`.
    pub mount_point: String,
    /// `f_mntfromname`: the device node, such as `/dev/disk4s1` or
    /// `/dev/sdb1`, or the share for a network volume (`//bora@nas/Backup`).
    pub device: String,
    /// `f_fstypename`: `apfs`, `exfat`, `smbfs`...; on Linux `ext4`,
    /// `vfat`, `cifs`, or `9p` and `drvfs` for Windows drives under WSL.
    pub fstype: String,
    pub read_only: bool,
    /// Linux only: the folder of the file system that is mounted there
    /// (mountinfo's root field): `/` for a whole drive, `/@data` for a
    /// btrfs subvolume, `/Docs` for a bind mount of a sub-folder. `None`
    /// on macOS.
    pub root: Option<String>,
}

/// What `diskutil info -plist <mount point>` says about a volume, or on
/// Linux `lsblk` (or, for a Windows drive under WSL, `Get-Volume`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskInfo {
    pub volume_uuid: Option<String>,
    /// `DiskUUID`: the partition's UUID (GPT), present on many drives whose
    /// file system has no volume UUID.
    pub disk_uuid: Option<String>,
    pub volume_name: Option<String>,
    pub device_node: Option<String>,
    pub mount_point: Option<String>,
    /// `WritableVolume`, when reported.
    pub writable: Option<bool>,
}

/// The identity recorded for a local path at setup. Exactly one proof is
/// recorded, the strongest available:
///
/// 1. `volume_uuid`, the file system's UUID;
/// 2. `partition_uuid` (diskutil's `DiskUUID`), for drives whose file
///    system has no UUID (some exFAT drives) but whose partition does;
/// 3. `device` plus the volume name, the weak fallback: a device node such
///    as `/dev/disk4s1` can change when drives are plugged in another order.
///    Network volumes, which diskutil does not describe, record
///    `<fstype>:<share>` here (`smbfs://bora@nas/Backup`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub mount_point: String,
    pub volume_name: String,
    pub volume_uuid: Option<String>,
    pub partition_uuid: Option<String>,
    pub device: Option<String>,
    /// Linux: the folder of the file system mounted at the mount point,
    /// when it is not the whole file system (`/@data`, a btrfs subvolume).
    /// `None` means `/`; compared only where the mount reports one, so
    /// another subvolume of the same drive, or a bind mount of one of its
    /// folders, is the wrong drive.
    pub mount_root: Option<String>,
}

impl Identity {
    /// Rebuilds an identity from the fields recorded in the config. Needs a
    /// UUID, a partition UUID or a device node. A missing mount point is
    /// derived from a drive path (see [`crate::platform::drive_path_on`]):
    /// `/Volumes/<name>/...` on macOS, `/media/<user>/<name>/...`,
    /// `/run/media/<user>/<name>/...` or `/mnt/<name>/...` on Linux.
    pub fn recorded(path: &str, fields: Recorded<'_>) -> Option<Identity> {
        let non_empty = |s: Option<&str>| s.filter(|v| !v.is_empty()).map(str::to_string);
        let volume_uuid = non_empty(fields.volume_uuid);
        let partition_uuid = non_empty(fields.partition_uuid);
        let device = non_empty(fields.device);
        if volume_uuid.is_none() && partition_uuid.is_none() && device.is_none() {
            return None;
        }
        let mount_point =
            non_empty(fields.mount_point).or_else(|| drive_mount_point(Path::new(path)))?;
        Some(Identity {
            mount_point,
            volume_name: fields.volume_name.unwrap_or_default().to_string(),
            volume_uuid,
            partition_uuid,
            device,
            mount_root: non_empty(fields.mount_root).filter(|r| r != "/"),
        })
    }

    /// True when there is no UUID of either kind, so the check compares
    /// device and name only.
    pub fn is_weak(&self) -> bool {
        self.volume_uuid.is_none() && self.partition_uuid.is_none()
    }

    /// `volume "Backup" (UUID A1B2C3D4-...)`, as the messages show it.
    pub fn describe(&self) -> String {
        format!("volume {}", self.describe_short())
    }

    fn name_quoted(&self) -> String {
        if self.volume_name.is_empty() {
            "with no name".to_string()
        } else {
            format!("\"{}\"", self.volume_name)
        }
    }

    fn describe_short(&self) -> String {
        let proof = self.describe_proof();
        match &self.mount_root {
            Some(root) if root != "/" => format!("{proof}, folder {root} of it"),
            _ => proof,
        }
    }

    fn describe_proof(&self) -> String {
        match (&self.volume_uuid, &self.partition_uuid, &self.device) {
            (Some(uuid), _, _) => format!("{} (UUID {})", self.name_quoted(), short_uuid(uuid)),
            (None, Some(uuid), _) => format!(
                "{} (partition UUID {})",
                self.name_quoted(),
                short_uuid(uuid)
            ),
            (None, None, Some(device)) => {
                format!("{} (device {device}, no UUID)", self.name_quoted())
            }
            (None, None, None) => self.name_quoted(),
        }
    }

    /// The headline word for messages: `Backup drive`, or `Drive`.
    fn drive(&self) -> String {
        if self.volume_name.is_empty() {
            "Drive".to_string()
        } else {
            format!("{} drive", self.volume_name)
        }
    }
}

/// The volume fields as recorded in the config, for [`Identity::recorded`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Recorded<'a> {
    pub mount_point: Option<&'a str>,
    pub volume_name: Option<&'a str>,
    pub volume_uuid: Option<&'a str>,
    pub partition_uuid: Option<&'a str>,
    pub device: Option<&'a str>,
    pub mount_root: Option<&'a str>,
}

/// `A1B2C3D4-...` from a full UUID, as in the design's mockup.
pub fn short_uuid(uuid: &str) -> String {
    match uuid.split_once('-') {
        Some((head, _)) => format!("{head}-..."),
        None => uuid.to_string(),
    }
}

/// Whether the path is read (a source) or written (a destination).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Source,
    Destination,
}

impl Role {
    fn from_write(needs_write: bool) -> Role {
        if needs_write {
            Role::Destination
        } else {
            Role::Source
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Source => "Source",
            Role::Destination => "Destination",
        })
    }
}

/// Why a path is not on the volume it should be, or could not be checked.
///
/// `Display` gives a headline and an indented explanation, as in the
/// design's "Backup drive not mounted" mockup; callers add the `✗`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VolumeError {
    /// The recorded mount point is missing, or is a plain folder on another volume.
    NotMounted {
        role: Role,
        path: String,
        expected: Box<Identity>,
        /// Name of the volume the mount point folder is on; `None` when the
        /// mount point does not exist at all.
        found_on: Option<String>,
        /// Where the recorded drive is mounted instead, when it is plugged
        /// in but macOS mounted it elsewhere (`/Volumes/Backup 1`, because
        /// a folder was left at `/Volumes/Backup`).
        elsewhere: Option<String>,
        /// Under WSL: the letter Windows has the drive at now, when Windows
        /// has it but WSL has not mounted it anywhere.
        windows_letter: Option<char>,
    },
    /// Something is mounted there, but not the recorded volume.
    WrongVolume {
        role: Role,
        path: String,
        expected: Box<Identity>,
        found: Box<Identity>,
    },
    ReadOnly {
        role: Role,
        path: String,
        expected: Box<Identity>,
    },
    /// The volume is right but the folder is not there.
    Missing {
        role: Role,
        path: String,
        expected: Box<Identity>,
    },
    NotADirectory {
        role: Role,
        path: String,
    },
    /// Setup only: a drive path (`/Volumes/...`, `/media/...`) that lies
    /// on the system disk.
    OnStartupDisk {
        path: String,
        mount_point: String,
    },
    NotAbsolute {
        path: String,
    },
    /// statfs, diskutil, lsblk or PowerShell failed.
    Probe {
        path: String,
        message: String,
    },
    /// The check did not finish within its limit ([`DEADLINE`], or
    /// [`check_deadline`] under WSL): a hung network mount blocks statfs
    /// and getfsstat, and under WSL PowerShell may be slow to answer.
    NotAnswering {
        path: String,
        /// The limit that ran out.
        limit: Duration,
    },
    Unsupported,
}

impl VolumeError {
    /// A short stable code for `--json` output and doctor.
    pub fn code(&self) -> &'static str {
        match self {
            VolumeError::NotMounted { .. } | VolumeError::OnStartupDisk { .. } => "not-mounted",
            VolumeError::WrongVolume { .. } => "wrong-volume",
            VolumeError::ReadOnly { .. } => "read-only",
            VolumeError::Missing { .. } => "missing",
            VolumeError::NotADirectory { .. } => "not-a-folder",
            VolumeError::NotAbsolute { .. } => "not-absolute",
            VolumeError::Probe { .. } => "probe-failed",
            VolumeError::NotAnswering { .. } => "not-answering",
            VolumeError::Unsupported => "unsupported",
        }
    }

    /// The first line of the message.
    pub fn headline(&self) -> String {
        match self {
            VolumeError::NotMounted {
                expected,
                elsewhere: Some(_),
                ..
            } => format!("{} mounted somewhere else", expected.drive()),
            VolumeError::NotMounted { expected, .. } => format!("{} not mounted", expected.drive()),
            VolumeError::WrongVolume { expected, .. } => {
                format!("Wrong drive at {}", expected.mount_point)
            }
            VolumeError::ReadOnly { expected, .. } => format!("{} is read-only", expected.drive()),
            VolumeError::Missing { role, .. } => format!("{role} folder missing"),
            VolumeError::NotADirectory { role, .. } => format!("{role} is not a folder"),
            VolumeError::OnStartupDisk { .. } => "Drive not mounted".to_string(),
            VolumeError::NotAbsolute { path } => format!("{path} is not an absolute path"),
            VolumeError::Probe { .. } => "Could not check the volume".to_string(),
            VolumeError::NotAnswering { .. } => "Could not check the volume".to_string(),
            VolumeError::Unsupported => "Volume checks are not supported here yet".to_string(),
        }
    }

    /// The explanation under the headline, unwrapped.
    pub fn detail(&self) -> String {
        match self {
            VolumeError::NotMounted {
                role,
                path,
                expected,
                found_on,
                elsewhere,
                windows_letter,
            } => {
                let now = match found_on {
                    Some(volume) => format!(
                        "{} is currently a folder on {volume}.",
                        expected.mount_point
                    ),
                    None => format!("{} does not exist.", expected.mount_point),
                };
                match elsewhere {
                    Some(actual) => format!(
                        "{role} {path} expects {} at {}, but that drive is mounted at {actual}. {now}",
                        expected.describe(),
                        expected.mount_point
                    ),
                    None => format!(
                        "{role} {path} expects {}. {now} {}",
                        expected.describe(),
                        plug_in(&expected.mount_point, *windows_letter, "run again")
                    ),
                }
            }
            VolumeError::WrongVolume {
                role,
                path,
                expected,
                found,
            } => format!(
                "{role} {path} expects {}, but {} is {}. Plug in the right drive and run again.",
                expected.describe(),
                expected.mount_point,
                found.describe()
            ),
            VolumeError::ReadOnly { role, path, expected } => format!(
                "{role} {path} is on {}, which is mounted read-only, so OneFolder cannot write to it.",
                expected.mount_point
            ),
            VolumeError::Missing { role, path, expected } => format!(
                "{path} does not exist on {}. Create it, or edit the job to point at the right folder.{}",
                expected.describe(),
                if *role == Role::Destination {
                    ""
                } else {
                    " A source must exist before it can be synced."
                }
            ),
            VolumeError::NotADirectory { path, .. } => format!("{path} is a file, not a folder."),
            VolumeError::OnStartupDisk { path, mount_point } => match host::ui_os() {
                host::Os::Mac => format!(
                    "{path} is under /Volumes, but it is a folder on the startup disk ({mount_point}), \
                     not on a mounted drive. Plug the drive in and try again."
                ),
                _ => format!(
                    "{path} is a folder on the system disk, not on a mounted drive. {}",
                    plug_in(
                        &host::drive_path_on(
                            host::ui_os(),
                            host::user_name().as_deref(),
                            Path::new(path)
                        )
                        .map(|d| d.mount_point)
                            .unwrap_or_default(),
                        None,
                        "try again"
                    )
                ),
            },
            VolumeError::NotAbsolute { .. } => "Give the full path, starting with /.".to_string(),
            VolumeError::Probe { path, message } => format!("{path}: {message}"),
            VolumeError::NotAnswering { path, limit } => {
                format!("{path} {}", not_answering_words(*limit))
            }
            VolumeError::Unsupported => {
                "OneFolder can record and check external drives on macOS and Linux only."
                    .to_string()
            }
        }
    }
}

impl VolumeError {
    /// What to do about a drive mounted somewhere else: one action, which
    /// points the job at the place the drive is mounted now (the edit
    /// records the drive there). `None` for any other error.
    pub fn hint(&self, job: &str) -> Option<String> {
        let VolumeError::NotMounted {
            role,
            path,
            expected,
            elsewhere: Some(actual),
            ..
        } = self
        else {
            return None;
        };
        let moved = Path::new(path)
            .strip_prefix(&expected.mount_point)
            .map(|rest| Path::new(actual).join(rest).display().to_string())
            .unwrap_or_else(|_| actual.clone());
        Some(match role {
            Role::Destination => format!(
                "To use the drive where it is, change the destination's path to {moved} \
                 with: onefolder jobs edit {job} (the drive is recorded again at the new path)."
            ),
            Role::Source => format!(
                "To use the drive where it is, point the job's source at it with: \
                 onefolder jobs edit {job} --source \"{moved}\""
            ),
        })
    }

    /// The whole message for a run: the headline, the explanation and any
    /// hint, wrapped and indented as [`fmt::Display`] does.
    pub fn message(&self, job: &str) -> String {
        let mut text = self.to_string();
        if let Some(hint) = self.hint(job) {
            for line in wrap(&hint, 64) {
                text.push_str("\n  ");
                text.push_str(&line);
            }
        }
        text
    }
}

/// How serious a volume state is, for `doctor` and `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    /// Worth a look: an unplugged drive, a folder still to be created.
    Warning,
    /// Needs fixing: the wrong drive, a read-only one.
    Problem,
    /// Not checked here.
    Skipped,
}

/// The short state of a checked volume, in the words `doctor` and `status`
/// both use: `mounted · volume "Backup"`, `not mounted (expects volume
/// "Backup")`, `wrong volume (...)`, `mounted read-only`...
pub fn state_line(result: &Result<(), VolumeError>, expected: &Identity) -> (Severity, String) {
    let expects = if expected.volume_name.is_empty() {
        expected.describe()
    } else {
        format!("volume \"{}\"", expected.volume_name)
    };
    let e = match result {
        Ok(()) if expected.is_weak() => {
            return (
                Severity::Ok,
                format!(
                    "mounted · volume \"{}\" (no UUID, device and name checked)",
                    expected.volume_name
                ),
            )
        }
        Ok(()) => {
            return (
                Severity::Ok,
                format!("mounted · volume \"{}\"", expected.volume_name),
            )
        }
        Err(e) => e,
    };
    match e {
        VolumeError::NotMounted {
            elsewhere: Some(actual),
            ..
        } => (
            Severity::Problem,
            format!(
                "not mounted at {}, the drive is at {actual} (expects {expects})",
                expected.mount_point
            ),
        ),
        VolumeError::NotMounted { .. } | VolumeError::OnStartupDisk { .. } => (
            Severity::Warning,
            format!("not mounted (expects {expects})"),
        ),
        VolumeError::WrongVolume { found, .. } => (
            Severity::Problem,
            format!(
                "wrong volume (expects {}, found {})",
                expected.describe(),
                found.describe()
            ),
        ),
        VolumeError::ReadOnly { .. } => (Severity::Problem, "mounted read-only".into()),
        VolumeError::Missing { .. } => (
            Severity::Warning,
            format!("mounted, but the folder is missing (on {expects})"),
        ),
        VolumeError::NotADirectory { .. } => (Severity::Problem, "not a folder".into()),
        VolumeError::Unsupported => (Severity::Skipped, "volume check not supported here".into()),
        VolumeError::NotAnswering { limit, .. } => (Severity::Problem, not_answering_words(*limit)),
        other => (
            Severity::Problem,
            format!("could not check: {}", other.detail()),
        ),
    }
}

/// The state of a local folder with no drive recorded (on the startup
/// disk), in the words `doctor` and `status` both use, with a short code:
/// `no-drive` when the folder is there, `folder-missing`, `not-a-folder`
/// or `probe-failed`.
pub fn folder_line(path: &Path) -> (Severity, String, &'static str) {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => (
            Severity::Ok,
            "folder ok · no drive to check".into(),
            "no-drive",
        ),
        Ok(_) => (Severity::Problem, "not a folder".into(), "not-a-folder"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (
            Severity::Warning,
            "folder missing · no drive to check".into(),
            "folder-missing",
        ),
        Err(e) => (
            Severity::Problem,
            format!("not readable: {e}"),
            "probe-failed",
        ),
    }
}

impl fmt::Display for VolumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.headline())?;
        for line in wrap(&self.detail(), 64) {
            write!(f, "\n  {line}")?;
        }
        Ok(())
    }
}

/// What to do about a drive that is not there: plug it in, and under WSL,
/// where a drive plugged in after WSL started is not mounted on its own,
/// mount it at the recorded `/mnt/<letter>`.
fn plug_in(mount_point: &str, windows_letter: Option<char>, then: &str) -> String {
    if host::ui_os() == host::Os::Wsl {
        if let Some(letter) = windows_letter {
            return format!(
                "Windows has the drive as {letter}:, but WSL has not mounted it; {}. Then {then}.",
                host::wsl_mount_hint(letter, mount_point)
            );
        }
        if let Some(letter) = host::wsl_drive_letter(mount_point) {
            return format!(
                "Plug the drive in and {}. Then {then}.",
                host::wsl_mount_hint(letter, mount_point)
            );
        }
    }
    format!("Plug the drive in and {then}.")
}

/// Greedy word wrap.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// True when an absolute path lies where drives are mounted (`/Volumes`
/// on macOS, `/media`, `/run/media` or `/mnt` on Linux; see
/// [`crate::platform::drive_path_on`]), after resolving `.` and `..`
/// lexically; `/Volumes` ignoring case (APFS and HFS+ are usually case
/// insensitive, so `/VOLUMES/Backup` is the same place).
pub fn is_under_drives(path: &Path) -> bool {
    host::drive_path(path).is_some()
}

/// The mount point of the drive a drive path is on: `/Volumes/<name>`,
/// `/media/<user>/<name>`...
pub fn drive_mount_point(path: &Path) -> Option<String> {
    host::drive_path(path).map(|d| d.mount_point)
}

/// True for the system disk's mount points, which are never recorded
/// (see [`crate::platform::is_system_mount_on`]): `/` and
/// `/System/Volumes/...` on macOS (a folder at `/Volumes/Backup` with no
/// drive plugged in reports `/System/Volumes/Data`); anything not under a
/// drive root on Linux, and `/mnt/c` under WSL.
pub fn is_startup(mount_point: &str, fstype: &str) -> bool {
    host::is_system_mount(mount_point, fstype)
}

/// The closest existing ancestor of `path`, itself included.
fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.symlink_metadata().is_ok())
        .map(Path::to_path_buf)
}

/// A statfs failure: "unsupported" on platforms without an implementation.
fn io_error(path: &Path, e: io::Error) -> VolumeError {
    if e.kind() == io::ErrorKind::Unsupported {
        VolumeError::Unsupported
    } else {
        probe(path, e)
    }
}

fn probe(path: &Path, e: impl fmt::Display) -> VolumeError {
    VolumeError::Probe {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

/// How long a volume check or lookup may take. statfs, getfsstat and
/// `stat` block without a limit on a network mount whose server stopped
/// answering, so the work runs on its own thread and is abandoned after
/// this long.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// The limit of a drive check under WSL, where naming a Windows drive
/// starts PowerShell, which can take many seconds when Windows is busy.
pub const WSL_DEADLINE: Duration = Duration::from_secs(30);

/// How long a drive check (identify, check, check_creatable) may take:
/// [`DEADLINE`], or [`WSL_DEADLINE`] under WSL.
pub fn check_deadline() -> Duration {
    if host::os() == host::Os::Wsl {
        WSL_DEADLINE
    } else {
        DEADLINE
    }
}

/// The words for a check that ran out of time: under WSL's longer limit,
/// PowerShell is the likely cause.
fn not_answering_words(limit: Duration) -> String {
    if limit > DEADLINE {
        format!(
            "PowerShell did not answer in {} seconds (or a network drive is hanging)",
            limit.as_secs()
        )
    } else {
        format!(
            "did not answer in {} seconds, is a network drive hanging?",
            DEADLINE.as_secs()
        )
    }
}

/// Runs `work` on a thread of its own with a small runtime (diskutil is a
/// subprocess), and gives up after `limit`. A thread stuck in a system
/// call cannot be stopped: it is left behind, and ends with the process.
pub async fn within<T, F, Fut>(path: &Path, limit: Duration, work: F) -> Result<T, VolumeError>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, VolumeError>>,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let shown = path.display().to_string();
    let spawned = std::thread::Builder::new()
        .name("volume-check".into())
        .spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(work()),
                Err(e) => Err(VolumeError::Probe {
                    path: shown,
                    message: e.to_string(),
                }),
            };
            let _ = tx.send(result);
        });
    if let Err(e) = spawned {
        return Err(probe(path, e));
    }
    match tokio::time::timeout(limit, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(probe(path, "the check stopped unexpectedly")),
        Err(_) => Err(VolumeError::NotAnswering {
            path: path.display().to_string(),
            limit,
        }),
    }
}

/// Runs blocking file system work (`stat`, `read_dir`, following
/// symlinks) on a thread of its own, within `limit`; see [`within`].
pub async fn guarded<T, F>(path: &Path, limit: Duration, work: F) -> Result<T, VolumeError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    within(path, limit, move || async move { Ok(work()) }).await
}

/// [`guarded`] for code that is not async: waits on a channel instead.
pub fn guarded_blocking<T, F>(path: &Path, limit: Duration, work: F) -> Result<T, VolumeError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("volume-check".into())
        .spawn(move || {
            let _ = tx.send(work());
        });
    if let Err(e) = spawned {
        return Err(probe(path, e));
    }
    match rx.recv_timeout(limit) {
        Ok(value) => Ok(value),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(VolumeError::NotAnswering {
            path: path.display().to_string(),
            limit,
        }),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(probe(path, "the check stopped unexpectedly"))
        }
    }
}

/// How long the quick mount look of the list screens may take.
pub const QUICK_DEADLINE: Duration = Duration::from_secs(2);

/// Finds the volume that holds `path` (see [`identify_now`]), within
/// [`DEADLINE`].
pub async fn identify(path: &Path) -> Result<Found, VolumeError> {
    let owned = path.to_path_buf();
    within(path, check_deadline(), move || async move {
        identify_now(&owned).await
    })
    .await
}

/// The checks before a run (see [`check_now`]), within [`DEADLINE`].
pub async fn check(path: &Path, expected: &Identity, needs_write: bool) -> Result<(), VolumeError> {
    let owned = path.to_path_buf();
    let expected = expected.clone();
    within(path, check_deadline(), move || async move {
        check_now(&owned, &expected, needs_write).await
    })
    .await
}

/// Where a path really leads: each symlink along it followed (relative
/// targets against their folder), `.` and `..` resolved, and the parts
/// that do not exist yet kept as they are. A dangling symlink, such as one
/// into an unplugged drive, still resolves to its target. Gives up on a
/// loop after 40 links and returns what it has.
pub fn resolve(path: &Path) -> PathBuf {
    let mut pending: Vec<std::ffi::OsString> = path
        .components()
        .rev()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p.to_os_string()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect();
    let mut out = PathBuf::from("/");
    let mut links = 0;
    while let Some(part) = pending.pop() {
        if part == ".." {
            out.pop();
            continue;
        }
        let next = out.join(&part);
        match std::fs::read_link(&next) {
            Ok(target) if links < 40 => {
                links += 1;
                if target.is_absolute() {
                    out = PathBuf::from("/");
                }
                for c in target.components().rev() {
                    match c {
                        Component::Normal(p) => pending.push(p.to_os_string()),
                        Component::ParentDir => pending.push("..".into()),
                        _ => {}
                    }
                }
            }
            _ => out = next,
        }
    }
    out
}

/// True when a path is on a drive path (see [`is_under_drives`]) as
/// written, or once its symlinks are followed.
pub fn leads_onto_drive(path: &Path) -> bool {
    is_under_drives(path) || is_under_drives(&resolve(path))
}

/// What setup learns about the volume that holds a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The identity to record; `None` on the startup disk, which is never
    /// recorded (its data volume UUID changes with a reinstall or a
    /// migration, and a home folder cannot be unplugged).
    pub identity: Option<Identity>,
    pub mount_point: String,
    /// Mounted read-only right now (a destination could not be written).
    pub read_only: bool,
}

/// Network file systems, which diskutil and lsblk do not describe, for
/// which the mount's device and type become a weak identity: macOS's
/// names, then Linux's.
const NETWORK_FSTYPES: &[&str] = &[
    "smbfs",
    "afpfs",
    "nfs",
    "webdav",
    "cifs",
    "smb3",
    "nfs4",
    "fuse.sshfs",
    "davfs",
    "fuse.rclone",
];

/// Finds the volume that holds `path`. The path need not exist yet: the
/// closest existing parent is measured instead. Only a volume other than
/// the startup disk gets an identity. On Linux the system disk is not
/// probed at all: it is never recorded, and its mount (a btrfs subvolume,
/// an overlay, a tmpfs `/tmp`) need not be a block device lsblk describes.
async fn identify_now(path: &Path) -> Result<Found, VolumeError> {
    if host::os() != host::Os::Mac {
        let mount = locate(path)?;
        if is_startup(&mount.mount_point, &mount.fstype) {
            return Ok(Found {
                identity: None,
                read_only: mount.read_only,
                mount_point: mount.mount_point,
            });
        }
    }
    let (identity, mount, writable) = identify_any(path).await?;
    let startup = is_startup(&mount.mount_point, &mount.fstype);
    Ok(Found {
        identity: (!startup).then_some(identity),
        read_only: mount.read_only || writable == Some(false),
        mount_point: mount.mount_point,
    })
}

/// Like [`identify`], but gives the startup disk's identity too (tests use
/// it to check against a real volume), with the mount and whether diskutil
/// reports it writable.
pub(crate) async fn identify_any(
    path: &Path,
) -> Result<(Identity, Mount, Option<bool>), VolumeError> {
    let mount = locate(path)?;
    let (seen, writable) = seen(&mount, false).await?;
    Ok((recordable(&seen), mount, writable))
}

/// The mount that holds `path`, or where it leads: a symlink is followed
/// even when its target does not exist yet (an unplugged drive), since
/// that is the volume to record. A drive path that lies on the startup
/// disk is refused.
fn locate(path: &Path) -> Result<Mount, VolumeError> {
    if !path.is_absolute() {
        return Err(VolumeError::NotAbsolute {
            path: path.display().to_string(),
        });
    }
    let real = resolve(path);
    let existing = existing_ancestor(&real).ok_or_else(|| probe(path, "no part of it exists"))?;
    let mount = platform::statfs(&existing).map_err(|e| io_error(&existing, e))?;
    if is_under_drives(&real) && is_startup(&mount.mount_point, &mount.fstype) {
        // The place the message is about is where the path leads.
        return Err(VolumeError::OnStartupDisk {
            path: real.display().to_string(),
            mount_point: mount.mount_point,
        });
    }
    Ok(mount)
}

/// Everything known about the volume mounted at `mount`: every proof
/// diskutil (or lsblk, or Windows) reports, or, for a network volume they
/// cannot describe (unless `need_diskutil`), the mount's own fallback.
/// Also says whether the volume is reported writable.
async fn seen(mount: &Mount, need_diskutil: bool) -> Result<(Identity, Option<bool>), VolumeError> {
    match platform::disk_info(&mount.mount_point).await {
        Ok(info) => {
            // A drive with no label (some Linux file systems, device-mapper
            // volumes) is named after its mount point, so the weakest proof,
            // device and name, still has a name to compare.
            let volume_name = match info.volume_name.clone().filter(|n| !n.is_empty()) {
                Some(name) => name,
                None if host::os() != host::Os::Mac => mount_name(mount),
                None => String::new(),
            };
            let identity = Identity {
                mount_point: mount.mount_point.clone(),
                volume_name,
                volume_uuid: info.volume_uuid.clone().filter(|u| !u.is_empty()),
                partition_uuid: info.disk_uuid.clone().filter(|u| !u.is_empty()),
                device: Some(
                    info.device_node
                        .clone()
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(|| mount.device.clone()),
                ),
                mount_root: mount.root.clone(),
            };
            Ok((identity, info.writable))
        }
        Err(_) if !need_diskutil && is_network(mount) => Ok((network_identity(mount), None)),
        Err(e) => Err(probe(Path::new(&mount.mount_point), e)),
    }
}

/// True for a mount that only a weak `<fstype>:<source>` identity can
/// describe: a network share, and on Linux any file system whose source is
/// not a block device (a ZFS dataset, mergerfs, a FUSE file system), which
/// lsblk cannot describe. A Windows drive under WSL (`E:\`) is not one: Windows
/// names it, and when it cannot, the check fails rather than weakens.
fn is_network(mount: &Mount) -> bool {
    // Under WSL a share can be mounted with drvfs too (`\\nas\share`).
    let windows_share =
        matches!(mount.fstype.as_str(), "drvfs" | "9p") && mount.device.starts_with("\\\\");
    let drive_letter = {
        let mut chars = mount.device.chars();
        matches!((chars.next(), chars.next()), (Some(c), Some(':')) if c.is_ascii_alphabetic())
    };
    let not_block = host::os() != host::Os::Mac
        && !mount.device.starts_with("/dev/")
        && !drive_letter
        && !matches!(mount.fstype.as_str(), "drvfs" | "9p");
    !is_startup(&mount.mount_point, &mount.fstype)
        && (NETWORK_FSTYPES.contains(&mount.fstype.as_str()) || windows_share || not_block)
}

/// The last part of a mount point, `Backup` for `/media/bora/Backup`.
fn mount_name(mount: &Mount) -> String {
    Path::new(&mount.mount_point)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The weak identity of a network volume: `<fstype>:<share>` and the
/// name the share is mounted under.
fn network_identity(mount: &Mount) -> Identity {
    Identity {
        mount_point: mount.mount_point.clone(),
        volume_name: mount_name(mount),
        volume_uuid: None,
        partition_uuid: None,
        device: Some(format!("{}:{}", mount.fstype, mount.device)),
        mount_root: mount.root.clone(),
    }
}

/// Keeps only the strongest proof of a seen volume, as the config records it.
fn recordable(seen: &Identity) -> Identity {
    let mut id = seen.clone();
    if id.volume_uuid.is_some() {
        id.partition_uuid = None;
        id.device = None;
    } else if id.partition_uuid.is_some() {
        id.device = None;
    }
    // `/`, the whole file system, is what a missing root means.
    id.mount_root = id.mount_root.filter(|r| r != "/");
    id
}

/// True when the volume seen now is the recorded one, by the strongest
/// proof that was recorded.
fn same_volume(expected: &Identity, seen: &Identity) -> bool {
    let eq = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    // Where the mount says which folder of the file system it shows (Linux),
    // it must be the recorded one.
    if let Some(root) = &seen.mount_root {
        if expected.mount_root.as_deref().unwrap_or("/") != root {
            return false;
        }
    }
    if expected.volume_uuid.is_some() {
        eq(&expected.volume_uuid, &seen.volume_uuid)
    } else if expected.partition_uuid.is_some() {
        eq(&expected.partition_uuid, &seen.partition_uuid)
    } else {
        expected.device.is_some()
            && expected.device == seen.device
            && expected.volume_name == seen.volume_name
    }
}

/// The four checks from DESIGN.md:
///
/// 1. `statfs` on the recorded mount point reports it as a mount point of
///    its own (which rules out `/` and the startup disk when a drive was
///    recorded);
/// 2. the volume mounted there is the recorded one: diskutil reports the
///    recorded UUID, partition UUID, or device node and name;
/// 3. the path exists, is a directory, and is on that mount;
/// 4. the volume is not read-only, when `needs_write`.
///
/// The identity is compared before the folder is looked for, so a drive of
/// the same name mounted in its place is "wrong volume", never "folder
/// missing", and an unplugged drive is "not mounted".
async fn check_now(path: &Path, expected: &Identity, needs_write: bool) -> Result<(), VolumeError> {
    let role = Role::from_write(needs_write);
    let shown = path.display().to_string();
    let not_mounted = |found_on, (elsewhere, windows_letter): (Option<String>, Option<char>)| {
        VolumeError::NotMounted {
            role,
            path: shown.clone(),
            expected: Box::new(expected.clone()),
            found_on,
            elsewhere,
            windows_letter,
        }
    };

    // 1. The recorded mount point is a mount point of its own right now.
    // When it is not, the drive may still be plugged in and mounted under
    // another name.
    let mount_point = Path::new(&expected.mount_point);
    let mount = match platform::statfs(mount_point) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(not_mounted(None, mounted_elsewhere(expected).await));
        }
        Err(e) => return Err(io_error(mount_point, e)),
        Ok(m) if m.mount_point != expected.mount_point => {
            let label = volume_label(&m).await;
            return Err(not_mounted(Some(label), mounted_elsewhere(expected).await));
        }
        Ok(m) => m,
    };

    // 2. It is the recorded volume. A UUID needs diskutil's word for it.
    let (seen, writable) = seen(&mount, !expected.is_weak()).await?;
    if !same_volume(expected, &seen) {
        return Err(VolumeError::WrongVolume {
            role,
            path: shown,
            expected: Box::new(expected.clone()),
            found: Box::new(recordable(&seen)),
        });
    }

    // 3. The folder exists, is a directory, and lives on that mount (a
    // symlink could lead elsewhere).
    match std::fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(VolumeError::Missing {
                role,
                path: shown,
                expected: Box::new(expected.clone()),
            })
        }
        Err(e) => return Err(probe(path, e)),
        Ok(m) if !m.is_dir() => return Err(VolumeError::NotADirectory { role, path: shown }),
        Ok(_) => {}
    }
    let on = platform::statfs(path).map_err(|e| io_error(path, e))?;
    if on.mount_point != expected.mount_point {
        return Err(not_mounted(Some(volume_label(&on).await), (None, None)));
    }

    // 4. Writable, for a destination.
    if needs_write && (mount.read_only || on.read_only || writable == Some(false)) {
        return Err(VolumeError::ReadOnly {
            role,
            path: shown,
            expected: Box::new(expected.clone()),
        });
    }
    Ok(())
}

/// For a destination folder that does not exist yet on a drive [`check`]
/// has verified (it returned [`VolumeError::Missing`]): the nearest
/// existing ancestor must be a folder on the recorded mount (a symlinked
/// parent could lead elsewhere, such as the startup disk), and that volume
/// must be writable, so the copy creates the folder on the right drive.
pub async fn check_creatable(path: &Path, expected: &Identity) -> Result<(), VolumeError> {
    let owned = path.to_path_buf();
    let expected = expected.clone();
    within(path, check_deadline(), move || async move {
        check_creatable_now(&owned, &expected).await
    })
    .await
}

async fn check_creatable_now(path: &Path, expected: &Identity) -> Result<(), VolumeError> {
    let shown = path.display().to_string();
    let Some(ancestor) = path.ancestors().skip(1).find(|p| p.exists()) else {
        return Err(VolumeError::Missing {
            role: Role::Destination,
            path: shown,
            expected: Box::new(expected.clone()),
        });
    };
    match std::fs::metadata(ancestor) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(VolumeError::NotADirectory {
                role: Role::Destination,
                path: ancestor.display().to_string(),
            })
        }
        Err(e) => return Err(probe(ancestor, e)),
    }
    // statfs follows symlinks, so this is where the folder would land.
    let on = platform::statfs(ancestor).map_err(|e| io_error(ancestor, e))?;
    if on.mount_point != expected.mount_point {
        return Err(VolumeError::NotMounted {
            role: Role::Destination,
            path: shown,
            expected: Box::new(expected.clone()),
            found_on: Some(volume_label(&on).await),
            elsewhere: None,
            windows_letter: None,
        });
    }
    if on.read_only {
        return Err(VolumeError::ReadOnly {
            role: Role::Destination,
            path: shown,
            expected: Box::new(expected.clone()),
        });
    }
    Ok(())
}

/// Looks for the recorded drive among the volumes mounted right now, other
/// than at its recorded mount point: macOS mounts a drive at
/// `/Volumes/Backup 1` when a folder is in the way at `/Volumes/Backup`,
/// udisks at `/media/<user>/Backup1`, and a disk image can be attached
/// anywhere. Gives the mount point it is at.
///
/// A drive with a UUID costs one `diskutil info -plist <UUID>` (on Linux
/// one `lsblk -J` of every device, and under WSL a `Get-Volume` when that
/// finds nothing), which names the mount point directly (an unknown UUID
/// is an error). A drive recorded by device node only is looked for in
/// the mount table with no process spawned; network shares are never
/// matched that way.
pub async fn mounted_elsewhere(expected: &Identity) -> (Option<String>, Option<char>) {
    let uuid = expected
        .volume_uuid
        .as_deref()
        .or(expected.partition_uuid.as_deref());
    if let Some(uuid) = uuid {
        let Ok(info) = platform::disk_info(uuid).await else {
            return (None, None);
        };
        let seen = Identity {
            mount_point: info.mount_point.clone().unwrap_or_default(),
            volume_name: info.volume_name.clone().unwrap_or_default(),
            volume_uuid: info.volume_uuid.clone(),
            partition_uuid: info.disk_uuid.clone(),
            device: info.device_node.clone(),
            mount_root: None,
        };
        if !same_volume(expected, &seen) {
            return (None, None);
        }
        return match info.mount_point.filter(|m| !m.is_empty()) {
            Some(m) if m != expected.mount_point => (Some(m), None),
            Some(_) => (None, None),
            // Windows has it (a drive letter) but WSL mounted it nowhere.
            None => (None, windows_letter(info.device_node.as_deref())),
        };
    }
    let Some(device) = expected.device.as_deref() else {
        return (None, None);
    };
    let found = platform::mounts().ok().and_then(|all| {
        all.into_iter()
            .find(|m| {
                m.mount_point != expected.mount_point
                    && !is_startup(&m.mount_point, &m.fstype)
                    && !NETWORK_FSTYPES.contains(&m.fstype.as_str())
                    && m.device == device
            })
            .map(|m| m.mount_point)
    });
    (found, None)
}

/// The letter of a Windows device `F:`.
fn windows_letter(device: Option<&str>) -> Option<char> {
    let mut chars = device?.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(c), Some(':'), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase()),
        _ => None,
    }
}

/// A quick, spawn-free look for list screens: `Some(true)` when the
/// recorded mount point is currently a mount point, `None` when statfs did
/// not answer within [`QUICK_DEADLINE`] (a hung network drive). Does not
/// prove the UUID; [`check`] does.
pub fn mount_state(expected: &Identity) -> Option<bool> {
    let mount_point = expected.mount_point.clone();
    guarded_blocking(
        Path::new(&expected.mount_point),
        QUICK_DEADLINE,
        move || {
            platform::statfs(Path::new(&mount_point)).is_ok_and(|m| m.mount_point == mount_point)
        },
    )
    .ok()
}

/// Whether a path leads onto a drive (see [`leads_onto_drive`]) and
/// where it really leads, within [`DEADLINE`]. A path that does not answer
/// counts as leading onto a drive, so nothing is assumed about it.
pub async fn leads_where(path: &Path) -> (bool, PathBuf, Option<VolumeError>) {
    let owned = path.to_path_buf();
    match guarded(path, DEADLINE, move || {
        (leads_onto_drive(&owned), resolve(&owned))
    })
    .await
    {
        Ok((leads, real)) => (leads, real, None),
        Err(e) => (true, path.to_path_buf(), Some(e)),
    }
}

/// How a mount is named in messages: the volume name from diskutil, with
/// the startup disk called by the name of `/` ("Macintosh HD"). On Linux
/// the system disk is "the system disk" (its label says little), and
/// another system mount says which it is: "/home, part of the system disk".
async fn volume_label(mount: &Mount) -> String {
    let mount_point = mount.mount_point.as_str();
    let startup = is_startup(mount_point, &mount.fstype);
    if startup && host::os() != host::Os::Mac {
        return if mount_point == "/" {
            host::words().system_disk.to_string()
        } else {
            format!("{mount_point}, part of the system disk")
        };
    }
    let target = if startup { "/" } else { mount_point };
    match platform::disk_info(target).await {
        Ok(DiskInfo {
            volume_name: Some(name),
            ..
        }) if !name.is_empty() => name,
        _ if startup => host::words().system_disk.to_string(),
        _ => mount_point.to_string(),
    }
}

/// Space on the file system holding `path`, from `statvfs(3)`: bytes
/// free to an ordinary user (available blocks) and bytes in use (all
/// blocks less the free ones), each times the fragment size. For the
/// folder browser: the free space in its title, and the used space of a
/// drive mounted inside the folder shown, in place of walking it. Can
/// wait on a slow drive, so call it off the key loop.
#[cfg(unix)]
pub fn space(path: &Path) -> io::Result<Space> {
    use nix::errno::Errno;
    use nix::libc;
    use nix::NixPath;
    let mut buf = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    let res = path
        .with_nix_path(|p| {
            // SAFETY: `p` is a NUL-terminated path that outlives the call and
            // `buf` points to writable memory the size of `struct statvfs`.
            unsafe { libc::statvfs(p.as_ptr(), buf.as_mut_ptr()) }
        })
        .map_err(io::Error::from)?;
    Errno::result(res).map_err(io::Error::from)?;
    // SAFETY: the buffer started zeroed and the call succeeded, so the
    // kernel filled it in.
    let st = unsafe { buf.assume_init() };
    #[allow(
        clippy::unnecessary_cast,
        reason = "the field types differ by platform"
    )]
    let (frsize, blocks, bfree, bavail) = (
        st.f_frsize as u64,
        st.f_blocks as u64,
        st.f_bfree as u64,
        st.f_bavail as u64,
    );
    Ok(Space {
        free: bavail.saturating_mul(frsize),
        used: blocks.saturating_sub(bfree).saturating_mul(frsize),
    })
}

#[cfg(not(unix))]
pub fn space(_path: &Path) -> io::Result<Space> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "free space is known on Unix only",
    ))
}

/// File systems known to tell names apart by case on Linux. Every other
/// one (FAT, exFAT, NTFS, HFS+, network shares such as CIFS, NFS and sshfs
/// whose server may ignore case, FUSE, Windows drives under WSL, anything
/// unknown) is taken to ignore case, the safe side: a case-only rename on
/// a destination that ignores case is the same file, and moving it onto
/// itself would delete it.
const CASE_SENSITIVE_FSTYPES: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "jfs", "tmpfs", "overlay", "zfs", "wslfs",
    "lxfs",
];

/// Of those, the ones where a folder can be made case-insensitive
/// (`chattr +F`, the casefold flag).
const CASEFOLD_FSTYPES: &[&str] = &["ext4", "f2fs"];

/// Whether a folder ignores case on a platform: every local folder on
/// macOS (APFS and HFS+ drives as macOS formats them, and rclone's local
/// backend there); on Linux every file system but the known
/// case-sensitive ones, and on ext4 and f2fs a folder with the casefold
/// flag. `casefolded` is that flag, `None` when it could not be read
/// (which counts as ignoring case).
pub fn fstype_ignores_case(os: host::Os, fstype: &str, casefolded: Option<bool>) -> bool {
    if os == host::Os::Mac || !CASE_SENSITIVE_FSTYPES.contains(&fstype) {
        return true;
    }
    if CASEFOLD_FSTYPES.contains(&fstype) {
        return casefolded != Some(false);
    }
    false
}

/// Whether the folder at `path` (or, when it does not exist yet, the
/// closest folder above it) ignores the case of names, for move
/// detection. When the mount cannot be read in time, case is taken to be
/// ignored, which never plans a move by case alone.
pub async fn ignores_case(path: &Path) -> bool {
    if host::os() == host::Os::Mac {
        return true;
    }
    let owned = path.to_path_buf();
    guarded(path, DEADLINE, move || {
        let real = resolve(&owned);
        let Some(folder) = existing_ancestor(&real) else {
            return true;
        };
        match platform::statfs(&folder) {
            Ok(m) => {
                let casefolded = if CASEFOLD_FSTYPES.contains(&m.fstype.as_str()) {
                    casefolded(&folder)
                } else {
                    None
                };
                fstype_ignores_case(host::os(), &m.fstype, casefolded)
            }
            Err(_) => true,
        }
    })
    .await
    .unwrap_or(true)
}

/// The casefold flag of a folder (`FS_CASEFOLD_FL`, through the
/// `FS_IOC_GETFLAGS` ioctl), `None` when it cannot be read.
#[cfg(target_os = "linux")]
fn casefolded(folder: &Path) -> Option<bool> {
    use std::os::fd::AsRawFd;
    const FS_CASEFOLD_FL: nix::libc::c_int = 0x4000_0000;
    let dir = std::fs::File::open(folder).ok()?;
    let mut flags: nix::libc::c_int = 0;
    // SAFETY: `dir` is an open descriptor for the call's length, and the
    // kernel writes one int of flags into `flags`.
    let res = unsafe {
        nix::libc::ioctl(
            dir.as_raw_fd(),
            nix::libc::FS_IOC_GETFLAGS,
            &mut flags as *mut nix::libc::c_int,
        )
    };
    (res == 0).then_some(flags & FS_CASEFOLD_FL != 0)
}

#[cfg(not(target_os = "linux"))]
fn casefolded(_folder: &Path) -> Option<bool> {
    None
}

/// What [`space`] reports, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub free: u64,
    pub used: u64,
}

/// Stub for platforms without a volume implementation.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::{DiskInfo, Mount};
    use std::io;
    use std::path::Path;

    pub fn statfs(_path: &Path) -> io::Result<Mount> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "volume checks are supported on macOS and Linux only",
        ))
    }

    pub async fn disk_info(_mount_point: &str) -> Result<DiskInfo, String> {
        Err("volume checks are supported on macOS and Linux only".to_string())
    }

    pub fn mounts() -> io::Result<Vec<Mount>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "volume checks are supported on macOS and Linux only",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backup() -> Identity {
        Identity {
            mount_point: "/Volumes/Backup".into(),
            volume_name: "Backup".into(),
            volume_uuid: Some("A1B2C3D4-0000-4E5F-8A9B-000000000001".into()),
            partition_uuid: None,
            device: None,
            mount_root: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_followed_even_when_they_dangle() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        symlink("a/b", root.join("rel")).unwrap();
        symlink("/Volumes/OneFolderNoSuchDrive/Docs", root.join("abs")).unwrap();
        symlink("loop", root.join("loop")).unwrap();
        assert_eq!(resolve(&root.join("rel/c")), root.join("a/b/c"));
        assert_eq!(resolve(&root.join("rel/../x")), root.join("a/x"));
        assert_eq!(
            resolve(&root.join("abs/2026")),
            PathBuf::from("/Volumes/OneFolderNoSuchDrive/Docs/2026")
        );
        assert!(leads_onto_drive(&root.join("abs")));
        assert!(!leads_onto_drive(&root.join("rel")));
        assert!(leads_onto_drive(Path::new("/Volumes/Backup")));
        // A loop ends.
        let _ = resolve(&root.join("loop/x"));
        // Through a symlinked parent folder too.
        symlink(root.join("abs"), root.join("via")).unwrap();
        assert!(leads_onto_drive(&root.join("via/Sub")));
    }

    /// A symlink into /Volumes whose drive is not there is the drive not
    /// being mounted: nothing is recorded for the startup disk under it.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn identify_follows_a_symlink_onto_an_unplugged_drive() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("Backup");
        std::os::unix::fs::symlink("/Volumes/OneFolderNoSuchDrive/Docs", &link).unwrap();
        match identify(&link).await {
            Err(e @ VolumeError::OnStartupDisk { .. }) => {
                let detail = e.detail();
                assert!(
                    detail.starts_with("/Volumes/OneFolderNoSuchDrive/Docs is under /Volumes"),
                    "the link's target is named: {detail}"
                );
            }
            other => panic!("expected not mounted, got {other:?}"),
        }
        // A link to an ordinary folder is measured where it leads.
        let target = dir.path().join("real");
        std::fs::create_dir_all(&target).unwrap();
        let to_real = dir.path().join("to-real");
        std::os::unix::fs::symlink(&target, &to_real).unwrap();
        let a = identify(&to_real).await.unwrap();
        let b = identify(&target).await.unwrap();
        assert_eq!(a, b);
    }

    /// A check that blocks (a hung network mount) is abandoned after the
    /// limit, with words that say what probably happened.
    #[tokio::test]
    async fn a_check_that_hangs_is_given_up_on() {
        let started = std::time::Instant::now();
        let slow = within(
            Path::new("/Volumes/NAS/Backup"),
            Duration::from_millis(200),
            || async {
                // A blocking system call, as statfs on a hung mount is.
                std::thread::sleep(Duration::from_secs(3));
                Ok(())
            },
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        let e = slow.unwrap_err();
        assert_eq!(e.code(), "not-answering");
        assert_eq!(
            e.detail(),
            "/Volumes/NAS/Backup did not answer in 10 seconds, is a network drive hanging?"
        );
        let (severity, line) = state_line(&Err(e), &backup());
        assert_eq!(severity, Severity::Problem);
        assert_eq!(
            line,
            "did not answer in 10 seconds, is a network drive hanging?"
        );
        // The same for blocking code, such as the lists' quick look.
        let slow = guarded_blocking(
            Path::new("/Volumes/NAS"),
            Duration::from_millis(200),
            || {
                std::thread::sleep(Duration::from_secs(3));
                true
            },
        );
        assert_eq!(slow.unwrap_err().code(), "not-answering");
        let quick = guarded(Path::new("/"), Duration::from_secs(5), || 3).await;
        assert_eq!(quick.unwrap(), 3);
        // A quick one gives its own answer.
        let quick: Result<u8, VolumeError> =
            within(Path::new("/"), Duration::from_secs(5), || async { Ok(7) }).await;
        assert_eq!(quick.unwrap(), 7);
    }

    #[test]
    fn volumes_paths() {
        assert!(is_under_drives(Path::new("/Volumes/Backup/Docs")));
        assert!(is_under_drives(Path::new("/VOLUMES/Backup")));
        assert!(is_under_drives(Path::new("/private/../Volumes/Backup")));
        assert!(!is_under_drives(Path::new("/Volumes")));
        assert!(!is_under_drives(Path::new("/Volumesque/x")));
        assert_eq!(
            drive_mount_point(Path::new("/Volumes/Backup/Docs")).as_deref(),
            Some("/Volumes/Backup")
        );
        // Linux's drive roots count on Linux only.
        let linux = host::os() != host::Os::Mac;
        assert_eq!(is_under_drives(Path::new("/mnt/data/Docs")), linux);
        assert_eq!(is_under_drives(Path::new("/run/media/u/B/Docs")), linux);
    }

    #[test]
    fn linux_and_wsl_messages_name_the_system_disk_and_the_mount() {
        let on_root = VolumeError::OnStartupDisk {
            path: "/media/bora/Backup/Documents".into(),
            mount_point: "/".into(),
        };
        let mac = on_root.detail();
        assert!(
            mac.contains("is under /Volumes, but it is a folder on the startup disk"),
            "{mac}"
        );
        let linux = host::testing::describe(host::Os::Linux);
        assert_eq!(
            on_root.detail(),
            "/media/bora/Backup/Documents is a folder on the system disk, not on a mounted \
             drive. Plug the drive in and try again."
        );
        let wsl_drive = Identity {
            mount_point: "/mnt/e".into(),
            ..backup()
        };
        let unplugged = VolumeError::NotMounted {
            role: Role::Destination,
            path: "/mnt/e/Backup".into(),
            expected: Box::new(wsl_drive.clone()),
            found_on: Some("the system disk".into()),
            elsewhere: None,
            windows_letter: None,
        };
        assert!(
            unplugged.detail().ends_with(
                "/mnt/e is currently a folder on the system disk. Plug the drive in and run again."
            ),
            "{}",
            unplugged.detail()
        );
        drop(linux);
        let _wsl = host::testing::describe(host::Os::Wsl);
        assert!(
            unplugged.detail().ends_with(
                "Plug the drive in and mount it with: sudo mount -t drvfs E: /mnt/e. Then run again."
            ),
            "{}",
            unplugged.detail()
        );
        let on_root = VolumeError::OnStartupDisk {
            path: "/mnt/e/Backup".into(),
            mount_point: "/".into(),
        };
        assert_eq!(
            on_root.detail(),
            "/mnt/e/Backup is a folder on the system disk, not on a mounted drive. Plug the \
             drive in and mount it with: sudo mount -t drvfs E: /mnt/e. Then try again."
        );
    }

    #[test]
    fn startup_mount_points() {
        assert!(is_startup("/", "apfs"));
        assert!(!is_startup("/Volumes/Backup", "apfs"));
        // The rest of the rule is the platform's (see platform's tests).
        assert_eq!(
            is_startup("/System/Volumes/Data", "apfs"),
            host::os() == host::Os::Mac
        );
    }

    #[test]
    fn recorded_needs_a_proof() {
        let base = Recorded {
            volume_name: Some("Backup"),
            ..Recorded::default()
        };
        assert_eq!(Identity::recorded("/Volumes/Backup/Docs", base), None);
        let id = Identity::recorded(
            "/Volumes/Backup/Docs",
            Recorded {
                volume_uuid: Some("A1B2C3D4-0000-4E5F-8A9B-000000000001"),
                ..base
            },
        )
        .unwrap();
        assert_eq!(id, backup());
        let partition = Identity::recorded(
            "/Volumes/Backup/Docs",
            Recorded {
                partition_uuid: Some("C0FFEE00-1111-2222-3333-444444444444"),
                ..base
            },
        )
        .unwrap();
        assert!(!partition.is_weak());
        assert_eq!(
            partition.describe(),
            "volume \"Backup\" (partition UUID C0FFEE00-...)"
        );
        let weak = Identity::recorded(
            "/Volumes/BACKUP/x",
            Recorded {
                mount_point: Some("/Volumes/BACKUP"),
                volume_name: Some("BACKUP"),
                volume_uuid: Some(""),
                device: Some("/dev/disk4s1"),
                ..Recorded::default()
            },
        )
        .unwrap();
        assert!(weak.is_weak());
        assert_eq!(
            weak.describe(),
            "volume \"BACKUP\" (device /dev/disk4s1, no UUID)"
        );
    }

    fn mount(mount_point: &str, device: &str, fstype: &str) -> Mount {
        Mount {
            mount_point: mount_point.into(),
            device: device.into(),
            fstype: fstype.into(),
            read_only: false,
            root: None,
        }
    }

    #[test]
    fn network_volumes_get_a_weak_identity() {
        let m = mount(
            "/Volumes/Backup",
            "//bora@nas._smb._tcp.local/Backup",
            "smbfs",
        );
        assert!(is_network(&m));
        let id = network_identity(&m);
        assert!(id.is_weak());
        assert_eq!(id.volume_name, "Backup");
        assert_eq!(
            id.device.as_deref(),
            Some("smbfs://bora@nas._smb._tcp.local/Backup")
        );
        assert!(!is_network(&mount("/Volumes/X", "/dev/disk4s1", "exfat")));
        assert!(!is_network(&mount("/", "/dev/disk3s1s1", "nfs")));
        assert!(is_network(&mount(
            "/Volumes/s",
            "\\\\Mac\\winshare",
            "drvfs"
        )));
        assert!(!is_network(&mount("/Volumes/e", "E:\\", "drvfs")));
    }

    #[test]
    fn only_the_strongest_proof_is_recorded_and_compared() {
        let seen = Identity {
            mount_point: "/Volumes/X".into(),
            volume_name: "X".into(),
            volume_uuid: None,
            partition_uuid: Some("P-1".into()),
            device: Some("/dev/disk4s1".into()),
            mount_root: None,
        };
        let rec = recordable(&seen);
        assert_eq!(rec.partition_uuid.as_deref(), Some("P-1"));
        assert_eq!(rec.device, None);
        assert!(same_volume(&rec, &seen));
        // The partition UUID decides, even when the device node moved.
        let moved = Identity {
            device: Some("/dev/disk5s1".into()),
            ..seen.clone()
        };
        assert!(same_volume(&rec, &moved));
        let other = Identity {
            partition_uuid: Some("p-2".into()),
            ..seen.clone()
        };
        assert!(!same_volume(&rec, &other));
        // Device and name, for the weakest.
        let weak = Identity {
            partition_uuid: None,
            ..seen.clone()
        };
        assert!(same_volume(&weak, &weak));
        assert!(!same_volume(&weak, &moved));
        assert!(!same_volume(&backup(), &seen));
    }

    #[test]
    fn not_mounted_message_matches_the_mockup() {
        let e = VolumeError::NotMounted {
            role: Role::Destination,
            path: "/Volumes/Backup/Documents".into(),
            expected: Box::new(backup()),
            found_on: Some("Macintosh HD".into()),
            elsewhere: None,
            windows_letter: None,
        };
        assert_eq!(e.hint("documents"), None);
        assert_eq!(e.message("documents"), e.to_string());
        assert_eq!(
            e.to_string(),
            "Backup drive not mounted\n  \
             Destination /Volumes/Backup/Documents expects volume \"Backup\"\n  \
             (UUID A1B2C3D4-...). /Volumes/Backup is currently a folder on\n  \
             Macintosh HD. Plug the drive in and run again."
        );
        assert_eq!(e.code(), "not-mounted");
    }

    #[test]
    fn other_messages() {
        let wrong = VolumeError::WrongVolume {
            role: Role::Source,
            path: "/Volumes/Backup/Documents".into(),
            expected: Box::new(backup()),
            found: Box::new(Identity {
                volume_name: "Other".into(),
                volume_uuid: Some("FFFF0000-1".into()),
                ..backup()
            }),
        };
        let text = wrong.to_string();
        assert!(
            text.starts_with("Wrong drive at /Volumes/Backup\n  Source "),
            "{text}"
        );
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("is volume \"Other\" (UUID FFFF0000-...)"),
            "{text}"
        );
        let ro = VolumeError::ReadOnly {
            role: Role::Destination,
            path: "/Volumes/Backup/D".into(),
            expected: Box::new(backup()),
        };
        assert!(ro.to_string().starts_with("Backup drive is read-only\n"));
        assert!(!format!("{ro}{wrong}").contains('\u{2014}'), "no em-dashes");
    }

    #[test]
    fn a_drive_mounted_elsewhere_says_where_and_what_to_do() {
        let e = VolumeError::NotMounted {
            role: Role::Destination,
            path: "/Volumes/Backup/Documents".into(),
            expected: Box::new(backup()),
            found_on: Some("Macintosh HD".into()),
            elsewhere: Some("/Volumes/Backup 1".into()),
            windows_letter: None,
        };
        let text = e.message("documents");
        assert!(
            text.starts_with("Backup drive mounted somewhere else\n  "),
            "{text}"
        );
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("but that drive is mounted at /Volumes/Backup 1."),
            "{flat}"
        );
        assert!(
            flat.contains("/Volumes/Backup is currently a folder on Macintosh HD."),
            "{flat}"
        );
        assert!(
            flat.contains(
                "change the destination's path to /Volumes/Backup 1/Documents with: \
                 onefolder jobs edit documents (the drive is recorded again at the new path)."
            ),
            "{flat}"
        );
        assert!(!flat.contains("--rerecord"), "{flat}");
        assert!(!flat.contains("Eject"), "one action: {flat}");
        assert!(!flat.contains("Plug the drive in"), "{flat}");
        assert_eq!(e.code(), "not-mounted");
        let (severity, line) = state_line(&Err(e.clone()), &backup());
        assert_eq!(severity, Severity::Problem);
        assert_eq!(
            line,
            "not mounted at /Volumes/Backup, the drive is at /Volumes/Backup 1 (expects volume \"Backup\")"
        );
        // A source gets the --source hint.
        let source = VolumeError::NotMounted {
            role: Role::Source,
            path: "/Volumes/Backup/Documents".into(),
            expected: Box::new(backup()),
            found_on: None,
            elsewhere: Some("/Volumes/Backup 1".into()),
            windows_letter: None,
        };
        let hint = source.hint("documents").unwrap();
        assert!(
            hint.ends_with(
                "onefolder jobs edit documents --source \"/Volumes/Backup 1/Documents\""
            ),
            "{hint}"
        );
    }

    #[test]
    fn state_lines_match_doctor() {
        let id = backup();
        assert_eq!(
            state_line(&Ok(()), &id),
            (Severity::Ok, "mounted · volume \"Backup\"".to_string())
        );
        let unplugged = VolumeError::NotMounted {
            role: Role::Destination,
            path: "/Volumes/Backup/D".into(),
            expected: Box::new(id.clone()),
            found_on: None,
            elsewhere: None,
            windows_letter: None,
        };
        assert_eq!(
            state_line(&Err(unplugged), &id),
            (
                Severity::Warning,
                "not mounted (expects volume \"Backup\")".to_string()
            )
        );
        let ro = VolumeError::ReadOnly {
            role: Role::Destination,
            path: "/Volumes/Backup/D".into(),
            expected: Box::new(id.clone()),
        };
        assert_eq!(
            state_line(&Err(ro), &id),
            (Severity::Problem, "mounted read-only".to_string())
        );
    }

    /// Under WSL, a drive Windows has at another letter is mounted back at
    /// its recorded place with that letter.
    #[test]
    fn wsl_hints_use_the_letter_windows_reports() {
        let _wsl = host::testing::describe(host::Os::Wsl);
        let unplugged = VolumeError::NotMounted {
            role: Role::Destination,
            path: "/mnt/e/Backup".into(),
            expected: Box::new(Identity {
                mount_point: "/mnt/e".into(),
                ..backup()
            }),
            found_on: Some("the system disk".into()),
            elsewhere: None,
            windows_letter: Some('F'),
        };
        let flat = unplugged.detail();
        assert!(
            flat.ends_with(
                "Windows has the drive as F:, but WSL has not mounted it; mount it with: \
                 sudo mount -t drvfs F: /mnt/e. Then run again."
            ),
            "{flat}"
        );
        assert_eq!(windows_letter(Some("f:")), Some('F'));
        assert_eq!(windows_letter(Some("/dev/sdb1")), None);
        assert_eq!(windows_letter(None), None);
        // The longer limit under WSL names PowerShell.
        let slow = VolumeError::NotAnswering {
            path: "/mnt/e".into(),
            limit: WSL_DEADLINE,
        };
        assert_eq!(
            slow.detail(),
            "/mnt/e PowerShell did not answer in 30 seconds (or a network drive is hanging)"
        );
    }

    /// Where the mount names the folder of the file system it shows
    /// (Linux), another subvolume or a bind mount of a folder is not the
    /// recorded drive, though the UUID is the same.
    #[test]
    fn the_mounted_folder_of_the_drive_must_match() {
        let seen = |root: &str| Identity {
            mount_root: Some(root.into()),
            ..backup()
        };
        let whole = backup();
        assert!(same_volume(&whole, &seen("/")));
        assert!(!same_volume(&whole, &seen("/Docs")));
        let sub_a = Identity {
            mount_root: Some("/@a".into()),
            ..backup()
        };
        assert!(same_volume(&sub_a, &seen("/@a")));
        assert!(!same_volume(&sub_a, &seen("/@b")));
        assert!(!same_volume(&sub_a, &seen("/")));
        // macOS reports no root: not compared.
        assert!(same_volume(&sub_a, &backup()));
        assert_eq!(recordable(&seen("/")).mount_root, None);
        assert_eq!(recordable(&seen("/@a")).mount_root.as_deref(), Some("/@a"));
        assert_eq!(
            seen("/@b").describe(),
            "volume \"Backup\" (UUID A1B2C3D4-...), folder /@b of it"
        );
        let id = Identity::recorded(
            "/media/u/Pool/x",
            Recorded {
                mount_point: Some("/media/u/Pool"),
                volume_uuid: Some("A-1"),
                mount_root: Some("/"),
                ..Recorded::default()
            },
        )
        .unwrap();
        assert_eq!(id.mount_root, None, "/ is the same as none");
    }

    #[test]
    fn which_folders_ignore_case() {
        use host::Os;
        for fstype in ["apfs", "ext4", "msdos"] {
            assert!(
                fstype_ignores_case(Os::Mac, fstype, Some(false)),
                "{fstype}"
            );
        }
        for os in [Os::Linux, Os::Wsl] {
            for fstype in [
                "ext2", "ext3", "btrfs", "xfs", "jfs", "tmpfs", "overlay", "zfs", "wslfs", "lxfs",
            ] {
                assert!(!fstype_ignores_case(os, fstype, None), "{fstype}");
            }
            for fstype in [
                "vfat",
                "exfat",
                "ntfs",
                "ntfs3",
                "fuseblk",
                "hfsplus",
                "cifs",
                "smb3",
                "nfs",
                "nfs4",
                "fuse.sshfs",
                "fuse.rclone",
                "9p",
                "drvfs",
                "virtiofs",
                "something-new",
            ] {
                assert!(fstype_ignores_case(os, fstype, Some(false)), "{fstype}");
            }
            // ext4 and f2fs: case counts unless the folder is casefolded,
            // or its flag could not be read.
            for fstype in ["ext4", "f2fs"] {
                assert!(!fstype_ignores_case(os, fstype, Some(false)), "{fstype}");
                assert!(fstype_ignores_case(os, fstype, Some(true)), "{fstype}");
                assert!(fstype_ignores_case(os, fstype, None), "{fstype}");
            }
        }
    }

    #[test]
    fn wrap_keeps_words_whole() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("", 5), Vec::<String>::new());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[tokio::test]
    async fn other_platforms_say_unsupported() {
        let err = identify(Path::new("/tmp")).await.unwrap_err();
        assert_eq!(err, VolumeError::Unsupported);
    }

    /// A folder still to be created is checked where it would land: the
    /// nearest existing ancestor must be on the recorded mount.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn creatable_folders_land_on_the_recorded_mount() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let here = platform::statfs(&root).unwrap();
        let identity = |mount_point: &str| Identity {
            mount_point: mount_point.to_string(),
            volume_name: "Backup".into(),
            volume_uuid: Some("A1B2C3D4-0000-4E5F-8A9B-000000000001".into()),
            partition_uuid: None,
            device: None,
            mount_root: None,
        };
        let missing = root.join("new/deeper/dst");
        assert_eq!(
            check_creatable(&missing, &identity(&here.mount_point)).await,
            Ok(())
        );
        // A symlinked parent that leads off the recorded drive is refused.
        let err = check_creatable(&missing, &identity("/Volumes/Backup"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "not-mounted", "{err}");
        // A file where a parent folder should be.
        std::fs::write(root.join("file"), "x").unwrap();
        let err = check_creatable(&root.join("file/dst"), &identity(&here.mount_point))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "not-a-folder", "{err}");
    }
}
