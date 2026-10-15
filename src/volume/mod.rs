//! Volume identity for local paths: proves a path is on the drive that was
//! recorded at setup, so an unplugged drive is never mistaken for an empty
//! folder. The interface takes a path and returns an [`Identity`]; the
//! macOS implementation (statfs and `diskutil info -plist`) lives in
//! `macos.rs` behind `cfg(target_os)`, and other platforms get a stub that
//! says the check is not supported yet.
//!
//! [`identify`] runs at setup (`jobs add`, `jobs edit`); [`check`] runs the
//! four checks from DESIGN.md before a run and in `doctor`.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

/// Where the volume holding a path is mounted, from `statfs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// `f_mntonname`: `/Volumes/Backup`, `/` or `/System/Volumes/Data`.
    pub mount_point: String,
    /// `f_mntfromname`: the device node, such as `/dev/disk4s1`, or the
    /// share for a network volume (`//bora@nas/Backup`).
    pub device: String,
    /// `f_fstypename`: `apfs`, `exfat`, `smbfs`...
    pub fstype: String,
    pub read_only: bool,
}

/// What `diskutil info -plist <mount point>` says about a volume.
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
}

impl Identity {
    /// Rebuilds an identity from the fields recorded in the config. Needs a
    /// UUID, a partition UUID or a device node. A missing mount point is
    /// derived from a `/Volumes/<name>/...` path, which is where macOS
    /// mounts drives.
    pub fn recorded(path: &str, fields: Recorded<'_>) -> Option<Identity> {
        let non_empty = |s: Option<&str>| s.filter(|v| !v.is_empty()).map(str::to_string);
        let volume_uuid = non_empty(fields.volume_uuid);
        let partition_uuid = non_empty(fields.partition_uuid);
        let device = non_empty(fields.device);
        if volume_uuid.is_none() && partition_uuid.is_none() && device.is_none() {
            return None;
        }
        let mount_point =
            non_empty(fields.mount_point).or_else(|| volumes_mount_point(Path::new(path)))?;
        Some(Identity {
            mount_point,
            volume_name: fields.volume_name.unwrap_or_default().to_string(),
            volume_uuid,
            partition_uuid,
            device,
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
    /// Setup only: a `/Volumes/...` path that lies on the startup disk.
    OnStartupDisk {
        path: String,
        mount_point: String,
    },
    NotAbsolute {
        path: String,
    },
    /// statfs or diskutil failed.
    Probe {
        path: String,
        message: String,
    },
    /// The check did not finish within [`DEADLINE`]: a hung network
    /// mount blocks statfs and getfsstat.
    NotAnswering {
        path: String,
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
                        "{role} {path} expects {}. {now} Plug the drive in and run again.",
                        expected.describe()
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
            VolumeError::OnStartupDisk { path, mount_point } => format!(
                "{path} is under /Volumes, but it is a folder on the startup disk ({mount_point}), \
                 not on a mounted drive. Plug the drive in and try again."
            ),
            VolumeError::NotAbsolute { .. } => "Give the full path, starting with /.".to_string(),
            VolumeError::Probe { path, message } => format!("{path}: {message}"),
            VolumeError::NotAnswering { path } => format!("{path} {}", not_answering_words()),
            VolumeError::Unsupported => {
                "OneFolder can record and check external drives on macOS only.".to_string()
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
        VolumeError::NotAnswering { .. } => (Severity::Problem, not_answering_words()),
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

/// True when an absolute path lies under `/Volumes`, after resolving `.`
/// and `..` lexically and ignoring case (APFS and HFS+ are usually case
/// insensitive, so `/VOLUMES/Backup` is the same place).
pub fn is_under_volumes(path: &Path) -> bool {
    volumes_parts(path).is_some()
}

/// `/Volumes/<name>` for a path under `/Volumes`.
pub fn volumes_mount_point(path: &Path) -> Option<String> {
    volumes_parts(path).map(|name| format!("/Volumes/{name}"))
}

fn volumes_parts(path: &Path) -> Option<String> {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    let first = parts.first()?.to_str()?;
    if parts.len() > 1 && first.eq_ignore_ascii_case("Volumes") {
        Some(parts[1].to_string_lossy().into_owned())
    } else {
        None
    }
}

/// True for the startup disk's mount points: `/` (the sealed system
/// volume) and `/System/Volumes/...` (the data volume that holds `/Users`,
/// `/Volumes` itself and `/private`). A folder at `/Volumes/Backup` with no
/// drive plugged in reports `/System/Volumes/Data`.
pub fn is_startup(mount_point: &str) -> bool {
    mount_point == "/" || mount_point.starts_with("/System/Volumes/")
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

/// The words for a check that ran out of time.
fn not_answering_words() -> String {
    format!(
        "did not answer in {} seconds, is a network drive hanging?",
        DEADLINE.as_secs()
    )
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
    within(
        path,
        DEADLINE,
        move || async move { identify_now(&owned).await },
    )
    .await
}

/// The checks before a run (see [`check_now`]), within [`DEADLINE`].
pub async fn check(path: &Path, expected: &Identity, needs_write: bool) -> Result<(), VolumeError> {
    let owned = path.to_path_buf();
    let expected = expected.clone();
    within(path, DEADLINE, move || async move {
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

/// True when a path is under `/Volumes` as written, or once its symlinks
/// are followed.
pub fn leads_into_volumes(path: &Path) -> bool {
    is_under_volumes(path) || is_under_volumes(&resolve(path))
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

/// File systems that diskutil does not describe, for which statfs's
/// device and type become a weak identity.
const NETWORK_FSTYPES: &[&str] = &["smbfs", "afpfs", "nfs", "webdav"];

/// Finds the volume that holds `path`. The path need not exist yet: the
/// closest existing parent is measured instead. Only a volume other than
/// the startup disk gets an identity.
async fn identify_now(path: &Path) -> Result<Found, VolumeError> {
    let (identity, mount, writable) = identify_any(path).await?;
    let startup = is_startup(&mount.mount_point);
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
    if !path.is_absolute() {
        return Err(VolumeError::NotAbsolute {
            path: path.display().to_string(),
        });
    }
    // A symlink is followed to where it leads, even when that does not
    // exist yet (an unplugged drive): that is the volume to record.
    let real = resolve(path);
    let existing = existing_ancestor(&real).ok_or_else(|| probe(path, "no part of it exists"))?;
    let mount = platform::statfs(&existing).map_err(|e| io_error(&existing, e))?;
    if is_under_volumes(&real) && is_startup(&mount.mount_point) {
        // The place the message is about is where the path leads.
        return Err(VolumeError::OnStartupDisk {
            path: real.display().to_string(),
            mount_point: mount.mount_point,
        });
    }
    let (seen, writable) = seen(&mount, false).await?;
    Ok((recordable(&seen), mount, writable))
}

/// Everything known about the volume mounted at `mount`: every proof
/// diskutil reports, or, for a network volume diskutil cannot describe
/// (unless `need_diskutil`), the statfs fallback. Also says whether
/// diskutil reports it writable.
async fn seen(mount: &Mount, need_diskutil: bool) -> Result<(Identity, Option<bool>), VolumeError> {
    match platform::diskutil_info(&mount.mount_point).await {
        Ok(info) => {
            let identity = Identity {
                mount_point: mount.mount_point.clone(),
                volume_name: info.volume_name.clone().unwrap_or_default(),
                volume_uuid: info.volume_uuid.clone().filter(|u| !u.is_empty()),
                partition_uuid: info.disk_uuid.clone().filter(|u| !u.is_empty()),
                device: Some(
                    info.device_node
                        .clone()
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(|| mount.device.clone()),
                ),
            };
            Ok((identity, info.writable))
        }
        Err(_) if !need_diskutil && is_network(mount) => Ok((network_identity(mount), None)),
        Err(e) => Err(probe(Path::new(&mount.mount_point), e)),
    }
}

fn is_network(mount: &Mount) -> bool {
    !is_startup(&mount.mount_point) && NETWORK_FSTYPES.contains(&mount.fstype.as_str())
}

/// The weak identity of a network volume: `<fstype>:<share>` and the
/// name the share is mounted under.
fn network_identity(mount: &Mount) -> Identity {
    Identity {
        mount_point: mount.mount_point.clone(),
        volume_name: Path::new(&mount.mount_point)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        volume_uuid: None,
        partition_uuid: None,
        device: Some(format!("{}:{}", mount.fstype, mount.device)),
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
    id
}

/// True when the volume seen now is the recorded one, by the strongest
/// proof that was recorded.
fn same_volume(expected: &Identity, seen: &Identity) -> bool {
    let eq = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
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
    let not_mounted = |found_on, elsewhere| VolumeError::NotMounted {
        role,
        path: shown.clone(),
        expected: Box::new(expected.clone()),
        found_on,
        elsewhere,
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
            let label = volume_label(&m.mount_point).await;
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
        return Err(not_mounted(Some(volume_label(&on.mount_point).await), None));
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
    within(path, DEADLINE, move || async move {
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
            found_on: Some(volume_label(&on.mount_point).await),
            elsewhere: None,
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
/// and a disk image can be attached anywhere. Gives the mount point it is
/// at.
///
/// A drive with a UUID costs one `diskutil info -plist <UUID>`, which
/// names the mount point directly (an unknown UUID is an error). A drive
/// recorded by device node only is looked for in the mount table with no
/// process spawned; network shares are never matched that way.
pub async fn mounted_elsewhere(expected: &Identity) -> Option<String> {
    let uuid = expected
        .volume_uuid
        .as_deref()
        .or(expected.partition_uuid.as_deref());
    if let Some(uuid) = uuid {
        let info = platform::diskutil_info(uuid).await.ok()?;
        let mount_point = info
            .mount_point
            .clone()
            .filter(|m| !m.is_empty() && *m != expected.mount_point)?;
        let seen = Identity {
            mount_point: mount_point.clone(),
            volume_name: info.volume_name.clone().unwrap_or_default(),
            volume_uuid: info.volume_uuid.clone(),
            partition_uuid: info.disk_uuid.clone(),
            device: info.device_node.clone(),
        };
        return same_volume(expected, &seen).then_some(mount_point);
    }
    let device = expected.device.as_deref()?;
    platform::mounts()
        .ok()?
        .into_iter()
        .find(|m| {
            m.mount_point != expected.mount_point
                && !is_startup(&m.mount_point)
                && !NETWORK_FSTYPES.contains(&m.fstype.as_str())
                && m.device == device
        })
        .map(|m| m.mount_point)
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

/// Whether a path leads onto a drive (see [`leads_into_volumes`]) and
/// where it really leads, within [`DEADLINE`]. A path that does not answer
/// counts as leading onto a drive, so nothing is assumed about it.
pub async fn leads_where(path: &Path) -> (bool, PathBuf, Option<VolumeError>) {
    let owned = path.to_path_buf();
    match guarded(path, DEADLINE, move || {
        (leads_into_volumes(&owned), resolve(&owned))
    })
    .await
    {
        Ok((leads, real)) => (leads, real, None),
        Err(e) => (true, path.to_path_buf(), Some(e)),
    }
}

/// How a mount point is named in messages: the volume name from diskutil,
/// with the startup disk called by the name of `/` ("Macintosh HD").
async fn volume_label(mount_point: &str) -> String {
    let target = if is_startup(mount_point) {
        "/"
    } else {
        mount_point
    };
    match platform::diskutil_info(target).await {
        Ok(DiskInfo {
            volume_name: Some(name),
            ..
        }) if !name.is_empty() => name,
        _ if is_startup(mount_point) => "the startup disk".to_string(),
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

/// What [`space`] reports, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub free: u64,
    pub used: u64,
}

/// Stub for platforms without a volume implementation yet.
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{DiskInfo, Mount};
    use std::io;
    use std::path::Path;

    pub fn statfs(_path: &Path) -> io::Result<Mount> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "volume checks are supported on macOS only",
        ))
    }

    pub async fn diskutil_info(_mount_point: &str) -> Result<DiskInfo, String> {
        Err("volume checks are supported on macOS only".to_string())
    }

    pub fn mounts() -> io::Result<Vec<Mount>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "volume checks are supported on macOS only",
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
        assert!(leads_into_volumes(&root.join("abs")));
        assert!(!leads_into_volumes(&root.join("rel")));
        assert!(leads_into_volumes(Path::new("/Volumes/Backup")));
        // A loop ends.
        let _ = resolve(&root.join("loop/x"));
        // Through a symlinked parent folder too.
        symlink(root.join("abs"), root.join("via")).unwrap();
        assert!(leads_into_volumes(&root.join("via/Sub")));
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
        assert!(is_under_volumes(Path::new("/Volumes/Backup/Docs")));
        assert!(is_under_volumes(Path::new("/VOLUMES/Backup")));
        assert!(is_under_volumes(Path::new("/private/../Volumes/Backup")));
        assert!(!is_under_volumes(Path::new("/Volumes")));
        assert!(!is_under_volumes(Path::new("/Volumesque/x")));
        assert_eq!(
            volumes_mount_point(Path::new("/Volumes/Backup/Docs")).as_deref(),
            Some("/Volumes/Backup")
        );
    }

    #[test]
    fn startup_mount_points() {
        assert!(is_startup("/"));
        assert!(is_startup("/System/Volumes/Data"));
        assert!(!is_startup("/Volumes/Backup"));
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
    }

    #[test]
    fn only_the_strongest_proof_is_recorded_and_compared() {
        let seen = Identity {
            mount_point: "/Volumes/X".into(),
            volume_name: "X".into(),
            volume_uuid: None,
            partition_uuid: Some("P-1".into()),
            device: Some("/dev/disk4s1".into()),
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

    #[test]
    fn wrap_keeps_words_whole() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("", 5), Vec::<String>::new());
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn other_platforms_say_unsupported() {
        let err = identify(Path::new("/tmp")).await.unwrap_err();
        assert_eq!(err, VolumeError::Unsupported);
    }

    /// A folder still to be created is checked where it would land: the
    /// nearest existing ancestor must be on the recorded mount.
    #[cfg(target_os = "macos")]
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
