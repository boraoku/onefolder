//! Volume identity on Windows, from the Win32 volume calls (no PowerShell,
//! so no wait for it to start): the volume a path is on
//! (`GetVolumePathNameW`), its GUID (`GetVolumeNameForVolumeMountPointW`),
//! label, file system and read-only flag (`GetVolumeInformationW`), and the
//! drive letters a volume has now (`GetVolumePathNamesForVolumeNameW`).
//!
//! The identity recorded is the volume GUID, the same one WSL reads with
//! `Get-Volume` (its `UniqueId`, `\\?\Volume{GUID}\`): Windows gives each
//! volume one and keeps it whatever letter the drive gets, so a drive
//! plugged in as `E:` instead of `D:` is still found. A network drive or
//! share has no GUID and gets the weak `<fstype>:<share>` identity.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, GetVolumePathNamesForVolumeNameW,
};
use windows_sys::Win32::System::Diagnostics::Debug::SetThreadErrorMode;

use super::{DiskInfo, Mount, Space};

/// `GetDriveTypeW`: no volume at that root.
const DRIVE_NO_ROOT_DIR: u32 = 1;
/// `GetDriveTypeW`: a network drive.
const DRIVE_REMOTE: u32 = 4;
/// `GetVolumeInformationW`'s flag for a read-only volume.
const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;
/// `SetThreadErrorMode`: fail instead of asking to insert a disk.
const SEM_FAILCRITICALERRORS: u32 = 0x0001;
/// The file system type given to a network drive or share.
pub const NETWORK_FSTYPE: &str = "smb";
/// Room for a path, in UTF-16 units (long paths included).
const PATH_ROOM: usize = 32 * 1024;

/// A path as the NUL-terminated UTF-16 the W calls take.
fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// The text before the first NUL of a UTF-16 buffer.
fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..len])
        .to_string_lossy()
        .into_owned()
}

/// Turns off the "insert a disk" dialog for this thread while alive, so
/// an empty card reader fails at once.
struct QuietErrors;

impl QuietErrors {
    fn new() -> QuietErrors {
        // SAFETY: plain call; the old mode is not asked for.
        unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, std::ptr::null_mut()) };
        QuietErrors
    }
}

impl Drop for QuietErrors {
    fn drop(&mut self) {
        // SAFETY: as in `new`.
        unsafe { SetThreadErrorMode(0, std::ptr::null_mut()) };
    }
}

/// The root of the volume holding `path`: `D:\`, `C:\` or `\\nas\share\`
/// (or the folder a volume is mounted at), with the drive letter in upper
/// case.
fn volume_path(path: &Path) -> io::Result<String> {
    let name = wide(path.as_os_str());
    let mut buf = vec![0u16; PATH_ROOM];
    // SAFETY: `name` is NUL-terminated and `buf` holds the length passed.
    let ok = unsafe { GetVolumePathNameW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut root = from_wide(&buf);
    if !root.ends_with('\\') {
        root.push('\\');
    }
    Ok(upper_drive(&root))
}

/// `d:\` as `D:\`; other roots unchanged.
fn upper_drive(root: &str) -> String {
    let bytes = root.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        format!("{}{}", (bytes[0] as char).to_ascii_uppercase(), &root[1..])
    } else {
        root.to_string()
    }
}

/// The volume GUID path (`\\?\Volume{GUID}\`) of the volume at `root`.
/// `None` for a network share, which has none.
fn volume_guid_path(root: &str) -> Option<String> {
    let name = wide(OsStr::new(root));
    let mut buf = [0u16; 64];
    // SAFETY: `name` is NUL-terminated and `buf` holds the length passed
    // (a GUID path is 49 units with its NUL).
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    (ok != 0).then(|| from_wide(&buf))
}

/// The GUID in a volume GUID path, lower case: `3f2504e0-...` from
/// `\\?\Volume{3F2504E0-...}\`.
pub fn guid_of(volume: &str) -> Option<String> {
    let start = volume.find('{')? + 1;
    let end = start + volume[start..].find('}')?;
    let guid = &volume[start..end];
    is_guid(guid).then(|| guid.to_ascii_lowercase())
}

/// True for a bare GUID such as `3f2504e0-4f89-11d3-9a0c-0305e82c3301`.
pub fn is_guid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// What `GetVolumeInformationW` says about the volume at `root`.
struct VolumeInformation {
    label: String,
    fstype: String,
    read_only: bool,
}

fn volume_information(root: &str) -> io::Result<VolumeInformation> {
    let name = wide(OsStr::new(root));
    let mut label = [0u16; 261];
    let mut fstype = [0u16; 261];
    let mut flags = 0u32;
    // SAFETY: `name` is NUL-terminated, both buffers hold the lengths
    // passed, and the serial number and component length are not asked for.
    let ok = unsafe {
        GetVolumeInformationW(
            name.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            fstype.as_mut_ptr(),
            fstype.len() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(VolumeInformation {
        label: from_wide(&label),
        fstype: from_wide(&fstype).to_ascii_lowercase(),
        read_only: flags & FILE_READ_ONLY_VOLUME != 0,
    })
}

/// True for a network drive (`Z:\` mapped to a share) or a share itself.
fn is_remote(root: &str) -> bool {
    if root.starts_with(r"\\") && !root.starts_with(r"\\?\") {
        return true;
    }
    let name = wide(OsStr::new(root));
    // SAFETY: `name` is NUL-terminated.
    unsafe { GetDriveTypeW(name.as_ptr()) == DRIVE_REMOTE }
}

/// The drive letters and folders a volume is mounted at now, the first
/// being the one to name it by (`E:\`).
fn mount_points(volume: &str) -> Vec<String> {
    let name = wide(OsStr::new(volume));
    let mut buf = vec![0u16; 4096];
    let mut needed = 0u32;
    // SAFETY: `name` is NUL-terminated and `buf` holds the length passed;
    // the answer is a list of NUL-terminated strings ending with an empty one.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(
            name.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return Vec::new();
    }
    buf.split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(|s| upper_drive(&String::from_utf16_lossy(s)))
        .collect()
}

/// The volume holding `path`: where it is mounted, its GUID path as the
/// device (a share's own name for a network drive), file system and
/// read-only flag.
pub fn statfs(path: &Path) -> io::Result<Mount> {
    let _quiet = QuietErrors::new();
    let mount_point = volume_path(path)?;
    let info = volume_information(&mount_point)?;
    let (device, fstype) = if is_remote(&mount_point) {
        (mount_point.clone(), NETWORK_FSTYPE.to_string())
    } else {
        let device = volume_guid_path(&mount_point).ok_or_else(io::Error::last_os_error)?;
        (device, info.fstype)
    };
    Ok(Mount {
        mount_point,
        device,
        fstype,
        read_only: info.read_only,
        root: None,
    })
}

/// What Windows says about a volume, found by its mount point (`D:\`) or
/// by its bare GUID (for a drive that now has another letter, or none).
pub async fn disk_info(target: &str) -> Result<DiskInfo, String> {
    let target = target.to_string();
    tokio::task::spawn_blocking(move || disk_info_now(&target))
        .await
        .map_err(|e| e.to_string())?
}

fn disk_info_now(target: &str) -> Result<DiskInfo, String> {
    let _quiet = QuietErrors::new();
    let (volume, mount_point) = if is_guid(target) {
        let volume = format!(r"\\?\Volume{{{target}}}\");
        let mount_point = mount_points(&volume).into_iter().next();
        (volume, mount_point)
    } else {
        let mount_point = volume_path(Path::new(target)).map_err(|e| e.to_string())?;
        let volume = volume_guid_path(&mount_point)
            .ok_or_else(|| format!("Windows has no volume GUID for {mount_point}"))?;
        (volume, Some(mount_point))
    };
    let info = volume_information(&volume).map_err(|e| {
        if is_guid(target) {
            format!("no volume with UUID {target}")
        } else {
            e.to_string()
        }
    })?;
    Ok(DiskInfo {
        volume_uuid: guid_of(&volume),
        disk_uuid: None,
        volume_name: Some(info.label),
        device_node: Some(volume),
        mount_point,
        writable: Some(!info.read_only),
    })
}

/// Every drive letter with a volume in it, network drives left out (they
/// are never a drive moved to another letter, and can be slow to answer).
pub fn mounts() -> io::Result<Vec<Mount>> {
    // SAFETY: plain call.
    let letters = unsafe { GetLogicalDrives() };
    let mut found = Vec::new();
    for (i, letter) in ('A'..='Z').enumerate() {
        if letters & (1 << i) == 0 {
            continue;
        }
        let root = format!(r"{letter}:\");
        let name = wide(OsStr::new(&root));
        // SAFETY: `name` is NUL-terminated.
        let kind = unsafe { GetDriveTypeW(name.as_ptr()) };
        if kind == DRIVE_NO_ROOT_DIR || kind == DRIVE_REMOTE {
            continue;
        }
        // An empty card reader or DVD drive has no volume to describe.
        if let Ok(mount) = statfs(Path::new(&root)) {
            found.push(mount);
        }
    }
    Ok(found)
}

/// Free and used space on the volume holding `path`.
pub fn space(path: &Path) -> io::Result<Space> {
    let _quiet = QuietErrors::new();
    let name = wide(path.as_os_str());
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `name` is NUL-terminated and the three outputs are writable.
    let ok = unsafe { GetDiskFreeSpaceExW(name.as_ptr(), &mut available, &mut total, &mut free) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Space {
        free: available,
        used: total.saturating_sub(free),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids() {
        let path = r"\\?\Volume{3F2504E0-4F89-11D3-9A0C-0305E82C3301}\";
        assert_eq!(
            guid_of(path).as_deref(),
            Some("3f2504e0-4f89-11d3-9a0c-0305e82c3301")
        );
        assert!(is_guid("3f2504e0-4f89-11d3-9a0c-0305e82c3301"));
        assert!(!is_guid(r"D:\"));
        assert!(!is_guid("3f2504e0-4f89-11d3-9a0c"));
        assert_eq!(guid_of(r"D:\"), None);
        assert_eq!(upper_drive(r"d:\"), r"D:\");
        assert_eq!(upper_drive(r"\\nas\share\"), r"\\nas\share\");
    }

    /// The temporary folder's volume, as this computer has it.
    #[tokio::test]
    async fn the_temporary_folders_volume() {
        let dir = tempfile::tempdir().unwrap();
        let mount = statfs(dir.path()).unwrap();
        assert!(
            mount.mount_point.ends_with('\\'),
            "a root: {}",
            mount.mount_point
        );
        assert!(
            dir.path()
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with(&mount.mount_point.to_ascii_uppercase()),
            "{} is on {}",
            dir.path().display(),
            mount.mount_point
        );
        assert!(mount.device.starts_with(r"\\?\Volume{"), "{}", mount.device);
        assert!(!mount.fstype.is_empty());

        let info = disk_info(&mount.mount_point).await.unwrap();
        let guid = info.volume_uuid.clone().expect("a GUID");
        assert_eq!(
            info.mount_point.as_deref(),
            Some(mount.mount_point.as_str())
        );
        assert_eq!(info.device_node.as_deref(), Some(mount.device.as_str()));

        // Found again by its GUID, at the same letter.
        let again = disk_info(&guid).await.unwrap();
        assert_eq!(again.volume_uuid.as_deref(), Some(guid.as_str()));
        assert_eq!(again.mount_point, info.mount_point);

        assert!(mounts()
            .unwrap()
            .iter()
            .any(|m| m.mount_point == mount.mount_point));
        let space = space(dir.path()).unwrap();
        assert!(space.free > 0 && space.used > 0, "{space:?}");
        assert!(disk_info("00000000-0000-0000-0000-000000000000")
            .await
            .is_err());
    }
}
