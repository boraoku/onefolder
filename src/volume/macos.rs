//! macOS volume identity: mount point and device from `statfs`, name, UUID,
//! device node and writability from `diskutil info -plist`.
//!
//! `statfs` is called through the libc that the `nix` crate re-exports,
//! because nix's safe `Statfs` wrapper does not expose `f_mntonname` and
//! `f_mntfromname`. Reading `f_mntonname` is also what makes firmlinks come
//! out right: `/Users/bora` reports `/System/Volumes/Data`, whereas walking
//! up until the device changes would stop at the firmlink `/Users`.

use std::io;
use std::path::Path;
use std::time::Duration;

use nix::errno::Errno;
use nix::libc;
use nix::NixPath;
use serde::Deserialize;

use super::{DiskInfo, Mount};
use crate::rclone::exec;

/// Where macOS keeps diskutil. An absolute path, so a trimmed PATH (as in
/// cron, or the tests) still finds it.
const DISKUTIL: &str = "/usr/sbin/diskutil";

/// A drive that is spinning up can take a few seconds; a hung one never answers.
const DISKUTIL_TIMEOUT: Duration = Duration::from_secs(20);

/// `statfs(2)` on `path`.
pub fn statfs(path: &Path) -> io::Result<Mount> {
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    let res = path
        .with_nix_path(|p| {
            // SAFETY: `p` is a NUL-terminated path that outlives the call and
            // `buf` points to writable memory the size of `struct statfs`.
            unsafe { libc::statfs(p.as_ptr(), buf.as_mut_ptr()) }
        })
        .map_err(io::Error::from)?;
    Errno::result(res).map_err(io::Error::from)?;
    // SAFETY: the buffer started zeroed (a valid `struct statfs`) and the
    // call succeeded, so the kernel filled it in.
    let st = unsafe { buf.assume_init() };
    Ok(Mount {
        mount_point: c_string(&st.f_mntonname),
        device: c_string(&st.f_mntfromname),
        fstype: c_string(&st.f_fstypename),
        read_only: st.f_flags & (libc::MNT_RDONLY as u32) != 0,
        root: None,
    })
}

/// Every mounted file system (`getfsstat(2)`), without waiting on any of
/// them (`MNT_NOWAIT`), so a hung network share cannot stall the lookup.
pub fn mounts() -> io::Result<Vec<Mount>> {
    // SAFETY: a null buffer asks only for the count.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let count = usize::try_from(Errno::result(count).map_err(io::Error::from)?).unwrap_or(0);
    // Room for a few more, in case a drive is mounted in between.
    let room = count + 8;
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(room);
    let bytes = libc::c_int::try_from(room * std::mem::size_of::<libc::statfs>())
        .map_err(|_| io::Error::other("too many mounted file systems"))?;
    // SAFETY: `buf` has capacity for `room` entries and `bytes` says so;
    // the kernel writes at most that many and returns how many it wrote.
    let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    let got = usize::try_from(Errno::result(got).map_err(io::Error::from)?).unwrap_or(0);
    // SAFETY: the first `got` entries were written by the kernel, and `got`
    // never exceeds the capacity it was given.
    unsafe { buf.set_len(got.min(room)) };
    Ok(buf
        .iter()
        .map(|st| Mount {
            mount_point: c_string(&st.f_mntonname),
            device: c_string(&st.f_mntfromname),
            fstype: c_string(&st.f_fstypename),
            read_only: st.f_flags & (libc::MNT_RDONLY as u32) != 0,
            root: None,
        })
        .collect())
}

/// A fixed-size, NUL-terminated C char array as a Rust string.
fn c_string(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Runs `diskutil info -plist <mount point>` and parses the answer.
pub async fn disk_info(mount_point: &str) -> Result<DiskInfo, String> {
    let program = Path::new(DISKUTIL);
    let args = ["info", "-plist", mount_point];
    let run = exec::run_capture(program, &args);
    let captured = tokio::time::timeout(DISKUTIL_TIMEOUT, run)
        .await
        .map_err(|_| {
            format!(
                "diskutil did not answer within {} seconds",
                DISKUTIL_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| e.to_string())?;
    // diskutil reports errors as a plist too, with exit code 1.
    match parse_diskutil(captured.stdout.as_bytes()) {
        Ok(info) => Ok(info),
        Err(e) if captured.status.success() => Err(e),
        Err(_) => Err(format!(
            "diskutil failed: {}",
            captured.stderr.trim().lines().next().unwrap_or("no output")
        )),
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawInfo {
    #[serde(rename = "VolumeUUID")]
    volume_uuid: Option<String>,
    #[serde(rename = "DiskUUID")]
    disk_uuid: Option<String>,
    #[serde(rename = "VolumeName")]
    volume_name: Option<String>,
    #[serde(rename = "DeviceNode")]
    device_node: Option<String>,
    #[serde(rename = "MountPoint")]
    mount_point: Option<String>,
    #[serde(rename = "WritableVolume")]
    writable_volume: Option<bool>,
    #[serde(rename = "Error")]
    error: Option<bool>,
    #[serde(rename = "ErrorMessage")]
    error_message: Option<String>,
}

/// Parses `diskutil info -plist` output. diskutil's own error plist
/// (`Error = true`, `ErrorMessage`) becomes an `Err` with that message.
pub fn parse_diskutil(bytes: &[u8]) -> Result<DiskInfo, String> {
    let raw: RawInfo =
        plist::from_bytes(bytes).map_err(|e| format!("could not read diskutil's answer: {e}"))?;
    if raw.error == Some(true) {
        return Err(raw
            .error_message
            .unwrap_or_else(|| "diskutil reported an error".to_string()));
    }
    let non_empty = |s: Option<String>| s.filter(|v| !v.is_empty());
    Ok(DiskInfo {
        volume_uuid: non_empty(raw.volume_uuid),
        disk_uuid: non_empty(raw.disk_uuid),
        volume_name: raw.volume_name,
        device_node: non_empty(raw.device_node),
        mount_point: non_empty(raw.mount_point),
        writable: raw.writable_volume,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{check, identify, identify_any, is_startup, recordable, Identity};
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/volume")
            .join(name);
        std::fs::read(path).unwrap()
    }

    #[test]
    fn parses_the_boot_volume() {
        let info = parse_diskutil(&fixture("diskutil-boot.plist")).unwrap();
        assert_eq!(
            info,
            DiskInfo {
                volume_uuid: Some("3D1B1A9E-8462-41B3-B631-A8CDA508AD0A".into()),
                disk_uuid: Some("3D1B1A9E-8462-41B3-B631-A8CDA508AD0A".into()),
                volume_name: Some("Macintosh HD".into()),
                device_node: Some("/dev/disk3s1s1".into()),
                mount_point: Some("/".into()),
                writable: Some(false),
            }
        );
    }

    #[test]
    fn parses_an_exfat_volume_without_uuid() {
        let info = parse_diskutil(&fixture("diskutil-exfat.plist")).unwrap();
        assert_eq!(info.volume_uuid, None);
        assert_eq!(info.disk_uuid, None);
        assert_eq!(info.volume_name.as_deref(), Some("BACKUP"));
        assert_eq!(info.device_node.as_deref(), Some("/dev/disk4s1"));
        assert_eq!(info.writable, Some(true));
    }

    #[test]
    fn parses_an_exfat_volume_with_a_partition_uuid() {
        let info = parse_diskutil(&fixture("diskutil-exfat-gpt.plist")).unwrap();
        assert_eq!(info.volume_uuid, None);
        assert_eq!(
            info.disk_uuid.as_deref(),
            Some("7C3457EF-0000-11AA-AA11-00306543ECAC")
        );
    }

    #[test]
    fn diskutil_errors_are_reported() {
        let err = parse_diskutil(&fixture("diskutil-error.plist")).unwrap_err();
        assert_eq!(err, "Could not find disk: /Users");
        assert!(parse_diskutil(b"not a plist").is_err());
    }

    #[test]
    fn the_mount_table_lists_the_root() {
        let all = mounts().unwrap();
        assert!(all.iter().any(|m| m.mount_point == "/"), "{all:?}");
    }

    #[test]
    fn statfs_walks_to_the_real_mount_point() {
        // The root is a mount point of its own.
        let root = statfs(Path::new("/")).unwrap();
        assert_eq!(root.mount_point, "/");
        assert!(root.device.starts_with("/dev/"), "{root:?}");
        assert_eq!(root.fstype, "apfs");

        // A temp dir sits on the data volume, not on the firmlink above it.
        let dir = tempfile::tempdir().unwrap();
        let m = statfs(dir.path()).unwrap();
        assert!(is_startup(&m.mount_point, &m.fstype), "{m:?}");
        assert_ne!(m.mount_point, dir.path().display().to_string());

        assert_eq!(
            statfs(Path::new("/no/such/path")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    /// The startup disk's identity, recorded anyway, for the checks below.
    async fn startup_identity(path: &Path) -> Identity {
        identify_any(path).await.unwrap().0
    }

    #[tokio::test]
    async fn the_startup_disk_is_never_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let found = identify(&dir.path().join("not/yet/created")).await.unwrap();
        assert_eq!(found.identity, None);
        assert!(is_startup(&found.mount_point, "apfs"), "{found:?}");
        assert!(!found.read_only);
        // Measured through the closest existing parent.
        let id = startup_identity(&dir.path().join("not/yet")).await;
        assert_eq!(id, startup_identity(dir.path()).await);
        assert!(id.volume_uuid.is_some(), "{id:?}");
        assert_eq!(id, recordable(&id));
    }

    #[tokio::test]
    async fn a_folder_under_volumes_on_the_startup_disk_is_refused() {
        // /Volumes itself lives on the startup disk.
        let err = identify(Path::new("/Volumes/OneFolderNoSuchDrive/Docs"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "not-mounted", "{err:?}");
    }

    #[tokio::test]
    async fn the_recorded_volume_passes_its_check() {
        let dir = tempfile::tempdir().unwrap();
        let id = startup_identity(dir.path()).await;
        check(dir.path(), &id, true).await.unwrap();
        // A missing folder on the right volume is "missing", not "not mounted".
        let err = check(&dir.path().join("gone"), &id, true)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "missing", "{err}");
    }

    /// The unplugged drive: the recorded mount point is now a plain folder
    /// on the startup disk.
    #[tokio::test]
    async fn an_unplugged_drive_is_not_mounted() {
        let dir = tempfile::tempdir().unwrap();
        let drive = dir.path().join("Backup");
        let docs = drive.join("Documents");
        std::fs::create_dir_all(&docs).unwrap();
        let expected = Identity {
            mount_point: drive.display().to_string(),
            volume_name: "Backup".into(),
            volume_uuid: Some("A1B2C3D4-0000-4E5F-8A9B-000000000001".into()),
            partition_uuid: None,
            device: None,
            mount_root: None,
        };
        let err = check(&docs, &expected, true).await.unwrap_err();
        assert_eq!(err.code(), "not-mounted");
        let text = err.to_string();
        assert!(text.starts_with("Backup drive not mounted\n"), "{text}");
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains(&format!(
                "Destination {} expects volume \"Backup\" (UUID A1B2C3D4-...).",
                docs.display()
            )),
            "{flat}"
        );
        assert!(
            flat.contains(&format!("{} is currently a folder on", drive.display())),
            "{flat}"
        );
        assert!(flat.ends_with("Plug the drive in and run again."), "{flat}");

        // And when the mount point is gone altogether.
        let gone = Identity {
            mount_point: dir.path().join("Nope").display().to_string(),
            ..expected.clone()
        };
        let err = check(&docs, &gone, false).await.unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    /// The right mount point but another drive: the UUID gives it away,
    /// whether or not the folder is there.
    #[tokio::test]
    async fn a_different_volume_at_the_mount_point_is_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let mut id = startup_identity(dir.path()).await;
        id.volume_uuid = Some("00000000-0000-0000-0000-000000000000".into());
        let err = check(dir.path(), &id, false).await.unwrap_err();
        assert_eq!(err.code(), "wrong-volume", "{err}");

        // An impostor of the same name, and the folder absent: still the
        // wrong volume, never "folder missing".
        let err = check(&dir.path().join("absent"), &id, true)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "wrong-volume", "{err}");

        // A partition UUID is compared the same way.
        let mut by_partition = startup_identity(dir.path()).await;
        by_partition.volume_uuid = None;
        by_partition.partition_uuid = Some("00000000-0000-0000-0000-000000000000".into());
        let err = check(&dir.path().join("absent"), &by_partition, true)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "wrong-volume", "{err}");

        // Without either UUID the device node and name must both match.
        let mut weak = startup_identity(dir.path()).await;
        weak.volume_uuid = None;
        weak.device = Some("/dev/disk99s9".into());
        let err = check(dir.path(), &weak, false).await.unwrap_err();
        assert_eq!(err.code(), "wrong-volume", "{err}");
    }

    #[tokio::test]
    async fn the_sealed_system_volume_is_read_only() {
        let found = identify(Path::new("/")).await.unwrap();
        assert!(found.read_only);
        let id = startup_identity(Path::new("/")).await;
        assert_eq!(id.mount_point, "/");
        check(Path::new("/"), &id, false).await.unwrap();
        let err = check(Path::new("/"), &id, true).await.unwrap_err();
        assert_eq!(err.code(), "read-only", "{err}");
    }
}
