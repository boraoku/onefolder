use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::model::{DriveType, Mode};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/config")
        .join(name)
}

fn load(name: &str) -> Result<ConfigFile, ConfigError> {
    ConfigFile::load(&fixture(name))
}

#[test]
fn sample_loads_with_the_design_values() {
    let file = load("sample.toml").unwrap();
    let c = &file.config;
    assert_eq!(c.version, 1);
    assert_eq!(c.accounts.len(), 1);
    assert_eq!(c.accounts[0].label, "work");
    assert_eq!(c.accounts[0].drive_type, DriveType::Business);
    assert_eq!(
        c.accounts[0].added.map(|d| d.to_string()).as_deref(),
        Some("2026-10-13T09:40:00+11:00")
    );
    let job = c.job("documents").unwrap();
    assert_eq!(job.excludes, vec![".DS_Store", "node_modules/**", "*.tmp"]);
    assert_eq!(job.destinations.len(), 2);
    assert_eq!(job.destinations[0].mode(), Mode::TwoWay);
    assert_eq!(job.destinations[0].display(), "work:Backups/Documents");
    assert_eq!(job.destinations[0].type_name(), "onedrive");
    assert_eq!(job.destinations[1].path(), "/Volumes/Backup/Documents");
    match &job.destinations[1] {
        Destination::Local {
            mode,
            volume_uuid,
            mount_point,
            ..
        } => {
            assert_eq!(*mode, Mode::OneWay);
            assert_eq!(
                volume_uuid.as_deref(),
                Some("A1B2C3D4-0000-4E5F-8A9B-000000000001")
            );
            assert_eq!(mount_point.as_deref(), Some("/Volumes/Backup"));
        }
        other => panic!("expected a local destination, got {other:?}"),
    }
    assert_eq!(c.destination_count(), 2);
}

#[test]
fn sample_round_trips_byte_for_byte() {
    let original = fs::read_to_string(fixture("sample.toml")).unwrap();
    let mut file = load("sample.toml").unwrap();
    assert_eq!(file.render().unwrap(), original);
}

#[test]
fn save_and_reload_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("onefolder/config.toml");
    let mut file = ConfigFile::load(&fixture("sample.toml")).unwrap();
    file.path = path.clone();
    file.save().unwrap();
    let again = ConfigFile::load(&path).unwrap();
    assert_eq!(again.config, file.config);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        fs::read_to_string(fixture("sample.toml")).unwrap()
    );
}

#[test]
fn extras_round_trip_unchanged() {
    let original = fs::read_to_string(fixture("extras.toml")).unwrap();
    let mut file = load("extras.toml").unwrap();
    assert_eq!(file.render().unwrap(), original);
}

#[test]
fn edits_keep_comments_and_unknown_keys() {
    let mut file = load("extras.toml").unwrap();
    {
        let c = &mut file.config;
        // Rename the job: found again through its destination id.
        c.jobs[0].name = "docs".into();
        c.jobs[0].excludes.push("node_modules/**".into());
        let id = c.jobs[0].new_destination_id();
        assert_ne!(id, "d-7f3a");
        // A fixed id keeps the snapshot stable.
        c.jobs[0]
            .destinations
            .push(Destination::local("d-0003".into(), "/tmp/backup".into()));
        c.accounts.push(Account {
            label: "personal".into(),
            remote: Account::remote_for("personal"),
            drive_type: DriveType::Personal,
            added: None,
            delta: false,
        });
        c.jobs.push(Job {
            name: "photos".into(),
            source: "/Users/bora/Pictures".into(),
            destinations: vec![Destination::onedrive(
                "d-0002".into(),
                "personal".into(),
                "Photos".into(),
                Mode::OneWay,
            )],
            ..Job::default()
        });
    }
    let text = file.render().unwrap();
    for kept in [
        "# OneFolder config, edited by hand.",
        "theme = \"dark\"   # not a OneFolder key yet; must survive a save",
        "label      = \"work\"                 # the work tenant",
        "delta      = true                   # unknown to milestone 1",
        "notes    = \"keep me\"",
        "# renamed in a test",
        "  # The cloud copy.",
        "colour  = \"blue\"                  # unknown key in a destination",
        "[extra]",
        "anything = [1, 2, 3]",
        "   # spaced oddly on purpose",
    ] {
        assert!(text.contains(kept), "lost {kept:?} in:\n{text}");
    }
    assert!(text.contains("name     = \"docs\""), "{text}");
    assert!(!text.contains("\"documents\""), "{text}");

    // The result parses back to the same model.
    let again = ConfigFile::parse(Path::new("config.toml"), &text).unwrap();
    assert_eq!(again.config, file.config);
    assert_eq!(again.config.jobs[0].destinations[0].id(), "d-7f3a");

    insta::with_settings!({
        snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
        prepend_module_to_snapshot => false,
    }, {
        insta::assert_snapshot!("config_edited_extras", text);
    });
}

#[test]
fn removing_items_drops_their_tables_only() {
    let mut file = load("sample.toml").unwrap();
    file.config.jobs[0].destinations.remove(1);
    let text = file.render().unwrap();
    assert!(!text.contains("d-91c0"));
    assert!(!text.contains("volume_uuid"));
    assert!(text.contains("id      = \"d-7f3a\"                # stable short id"));
}

#[test]
fn missing_file_is_empty_version_1() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = ConfigFile::load(&dir.path().join("config.toml")).unwrap();
    assert!(!file.existed);
    assert_eq!(file.config, Config::default());
    assert_eq!(file.config.version, 1);
    assert_eq!(file.render().unwrap(), "version = 1\n");
}

#[test]
fn missing_version_reads_as_current() {
    let file = ConfigFile::parse(Path::new("c.toml"), "").unwrap();
    assert_eq!(file.config.version, CONFIG_VERSION);
}

#[test]
fn newer_version_is_refused() {
    let err = load("newer-version.toml").unwrap_err();
    assert!(
        matches!(
            err,
            ConfigError::NewerVersion {
                found: 2,
                supported: 1,
                ..
            }
        ),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("newer OneFolder (config version 2)"), "{msg}");
    assert!(msg.contains("Update OneFolder"), "{msg}");
}

#[test]
fn odd_versions_are_refused() {
    for text in ["version = 0", "version = \"1\"", "version = -3"] {
        let err = ConfigFile::parse(Path::new("c.toml"), text).unwrap_err();
        assert!(
            matches!(err, ConfigError::BadVersion { .. }),
            "{text}: {err:?}"
        );
    }
}

#[test]
fn syntax_errors_are_reported() {
    let err = load("invalid-syntax.toml").unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }), "{err:?}");
}

/// Each invalid fixture, with a phrase the message must contain.
#[test]
fn every_validation_rule_has_a_failing_fixture() {
    let cases = [
        (
            "invalid-duplicate-job.toml",
            "job name \"documents\" is used more than once",
        ),
        (
            "invalid-duplicate-account.toml",
            "account label \"work\" is used more than once",
        ),
        (
            "invalid-duplicate-destination-id.toml",
            "uses destination id \"d-0001\" more than once",
        ),
        (
            "invalid-unknown-account.toml",
            "uses account \"personal\", which does not exist",
        ),
        (
            "invalid-relative-source.toml",
            "\"~/Documents\", which is not an absolute path",
        ),
        (
            "invalid-account-label.toml",
            "account label \"Work Account\" must be lowercase",
        ),
        (
            "invalid-job-name.toml",
            "job name \"-documents\" must be lowercase",
        ),
        ("invalid-remote-name.toml", "expected \"onefolder-work\""),
        (
            "invalid-local-relative-path.toml",
            "\"Backup/Documents\", which is not an absolute path",
        ),
        (
            "invalid-volume-without-uuid.toml",
            "is under /Volumes but has no volume_uuid (or volume_partition_uuid, or volume_device and volume_name) recorded",
        ),
        (
            "invalid-volume-device-without-name.toml",
            "is under /Volumes but has no volume_uuid",
        ),
        (
            "invalid-source-volume.toml",
            "has its source under /Volumes but no source_volume_uuid",
        ),
        (
            "invalid-destination-inside-source.toml",
            "(/Users/bora/Documents/Backup) is inside the source folder",
        ),
        (
            "invalid-duplicate-destination.toml",
            "lists work:/Docs/ more than once",
        ),
        ("invalid-empty-exclude.toml", "has an empty exclude pattern"),
        (
            "invalid-destination-id.toml",
            "destination id \"backup\", expected",
        ),
    ];
    for (name, phrase) in cases {
        let err = load(name).unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid { .. }),
            "{name}: expected a validation error, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains(phrase), "{name}: {msg:?} lacks {phrase:?}");
    }
}

/// Values outside the allowed set are caught while reading the shape.
#[test]
fn bad_enum_values_are_refused() {
    let cases = [
        ("invalid-mode.toml", "mirror"),
        ("invalid-drive-type.toml", "family"),
        ("invalid-destination-type.toml", "dropbox"),
    ];
    for (name, value) in cases {
        let err = load(name).unwrap_err();
        assert!(matches!(err, ConfigError::Schema { .. }), "{name}: {err:?}");
        assert!(err.to_string().contains(value), "{name}: {err}");
    }
}

#[test]
fn several_problems_are_listed_together() {
    let text = r#"
[[accounts]]
label = "work"
remote = "work"
drive_type = "business"
[[accounts]]
label = "work"
remote = "onefolder-work"
drive_type = "personal"
"#;
    let err = ConfigFile::parse(Path::new("/x/config.toml"), text).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.starts_with("/x/config.toml has 2 problems to fix:\n  ✗ "),
        "{msg}"
    );
}

#[test]
fn save_refuses_an_invalid_config() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = ConfigFile::load(&dir.path().join("config.toml")).unwrap();
    file.config.jobs.push(Job::new("x", "relative"));
    assert!(file.save().is_err());
    assert!(!dir.path().join("config.toml").exists());
}

#[test]
fn paths_compose_file_names() {
    let p = Paths {
        config_dir: PathBuf::from("/c"),
        data_dir: PathBuf::from("/d"),
    };
    assert_eq!(p.config_file(), PathBuf::from("/c/config.toml"));
    assert_eq!(p.state_file(), PathBuf::from("/d/state.json"));
    assert_eq!(p.logs_dir(), PathBuf::from("/d/logs"));
    assert_eq!(p.bisync_dir(), PathBuf::from("/d/bisync"));
}

/// The only unit test that touches these variables, so it cannot race
/// another test in this binary.
#[test]
fn paths_honour_the_environment_overrides() {
    use super::paths::{CONFIG_DIR_ENV, DATA_DIR_ENV};
    let saved = (
        std::env::var_os(CONFIG_DIR_ENV),
        std::env::var_os(DATA_DIR_ENV),
    );

    std::env::set_var(CONFIG_DIR_ENV, "/tmp/of-config");
    std::env::set_var(DATA_DIR_ENV, "/tmp/of-data");
    let p = Paths::from_env().unwrap();
    assert_eq!(p.config_dir, PathBuf::from("/tmp/of-config"));
    assert_eq!(p.data_dir, PathBuf::from("/tmp/of-data"));

    // Empty values are ignored and the defaults under the home directory apply.
    std::env::set_var(CONFIG_DIR_ENV, "");
    std::env::remove_var(DATA_DIR_ENV);
    let p = Paths::from_env().unwrap();
    assert!(p.config_dir.ends_with(".config/onefolder"), "{p:?}");
    assert!(p.data_dir.ends_with(".local/share/onefolder"), "{p:?}");

    for (name, value) in [(CONFIG_DIR_ENV, saved.0), (DATA_DIR_ENV, saved.1)] {
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }
}

const THREE_JOBS: &str = r#"version = 1

# Job a.
[[jobs]]
name = "a"
source = "/a"

  # a's destination.
  [[jobs.destinations]]
  id = "d-000a"
  type = "local"
  path = "/tmp/a"

# Job b.
[[jobs]]
name = "b"
source = "/b"
owner = "b"   # unknown key that belongs to b

  [[jobs.destinations]]
  id = "d-0001"
  type = "local"
  path = "/tmp/b1"

  [[jobs.destinations]]
  id = "d-0002"
  type = "local"
  path = "/tmp/b2"

# Job c.
[[jobs]]
name = "c"
source = "/c"

[extra]
kept = true
"#;

fn three_jobs() -> ConfigFile {
    ConfigFile::parse(Path::new("config.toml"), THREE_JOBS).unwrap()
}

fn reload(file: &mut ConfigFile) -> (String, Config) {
    let text = file.render().unwrap();
    let again = ConfigFile::parse(Path::new("config.toml"), &text)
        .unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(again.config, file.config, "{text}");
    (text, again.config)
}

fn job_names(c: &Config) -> Vec<&str> {
    c.jobs.iter().map(|j| j.name.as_str()).collect()
}

fn dest_ids(job: &Job) -> Vec<&str> {
    job.destinations.iter().map(|d| d.id()).collect()
}

#[test]
fn reversed_destinations_are_saved_in_the_new_order() {
    let mut file = load("sample.toml").unwrap();
    file.config.jobs[0].destinations.reverse();
    let (text, again) = reload(&mut file);
    assert_eq!(dest_ids(&again.jobs[0]), vec!["d-91c0", "d-7f3a"]);
    assert!(text.find("d-91c0").unwrap() < text.find("d-7f3a").unwrap());
    // Comments travel with their table.
    assert!(text.contains("# stable short id; names log and bisync directories"));
}

#[test]
fn reversed_jobs_are_saved_in_the_new_order() {
    let mut file = three_jobs();
    file.config.jobs.reverse();
    let (text, again) = reload(&mut file);
    assert_eq!(job_names(&again), vec!["c", "b", "a"]);
    assert_eq!(dest_ids(&again.jobs[1]), vec!["d-0001", "d-0002"]);
    let pos = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle} in {text}"))
    };
    assert!(pos("# Job c.") < pos("# Job b.") && pos("# Job b.") < pos("# Job a."));
    assert!(pos("# Job a.") < pos("# a's destination."));
    assert!(pos("# a's destination.") < pos("[extra]"), "{text}");
    assert!(text.ends_with("[extra]\nkept = true\n"), "{text}");
}

#[test]
fn a_job_inserted_at_the_front_comes_first() {
    let mut file = three_jobs();
    file.config.jobs.insert(
        0,
        Job {
            name: "first".into(),
            source: "/first".into(),
            destinations: vec![Destination::local("d-00f0".into(), "/tmp/f".into())],
            ..Job::default()
        },
    );
    file.config.jobs[2]
        .destinations
        .insert(0, Destination::local("d-0003".into(), "/tmp/b3".into()));
    let (text, again) = reload(&mut file);
    assert_eq!(job_names(&again), vec!["first", "a", "b", "c"]);
    assert_eq!(dest_ids(&again.jobs[2]), vec!["d-0003", "d-0001", "d-0002"]);
    assert!(text.find("\"first\"").unwrap() < text.find("# Job a.").unwrap());
}

#[test]
fn a_new_job_never_takes_another_jobs_table_through_an_id() {
    let mut file = three_jobs();
    // "a2" is new and happens to reuse b's destination id: ids are only unique per job.
    file.config.jobs.insert(
        0,
        Job {
            name: "a2".into(),
            source: "/a2".into(),
            destinations: vec![Destination::local("d-0001".into(), "/tmp/x".into())],
            ..Job::default()
        },
    );
    let (text, _) = reload(&mut file);
    let a2 = text.find("name = \"a2\"").unwrap();
    let b = text.find("name = \"b\"").unwrap();
    let owner = text
        .find("owner = \"b\"   # unknown key that belongs to b")
        .unwrap();
    assert!(a2 < b && b < owner, "{text}");
    assert_eq!(text.matches("owner").count(), 1);
}

#[test]
fn a_renamed_job_keeps_its_table_ids_and_comments() {
    let mut file = three_jobs();
    file.config.jobs[1].name = "bee".into();
    let (text, again) = reload(&mut file);
    assert_eq!(job_names(&again), vec!["a", "bee", "c"]);
    assert_eq!(dest_ids(&again.jobs[1]), vec!["d-0001", "d-0002"]);
    assert!(
        text.contains("# Job b.\n[[jobs]]\nname = \"bee\"\nsource = \"/b\"\nowner = \"b\""),
        "{text}"
    );
}

#[test]
fn removing_a_job_keeps_its_neighbours_comments() {
    let mut file = three_jobs();
    file.config.jobs.remove(1);
    let (text, again) = reload(&mut file);
    assert_eq!(job_names(&again), vec!["a", "c"]);
    for kept in ["# Job a.", "# a's destination.", "# Job c.", "[extra]"] {
        assert!(text.contains(kept), "lost {kept:?}:\n{text}");
    }
    assert!(!text.contains("owner"));
    assert!(!text.contains("d-0001"));
}

#[test]
fn defaults_are_not_written_unless_already_present() {
    let mut file = three_jobs();
    file.config.jobs[0].source = "/a-moved".into();
    let (text, _) = reload(&mut file);
    assert!(!text.contains("excludes"), "{text}");
    assert!(!text.contains("mode"), "{text}");

    // An explicit default in the file stays.
    let mut file = load("sample.toml").unwrap();
    file.config.jobs[0].excludes.clear();
    let (text, _) = reload(&mut file);
    assert!(text.contains("mode        = \"one-way\"           # default for local destinations"));
    assert!(
        text.contains("excludes = []   # rclone filter syntax"),
        "{text}"
    );
}

#[test]
fn new_keys_follow_the_indentation_of_their_table() {
    let mut file = load("sample.toml").unwrap();
    if let Destination::Onedrive { mode, .. } = &mut file.config.jobs[0].destinations[0] {
        *mode = Mode::OneWay;
    }
    if let Destination::Local { volume_name, .. } = &mut file.config.jobs[0].destinations[1] {
        *volume_name = None;
    }
    file.config.jobs[0]
        .destinations
        .push(Destination::local("d-0bad".into(), "/tmp/new".into()));
    let (text, _) = reload(&mut file);
    assert!(
        text.contains("\n  [[jobs.destinations]]\n  id = \"d-0bad\"\n  type = \"local\"\n  path = \"/tmp/new\"\n"),
        "{text:?}"
    );
    assert!(
        text.contains("  mode    = \"one-way\"               # one-way | two-way"),
        "{text}"
    );
    assert!(!text.contains("volume_name"));

    // A key added to an existing indented table is indented like its siblings.
    if let Destination::Local { volume_name, .. } = &mut file.config.jobs[0].destinations[1] {
        *volume_name = Some("Backup".into());
    }
    let (text, _) = reload(&mut file);
    assert!(
        text.contains("  mount_point = \"/Volumes/Backup\"\n  volume_name = \"Backup\"\n"),
        "{text}"
    );
}

#[test]
fn volumes_rule_is_not_fooled_by_case_or_dots() {
    for path in [
        "/VOLUMES/Backup/Docs",
        "/private/../Volumes/Backup",
        "/./volumes/x",
    ] {
        let text = format!(
            "[[jobs]]\nname = \"a\"\nsource = \"/a\"\n[[jobs.destinations]]\nid = \"d-0001\"\ntype = \"local\"\npath = \"{path}\"\n"
        );
        let err = ConfigFile::parse(Path::new("c.toml"), &text).unwrap_err();
        assert!(err.to_string().contains("no volume_uuid"), "{path}: {err}");
    }
    let ok = "[[jobs]]\nname = \"a\"\nsource = \"/a\"\n[[jobs.destinations]]\nid = \"d-0001\"\ntype = \"local\"\npath = \"/Volumesque/x\"\n";
    assert!(ConfigFile::parse(Path::new("c.toml"), ok).is_ok());
}

#[test]
fn both_destination_types_and_volume_fields_round_trip() {
    let original = fs::read_to_string(fixture("both-destination-types.toml")).unwrap();
    let mut file = load("both-destination-types.toml").unwrap();
    assert_eq!(file.render().unwrap(), original, "byte for byte");

    let job = file.config.job("projects").unwrap().clone();
    let source = job.source_identity().unwrap();
    assert_eq!(source.mount_point, "/Volumes/Work");
    assert_eq!(
        source.volume_uuid.as_deref(),
        Some("B0B0B0B0-0000-4E5F-8A9B-000000000002")
    );
    let local = job.destinations[1].identity().unwrap();
    assert!(local.is_weak());
    assert_eq!(local.device.as_deref(), Some("/dev/disk4s1"));
    assert_eq!(job.destinations[1].mode(), Mode::OneWay);

    // Swap the weak identity for a UUID: the device key goes, the uuid key comes.
    let strong = crate::volume::Identity {
        mount_point: "/Volumes/BACKUP".into(),
        volume_name: "BACKUP".into(),
        volume_uuid: Some("C0C0C0C0-0000-4E5F-8A9B-000000000003".into()),
        partition_uuid: None,
        device: None,
        mount_root: None,
    };
    file.config.jobs[0].destinations[1].set_identity(Some(&strong));
    let (text, again) = reload(&mut file);
    assert!(!text.contains("volume_device"), "{text}");
    assert!(
        text.contains("  volume_uuid = \"C0C0C0C0-0000-4E5F-8A9B-000000000003\"\n"),
        "{text}"
    );
    assert!(text.contains("# exFAT stick, no UUID"), "{text}");
    assert_eq!(again.jobs[0].destinations[1].identity(), Some(strong));
}

#[test]
fn a_device_and_name_satisfy_the_volumes_rule() {
    let text = "[[jobs]]\nname = \"a\"\nsource = \"/a\"\n[[jobs.destinations]]\nid = \"d-0001\"\ntype = \"local\"\npath = \"/Volumes/BACKUP/x\"\nvolume_device = \"/dev/disk4s1\"\nvolume_name = \"BACKUP\"\n";
    let file = ConfigFile::parse(Path::new("c.toml"), text).unwrap();
    let id = file.config.jobs[0].destinations[0].identity().unwrap();
    assert_eq!(id.mount_point, "/Volumes/BACKUP", "derived from the path");
}

#[test]
fn renaming_a_job_keeps_its_destination_ids_through_a_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut file = load("both-destination-types.toml").unwrap();
    file.path = path.clone();
    file.config.jobs[0].name = "work-projects".into();
    file.save().unwrap();
    let again = ConfigFile::load(&path).unwrap();
    let ids: Vec<&str> = again.config.jobs[0]
        .destinations
        .iter()
        .map(|d| d.id())
        .collect();
    assert_eq!(again.config.jobs[0].name, "work-projects");
    assert_eq!(ids, vec!["d-7f3a", "d-91c0"]);
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("source = \"/Volumes/Work/Projects\"   # on an external drive"),
        "{text}"
    );
}

#[test]
fn validate_job_needs_no_other_jobs() {
    let job = Job::new("Bad", "rel");
    let problems = validate_job(&job, &[]);
    assert_eq!(problems.len(), 2, "{problems:?}");
    let mut ok = Job::new("ok", "/Users/bora/Documents");
    ok.destinations.push(Destination::local(
        "d-0001".into(),
        "/Users/bora/Backup".into(),
    ));
    assert!(validate_job(&ok, &[]).is_empty());
    // The source inside the destination is caught too.
    ok.destinations[0] = Destination::local("d-0001".into(), "/USERS/bora".into());
    assert!(validate_job(&ok, &[])[0].contains("contains the source folder"));
    ok.destinations[0] = Destination::local("d-0001".into(), "/Users/bora/Documents/.".into());
    assert!(validate_job(&ok, &[])[0].contains("is the source folder itself"));
}

#[test]
fn accounts_are_added_and_removed_with_delta() {
    let mut file = ConfigFile::parse(
        Path::new("config.toml"),
        "version = 1\n\n[[accounts]]\nlabel = \"work\"   # mine\nremote = \"onefolder-work\"\ndrive_type = \"business\"\ndelta = false\n",
    )
    .unwrap();
    assert!(!file.config.accounts[0].delta);
    file.config.accounts.push(Account {
        label: "home".into(),
        remote: Account::remote_for("home"),
        drive_type: DriveType::Personal,
        added: Some("2026-10-13T02:52:00Z".parse().unwrap()),
        delta: true,
    });
    let text = file.render().unwrap();
    // A hand-written default survives; a new account writes delta only when on.
    assert!(text.contains("delta = false"), "{text}");
    assert!(text.contains("label = \"home\""), "{text}");
    assert!(text.contains("added = 2026-10-13T02:52:00Z"), "{text}");
    assert!(text.contains("delta = true"), "{text}");

    file.config.accounts.retain(|a| a.label != "work");
    let text = file.render().unwrap();
    assert!(!text.contains("work"), "{text}");
    assert!(!text.contains("# mine"), "{text}");
    let again = ConfigFile::parse(Path::new("config.toml"), &text).unwrap();
    assert_eq!(again.config.accounts.len(), 1);
    assert!(again.config.accounts[0].delta);
}

#[test]
fn an_empty_onedrive_path_is_the_drive_root() {
    let text = "[[accounts]]\nlabel = \"work\"\nremote = \"onefolder-work\"\ndrive_type = \"business\"\n[[jobs]]\nname = \"a\"\nsource = \"/a\"\n[[jobs.destinations]]\nid = \"d-0001\"\ntype = \"onedrive\"\naccount = \"work\"\npath = \"\"\nmode = \"one-way\"\n";
    let file = ConfigFile::parse(Path::new("c.toml"), text).unwrap();
    assert_eq!(
        file.config.jobs[0].destinations[0].display(),
        "work: (drive root)"
    );
}

#[test]
fn a_partition_uuid_satisfies_the_volumes_rule() {
    let text = "[[jobs]]\nname = \"a\"\nsource = \"/Volumes/S/src\"\nsource_volume_partition_uuid = \"P-2\"\n[[jobs.destinations]]\nid = \"d-0001\"\ntype = \"local\"\npath = \"/Volumes/BACKUP/x\"\nvolume_partition_uuid = \"P-1\"\n";
    let file = ConfigFile::parse(Path::new("c.toml"), text).unwrap();
    let job = &file.config.jobs[0];
    assert_eq!(
        job.destinations[0]
            .identity()
            .unwrap()
            .partition_uuid
            .as_deref(),
        Some("P-1")
    );
    assert_eq!(job.source_identity().unwrap().mount_point, "/Volumes/S");
}

#[cfg(unix)]
#[test]
fn overlaps_through_a_symlink_are_found_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir_all(real.join("src")).unwrap();
    std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
    let mut job = Job::new("a", real.join("src").display().to_string());
    job.destinations.push(Destination::local(
        "d-0001".into(),
        dir.path().join("link/src/inner/new").display().to_string(),
    ));
    assert!(validate_job(&job, &[]).is_empty(), "lexically fine");
    let found = overlaps_on_disk(&job);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("is inside the source folder (after following symlinks)"));
    assert_eq!(
        resolve_existing(&dir.path().join("link/src/inner/new")),
        fs::canonicalize(&real).unwrap().join("src/inner/new")
    );
}

#[test]
fn copy_hidden_defaults_to_off_and_round_trips_with_comments() {
    let text = "version = 1\n\n# my jobs\n[[jobs]]\nname = \"docs\"   # the main one\nsource = \"/a\"\n\n[[jobs]]\nname = \"code\"\nsource = \"/b\"\ncopy_hidden = false   # leave dot files out\n";
    let mut file = ConfigFile::parse(Path::new("config.toml"), text).unwrap();
    // A missing key is off, as is a hand-written false.
    assert!(!file.config.jobs[0].copy_hidden);
    assert!(!file.config.jobs[1].copy_hidden);
    // Nothing changed: the file is written back as it was.
    assert_eq!(file.render().unwrap(), text);

    // Turning it on writes the key; the comments stay where they were.
    file.config.jobs[0].copy_hidden = true;
    let on = file.render().unwrap();
    assert!(on.contains("# my jobs"), "{on}");
    assert!(on.contains("name = \"docs\"   # the main one"), "{on}");
    assert!(on.contains("copy_hidden = true"), "{on}");
    assert!(
        on.contains("copy_hidden = false   # leave dot files out"),
        "{on}"
    );
    let again = ConfigFile::parse(Path::new("config.toml"), &on).unwrap();
    assert!(again.config.jobs[0].copy_hidden);
    assert!(!again.config.jobs[1].copy_hidden);

    // Turning it off again: a key the file did not have before is written
    // as false (the file now has it), and the hand-written one keeps its
    // comment when it flips.
    let mut file = again;
    file.config.jobs[0].copy_hidden = false;
    file.config.jobs[1].copy_hidden = true;
    let flipped = file.render().unwrap();
    assert!(
        flipped.contains("copy_hidden = true   # leave dot files out"),
        "{flipped}"
    );
    let again = ConfigFile::parse(Path::new("config.toml"), &flipped).unwrap();
    assert!(!again.config.jobs[0].copy_hidden);
    assert!(again.config.jobs[1].copy_hidden);

    // A new job writes the key only when it is on.
    let mut file = ConfigFile::parse(Path::new("config.toml"), "version = 1\n").unwrap();
    file.config.jobs.push(Job::new("plain", "/c"));
    let mut hidden = Job::new("dots", "/d");
    hidden.copy_hidden = true;
    file.config.jobs.push(hidden);
    let text = file.render().unwrap();
    assert_eq!(text.matches("copy_hidden").count(), 1, "{text}");
    assert!(text.contains("copy_hidden = true"), "{text}");
}
