//! End-to-end tests of the `onefolder` binary. They never touch the real
//! config: every run points `ONEFOLDER_CONFIG_DIR` and `ONEFOLDER_DATA_DIR`
//! at a temporary directory.

use std::path::Path;

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// The binary's words on this platform, as `crate::platform` picks them:
/// macOS names the Mac and Homebrew; Linux names "this computer" and
/// rclone's own installer (these tests keep `brew` off PATH).
const MAC: bool = cfg!(target_os = "macos");
const THIS_COMPUTER: &str = if MAC { "this Mac" } else { "this computer" };
const SYSTEM_DISK: &str = if MAC {
    "the startup disk"
} else {
    "the system disk"
};
const INSTALL: &str = if MAC {
    "install it with: brew install rclone"
} else {
    "install it from https://rclone.org/install/ (or brew install rclone with Homebrew)"
};
const UPDATE: &str = if MAC {
    "update it with: brew upgrade rclone"
} else {
    "update it from https://rclone.org/install/"
};

const BREW_MESSAGE: &str = if MAC {
    "\
OneFolder needs rclone 1.66 or newer, and it is not on your PATH.

  Install it with Homebrew:   brew install rclone
  Then run onefolder again.
"
} else {
    "\
OneFolder needs rclone 1.66 or newer, and it is not on your PATH.

  Install it from https://rclone.org/install/ (or brew install rclone with Homebrew)
  Then run onefolder again.
"
};

/// The binary, isolated from the user's files.
fn onefolder(home: &TempDir) -> Command {
    let mut cmd = cargo_bin_cmd!("onefolder");
    cmd.env("ONEFOLDER_CONFIG_DIR", home.path().join("config"))
        .env("ONEFOLDER_DATA_DIR", home.path().join("data"))
        .env("NO_COLOR", "1")
        .env_remove("RCLONE_CONFIG");
    cmd
}

/// A PATH holding nothing at all.
fn empty_path() -> TempDir {
    tempfile::tempdir().unwrap()
}

/// A PATH holding a fake rclone that reports `version_line`.
#[cfg(unix)]
fn fake_rclone_path(version_line: &str) -> TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("rclone");
    std::fs::write(&bin, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn regex_escape(text: &str) -> String {
    text.replace('.', r"\.")
}

fn json(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).expect("stdout is one JSON document")
}

#[test]
fn version_prints_the_crate_version() {
    let home = tempfile::tempdir().unwrap();
    onefolder(&home).arg("version").assert().success().stdout(
        predicate::str::is_match(format!(
            r"^onefolder {} \(commit [0-9a-z]+\)\n",
            regex_escape(env!("CARGO_PKG_VERSION"))
        ))
        .unwrap(),
    );
}

#[test]
fn version_without_rclone_still_succeeds() {
    let home = tempfile::tempdir().unwrap();
    let path = empty_path();
    onefolder(&home)
        .env("PATH", path.path())
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("rclone not found on PATH"));
}

#[cfg(unix)]
#[test]
fn version_reports_the_rclone_found() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.75.2");
    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["version", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    assert_eq!(v["command"], "version");
    assert_eq!(v["data"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["data"]["rclone"]["version"], "1.75.2");
    assert_eq!(v["data"]["rclone"]["supported"], true);
    assert!(v["data"]["commit"].as_str().is_some_and(|c| !c.is_empty()));
}

#[cfg(unix)]
#[test]
fn version_json_with_an_old_rclone() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.60.0");
    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["version", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["rclone"]["found"], true);
    assert_eq!(v["data"]["rclone"]["version"], "1.60.0");
    assert_eq!(v["data"]["rclone"]["supported"], false);

    onefolder(&home)
        .env("PATH", path.path())
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("rclone 1.60.0 at "))
        .stdout(predicate::str::contains(format!(
            "older than 1.66 · {UPDATE}"
        )));
}

#[test]
fn help_lists_every_subcommand() {
    let home = tempfile::tempdir().unwrap();
    let mut assert = onefolder(&home).arg("--help").assert().success();
    for sub in [
        "accounts",
        "jobs",
        "run",
        "status",
        "doctor",
        "completion",
        "version",
    ] {
        assert = assert.stdout(predicate::str::contains(format!("  {sub} ")));
    }
    assert
        .stdout(predicate::str::contains("--json"))
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn run_help_lists_its_flags() {
    let home = tempfile::tempdir().unwrap();
    let mut assert = onefolder(&home).args(["run", "--help"]).assert().success();
    for flag in [
        "--dry-run",
        "--yes",
        "--json",
        "--folder-deletes",
        "--dest",
        "--resync",
    ] {
        assert = assert.stdout(predicate::str::contains(flag));
    }
}

#[test]
fn doctor_json_reports_the_envelope() {
    let home = tempfile::tempdir().unwrap();
    let output = onefolder(&home)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let v = json(&output.stdout);
    assert_eq!(v["command"], "doctor");
    assert_eq!(v["onefolder"], env!("CARGO_PKG_VERSION"));
    assert!(v["error"].is_null());
    let data = &v["data"];
    assert_eq!(data["config"]["exists"], false);
    assert_eq!(data["config"]["loads"], true);
    assert_eq!(data["config"]["version"], 1);
    assert_eq!(data["config"]["accounts"], 0);
    assert_eq!(data["config"]["jobs"], 0);
    assert_eq!(
        Path::new(data["config"]["path"].as_str().unwrap()),
        home.path().join("config/config.toml")
    );
    assert_eq!(data["data_dir"]["status"], "ok");
    assert_eq!(data["rclone"]["minimum"], "1.66.0");

    // Exit code follows the result: 0 when fine, 1 when something needs attention.
    let ok = data["problems"] == 0;
    assert_eq!(v["ok"], ok);
    assert_eq!(output.status.code(), Some(if ok { 0 } else { 1 }));
}

#[test]
fn doctor_without_rclone_reports_instead_of_exiting() {
    let home = tempfile::tempdir().unwrap();
    let path = empty_path();
    let output = onefolder(&home)
        .env("PATH", path.path())
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let v = json(&output.stdout);
    assert_eq!(v["ok"], false);
    assert_eq!(v["data"]["rclone"]["status"], "problem");
    assert_eq!(v["data"]["problems"], 1);

    onefolder(&home)
        .env("PATH", path.path())
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("➤ rclone"))
        .stdout(predicate::str::contains(format!(
            "✗ not found on PATH · {INSTALL}"
        )))
        .stdout(predicate::str::contains("1 problem · 0 warnings"));
}

#[cfg(unix)]
#[test]
fn doctor_text_with_rclone() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.75.2");
    onefolder(&home)
        .env("PATH", path.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("Doctor\n\n➤ rclone\n  ✓ "))
        .stdout(predicate::str::contains("· 1.75.2 (minimum 1.66)"))
        .stdout(predicate::str::contains("0 problems · 0 warnings"));
}

#[test]
fn run_without_rclone_shows_the_brew_message() {
    let home = tempfile::tempdir().unwrap();
    let path = empty_path();
    onefolder(&home)
        .env("PATH", path.path())
        .arg("run")
        .assert()
        .code(2)
        .stdout("")
        .stderr(BREW_MESSAGE);
}

#[test]
fn every_gated_command_needs_rclone() {
    let home = tempfile::tempdir().unwrap();
    let path = empty_path();
    for args in [
        vec!["accounts"],
        vec!["accounts", "add", "work"],
        vec!["jobs", "list"],
        vec!["jobs", "release", "documents", "Old"],
        vec!["run", "documents", "--dry-run"],
        vec![],
    ] {
        onefolder(&home)
            .env("PATH", path.path())
            .args(&args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains(if MAC {
                "brew install rclone"
            } else {
                "https://rclone.org/install/"
            }));
    }
}

/// `run --json` streams NDJSON, so a missing rclone is a `refused` line
/// and a `done` line rather than an envelope.
#[test]
fn run_json_without_rclone_is_refused_in_ndjson() {
    let home = tempfile::tempdir().unwrap();
    let path = empty_path();
    let output = onefolder(&home)
        .env("PATH", path.path())
        .args(["run", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["event"], "refused");
    assert_eq!(lines[0]["code"], "rclone-missing");
    assert_eq!(lines[1]["event"], "done");
    assert_eq!(lines[1]["ok"], false);
}

#[cfg(unix)]
#[test]
fn old_rclone_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.65.1");
    onefolder(&home)
        .env("PATH", path.path())
        .args(["jobs", "list"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "OneFolder needs rclone 1.66 or newer, and the one on your PATH is 1.65.1.",
        ))
        .stderr(predicate::str::contains(if MAC {
            "brew upgrade rclone"
        } else {
            "Update it from https://rclone.org/install/"
        }));
}

/// `doctor --prune-logs` lists with `--dry-run`, then removes: old logs
/// go, the last run's log stays.
#[cfg(unix)]
#[test]
fn doctor_prunes_logs() {
    let home = tempfile::tempdir().unwrap();
    let logs = home.path().join("data/logs/documents/d-7f3a");
    std::fs::create_dir_all(&logs).unwrap();
    let old = logs.join("2020-01-01T090000.jsonl");
    let kept = logs.join("2020-01-02T090000.jsonl");
    std::fs::write(&old, "old\n").unwrap();
    std::fs::write(&kept, "kept\n").unwrap();
    // A job no longer in the config keeps its logs.
    let removed_job = home.path().join("data/logs/photos/d-0001");
    std::fs::create_dir_all(&removed_job).unwrap();
    std::fs::write(removed_job.join("2020-01-01T090000.jsonl"), "x\n").unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!(
            "version = 1\n[[jobs]]\nname = \"documents\"\nsource = \"{}\"\n\n  \
             [[jobs.destinations]]\n  id = \"d-7f3a\"\n  type = \"local\"\n  path = \"{}\"\n",
            home.path().join("src").display(),
            home.path().join("dst").display()
        ),
    )
    .unwrap();
    std::fs::write(
        home.path().join("data/state.json"),
        r#"{"version":1,"runs":{"documents/d-7f3a":{"started":"2020-01-02T09:00:00+11:00",
           "finished":"2020-01-02T09:01:00+11:00","mode":"one-way","dry_run":false,
           "outcome":"ok","log":"logs/documents/d-7f3a/2020-01-02T090000.jsonl",
           "rclone_version":"1.75.2"}}}"#,
    )
    .unwrap();
    onefolder(&home)
        .args(["doctor", "--prune-logs", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Prune logs (dry run, nothing is removed)",
        ))
        .stdout(predicate::str::contains(
            "documents/d-7f3a/2020-01-01T090000.jsonl   older than 90 days",
        ))
        .stdout(predicate::str::contains(
            "photos/d-0001/2020-01-01T090000.jsonl   kept, job removed (older than 90 days)",
        ))
        .stdout(predicate::str::contains(
            "Would remove 1 file, 4 B · kept 2 logs, 7 B",
        ));
    assert!(old.exists());
    let out = onefolder(&home)
        .args(["doctor", "--prune-logs", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out.stdout);
    assert_eq!(v["command"], "doctor");
    assert_eq!(v["data"]["removed"][0]["reason"], "old");
    assert_eq!(v["data"]["kept"], 2);
    let why: Vec<&str> = v["data"]["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kept"].as_str().unwrap())
        .collect();
    assert_eq!(why, vec!["last-run", "job-removed"]);
    assert!(!old.exists());
    assert!(kept.exists(), "the last run's log stays");
}

/// `status` reads the config and state only, so it works without rclone
/// (the remotes are then "not checked"); it exits 0 whatever the runs
/// say, and 2 when the config does not load or the job is unknown.
#[test]
fn status_without_rclone_and_its_exit_codes() {
    let home = tempfile::tempdir().unwrap();
    let path = empty_path();
    let src = home.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::create_dir_all(home.path().join("data")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!(
            "version = 1\n\n[[accounts]]\nlabel = \"work\"\nremote = \"onefolder-work\"\n\
             drive_type = \"business\"\n\n[[jobs]]\nname = \"documents\"\nsource = \"{}\"\n\n  \
             [[jobs.destinations]]\n  id = \"d-0001\"\n  type = \"local\"\n  path = \"{}\"\n\n  \
             [[jobs.destinations]]\n  id = \"d-0002\"\n  type = \"onedrive\"\n  account = \"work\"\n  \
             path = \"Docs\"\n  mode = \"two-way\"\n",
            src.display(),
            home.path().join("dst").display()
        ),
    )
    .unwrap();
    std::fs::write(
        home.path().join("data/state.json"),
        r#"{"version":1,"runs":{"documents/d-0001":{"started":"2026-10-13T09:41:02+11:00",
        "finished":"2026-10-13T09:43:40+11:00","mode":"one-way","dry_run":false,"outcome":"failed",
        "log":"","rclone_version":"1.75.2","reason":"Destination folder missing"}},
        "holds":{"documents/d-0002":["Old"]}}"#,
    )
    .unwrap();
    let out = onefolder(&home)
        .env("PATH", path.path())
        .arg("status")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("Status  1 job · 2 destinations\n"),
        "{text}"
    );
    assert!(text.contains("✗ failed"), "{text}");
    assert!(
        text.contains("Reason     Destination folder missing"),
        "{text}"
    );
    assert!(
        text.contains("folder missing · no drive to check"),
        "{text}"
    );
    assert!(text.contains("never run"), "{text}");
    assert!(
        text.contains("not checked, rclone is not available"),
        "{text}"
    );
    assert!(text.contains("On hold    \"Old"), "{text}");
    assert!(text.contains("first two-way run"), "{text}");

    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["status", "documents", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out.stdout);
    assert_eq!(v["command"], "status");
    assert_eq!(v["ok"], true);
    let d = &v["data"]["jobs"][0]["destinations"];
    assert_eq!(d[0]["last_run"]["outcome"], "failed");
    assert_eq!(d[0]["volume"]["state"], "folder-missing");
    assert!(d[1]["remote_exists"].is_null());
    assert_eq!(d[1]["pending"]["state"], "first-run");
    assert_eq!(d[1]["holds"][0], "Old");

    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["status", "nope", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json(&out.stdout)["error"]["code"], "unknown-job");

    std::fs::write(home.path().join("config/config.toml"), "version = [").unwrap();
    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json(&out.stdout)["error"]["code"], "config-invalid");
}

/// `jobs release` lifts a hold on every destination that has it, says the
/// folder comes back on the next run, and refuses a folder not on hold.
#[cfg(unix)]
#[test]
fn release_lifts_a_hold() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.75.2");
    let src = home.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::create_dir_all(home.path().join("data")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!(
            "version = 1\n\n[[jobs]]\nname = \"documents\"\nsource = \"{}\"\n\n  \
             [[jobs.destinations]]\n  id = \"d-0001\"\n  type = \"local\"\n  path = \"{}\"\n  mode = \"two-way\"\n",
            src.display(),
            home.path().join("dst").display()
        ),
    )
    .unwrap();
    let state = home.path().join("data/state.json");
    std::fs::write(
        &state,
        r#"{"version":1,"holds":{"documents/d-0001":["Old projects/2019","Archive"]}}"#,
    )
    .unwrap();
    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["jobs", "release", "documents", "Old projects/2019/"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("✓ Released \"Old projects/2019\""), "{text}");
    assert!(
        text.contains(&format!(
            "copies \"Old projects/2019\" back to {THIS_COMPUTER}"
        )),
        "{text}"
    );
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(
        saved["holds"]["documents/d-0001"],
        serde_json::json!(["Archive"])
    );
    // Not on hold (any more): refused, nothing changed.
    let out = onefolder(&home)
        .env("PATH", path.path())
        .args([
            "jobs",
            "release",
            "documents",
            "Old projects/2019",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("held folders: \"Archive\""),
        "{v}"
    );
}

/// `run` refuses before anything runs: no jobs, an unknown job, and no
/// `--yes` without a terminal. Exit code 2, and with `--json` a `refused`
/// line followed by `done`.
#[cfg(unix)]
#[test]
fn run_refusals() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.75.2");
    onefolder(&home)
        .env("PATH", path.path())
        .args(["run", "documents"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("There are no jobs yet"));

    let source = home.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    let dest = home.path().join("dst");
    onefolder(&home)
        .env("PATH", path.path())
        .args(["jobs", "add", "documents", "--source"])
        .arg(&source)
        .arg("--local")
        .arg(&dest)
        .assert()
        .success();

    onefolder(&home)
        .env("PATH", path.path())
        .args(["run", "nope"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("There is no job called nope"));
    onefolder(&home)
        .env("PATH", path.path())
        .args(["run", "documents", "--dest", "d-zzzz"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("has no destination d-zzzz"));
    onefolder(&home)
        .env("PATH", path.path())
        .args(["run"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Name the job to run"));
    onefolder(&home)
        .env("PATH", path.path())
        .args(["run", "documents"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Add --yes"));

    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["run", "documents", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let lines: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["event"], "refused");
    assert_eq!(lines[0]["code"], "needs-yes");
    assert_eq!(lines[1]["event"], "done");
    assert_eq!(lines[1]["exit_code"], 2);
    assert!(!data_has_state(&home), "nothing was recorded");
}

/// Several jobs refuse before anything runs: `--dest` belongs to one job,
/// an unknown job refuses the whole batch, and no terminal needs `--yes`.
/// With `--json`, `refused` and a `batch-done` with no jobs.
#[cfg(unix)]
#[test]
fn batch_refusals() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.75.2");
    for name in ["documents", "photos"] {
        let source = home.path().join(name);
        std::fs::create_dir_all(&source).unwrap();
        onefolder(&home)
            .env("PATH", path.path())
            .args(["jobs", "add", name, "--source"])
            .arg(&source)
            .arg("--local")
            .arg(home.path().join(format!("{name}-copy")))
            .assert()
            .success();
    }
    let json = |args: &[&str]| -> (Option<i32>, Vec<serde_json::Value>) {
        let out = onefolder(&home)
            .env("PATH", path.path())
            .args(args)
            .output()
            .unwrap();
        let lines = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        (out.status.code(), lines)
    };
    for (args, code) in [
        (
            &[
                "run",
                "documents",
                "photos",
                "--dest",
                "d-0001",
                "--yes",
                "--json",
            ][..],
            "dest-needs-one-job",
        ),
        (
            &["run", "--all", "--dest", "d-0001", "--yes", "--json"][..],
            "dest-needs-one-job",
        ),
        (
            &["run", "documents", "nope", "--yes", "--json"][..],
            "unknown-job",
        ),
        (&["run", "--all", "--json"][..], "needs-yes"),
    ] {
        let (exit, lines) = json(args);
        assert_eq!(exit, Some(2), "{args:?}");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0]["event"], "refused");
        assert_eq!(lines[0]["code"], code, "{args:?}");
        assert_eq!(lines[1]["event"], "batch-done");
        assert_eq!(lines[1]["exit_code"], 2);
        assert_eq!(lines[1]["jobs"], serde_json::json!([]));
    }
    onefolder(&home)
        .env("PATH", path.path())
        .args(["run", "documents", "nope", "--yes"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "There is no job called nope. See onefolder jobs list. No job was run.",
        ));
    onefolder(&home)
        .env("PATH", path.path())
        .args(["run", "documents", "photos", "--dest", "d-0001", "--yes"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "--dest runs one destination of one job",
        ));
    // --all with job names is a usage error.
    onefolder(&home)
        .env("PATH", path.path())
        .args(["run", "--all", "documents"])
        .assert()
        .code(2);
    assert!(!data_has_state(&home), "nothing was recorded");
}

fn data_has_state(home: &TempDir) -> bool {
    home.path().join("data/state.json").exists()
}

#[cfg(unix)]
#[test]
fn menu_needs_a_terminal() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.75.2");
    onefolder(&home)
        .env("PATH", path.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("needs a terminal"));
}

#[test]
fn completion_scripts() {
    let home = tempfile::tempdir().unwrap();
    for shell in ["bash", "zsh", "fish"] {
        onefolder(&home)
            .args(["completion", shell])
            .assert()
            .success()
            .stdout(predicate::str::contains("onefolder"));
    }
    onefolder(&home)
        .args(["completion", "powershell"])
        .assert()
        .failure();
}

#[test]
fn a_newer_config_is_refused_by_doctor() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(home.path().join("config/config.toml"), "version = 7\n").unwrap();
    onefolder(&home)
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            "newer OneFolder (config version 7)",
        ));
}

/// A reader that goes away early (`onefolder doctor | head -1`) is not an
/// error, and certainly not a panic.
#[cfg(unix)]
#[test]
fn a_closed_pipe_is_not_an_error() {
    let home = tempfile::tempdir().unwrap();
    let bin = assert_cmd::cargo::cargo_bin!("onefolder");
    for args in ["completion zsh", "doctor", "doctor --json", "version"] {
        let script =
            format!("{{ sleep 0.3; \"$0\" {args}; }} | true; echo \"status=${{PIPESTATUS[0]}}\"");
        let out = std::process::Command::new("bash")
            .args(["-c", &script])
            .arg(bin)
            .env("ONEFOLDER_CONFIG_DIR", home.path().join("config"))
            .env("ONEFOLDER_DATA_DIR", home.path().join("data"))
            .env(
                "PATH",
                format!(
                    "/usr/bin:/bin:{}",
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("panicked"), "{args}: {stderr}");
        assert!(
            stdout.contains("status=0") || stdout.contains("status=1"),
            "{args}: {stdout} {stderr}"
        );
    }
}

// ---------------------------------------------------------------- jobs

/// A home with one hand-written account, a source folder, and a fake
/// rclone on PATH (jobs commands need rclone to be present).
#[cfg(unix)]
struct JobsHome {
    home: TempDir,
    path: TempDir,
}

#[cfg(unix)]
impl JobsHome {
    fn new() -> JobsHome {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::create_dir_all(home.path().join("Documents")).unwrap();
        // The drive the local destination goes on; its folder is created by add.
        std::fs::create_dir_all(home.path().join("Backup")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            "version = 1\n\n[[accounts]]\nlabel = \"work\"\nremote = \"onefolder-work\"\ndrive_type = \"business\"\n",
        )
        .unwrap();
        JobsHome {
            home,
            path: fake_rclone_path("rclone v1.75.2"),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = onefolder(&self.home);
        cmd.env("PATH", self.path.path());
        cmd
    }

    fn p(&self, rel: &str) -> String {
        self.home.path().join(rel).display().to_string()
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.home.path().join("config/config.toml")).unwrap()
    }

    /// `jobs add documents` with a OneDrive and a local destination.
    fn add_documents(&self) -> std::process::Output {
        self.cmd()
            .args(["jobs", "add", "documents", "--source"])
            .arg(self.p("Documents"))
            .args(["--exclude", ".DS_Store", "--exclude", "*.tmp"])
            .args(["--onedrive", "work:Backups/Documents:two-way", "--local"])
            .arg(self.p("Backup/Documents"))
            .output()
            .unwrap()
    }

    fn list_json(&self) -> serde_json::Value {
        let out = self
            .cmd()
            .args(["jobs", "list", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        json(&out.stdout)
    }
}

/// Flags get the form's warning about bisync's file names; the job is
/// still saved, as the flags asked.
#[cfg(unix)]
#[test]
fn jobs_add_with_flags_warns_about_names_too_long_for_bisync() {
    let h = JobsHome::new();
    let long = "a-long-folder-name-".repeat(6);
    std::fs::create_dir_all(h.home.path().join(&long)).unwrap();
    let out = h
        .cmd()
        .args(["jobs", "add", "long", "--source"])
        .arg(h.p(&long))
        .args(["--onedrive"])
        .arg(format!(
            "work:Backups/{}:two-way",
            "another-long-folder-name-".repeat(4)
        ))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        stdout.contains("too long for bisync's working file names"),
        "{stdout}"
    );
    assert!(h.config().contains("name = \"long\""));
}

#[cfg(unix)]
#[test]
fn jobs_add_with_flags_creates_both_destination_types() {
    let h = JobsHome::new();
    let out = h.add_documents();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.starts_with("✓ Added job documents\n"), "{stdout}");
    assert!(stdout.contains("→ work:Backups/Documents"), "{stdout}");
    assert!(stdout.contains("two-way"), "{stdout}");
    assert!(
        stdout.contains(&format!("→ created folder {}", h.p("Backup/Documents"))),
        "{stdout}"
    );
    // A temp folder is on the startup disk, which is never recorded.
    assert!(
        stdout.contains(&format!("{SYSTEM_DISK}, so there is no drive to record.")),
        "{stdout}"
    );

    assert!(h.home.path().join("Backup/Documents").is_dir());
    let config = h.config();
    assert!(config.contains("name = \"documents\""), "{config}");
    assert!(
        config.contains("excludes = [\".DS_Store\", \"*.tmp\"]"),
        "{config}"
    );
    assert!(config.contains("type = \"onedrive\""), "{config}");
    assert!(config.contains("mode = \"two-way\""), "{config}");
    assert!(config.contains("type = \"local\""), "{config}");
    assert!(
        !config.contains("one-way"),
        "the local default is not written: {config}"
    );
    assert!(!config.contains("volume_uuid"), "{config}");
    assert!(!config.contains("mount_point"), "{config}");

    // A second add with the same name is refused.
    h.cmd()
        .args(["jobs", "add", "documents", "--source"])
        .arg(h.p("Documents"))
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "there is already a job called documents",
        ));
}

#[cfg(unix)]
#[test]
fn jobs_list_json_and_text() {
    let h = JobsHome::new();
    assert!(h.add_documents().status.success());
    let v = h.list_json();
    assert_eq!(v["command"], "jobs list");
    assert_eq!(v["ok"], true);
    let jobs = v["data"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job["name"], "documents");
    assert_eq!(job["source"], h.p("Documents"));
    assert_eq!(job["excludes"], serde_json::json!([".DS_Store", "*.tmp"]));
    let dests = job["destinations"].as_array().unwrap();
    assert_eq!(dests[0]["type"], "onedrive");
    assert_eq!(dests[0]["account"], "work");
    assert_eq!(dests[0]["path"], "Backups/Documents");
    assert_eq!(dests[0]["mode"], "two-way");
    assert_eq!(dests[0]["display"], "work:Backups/Documents");
    assert!(dests[0]["volume"].is_null());
    assert_eq!(dests[1]["type"], "local");
    assert_eq!(dests[1]["mode"], "one-way");
    assert!(dests[1]["last_run"].is_null());
    assert_eq!(dests[1]["holds"], serde_json::json!([]));
    for d in dests {
        let id = d["id"].as_str().unwrap();
        assert!(id.starts_with("d-") && id.len() == 6, "{id}");
    }
    // On the startup disk: no drive recorded, so nothing to be mounted.
    assert!(dests[1]["mounted"].is_null());
    assert!(dests[1]["volume"].is_null());

    // Not a terminal: bare `jobs` prints the list too.
    for args in [vec!["jobs"], vec!["jobs", "list"]] {
        h.cmd()
            .args(&args)
            .assert()
            .success()
            .stdout(predicate::str::starts_with(
                "Jobs  1 job · 2 destinations\n\n  documents ",
            ))
            .stdout(predicate::str::contains("→ work:Backups/Documents"))
            .stdout(predicate::str::contains("two-way   never run"));
    }
}

#[cfg(unix)]
#[test]
fn jobs_edit_rename_keeps_ids_and_moves_data() {
    let h = JobsHome::new();
    assert!(h.add_documents().status.success());
    let before = h.list_json();
    let ids: Vec<String> = before["data"][0]["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .collect();
    let first = &ids[0];
    // Pretend a run happened: a log, a bisync work dir and a state record.
    let data = h.home.path().join("data");
    std::fs::create_dir_all(data.join(format!("logs/documents/{first}"))).unwrap();
    std::fs::write(
        data.join(format!("logs/documents/{first}/run.jsonl")),
        "{}\n",
    )
    .unwrap();
    std::fs::create_dir_all(data.join(format!("bisync/documents/{first}"))).unwrap();
    std::fs::write(
        data.join("state.json"),
        format!(
            r#"{{"version": 1, "runs": {{"documents/{first}": {{
              "started": "2026-10-13T09:41:02+11:00", "finished": "2026-10-13T09:43:40+11:00",
              "mode": "two-way", "dry_run": false, "outcome": "ok",
              "log": "logs/documents/{first}/run.jsonl", "rclone_version": "1.75.2"}}}},
              "holds": {{"documents/{first}": ["Old"]}}}}"#
        ),
    )
    .unwrap();

    h.cmd()
        .args(["jobs", "edit", "documents", "--rename", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "✓ Saved job docs (was documents)\n",
        ))
        .stdout(predicate::str::contains(
            "moved logs/documents to logs/docs",
        ))
        .stdout(predicate::str::contains(
            "moved bisync/documents to bisync/docs",
        ))
        .stdout(predicate::str::contains("renamed 2 state entries"));

    let after = h.list_json();
    let job = &after["data"][0];
    assert_eq!(job["name"], "docs");
    let new_ids: Vec<String> = job["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(new_ids, ids, "rename keeps destination ids");
    assert_eq!(job["destinations"][0]["last_run"]["outcome"], "ok");
    assert_eq!(job["destinations"][0]["holds"], serde_json::json!(["Old"]));
    assert!(data.join(format!("logs/docs/{first}/run.jsonl")).exists());
    assert!(data.join(format!("bisync/docs/{first}")).is_dir());
    assert!(!data.join("logs/documents").exists());

    // The other flags: change excludes, drop a destination.
    h.cmd()
        .args([
            "jobs",
            "edit",
            "docs",
            "--exclude",
            "node_modules/**",
            "--remove-dest",
        ])
        .arg(&ids[0])
        .assert()
        .success();
    let v = h.list_json();
    assert_eq!(
        v["data"][0]["excludes"],
        serde_json::json!(["node_modules/**"])
    );
    assert_eq!(v["data"][0]["destinations"].as_array().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn jobs_remove_needs_force_without_a_terminal() {
    let h = JobsHome::new();
    assert!(h.add_documents().status.success());
    h.cmd()
        .args(["jobs", "remove", "documents"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("add --force"));
    h.cmd()
        .args(["jobs", "remove", "documents", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("✓ Removed job documents\n"))
        .stdout(predicate::str::contains(
            "files on its destinations are not touched",
        ));
    assert_eq!(h.list_json()["data"], serde_json::json!([]));
    assert!(
        h.home.path().join("Backup/Documents").is_dir(),
        "files are left alone"
    );
    h.cmd()
        .args(["jobs", "remove", "documents", "--force"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("there is no job called documents"));
}

#[cfg(unix)]
#[test]
fn jobs_validation_failures_exit_1_with_clear_messages() {
    let h = JobsHome::new();
    let docs = h.p("Documents");
    let cases: Vec<(Vec<String>, &str)> = vec![
        (
            vec!["Bad-Name".into(), "--source".into(), docs.clone()],
            "job name \"Bad-Name\" must be lowercase",
        ),
        (
            vec!["a".into(), "--source".into(), h.p("Nope")],
            "that folder does not exist",
        ),
        (
            vec![
                "a".into(),
                "--source".into(),
                docs.clone(),
                "--onedrive".into(),
                "personal:Docs:one-way".into(),
            ],
            "uses account \"personal\", which does not exist",
        ),
        (
            vec![
                "a".into(),
                "--source".into(),
                docs.clone(),
                "--onedrive".into(),
                "work:Docs".into(),
            ],
            "say how to sync it, such as work:Docs:one-way or work:Docs:two-way",
        ),
        (
            vec![
                "a".into(),
                "--source".into(),
                docs.clone(),
                "--local".into(),
                format!("{docs}/inside"),
            ],
            "is inside the source folder",
        ),
        (
            vec![
                "a".into(),
                "--source".into(),
                docs.clone(),
                "--local".into(),
                "/tmp/x".into(),
                "--local".into(),
                "/tmp/x/".into(),
            ],
            "lists /tmp/x more than once",
        ),
        (
            vec!["--source".into(), docs.clone()],
            "needs a name with flags",
        ),
        (
            vec!["a".into(), "--exclude".into(), "*.tmp".into()],
            "needs --source",
        ),
    ];
    for (args, phrase) in cases {
        h.cmd()
            .args(["jobs", "add"])
            .args(&args)
            .assert()
            .code(1)
            .stderr(predicate::str::starts_with("✗ "))
            .stderr(predicate::str::contains(phrase));
    }
    assert!(!h.config().contains("[[jobs]]"), "nothing was saved");

    // With --json the failure is an envelope too.
    let out = h
        .cmd()
        .args(["jobs", "add", "a", "--json", "--source"])
        .arg(h.p("Nope"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "--json failures exit 2");
    let v = json(&out.stdout);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "invalid");

    h.cmd()
        .args(["jobs", "edit", "missing", "--rename", "x"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("there is no job called missing"));
}

#[cfg(unix)]
#[test]
fn jobs_add_without_flags_or_terminal_is_refused() {
    let h = JobsHome::new();
    h.cmd()
        .args(["jobs", "add", "documents"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "needs a terminal for the form, or flags",
        ));
    h.cmd()
        .args(["jobs", "edit", "documents"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("needs a terminal for the form"));
}

#[cfg(unix)]
#[test]
fn jobs_add_dry_run_saves_nothing() {
    let h = JobsHome::new();
    let before = h.config();
    h.cmd()
        .args(["jobs", "add", "documents", "--dry-run", "--source"])
        .arg(h.p("Documents"))
        .arg("--local")
        .arg(h.p("Backup/Documents"))
        .assert()
        .success()
        .stdout(predicate::str::contains("(dry run, nothing saved)"));
    assert_eq!(h.config(), before);
    assert!(!h.home.path().join("Backup/Documents").exists());
}

#[test]
fn jobs_add_help_documents_the_flag_syntax() {
    let home = tempfile::tempdir().unwrap();
    onefolder(&home)
        .args(["jobs", "add", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--onedrive ACCOUNT:PATH:MODE"))
        .stdout(predicate::str::contains("--local PATH[:MODE]"))
        .stdout(predicate::str::contains("a local one defaults to one-way"))
        .stdout(predicate::str::contains("--exclude <PATTERN>"));
    onefolder(&home)
        .args(["jobs", "edit", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--rename <NEW>"))
        .stdout(predicate::str::contains("--remove-dest <ID>"))
        .stdout(predicate::str::contains("--rerecord <ID>"));
}

/// The state file of a home, as JSON.
#[cfg(unix)]
fn state_json(h: &JobsHome) -> serde_json::Value {
    let text = std::fs::read_to_string(h.home.path().join("data/state.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Pretends both destinations of documents have run: a log, a bisync
/// folder and state entries each. Returns their ids.
#[cfg(unix)]
fn fake_runs(h: &JobsHome) -> Vec<String> {
    let ids: Vec<String> = h.list_json()["data"][0]["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .collect();
    let data = h.home.path().join("data");
    let mut runs = serde_json::Map::new();
    let mut bisync = serde_json::Map::new();
    for id in &ids {
        std::fs::create_dir_all(data.join(format!("logs/documents/{id}"))).unwrap();
        std::fs::write(data.join(format!("logs/documents/{id}/run.jsonl")), "{}\n").unwrap();
        std::fs::create_dir_all(data.join(format!("bisync/documents/{id}"))).unwrap();
        std::fs::write(data.join(format!("bisync/documents/{id}/x.lst")), "").unwrap();
        runs.insert(
            format!("documents/{id}"),
            serde_json::json!({
                "started": "2026-10-13T09:41:02+11:00", "finished": "2026-10-13T09:43:40+11:00",
                "mode": "two-way", "dry_run": false, "outcome": "ok",
                "log": format!("logs/documents/{id}/run.jsonl"), "rclone_version": "1.75.2"
            }),
        );
        bisync.insert(
            format!("documents/{id}"),
            serde_json::json!({"initialised": true}),
        );
    }
    let state = serde_json::json!({"version": 1, "runs": runs, "bisync": bisync});
    std::fs::write(data.join("state.json"), state.to_string()).unwrap();
    ids
}

#[cfg(unix)]
#[test]
fn jobs_rename_rewrites_log_paths_and_records_pairs() {
    let h = JobsHome::new();
    assert!(h.add_documents().status.success());
    let ids = fake_runs(&h);
    h.cmd()
        .args(["jobs", "edit", "documents", "--rename", "docs"])
        .assert()
        .success();
    let state = state_json(&h);
    let key = format!("docs/{}", ids[0]);
    assert_eq!(
        state["runs"][&key]["log"],
        format!("logs/docs/{}/run.jsonl", ids[0])
    );
    // Each surviving bisync record now names its pair.
    assert_eq!(state["bisync"][&key]["source"], h.p("Documents"));
    assert_eq!(
        state["bisync"][&key]["destination"],
        "work:Backups/Documents"
    );
}

#[cfg(unix)]
#[test]
fn jobs_rename_with_an_unreadable_state_changes_nothing() {
    let h = JobsHome::new();
    assert!(h.add_documents().status.success());
    let state_file = h.home.path().join("data/state.json");
    std::fs::create_dir_all(state_file.parent().unwrap()).unwrap();
    std::fs::write(&state_file, "{not json").unwrap();
    let before = h.config();
    h.cmd()
        .args(["jobs", "edit", "documents", "--rename", "docs"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("nothing was changed"));
    assert_eq!(h.config(), before);
}

#[cfg(unix)]
#[test]
fn jobs_rename_reports_what_did_not_move_and_exits_1() {
    let h = JobsHome::new();
    assert!(h.add_documents().status.success());
    let ids = fake_runs(&h);
    // A removed job called docs left a log file of the same name.
    let clash = h.home.path().join(format!("data/logs/docs/{}", ids[0]));
    std::fs::create_dir_all(&clash).unwrap();
    std::fs::write(clash.join("run.jsonl"), "old\n").unwrap();
    h.cmd()
        .args(["jobs", "edit", "documents", "--rename", "docs"])
        .assert()
        .code(1)
        .stdout(predicate::str::starts_with(
            "◎ Saved job docs (was documents)",
        ))
        .stdout(predicate::str::contains("run.jsonl did not move"));
    // The config was saved: the job is renamed.
    assert_eq!(h.list_json()["data"][0]["name"], "docs");
}

#[cfg(unix)]
#[test]
fn jobs_source_change_sets_two_way_state_aside() {
    let h = JobsHome::new();
    assert!(h.add_documents().status.success());
    let ids = fake_runs(&h);
    std::fs::create_dir_all(h.home.path().join("Elsewhere")).unwrap();
    h.cmd()
        .args(["jobs", "edit", "documents", "--source"])
        .arg(h.p("Elsewhere"))
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "the job's source changed, so its two-way sync state was set aside",
        ))
        .stdout(predicate::str::contains(
            "the next two-way run starts with a merge",
        ));
    let state = state_json(&h);
    assert!(state["bisync"].as_object().unwrap().is_empty(), "{state}");
    assert!(
        state["runs"][format!("documents/{}", ids[0])].is_object(),
        "runs stay"
    );
    let bisync = h.home.path().join("data/bisync/documents");
    for id in &ids {
        assert!(!bisync.join(id).exists());
        let aside = std::fs::read_dir(&bisync)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{id}.replaced-"))
            });
        assert!(aside, "{id} set aside");
    }
}

#[cfg(unix)]
#[test]
fn jobs_edit_never_recreates_an_existing_destinations_folder() {
    let h = JobsHome::new();
    assert!(h.add_documents().status.success());
    let folder = h.home.path().join("Backup/Documents");
    std::fs::remove_dir(&folder).unwrap();
    h.cmd()
        .args(["jobs", "edit", "documents", "--exclude", ".DS_Store"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created folder").not());
    assert!(
        !folder.exists(),
        "only new or re-pathed destinations get a folder"
    );
    // A new destination whose parent does not exist gets none either.
    h.cmd()
        .args(["jobs", "edit", "documents", "--local"])
        .arg(h.p("Nowhere/Deep/Copy"))
        .assert()
        .success()
        .stdout(predicate::str::contains("did not create"))
        .stdout(predicate::str::contains("its parent folder does not exist"));
    assert!(!h.home.path().join("Nowhere").exists());
}

#[cfg(unix)]
#[test]
fn jobs_remove_sets_two_way_state_aside_and_drops_state_keys() {
    let h = JobsHome::new();
    assert!(h.add_documents().status.success());
    let ids = fake_runs(&h);
    h.cmd()
        .args(["jobs", "remove", "documents", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "its two-way sync state was set aside",
        ))
        .stdout(predicate::str::contains(
            "removed 4 state entries for documents",
        ))
        .stdout(predicate::str::contains("Its run logs are kept"));
    let data = h.home.path().join("data");
    assert!(!data.join("bisync/documents").exists());
    assert!(std::fs::read_dir(data.join("bisync"))
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e
            .file_name()
            .to_string_lossy()
            .starts_with("documents.removed-")));
    assert!(data
        .join(format!("logs/documents/{}/run.jsonl", ids[0]))
        .exists());
    let state = state_json(&h);
    assert!(state["runs"].as_object().unwrap().is_empty());
    assert!(state["bisync"].as_object().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn jobs_rerecord_and_the_drive_root() {
    let h = JobsHome::new();
    h.cmd()
        .args(["jobs", "add", "root", "--source"])
        .arg(h.p("Documents"))
        .args(["--onedrive", "work::two-way", "--local"])
        .arg(h.p("Backup"))
        .assert()
        .success()
        .stdout(predicate::str::contains("→ work: (drive root)"));
    let v = h.list_json();
    let dests = v["data"][0]["destinations"].as_array().unwrap();
    assert_eq!(dests[0]["path"], "");
    let onedrive = dests[0]["id"].as_str().unwrap().to_string();
    let local = dests[1]["id"].as_str().unwrap().to_string();
    h.cmd()
        .args(["jobs", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("→ work: (drive root)"));

    // Re-recording a folder on the startup disk records nothing and keeps the id.
    h.cmd()
        .args(["jobs", "edit", "root", "--rerecord", &local])
        .assert()
        .success()
        .stdout(predicate::str::contains(SYSTEM_DISK));
    assert_eq!(
        h.list_json()["data"][0]["destinations"][1]["id"],
        local.as_str()
    );
    h.cmd()
        .args(["jobs", "edit", "root", "--rerecord", &onedrive])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "only a local destination has a drive",
        ));
    h.cmd()
        .args(["jobs", "edit", "root", "--rerecord", "d-zzzz"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("has no destination d-zzzz"));
}

/// Writes a config with one job whose local destination has a recorded
/// drive at `mount`, a plain folder on the startup disk (as an unplugged
/// drive's mount point is).
#[cfg(unix)]
fn config_with_recorded_drive(home: &TempDir, mount: &std::path::Path) {
    let src = home.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(mount.join("Docs")).unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!(
            "version = 1\n\n[[jobs]]\nname = \"documents\"\nsource = \"{}\"\n\n  \
             [[jobs.destinations]]\n  id = \"d-0001\"\n  type = \"local\"\n  path = \"{}\"\n  \
             volume_uuid = \"A1B2C3D4-0000-4E5F-8A9B-000000000001\"\n  volume_name = \"Backup\"\n  \
             mount_point = \"{}\"\n",
            src.display(),
            mount.join("Docs").display(),
            mount.display()
        ),
    )
    .unwrap();
}

/// `--rerecord` never drops a recorded drive because the folder is now on
/// the startup disk (the drive looks unplugged); `--forget-drive` is the
/// only way to drop it.
#[cfg(unix)]
#[test]
fn rerecord_keeps_a_drive_that_looks_unplugged_and_forget_drive_drops_it() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.75.2");
    let mount = std::fs::canonicalize(home.path()).unwrap().join("Backup");
    config_with_recorded_drive(&home, &mount);
    let config = home.path().join("config/config.toml");
    let before = std::fs::read_to_string(&config).unwrap();
    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["jobs", "edit", "documents", "--rerecord", "d-0001"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("the drive looks unplugged"), "{stderr}");
    assert!(
        stderr.contains("onefolder jobs edit documents --forget-drive d-0001"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        before,
        "unchanged"
    );

    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["jobs", "edit", "documents", "--forget-drive", "d-0001"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read_to_string(&config).unwrap();
    assert!(!after.contains("volume_uuid"), "{after}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("forgot the drive"));
}

/// `jobs edit`, `release` and `remove` refuse while a run of the job holds
/// its lock (a live process named onefolder).
#[cfg(unix)]
#[test]
fn jobs_are_not_changed_while_they_run() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_rclone_path("rclone v1.75.2");
    let mount = std::fs::canonicalize(home.path()).unwrap().join("Backup");
    config_with_recorded_drive(&home, &mount);
    // A stand-in for a running OneFolder: `sleep`, under that name (on
    // macOS the copy is signed again, or macOS kills it for its broken
    // signature).
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy("/bin/sleep", bin.join("onefolder")).unwrap();
    if MAC {
        let signed = std::process::Command::new("/usr/bin/codesign")
            .args(["-s", "-", "-f"])
            .arg(bin.join("onefolder"))
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(signed.success());
    }
    let mut child = std::process::Command::new(bin.join("onefolder"))
        .arg("30")
        .spawn()
        .unwrap();
    let runs = home.path().join("data/runs");
    std::fs::create_dir_all(&runs).unwrap();
    std::fs::write(
        runs.join("documents.lock"),
        format!(
            "{{\"pid\":{},\"started\":\"2099-01-01T00:00:00Z\"}}",
            child.id()
        ),
    )
    .unwrap();
    for args in [
        vec!["jobs", "edit", "documents", "--exclude", "*.tmp"],
        vec!["jobs", "release", "documents", "Old"],
        vec!["jobs", "remove", "documents", "--force"],
    ] {
        let out = onefolder(&home)
            .env("PATH", path.path())
            .args(&args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(
            stderr.contains("a run of documents is in progress"),
            "{args:?}: {stderr}"
        );
    }
    let _ = child.kill();
    let _ = child.wait();
    // With the run gone the lock is stale, and the edit goes ahead.
    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["jobs", "edit", "documents", "--exclude", "*.tmp"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!runs.join("documents.lock").exists());
}

/// A state file that does not load: status still lists the jobs, says
/// why the runs are missing, and `--json` carries `state_error` with
/// `ok: false` (exit 0).
#[test]
fn status_says_when_the_state_file_does_not_load() {
    let home = tempfile::tempdir().unwrap();
    let path = empty_path();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::create_dir_all(home.path().join("data")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        "version = 1\n\n[[jobs]]\nname = \"documents\"\nsource = \"/tmp\"\n",
    )
    .unwrap();
    std::fs::write(home.path().join("data/state.json"), "{\"version\": 9}").unwrap();
    let out = onefolder(&home)
        .env("PATH", path.path())
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out.stdout);
    assert_eq!(v["ok"], false);
    assert!(
        v["data"]["state_error"]
            .as_str()
            .unwrap()
            .contains("newer OneFolder"),
        "{v:#}"
    );
    assert_eq!(v["data"]["jobs"][0]["name"], "documents");
    let out = onefolder(&home)
        .env("PATH", path.path())
        .arg("status")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("newer OneFolder"), "{text}");
    assert!(
        text.contains("showing jobs without their last runs"),
        "{text}"
    );
}

/// `--copy-hidden yes|no` on `jobs add` and `jobs edit`: saved as
/// `copy_hidden`, shown by `jobs list` and its JSON; anything else is a
/// usage error.
#[cfg(unix)]
#[test]
fn jobs_copy_hidden_flag() {
    let h = JobsHome::new();
    let out = h
        .cmd()
        .args(["jobs", "add", "dots", "--source"])
        .arg(h.p("Documents"))
        .args(["--copy-hidden", "yes", "--local"])
        .arg(h.p("Backup/Dots"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.contains("\n  hidden files copied\n"), "{stdout}");
    assert!(h.config().contains("copy_hidden = true"), "{}", h.config());
    assert_eq!(h.list_json()["data"][0]["copy_hidden"], true);
    h.cmd()
        .args(["jobs", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("· hidden files copied"));

    // The default, and turning it off: the key stays, set to false.
    assert!(h.add_documents().status.success());
    let jobs = h.list_json();
    assert_eq!(jobs["data"][1]["name"], "documents");
    assert_eq!(jobs["data"][1]["copy_hidden"], false);
    assert!(
        !h.config().contains("copy_hidden = false"),
        "{}",
        h.config()
    );
    let out = h
        .cmd()
        .args(["jobs", "edit", "dots", "--copy-hidden", "no"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("\n  hidden files not copied\n"));
    assert_eq!(h.list_json()["data"][0]["copy_hidden"], false);
    assert!(h.config().contains("copy_hidden = false"), "{}", h.config());
    h.cmd()
        .args(["jobs", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hidden files copied").not());

    // Only yes or no.
    h.cmd()
        .args(["jobs", "edit", "dots", "--copy-hidden", "maybe"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("[possible values: yes, no]"));
    h.cmd()
        .args(["jobs", "add", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--copy-hidden <yes|no>"));
}

/// The directory holding the real rclone, if there is one. Tests that
/// need it skip without it, except with `ONEFOLDER_INTEGRATION=1`, where a
/// missing rclone fails them.
fn real_rclone_dir() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path)
        .chain([
            std::path::PathBuf::from("/opt/homebrew/bin"),
            std::path::PathBuf::from("/usr/local/bin"),
        ])
        .find(|dir| dir.join("rclone").is_file());
    if found.is_none() {
        let strict = std::env::var("ONEFOLDER_INTEGRATION").is_ok_and(|v| v == "1");
        assert!(
            !strict,
            "rclone is not installed, and ONEFOLDER_INTEGRATION is set"
        );
        eprintln!("skipped: rclone is not installed");
    }
    found
}

/// `run --json --dry-run` over a few hundred files sends live figures of
/// the preview as `progress` events, with the documented fields.
#[cfg(unix)]
#[test]
fn run_json_dry_run_reports_progress() {
    let Some(rclone_dir) = real_rclone_dir() else {
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(home.path()).unwrap();
    let (src, dst) = (root.join("src"), root.join("dst"));
    for i in 0..300 {
        let dir = src.join(format!("folder-{:02}", i % 20));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("file-{i:03}.txt")), format!("file {i}")).unwrap();
    }
    std::fs::create_dir_all(&dst).unwrap();
    for i in 0..40 {
        std::fs::write(dst.join(format!("old-{i:02}.txt")), "old").unwrap();
    }
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::write(
        root.join("config/config.toml"),
        format!(
            "version = 1\n\n[[jobs]]\nname = \"documents\"\nsource = \"{}\"\n\n  [[jobs.destinations]]\n  id = \"d-0001\"\n  type = \"local\"\n  path = \"{}\"\n",
            src.display(),
            dst.display()
        ),
    )
    .unwrap();
    let out = onefolder(&home)
        .env("PATH", format!("{}:/usr/bin:/bin", rclone_dir.display()))
        .env("RCLONE_CONFIG", root.join("rclone.conf"))
        .args(["run", "documents", "--json", "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let progress: Vec<&serde_json::Value> =
        lines.iter().filter(|l| l["event"] == "progress").collect();
    assert!(!progress.is_empty(), "{lines:#?}");
    for p in &progress {
        assert_eq!(p["job"], "documents");
        assert_eq!(p["destination"], "d-0001");
        assert_eq!(p["step"], "previewing");
        assert!(p["elapsed"].is_u64(), "{p}");
        assert!(p["checked"].is_u64(), "{p}");
        assert!(p["listed"].is_u64(), "rclone 1.75 says how many: {p}");
        assert!(p["to_copy"]["files"].as_u64().unwrap() <= 300, "{p}");
        assert!(p["to_copy"]["bytes"].is_u64(), "{p}");
        assert!(p["destination_listed"].as_u64().unwrap() <= 40, "{p}");
        assert!(p["destination_done"].is_boolean(), "{p}");
        assert!(p["still_listing"].is_boolean(), "{p}");
    }
    // The progress comes before the preview it leads to.
    let at = |kind: &str| lines.iter().position(|l| l["event"] == kind).unwrap();
    assert!(at("progress") < at("preview"));
    let preview = &lines[at("preview")];
    assert_eq!(preview["new"]["files"], 300);
    assert_eq!(preview["destination_only"], 40);
    assert_eq!(preview["copy_hidden"], false);
    assert!(preview["effective_excludes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == ".*/**"));
}

/// `--moves ask` with output piped (no terminal to ask on) never moves:
/// the moved file is uploaded again, the old copy stays, and the output
/// says why. Needs a real rclone (local folders only); skipped without
/// one, since the CI job running this file has none (the integration
/// suite covers it there).
#[test]
fn moves_ask_without_a_terminal_uploads_again() {
    let Some(rclone_dir) = real_rclone_dir() else {
        eprintln!("skipped: rclone is not installed");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(home.path()).unwrap();
    let (src, dst) = (root.join("src"), root.join("dst"));
    std::fs::create_dir_all(src.join("Docs")).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::write(src.join("Docs/report.pdf"), "the quarterly report").unwrap();
    let rclone = rclone_dir.join("rclone");
    let seeded = std::process::Command::new(&rclone)
        .env("RCLONE_CONFIG", root.join("rclone.conf"))
        .arg("copy")
        .arg(&src)
        .arg(&dst)
        .status()
        .unwrap();
    assert!(seeded.success());
    std::fs::create_dir_all(src.join("Archive")).unwrap();
    std::fs::rename(src.join("Docs/report.pdf"), src.join("Archive/report.pdf")).unwrap();
    std::fs::write(
        root.join("config/config.toml"),
        format!(
            "version = 1\n\n[[jobs]]\nname = \"documents\"\nsource = \"{}\"\n\n  \
             [[jobs.destinations]]\n  id = \"d-0001\"\n  type = \"local\"\n  path = \"{}\"\n",
            src.display(),
            dst.display()
        ),
    )
    .unwrap();
    let path = format!("{}:/usr/bin:/bin", rclone_dir.display());
    let run = |extra: &[&str]| {
        let mut args = vec!["run", "documents", "--yes", "--moves", "ask"];
        args.extend(extra);
        let out = onefolder(&home)
            .env("PATH", &path)
            .env("RCLONE_CONFIG", root.join("rclone.conf"))
            .args(&args)
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    // NDJSON: the decision is copy, not asked.
    let (code, text) = run(&["--json", "--dry-run"]);
    assert_eq!(code, Some(0), "{text}");
    let decided: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|v: &serde_json::Value| v["event"] == "moves")
        .collect();
    assert_eq!(decided.len(), 1, "{text}");
    assert_eq!(decided[0]["answer"], "copy");
    assert_eq!(decided[0]["by"], "not-asked");
    // Plain text, the real run: said plainly, and nothing moved.
    let (code, text) = run(&[]);
    assert_eq!(code, Some(0), "{text}");
    assert!(
        text.contains(
            "1 moved file: will be uploaded again, the old copy stays on the drive   \
             (--moves ask, and there is no terminal to ask on)"
        ),
        "{text}"
    );
    assert!(text.contains("will be uploaded again"), "{text}");
    assert!(!text.contains("Moved:"), "{text}");
    assert!(dst.join("Docs/report.pdf").exists(), "the old copy stays");
    assert!(dst.join("Archive/report.pdf").exists(), "uploaded again");
}
