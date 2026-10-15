//! Finds rclone on PATH, reads its version and checks it against the minimum.

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use super::exec;

/// File name of the rclone binary.
pub const BINARY: &str = if cfg!(windows) {
    "rclone.exe"
} else {
    "rclone"
};

/// Oldest rclone OneFolder works with: the first release with
/// `--conflict-resolve`, `--resync-mode` and `--recover`.
pub const MINIMUM: Version = Version {
    major: 1,
    minor: 66,
    patch: 0,
};

/// An rclone release number. Pre-release suffixes such as `-DEV` or
/// `-beta.8312` are dropped, so a beta counts as its release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// `1.66`, as used in the friendly messages.
    pub fn short(self) -> String {
        format!("{}.{}", self.major, self.minor)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// A usable rclone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rclone {
    /// Absolute path of the binary.
    pub path: PathBuf,
    pub version: Version,
}

/// Why rclone cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DetectError {
    #[error("rclone is not on PATH")]
    NotFound,
    #[error("rclone {found} at {} is older than {MINIMUM}", path.display())]
    TooOld { path: PathBuf, found: Version },
    #[error("could not read the version of {}: {detail}", path.display())]
    Unreadable { path: PathBuf, detail: String },
}

impl DetectError {
    /// The friendly message printed before exiting with code 2.
    pub fn user_message(&self) -> String {
        use crate::platform::{self, Os, RcloneAction};
        let min = MINIMUM.short();
        if platform::ui_os() != Os::Mac {
            let (problem, action) = match self {
                DetectError::NotFound => {
                    ("it is not on your PATH".to_string(), RcloneAction::Install)
                }
                DetectError::TooOld { found, .. } => (
                    format!("the one on your PATH is {found}"),
                    RcloneAction::Upgrade,
                ),
                DetectError::Unreadable { path, .. } => (
                    format!("{} did not report its version", path.display()),
                    RcloneAction::Reinstall,
                ),
            };
            let advice = platform::rclone_advice(action);
            let mut chars = advice.chars();
            let advice = chars
                .next()
                .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default();
            return format!(
                "OneFolder needs rclone {min} or newer, and {problem}.\n\
                 \n  {advice}\
                 \n  Then run onefolder again.\n"
            );
        }
        match self {
            DetectError::NotFound => format!(
                "OneFolder needs rclone {min} or newer, and it is not on your PATH.\n\
                 \n  Install it with Homebrew:   brew install rclone\
                 \n  Then run onefolder again.\n"
            ),
            DetectError::TooOld { found, .. } => format!(
                "OneFolder needs rclone {min} or newer, and the one on your PATH is {found}.\n\
                 \n  Update it with Homebrew:    brew upgrade rclone\
                 \n  Then run onefolder again.\n"
            ),
            DetectError::Unreadable { path, .. } => format!(
                "OneFolder needs rclone {min} or newer, and {} did not report its version.\n\
                 \n  Reinstall it with Homebrew: brew reinstall rclone\
                 \n  Then run onefolder again.\n",
                path.display()
            ),
        }
    }

    /// A short stable code for `--json` output.
    pub fn code(&self) -> &'static str {
        match self {
            DetectError::NotFound => "rclone-missing",
            DetectError::TooOld { .. } => "rclone-too-old",
            DetectError::Unreadable { .. } => "rclone-unreadable",
        }
    }

    /// Path of the binary that was found, if any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            DetectError::NotFound => None,
            DetectError::TooOld { path, .. } | DetectError::Unreadable { path, .. } => Some(path),
        }
    }
}

/// Parses the output of `rclone version`. The first line looks like
/// `rclone v1.75.2`, sometimes with a suffix such as `-DEV` or `-beta.8312.abc`.
pub fn parse_version(output: &str) -> Option<Version> {
    let first = output.lines().next()?.trim();
    let mut words = first.split_whitespace();
    if words.next()? != "rclone" {
        return None;
    }
    let raw = words.next()?;
    let raw = raw.strip_prefix('v').unwrap_or(raw);
    let core = raw.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
    })
}

/// True when `found` meets the minimum.
pub fn is_supported(found: Version) -> bool {
    found >= MINIMUM
}

/// Looks for the rclone binary in the directories of a PATH value.
pub fn find_on_path(path_var: Option<&OsStr>) -> Option<PathBuf> {
    find_program(path_var, BINARY)
}

/// Looks for a program in the directories of a PATH value.
pub fn find_program(path_var: Option<&OsStr>, name: &str) -> Option<PathBuf> {
    let path_var = path_var?;
    env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
        .map(|p| std::path::absolute(&p).unwrap_or(p))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// How long `rclone version` may take before it counts as unreadable.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Finds rclone on this process's PATH and checks its version.
pub async fn detect() -> Result<Rclone, DetectError> {
    detect_in(env::var_os("PATH").as_deref()).await
}

/// Like [`detect`], against a given PATH value.
pub async fn detect_in(path_var: Option<&OsStr>) -> Result<Rclone, DetectError> {
    detect_with_timeout(path_var, VERSION_TIMEOUT).await
}

async fn detect_with_timeout(
    path_var: Option<&OsStr>,
    limit: Duration,
) -> Result<Rclone, DetectError> {
    let path = find_on_path(path_var).ok_or(DetectError::NotFound)?;
    let run = exec::run_capture(&path, &["version"]);
    let captured = match tokio::time::timeout(limit, run).await {
        Ok(result) => result.map_err(|e| DetectError::Unreadable {
            path: path.clone(),
            detail: e.to_string(),
        })?,
        Err(_) => {
            return Err(DetectError::Unreadable {
                path,
                detail: format!(
                    "`rclone version` did not finish within {} seconds",
                    limit.as_secs_f32()
                ),
            })
        }
    };
    if !captured.status.success() {
        return Err(DetectError::Unreadable {
            path,
            detail: format!(
                "`rclone version` exited with {}: {}",
                captured.status,
                captured.stderr.lines().next().unwrap_or("").trim()
            ),
        });
    }
    let version = parse_version(&captured.stdout).ok_or_else(|| DetectError::Unreadable {
        path: path.clone(),
        detail: format!(
            "unexpected output: {}",
            captured.stdout.lines().next().unwrap_or("").trim()
        ),
    })?;
    if !is_supported(version) {
        return Err(DetectError::TooOld {
            path,
            found: version,
        });
    }
    Ok(Rclone { path, version })
}

/// How long `brew info` may take before the doctor gives up on it.
pub const BREW_TIMEOUT: Duration = Duration::from_secs(5);

/// The newest rclone Homebrew offers: `brew info --json=v2 rclone`, with
/// `brew` from PATH. `None` without brew, after [`BREW_TIMEOUT`], or when
/// the answer does not read (offline, an old tap, no formula).
pub async fn latest_from_homebrew() -> Option<Version> {
    let brew = find_program(env::var_os("PATH").as_deref(), "brew")?;
    let run = exec::run_capture(&brew, &["info", "--json=v2", "rclone"]);
    let captured = tokio::time::timeout(BREW_TIMEOUT, run).await.ok()?.ok()?;
    if !captured.status.success() {
        return None;
    }
    parse_brew_info(&captured.stdout)
}

/// Reads `formulae[0].versions.stable` from `brew info --json=v2`.
pub fn parse_brew_info(text: &str) -> Option<Version> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let stable = json
        .get("formulae")?
        .get(0)?
        .get("versions")?
        .get("stable")?
        .as_str()?;
    parse_version(&format!("rclone v{stable}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homebrew_versions() {
        let text = r#"{"formulae":[{"name":"rclone","versions":{"stable":"1.76.1","head":"HEAD","bottle":true}}],"casks":[]}"#;
        assert_eq!(
            parse_brew_info(text),
            Some(Version {
                major: 1,
                minor: 76,
                patch: 1
            })
        );
        assert_eq!(parse_brew_info("{}"), None);
        assert_eq!(parse_brew_info("Error: No available formula"), None);
        assert_eq!(parse_brew_info(r#"{"formulae":[]}"#), None);
    }

    fn v(major: u32, minor: u32, patch: u32) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    #[test]
    fn parses_real_first_lines() {
        let cases = [
            ("rclone v1.75.2", v(1, 75, 2)),
            ("rclone v1.66.0", v(1, 66, 0)),
            ("rclone v1.53.3-DEV", v(1, 53, 3)),
            ("rclone v1.69.0-beta.8395.5ba6c5b6c.fix-bisync", v(1, 69, 0)),
            ("rclone v1.60.1-termux", v(1, 60, 1)),
            ("rclone 1.45", v(1, 45, 0)),
            ("  rclone v1.75.2  ", v(1, 75, 2)),
        ];
        for (line, want) in cases {
            assert_eq!(parse_version(line), Some(want), "{line}");
        }
    }

    #[test]
    fn parses_full_version_output() {
        let out = "rclone v1.75.2\n- os/version: darwin 26.0 (64 bit)\n- os/kernel: 25.0.0 (arm64)\n- os/type: darwin\n- os/arch: arm64 (ARMv8 compatible)\n- go/version: go1.25.3\n- go/linking: dynamic\n- go/tags: none\n";
        assert_eq!(parse_version(out), Some(v(1, 75, 2)));
    }

    #[test]
    fn rejects_other_output() {
        for line in [
            "",
            "rclone",
            "rsync version 3.2.7",
            "rclone vX.Y",
            "rclone v1",
            "rclone v1.2.3.4",
        ] {
            assert_eq!(parse_version(line), None, "{line:?}");
        }
    }

    #[test]
    fn minimum_comparison() {
        assert!(!is_supported(v(1, 65, 2)));
        assert!(!is_supported(v(0, 99, 99)));
        assert!(is_supported(v(1, 66, 0)));
        assert!(is_supported(v(1, 75, 2)));
        assert!(is_supported(v(1, 100, 0)));
        assert!(is_supported(v(2, 0, 0)));
        assert_eq!(MINIMUM.to_string(), "1.66.0");
        assert_eq!(MINIMUM.short(), "1.66");
    }

    #[tokio::test]
    async fn missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_on_path(Some(dir.path().as_os_str())), None);
        assert_eq!(find_on_path(None), None);
        let err = detect_in(Some(dir.path().as_os_str())).await.unwrap_err();
        assert_eq!(err, DetectError::NotFound);
        let msg = err.user_message();
        assert!(msg.contains("OneFolder needs rclone 1.66 or newer, and it is not on your PATH."));
        assert!(msg.contains("  Install it with Homebrew:   brew install rclone"));
        assert!(msg.contains("  Then run onefolder again."));
    }

    #[cfg(unix)]
    fn fake_rclone(dir: &Path, first_line: &str) {
        fake_script(dir, &format!("echo '{first_line}'"));
    }

    #[cfg(unix)]
    fn fake_script(dir: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(BINARY);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_zero_exit_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        fake_script(
            dir.path(),
            "echo 'rclone v1.75.2'\necho 'config broken' >&2\nexit 3",
        );
        let err = detect_in(Some(dir.path().as_os_str())).await.unwrap_err();
        match &err {
            DetectError::Unreadable { detail, .. } => {
                assert!(detail.contains("config broken"), "{detail}")
            }
            other => panic!("expected Unreadable, got {other:?}"),
        }
        let msg = err.user_message();
        assert!(msg.contains("did not report its version"), "{msg}");
        assert!(
            msg.contains("  Reinstall it with Homebrew: brew reinstall rclone"),
            "{msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hanging_rclone_times_out() {
        let dir = tempfile::tempdir().unwrap();
        fake_script(dir.path(), "exec sleep 30");
        let started = std::time::Instant::now();
        let err = detect_with_timeout(Some(dir.path().as_os_str()), Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            matches!(&err, DetectError::Unreadable { detail, .. } if detail.contains("did not finish")),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_binaries() {
        let ok = tempfile::tempdir().unwrap();
        fake_rclone(ok.path(), "rclone v1.75.2");
        let found = detect_in(Some(ok.path().as_os_str())).await.unwrap();
        assert_eq!(found.version, v(1, 75, 2));
        assert!(found.path.is_absolute());

        let old = tempfile::tempdir().unwrap();
        fake_rclone(old.path(), "rclone v1.60.0");
        let err = detect_in(Some(old.path().as_os_str())).await.unwrap_err();
        assert!(matches!(err, DetectError::TooOld { found, .. } if found == v(1, 60, 0)));
        assert!(err
            .user_message()
            .contains("the one on your PATH is 1.60.0"));

        let odd = tempfile::tempdir().unwrap();
        fake_rclone(odd.path(), "hello");
        let err = detect_in(Some(odd.path().as_os_str())).await.unwrap_err();
        assert!(matches!(err, DetectError::Unreadable { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(BINARY), "not a program").unwrap();
        assert_eq!(find_on_path(Some(dir.path().as_os_str())), None);
    }
}
