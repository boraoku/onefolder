//! The data model shared by config, state, sync and the UI.
//!
//! Field names and value spellings match the Data model section of
//! DESIGN.md, so the serde derives here define the on-disk shape of both
//! `config.toml` and `state.json`.

use std::collections::hash_map::RandomState;
use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::volume::{Identity, Recorded};

/// A timestamp as TOML writes it (`2026-10-13T09:40:00+11:00`).
pub use toml_edit::Datetime;

/// Prefix of every rclone remote that OneFolder creates.
pub const REMOTE_PREFIX: &str = "onefolder-";

/// Kind of OneDrive account, as rclone's `drive_type` option spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriveType {
    Personal,
    Business,
}

impl DriveType {
    pub fn as_str(self) -> &'static str {
        match self {
            DriveType::Personal => "personal",
            DriveType::Business => "business",
        }
    }
}

/// One OneDrive account, backed by one rclone remote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// What the user sees; unique, lowercase, `[a-z0-9][a-z0-9-]*`.
    pub label: String,
    /// rclone remote name, always `onefolder-` + label.
    pub remote: String,
    pub drive_type: DriveType,
    /// When the account was added. Optional so a hand-written entry still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<Datetime>,
    /// True when the rclone remote was created with `delta = true`, so
    /// `--fast-list` uses OneDrive's delta listing. Off by default
    /// (DESIGN.md decision 5); written to the file only when on.
    #[serde(default, skip_serializing_if = "is_false")]
    pub delta: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Account {
    /// The rclone remote name for a label.
    pub fn remote_for(label: &str) -> String {
        format!("{REMOTE_PREFIX}{label}")
    }
}

/// Sync direction of one destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    /// Additive `rclone copy`; never deletes on the destination.
    #[serde(rename = "one-way")]
    OneWay,
    /// `rclone bisync`; changes and deletions propagate both ways.
    #[serde(rename = "two-way")]
    TwoWay,
}

impl Mode {
    pub const ALL: [Mode; 2] = [Mode::OneWay, Mode::TwoWay];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::OneWay => "one-way",
            Mode::TwoWay => "two-way",
        }
    }
}

fn default_local_mode() -> Mode {
    Mode::OneWay
}

fn is_default_local_mode(mode: &Mode) -> bool {
    *mode == default_local_mode()
}

/// A typed destination. The `type` key in the config selects the variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Destination {
    /// A folder on a OneDrive account.
    Onedrive {
        /// Stable short id, `d-` plus four hex characters; never edited.
        id: String,
        /// Label of an account in the same config.
        account: String,
        /// Path relative to the drive root.
        path: String,
        mode: Mode,
    },
    /// A folder on a local or external volume.
    Local {
        id: String,
        /// Absolute path.
        path: String,
        /// Defaults to one-way, the safe choice for external drives.
        #[serde(
            default = "default_local_mode",
            skip_serializing_if = "is_default_local_mode"
        )]
        mode: Mode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume_uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_point: Option<String>,
        /// The partition's UUID (diskutil's `DiskUUID`), recorded only when
        /// the file system has no UUID (some exFAT drives).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume_partition_uuid: Option<String>,
        /// Device node (`/dev/disk4s1`), or `<fstype>:<share>` for a network
        /// volume, recorded only when there is no UUID of either kind; the
        /// check then compares device and name, a weaker check.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume_device: Option<String>,
        /// Linux: the folder of the file system mounted at `mount_point`
        /// (a btrfs subvolume such as `/@data`), recorded only when it is
        /// not the whole file system.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume_root: Option<String>,
    },
}

impl Destination {
    /// A OneDrive destination.
    pub fn onedrive(id: String, account: String, path: String, mode: Mode) -> Self {
        Destination::Onedrive {
            id,
            account,
            path,
            mode,
        }
    }

    /// A local destination with the default mode (one-way) and no volume identity yet.
    pub fn local(id: String, path: String) -> Self {
        Destination::Local {
            id,
            path,
            mode: default_local_mode(),
            volume_uuid: None,
            volume_name: None,
            mount_point: None,
            volume_partition_uuid: None,
            volume_device: None,
            volume_root: None,
        }
    }

    /// The volume identity recorded for a local destination, if any.
    pub fn identity(&self) -> Option<Identity> {
        match self {
            Destination::Onedrive { .. } => None,
            Destination::Local {
                path,
                volume_uuid,
                volume_name,
                mount_point,
                volume_partition_uuid,
                volume_device,
                volume_root,
                ..
            } => Identity::recorded(
                path,
                Recorded {
                    mount_point: mount_point.as_deref(),
                    volume_name: volume_name.as_deref(),
                    volume_uuid: volume_uuid.as_deref(),
                    partition_uuid: volume_partition_uuid.as_deref(),
                    device: volume_device.as_deref(),
                    mount_root: volume_root.as_deref(),
                },
            ),
        }
    }

    /// Records (or clears) the volume identity of a local destination.
    /// Does nothing for a OneDrive destination.
    pub fn set_identity(&mut self, identity: Option<&Identity>) {
        if let Destination::Local {
            volume_uuid,
            volume_name,
            mount_point,
            volume_partition_uuid,
            volume_device,
            volume_root,
            ..
        } = self
        {
            *volume_root = identity.and_then(|i| i.mount_root.clone());
            *volume_uuid = identity.and_then(|i| i.volume_uuid.clone());
            *volume_name = identity.map(|i| i.volume_name.clone());
            *mount_point = identity.map(|i| i.mount_point.clone());
            *volume_partition_uuid = identity.and_then(|i| i.partition_uuid.clone());
            *volume_device = identity.and_then(|i| i.device.clone());
        }
    }

    pub fn set_mode(&mut self, new: Mode) {
        match self {
            Destination::Onedrive { mode, .. } | Destination::Local { mode, .. } => *mode = new,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Destination::Onedrive { id, .. } | Destination::Local { id, .. } => id,
        }
    }

    pub fn mode(&self) -> Mode {
        match self {
            Destination::Onedrive { mode, .. } | Destination::Local { mode, .. } => *mode,
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Destination::Onedrive { path, .. } | Destination::Local { path, .. } => path,
        }
    }

    /// The `type` value as written in the config.
    pub fn type_name(&self) -> &'static str {
        match self {
            Destination::Onedrive { .. } => "onedrive",
            Destination::Local { .. } => "local",
        }
    }

    /// How the destination is shown to the user: `work:Backups/Documents` or the local path.
    pub fn display(&self) -> String {
        match self {
            Destination::Onedrive { account, path, .. } if path.is_empty() => {
                format!("{account}: (drive root)")
            }
            Destination::Onedrive { account, path, .. } => format!("{account}:{path}"),
            Destination::Local { path, .. } => path.clone(),
        }
    }
}

/// A sync job: one local source and an ordered list of destinations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Job {
    /// Unique, lowercase, `[a-z0-9][a-z0-9-]*`; used on the command line.
    pub name: String,
    /// Absolute path of the local folder.
    pub source: String,
    /// rclone filter patterns, passed as repeated `--exclude`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    /// True to sync hidden files and folders (names starting with a dot).
    /// Off by default: hidden files are left out, and the built-in junk
    /// list ([`crate::rclone::BUILTIN_EXCLUDES`]) is left out either way.
    /// Written to the file only when on, or when the file already had it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub copy_hidden: bool,
    /// Volume identity of the source, recorded when the source is on an
    /// external volume (under `/Volumes`), so a run can refuse to read an
    /// empty mount point. Same meaning as the destination's `volume_*` keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_volume_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_volume_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_mount_point: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_volume_partition_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_volume_device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_volume_root: Option<String>,
    #[serde(default)]
    pub destinations: Vec<Destination>,
}

impl Job {
    /// A job with a name and a source and nothing else yet.
    pub fn new(name: impl Into<String>, source: impl Into<String>) -> Job {
        Job {
            name: name.into(),
            source: source.into(),
            ..Job::default()
        }
    }

    /// A fresh destination id that no destination of this job uses yet.
    pub fn new_destination_id(&self) -> String {
        let taken: HashSet<&str> = self.destinations.iter().map(|d| d.id()).collect();
        generate_destination_id(|candidate| taken.contains(candidate))
    }

    /// The volume identity recorded for the source, if any.
    pub fn source_identity(&self) -> Option<Identity> {
        Identity::recorded(
            &self.source,
            Recorded {
                mount_point: self.source_mount_point.as_deref(),
                volume_name: self.source_volume_name.as_deref(),
                volume_uuid: self.source_volume_uuid.as_deref(),
                partition_uuid: self.source_volume_partition_uuid.as_deref(),
                device: self.source_volume_device.as_deref(),
                mount_root: self.source_volume_root.as_deref(),
            },
        )
    }

    /// Records (or clears) the volume identity of the source.
    pub fn set_source_identity(&mut self, identity: Option<&Identity>) {
        self.source_volume_uuid = identity.and_then(|i| i.volume_uuid.clone());
        self.source_volume_name = identity.map(|i| i.volume_name.clone());
        self.source_mount_point = identity.map(|i| i.mount_point.clone());
        self.source_volume_partition_uuid = identity.and_then(|i| i.partition_uuid.clone());
        self.source_volume_device = identity.and_then(|i| i.device.clone());
        self.source_volume_root = identity.and_then(|i| i.mount_root.clone());
    }
}

/// Generates `d-` plus four lowercase hex characters, retrying while `is_taken`
/// says the candidate is in use. Random first, then a full scan, so it only
/// gives up if all 65,536 ids are taken, which a job never reaches.
pub fn generate_destination_id(is_taken: impl Fn(&str) -> bool) -> String {
    for _ in 0..256 {
        let candidate = format!("d-{:04x}", random_u16());
        if !is_taken(&candidate) {
            return candidate;
        }
    }
    (0..=u16::MAX)
        .map(|n| format!("d-{n:04x}"))
        .find(|candidate| !is_taken(candidate))
        .unwrap_or_else(|| "d-ffff".to_string())
}

/// True when `id` has the generated shape `d-` plus four lowercase hex characters.
pub fn is_valid_destination_id(id: &str) -> bool {
    id.len() == 6
        && id.starts_with("d-")
        && id[2..]
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// A small source of randomness without a crate: std's randomly keyed
/// hasher, fed the clock and a counter.
fn random_u16() -> u16 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    (hasher.finish() & 0xffff) as u16
}

/// True when `name` is a valid account label or job name: lowercase, `[a-z0-9][a-z0-9-]*`.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Result of a run for one destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Ok,
    Partial,
    Failed,
    Cancelled,
    NeedsResync,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Partial => "partial",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
            Outcome::NeedsResync => "needs-resync",
        }
    }
}

/// A count split by side: `local` is the Mac, `remote` is the destination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideCounts {
    pub local: u64,
    pub remote: u64,
}

impl SideCounts {
    pub fn total(self) -> u64 {
        self.local + self.remote
    }
}

/// The record of the last run of one destination, as stored in `state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// RFC 3339 timestamp.
    pub started: String,
    /// RFC 3339 timestamp.
    pub finished: String,
    pub mode: Mode,
    pub dry_run: bool,
    pub outcome: Outcome,
    #[serde(default)]
    pub added: SideCounts,
    #[serde(default)]
    pub updated: SideCounts,
    #[serde(default)]
    pub deleted: SideCounts,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub errors: u64,
    /// Path of the run log, relative to the data directory.
    pub log: String,
    pub rclone_version: String,
    /// Why the run did not finish cleanly, in words: the headline of the
    /// problem that stopped it ("Backup drive not mounted"). Absent for a
    /// run that went to plan, and in records written before milestone 6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One file that failed during a run, with rclone's reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileError {
    pub path: String,
    pub reason: String,
}

/// What the summary block shows for one destination after a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub job: String,
    /// Display form of the destination, from [`Destination::display`].
    pub destination: String,
    /// `onedrive` or `local`, so the block can say "to OneDrive" or "to drive".
    pub destination_type: String,
    pub mode: Mode,
    pub outcome: Outcome,
    pub dry_run: bool,
    pub added: SideCounts,
    pub updated: SideCounts,
    pub deleted: SideCounts,
    /// Files skipped as unchanged, when known.
    pub unchanged: Option<u64>,
    pub bytes: u64,
    pub duration_ms: u64,
    pub errors: Vec<FileError>,
    /// Two-way conflicts: destination copies renamed with the conflict suffix.
    pub conflicts: Vec<String>,
    /// One-way only: files found only on the destination and left alone.
    pub destination_only: Option<u64>,
    /// Log path as it should be shown (already shortened with `~`).
    pub log: Option<String>,
    /// Two-way: folders the user chose to delete that were removed on the
    /// destination with everything left in them (hidden and system files).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_folders: Vec<String>,
    /// One-way: files moved on the destination instead of uploaded again
    /// (a dry run: those that would be). Absent when no moved file was
    /// found, or the run uploaded them again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved: Option<Moved>,
}

/// Files moved on the destination, and their bytes (not sent: a move on
/// the destination uploads nothing).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moved {
    pub files: u64,
    pub bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_destination_defaults_to_one_way() {
        let d = Destination::local("d-0001".into(), "/Volumes/Backup/Docs".into());
        assert_eq!(d.mode(), Mode::OneWay);

        let parsed: Destination =
            toml_edit::de::from_str("id = \"d-91c0\"\ntype = \"local\"\npath = \"/tmp/x\"\n")
                .unwrap();
        assert_eq!(parsed.mode(), Mode::OneWay);
    }

    #[test]
    fn new_destinations_have_no_identity_and_default_modes() {
        let d = Destination::local("d-0001".into(), "/tmp/x".into());
        assert_eq!(d.identity(), None);
        assert_eq!(d.type_name(), "local");
        let o = Destination::onedrive("d-0002".into(), "work".into(), "Docs".into(), Mode::TwoWay);
        assert_eq!(o.mode(), Mode::TwoWay);
        assert_eq!(o.display(), "work:Docs");
        let root =
            Destination::onedrive("d-0003".into(), "work".into(), String::new(), Mode::OneWay);
        assert_eq!(root.display(), "work: (drive root)");
        assert_eq!(o.identity(), None);
        let job = Job::new("a", "/a");
        assert!(job.excludes.is_empty() && job.destinations.is_empty());
        assert_eq!(job.source_identity(), None);
    }

    #[test]
    fn identity_round_trips_through_the_fields() {
        let strong = Identity {
            mount_point: "/Volumes/Backup".into(),
            volume_name: "Backup".into(),
            volume_uuid: Some("A1B2C3D4-0000-4E5F-8A9B-000000000001".into()),
            partition_uuid: None,
            device: None,
            mount_root: None,
        };
        let weak = Identity {
            mount_point: "/Volumes/BACKUP".into(),
            volume_name: "BACKUP".into(),
            volume_uuid: None,
            partition_uuid: None,
            device: Some("/dev/disk4s1".into()),
            mount_root: None,
        };
        let partition = Identity {
            mount_point: "/Volumes/STICK".into(),
            volume_name: "STICK".into(),
            volume_uuid: None,
            partition_uuid: Some("7C3457EF-0000-11AA-AA11-00306543ECAC".into()),
            device: None,
            mount_root: None,
        };
        for id in [strong, weak, partition] {
            let mut d = Destination::local("d-0001".into(), format!("{}/Docs", id.mount_point));
            d.set_identity(Some(&id));
            assert_eq!(d.identity().as_ref(), Some(&id));
            let mut job = Job::new("a", format!("{}/Src", id.mount_point));
            job.set_source_identity(Some(&id));
            assert_eq!(job.source_identity().as_ref(), Some(&id));
            d.set_identity(None);
            assert_eq!(d.identity(), None);
        }
        // Setting an identity on a OneDrive destination changes nothing.
        let mut o = Destination::onedrive("d-0002".into(), "w".into(), "D".into(), Mode::OneWay);
        let before = o.clone();
        o.set_identity(None);
        assert_eq!(o, before);
    }

    #[test]
    fn onedrive_destination_requires_mode() {
        let parsed: Result<Destination, _> = toml_edit::de::from_str(
            "id = \"d-7f3a\"\ntype = \"onedrive\"\naccount = \"work\"\npath = \"Docs\"\n",
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn destination_id_has_expected_shape() {
        for _ in 0..200 {
            let id = generate_destination_id(|_| false);
            assert!(is_valid_destination_id(&id), "bad id {id}");
        }
    }

    #[test]
    fn destination_ids_never_collide_within_a_job() {
        let mut job = Job::new("documents", "/Users/bora/Documents");
        for _ in 0..2000 {
            let id = job.new_destination_id();
            job.destinations.push(Destination::local(id, "/tmp".into()));
        }
        let unique: HashSet<&str> = job.destinations.iter().map(|d| d.id()).collect();
        assert_eq!(unique.len(), 2000);
    }

    #[test]
    fn id_generation_falls_back_to_a_scan() {
        // Everything but one id is taken: the scan must find it.
        let id = generate_destination_id(|c| c != "d-beef");
        assert_eq!(id, "d-beef");
    }

    #[test]
    fn names() {
        assert!(is_valid_name("work"));
        assert!(is_valid_name("0day-2"));
        assert!(!is_valid_name("Work"));
        assert!(!is_valid_name("-work"));
        assert!(!is_valid_name("my job"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_destination_id("d-ABCD"));
        assert!(!is_valid_destination_id("d-12345"));
    }

    #[test]
    fn outcome_names_match_serde() {
        for o in [
            Outcome::Ok,
            Outcome::Partial,
            Outcome::Failed,
            Outcome::Cancelled,
            Outcome::NeedsResync,
        ] {
            assert_eq!(serde_json::to_value(o).unwrap(), o.as_str());
        }
        for m in [Mode::OneWay, Mode::TwoWay] {
            assert_eq!(serde_json::to_value(m).unwrap(), m.as_str());
        }
    }

    #[test]
    fn run_record_matches_design_shape() {
        let json = r#"{
          "started": "2026-10-13T09:41:02+11:00",
          "finished": "2026-10-13T09:43:40+11:00",
          "mode": "two-way", "dry_run": false,
          "outcome": "needs-resync",
          "added": {"local": 2, "remote": 14}, "updated": {"local": 0, "remote": 3},
          "deleted": {"local": 1, "remote": 0},
          "bytes": 184329113, "errors": 0,
          "log": "logs/documents/d-7f3a/2026-10-13T094102.jsonl",
          "rclone_version": "1.75.2"
        }"#;
        let r: RunRecord = serde_json::from_str(json).unwrap();
        assert_eq!(r.outcome, Outcome::NeedsResync);
        assert_eq!(r.added.remote, 14);
        assert_eq!(r.mode, Mode::TwoWay);
    }
}
