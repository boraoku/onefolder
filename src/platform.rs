//! Where OneFolder runs: macOS, Linux, Linux under WSL (the Windows
//! Subsystem for Linux), or Windows itself. Answers the few questions
//! whose answer differs:
//! where drives are mounted, which mount points are the system disk, what
//! a drive path looks like in a placeholder, and how the computer is named
//! in messages ("this Mac" on macOS, "this computer" elsewhere).
//!
//! Each answer is a pure `*_on(os, ...)` function with a thin wrapper for
//! the platform it runs on, so the tests on a Mac cover the Linux and WSL
//! branches too.

use std::path::{Component, Path};
use std::sync::OnceLock;

/// The platform, as far as OneFolder's behaviour goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Mac,
    Linux,
    /// Linux under WSL: Windows drives are mounted at `/mnt/<letter>`.
    Wsl,
    /// Windows itself: a drive is a letter (`D:\`) or a share
    /// (`\\nas\share\`), and the system drive is `%SystemDrive%`.
    Windows,
}

/// The platform this binary runs on, worked out once.
pub fn os() -> Os {
    static OS: OnceLock<Os> = OnceLock::new();
    *OS.get_or_init(|| {
        if cfg!(target_os = "macos") {
            Os::Mac
        } else if cfg!(windows) {
            Os::Windows
        } else if is_wsl() {
            Os::Wsl
        } else {
            Os::Linux
        }
    })
}

/// True under WSL 1 or 2: the interop handler is registered, WSL named the
/// distribution, or the kernel says it is Microsoft's.
pub fn is_wsl() -> bool {
    if cfg!(target_os = "macos") || cfg!(windows) {
        return false;
    }
    detect_wsl(
        Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists(),
        std::env::var_os("WSL_DISTRO_NAME").is_some_and(|v| !v.is_empty()),
        std::fs::read_to_string("/proc/version").ok().as_deref(),
    )
}

/// [`is_wsl`] from what it reads.
pub fn detect_wsl(interop: bool, distro_named: bool, proc_version: Option<&str>) -> bool {
    interop
        || distro_named
        || proc_version.is_some_and(|v| v.to_ascii_lowercase().contains("microsoft"))
}

/// The user's login name, for `/media/<user>`: `USER`, then `LOGNAME`,
/// then the last part of the home folder.
pub fn user_name() -> Option<String> {
    ["USER", "LOGNAME"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty() && !v.contains('/'))
        .or_else(|| {
            std::env::var_os("HOME").and_then(|h| {
                Path::new(&h)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
        })
}

/// A path that lies where a drive would be mounted: the drive root it is
/// under (`/Volumes`, `/media`...) and the drive's mount point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrivePath {
    pub root: String,
    pub mount_point: String,
}

/// Where a drive holding `path` would be mounted, judged from the path
/// alone after resolving `.` and `..` lexically:
///
/// - `/Volumes/<name>` (ignoring case, as APFS does);
/// - on Linux, `/media/<user>/<name>` (udisks, with `user` the login
///   name) or `/media/<name>` (older systems), `/run/media/<user>/<name>`
///   (Fedora, Arch) and `/mnt/<name>`;
/// - on WSL the same, except the system drive `/mnt/c` and WSL's own
///   `/mnt/wsl` and `/mnt/wslg`, which are not drives to record;
/// - on Windows any drive letter but the system drive's (`D:\Backup` is on
///   `D:\`), and a share (`\\nas\share\Backup` is on `\\nas\share\`).
pub fn drive_path_on(os: Os, user: Option<&str>, path: &Path) -> Option<DrivePath> {
    if os == Os::Windows {
        let root = windows_drive_root(&path.to_string_lossy())?;
        let system = root.eq_ignore_ascii_case(&windows_system_drive());
        return (!system).then(|| DrivePath {
            root: root.clone(),
            mount_point: root,
        });
    }
    let parts = lexical_parts(path);
    let part = |i: usize| parts.get(i).map(String::as_str);
    let first = part(0)?;
    if first.eq_ignore_ascii_case("Volumes") {
        let name = part(1)?;
        return Some(DrivePath {
            root: "/Volumes".into(),
            mount_point: format!("/Volumes/{name}"),
        });
    }
    if os == Os::Mac {
        return None;
    }
    let found = match (first, part(1), part(2)) {
        ("media", Some(u), Some(name)) if Some(u) == user => DrivePath {
            root: "/media".into(),
            mount_point: format!("/media/{u}/{name}"),
        },
        ("media", Some(u), None) if Some(u) == user => return None,
        ("media", Some(name), _) => DrivePath {
            root: "/media".into(),
            mount_point: format!("/media/{name}"),
        },
        ("run", Some("media"), Some(u)) => DrivePath {
            root: "/run/media".into(),
            mount_point: format!("/run/media/{u}/{}", part(3)?),
        },
        ("mnt", Some(name), _) => DrivePath {
            root: "/mnt".into(),
            mount_point: format!("/mnt/{name}"),
        },
        _ => return None,
    };
    // Under WSL the system drive and WSL's own folders are not drive paths.
    let wsl_own = os == Os::Wsl && is_wsl_own(&found.mount_point);
    (!wsl_own).then_some(found)
}

/// The Windows system drive `/mnt/c` and WSL's own `/mnt/wsl` and
/// `/mnt/wslg` (though a disk attached with `wsl --mount` below `/mnt/wsl`
/// is a drive of its own; see [`is_system_mount_on`]).
fn is_wsl_own(mount_point: &str) -> bool {
    ["/mnt/c", "/mnt/wsl", "/mnt/wslg"].contains(&mount_point)
}

/// [`drive_path_on`] for this platform and user.
pub fn drive_path(path: &Path) -> Option<DrivePath> {
    drive_path_on(os(), user_name().as_deref(), path)
}

/// The drive or share a Windows path is on, as its root: `D:\` for
/// `D:\Backup` (also `d:/Backup` and `\\?\D:\Backup`), `\\nas\share\` for
/// `\\nas\share\Backup` (also `\\?\UNC\nas\share\Backup`). `None` for any
/// other path, `D:Backup` (relative to D:'s current folder) included.
/// Read from the text alone, so it answers the same on every platform.
pub fn windows_drive_root(path: &str) -> Option<String> {
    let unc;
    let mut p = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix("//?/"))
        .unwrap_or(path);
    if let Some(rest) = p.strip_prefix(r"UNC\") {
        unc = format!(r"\\{rest}");
        p = &unc;
    }
    let bytes = p.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let rooted = bytes.len() == 2 || matches!(bytes[2], b'\\' | b'/');
        // `D:` alone is the drive too (`SystemDrive` is written so).
        return rooted.then(|| format!(r"{}:\", (bytes[0] as char).to_ascii_uppercase()));
    }
    let rest = p.strip_prefix(r"\\").or_else(|| p.strip_prefix("//"))?;
    let mut parts = rest.split(['\\', '/']).filter(|s| !s.is_empty());
    let (server, share) = (parts.next()?, parts.next()?);
    Some(format!(r"\\{server}\{share}\"))
}

/// The root of the Windows system drive, `C:\`, from `SystemDrive`.
pub fn windows_system_drive() -> String {
    std::env::var("SystemDrive")
        .ok()
        .and_then(|d| windows_drive_root(&d))
        .unwrap_or_else(|| r"C:\".to_string())
}

/// A path's normal components, with `.` and `..` resolved lexically.
fn lexical_parts(path: &Path) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    parts
}

/// The standard Linux mount points that belong to the system: a separate
/// file system mounted at one of them is still part of the system disk,
/// never recorded and never checked.
pub const LINUX_SYSTEM_MOUNTS: &[&str] = &[
    "/",
    "/boot",
    "/boot/efi",
    "/efi",
    "/usr",
    "/usr/local",
    "/var",
    "/var/log",
    "/var/tmp",
    "/home",
    "/tmp",
    "/opt",
    "/root",
    "/nix",
    "/snap",
];

/// File systems with no disk behind them (memory, the kernel's own views,
/// container layers, snap packages): never a drive to record.
pub const VIRTUAL_FSTYPES: &[&str] = &[
    "tmpfs",
    "ramfs",
    "overlay",
    "aufs",
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "cgroup",
    "cgroup2",
    "squashfs",
    "autofs",
    "mqueue",
    "securityfs",
    "debugfs",
    "tracefs",
    "bpf",
    "pstore",
    "efivarfs",
    "configfs",
    "fusectl",
    "hugetlbfs",
    "binfmt_misc",
    "rpc_pipefs",
    "nsfs",
    "selinuxfs",
    "fuse.gvfsd-fuse",
    "fuse.portal",
    "fuse.lxcfs",
    "wslfs",
    "lxfs",
];

/// True for a mount on the system disk, which is never recorded as a
/// drive (it cannot be unplugged, and its identity can change with a
/// reinstall):
///
/// - macOS: `/` (the sealed system volume) and `/System/Volumes/...`
///   (the data volume that holds `/Users`, `/Volumes` itself and
///   `/private`); a folder at `/Volumes/Backup` with no drive plugged in
///   reports `/System/Volumes/Data`. `fstype` is not looked at;
/// - Linux: `/`, a virtual file system ([`VIRTUAL_FSTYPES`]), or one of
///   the standard system mount points ([`LINUX_SYSTEM_MOUNTS`]). Every
///   other mount (a drive at `/media/<user>/Backup`, `/data`,
///   `/srv/backup` or `/mnt`, a network share) is a drive and is checked;
/// - WSL: the same, and the Windows system drive `/mnt/c`;
/// - Windows: the system drive (`C:\`). `fstype` is not looked at.
pub fn is_system_mount_on(os: Os, mount_point: &str, fstype: &str) -> bool {
    match os {
        Os::Windows => windows_drive_root(mount_point)
            .is_some_and(|root| root.eq_ignore_ascii_case(&windows_system_drive())),
        Os::Mac => mount_point == "/" || mount_point.starts_with("/System/Volumes/"),
        Os::Linux | Os::Wsl => {
            LINUX_SYSTEM_MOUNTS.contains(&mount_point)
                || VIRTUAL_FSTYPES.contains(&fstype)
                || (os == Os::Wsl && mount_point == "/mnt/c")
        }
    }
}

/// [`is_system_mount_on`] for this platform.
pub fn is_system_mount(mount_point: &str, fstype: &str) -> bool {
    is_system_mount_on(os(), mount_point, fstype)
}

/// The note for a folder on the system disk, which has no drive to
/// record: `on the startup disk` on a Mac; on Linux the mount it is on,
/// `on /, the system disk` or `on /home, part of the system disk`, since a
/// separate file system is never called the system disk without saying so.
pub fn system_disk_note_on(os: Os, mount_point: &str) -> String {
    match os {
        Os::Mac => "on the startup disk".to_string(),
        Os::Windows => format!("on {mount_point}, the system drive"),
        _ if mount_point == "/" => "on /, the system disk".to_string(),
        _ => format!("on {mount_point}, part of the system disk"),
    }
}

/// [`system_disk_note_on`] for the platform the messages describe.
pub fn system_disk_note(mount_point: &str) -> String {
    system_disk_note_on(ui_os(), mount_point)
}

/// The drive letter of a WSL mount point `/mnt/<letter>`, upper case.
pub fn wsl_drive_letter(mount_point: &str) -> Option<char> {
    let rest = mount_point.strip_prefix("/mnt/")?;
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase()),
        _ => None,
    }
}

/// What to do on WSL when a recorded Windows drive is not mounted: WSL
/// mounts the drives present when it starts, not one plugged in later.
/// The drive goes back at its recorded mount point, whatever letter
/// Windows gave it this time.
pub fn wsl_mount_hint(letter: char, mount_point: &str) -> String {
    format!(
        "mount it with: sudo mount -t drvfs {}: {mount_point}",
        letter.to_ascii_uppercase()
    )
}

/// The example path in the job form's drive field.
pub fn placeholder_drive_path_on(os: Os, user: Option<&str>) -> String {
    match (os, user) {
        (Os::Mac, _) => "/Volumes/Backup/Documents".to_string(),
        (Os::Wsl, _) => "/mnt/d/Backup/Documents".to_string(),
        (Os::Windows, _) => r"D:\Backup\Documents".to_string(),
        (Os::Linux, Some(user)) => format!("/media/{user}/Backup/Documents"),
        (Os::Linux, None) => "/media/Backup/Documents".to_string(),
    }
}

/// [`placeholder_drive_path_on`] for the platform the screens describe.
pub fn placeholder_drive_path() -> String {
    placeholder_drive_path_on(ui_os(), user_name().as_deref())
}

/// The folder the browser's `v` key opens: the first that exists of the
/// places drives are mounted, or the last of them when none does. On
/// Windows the first drive after the system drive (`D:\`, `E:\`...), or
/// the system drive when there is none.
pub fn drives_folder_on(os: Os, user: Option<&str>, exists: impl Fn(&Path) -> bool) -> String {
    let mut candidates: Vec<String> = Vec::new();
    match os {
        Os::Mac => candidates.push("/Volumes".into()),
        Os::Linux => {
            if let Some(user) = user {
                candidates.push(format!("/media/{user}"));
                candidates.push(format!("/run/media/{user}"));
            }
            candidates.push("/media".into());
            candidates.push("/mnt".into());
        }
        Os::Wsl => candidates.push("/mnt".into()),
        Os::Windows => {
            let system = windows_system_drive();
            candidates.extend(
                ('D'..='Z')
                    .map(|l| format!(r"{l}:\"))
                    .filter(|d| !d.eq_ignore_ascii_case(&system)),
            );
            candidates.push(system);
        }
    }
    let last = candidates.last().cloned().unwrap_or_else(|| "/".into());
    candidates
        .into_iter()
        .find(|c| exists(Path::new(c)))
        .unwrap_or(last)
}

/// [`drives_folder_on`] for the platform the screens describe.
pub fn drives_folder() -> String {
    drives_folder_on(ui_os(), user_name().as_deref(), Path::is_dir)
}

/// The words that name this computer and its disks in messages.
#[derive(Debug, PartialEq, Eq)]
pub struct Words {
    /// `this Mac`, `this computer`.
    pub this_computer: &'static str,
    /// `your Mac`, `your computer`.
    pub your_computer: &'static str,
    /// `the startup disk`, `the system disk`.
    pub system_disk: &'static str,
    /// Who limits a file name's length: `macOS`, `the file system`.
    pub file_system: &'static str,
    /// The possessive of [`Words::file_system`].
    pub file_systems: &'static str,
    /// `to Mac` after a count (`2 to Mac`), or `here`.
    to_local: &'static str,
    /// `on Mac` after a count (`1 on Mac`), or `here`.
    on_local: &'static str,
}

impl Words {
    /// `2 to Mac`, or `2 here`.
    pub fn to_local(&self, n: impl std::fmt::Display) -> String {
        format!("{n} {}", self.to_local)
    }

    /// `1 on Mac`, or `1 here`.
    pub fn on_local(&self, n: impl std::fmt::Display) -> String {
        format!("{n} {}", self.on_local)
    }
}

/// The words on macOS.
pub const MAC_WORDS: Words = Words {
    this_computer: "this Mac",
    your_computer: "your Mac",
    system_disk: "the startup disk",
    file_system: "macOS",
    file_systems: "macOS's",
    to_local: "to Mac",
    on_local: "on Mac",
};

/// The words on Windows, where disks are drives.
pub const WINDOWS_WORDS: Words = Words {
    this_computer: "this computer",
    your_computer: "your computer",
    system_disk: "the system drive",
    file_system: "the file system",
    file_systems: "the file system's",
    to_local: "here",
    on_local: "here",
};

/// The words on Linux and WSL.
pub const OTHER_WORDS: Words = Words {
    this_computer: "this computer",
    your_computer: "your computer",
    system_disk: "the system disk",
    file_system: "the file system",
    file_systems: "the file system's",
    to_local: "here",
    on_local: "here",
};

/// The words for a platform.
pub fn words_on(os: Os) -> &'static Words {
    match os {
        Os::Mac => &MAC_WORDS,
        Os::Linux | Os::Wsl => &OTHER_WORDS,
        Os::Windows => &WINDOWS_WORDS,
    }
}

/// The words for the platform the screens describe.
pub fn words() -> &'static Words {
    words_on(ui_os())
}

/// The platform the screens and messages describe: [`os`], except in unit
/// tests, where it is macOS unless a test says otherwise
/// ([`testing::describe`]), so the snapshots of rendered screens, laid out
/// around the macOS words, are the same on every platform.
pub fn ui_os() -> Os {
    #[cfg(test)]
    {
        testing::UI_OS.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        os()
    }
}

/// What to do about rclone, as `doctor`, `version` and the rclone-missing
/// message say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcloneAction {
    Install,
    Upgrade,
    Reinstall,
}

/// The advice for an [`RcloneAction`]: `install it with: brew install
/// rclone` on macOS; on Linux rclone's own installer (`install it from
/// https://rclone.org/install/ (or brew install rclone with Homebrew)`),
/// and `brew upgrade rclone` only when `brew` is on PATH; on Windows
/// rclone's downloads or winget.
pub fn rclone_advice_on(os: Os, action: RcloneAction, brew: bool) -> String {
    let (verb, command) = match action {
        RcloneAction::Install => ("install", "brew install rclone"),
        RcloneAction::Upgrade => ("update", "brew upgrade rclone"),
        RcloneAction::Reinstall => ("reinstall", "brew reinstall rclone"),
    };
    if os == Os::Windows {
        return match action {
            RcloneAction::Install => {
                "install it from https://rclone.org/install/ (or winget install Rclone.Rclone)"
                    .to_string()
            }
            _ => format!("{verb} it from https://rclone.org/install/"),
        };
    }
    match (os, action, brew) {
        (Os::Mac, _, _) | (_, RcloneAction::Upgrade | RcloneAction::Reinstall, true) => {
            format!("{verb} it with: {command}")
        }
        (_, RcloneAction::Install, _) => {
            "install it from https://rclone.org/install/ (or brew install rclone with Homebrew)"
                .to_string()
        }
        (_, _, false) => format!("{verb} it from https://rclone.org/install/"),
    }
}

/// [`rclone_advice_on`] for the platform the messages describe, looking
/// for `brew` on PATH.
pub fn rclone_advice(action: RcloneAction) -> String {
    let os = ui_os();
    let brew = os != Os::Mac
        && crate::rclone::detect::find_program(std::env::var_os("PATH").as_deref(), "brew")
            .is_some();
    rclone_advice_on(os, action, brew)
}

/// Unit-test helpers: which platform the screens describe.
#[cfg(test)]
pub mod testing {
    use super::Os;
    use std::cell::Cell;

    thread_local! {
        pub(super) static UI_OS: Cell<Os> = const { Cell::new(Os::Mac) };
    }

    /// Makes this test thread's screens and messages describe `os` until
    /// the guard is dropped.
    pub fn describe(os: Os) -> Guard {
        let before = UI_OS.with(|c| c.replace(os));
        Guard(before)
    }

    pub struct Guard(Os);

    impl Drop for Guard {
        fn drop(&mut self) {
            UI_OS.with(|c| c.set(self.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_is_recognised_by_any_sign() {
        assert!(detect_wsl(true, false, None));
        assert!(detect_wsl(false, true, None));
        assert!(detect_wsl(
            false,
            false,
            Some("Linux version 5.15.167.4-microsoft-standard-WSL2 (root@...)")
        ));
        assert!(detect_wsl(
            false,
            false,
            Some("Linux version 4.4.0-19041-Microsoft (Microsoft@Microsoft.com)")
        ));
        assert!(!detect_wsl(
            false,
            false,
            Some("Linux version 6.8.0-45-generic (buildd@lcy02-amd64-075)")
        ));
        assert!(!detect_wsl(false, false, None));
    }

    #[test]
    fn this_platform_is_known() {
        if cfg!(target_os = "macos") {
            assert_eq!(os(), Os::Mac);
        } else if cfg!(windows) {
            assert_eq!(os(), Os::Windows);
            assert!(!is_wsl());
        } else {
            assert!(matches!(os(), Os::Linux | Os::Wsl));
        }
        // Unit tests describe the Mac unless they ask otherwise.
        assert_eq!(words(), &MAC_WORDS);
        {
            let _linux = testing::describe(Os::Linux);
            assert_eq!(words(), &OTHER_WORDS);
        }
        assert_eq!(words(), &MAC_WORDS);
    }

    fn mount_point(os: Os, user: Option<&str>, path: &str) -> Option<String> {
        drive_path_on(os, user, Path::new(path)).map(|d| d.mount_point)
    }

    #[test]
    fn drive_paths_on_a_mac() {
        let mac = |p| mount_point(Os::Mac, Some("bora"), p);
        assert_eq!(
            mac("/Volumes/Backup/Docs").as_deref(),
            Some("/Volumes/Backup")
        );
        assert_eq!(mac("/VOLUMES/Backup").as_deref(), Some("/Volumes/Backup"));
        assert_eq!(mac("/private/../Volumes/B").as_deref(), Some("/Volumes/B"));
        assert_eq!(mac("/Volumes"), None);
        assert_eq!(mac("/Volumesque/x"), None);
        assert_eq!(mac("/media/bora/Backup"), None);
        assert_eq!(mac("/mnt/d"), None);
        assert_eq!(
            drive_path_on(Os::Mac, None, Path::new("/Volumes/B/x"))
                .unwrap()
                .root,
            "/Volumes"
        );
    }

    #[test]
    fn drive_paths_on_linux() {
        let linux = |p| mount_point(Os::Linux, Some("bora"), p);
        assert_eq!(
            linux("/media/bora/Backup/Documents").as_deref(),
            Some("/media/bora/Backup")
        );
        assert_eq!(linux("/media/bora"), None, "the user's own folder");
        assert_eq!(
            linux("/media/Backup/Docs").as_deref(),
            Some("/media/Backup")
        );
        assert_eq!(
            linux("/run/media/bora/Backup/Docs").as_deref(),
            Some("/run/media/bora/Backup")
        );
        assert_eq!(linux("/run/media/bora"), None);
        assert_eq!(linux("/mnt/data/x").as_deref(), Some("/mnt/data"));
        assert_eq!(linux("/mnt/c/Users").as_deref(), Some("/mnt/c"));
        assert_eq!(linux("/mnt"), None);
        assert_eq!(
            linux("/Volumes/Backup/x").as_deref(),
            Some("/Volumes/Backup")
        );
        assert_eq!(linux("/home/bora/Documents"), None);
        assert_eq!(linux("/media/../home/x"), None);
        // Someone else's drive is still a drive.
        assert_eq!(
            mount_point(Os::Linux, Some("bora"), "/media/sam/Stick/x").as_deref(),
            Some("/media/sam")
        );
        assert_eq!(
            drive_path_on(Os::Linux, None, Path::new("/run/media/u/B"))
                .unwrap()
                .root,
            "/run/media"
        );
    }

    #[test]
    fn drive_paths_on_wsl() {
        let wsl = |p| mount_point(Os::Wsl, Some("bora"), p);
        assert_eq!(wsl("/mnt/d/Backup").as_deref(), Some("/mnt/d"));
        assert_eq!(wsl("/mnt/c/Users/bora/Documents"), None, "the system drive");
        assert_eq!(wsl("/mnt/wsl/x"), None);
        assert_eq!(wsl("/mnt/wslg/x"), None);
        assert_eq!(
            wsl("/media/bora/Backup/x").as_deref(),
            Some("/media/bora/Backup")
        );
    }

    #[test]
    fn windows_drive_roots() {
        assert_eq!(windows_drive_root(r"D:\Backup").as_deref(), Some(r"D:\"));
        assert_eq!(windows_drive_root("d:/Backup").as_deref(), Some(r"D:\"));
        assert_eq!(windows_drive_root(r"E:\").as_deref(), Some(r"E:\"));
        assert_eq!(windows_drive_root("C:").as_deref(), Some(r"C:\"));
        assert_eq!(windows_drive_root(r"\\?\D:\x").as_deref(), Some(r"D:\"));
        assert_eq!(
            windows_drive_root(r"\\nas\share\Backup").as_deref(),
            Some(r"\\nas\share\")
        );
        assert_eq!(
            windows_drive_root(r"\\?\UNC\nas\share\x").as_deref(),
            Some(r"\\nas\share\")
        );
        assert_eq!(windows_drive_root(r"\\nas"), None, "a server, no share");
        assert_eq!(windows_drive_root("D:Backup"), None, "relative to D:");
        assert_eq!(windows_drive_root("/Volumes/Backup"), None);
        assert_eq!(windows_drive_root("Backup"), None);
    }

    #[test]
    fn drive_paths_on_windows() {
        // SystemDrive is C: on CI's Windows runners and unset elsewhere,
        // where C: is assumed.
        let win = |p: &str| mount_point(Os::Windows, Some("bora"), p);
        assert_eq!(win(r"D:\Backup\Documents").as_deref(), Some(r"D:\"));
        assert_eq!(win("e:/Photos").as_deref(), Some(r"E:\"));
        assert_eq!(win(r"C:\Users\bora\Documents"), None, "the system drive");
        assert_eq!(win(r"c:\x"), None);
        assert_eq!(win(r"\\nas\share\Backup").as_deref(), Some(r"\\nas\share\"));
        assert_eq!(win("/Volumes/Backup"), None);
        assert_eq!(
            drive_path_on(Os::Windows, None, Path::new(r"D:\x"))
                .unwrap()
                .root,
            r"D:\"
        );
        assert!(is_system_mount_on(Os::Windows, r"C:\", "ntfs"));
        assert!(is_system_mount_on(Os::Windows, "c:", "ntfs"));
        assert!(!is_system_mount_on(Os::Windows, r"D:\", "ntfs"));
        assert!(!is_system_mount_on(Os::Windows, r"\\nas\share\", "smb"));
        assert_eq!(
            system_disk_note_on(Os::Windows, r"C:\"),
            r"on C:\, the system drive"
        );
        assert_eq!(
            placeholder_drive_path_on(Os::Windows, Some("bora")),
            r"D:\Backup\Documents"
        );
        let none = |_: &Path| false;
        assert_eq!(drives_folder_on(Os::Windows, None, none), r"C:\");
        let e = |p: &Path| p == Path::new(r"E:\");
        assert_eq!(drives_folder_on(Os::Windows, None, e), r"E:\");
        assert_eq!(words_on(Os::Windows).system_disk, "the system drive");
        assert_eq!(
            rclone_advice_on(Os::Windows, RcloneAction::Install, false),
            "install it from https://rclone.org/install/ (or winget install Rclone.Rclone)"
        );
        assert_eq!(
            rclone_advice_on(Os::Windows, RcloneAction::Upgrade, true),
            "update it from https://rclone.org/install/"
        );
    }

    #[test]
    fn system_mounts() {
        assert!(is_system_mount_on(Os::Mac, "/", "apfs"));
        assert!(is_system_mount_on(Os::Mac, "/System/Volumes/Data", "apfs"));
        assert!(!is_system_mount_on(Os::Mac, "/Volumes/Backup", "apfs"));
        assert!(
            !is_system_mount_on(Os::Mac, "/home", "autofs"),
            "the macOS rule is unchanged"
        );
        for os in [Os::Linux, Os::Wsl] {
            for (system, fstype) in [
                ("/", "ext4"),
                ("/home", "ext4"),
                ("/boot/efi", "vfat"),
                ("/var", "xfs"),
                ("/tmp", "tmpfs"),
                ("/nix", "btrfs"),
                ("/media/bora/ram", "tmpfs"),
                ("/snap/core22/1380", "squashfs"),
                ("/data", "overlay"),
            ] {
                assert!(is_system_mount_on(os, system, fstype), "{os:?} {system}");
            }
            for (drive, fstype) in [
                ("/media/bora/Backup", "ext4"),
                ("/run/media/bora/Backup", "exfat"),
                ("/mnt", "ext4"),
                ("/mnt/d", "ext4"),
                ("/data", "ext4"),
                ("/srv/backup", "btrfs"),
                ("/media/nas", "cifs"),
                ("/mnt/pool", "zfs"),
                ("/mnt/wsl/PHYSICALDRIVE1p1", "ext4"),
            ] {
                assert!(!is_system_mount_on(os, drive, fstype), "{os:?} {drive}");
            }
        }
        assert!(!is_system_mount_on(Os::Linux, "/mnt/c", "ext4"));
        assert!(is_system_mount_on(Os::Wsl, "/mnt/c", "9p"));
        assert!(is_system_mount_on(Os::Wsl, "/", "wslfs"));
        assert!(is_system_mount_on(Os::Wsl, "/mnt/wslg", "tmpfs"));
        assert!(!is_system_mount_on(Os::Wsl, "/mnt/d", "drvfs"));
    }

    #[test]
    fn the_system_disk_note_names_the_mount() {
        assert_eq!(
            system_disk_note_on(Os::Mac, "/System/Volumes/Data"),
            "on the startup disk"
        );
        assert_eq!(system_disk_note_on(Os::Linux, "/"), "on /, the system disk");
        assert_eq!(
            system_disk_note_on(Os::Linux, "/home"),
            "on /home, part of the system disk"
        );
    }

    #[test]
    fn wsl_drive_letters_and_the_mount_hint() {
        assert_eq!(wsl_drive_letter("/mnt/e"), Some('E'));
        assert_eq!(wsl_drive_letter("/mnt/E"), Some('E'));
        assert_eq!(wsl_drive_letter("/mnt/data"), None);
        assert_eq!(wsl_drive_letter("/media/e"), None);
        assert_eq!(
            wsl_mount_hint('e', "/mnt/e"),
            "mount it with: sudo mount -t drvfs E: /mnt/e"
        );
        assert_eq!(
            wsl_mount_hint('F', "/mnt/e"),
            "mount it with: sudo mount -t drvfs F: /mnt/e"
        );
    }

    #[test]
    fn placeholders_and_the_drives_folder() {
        assert_eq!(
            placeholder_drive_path_on(Os::Mac, Some("bora")),
            "/Volumes/Backup/Documents"
        );
        assert_eq!(
            placeholder_drive_path_on(Os::Linux, Some("bora")),
            "/media/bora/Backup/Documents"
        );
        assert_eq!(
            placeholder_drive_path_on(Os::Wsl, Some("bora")),
            "/mnt/d/Backup/Documents"
        );
        assert_eq!(placeholder_drive_path(), "/Volumes/Backup/Documents");

        let none = |_: &Path| false;
        assert_eq!(drives_folder_on(Os::Mac, Some("bora"), none), "/Volumes");
        assert_eq!(drives_folder_on(Os::Linux, Some("bora"), none), "/mnt");
        assert_eq!(drives_folder_on(Os::Wsl, Some("bora"), none), "/mnt");
        let only = |want: &'static str| move |p: &Path| p == Path::new(want);
        assert_eq!(
            drives_folder_on(Os::Linux, Some("bora"), only("/run/media/bora")),
            "/run/media/bora"
        );
        assert_eq!(
            drives_folder_on(Os::Linux, Some("bora"), |p: &Path| p.starts_with("/media")),
            "/media/bora"
        );
        assert_eq!(drives_folder_on(Os::Linux, None, only("/media")), "/media");
    }

    #[test]
    fn words_by_platform() {
        assert_eq!(MAC_WORDS.to_local(2), "2 to Mac");
        assert_eq!(MAC_WORDS.on_local(1), "1 on Mac");
        assert_eq!(OTHER_WORDS.to_local(2), "2 here");
        assert_eq!(OTHER_WORDS.on_local(1), "1 here");
        assert_eq!(words_on(Os::Wsl).this_computer, "this computer");
        assert_eq!(words_on(Os::Mac).this_computer, "this Mac");
    }

    #[test]
    fn rclone_advice_by_platform() {
        use RcloneAction::*;
        assert_eq!(
            rclone_advice_on(Os::Mac, Install, false),
            "install it with: brew install rclone"
        );
        assert_eq!(
            rclone_advice_on(Os::Mac, Upgrade, false),
            "update it with: brew upgrade rclone"
        );
        assert_eq!(
            rclone_advice_on(Os::Mac, Reinstall, false),
            "reinstall it with: brew reinstall rclone"
        );
        for brew in [false, true] {
            assert_eq!(
                rclone_advice_on(Os::Linux, Install, brew),
                "install it from https://rclone.org/install/ (or brew install rclone with Homebrew)"
            );
        }
        assert_eq!(
            rclone_advice_on(Os::Linux, Upgrade, true),
            "update it with: brew upgrade rclone"
        );
        assert_eq!(
            rclone_advice_on(Os::Wsl, Upgrade, false),
            "update it from https://rclone.org/install/"
        );
        assert_eq!(
            rclone_advice_on(Os::Linux, Reinstall, false),
            "reinstall it from https://rclone.org/install/"
        );
    }
}
