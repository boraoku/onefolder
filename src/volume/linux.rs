//! Linux volume identity: the mount of a path from `/proc/self/mountinfo`
//! (no subprocess), the drive's UUID, partition UUID, label and device
//! from `lsblk -J`, and under WSL a Windows drive's volume GUID and label
//! from PowerShell's `Get-Volume`.
//!
//! `lsblk` runs unprivileged: util-linux reads udev's database, or probes
//! the device itself when it may. A mount whose source is not a block
//! device (a network share, a tmpfs) has no `lsblk` answer; network
//! shares get the weak `<fstype>:<source>` identity in `mod.rs`.
//!
//! Under WSL, Windows drives are mounted with `drvfs`: fstype `9p` with
//! `aname=drvfs` in the super options on WSL 2, fstype `drvfs` on WSL 1.
//! Linux sees no block device for them, so Windows names the volume.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{DiskInfo, Mount};
use crate::rclone::{detect, exec};

/// The mount table of this process's mount namespace.
const MOUNTINFO: &str = "/proc/self/mountinfo";

/// The columns asked of `lsblk`. `MOUNTPOINTS` (util-linux 2.37 and
/// newer) lists every place a device is mounted; older versions know
/// `MOUNTPOINT` only, which [`lsblk`] falls back to.
const LSBLK_COLUMNS: &str = "PATH,UUID,PARTUUID,LABEL,FSTYPE,RO,MOUNTPOINTS";
const LSBLK_COLUMNS_OLD: &str = "PATH,UUID,PARTUUID,LABEL,FSTYPE,RO,MOUNTPOINT";

/// Where Windows keeps PowerShell, seen from WSL, when `powershell.exe` is
/// not on PATH (WSL adds the Windows PATH unless told not to).
const POWERSHELL_FALLBACK: &str = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mount: Mount,
    /// The device number (major, minor) of the mounted file system.
    pub dev: (u32, u32),
    /// The Windows drive letter of a WSL drvfs mount, upper case.
    pub drive_letter: Option<char>,
}

/// `statfs`'s answer on Linux: the mount that holds the path once its
/// symlinks are resolved. A path that does not exist is `NotFound`, as on
/// macOS. See [`mount_of`] for how the entry is chosen.
pub fn statfs(path: &Path) -> io::Result<Mount> {
    use std::os::unix::fs::MetadataExt;
    let real = std::fs::canonicalize(path)?;
    let dev = split_dev(std::fs::metadata(&real)?.dev());
    let entries = read_mountinfo()?;
    mount_of(&entries, &real, Some(dev))
        .map(|e| e.mount.clone())
        .ok_or_else(|| io::Error::other(format!("no mount holds {}", real.display())))
}

/// A `st_dev` as (major, minor), as glibc's `major()` and `minor()` split it.
pub fn split_dev(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

/// Every mounted file system.
pub fn mounts() -> io::Result<Vec<Mount>> {
    Ok(read_mountinfo()?.into_iter().map(|e| e.mount).collect())
}

fn read_mountinfo() -> io::Result<Vec<Entry>> {
    Ok(parse_mountinfo(&std::fs::read_to_string(MOUNTINFO)?))
}

/// The entry holding `path` (absolute, symlinks resolved): the longest
/// mount point that is `path` or one of its ancestors and whose device is
/// `dev`, the path's own `st_dev`. Of two such mounts on the same point the
/// later one is on top, and wins.
///
/// The device decides what longest-prefix alone gets wrong: a file system
/// mounted over a parent folder later (a tmpfs over `/media/<user>`) hides
/// the drive below it, though the drive's entry is still listed. When no
/// prefix has the path's device (a btrfs subvolume reports a device of its
/// own that mountinfo does not list), or `dev` is unknown, the longest
/// prefix is taken.
pub fn mount_of<'a>(
    entries: &'a [Entry],
    path: &Path,
    dev: Option<(u32, u32)>,
) -> Option<&'a Entry> {
    let longest = |same_dev: bool| {
        let mut best: Option<&Entry> = None;
        for e in entries {
            let point = Path::new(&e.mount.mount_point);
            if !path.starts_with(point) || (same_dev && Some(e.dev) != dev) {
                continue;
            }
            let longer = best.is_none_or(|b| {
                point.components().count() >= Path::new(&b.mount.mount_point).components().count()
            });
            if longer {
                best = Some(e);
            }
        }
        best
    };
    longest(true).or_else(|| longest(false))
}

/// Parses `/proc/self/mountinfo`:
///
/// ```text
/// 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
/// ```
///
/// the ID, parent ID, device number, root, mount point, per-mount options,
/// optional fields up to `-`, then fstype, source and super options.
/// Spaces, tabs, newlines and backslashes in fields are octal escapes
/// (`\040`). A line that does not fit is skipped.
pub fn parse_mountinfo(text: &str) -> Vec<Entry> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Entry> {
    let fields: Vec<&str> = line.split(' ').collect();
    let dash = fields.iter().position(|f| *f == "-")?;
    if dash < 6 || fields.len() < dash + 3 {
        return None;
    }
    let mount_point = unescape(fields[4]);
    let options = fields[5];
    let fstype = unescape(fields[dash + 1]);
    let source = unescape(fields[dash + 2]);
    let super_options = fields
        .get(dash + 3)
        .map(|s| unescape(s))
        .unwrap_or_default();
    // Read-only either for this mount or for the whole file system (ext4
    // remounts its superblock read-only after an error, while the mount's
    // own options still say rw).
    let read_only =
        options.split(',').any(|o| o == "ro") || super_options.split(',').any(|o| o == "ro");
    let dev = fields[2]
        .split_once(':')
        .and_then(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?)))?;
    let root = unescape(fields[3]);
    let drive_letter = if is_drvfs(&fstype, &super_options) {
        drvfs_letter(&super_options, &source, &mount_point)
    } else {
        None
    };
    Some(Entry {
        mount: Mount {
            mount_point,
            device: source,
            fstype,
            read_only,
            root: Some(root),
        },
        dev,
        drive_letter,
    })
}

/// Undoes the kernel's octal escapes (`\040` for a space, `\134` for a
/// backslash).
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            let digits = &bytes[i + 1..i + 4];
            if digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                let value = digits
                    .iter()
                    .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                if let Ok(byte) = u8::try_from(value) {
                    out.push(byte);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A Windows drive under WSL: `drvfs` (WSL 1), or `9p` with
/// `aname=drvfs` (WSL 2).
pub fn is_drvfs(fstype: &str, super_options: &str) -> bool {
    fstype == "drvfs"
        || (fstype == "9p" && super_options.split([',', ';']).any(|o| o == "aname=drvfs"))
}

/// The drive letter of a drvfs mount: `path=D:\` in the super options,
/// else a source such as `D:\` or `D:`, else a mount point `/mnt/d`.
fn drvfs_letter(super_options: &str, source: &str, mount_point: &str) -> Option<char> {
    let letter_of = |s: &str| {
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), Some(':')) if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase()),
            _ => None,
        }
    };
    super_options
        .split([',', ';'])
        .find_map(|o| o.strip_prefix("path=").and_then(letter_of))
        .or_else(|| letter_of(source))
        .or_else(|| crate::platform::wsl_drive_letter(mount_point))
}

/// What is known of the volume at a mount point (from `lsblk`, or from
/// Windows for a WSL drive), or of the volume with a UUID or partition
/// UUID, when `target` does not start with `/`. The UUID lookup gives the
/// place the volume is mounted, if anywhere.
pub async fn disk_info(target: &str) -> Result<DiskInfo, String> {
    if !target.starts_with('/') {
        return by_uuid(target).await;
    }
    let entries = read_mountinfo().map_err(|e| format!("could not read {MOUNTINFO}: {e}"))?;
    let entry = entries
        .iter()
        .rev()
        .find(|e| e.mount.mount_point == target)
        .ok_or_else(|| format!("{target} is not a mount point"))?;
    if let Some(letter) = entry.drive_letter {
        let mut info = windows_volume(letter).await?;
        info.mount_point = Some(entry.mount.mount_point.clone());
        return Ok(info);
    }
    let device = &entry.mount.device;
    if !device.starts_with("/dev/") {
        return Err(format!("{device} is not a block device"));
    }
    let devices = lsblk(Some(device)).await?;
    let mut info = devices
        .into_iter()
        .find(|d| d.device_node.as_deref() == Some(device.as_str()))
        .ok_or_else(|| format!("lsblk did not describe {device}"))?;
    info.mount_point = Some(entry.mount.mount_point.clone());
    Ok(info)
}

/// The volume with this UUID (or partition UUID) and where it is mounted:
/// among the block devices, and under WSL among the Windows volumes.
async fn by_uuid(uuid: &str) -> Result<DiskInfo, String> {
    let same = |v: &Option<String>| v.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(uuid));
    let found = lsblk(None).await.ok().and_then(|devices| {
        devices
            .into_iter()
            .find(|d| same(&d.volume_uuid) || same(&d.disk_uuid))
    });
    if let Some(info) = found {
        return Ok(info);
    }
    if crate::platform::os() == crate::platform::Os::Wsl {
        let mut info = windows_volume_by_guid(uuid).await?;
        // A Windows drive is somewhere only if WSL mounted it, at
        // `/mnt/<letter>` or wherever someone mounted that letter.
        let letter = info.device_node.as_deref().and_then(|d| d.chars().next());
        info.mount_point = letter.and_then(|l| drvfs_mount_of(&read_mountinfo().ok()?, l));
        return Ok(info);
    }
    Err(format!("no volume with UUID {uuid}"))
}

/// Where WSL has mounted the Windows drive `letter`, if anywhere: the
/// last drvfs entry with that letter, preferring `/mnt/<letter>`.
pub fn drvfs_mount_of(entries: &[Entry], letter: char) -> Option<String> {
    let letter = letter.to_ascii_uppercase();
    let points: Vec<&str> = entries
        .iter()
        .filter(|e| e.drive_letter == Some(letter))
        .map(|e| e.mount.mount_point.as_str())
        .collect();
    let usual = format!("/mnt/{}", letter.to_ascii_lowercase());
    points
        .iter()
        .find(|p| **p == usual)
        .or(points.last())
        .map(|p| p.to_string())
}

/// `lsblk -J` for one device, or every device, flattened.
async fn lsblk(device: Option<&str>) -> Result<Vec<DiskInfo>, String> {
    let program = program_on_path("lsblk", &["/usr/bin/lsblk", "/bin/lsblk", "/sbin/lsblk"])
        .ok_or_else(|| "lsblk is not installed (it comes with util-linux)".to_string())?;
    let mut last = String::new();
    for columns in [LSBLK_COLUMNS, LSBLK_COLUMNS_OLD] {
        let mut args = vec!["-J", "-o", columns];
        args.extend(device);
        let captured = run(&program, &args).await?;
        if captured.status.success() {
            return parse_lsblk(&captured.stdout);
        }
        last = captured.stderr.clone();
        // An older lsblk names the column it does not know; anything else
        // is the answer.
        if !last.contains("unknown column") {
            break;
        }
    }
    Err(failure("lsblk", &last))
}

/// Parses `lsblk -J` output into one [`DiskInfo`] per device, children
/// (partitions) included. Handles both the newer `mountpoints` list and
/// the older `mountpoint`, and `ro` as a boolean or `"0"`/`"1"`.
pub fn parse_lsblk(text: &str) -> Result<Vec<DiskInfo>, String> {
    let doc: Value =
        serde_json::from_str(text).map_err(|e| format!("could not read lsblk's answer: {e}"))?;
    let mut out = Vec::new();
    let mut pending: Vec<&Value> = doc
        .get("blockdevices")
        .and_then(Value::as_array)
        .ok_or_else(|| "lsblk's answer has no blockdevices".to_string())?
        .iter()
        .rev()
        .collect();
    while let Some(d) = pending.pop() {
        let text = |key: &str| {
            d.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let mount_point = match d.get("mountpoints").and_then(Value::as_array) {
            Some(list) => list
                .iter()
                .filter_map(Value::as_str)
                .find(|m| m.starts_with('/'))
                .map(str::to_string),
            None => text("mountpoint").filter(|m| m.starts_with('/')),
        };
        let ro = match d.get("ro") {
            Some(Value::Bool(b)) => Some(*b),
            Some(Value::String(s)) => match s.as_str() {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            },
            Some(Value::Number(n)) => n.as_u64().map(|n| n != 0),
            _ => None,
        };
        out.push(DiskInfo {
            volume_uuid: text("uuid"),
            disk_uuid: text("partuuid"),
            volume_name: text("label"),
            device_node: text("path"),
            mount_point,
            writable: ro.map(|ro| !ro),
        });
        if let Some(children) = d.get("children").and_then(Value::as_array) {
            pending.extend(children.iter().rev());
        }
    }
    Ok(out)
}

/// A Windows volume by drive letter, through PowerShell's `Get-Volume`.
async fn windows_volume(letter: char) -> Result<DiskInfo, String> {
    let script = format!(
        "Get-Volume -DriveLetter {letter} | \
         Select-Object DriveLetter,UniqueId,FileSystemLabel,FileSystem,DriveType | ConvertTo-Json"
    );
    let mut list = powershell(&script).await?;
    let mut info = list
        .pop()
        .ok_or_else(|| format!("Windows reports no volume {letter}:"))?;
    info.device_node = Some(format!("{letter}:"));
    Ok(info)
}

/// A Windows volume by its GUID, through `Get-Volume | Where-Object`.
async fn windows_volume_by_guid(guid: &str) -> Result<DiskInfo, String> {
    // The GUID comes from the config: only hex digits and dashes go into
    // the command.
    if guid.is_empty() || !guid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(format!("no volume with UUID {guid}"));
    }
    let script = format!(
        "Get-Volume | Where-Object UniqueId -like '*{guid}*' | \
         Select-Object DriveLetter,UniqueId,FileSystemLabel,FileSystem,DriveType | ConvertTo-Json"
    );
    powershell(&script)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| format!("no volume with UUID {guid}"))
}

/// Runs a PowerShell command and reads its `ConvertTo-Json` answer. The
/// output is set to UTF-8 first, so a label in any script survives.
async fn powershell(script: &str) -> Result<Vec<DiskInfo>, String> {
    if interop_off() {
        return Err(INTEROP_OFF.to_string());
    }
    let program = program_on_path("powershell.exe", &[POWERSHELL_FALLBACK]).ok_or_else(|| {
        "powershell.exe was not found, so Windows drives cannot be identified".to_string()
    })?;
    let command = format!("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; {script}");
    let args = [
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        command.as_str(),
    ];
    let captured = run(&program, &args).await.map_err(|e| {
        // With interop off, Linux cannot run a Windows program at all.
        if e.contains("os error 8") || interop_off() {
            INTEROP_OFF.to_string()
        } else {
            e
        }
    })?;
    if !captured.status.success() {
        return Err(failure("PowerShell", &captured.stderr));
    }
    parse_get_volume(&captured.stdout)
}

/// What to say when WSL cannot start Windows programs.
const INTEROP_OFF: &str = "Windows interop is off in this WSL distribution, so OneFolder cannot \
     ask Windows which drive this is. Turn it on with [interop] enabled = true in /etc/wsl.conf, \
     then run wsl --shutdown in Windows and start WSL again.";

/// True when WSL's interop, which runs Windows programs such as
/// PowerShell, is turned off: its binfmt handler says `disabled`. When the
/// handler is not registered at all, starting PowerShell fails with "Exec
/// format error", which [`powershell`] reads the same way.
fn interop_off() -> bool {
    std::fs::read_to_string("/proc/sys/fs/binfmt_misc/WSLInterop")
        .is_ok_and(|text| interop_disabled(&text))
}

/// Reads the WSLInterop handler's state: its first line is `enabled` or
/// `disabled`.
fn interop_disabled(text: &str) -> bool {
    text.lines().next().is_some_and(|l| l.trim() == "disabled")
}

/// Parses `Get-Volume | Select-Object ... | ConvertTo-Json`: one object,
/// or an array of them, possibly after a UTF-8 byte order mark and with
/// CRLF line ends; nothing at all when no volume matched. `UniqueId`
/// (`\\?\Volume{GUID}\`) gives the GUID, `FileSystemLabel` the name, and
/// `DriveLetter` (a string, or a character code) the device `D:`.
pub fn parse_get_volume(text: &str) -> Result<Vec<DiskInfo>, String> {
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let doc: Value = serde_json::from_str(text)
        .map_err(|e| format!("could not read PowerShell's answer: {e}"))?;
    let items = match doc {
        Value::Array(items) => items,
        other => vec![other],
    };
    Ok(items
        .iter()
        .map(|v| {
            let text = |key: &str| {
                v.get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            let letter = match v.get("DriveLetter") {
                Some(Value::String(s)) => s.chars().next(),
                Some(Value::Number(n)) => n.as_u64().and_then(|n| char::from_u32(n as u32)),
                _ => None,
            }
            .filter(char::is_ascii_alphabetic)
            .map(|c| c.to_ascii_uppercase());
            DiskInfo {
                volume_uuid: text("UniqueId").map(|id| volume_guid(&id)),
                disk_uuid: None,
                volume_name: Some(text("FileSystemLabel").unwrap_or_default()),
                device_node: letter.map(|l| format!("{l}:")),
                mount_point: None,
                writable: None,
            }
        })
        .collect())
}

/// The GUID inside `\\?\Volume{GUID}\`, or the whole id when it has none.
fn volume_guid(unique_id: &str) -> String {
    unique_id
        .split_once('{')
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(guid, _)| guid.to_string())
        .unwrap_or_else(|| unique_id.to_string())
}

/// Runs a probe. Its time limit is the drive check's own
/// ([`super::check_deadline`]), which abandons the whole check.
async fn run(program: &Path, args: &[&str]) -> Result<exec::Captured, String> {
    exec::run_capture(program, args)
        .await
        .map_err(|e| e.to_string())
}

/// `name` on PATH, else the first of `fallbacks` that exists, so a trimmed
/// PATH (cron, a systemd unit) still finds it.
fn program_on_path(name: &str, fallbacks: &[&str]) -> Option<PathBuf> {
    detect::find_program(std::env::var_os("PATH").as_deref(), name)
        .or_else(|| fallbacks.iter().map(PathBuf::from).find(|p| p.is_file()))
}

/// A probe's failure in words, from the first line of its error output:
/// `lsblk failed: lsblk: /dev/sdz1: not a block device`.
fn failure(name: &str, stderr: &str) -> String {
    format!(
        "{name} failed: {}",
        stderr.trim().lines().next().unwrap_or("no output")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{is_system_mount_on, Os};

    fn fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/volume")
            .join(name);
        std::fs::read_to_string(path).unwrap()
    }

    fn holder<'a>(entries: &'a [Entry], path: &str) -> &'a Mount {
        &mount_of(entries, Path::new(path), None).unwrap().mount
    }

    /// An Ubuntu 24.04 machine (an OrbStack VM, with the Mac's folders
    /// shared in under virtiofs).
    #[test]
    fn a_plain_machine_has_its_system_disk_at_the_root() {
        let entries = parse_mountinfo(&fixture("mountinfo-ubuntu.txt"));
        assert!(entries.len() > 40, "{}", entries.len());
        let root = holder(&entries, "/home/bora/Documents");
        assert_eq!(root.mount_point, "/");
        assert_eq!(root.fstype, "btrfs");
        assert_eq!(root.device, "/dev/vdb1");
        assert!(!root.read_only);
        assert!(is_system_mount_on(
            Os::Linux,
            &root.mount_point,
            &root.fstype
        ));
        assert_eq!(holder(&entries, "/tmp/x").fstype, "tmpfs");
        assert_eq!(
            holder(&entries, "/mnt/mac/Users/bora").mount_point,
            "/mnt/mac"
        );
        // A prefix by component, not by text.
        assert_eq!(holder(&entries, "/tmpx").mount_point, "/");
        assert!(entries.iter().all(|e| e.drive_letter.is_none()));
    }

    /// The same machine with three drives mounted where udisks puts them,
    /// one with a space in its name and one read-only.
    #[test]
    fn drives_under_media_are_found_with_their_names_unescaped() {
        let entries = parse_mountinfo(&fixture("mountinfo-ubuntu-drives.txt"));
        let backup = holder(&entries, "/media/bora/Backup/Documents");
        assert_eq!(
            backup,
            &Mount {
                mount_point: "/media/bora/Backup".into(),
                device: "/dev/loop0".into(),
                fstype: "ext4".into(),
                read_only: false,
                root: Some("/".into()),
            }
        );
        let passport = holder(&entries, "/media/bora/My Passport/Photos");
        assert_eq!(passport.mount_point, "/media/bora/My Passport");
        assert_eq!(passport.fstype, "exfat");
        assert_eq!(passport.device, "/dev/loop1p1");
        let stick = holder(&entries, "/media/bora/STICK");
        assert!(stick.read_only);
        assert_eq!(stick.fstype, "vfat");
        assert!(!is_system_mount_on(
            Os::Linux,
            &stick.mount_point,
            &stick.fstype
        ));
        // The folder that holds the drives is on the system disk.
        assert_eq!(holder(&entries, "/media/bora").mount_point, "/");
    }

    #[test]
    fn windows_drives_under_wsl_2_are_drvfs() {
        let entries = parse_mountinfo(&fixture("mountinfo-wsl2.txt"));
        let c = mount_of(&entries, Path::new("/mnt/c/Users/bora"), None).unwrap();
        assert_eq!(c.mount.mount_point, "/mnt/c");
        assert_eq!(c.mount.fstype, "9p");
        assert_eq!(c.mount.device, "C:\\");
        assert_eq!(c.drive_letter, Some('C'));
        assert!(is_system_mount_on(Os::Wsl, "/mnt/c", "9p"));
        let d = mount_of(&entries, Path::new("/mnt/d/Backup"), None).unwrap();
        assert_eq!(d.drive_letter, Some('D'));
        assert!(!is_system_mount_on(Os::Wsl, "/mnt/d", "9p"));
        // Mounted by hand later: `sudo mount -t drvfs E: /mnt/e`.
        let e = mount_of(&entries, Path::new("/mnt/e"), None).unwrap();
        assert_eq!(e.drive_letter, Some('E'));
        assert_eq!(e.mount.device, "E:");
        assert_eq!(drvfs_mount_of(&entries, 'e').as_deref(), Some("/mnt/e"));
        assert_eq!(drvfs_mount_of(&entries, 'F'), None);
        let moved = parse_mountinfo(
            "90 62 0:45 / /mnt/x rw,relatime - 9p E: rw,dirsync,aname=drvfs;path=E:;uid=0\n",
        );
        assert_eq!(drvfs_mount_of(&moved, 'E').as_deref(), Some("/mnt/x"));
        // WSL's own 9p mounts are not Windows drives.
        let drivers = mount_of(&entries, Path::new("/usr/lib/wsl/drivers/x"), None).unwrap();
        assert_eq!(drivers.mount.fstype, "9p");
        assert_eq!(drivers.drive_letter, None);
        assert_eq!(holder(&entries, "/home/bora").mount_point, "/");
        assert_eq!(
            holder(&entries, "/mnt/wslg/distro/x").mount_point,
            "/mnt/wslg/distro"
        );
    }

    /// Recorded on WSL 1 (Ubuntu 24.04 on Windows 11), with an exFAT drive
    /// mounted by hand at /mnt/e and a network share at /mnt/winshare.
    #[test]
    fn windows_drives_under_wsl_1_are_drvfs() {
        let entries = parse_mountinfo(&fixture("mountinfo-wsl1.txt"));
        let c = mount_of(&entries, Path::new("/mnt/c/Users"), None).unwrap();
        assert_eq!(c.mount.fstype, "drvfs");
        assert_eq!(c.mount.device, "C:\\");
        assert_eq!(c.drive_letter, Some('C'));
        let e = mount_of(&entries, Path::new("/mnt/e/Backup"), None).unwrap();
        assert_eq!(e.drive_letter, Some('E'));
        assert!(!e.mount.read_only);
        // A share mounted with drvfs has no drive letter.
        let share = mount_of(&entries, Path::new("/mnt/winshare/x"), None).unwrap();
        assert_eq!(share.mount.device, "\\\\Mac\\winshare");
        assert_eq!(share.drive_letter, None);
        assert_eq!(holder(&entries, "/home").fstype, "wslfs");
    }

    /// A tmpfs mounted over `/media/bora` after the drive: the drive's
    /// entry is still listed, deeper, but the path is on the tmpfs, whose
    /// device is the path's own.
    #[test]
    fn a_file_system_mounted_over_the_drive_hides_it() {
        let entries = parse_mountinfo(&fixture("mountinfo-overmount.txt"));
        let path = Path::new("/media/bora/Backup/Docs");
        // By prefix alone, the hidden drive.
        assert_eq!(
            mount_of(&entries, path, None).unwrap().mount.mount_point,
            "/media/bora/Backup"
        );
        // By the path's device (the tmpfs, 0:67), what is really there.
        let on = mount_of(&entries, path, Some((0, 67))).unwrap();
        assert_eq!(on.mount.mount_point, "/media/bora");
        assert_eq!(on.mount.fstype, "tmpfs");
        // The drive's own device finds the drive.
        let drive = mount_of(&entries, path, Some((7, 0))).unwrap();
        assert_eq!(drive.mount.mount_point, "/media/bora/Backup");
        assert_eq!(drive.mount.root.as_deref(), Some("/"));
    }

    /// A bind mount of one of the drive's folders shows that folder as its
    /// root, so it is not the whole drive.
    #[test]
    fn a_bind_mount_shows_its_folder_as_the_root() {
        let entries = parse_mountinfo(&fixture("mountinfo-bind.txt"));
        let other = mount_of(&entries, Path::new("/media/bora/Other"), Some((7, 0))).unwrap();
        assert_eq!(other.mount.mount_point, "/media/bora/Other");
        assert_eq!(other.mount.root.as_deref(), Some("/Docs"));
        assert_eq!(other.mount.device, "/dev/loop0");
        assert_eq!(other.dev, (7, 0));
    }

    /// Two btrfs subvolumes mounted in turn at one mount point: the same
    /// device and file system, told apart by the root.
    #[test]
    fn btrfs_subvolumes_differ_by_their_root() {
        let a = parse_mountinfo(&fixture("mountinfo-subvolume-a.txt"));
        let b = parse_mountinfo(&fixture("mountinfo-subvolume-b.txt"));
        let path = Path::new("/media/bora/Pool/x");
        // A subvolume's own device (0:69) is not mountinfo's (0:67): the
        // longest prefix is taken.
        let on_a = mount_of(&a, path, Some((0, 69))).unwrap();
        let on_b = mount_of(&b, path, Some((0, 69))).unwrap();
        assert_eq!(on_a.mount.mount_point, on_b.mount.mount_point);
        assert_eq!(on_a.mount.device, on_b.mount.device);
        assert_eq!(on_a.mount.root.as_deref(), Some("/@a"));
        assert_eq!(on_b.mount.root.as_deref(), Some("/@b"));
    }

    #[test]
    fn device_numbers_split_as_glibc_does() {
        assert_eq!(split_dev(0x0700), (7, 0));
        assert_eq!(split_dev(0x0803), (8, 3));
        assert_eq!(split_dev(67), (0, 67));
        // Large minors and majors use the high bits.
        assert_eq!(split_dev((259 << 8) | (1 << 20) | 0x05), (259, 0x105));
    }

    /// A file system whose superblock is read-only (ext4 after an error)
    /// while the mount's own options still say rw.
    #[test]
    fn a_read_only_superblock_is_read_only() {
        let entries = parse_mountinfo(
            "124 181 7:1 / /media/bora/Backup rw,relatime shared:215 - ext4 /dev/loop1 ro\n",
        );
        assert!(entries[0].mount.read_only);
        let rw = parse_mountinfo(
            "124 181 7:1 / /media/bora/Backup rw,relatime shared:215 - ext4 /dev/loop1 rw,errors=remount-ro\n",
        );
        assert!(!rw[0].mount.read_only);
    }

    #[test]
    fn mountinfo_quirks() {
        assert_eq!(unescape("My\\040Passport"), "My Passport");
        assert_eq!(unescape("C:\\134"), "C:\\");
        assert_eq!(unescape("tab\\011and\\012line"), "tab\tand\nline");
        assert_eq!(unescape("C:\\;uid"), "C:\\;uid", "not an escape");
        assert_eq!(unescape("end\\04"), "end\\04");
        // Optional fields before the dash; garbage is skipped.
        let entries = parse_mountinfo(
            "36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 shared:2 - ext3 /dev/root rw,errors=continue\n\
             nonsense\n\
             1 2 3 4 5 6\n",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mount.mount_point, "/mnt2");
        assert_eq!(entries[0].mount.fstype, "ext3");
        // Of two mounts on one point, the later is on top.
        let stacked = parse_mountinfo(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n\
             2 1 8:17 / /media/u/B rw - ext4 /dev/sdb1 rw\n\
             3 1 8:33 / /media/u/B ro - vfat /dev/sdc1 rw\n",
        );
        assert_eq!(holder(&stacked, "/media/u/B/x").device, "/dev/sdc1");
    }

    #[test]
    fn lsblk_describes_an_ext4_drive_with_a_label() {
        let info = parse_lsblk(&fixture("lsblk-ext4.json")).unwrap();
        assert_eq!(
            info,
            [DiskInfo {
                volume_uuid: Some("87c01f90-c751-4b21-858a-300fc947ee16".into()),
                disk_uuid: None,
                volume_name: Some("Backup".into()),
                device_node: Some("/dev/loop0".into()),
                mount_point: Some("/media/bora/Backup".into()),
                writable: Some(true),
            }]
        );
    }

    #[test]
    fn lsblk_gives_an_exfat_partition_uuid() {
        let info = parse_lsblk(&fixture("lsblk-exfat-gpt.json")).unwrap();
        assert_eq!(info[0].volume_uuid.as_deref(), Some("FAF5-F3D8"));
        assert_eq!(
            info[0].disk_uuid.as_deref(),
            Some("2085294b-a85d-4187-b78f-0874aac3538d")
        );
        assert_eq!(info[0].volume_name.as_deref(), Some("My Passport"));
        // Without a file system UUID, the partition's is the proof.
        let info = parse_lsblk(&fixture("lsblk-exfat-no-uuid.json")).unwrap();
        assert_eq!(info[0].volume_uuid, None);
        assert!(info[0].disk_uuid.is_some());
    }

    #[test]
    fn lsblk_describes_a_fat_stick() {
        let info = parse_lsblk(&fixture("lsblk-vfat.json")).unwrap();
        assert_eq!(info[0].volume_uuid.as_deref(), Some("3C49-0C2E"));
        assert_eq!(info[0].volume_name.as_deref(), Some("STICK"));
        assert_eq!(info[0].disk_uuid, None);
    }

    #[test]
    fn lsblk_of_every_device_finds_a_uuid_and_where_it_is_mounted() {
        let all = parse_lsblk(&fixture("lsblk-all.json")).unwrap();
        let passport = all
            .iter()
            .find(|d| d.volume_uuid.as_deref() == Some("FAF5-F3D8"))
            .unwrap();
        assert_eq!(
            passport.mount_point.as_deref(),
            Some("/media/bora/My Passport")
        );
        assert_eq!(passport.device_node.as_deref(), Some("/dev/loop1p1"));
        // Swap and devices mounted nowhere have no mount point.
        for d in &all {
            if let Some(m) = &d.mount_point {
                assert!(m.starts_with('/'), "{d:?}");
            }
        }
        let unmounted = all
            .iter()
            .find(|d| d.device_node.as_deref() == Some("/dev/nbd0"))
            .unwrap();
        assert_eq!(unmounted.mount_point, None);
        assert_eq!(unmounted.volume_uuid, None);
    }

    #[test]
    fn lsblk_older_and_nested_forms() {
        let old = r#"{"blockdevices": [
            {"path":"/dev/sdb", "uuid":null, "partuuid":null, "label":null, "fstype":null,
             "ro":"0", "mountpoint":null, "children": [
                {"path":"/dev/sdb1", "uuid":"A-1", "partuuid":"P-1", "label":"Old",
                 "fstype":"ext4", "ro":"1", "mountpoint":"/media/Old"}
             ]}
        ]}"#;
        let info = parse_lsblk(old).unwrap();
        assert_eq!(info.len(), 2);
        assert_eq!(info[0].writable, Some(true));
        assert_eq!(info[1].device_node.as_deref(), Some("/dev/sdb1"));
        assert_eq!(info[1].mount_point.as_deref(), Some("/media/Old"));
        assert_eq!(info[1].writable, Some(false));
        assert!(parse_lsblk("not json").is_err());
        assert!(parse_lsblk("{}").is_err());
    }

    #[test]
    fn lsblk_errors_are_reported_in_words() {
        assert_eq!(
            failure("lsblk", &fixture("lsblk-missing.txt")),
            "lsblk failed: lsblk: /dev/nosuchdevice: not a block device"
        );
        assert_eq!(failure("lsblk", ""), "lsblk failed: no output");
    }

    #[test]
    fn get_volume_names_a_windows_drive() {
        let info = parse_get_volume(&fixture("get-volume-d.json")).unwrap();
        assert_eq!(
            info,
            [DiskInfo {
                volume_uuid: Some("3f2504e0-4f89-11d3-9a0c-0305e82c3301".into()),
                disk_uuid: None,
                volume_name: Some("Backup".into()),
                device_node: Some("D:".into()),
                mount_point: None,
                writable: None,
            }]
        );
        let list = parse_get_volume(&fixture("get-volume-list.json")).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].volume_name.as_deref(), Some("\u{5199}\u{771f}"));
        assert_eq!(list[0].device_node.as_deref(), Some("E:"));
        assert_eq!(
            list[1].device_node.as_deref(),
            Some("F:"),
            "a character code"
        );
        assert_eq!(list[1].volume_name.as_deref(), Some(""));
        assert_eq!(
            list[1].volume_uuid.as_deref(),
            Some("a1b2c3d4-0000-11ef-8a9b-000000000003")
        );
        // Recorded on WSL 1: CRLF, no byte order mark, DriveType a word.
        let e = parse_get_volume(&fixture("get-volume-wsl1-e.json")).unwrap();
        assert_eq!(
            e,
            [DiskInfo {
                volume_uuid: Some("83fa734b-0000-0000-0000-010000000000".into()),
                disk_uuid: None,
                volume_name: Some("Backup".into()),
                device_node: Some("E:".into()),
                mount_point: None,
                writable: None,
            }]
        );
        assert_eq!(
            failure("PowerShell", &fixture("get-volume-missing.txt")),
            "PowerShell failed: Get-Volume : No MSFT_Volume objects found with property \
             'DriveLetter' equal to 'Q'.  Verify the value of the property"
        );
        // No volume matched: no output at all.
        assert_eq!(parse_get_volume("\u{feff}\r\n").unwrap(), []);
        assert!(parse_get_volume("Get-Volume : No MSFT_Volume objects found").is_err());
        assert_eq!(volume_guid("no braces"), "no braces");
    }

    #[test]
    fn interop_state_is_read() {
        assert!(interop_disabled("disabled\ninterpreter /init\nflags: P\n"));
        assert!(!interop_disabled("enabled\ninterpreter /init\nflags: P\n"));
        assert!(!interop_disabled(""));
        assert!(INTEROP_OFF.contains("[interop] enabled = true in /etc/wsl.conf"));
    }

    #[test]
    fn drvfs_is_recognised() {
        assert!(is_drvfs("drvfs", ""));
        assert!(is_drvfs("9p", "rw,dirsync,aname=drvfs;path=C:\\;uid=1000"));
        assert!(!is_drvfs("9p", "ro,dirsync,aname=drivers;fmask=222"));
        assert!(!is_drvfs("ext4", "aname=drvfs"));
        assert_eq!(
            drvfs_letter("rw,aname=drvfs;path=d:\\;uid=0", "", ""),
            Some('D')
        );
        assert_eq!(drvfs_letter("rw", "G:\\", ""), Some('G'));
        assert_eq!(drvfs_letter("rw", "drvfs", "/mnt/h"), Some('H'));
        assert_eq!(drvfs_letter("rw", "drvfs", "/media/x"), None);
    }

    /// The real machine: these run on Linux only.
    #[cfg(target_os = "linux")]
    mod live {
        use super::super::super::{check, identify, Identity};
        use super::*;

        #[test]
        fn the_root_is_a_mount_point() {
            let root = statfs(Path::new("/")).unwrap();
            assert_eq!(root.mount_point, "/");
            assert!(mounts().unwrap().iter().any(|m| m.mount_point == "/"));
            assert_eq!(
                statfs(Path::new("/no/such/path")).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
            // A symlink is measured where it leads.
            let dir = tempfile::tempdir().unwrap();
            let link = dir.path().join("root");
            std::os::unix::fs::symlink("/", &link).unwrap();
            assert_eq!(statfs(&link).unwrap().mount_point, "/");
        }

        #[tokio::test]
        async fn the_system_disk_is_never_recorded() {
            let dir = tempfile::tempdir().unwrap();
            let found = identify(&dir.path().join("not/yet")).await.unwrap();
            assert_eq!(found.identity, None, "{found:?}");
        }

        #[tokio::test]
        async fn a_folder_under_media_on_the_system_disk_is_refused() {
            let user = crate::platform::user_name().unwrap_or_else(|| "nobody".into());
            let path = format!("/media/{user}/OneFolderNoSuchDrive/Docs");
            let err = identify(Path::new(&path)).await.unwrap_err();
            assert_eq!(err.code(), "not-mounted", "{err:?}");
        }

        /// The unplugged drive: the recorded mount point is a plain folder.
        #[tokio::test]
        async fn an_unplugged_drive_is_not_mounted() {
            let dir = tempfile::tempdir().unwrap();
            let drive = dir.path().join("Backup");
            let docs = drive.join("Documents");
            std::fs::create_dir_all(&docs).unwrap();
            let expected = Identity {
                mount_point: drive.display().to_string(),
                volume_name: "Backup".into(),
                volume_uuid: Some("87c01f90-c751-4b21-858a-300fc947ee16".into()),
                partition_uuid: None,
                device: None,
                mount_root: None,
            };
            let err = check(&docs, &expected, true).await.unwrap_err();
            assert_eq!(err.code(), "not-mounted", "{err}");
            let flat = err
                .to_string()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(flat.contains("is currently a folder on"), "{flat}");
            assert!(flat.ends_with("Plug the drive in and run again."), "{flat}");
        }

        /// A UUID no drive has is looked for and not found.
        #[tokio::test]
        async fn an_unknown_uuid_is_nowhere() {
            assert!(disk_info("00000000-0000-0000-0000-00000000dead")
                .await
                .is_err());
        }
    }
}
