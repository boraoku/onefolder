//! rclone remotes: `config create`, `config delete`, `config show`,
//! `config reconnect`, `about` and `listremotes`, one remote per account
//! named `onefolder-<label>`. OneFolder stores no tokens; they live in
//! rclone's own config file.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::detect::Rclone;
use super::exec::{self, Attached, AttachedOutput, ExecError, InterruptGuard};
use crate::model::DriveType;

/// How long `about` may take in `doctor` and `accounts test` before the
/// account counts as unreachable, so a dead network cannot hang them.
pub const ABOUT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `about` may take right after a sign-in, when OneDrive is often slower.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the quick config commands (`listremotes`, `config show`,
/// `config delete`) may take.
pub const CONFIG_TIMEOUT: Duration = Duration::from_secs(10);

/// What `rclone config show <remote>` says about a remote. Only the keys
/// OneFolder needs are kept; the token is never read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteConfig {
    /// The backend, such as `onedrive` or `alias`.
    pub kind: Option<String>,
    pub drive_type: Option<DriveType>,
    pub delta: bool,
}

/// Storage figures from `rclone about --json`. OneDrive reports all of
/// them; other backends may leave some out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trashed: Option<u64>,
}

/// Why an rclone remote command failed, in categories the user can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// The sign-in has expired or was revoked: `accounts reconnect` fixes it.
    TokenExpired,
    /// rclone's config has no remote by that name.
    RemoteMissing,
    /// No answer within the time limit.
    Timeout,
    /// Anything else (network, permissions, an rclone bug).
    Other,
}

/// A failed remote command.
#[derive(Debug, Error)]
pub enum RemoteError {
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error("rclone {command} did not finish within {} seconds", .limit.as_secs())]
    Timeout {
        command: &'static str,
        limit: Duration,
    },
    #[error("{message}")]
    Failed {
        command: &'static str,
        kind: FailureKind,
        /// rclone's last error line, without its timestamp and level.
        message: String,
        code: Option<i32>,
    },
    #[error("could not read the output of rclone {command}: {detail}")]
    Unreadable {
        command: &'static str,
        detail: String,
    },
}

impl RemoteError {
    pub fn kind(&self) -> FailureKind {
        match self {
            RemoteError::Timeout { .. } => FailureKind::Timeout,
            RemoteError::Failed { kind, .. } => *kind,
            RemoteError::Exec(_) | RemoteError::Unreadable { .. } => FailureKind::Other,
        }
    }
}

/// True when an rclone error says the network could not be reached. rclone
/// wraps every failed token refresh as "couldn't fetch token: ...", network
/// errors included, so these are checked before the token markers.
pub fn is_network_error(stderr: &str) -> bool {
    const NETWORK: [&str; 6] = [
        "dial tcp",
        "no such host",
        "i/o timeout",
        "connection refused",
        "network is unreachable",
        "tls handshake timeout",
    ];
    let text = stderr.to_ascii_lowercase();
    NETWORK.iter().any(|n| text.contains(n))
}

/// Sorts an rclone error message into a [`FailureKind`]. A network failure
/// is [`FailureKind::Other`] even inside rclone's "couldn't fetch token"
/// wrapper; only OAuth's own answers (`invalid_grant`, rclone's "maybe
/// token expired?" suffix, Microsoft's AADSTS codes) mean the sign-in expired.
pub fn classify(stderr: &str) -> FailureKind {
    let text = stderr.to_ascii_lowercase();
    const TOKEN: [&str; 6] = [
        "invalid_grant",
        "token expired",
        "token has expired",
        "empty token found",
        "invalidauthenticationtoken",
        "aadsts",
    ];
    if is_network_error(&text) {
        FailureKind::Other
    } else if TOKEN.iter().any(|t| text.contains(t)) {
        FailureKind::TokenExpired
    } else if text.contains("didn't find section in config file") {
        FailureKind::RemoteMissing
    } else {
        FailureKind::Other
    }
}

/// The most useful line of rclone's stderr: the last error-looking line,
/// without the `2026/10/13 13:52:29 CRITICAL: ` prefix and rclone's
/// "Failed to create file system for ..." wrapper.
pub fn clean_message(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let pick = lines
        .iter()
        .rev()
        .find(|l| {
            ["CRITICAL", "ERROR", "Fatal error", "Failed"]
                .iter()
                .any(|w| l.contains(w))
        })
        .or(lines.last())
        .copied()
        .unwrap_or("");
    let mut text = strip_log_prefix(pick);
    for wrapper in ["Failed to create file system for ", "Failed to about: "] {
        if let Some(rest) = text.strip_prefix(wrapper) {
            // `"remote:": reason` → `reason`
            text = match rest.split_once("\": ") {
                Some((_, reason)) if rest.starts_with('"') => reason,
                _ => rest,
            };
        }
    }
    let text = text.strip_prefix("Fatal error: ").unwrap_or(text);
    text.trim().to_string()
}

/// Drops `2026/10/13 13:52:29 NOTICE: ` (or `ERROR : `) from a log line.
fn strip_log_prefix(line: &str) -> &str {
    let bytes = line.as_bytes();
    let looks_dated = bytes.len() > 20
        && bytes[4] == b'/'
        && bytes[7] == b'/'
        && bytes[10] == b' '
        && bytes[13] == b':'
        && bytes[16] == b':';
    if !looks_dated {
        return line;
    }
    let Some(rest) = line.get(20..) else {
        return line;
    };
    match rest.split_once(": ") {
        Some((level, msg))
            if !level.is_empty() && level.trim().chars().all(|c| c.is_ascii_uppercase()) =>
        {
            msg
        }
        _ => rest,
    }
}

/// Parses `rclone config show <remote>`. Returns `None` when rclone does not
/// know the remote: it then prints the section header and a comment such as
/// `# couldn't find type of fs for "onefolder-x"`, and still exits 0.
pub fn parse_config_show(text: &str) -> Option<RemoteConfig> {
    let mut config = RemoteConfig::default();
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            if in_section {
                break;
            }
            in_section = true;
            continue;
        }
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "type" => config.kind = Some(value.to_string()),
            "drive_type" => {
                config.drive_type = match value {
                    "personal" => Some(DriveType::Personal),
                    // rclone records SharePoint libraries as documentLibrary;
                    // they behave like business drives.
                    "business" | "documentLibrary" => Some(DriveType::Business),
                    _ => None,
                }
            }
            "delta" => config.delta = matches!(value, "true" | "1" | "yes"),
            _ => {}
        }
    }
    config.kind.is_some().then_some(config)
}

/// Parses `rclone about <remote>: --json`.
pub fn parse_about(text: &str) -> Result<Quota, String> {
    serde_json::from_str(text.trim()).map_err(|e| e.to_string())
}

/// Parses `rclone listremotes`: one `name:` per line.
pub fn parse_listremotes(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter_map(|l| l.strip_suffix(':'))
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

async fn capture(
    rclone: &Rclone,
    command: &'static str,
    args: &[&str],
    limit: Duration,
) -> Result<exec::Captured, RemoteError> {
    match tokio::time::timeout(limit, exec::run_capture(&rclone.path, args)).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(RemoteError::Timeout { command, limit }),
    }
}

fn failed(command: &'static str, captured: &exec::Captured) -> RemoteError {
    let message = clean_message(&captured.stderr);
    RemoteError::Failed {
        command,
        kind: classify(&captured.stderr),
        message: if message.is_empty() {
            format!("rclone {command} exited with {}", captured.status)
        } else {
            message
        },
        code: captured.status.code(),
    }
}

/// The names of every remote in rclone's config.
pub async fn list(rclone: &Rclone) -> Result<Vec<String>, RemoteError> {
    let out = capture(rclone, "listremotes", &["listremotes"], CONFIG_TIMEOUT).await?;
    if !out.status.success() {
        return Err(failed("listremotes", &out));
    }
    Ok(parse_listremotes(&out.stdout))
}

/// The settings of one remote, or `None` when rclone does not have it.
pub async fn show(rclone: &Rclone, remote: &str) -> Result<Option<RemoteConfig>, RemoteError> {
    let out = capture(
        rclone,
        "config show",
        &["config", "show", remote],
        CONFIG_TIMEOUT,
    )
    .await?;
    if !out.status.success() {
        return Err(failed("config show", &out));
    }
    Ok(parse_config_show(&out.stdout))
}

/// Asks the drive for its storage figures, which also proves the sign-in works.
pub async fn about(rclone: &Rclone, remote: &str, limit: Duration) -> Result<Quota, RemoteError> {
    let target = format!("{remote}:");
    let out = capture(rclone, "about", &["about", &target, "--json"], limit).await?;
    if !out.status.success() {
        return Err(failed("about", &out));
    }
    parse_about(&out.stdout).map_err(|detail| RemoteError::Unreadable {
        command: "about",
        detail,
    })
}

/// Deletes a remote from rclone's config. Deleting a remote that does not
/// exist succeeds, as rclone itself treats it.
pub async fn delete(rclone: &Rclone, remote: &str) -> Result<(), RemoteError> {
    let out = capture(
        rclone,
        "config delete",
        &["config", "delete", remote],
        CONFIG_TIMEOUT,
    )
    .await?;
    if !out.status.success() {
        return Err(failed("config delete", &out));
    }
    Ok(())
}

/// The arguments of `rclone config create` for a OneDrive remote.
/// `--no-output` keeps rclone from printing the new remote, token included,
/// to the terminal.
pub fn create_args(remote: &str, drive_type: DriveType, delta: bool) -> Vec<String> {
    let mut args = vec![
        "config".to_string(),
        "create".to_string(),
        "--no-output".to_string(),
        remote.to_string(),
        "onedrive".to_string(),
        format!("drive_type={}", drive_type.as_str()),
    ];
    if delta {
        args.push("delta=true".to_string());
    }
    args
}

/// Runs `rclone config create <remote> onedrive drive_type=... [delta=true]`
/// with the terminal attached: rclone opens the browser, waits for the
/// sign-in on 127.0.0.1:53682 and writes the remote.
pub async fn create_onedrive(
    rclone: &Rclone,
    remote: &str,
    drive_type: DriveType,
    delta: bool,
    output: AttachedOutput,
    interrupts: &mut InterruptGuard,
) -> Result<Attached, ExecError> {
    let args = create_args(remote, drive_type, delta);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    exec::run_attached(&rclone.path, &args, output, interrupts).await
}

/// Runs `rclone config reconnect <remote>:` with the terminal attached, to
/// sign in again and replace an expired token.
pub async fn reconnect(
    rclone: &Rclone,
    remote: &str,
    output: AttachedOutput,
    interrupts: &mut InterruptGuard,
) -> Result<Attached, ExecError> {
    let target = format!("{remote}:");
    exec::run_attached(
        &rclone.path,
        &["config", "reconnect", &target],
        output,
        interrupts,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = crate::rclone::fixture_path(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn config_show_business() {
        let c = parse_config_show(&fixture("config-show-onedrive-business.txt")).unwrap();
        assert_eq!(c.kind.as_deref(), Some("onedrive"));
        assert_eq!(c.drive_type, Some(DriveType::Business));
        assert!(!c.delta);
    }

    #[test]
    fn config_show_personal_with_delta() {
        let c = parse_config_show(&fixture("config-show-onedrive-personal-delta.txt")).unwrap();
        assert_eq!(c.drive_type, Some(DriveType::Personal));
        assert!(c.delta);
    }

    #[test]
    fn config_show_alias_and_missing() {
        let c = parse_config_show(&fixture("config-show-alias.txt")).unwrap();
        assert_eq!(c.kind.as_deref(), Some("alias"));
        assert_eq!(c.drive_type, None);
        assert_eq!(parse_config_show(&fixture("config-show-missing.txt")), None);
        assert_eq!(parse_config_show(""), None);
    }

    #[test]
    fn config_show_reads_only_the_first_section() {
        let text =
            "[a]\ntype = onedrive\ndrive_type = personal\n\n[b]\ntype = alias\ndelta = true\n";
        let c = parse_config_show(text).unwrap();
        assert_eq!(c.kind.as_deref(), Some("onedrive"));
        assert!(!c.delta);
    }

    #[test]
    fn about_onedrive_and_alias() {
        let q = parse_about(&fixture("about-onedrive.json")).unwrap();
        assert_eq!(q.total, Some(1_099_511_627_776));
        assert_eq!(q.used, Some(338_812_420_413));
        assert_eq!(q.free, Some(760_694_187_747));
        assert_eq!(q.trashed, Some(1_520_342));
        let q = parse_about(&fixture("about-alias.json")).unwrap();
        assert!(q.total.is_some() && q.free.is_some() && q.trashed.is_none());
        assert_eq!(parse_about("{}").unwrap(), Quota::default());
        assert!(parse_about("Total: 1 TiB").is_err());
    }

    #[test]
    fn listremotes() {
        assert_eq!(
            parse_listremotes("onefolder-test:\nother:\n\n"),
            vec!["onefolder-test", "other"]
        );
        assert!(parse_listremotes("").is_empty());
    }

    #[test]
    fn classification() {
        assert_eq!(
            classify(&fixture("about-token-expired.txt")),
            FailureKind::TokenExpired
        );
        assert_eq!(
            classify(&fixture("about-invalid-grant.txt")),
            FailureKind::TokenExpired
        );
        assert_eq!(
            classify(&fixture("about-remote-missing.txt")),
            FailureKind::RemoteMissing
        );
        assert_eq!(
            classify(&fixture("about-directory-not-found.txt")),
            FailureKind::Other
        );
        assert_eq!(classify(&fixture("about-offline.txt")), FailureKind::Other);
        assert!(is_network_error(&fixture("about-offline.txt")));
        assert!(!is_network_error(&fixture("about-token-expired.txt")));
        // The wrapper alone is not an expired token.
        assert_eq!(
            classify("couldn't fetch token: Post \"https://login.microsoftonline.com\": connection refused"),
            FailureKind::Other
        );
        assert_eq!(
            classify("couldn't fetch token: something odd"),
            FailureKind::Other
        );
        assert_eq!(classify("Token Expired"), FailureKind::TokenExpired);
        assert_eq!(
            classify("dial tcp: lookup graph.microsoft.com: no such host"),
            FailureKind::Other
        );
    }

    #[test]
    fn messages_lose_timestamps_and_wrappers() {
        assert_eq!(
            clean_message(&fixture("about-remote-missing.txt")),
            "didn't find section in config file (\"onefolder-nope\")"
        );
        assert_eq!(
            clean_message(&fixture("about-directory-not-found.txt")),
            "about call failed: directory not found"
        );
        assert!(clean_message(&fixture("about-token-expired.txt"))
            .starts_with("failed to get root: Get "));
        assert!(clean_message(&fixture("about-token-expired.txt"))
            .contains("couldn't fetch token: oauth2: \"invalid_grant\""));
        assert_eq!(clean_message("plain words\n"), "plain words");
        assert_eq!(clean_message(""), "");
        // A multi-byte character where the date would end must not panic.
        // (byte 20 falls inside the é)
        assert_eq!(
            clean_message("2026/10/13 13:52:29é x"),
            "2026/10/13 13:52:29é x"
        );
        assert_eq!(
            clean_message("éééééééééééééééééééééé"),
            "éééééééééééééééééééééé"
        );
    }

    #[test]
    fn create_arguments() {
        assert_eq!(
            create_args("onefolder-work", DriveType::Business, false).join(" "),
            "config create --no-output onefolder-work onedrive drive_type=business"
        );
        assert_eq!(
            create_args("onefolder-home", DriveType::Personal, true).join(" "),
            "config create --no-output onefolder-home onedrive drive_type=personal delta=true"
        );
    }
}
