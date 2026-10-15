//! The `data` documents of the `accounts` commands. The text output is
//! rendered from these same values, so both always agree.

use serde::Serialize;

use crate::model::{Account, DriveType};
use crate::rclone::remotes::Quota;

/// One account, as `accounts list --json` prints it (the list's `data` is
/// an array of these).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountEntry {
    pub label: String,
    pub remote: String,
    pub drive_type: DriveType,
    pub delta: bool,
    /// RFC 3339, or null for a hand-written entry without a date.
    pub added: Option<String>,
    /// Whether rclone's config has the remote; null when rclone could not be asked.
    pub remote_exists: Option<bool>,
}

impl AccountEntry {
    pub fn new(account: &Account, remote_exists: Option<bool>) -> Self {
        AccountEntry {
            label: account.label.clone(),
            remote: account.remote.clone(),
            drive_type: account.drive_type,
            delta: account.delta,
            added: account.added.map(|d| d.to_string()),
            remote_exists,
        }
    }
}

/// The result of checking one account with `rclone about`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Health {
    Connected,
    TokenExpired,
    RemoteMissing,
    Timeout,
    Failed,
}

/// One account's line in `accounts test --json` (the `data` is an array of these).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestEntry {
    pub label: String,
    pub remote: String,
    pub drive_type: DriveType,
    pub ok: bool,
    pub status: Health,
    /// Storage figures when the check succeeded.
    pub quota: Option<Quota>,
    /// rclone's reason when it failed.
    pub message: Option<String>,
    /// The command that fixes it, when there is one.
    pub hint: Option<String>,
}

/// `accounts add --json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Added {
    pub account: AccountEntry,
    pub quota: Option<Quota>,
    pub dry_run: bool,
    /// Something worth knowing, such as rclone finding a different drive type.
    pub note: Option<String>,
    /// The sign-in worked but OneDrive did not answer the check in time:
    /// the account is kept, and `accounts test` should be run later.
    pub warning: Option<String>,
}

/// A job destination removed along with its account (`accounts remove --force`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemovedDestination {
    pub job: String,
    pub id: String,
    pub path: String,
}

/// `accounts remove --json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Removed {
    pub label: String,
    pub remote: String,
    /// True when rclone's remote was deleted (or would be, in a dry run).
    pub remote_deleted: bool,
    /// True when rclone had no such remote any more.
    pub remote_was_missing: bool,
    pub destinations_removed: Vec<RemovedDestination>,
    /// Jobs whose only destinations were on this account and now have none.
    pub jobs_left_empty: Vec<String>,
    /// Run, hold and bisync records of the removed destinations dropped from `state.json`.
    pub state_entries_removed: usize,
    pub dry_run: bool,
}

/// `accounts reconnect --json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reconnected {
    pub label: String,
    pub remote: String,
    /// True when rclone's remote was missing and was made again.
    pub recreated: bool,
    pub quota: Option<Quota>,
    pub dry_run: bool,
    /// As for [`Added`]: the check after signing in did not answer in time.
    pub warning: Option<String>,
}

/// A refused `accounts remove`: the jobs that still use the account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InUse {
    pub label: String,
    pub destinations: Vec<RemovedDestination>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonout::Envelope;

    fn account(delta: bool, added: bool) -> Account {
        Account {
            label: "work".into(),
            remote: "onefolder-work".into(),
            drive_type: DriveType::Business,
            added: added.then(|| "2026-10-13T09:40:00+11:00".parse().unwrap()),
            delta,
        }
    }

    /// Snapshots the JSON with the version replaced, so a release does not
    /// change every snapshot.
    fn snapshot<T: Serialize>(name: &str, value: &T) {
        let mut value = serde_json::to_value(value).unwrap();
        value["onefolder"] = "[version]".into();
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_json_snapshot!(name, value);
        });
    }

    #[test]
    fn list_shape() {
        let data = vec![
            AccountEntry::new(&account(false, true), Some(true)),
            AccountEntry::new(
                &Account {
                    label: "home".into(),
                    remote: "onefolder-home".into(),
                    drive_type: DriveType::Personal,
                    ..account(true, false)
                },
                Some(false),
            ),
        ];
        snapshot(
            "accounts_list_json",
            &Envelope::success("accounts list", true, data),
        );
    }

    #[test]
    fn test_shape() {
        let data = vec![
            TestEntry {
                label: "work".into(),
                remote: "onefolder-work".into(),
                drive_type: DriveType::Business,
                ok: true,
                status: Health::Connected,
                quota: Some(Quota {
                    total: Some(1_099_511_627_776),
                    used: Some(338_812_420_413),
                    free: Some(760_694_187_747),
                    trashed: Some(0),
                }),
                message: None,
                hint: None,
            },
            TestEntry {
                label: "personal".into(),
                remote: "onefolder-personal".into(),
                drive_type: DriveType::Personal,
                ok: false,
                status: Health::TokenExpired,
                quota: None,
                message: Some("failed to get root: couldn't fetch token".into()),
                hint: Some("onefolder accounts reconnect personal".into()),
            },
        ];
        snapshot(
            "accounts_test_json",
            &Envelope::success("accounts test", false, data),
        );
    }

    #[test]
    fn remove_and_add_shapes() {
        let removed = Removed {
            label: "work".into(),
            remote: "onefolder-work".into(),
            remote_deleted: true,
            remote_was_missing: false,
            destinations_removed: vec![RemovedDestination {
                job: "documents".into(),
                id: "d-7f3a".into(),
                path: "Backups/Documents".into(),
            }],
            jobs_left_empty: vec!["documents".into()],
            state_entries_removed: 2,
            dry_run: false,
        };
        snapshot(
            "accounts_remove_json",
            &Envelope::success("accounts remove", true, removed),
        );
        let added = Added {
            account: AccountEntry::new(&account(false, true), Some(true)),
            quota: Some(Quota {
                total: Some(1_099_511_627_776),
                used: Some(1),
                free: Some(1_099_511_627_775),
                trashed: None,
            }),
            dry_run: false,
            note: None,
            warning: None,
        };
        snapshot(
            "accounts_add_json",
            &Envelope::success("accounts add", true, added),
        );
    }
}
