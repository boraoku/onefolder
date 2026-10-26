//! Integration tests against a real rclone. They run only when
//! `ONEFOLDER_INTEGRATION=1` is set and rclone is on PATH:
//!
//! ```sh
//! ONEFOLDER_INTEGRATION=1 cargo test --test integration
//! ```
//!
//! Each test gets a temporary directory with OneFolder's config and data
//! directories, a private `RCLONE_CONFIG` whose `onefolder-test` remote is
//! an alias of a folder there (so the OneDrive code path, `remote:path`
//! addressing, `about` and `lsjson` on a remote, runs without a Microsoft
//! account), a source folder and a plain local destination. Files are
//! seeded with `rclone copy` so modification times match.
//!
//! One-way, two-way (with the folder prompt and the safety checks),
//! multi-destination sequencing, the volume check, the run lock and
//! `status`. The real unplugged drive is demonstrated by
//! `scripts/demo-unplugged-drive.sh`, which needs a disk image.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

/// The arguments of script(1) that run `shell` (a `/bin/sh` command line)
/// under a pseudo-terminal, writing no typescript: BSD's syntax on macOS,
/// util-linux's on Linux (`-c` runs the command through `$SHELL`, and `-e`
/// passes its exit status on).
fn script_args(shell: &str) -> Vec<String> {
    let args: &[&str] = if cfg!(target_os = "macos") {
        &["-q", "/dev/null", "/bin/sh", "-c", shell]
    } else {
        &["-q", "-e", "-c", shell, "/dev/null"]
    };
    args.iter().map(|a| a.to_string()).collect()
}

fn enabled() -> bool {
    let on = std::env::var("ONEFOLDER_INTEGRATION").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set ONEFOLDER_INTEGRATION=1 to run the integration suite");
    }
    on
}

/// The directory holding rclone: PATH first, then Homebrew's places.
fn rclone_dir() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .find(|dir| dir.join("rclone").is_file())
        .expect("ONEFOLDER_INTEGRATION is set but rclone is not installed")
}

/// One test's world.
struct World {
    _dir: TempDir,
    root: PathBuf,
    rclone_dir: PathBuf,
}

/// The ids the test config gives its destinations.
const LOCAL: &str = "d-0001";
const ALIAS: &str = "d-0002";
const TWO_WAY: &str = "d-0003";

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, so /var and /private/var do not differ between paths.
        let root = fs::canonicalize(dir.path()).unwrap();
        for d in ["config", "data", "src", "dst", "remote/Backups/Documents"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(
            root.join("rclone.conf"),
            format!(
                "[onefolder-test]\ntype = alias\nremote = {}\n",
                root.join("remote").display()
            ),
        )
        .unwrap();
        World {
            _dir: dir,
            root,
            rclone_dir: rclone_dir(),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Where the alias destination's files are on disk.
    fn alias_dir(&self) -> PathBuf {
        self.path("remote/Backups/Documents")
    }

    /// Writes the config: job `documents` with the given destinations.
    fn config(&self, destinations: &str) {
        let text = format!(
            r#"version = 1

[[accounts]]
label = "test"
remote = "onefolder-test"
drive_type = "business"

[[jobs]]
name = "documents"
source = "{src}"
excludes = [".DS_Store", "*.tmp"]
{destinations}
"#,
            src = self.path("src").display()
        );
        fs::write(self.path("config/config.toml"), text).unwrap();
    }

    fn local_destination(&self, id: &str, path: &Path) -> String {
        format!(
            "\n  [[jobs.destinations]]\n  id = \"{id}\"\n  type = \"local\"\n  path = \"{}\"\n",
            path.display()
        )
    }

    fn alias_destination(&self, id: &str, path: &str, mode: &str) -> String {
        format!(
            "\n  [[jobs.destinations]]\n  id = \"{id}\"\n  type = \"onedrive\"\n  account = \"test\"\n  path = \"{path}\"\n  mode = \"{mode}\"\n"
        )
    }

    /// The usual job: a local destination and the alias.
    fn both(&self) {
        self.config(
            &(self.local_destination(LOCAL, &self.path("dst"))
                + &self.alias_destination(ALIAS, "Backups/Documents", "one-way")),
        );
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("ONEFOLDER_CONFIG_DIR", self.path("config")),
            ("ONEFOLDER_DATA_DIR", self.path("data")),
            ("RCLONE_CONFIG", self.path("rclone.conf")),
            (
                "PATH",
                PathBuf::from(format!("{}:/usr/bin:/bin", self.rclone_dir.display())),
            ),
        ]
    }

    fn onefolder(&self) -> Command {
        let mut cmd = cargo_bin_cmd!("onefolder");
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        cmd.env("NO_COLOR", "1");
        cmd
    }

    /// `rclone copy`, so modification times match.
    fn seed(&self, from: &Path, to: &Path) {
        let status = StdCommand::new(self.rclone_dir.join("rclone"))
            .env("RCLONE_CONFIG", self.path("rclone.conf"))
            .arg("copy")
            .arg(from)
            .arg(to)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// A source with files already on both destinations, then changes:
    /// one new file, one changed file, one file only on each destination,
    /// and excluded files in the source.
    fn scenario(&self) {
        self.write("src/same.txt", "same");
        self.write("src/Projects/2026/budget.xlsx", "budget v1");
        self.seed(&self.path("src"), &self.path("dst"));
        self.seed(&self.path("src"), &self.alias_dir());
        self.write("src/Projects/2026/budget.xlsx", "budget v2, longer");
        self.write("src/Projects/2026/brief.docx", "brief");
        self.write("src/.DS_Store", "junk");
        self.write("src/draft.tmp", "scratch");
        self.write("dst/only-on-destination.txt", "keep me");
        fs::write(self.alias_dir().join("only-on-onedrive.txt"), "keep me too").unwrap();
    }

    fn state(&self) -> Value {
        let text = fs::read_to_string(self.path("data/state.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

/// Every file under `dir` with its contents and modification time.
fn tree(dir: &Path) -> BTreeMap<String, (String, std::time::SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
                let text = fs::read_to_string(&p).unwrap_or_default();
                let mtime = fs::metadata(&p).unwrap().modified().unwrap();
                out.insert(rel, (text, mtime));
            }
        }
    }
    out
}

fn ndjson(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8(stdout.to_vec())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l}: {e}")))
        .collect()
}

fn events<'a>(lines: &'a [Value], kind: &str, dest: &str) -> Vec<&'a Value> {
    lines
        .iter()
        .filter(|v| v["event"] == kind && v["destination"] == dest)
        .collect()
}

/// Checks a line against the shapes documented in src/jsonout/run.rs.
fn check_shape(v: &Value) {
    let has = |keys: &[&str]| {
        for k in keys {
            assert!(v.get(*k).is_some(), "{} lacks {k}: {v}", v["event"]);
        }
    };
    let event = v["event"].as_str().expect("every line has an event");
    // In a batch every event but `batch-done` carries the job's place.
    if let Some(b) = v.get("batch") {
        assert!(b["index"].is_u64() && b["of"].is_u64(), "{v}");
        assert!(b["index"].as_u64() <= b["of"].as_u64(), "{v}");
    }
    match event {
        "refused" => has(&["code", "message"]),
        "job-skipped" => has(&["job", "code", "reason", "batch"]),
        "batch-done" => {
            has(&["ok", "exit_code", "jobs"]);
            assert!(v.get("batch").is_none(), "{v}");
            for j in v["jobs"].as_array().unwrap() {
                assert!(j["job"].is_string() && j["outcome"].is_string(), "{j}");
                assert!(
                    j.get("reason").is_some() && j["destinations"].is_array(),
                    "{j}"
                );
            }
        }
        "done" => {
            has(&["job", "ok", "exit_code", "destinations"]);
            for d in v["destinations"].as_array().unwrap() {
                assert!(d["destination"].is_string() && d["outcome"].is_string());
            }
        }
        _ => {
            has(&["job", "destination"]);
            match event {
                "start" => has(&["type", "display", "mode", "index", "of", "dry_run"]),
                "step" => {
                    has(&["step"]);
                    let known = [
                        "checking",
                        "previewing",
                        "moving",
                        "copying",
                        "planning",
                        "restoring",
                        "syncing",
                        "resyncing",
                        "finishing",
                    ];
                    assert!(known.contains(&v["step"].as_str().unwrap()), "{v}");
                }
                "phase" => has(&["phase"]),
                "plan" => {
                    has(&[
                        "to_destination",
                        "to_local",
                        "delete_destination",
                        "delete_local",
                        "conflicts",
                        "folders",
                        "needs_confirmation",
                        "lift_max_delete",
                        "force",
                        "excludes",
                        "copy_hidden",
                        "effective_excludes",
                        "holds",
                    ]);
                    for g in ["to_destination", "to_local"] {
                        assert!(v[g]["files"].is_u64() && v[g]["bytes"].is_u64(), "{v}");
                        assert!(v[g]["new"].is_array() && v[g]["changed"].is_array());
                    }
                }
                "resync" => has(&[
                    "reason",
                    "detail",
                    "destination_missing",
                    "to_destination",
                    "to_local",
                    "kept",
                ]),
                "folder_prompt" => {
                    has(&[
                        "path",
                        "files",
                        "bytes",
                        "measured",
                        "maybe_rename",
                        "answer",
                        "by",
                    ]);
                }
                "conflict" => has(&["path", "kept_as"]),
                "preview" => {
                    has(&[
                        "new",
                        "changed",
                        "unchanged",
                        "destination_only",
                        "excludes",
                        "copy_hidden",
                        "effective_excludes",
                        "moves",
                    ]);
                    for g in ["new", "changed"] {
                        assert!(v[g]["files"].is_u64() && v[g]["bytes"].is_u64());
                        assert!(v[g]["paths"].is_array());
                    }
                    let m = &v["moves"];
                    assert!(m["count"].is_u64() && m["bytes"].is_u64(), "{v}");
                    assert!(m["pairs"].is_array() && m["groups"].is_array(), "{v}");
                    assert!(
                        ["undecided", "move", "copy"].contains(&m["choice"].as_str().unwrap()),
                        "{v}"
                    );
                }
                "moves" => {
                    has(&["answer", "by", "count", "bytes"]);
                    assert!(v["answer"] == "move" || v["answer"] == "copy", "{v}");
                }
                "stats" => has(&[
                    "bytes",
                    "total_bytes",
                    "transfers",
                    "total_transfers",
                    "checks",
                    "total_checks",
                    "errors",
                    "speed",
                    "eta",
                    "elapsed",
                    "transferring",
                ]),
                "file" => {
                    has(&["path", "size", "change", "side"]);
                    assert!(
                        ["added", "updated", "deleted"].contains(&v["change"].as_str().unwrap()),
                        "{v}"
                    );
                    assert!(v["side"] == "destination" || v["side"] == "local", "{v}");
                }
                "progress" if v["step"] == "moving" => {
                    has(&["elapsed", "moved"]);
                    assert!(
                        v["moved"]["files"].is_u64() && v["moved"]["of"].is_u64(),
                        "{v}"
                    );
                }
                "progress" => {
                    has(&[
                        "step",
                        "elapsed",
                        "checked",
                        "listed",
                        "to_copy",
                        "destination_listed",
                        "destination_done",
                        "still_listing",
                    ]);
                    assert!(v["step"] == "previewing" || v["step"] == "planning", "{v}");
                    assert!(v["to_copy"]["files"].is_u64() && v["to_copy"]["bytes"].is_u64());
                    // The destination listing is the one-way preview's own.
                    assert_eq!(v["destination_listed"].is_null(), v["step"] == "planning");
                }
                "error" => has(&["path", "message"]),
                "problem" | "note" => has(&["message"]),
                "skipped" => has(&["reason"]),
                "stopping" => {}
                "summary" => {
                    let s = &v["summary"];
                    for k in [
                        "job",
                        "destination",
                        "destination_type",
                        "mode",
                        "outcome",
                        "dry_run",
                        "added",
                        "updated",
                        "deleted",
                        "unchanged",
                        "bytes",
                        "duration_ms",
                        "errors",
                        "conflicts",
                        "destination_only",
                        "log",
                    ] {
                        assert!(s.get(k).is_some(), "summary lacks {k}: {s}");
                    }
                }
                other => panic!("undocumented event {other}: {v}"),
            }
        }
    }
}

#[test]
fn doctor_finds_the_real_rclone() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let output = w.onefolder().args(["doctor", "--json"]).output().unwrap();
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["data"]["rclone"]["status"], "ok", "{v:#}");
    assert_eq!(output.status.code(), Some(0));
}

/// New and changed files are copied to both destination types, files only
/// on the destination are left alone, excludes are honoured, the preview's
/// counts match what was transferred, the summary matches the disk, and
/// the run records land in state.json. The NDJSON stream follows the
/// documented shapes.
#[test]
fn one_way_copies_new_and_changed_files_to_both_destination_types() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.scenario();
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes", "--json"])
        .output()
        .unwrap();
    let lines = ndjson(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{lines:#?}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in &lines {
        check_shape(line);
    }
    assert_eq!(lines.last().unwrap()["event"], "done");
    assert_eq!(lines.last().unwrap()["ok"], true);

    for (id, dir, only) in [
        (LOCAL, w.path("dst"), "only-on-destination.txt"),
        (ALIAS, w.alias_dir(), "only-on-onedrive.txt"),
    ] {
        // The disk.
        let read = |rel: &str| fs::read_to_string(dir.join(rel)).unwrap();
        assert_eq!(read("Projects/2026/brief.docx"), "brief");
        assert_eq!(read("Projects/2026/budget.xlsx"), "budget v2, longer");
        assert_eq!(read("same.txt"), "same");
        assert!(
            read(only).starts_with("keep me"),
            "destination-only file untouched"
        );
        assert!(!dir.join(".DS_Store").exists(), "excluded");
        assert!(!dir.join("draft.tmp").exists(), "excluded");

        // The preview.
        let preview = events(&lines, "preview", id);
        assert_eq!(preview.len(), 1);
        let p = preview[0];
        assert_eq!(p["new"]["files"], 1, "{p}");
        assert_eq!(p["new"]["paths"][0], "Projects/2026/brief.docx");
        assert_eq!(p["changed"]["files"], 1);
        assert_eq!(p["changed"]["paths"][0], "Projects/2026/budget.xlsx");
        assert_eq!(p["unchanged"], 1);
        assert_eq!(p["destination_only"], 1);
        assert_eq!(p["excludes"], serde_json::json!([".DS_Store", "*.tmp"]));

        // What was transferred matches the preview.
        let files = events(&lines, "file", id);
        let mut got: Vec<(String, String)> = files
            .iter()
            .map(|f| {
                (
                    f["path"].as_str().unwrap().to_string(),
                    f["change"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("Projects/2026/brief.docx".to_string(), "added".to_string()),
                (
                    "Projects/2026/budget.xlsx".to_string(),
                    "updated".to_string()
                ),
            ]
        );

        // The summary matches the disk.
        let summary = &events(&lines, "summary", id)[0]["summary"];
        assert_eq!(summary["outcome"], "ok");
        assert_eq!(summary["dry_run"], false);
        assert_eq!(summary["added"]["remote"], 1);
        assert_eq!(summary["updated"]["remote"], 1);
        assert_eq!(summary["deleted"]["remote"], 0);
        assert_eq!(summary["unchanged"], 1);
        assert_eq!(summary["destination_only"], 1);
        assert_eq!(summary["bytes"], 5 + 17);
        assert_eq!(summary["errors"], serde_json::json!([]));
        let on_disk = tree(&dir).len();
        assert_eq!(
            on_disk, 4,
            "same, budget, brief and the destination-only file"
        );

        // The record.
        let state = w.state();
        let record = &state["runs"][format!("documents/{id}")];
        assert_eq!(record["outcome"], "ok", "{state:#}");
        assert_eq!(record["mode"], "one-way");
        assert_eq!(record["dry_run"], false);
        assert_eq!(record["added"]["remote"], 1);
        assert_eq!(record["updated"]["remote"], 1);
        assert_eq!(record["bytes"], 22);
        assert_eq!(record["errors"], 0);
        assert!(record["rclone_version"].as_str().unwrap().starts_with('1'));
        let log = record["log"].as_str().unwrap();
        assert!(log.starts_with(&format!("logs/documents/{id}/")), "{log}");
        let log_text = fs::read_to_string(w.path("data").join(log)).unwrap();
        let first: Value = serde_json::from_str(log_text.lines().next().unwrap()).unwrap();
        assert_eq!(first["onefolder"], "command");
        assert!(first["command"].as_str().unwrap().contains(" copy "));
        assert!(
            log_text.lines().count() > 3,
            "the raw rclone lines are kept"
        );
    }

    // A second run has nothing to do.
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let lines = ndjson(&out.stdout);
    for id in [LOCAL, ALIAS] {
        assert!(events(&lines, "file", id).is_empty());
        let s = &events(&lines, "summary", id)[0]["summary"];
        assert_eq!(s["unchanged"], 3);
        assert_eq!(s["added"]["remote"], 0);
    }
}

/// `--dry-run` shows the preview and changes nothing on any destination,
/// and records a dry run.
#[test]
fn dry_run_changes_nothing() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.scenario();
    let before = (tree(&w.path("dst")), tree(&w.alias_dir()));
    let out = w
        .onefolder()
        .args(["run", "documents", "--dry-run"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert_eq!(before, (tree(&w.path("dst")), tree(&w.alias_dir())));
    assert!(text.contains("  New        1 file     5 B"), "{text}");
    assert!(text.contains("  Changed    1 file    17 B"), "{text}");
    assert!(text.contains("Nothing will be deleted on the destination."));
    assert!(text.contains("one-way   dry run"), "{text}");
    // A dry run is recorded apart from the real runs.
    let state = w.state();
    for id in [LOCAL, ALIAS] {
        let record = &state["last_dry_run"][format!("documents/{id}")];
        assert_eq!(record["dry_run"], true);
        assert_eq!(record["outcome"], "ok");
        assert_eq!(record["added"]["remote"], 1);
        assert!(state["runs"].get(format!("documents/{id}")).is_none());
    }
    // The JSON form says the same, with no file events.
    let out = w
        .onefolder()
        .args(["run", "documents", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let lines = ndjson(&out.stdout);
    assert!(lines.iter().all(|l| l["event"] != "file"));
    assert_eq!(
        events(&lines, "summary", LOCAL)[0]["summary"]["dry_run"],
        true
    );
    assert_eq!(before, (tree(&w.path("dst")), tree(&w.alias_dir())));
}

/// An unreadable source file: the rest is copied, the outcome is partial,
/// the file and rclone's reason are listed, and the exit code is 1.
#[test]
fn a_per_file_error_is_partial() {
    if !enabled() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let w = World::new();
    w.config(&w.local_destination(LOCAL, &w.path("dst")));
    w.write("src/fine.txt", "fine");
    w.write("src/locked.txt", "secret");
    let locked = w.path("src/locked.txt");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes"])
        .output()
        .unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("one-way   partial"), "{text}");
    assert!(text.contains("Errors: 1"), "{text}");
    assert!(text.contains("  ✗ locked.txt   "), "{text}");
    assert_eq!(fs::read_to_string(w.path("dst/fine.txt")).unwrap(), "fine");
    assert!(!w.path("dst/locked.txt").exists());
    let record = &w.state()["runs"][format!("documents/{LOCAL}")];
    assert_eq!(record["outcome"], "partial");
    assert_eq!(record["errors"], 1);
    assert_eq!(record["added"]["remote"], 1);
}

/// A destination that cannot run fails and is recorded; the next ones
/// still run, a two-way one included (its first run, a merge).
#[test]
fn a_failed_destination_does_not_stop_the_next() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.config(
        &(w.local_destination(LOCAL, &w.path("missing/parent/dst"))
            + &w.alias_destination(ALIAS, "Backups/Documents", "one-way")
            + &w.alias_destination(TWO_WAY, "Backups/TwoWay", "two-way")),
    );
    w.write("src/a.txt", "a");
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes", "--json"])
        .output()
        .unwrap();
    let lines = ndjson(&out.stdout);
    for line in &lines {
        check_shape(line);
    }
    assert_eq!(out.status.code(), Some(1), "{lines:#?}");
    let problem = &events(&lines, "problem", LOCAL)[0]["message"];
    assert!(
        problem
            .as_str()
            .unwrap()
            .starts_with("Destination folder missing"),
        "{problem}"
    );
    assert!(
        !w.path("missing").exists(),
        "no folders made without a parent"
    );
    assert_eq!(
        events(&lines, "summary", LOCAL)[0]["summary"]["outcome"],
        "failed"
    );
    assert_eq!(
        events(&lines, "summary", ALIAS)[0]["summary"]["outcome"],
        "ok"
    );
    assert_eq!(
        fs::read_to_string(w.alias_dir().join("a.txt")).unwrap(),
        "a"
    );
    assert_eq!(
        fs::read_to_string(w.path("remote/Backups/TwoWay/a.txt")).unwrap(),
        "a"
    );
    let done = lines.last().unwrap();
    assert_eq!(
        done["destinations"],
        serde_json::json!([
            {"destination": LOCAL, "outcome": "failed", "reason": "Destination folder missing"},
            {"destination": ALIAS, "outcome": "ok", "reason": null},
            {"destination": TWO_WAY, "outcome": "ok", "reason": null},
        ])
    );
    let state = w.state();
    assert_eq!(
        state["runs"][format!("documents/{LOCAL}")]["outcome"],
        "failed"
    );
    assert_eq!(state["runs"][format!("documents/{ALIAS}")]["outcome"], "ok");
    assert_eq!(
        state["runs"][format!("documents/{TWO_WAY}")]["outcome"],
        "ok"
    );
    assert_eq!(
        state["bisync"][format!("documents/{TWO_WAY}")]["initialised"],
        true
    );

    // The plain text says the same, and the jobs list shows the last runs.
    let out = w.onefolder().args(["jobs", "list"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("✗ failed just now"), "{text}");
    assert!(text.contains("✓ just now"), "{text}");
}

/// `--dest` runs one destination only.
#[test]
fn dest_runs_one_destination() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/a.txt", "a");
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes", "--json", "--dest", ALIAS])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(w.alias_dir().join("a.txt").exists());
    assert!(!w.path("dst/a.txt").exists());
    let lines = ndjson(&out.stdout);
    assert!(events(&lines, "start", LOCAL).is_empty());
}

/// Ctrl-C mid-copy: OneFolder asks rclone to stop, records the run as
/// cancelled, prints the summary, and leaves no partial file behind.
#[test]
fn ctrl_c_mid_run_is_cancelled_and_recorded() {
    if !enabled() {
        return;
    }
    interrupted_mid_run("-INT");
}

/// SIGTERM (a `kill`) and SIGHUP (a closed terminal) are handled the same
/// way: rclone is stopped cleanly and the run is recorded as cancelled.
#[test]
fn sigterm_and_sighup_mid_run_are_cancelled_and_recorded() {
    if !enabled() {
        return;
    }
    interrupted_mid_run("-TERM");
    interrupted_mid_run("-HUP");
}

fn interrupted_mid_run(signal: &str) {
    let w = World::new();
    w.config(&w.local_destination(LOCAL, &w.path("dst")));
    w.write("src/small.txt", "small");
    // A large sparse file, a bandwidth limit and no server-side copy (set
    // through rclone's own environment variables), so the copy is still
    // running when the signal arrives.
    let big = fs::File::create(w.path("src/big.bin")).unwrap();
    big.set_len(64 * 1024 * 1024).unwrap();
    drop(big);

    let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_onefolder"));
    for (k, v) in w.env() {
        cmd.env(k, v);
    }
    let mut child = cmd
        .env("RCLONE_BWLIMIT", "2M")
        .env("RCLONE_DISABLE", "copy")
        .args(["run", "documents", "--yes", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    // Wait until bytes are moving, then press Ctrl-C (SIGINT to OneFolder).
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            panic!("the run ended before it could be interrupted: {lines:#?}");
        }
        let v: Value = serde_json::from_str(&line).unwrap();
        let moving = v["event"] == "stats" && v["bytes"].as_u64().unwrap_or(0) > 0;
        lines.push(v);
        if moving {
            break;
        }
        assert!(Instant::now() < deadline, "no progress: {lines:#?}");
    }
    let status = StdCommand::new("/bin/kill")
        .args([signal, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    for line in reader.lines() {
        lines.push(serde_json::from_str(&line.unwrap()).unwrap());
    }
    let exit = child.wait().unwrap();
    for line in &lines {
        check_shape(line);
    }
    assert_eq!(exit.code(), Some(1), "{lines:#?}");
    assert!(!events(&lines, "stopping", LOCAL).is_empty(), "{lines:#?}");
    let summary = &events(&lines, "summary", LOCAL)[0]["summary"];
    assert_eq!(summary["outcome"], "cancelled");
    assert_eq!(
        lines.last().unwrap()["destinations"][0]["outcome"],
        "cancelled"
    );
    let record = &w.state()["runs"][format!("documents/{LOCAL}")];
    assert_eq!(record["outcome"], "cancelled");
    assert!(
        !w.path("data/runs/documents.lock").exists(),
        "the run lock goes with the run"
    );
    assert!(!w.path("dst/big.bin").exists());
    let leftovers: Vec<_> = fs::read_dir(w.path("dst"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".partial"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert_no_rclone_left(&w);
}

/// No rclone process is still working on this test's folders.
fn assert_no_rclone_left(w: &World) {
    let out = StdCommand::new("/usr/bin/pgrep")
        .args(["-f", &format!("rclone.*{}", w.root.display())])
        .output()
        .unwrap();
    assert!(
        out.stdout.is_empty(),
        "rclone still running: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// More than 20 excludes go to rclone in a file beside the run log, and
/// still apply. A pattern a file would change (a carriage return) keeps
/// every pattern as a flag; spaces at either end of a pattern are trimmed
/// first, so they mean the same as a flag or in a file. The destination
/// listing filters the same way as the copy.
#[test]
fn many_excludes_go_in_a_file() {
    if !enabled() {
        return;
    }
    fn job(w: &World, extra: &[&str]) {
        let mut patterns: Vec<String> = (0..21).map(|i| format!("*.x{i}")).collect();
        patterns.extend(extra.iter().map(|p| p.to_string()));
        let quoted: Vec<String> = patterns
            .iter()
            .map(|p| serde_json::to_string(p).unwrap())
            .collect();
        let text = format!(
            "version = 1\n\n[[jobs]]\nname = \"documents\"\nsource = \"{}\"\nexcludes = [{}]\n{}",
            w.path("src").display(),
            quoted.join(", "),
            w.local_destination(LOCAL, &w.path("dst"))
        );
        fs::write(w.path("config/config.toml"), text).unwrap();
    }
    fn log_names(w: &World) -> Vec<String> {
        fs::read_dir(w.path(&format!("data/logs/documents/{LOCAL}")))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    // Plain patterns: a file.
    let w = World::new();
    job(&w, &[]);
    w.write("src/keep.txt", "k");
    w.write("src/skip.x20", "s");
    w.write("dst/old.x20", "excluded on the destination too");
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(w.path("dst/keep.txt").exists());
    assert!(!w.path("dst/skip.x20").exists());
    let names = log_names(&w);
    assert!(names.iter().any(|n| n.ends_with(".excludes")), "{names:?}");
    let log = names.iter().find(|n| n.ends_with(".jsonl")).unwrap();
    let text = fs::read_to_string(w.path(&format!("data/logs/documents/{LOCAL}/{log}"))).unwrap();
    assert!(text.contains("--exclude-from"), "{text}");
    // The listing used the same filter: the excluded destination file is
    // not counted as left alone.
    let lines = ndjson(&out.stdout);
    assert_eq!(events(&lines, "preview", LOCAL)[0]["destination_only"], 0);

    // Spaces at the ends are trimmed; a carriage return keeps flags.
    let w = World::new();
    job(&w, &[" lead.txt", "trail.txt ", "cr\r.txt"]);
    for name in [
        " lead.txt",
        "trail.txt ",
        "cr\r.txt",
        "lead.txt",
        "trail.txt",
    ] {
        fs::write(w.path("src").join(name), "x").unwrap();
    }
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    // The patterns mean the trimmed names, wherever they come in the
    // list (rclone itself trims only the last flag's trailing spaces).
    for excluded in ["lead.txt", "trail.txt"] {
        assert!(
            !w.path("dst").join(excluded).exists(),
            "{excluded:?} was copied"
        );
    }
    // The names with spaces are other files, and are copied.
    assert!(w.path("dst/ lead.txt").exists());
    assert!(w.path("dst/trail.txt ").exists());
    // Every pattern stayed a flag.
    assert!(!log_names(&w).iter().any(|n| n.ends_with(".excludes")));
    let log = log_names(&w)
        .into_iter()
        .find(|n| n.ends_with(".jsonl"))
        .unwrap();
    let text = fs::read_to_string(w.path(&format!("data/logs/documents/{LOCAL}/{log}"))).unwrap();
    assert!(!text.contains("--exclude-from"));
    assert!(text.contains("--exclude lead.txt "), "{text}");
    assert!(!text.contains("' lead.txt'"), "{text}");
    // rclone's local backend shows a carriage return in a name as the
    // control picture U+240D, so a pattern with a literal one matches no
    // local file, as a flag or in a file alike: the file is copied.
    assert!(w.path("dst/cr\r.txt").exists());
}

/// A file whose content is the same but whose time changed only gets its
/// time set; a file deleted from the source stays on the destination.
#[test]
fn time_only_changes_and_source_deletions() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.config(&w.local_destination(LOCAL, &w.path("dst")));
    w.write("src/touched.txt", "same");
    w.write("src/gone.txt", "bye");
    w.seed(&w.path("src"), &w.path("dst"));
    fs::remove_file(w.path("src/gone.txt")).unwrap();
    let status = StdCommand::new("/usr/bin/touch")
        .args(["-t", "202001010000"])
        .arg(w.path("src/touched.txt"))
        .status()
        .unwrap();
    assert!(status.success());

    let out = w
        .onefolder()
        .args(["run", "documents", "--yes"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("  ✓ touched.txt   time updated"), "{text}");
    assert!(text.contains("Updated: 1"), "{text}");
    let mtime = |p: PathBuf| fs::metadata(p).unwrap().modified().unwrap();
    assert_eq!(
        mtime(w.path("dst/touched.txt")),
        mtime(w.path("src/touched.txt"))
    );
    assert_eq!(fs::read_to_string(w.path("dst/gone.txt")).unwrap(), "bye");
    assert!(
        text.contains("Left alone: 1 file only on the destination"),
        "{text}"
    );

    // Now nothing is left to do: no question, no copy.
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let lines = ndjson(&out.stdout);
    assert_eq!(
        events(&lines, "note", LOCAL)[0]["message"],
        "Nothing to copy."
    );
    assert!(events(&lines, "step", LOCAL)
        .iter()
        .all(|s| s["step"] != "copying"));
    assert_eq!(
        events(&lines, "summary", LOCAL)[0]["summary"]["outcome"],
        "ok"
    );
}

/// A destination folder that does not exist yet is created inside an
/// existing parent; without a parent the run refuses and creates nothing.
#[test]
fn destination_folders_are_created_only_inside_an_existing_parent() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.config(&w.local_destination(LOCAL, &w.path("dst/new-folder")));
    w.write("src/a.txt", "a");
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        fs::read_to_string(w.path("dst/new-folder/a.txt")).unwrap(),
        "a"
    );

    let w = World::new();
    w.config(&w.local_destination(LOCAL, &w.path("no/parent/dst")));
    w.write("src/a.txt", "a");
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("✗ Destination folder missing"), "{text}");
    assert!(!w.path("no").exists());
    assert_eq!(
        w.state()["runs"][format!("documents/{LOCAL}")]["outcome"],
        "failed"
    );
}

/// Without a terminal and without `--yes`, nothing runs.
#[test]
fn no_terminal_without_yes_is_refused() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/a.txt", "a");
    let out = w.onefolder().args(["run", "documents"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--yes"));
    assert!(!w.path("dst/a.txt").exists());
    assert!(!w.path("data/state.json").exists());
}

// ------------------------------------------------------------- two-way

/// The destination type a two-way test runs against.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// A plain local folder.
    Local,
    /// The alias remote: the OneDrive code path.
    Alias,
}

const KINDS: [Kind; 2] = [Kind::Local, Kind::Alias];

impl World {
    /// The job with one two-way destination of this kind: its id and the
    /// folder its files are in.
    fn two_way(&self, kind: Kind) -> (&'static str, PathBuf) {
        match kind {
            Kind::Local => {
                self.config(&format!(
                    "\n  [[jobs.destinations]]\n  id = \"{LOCAL}\"\n  type = \"local\"\n  path = \"{}\"\n  mode = \"two-way\"\n",
                    self.path("dst").display()
                ));
                (LOCAL, self.path("dst"))
            }
            Kind::Alias => {
                self.config(&self.alias_destination(ALIAS, "Backups/Documents", "two-way"));
                (ALIAS, self.alias_dir())
            }
        }
    }

    /// `onefolder run documents --json` with more arguments: the exit code
    /// and the NDJSON lines, each checked against the documented shapes.
    fn run_json(&self, args: &[&str]) -> (Option<i32>, Vec<Value>) {
        let mut all = vec!["run", "documents", "--json"];
        all.extend(args);
        let out = self.onefolder().args(&all).output().unwrap();
        let lines = ndjson(&out.stdout);
        for l in &lines {
            check_shape(l);
        }
        (out.status.code(), lines)
    }

    /// Runs with `--yes` and expects it to finish ok.
    fn sync_ok(&self, id: &str, args: &[&str]) -> Vec<Value> {
        let mut all = vec!["--yes"];
        all.extend(args);
        let (code, lines) = self.run_json(&all);
        assert_eq!(code, Some(0), "{lines:#?}");
        assert_eq!(summary_of(&lines, id)["outcome"], "ok", "{lines:#?}");
        lines
    }

    fn workdir(&self, id: &str) -> PathBuf {
        self.path("data/bisync/documents").join(id)
    }

    fn remove(&self, rel: &str) {
        let p = self.path(rel);
        if p.is_dir() {
            fs::remove_dir_all(p).unwrap();
        } else {
            fs::remove_file(p).unwrap();
        }
    }
}

fn summary_of<'a>(lines: &'a [Value], id: &str) -> &'a Value {
    &events(lines, "summary", id)
        .first()
        .unwrap_or_else(|| panic!("no summary for {id}: {lines:#?}"))["summary"]
}

/// Every file under a folder and its contents.
fn contents(dir: &Path) -> BTreeMap<String, String> {
    tree(dir).into_iter().map(|(k, (v, _))| (k, v)).collect()
}

fn put(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

/// Enough files that the changes a test makes stay under bisync's 50%
/// deletion limit.
fn filler(w: &World) {
    for i in 0..10 {
        w.write(&format!("src/filler/f{i}.txt"), &format!("filler {i}"));
    }
}

/// Lets modification times move on, so a change is seen as one.
fn tick() {
    std::thread::sleep(Duration::from_millis(1100));
}

/// The first run merges, and needs an answer: no terminal without `--yes`
/// is refused; `--dry-run` says what the merge would do and changes
/// nothing; with `--yes` it merges, keeping the destination's differing
/// copy under a conflict name on the destination only, and records the
/// sync state.
#[test]
fn two_way_first_run_merges() {
    if !enabled() {
        return;
    }
    for kind in KINDS {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        w.write("src/a.txt", "a");
        w.write("src/Notes/plan.md", "local plan");
        put(&dst, "only-there.txt", "only on the destination");
        put(&dst, "Notes/plan.md", "the destination's plan, longer");
        let before = (contents(&w.path("src")), contents(&dst));

        let out = w.onefolder().args(["run", "documents"]).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{kind:?}");
        assert_eq!((contents(&w.path("src")), contents(&dst)), before);

        let (code, lines) = w.run_json(&["--dry-run"]);
        assert_eq!(code, Some(0), "{lines:#?}");
        let offer = events(&lines, "resync", id)[0];
        assert_eq!(offer["reason"], "first-run");
        assert_eq!(offer["to_local"]["files"], 1);
        assert_eq!(offer["kept"][0]["path"], "Notes/plan.md");
        let kept_as = offer["kept"][0]["kept_as"].as_str().unwrap().to_string();
        assert!(kept_as.starts_with("Notes/plan.conflict-") && kept_as.ends_with("-1.md"));
        assert_eq!(summary_of(&lines, id)["dry_run"], true);
        assert_eq!((contents(&w.path("src")), contents(&dst)), before);

        let lines = w.sync_ok(id, &[]);
        assert!(!events(&lines, "resync", id).is_empty());
        let src = contents(&w.path("src"));
        let remote = contents(&dst);
        assert_eq!(src["Notes/plan.md"], "local plan", "{kind:?}");
        assert_eq!(remote["Notes/plan.md"], "local plan");
        assert_eq!(src["only-there.txt"], "only on the destination");
        assert_eq!(remote["a.txt"], "a");
        assert_eq!(remote[&kept_as], "the destination's plan, longer");
        assert!(
            !src.contains_key(&kept_as),
            "the losing copy stays on the destination"
        );
        let state = w.state();
        let record = &state["bisync"][format!("documents/{id}")];
        assert_eq!(record["initialised"], true);
        assert!(record["last_resync"].is_string());
        assert!(crate_has_listings(&w.workdir(id)));
    }
}

fn crate_has_listings(workdir: &Path) -> bool {
    let names: Vec<String> = fs::read_dir(workdir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.iter().any(|n| n.ends_with(".path1.lst"))
        && names.iter().any(|n| n.ends_with(".path2.lst"))
}

/// The second run propagates adds, updates and file deletions both ways;
/// a deletion on the destination reaches this Mac without any prompt.
#[test]
fn two_way_second_run_propagates_both_ways() {
    if !enabled() {
        return;
    }
    for kind in KINDS {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        filler(&w);
        for f in [
            "edit.txt",
            "gone-here.txt",
            "gone-there.txt",
            "edit-there.txt",
        ] {
            w.write(&format!("src/{f}"), "v1");
        }
        w.sync_ok(id, &[]);
        tick();
        w.write("src/new-here.txt", "new here");
        w.write("src/edit.txt", "v2, edited here");
        w.remove("src/gone-here.txt");
        put(&dst, "new-there.txt", "new there");
        put(&dst, "edit-there.txt", "v2, edited there");
        fs::remove_file(dst.join("gone-there.txt")).unwrap();

        let lines = w.sync_ok(id, &[]);
        assert!(events(&lines, "folder_prompt", id).is_empty());
        let plan = events(&lines, "plan", id)[0];
        assert_eq!(
            plan["delete_destination"],
            serde_json::json!(["gone-here.txt"])
        );
        assert_eq!(plan["delete_local"], serde_json::json!(["gone-there.txt"]));
        let s = summary_of(&lines, id);
        assert_eq!(
            s["added"],
            serde_json::json!({"local": 1, "remote": 1}),
            "{kind:?} {s}"
        );
        assert_eq!(s["updated"], serde_json::json!({"local": 1, "remote": 1}));
        assert_eq!(s["deleted"], serde_json::json!({"local": 1, "remote": 1}));
        let src = contents(&w.path("src"));
        assert_eq!(src, contents(&dst), "{kind:?}");
        assert_eq!(src["edit.txt"], "v2, edited here");
        assert_eq!(src["edit-there.txt"], "v2, edited there");
        assert!(src.contains_key("new-there.txt") && !src.contains_key("gone-there.txt"));
        let files: Vec<&Value> = events(&lines, "file", id);
        assert!(files.iter().any(|f| f["path"] == "gone-there.txt"
            && f["change"] == "deleted"
            && f["side"] == "local"));
        assert_eq!(
            w.state()["runs"][format!("documents/{id}")]["mode"],
            "two-way"
        );
    }
}

/// A file changed on both sides: the local copy wins on both sides; the
/// destination's copy is kept there, renamed with the conflict suffix,
/// and never comes to this Mac, on this run or the next.
#[test]
fn two_way_conflict_keeps_both_copies_local_wins() {
    if !enabled() {
        return;
    }
    for kind in KINDS {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        filler(&w);
        w.write("src/Notes/plan.md", "v1");
        w.sync_ok(id, &[]);
        tick();
        w.write("src/Notes/plan.md", "v2 from this Mac");
        put(&dst, "Notes/plan.md", "v2 from the destination, longer");

        let lines = w.sync_ok(id, &[]);
        let conflict = events(&lines, "conflict", id)[0];
        assert_eq!(conflict["path"], "Notes/plan.md");
        let kept_as = conflict["kept_as"].as_str().unwrap().to_string();
        assert!(kept_as.starts_with("Notes/plan.conflict-"), "{kept_as}");
        let s = summary_of(&lines, id);
        assert_eq!(s["conflicts"].as_array().unwrap().len(), 1);
        assert!(s["conflicts"][0].as_str().unwrap().contains(&kept_as));
        let check = |w: &World| {
            let src = contents(&w.path("src"));
            let remote = contents(&dst);
            assert_eq!(src["Notes/plan.md"], "v2 from this Mac", "{kind:?}");
            assert_eq!(remote["Notes/plan.md"], "v2 from this Mac");
            assert_eq!(remote[&kept_as], "v2 from the destination, longer");
            assert!(!src.contains_key(&kept_as), "{kind:?}: {src:?}");
        };
        check(&w);
        // The next run leaves it so.
        let lines = w.sync_ok(id, &[]);
        assert!(events(&lines, "conflict", id).is_empty());
        check(&w);
    }
}

/// The folder prompt's three answers (by flag), and the default without a
/// terminal: keep. Keep holds the folder: the next run leaves it alone,
/// and `jobs release` brings it back to this Mac on the run after.
#[test]
fn folder_prompt_answers() {
    if !enabled() {
        return;
    }
    let setup = |kind: Kind| {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        filler(&w);
        w.write("src/Old projects/2019/a.txt", "a");
        w.write("src/Old projects/2019/drafts/b.txt", "b");
        w.write("src/Old projects/keep.txt", "keep");
        w.sync_ok(id, &[]);
        w.remove("src/Old projects/2019");
        (w, id, dst)
    };
    let folder = "Old projects/2019";

    // Delete.
    let (w, id, dst) = setup(Kind::Local);
    let lines = w.sync_ok(id, &["--folder-deletes", "delete"]);
    let prompt = events(&lines, "folder_prompt", id)[0];
    assert_eq!(prompt["path"], folder);
    assert_eq!(
        (prompt["answer"].clone(), prompt["by"].clone()),
        ("delete".into(), "flag".into())
    );
    assert_eq!(prompt["files"], 2);
    assert!(!dst.join(folder).exists());
    assert!(!w.path("src").join(folder).exists());
    assert!(dst.join("Old projects/keep.txt").exists());

    // Restore.
    let (w, id, dst) = setup(Kind::Alias);
    let lines = w.sync_ok(id, &["--folder-deletes", "restore"]);
    assert_eq!(events(&lines, "folder_prompt", id)[0]["answer"], "restore");
    assert_eq!(
        fs::read_to_string(w.path("src").join(folder).join("a.txt")).unwrap(),
        "a"
    );
    assert_eq!(
        fs::read_to_string(w.path("src").join(folder).join("drafts/b.txt")).unwrap(),
        "b"
    );
    assert!(dst.join(folder).join("a.txt").exists());
    assert!(w.state()["holds"].get(format!("documents/{id}")).is_none());

    // Keep, by flag and by default; then the hold; then release.
    for (kind, args, by) in [
        (Kind::Local, vec!["--folder-deletes", "keep"], "flag"),
        (Kind::Alias, vec![], "default"),
    ] {
        let (w, id, dst) = setup(kind);
        let lines = w.sync_ok(id, &args);
        let prompt = events(&lines, "folder_prompt", id)[0];
        assert_eq!(
            (prompt["answer"].clone(), prompt["by"].clone()),
            ("keep".into(), by.into())
        );
        assert!(dst.join(folder).join("a.txt").exists(), "{kind:?}");
        assert!(!w.path("src").join(folder).exists());
        assert_eq!(
            w.state()["holds"][format!("documents/{id}")],
            serde_json::json!([folder])
        );
        // Held: the next run does not ask and does not touch it.
        let lines = w.sync_ok(id, &["--folder-deletes", "delete"]);
        assert!(events(&lines, "folder_prompt", id).is_empty());
        assert_eq!(
            events(&lines, "plan", id)[0]["holds"],
            serde_json::json!([folder])
        );
        assert!(dst.join(folder).join("drafts/b.txt").exists());
        // Released: it comes back here on the next run.
        let out = w
            .onefolder()
            .args(["jobs", "release", "documents", folder])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0));
        let this_computer = if cfg!(target_os = "macos") {
            "this Mac"
        } else {
            "this computer"
        };
        assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("back to {this_computer}")));
        assert!(w.state()["holds"].get(format!("documents/{id}")).is_none());
        w.sync_ok(id, &[]);
        assert_eq!(
            fs::read_to_string(w.path("src").join(folder).join("drafts/b.txt")).unwrap(),
            "b"
        );
    }
}

/// Deleting more than half the files: without a terminal the destination
/// is not run (nothing deleted); on a terminal the confirmation shows the
/// counts, and after "y" the real run lifts the limit and succeeds.
#[test]
fn deleting_more_than_half_needs_confirmation() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let (id, dst) = w.two_way(Kind::Local);
    for i in 0..10 {
        w.write(&format!("src/f{i}.txt"), "x");
    }
    w.sync_ok(id, &[]);
    for i in 0..6 {
        w.remove(&format!("src/f{i}.txt"));
    }
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(1), "{lines:#?}");
    let plan = events(&lines, "plan", id)[0];
    assert_eq!(plan["needs_confirmation"][0]["count"], 6);
    assert_eq!(plan["needs_confirmation"][0]["total"], 10);
    let reason = events(&lines, "skipped", id)[0]["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        reason.contains("6 of 10") && reason.contains("terminal"),
        "{reason}"
    );
    assert_eq!(contents(&dst).len(), 10, "nothing deleted");

    // On a terminal (a pseudo-terminal from script(1), 40 by 120).
    let mut cmd = StdCommand::new("/usr/bin/script");
    for (k, v) in w.env() {
        cmd.env(k, v);
    }
    let shell = format!(
        "stty rows 40 cols 120; exec {} run documents",
        env!("CARGO_BIN_EXE_onefolder")
    );
    // Killed when the test fails, so no run is left behind.
    let mut child = KillOnDrop(
        cmd.env("TERM", "xterm-256color")
            .args(script_args(&shell))
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut stdout = child.stdout.take().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                seen.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        })
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let text = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
        if text.contains("Go ahead, with those deletions?") {
            assert!(text.contains("6 of 10"), "the counts are shown");
            break;
        }
        assert!(Instant::now() < deadline, "no confirmation: {text}");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"y").unwrap();
        stdin.flush().unwrap();
        // Kept open until the run is done, as a terminal would be.
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            assert!(Instant::now() < deadline, "the run did not finish");
            std::thread::sleep(Duration::from_millis(100));
        };
        drop(stdin);
        assert!(
            status.success(),
            "{}",
            String::from_utf8_lossy(&seen.lock().unwrap())
        );
    }
    let _ = reader.join();
    assert_eq!(contents(&dst).len(), 4);
    assert_eq!(
        w.state()["runs"][format!("documents/{id}")]["outcome"],
        "ok"
    );
}

/// A lock whose process is gone is removed (with `--yes`) and the run goes
/// on; a lock held by a running process stops the destination.
#[test]
fn stale_lock_is_removed_and_a_live_one_refused() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let (id, _) = w.two_way(Kind::Alias);
    filler(&w);
    w.sync_ok(id, &[]);
    let mut gone = StdCommand::new("/usr/bin/true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    let lock = w.workdir(id).join("a..b.lck");
    fs::write(&lock, format!(r#"{{"Session":"x","PID":"{pid}"}}"#)).unwrap();
    let lines = w.sync_ok(id, &[]);
    let notes: Vec<&str> = events(&lines, "note", id)
        .iter()
        .filter_map(|n| n["message"].as_str())
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains(&format!("stale lock left by process {pid}"))),
        "{notes:?}"
    );
    assert!(!lock.exists());

    // A process id that is running but is not rclone (this test): the id
    // was reused, so the lock is stale.
    fs::write(&lock, format!(r#"{{"PID":"{}"}}"#, std::process::id())).unwrap();
    w.sync_ok(id, &[]);
    assert!(!lock.exists());

    // A running rclone holds it.
    let rcd = KillOnDrop(
        StdCommand::new(w.rclone_dir.join("rclone"))
            .args(["rcd", "--rc-addr", "127.0.0.1:0", "--rc-no-auth"])
            .env("RCLONE_CONFIG", w.path("rclone.conf"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(300));
    fs::write(&lock, format!(r#"{{"PID":"{}"}}"#, rcd.id())).unwrap();
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(2));
    let problem = events(&lines, "problem", id)[0]["message"]
        .as_str()
        .unwrap();
    assert!(problem.starts_with("Another OneFolder or rclone run is syncing this destination"));
    assert!(lock.exists(), "a live lock is left alone");
    assert!(problem.contains("a..b.lck"), "the lock is named: {problem}");
    drop(rcd);
}

/// A child process killed (and waited for) when dropped, so a failing test
/// leaves nothing running.
struct KillOnDrop(std::process::Child);

impl std::ops::Deref for KillOnDrop {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A second conflict on the same file on the same day, next to a local
/// file that already has the first conflict name: every copy survives, each
/// under its own name, and the local file is never touched.
#[test]
fn second_same_day_conflict_loses_nothing() {
    if !enabled() {
        return;
    }
    for kind in KINDS {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        filler(&w);
        w.write("src/N/plan.md", "v1");
        w.sync_ok(id, &[]);
        let date = jiff::Zoned::now().strftime("%Y-%m-%d").to_string();
        let name = |n: u32| format!("N/plan.conflict-{date}-{n}.md");
        // The user's own file, with the name a first conflict copy would get.
        w.write(&format!("src/{}", name(1)), "my own file");

        tick();
        w.write("src/N/plan.md", "v2 here");
        put(&dst, "N/plan.md", "v2 there, longer");
        let lines = w.sync_ok(id, &[]);
        assert_eq!(
            events(&lines, "conflict", id)[0]["kept_as"],
            name(2),
            "{kind:?}"
        );

        tick();
        // Sizes differ: within the 2 s modify window an edit of the same
        // size is not a change to rclone.
        w.write("src/N/plan.md", "v3 here, again");
        put(&dst, "N/plan.md", "v3 there, longer still");
        let lines = w.sync_ok(id, &[]);
        assert_eq!(
            events(&lines, "conflict", id)[0]["kept_as"],
            name(3),
            "{kind:?}"
        );
        assert_eq!(
            summary_of(&lines, id)["updated"]["remote"],
            1,
            "the winning upload counts as an update"
        );

        let src = contents(&w.path("src"));
        let remote = contents(&dst);
        assert_eq!(src["N/plan.md"], "v3 here, again");
        assert_eq!(src[&name(1)], "my own file", "{kind:?}: {src:?}");
        assert!(!src.contains_key(&name(2)) && !src.contains_key(&name(3)));
        assert_eq!(remote["N/plan.md"], "v3 here, again");
        assert_eq!(remote[&name(2)], "v2 there, longer");
        assert_eq!(remote[&name(3)], "v3 there, longer still");
        // A quiet run afterwards changes nothing on either side.
        w.sync_ok(id, &[]);
        assert_eq!(contents(&w.path("src")), src);
        assert_eq!(contents(&dst), remote);
    }
}

/// A folder deleted on the destination goes from this Mac too, with no
/// prompt (the prompt is only for folders deleted locally).
#[test]
fn destination_side_folder_deletion_propagates() {
    if !enabled() {
        return;
    }
    for kind in KINDS {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        filler(&w);
        w.write("src/Old/a.txt", "a");
        w.write("src/Old/sub/b.txt", "b");
        w.sync_ok(id, &[]);
        fs::remove_dir_all(dst.join("Old")).unwrap();
        let lines = w.sync_ok(id, &["--folder-deletes", "keep"]);
        assert!(events(&lines, "folder_prompt", id).is_empty(), "{kind:?}");
        let plan = events(&lines, "plan", id)[0];
        assert_eq!(
            plan["delete_local"],
            serde_json::json!(["Old/a.txt", "Old/sub/b.txt"])
        );
        assert!(!w.path("src/Old/a.txt").exists() && !w.path("src/Old/sub/b.txt").exists());
        assert_eq!(contents(&w.path("src")), contents(&dst));
    }
}

/// A working directory that lost its listings (and their backups) gives
/// the merge again, with the reason, and then runs normally.
#[test]
fn damaged_working_directory_offers_a_resync() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let (id, dst) = w.two_way(Kind::Local);
    filler(&w);
    w.sync_ok(id, &[]);
    for entry in fs::read_dir(w.workdir(id)).unwrap() {
        let p = entry.unwrap().path();
        if p.to_string_lossy().contains(".lst") {
            fs::remove_file(p).unwrap();
        }
    }
    w.write("src/after.txt", "after");
    let lines = w.sync_ok(id, &[]);
    let offer = events(&lines, "resync", id)[0];
    assert_eq!(offer["reason"], "damaged");
    assert!(offer["detail"].as_str().unwrap().contains("missing"));
    assert!(dst.join("after.txt").exists());
    let lines = w.sync_ok(id, &[]);
    assert!(events(&lines, "resync", id).is_empty());
    assert!(!events(&lines, "plan", id).is_empty());
}

/// Ctrl-C mid two-way run: bisync wraps up, the run is recorded as
/// cancelled, no lock is left, and the next run says it is recovering
/// and finishes the job.
#[test]
fn ctrl_c_mid_two_way_run_then_recovery() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let (id, dst) = w.two_way(Kind::Local);
    filler(&w);
    w.sync_ok(id, &[]);
    let big = fs::File::create(w.path("src/big.bin")).unwrap();
    big.set_len(48 * 1024 * 1024).unwrap();
    drop(big);

    let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_onefolder"));
    for (k, v) in w.env() {
        cmd.env(k, v);
    }
    let mut child = cmd
        .env("RCLONE_BWLIMIT", "1M")
        .env("RCLONE_DISABLE", "copy")
        .args(["run", "documents", "--yes", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut syncing = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            panic!("the run ended before it could be interrupted: {lines:#?}");
        }
        let v: Value = serde_json::from_str(&line).unwrap();
        syncing |= v["event"] == "step" && v["step"] == "syncing";
        let moving = syncing && v["event"] == "stats" && v["bytes"].as_u64().unwrap_or(0) > 0;
        lines.push(v);
        if moving {
            break;
        }
        assert!(Instant::now() < deadline, "no progress: {lines:#?}");
    }
    let status = StdCommand::new("/bin/kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    for line in reader.lines() {
        lines.push(serde_json::from_str(&line.unwrap()).unwrap());
    }
    let exit = child.wait().unwrap();
    for line in &lines {
        check_shape(line);
    }
    assert_eq!(exit.code(), Some(1), "{lines:#?}");
    assert!(!events(&lines, "stopping", id).is_empty());
    assert_eq!(summary_of(&lines, id)["outcome"], "cancelled");
    assert_eq!(
        w.state()["runs"][format!("documents/{id}")]["outcome"],
        "cancelled"
    );
    let lock_left = fs::read_dir(w.workdir(id))
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".lck"));
    assert!(!lock_left, "bisync's lock is gone");
    assert_no_rclone_left(&w);

    let lines = w.sync_ok(id, &[]);
    let notes: Vec<&str> = events(&lines, "note", id)
        .iter()
        .filter_map(|n| n["message"].as_str())
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("Recovering from the interrupted run on ")),
        "{notes:?}"
    );
    assert_eq!(
        fs::metadata(dst.join("big.bin")).unwrap().len(),
        48 * 1024 * 1024
    );
}

// ------------------------------------------------------- milestone 6

/// A job with a local and an alias destination runs both, in config order,
/// writes a run record for each, and ends with one closing summary line
/// per destination.
#[test]
fn both_destinations_run_in_order_with_a_record_each() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/a.txt", "a");
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    let starts: Vec<(String, u64, u64)> = lines
        .iter()
        .filter(|l| l["event"] == "start")
        .map(|l| {
            (
                l["destination"].as_str().unwrap().to_string(),
                l["index"].as_u64().unwrap(),
                l["of"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        starts,
        [(LOCAL.to_string(), 1, 2), (ALIAS.to_string(), 2, 2)]
    );
    // The first destination's summary comes before the second starts.
    let position = |kind: &str, id: &str| {
        lines
            .iter()
            .position(|l| l["event"] == kind && l["destination"] == id)
            .unwrap()
    };
    assert!(position("summary", LOCAL) < position("start", ALIAS));
    let state = w.state();
    for id in [LOCAL, ALIAS] {
        let record = &state["runs"][format!("documents/{id}")];
        assert_eq!(record["outcome"], "ok", "{state:#}");
        assert_eq!(record["added"]["remote"], 1);
        assert!(record["log"].as_str().unwrap().contains(id));
    }
    assert!(w.path("dst/a.txt").exists() && w.alias_dir().join("a.txt").exists());

    // Plain text: a summary block each, then the closing summary.
    w.write("src/b.txt", "b");
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{text}");
    let closing = text
        .split("documents   2 destinations · 2 ok\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no closing summary:\n{text}"));
    let dst = w.path("dst").display().to_string();
    let rows: Vec<&str> = closing.lines().take(2).collect();
    assert!(rows[0].starts_with(&format!("  ✓ {dst}")), "{text}");
    assert!(rows[0].ends_with("one-way   ok"), "{text}");
    assert!(rows[1].starts_with("  ✓ test:Backups/Documents"), "{text}");
}

/// A change that arrives from two-way destination A reaches two-way
/// destination B in the same run when B comes later in the list.
#[test]
fn a_change_from_one_destination_reaches_a_later_one_in_the_same_run() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.config(&format!(
        "\n  [[jobs.destinations]]\n  id = \"{LOCAL}\"\n  type = \"local\"\n  path = \"{}\"\n  mode = \"two-way\"\n{}",
        w.path("dst").display(),
        w.alias_destination(ALIAS, "Backups/Documents", "two-way")
    ));
    filler(&w);
    // The first run merges both destinations.
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    assert_eq!(contents(&w.path("dst")), contents(&w.alias_dir()));
    tick();
    put(&w.path("dst"), "from-a.txt", "made on destination A");
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    assert_eq!(
        fs::read_to_string(w.path("src/from-a.txt")).unwrap(),
        "made on destination A"
    );
    assert_eq!(
        fs::read_to_string(w.alias_dir().join("from-a.txt")).unwrap(),
        "made on destination A",
        "B runs after A, so it gets A's change in the same run"
    );
    assert_eq!(summary_of(&lines, LOCAL)["added"]["local"], 1);
    assert_eq!(summary_of(&lines, ALIAS)["added"]["remote"], 1);
}

/// A local destination whose recorded drive identity does not match the
/// temporary folder (an unplugged drive: the mount point is a plain folder
/// on the startup disk) refuses with the design's message and is recorded
/// as failed with the reason; the other destination still runs. The
/// `done` line and the exit code reflect both, and `status` and `doctor`
/// say the same.
#[test]
fn a_drive_that_does_not_match_refuses_while_the_other_destination_runs() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let drive = w.path("Backup");
    fs::create_dir_all(drive.join("Documents")).unwrap();
    w.config(&format!(
        "\n  [[jobs.destinations]]\n  id = \"{LOCAL}\"\n  type = \"local\"\n  path = \"{}\"\n  \
         volume_uuid = \"A1B2C3D4-0000-4E5F-8A9B-000000000001\"\n  volume_name = \"Backup\"\n  \
         mount_point = \"{}\"\n{}",
        drive.join("Documents").display(),
        drive.display(),
        w.alias_destination(ALIAS, "Backups/Documents", "one-way")
    ));
    w.write("src/a.txt", "a");
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(1), "{lines:#?}");
    let problem = events(&lines, "problem", LOCAL)[0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        problem.starts_with("Backup drive not mounted\n  "),
        "{problem}"
    );
    let flat = problem.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains(&format!(
            "Destination {} expects volume \"Backup\" (UUID A1B2C3D4-...). {} is currently a folder on",
            drive.join("Documents").display(),
            drive.display()
        )),
        "{flat}"
    );
    assert!(flat.ends_with("Plug the drive in and run again."), "{flat}");
    assert!(
        !drive.join("Documents/a.txt").exists(),
        "nothing written to the impostor folder"
    );
    assert_eq!(summary_of(&lines, LOCAL)["outcome"], "failed");
    assert_eq!(summary_of(&lines, ALIAS)["outcome"], "ok");
    assert!(w.alias_dir().join("a.txt").exists());
    let done = lines.last().unwrap();
    assert_eq!(done["event"], "done");
    assert_eq!(done["exit_code"], 1);
    assert_eq!(done["ok"], false);
    assert_eq!(
        done["destinations"],
        serde_json::json!([
            {"destination": LOCAL, "outcome": "failed", "reason": "Backup drive not mounted"},
            {"destination": ALIAS, "outcome": "ok", "reason": null},
        ])
    );
    let record = &w.state()["runs"][format!("documents/{LOCAL}")];
    assert_eq!(record["outcome"], "failed");
    assert_eq!(record["reason"], "Backup drive not mounted");

    // status and doctor report the drive the same way.
    let out = w.onefolder().arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("Drive      ◎ not mounted (expects volume \"Backup\")"),
        "{text}"
    );
    assert!(
        text.contains("Reason     Backup drive not mounted"),
        "{text}"
    );
    let out = w.onefolder().arg("doctor").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("not mounted (expects volume \"Backup\")"),
        "{text}"
    );

    // Every destination failing is exit 2.
    w.config(&format!(
        "\n  [[jobs.destinations]]\n  id = \"{LOCAL}\"\n  type = \"local\"\n  path = \"{}\"\n  \
         volume_uuid = \"A1B2C3D4-0000-4E5F-8A9B-000000000001\"\n  volume_name = \"Backup\"\n  \
         mount_point = \"{}\"\n",
        drive.join("Documents").display(),
        drive.display()
    ));
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(2), "{lines:#?}");
    assert_eq!(lines.last().unwrap()["exit_code"], 2);
}

/// The run lock: a second run of the same job while one is going is
/// refused with exit 2, a stale lock is removed with a note, and no lock
/// is left behind.
#[test]
fn the_run_lock_refuses_a_concurrent_run() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.config(&w.local_destination(LOCAL, &w.path("dst")));
    let big = fs::File::create(w.path("src/big.bin")).unwrap();
    big.set_len(64 * 1024 * 1024).unwrap();
    drop(big);
    let lock = w.path("data/runs/documents.lock");

    let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_onefolder"));
    for (k, v) in w.env() {
        cmd.env(k, v);
    }
    let mut child = cmd
        .env("RCLONE_BWLIMIT", "2M")
        .env("RCLONE_DISABLE", "copy")
        .args(["run", "documents", "--yes", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "the run ended early"
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["event"] == "stats" && v["bytes"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "no progress");
    }
    let held: Value = serde_json::from_str(&fs::read_to_string(&lock).unwrap()).unwrap();
    assert_eq!(held["pid"], child.id());

    let out = w
        .onefolder()
        .args(["run", "documents", "--yes", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let lines = ndjson(&out.stdout);
    assert_eq!(lines[0]["event"], "refused");
    assert_eq!(lines[0]["code"], "locked");
    let message = lines[0]["message"].as_str().unwrap();
    assert!(
        message.starts_with("another OneFolder run of documents is in progress"),
        "{message}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another OneFolder run of documents is in progress"),
        "{stderr}"
    );
    // A dry run waits its turn too; the lock is untouched.
    let out = w
        .onefolder()
        .args(["run", "documents", "--dry-run"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(lock.exists());

    StdCommand::new("/bin/kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    for _ in reader.lines() {}
    child.wait().unwrap();
    assert!(!lock.exists(), "the lock goes with the run");
    assert_no_rclone_left(&w);

    // A stale lock (a process that is gone) is removed with a note.
    fs::write(
        &lock,
        r#"{"pid":99999999,"started":"2026-10-13T00:00:00Z"}"#,
    )
    .unwrap();
    fs::remove_file(w.path("src/big.bin")).unwrap();
    w.write("src/a.txt", "a");
    let out = w
        .onefolder()
        .args(["run", "documents", "--yes"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("Removed a stale run lock left by process 99999999"),
        "{stderr}"
    );
    assert!(!lock.exists());
}

/// `status --json` after a mixed history validates against the documented
/// shape (src/jsonout/status.rs), and a dry run never hides the last real
/// run.
#[test]
fn status_json_after_a_mixed_history() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.config(
        &(w.local_destination(LOCAL, &w.path("missing/parent/dst"))
            + &w.alias_destination(ALIAS, "Backups/Documents", "one-way")
            + &w.alias_destination(TWO_WAY, "Backups/TwoWay", "two-way")),
    );
    w.write("src/a.txt", "a");
    let (code, _) = w.run_json(&["--yes", "--dest", LOCAL]);
    assert_eq!(code, Some(2));
    let (code, _) = w.run_json(&["--yes", "--dest", ALIAS]);
    assert_eq!(code, Some(0));
    tick();
    w.write("src/b.txt", "b");
    let (code, _) = w.run_json(&["--dry-run", "--dest", ALIAS]);
    assert_eq!(code, Some(0));
    {
        let state = w.state();
        assert_eq!(
            state["runs"][format!("documents/{ALIAS}")]["dry_run"],
            false
        );
        assert_eq!(
            state["last_dry_run"][format!("documents/{ALIAS}")]["dry_run"],
            true
        );
    }

    let out = w.onefolder().args(["status", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["command"], "status");
    assert_eq!(v["ok"], true);
    assert!(v["error"].is_null());
    let jobs = v["data"]["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    for k in ["name", "source", "source_volume", "destinations"] {
        assert!(job.get(k).is_some(), "job lacks {k}: {job}");
    }
    let dests = job["destinations"].as_array().unwrap();
    assert_eq!(dests.len(), 3);
    for d in dests {
        for k in [
            "id",
            "type",
            "display",
            "path",
            "mode",
            "account",
            "remote",
            "remote_exists",
            "volume",
            "last_run",
            "last_dry_run",
            "dry_run_newer",
            "log",
            "holds",
            "pending",
        ] {
            assert!(d.get(k).is_some(), "destination lacks {k}: {d}");
        }
        assert!(d["holds"].is_array() && d["dry_run_newer"].is_boolean());
        if let Some(r) = d["last_run"].as_object() {
            for k in [
                "started",
                "finished",
                "mode",
                "dry_run",
                "outcome",
                "added",
                "updated",
                "deleted",
                "bytes",
                "errors",
                "log",
                "rclone_version",
            ] {
                assert!(r.contains_key(k), "run record lacks {k}: {d}");
            }
        }
    }
    let (local, alias, two) = (&dests[0], &dests[1], &dests[2]);
    assert_eq!(local["id"], LOCAL);
    assert_eq!(local["last_run"]["outcome"], "failed");
    assert_eq!(local["last_run"]["reason"], "Destination folder missing");
    assert_eq!(local["volume"]["state"], "folder-missing");
    assert_eq!(local["volume"]["severity"], "warning");
    assert!(local["remote_exists"].is_null());
    assert_eq!(alias["last_run"]["outcome"], "ok");
    assert_eq!(alias["last_run"]["dry_run"], false);
    assert_eq!(alias["last_dry_run"]["dry_run"], true);
    assert_eq!(alias["dry_run_newer"], true);
    assert_eq!(alias["account"], "test");
    assert_eq!(alias["remote"], "onefolder-test");
    assert_eq!(alias["remote_exists"], true);
    assert!(alias["volume"].is_null());
    assert!(Path::new(alias["log"].as_str().unwrap()).is_file());
    assert!(two["last_run"].is_null());
    assert_eq!(two["pending"]["state"], "first-run");

    // The text says the same; the jobs list mentions the newer dry run.
    let out = w.onefolder().arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Dry run    ✓ ok just now"), "{text}");
    assert!(text.contains("never run"), "{text}");
    let out = w.onefolder().args(["jobs", "list"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("· dry run just now"), "{text}");
}

// ------------------------------------------------- real disk images

/// Disk images attached by a test; dropping it detaches whatever is still
/// attached, whatever the test did.
struct Images {
    dir: PathBuf,
    attached: Vec<String>,
}

impl Images {
    fn new(dir: &Path) -> Images {
        Images {
            dir: dir.to_path_buf(),
            attached: Vec::new(),
        }
    }

    /// Creates a 20 MB APFS image with this volume name.
    fn create(&self, name: &str) -> PathBuf {
        let image = self.dir.join(format!("{name}.dmg"));
        let status = StdCommand::new("/usr/bin/hdiutil")
            .args([
                "create", "-quiet", "-size", "20m", "-fs", "APFS", "-volname", name,
            ])
            .arg(&image)
            .status()
            .unwrap();
        assert!(status.success(), "hdiutil create failed");
        image
    }

    /// Attaches an image, at `mount_point` if given, and returns its device.
    fn attach(&mut self, image: &Path, mount_point: Option<&Path>, read_only: bool) -> String {
        let mut cmd = StdCommand::new("/usr/bin/hdiutil");
        cmd.arg("attach").arg(image).arg("-nobrowse");
        if let Some(m) = mount_point {
            fs::create_dir_all(m).unwrap();
            cmd.arg("-mountpoint").arg(m);
        }
        if read_only {
            cmd.arg("-readonly");
        }
        let out = cmd.stderr(Stdio::null()).output().unwrap();
        assert!(out.status.success(), "hdiutil attach failed");
        let text = String::from_utf8_lossy(&out.stdout);
        let device = text
            .split_whitespace()
            .find(|w| w.starts_with("/dev/disk"))
            .expect("hdiutil names the device")
            .to_string();
        self.attached.push(device.clone());
        device
    }

    fn detach(&mut self, device: &str) {
        let status = StdCommand::new("/usr/bin/hdiutil")
            .args(["detach", "-quiet", device])
            .status()
            .unwrap();
        if !status.success() {
            let _ = StdCommand::new("/usr/bin/hdiutil")
                .args(["detach", "-quiet", "-force", device])
                .status();
        }
        self.attached.retain(|d| d != device);
    }
}

impl Drop for Images {
    fn drop(&mut self) {
        for device in std::mem::take(&mut self.attached) {
            let _ = StdCommand::new("/usr/bin/hdiutil")
                .args(["detach", "-quiet", "-force", &device])
                .status();
        }
    }
}

fn disk_tests_enabled() -> bool {
    if !cfg!(target_os = "macos") {
        eprintln!("skipped: the disk image tests need macOS (hdiutil)");
        return false;
    }
    let on = std::env::var("ONEFOLDER_DISK_TESTS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set ONEFOLDER_DISK_TESTS=1 to attach real disk images");
    }
    on
}

/// A real drive the tester mounted, named by `ONEFOLDER_DRIVE` (on Linux,
/// `scripts/demo-linux-drive.sh` shows how to make one: an ext4 image
/// mounted with sudo under `/media/$USER`). Skipped when it is not set.
fn real_drive() -> Option<PathBuf> {
    let drive = std::env::var_os("ONEFOLDER_DRIVE").filter(|v| !v.is_empty());
    if drive.is_none() {
        eprintln!("skipped: set ONEFOLDER_DRIVE=/path/to/a/mounted/drive");
    }
    drive.map(PathBuf::from)
}

/// The file system UUID of the drive mounted at `mount_point`, as
/// util-linux reports it.
#[cfg(target_os = "linux")]
fn drive_uuid(mount_point: &Path) -> String {
    let out = StdCommand::new("findmnt")
        .args(["-n", "-o", "UUID", "--mountpoint"])
        .arg(mount_point)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A destination on a real mounted drive (`ONEFOLDER_DRIVE`) is recorded
/// with the drive's UUID and checked before each run: the run goes ahead,
/// and `status --json` and `doctor --json` report the drive mounted where
/// it was recorded. Nothing is unmounted. A folder under the drives'
/// folder that is not on a drive is refused at setup as being on the
/// system disk.
#[test]
fn a_real_drive_is_recorded_and_checked() {
    if !enabled() {
        return;
    }
    let Some(drive) = real_drive() else { return };
    let drive = fs::canonicalize(&drive).expect("ONEFOLDER_DRIVE exists");
    let w = World::new();
    w.config("");
    w.write("src/a.txt", "a");
    let folder = drive.join(format!("OneFolderTest-{}", std::process::id()));
    // Removed whatever happens, so the drive is left as it was.
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _remove = Remove(folder.clone());
    let out = w
        .onefolder()
        .args(["jobs", "add", "drive", "--source"])
        .arg(w.path("src"))
        .arg("--local")
        .arg(&folder)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(said.contains(": recorded volume"), "{said}");

    // The drive's identity, as recorded.
    let out = w
        .onefolder()
        .args(["jobs", "list", "--json"])
        .output()
        .unwrap();
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    let job = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["name"] == "drive")
        .unwrap_or_else(|| panic!("no job drive: {list}"));
    let volume = &job["destinations"][0]["volume"];
    let mount_point = volume["mount_point"]
        .as_str()
        .unwrap_or_else(|| panic!("no drive recorded: {list}"))
        .to_string();
    assert!(drive.starts_with(&mount_point), "{volume}");
    let uuid = volume["volume_uuid"]
        .as_str()
        .or(volume["partition_uuid"].as_str())
        .unwrap_or_else(|| panic!("no UUID recorded: {volume}"))
        .to_string();
    #[cfg(target_os = "linux")]
    assert_eq!(uuid, drive_uuid(Path::new(&mount_point)), "{volume}");
    assert!(!uuid.is_empty());

    let out = w
        .onefolder()
        .args(["run", "drive", "--yes", "--json"])
        .output()
        .unwrap();
    let lines = ndjson(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{lines:#?}");
    assert_eq!(fs::read_to_string(folder.join("a.txt")).unwrap(), "a");

    let out = w.onefolder().args(["status", "--json"]).output().unwrap();
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    let of_drive = |jobs: &Value| -> Value {
        jobs.as_array()
            .unwrap()
            .iter()
            .find(|j| j["name"] == "drive")
            .unwrap_or_else(|| panic!("no job drive: {jobs}"))["destinations"][0]
            .clone()
    };
    let v = &of_drive(&status["data"]["jobs"])["volume"];
    assert_eq!(v["state"], "mounted", "{v}");
    assert_eq!(v["mount_point"], mount_point.as_str(), "{v}");
    let out = w.onefolder().args(["doctor", "--json"]).output().unwrap();
    let doctor: Value = serde_json::from_slice(&out.stdout).unwrap();
    let d = &of_drive(&doctor["data"]["jobs"]);
    assert_eq!(d["volume"], "mounted", "{d}");
    assert!(
        d["message"]
            .as_str()
            .unwrap()
            .starts_with("mounted · volume"),
        "{d}"
    );

    // A folder where drives are mounted, but on the system disk.
    let (root, words) = if cfg!(target_os = "macos") {
        ("/Volumes".to_string(), "on the startup disk")
    } else {
        let user = std::env::var("USER").unwrap_or_else(|_| "nobody".into());
        (format!("/media/{user}"), "on the system disk")
    };
    let nowhere = format!("{root}/OneFolderNoSuchDrive/Documents");
    let out = w
        .onefolder()
        .args(["jobs", "add", "nowhere", "--source"])
        .arg(w.path("src"))
        .arg("--local")
        .arg(&nowhere)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stderr).to_string() + &String::from_utf8_lossy(&out.stdout);
    assert_ne!(out.status.code(), Some(0), "{text}");
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains(words), "{flat}");
}

/// A case-only rename onto a mounted drive that ignores case
/// (`ONEFOLDER_CASELESS_DRIVE`: a FAT or exFAT drive, or a CIFS share) is
/// never a move, even from a source that tells case apart: moving
/// `Report.pdf` to `report.pdf` there would be a move of a file onto
/// itself, which rclone answers by deleting it. The file stays, with its
/// content, and nothing moves. Skipped when the variable is not set.
#[test]
fn a_case_only_rename_onto_a_drive_that_ignores_case_is_not_a_move() {
    if !enabled() {
        return;
    }
    let Some(drive) = std::env::var_os("ONEFOLDER_CASELESS_DRIVE").filter(|v| !v.is_empty()) else {
        eprintln!("skipped: set ONEFOLDER_CASELESS_DRIVE=/path/to/a/mounted/fat/or/cifs/drive");
        return;
    };
    let drive = fs::canonicalize(PathBuf::from(drive)).expect("the drive exists");
    let w = World::new();
    w.config("");
    w.write("src/Report.pdf", "the quarterly report");
    w.write("src/Photos/a.jpg", "photo a");
    let folder = drive.join(format!("OneFolderCase-{}", std::process::id()));
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _remove = Remove(folder.clone());
    let out = w
        .onefolder()
        .args(["jobs", "add", "caseless", "--source"])
        .arg(w.path("src"))
        .arg("--local")
        .arg(&folder)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let run = || {
        let out = w
            .onefolder()
            .args(["run", "caseless", "--yes", "--json", "--moves", "move"])
            .output()
            .unwrap();
        (out.status.code(), ndjson(&out.stdout))
    };
    let (code, lines) = run();
    assert_eq!(code, Some(0), "{lines:#?}");
    fs::rename(w.path("src/Report.pdf"), w.path("src/report.pdf")).unwrap();
    fs::rename(w.path("src/Photos"), w.path("src/photos")).unwrap();
    let (code, lines) = run();
    assert_eq!(code, Some(0), "{lines:#?}");
    let preview = lines
        .iter()
        .find(|l| l["event"] == "preview")
        .unwrap_or_else(|| panic!("no preview: {lines:#?}"));
    assert_eq!(preview["moves"]["count"], 0, "{preview}");
    assert!(lines.iter().all(|l| l["event"] != "moves"), "{lines:#?}");
    // Whatever spelling the drive kept, the report is there with its content.
    let files = contents(&folder);
    let reports: Vec<&String> = files
        .keys()
        .filter(|k| k.eq_ignore_ascii_case("report.pdf"))
        .collect();
    assert_eq!(reports.len(), 1, "{files:?}");
    assert_eq!(files[reports[0]], "the quarterly report");
    assert_eq!(files.len(), 2, "{files:?}");
}

/// Real external volumes (APFS disk images): an unplugged drive, a mount
/// point left behind as an empty folder, the drive mounted somewhere else
/// with other volumes mounted first, and a read-only drive. Every case
/// refuses before anything is written. Runs only with
/// `ONEFOLDER_DISK_TESTS=1`; the images are detached by a drop guard.
#[test]
fn real_drives_unplugged_moved_and_read_only() {
    if !disk_tests_enabled() {
        return;
    }
    let w = World::new();
    w.config("");
    w.write("src/a.txt", "a");
    let mut images = Images::new(&w.root);
    let name = format!("OFTest{}", std::process::id());
    let image = images.create(&name);
    let run = |job: &str| {
        let out = w
            .onefolder()
            .args(["run", job, "--yes", "--json"])
            .output()
            .unwrap();
        let lines = ndjson(&out.stdout);
        (out.status.code(), lines)
    };
    let problem = |lines: &[Value]| -> String {
        let p = lines
            .iter()
            .find(|l| l["event"] == "problem")
            .unwrap_or_else(|| panic!("no problem: {lines:#?}"));
        p["message"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };

    // 1. Unplugged: attached under /Volumes, recorded, run, detached.
    let device = images.attach(&image, None, false);
    let volume = PathBuf::from(format!("/Volumes/{name}"));
    fs::create_dir_all(volume.join("Docs")).unwrap();
    let out = w
        .onefolder()
        .args(["jobs", "add", "unplugged", "--source"])
        .arg(w.path("src"))
        .arg("--local")
        .arg(volume.join("Docs"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (code, lines) = run("unplugged");
    assert_eq!(code, Some(0), "{lines:#?}");
    assert!(volume.join("Docs/a.txt").exists());
    images.detach(&device);
    let (code, lines) = run("unplugged");
    assert_eq!(code, Some(2), "{lines:#?}");
    let text = problem(&lines);
    assert!(
        text.starts_with(&format!("{name} drive not mounted")),
        "{text}"
    );
    assert!(
        text.contains("does not exist. Plug the drive in and run again."),
        "{text}"
    );

    // 2. The mount point left behind as an empty folder.
    let backup = w.path("Backup");
    let device = images.attach(&image, Some(&backup), false);
    let out = w
        .onefolder()
        .args(["jobs", "add", "empty", "--source"])
        .arg(w.path("src"))
        .arg("--local")
        .arg(backup.join("Docs"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    images.detach(&device);
    assert!(backup.is_dir(), "the mount point stays as a folder");
    let (code, lines) = run("empty");
    assert_eq!(code, Some(2), "{lines:#?}");
    let text = problem(&lines);
    assert!(
        text.starts_with(&format!("{name} drive not mounted")),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "{} is currently a folder on Macintosh HD. Plug the drive in and run again.",
            backup.display()
        )),
        "{text}"
    );
    assert_eq!(fs::read_dir(&backup).unwrap().count(), 0, "nothing written");
    // Re-recording now would drop the drive: refused.
    let out = w
        .onefolder()
        .args(["jobs", "edit", "empty", "--rerecord"])
        .arg(lines[0]["destination"].as_str().unwrap())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("looks unplugged"));

    // 3. Mounted somewhere else, with other volumes mounted first.
    let decoys: Vec<String> = (0..2)
        .map(|i| {
            let decoy = images.create(&format!("{name}D{i}"));
            images.attach(&decoy, None, false)
        })
        .collect();
    let elsewhere = w.path("Elsewhere");
    let device = images.attach(&image, Some(&elsewhere), false);
    let (code, lines) = run("empty");
    assert_eq!(code, Some(2), "{lines:#?}");
    let text = problem(&lines);
    assert!(
        text.starts_with(&format!("{name} drive mounted somewhere else")),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "but that drive is mounted at {}.",
            elsewhere.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "change the destination's path to {} with: onefolder jobs edit empty",
            elsewhere.join("Docs").display()
        )),
        "{text}"
    );
    let out = w
        .onefolder()
        .args(["status", "empty", "--json"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let volume_state = &v["data"]["jobs"][0]["destinations"][0]["volume"];
    assert_eq!(volume_state["state"], "not-mounted");
    assert_eq!(volume_state["elsewhere"], elsewhere.display().to_string());
    images.detach(&device);
    for d in decoys {
        images.detach(&d);
    }

    // 4. The right drive at the right place, but read-only.
    images.attach(&image, Some(&backup), true);
    let (code, lines) = run("empty");
    assert_eq!(code, Some(2), "{lines:#?}");
    let text = problem(&lines);
    assert!(
        text.starts_with(&format!("{name} drive is read-only")),
        "{text}"
    );
    let out = w.onefolder().arg("status").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("✗ mounted read-only"));
}

/// A local destination that is a symlink into /Volumes: with no drive
/// recorded it is refused (it could be an empty folder left by an
/// unplugged drive), and `jobs add` will not record it while the drive it
/// leads to is not mounted.
#[cfg(unix)]
#[test]
fn a_symlink_into_volumes_needs_a_recorded_drive() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let link = w.path("dst-link");
    // Where drives are mounted on this system (on Linux not /Volumes, which
    // some virtual machines share in from their Mac).
    let target = if cfg!(target_os = "macos") {
        "/Volumes/OneFolderNoSuchDrive/Documents"
    } else {
        "/media/OneFolderNoSuchDrive/Documents"
    };
    std::os::unix::fs::symlink(target, &link).unwrap();
    w.config(
        &(w.local_destination(LOCAL, &link)
            + &w.alias_destination(ALIAS, "Backups/Documents", "one-way")),
    );
    w.write("src/a.txt", "a");
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(1), "{lines:#?}");
    let problem = events(&lines, "problem", LOCAL)[0]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        problem.starts_with("No drive recorded for this destination\n"),
        "{problem}"
    );
    assert!(
        problem.contains(&format!("leads to {target} on a drive")),
        "{problem}"
    );
    assert!(problem.contains("--rerecord d-0001"), "{problem}");
    assert_eq!(summary_of(&lines, ALIAS)["outcome"], "ok");

    // Adding it: the drive behind the link cannot be proven.
    let out = w
        .onefolder()
        .args(["jobs", "add", "linked", "--source"])
        .arg(w.path("src"))
        .arg("--local")
        .arg(&link)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("could not record the drive"), "{text}");
    assert!(!fs::read_to_string(w.path("config/config.toml"))
        .unwrap()
        .contains("linked"));
}

impl World {
    /// Rewrites the job's excludes to `*.tmp` only (so `.DS_Store` is left
    /// to the built-in list) and sets `copy_hidden`.
    fn copy_hidden(&self, on: bool) {
        let path = self.path("config/config.toml");
        let text = fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for line in text.lines() {
            if line.starts_with("excludes = ") {
                out.push_str(&format!("excludes = [\"*.tmp\"]\ncopy_hidden = {on}\n"));
            } else if !line.starts_with("copy_hidden = ") {
                out.push_str(line);
                out.push('\n');
            }
        }
        fs::write(path, out).unwrap();
    }
}

impl World {
    /// The merge an older OneFolder made, before the built-in list and the
    /// hidden patterns: bisync with only the conflict pattern, and the
    /// sync state saying the destination is set up.
    fn old_merge(&self, kind: Kind, id: &str) {
        let path2 = match kind {
            Kind::Local => self.path("dst").display().to_string(),
            Kind::Alias => "onefolder-test:Backups/Documents".to_string(),
        };
        let workdir = self.workdir(id);
        fs::create_dir_all(&workdir).unwrap();
        let status = StdCommand::new(self.rclone_dir.join("rclone"))
            .env("RCLONE_CONFIG", self.path("rclone.conf"))
            .arg("bisync")
            .arg(self.path("src"))
            .arg(&path2)
            .arg("--workdir")
            .arg(&workdir)
            .args([
                "--resync",
                "--resync-mode",
                "path1",
                "--create-empty-src-dirs",
                "--compare",
                "size,modtime",
                "--modify-window",
                "2s",
                "--exclude",
                "*.conflict-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]-*",
            ])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{kind:?}");
        fs::write(
            self.path("data/state.json"),
            format!(r#"{{"version":1,"bisync":{{"documents/{id}":{{"initialised":true}}}}}}"#),
        )
        .unwrap();
    }
}

/// The junk the built-in list leaves out, and hidden files.
const JUNK: [&str; 6] = [
    ".DS_Store",
    "._report.docx",
    "Thumbs.db",
    "desktop.ini",
    "Projects/.DS_Store",
    "Projects/Thumbs.db",
];
const HIDDEN: [&str; 3] = [".hidden", ".hiddendir/file", "Projects/.env"];

/// One-way leaves system files and hidden files behind by default, at any
/// depth; with `copy_hidden = true` the hidden files go, the junk still
/// does not. Both destination types.
#[test]
fn one_way_leaves_system_and_hidden_files_behind() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.copy_hidden(false);
    w.write("src/report.docx", "report");
    w.write("src/Projects/plan.md", "plan");
    for f in JUNK.iter().chain(HIDDEN.iter()) {
        w.write(&format!("src/{f}"), "x");
    }
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, dir) in [(LOCAL, w.path("dst")), (ALIAS, w.alias_dir())] {
        let files: Vec<String> = contents(&dir).into_keys().collect();
        assert_eq!(files, ["Projects/plan.md", "report.docx"], "{id}");
        let preview = events(&lines, "preview", id)[0];
        assert_eq!(preview["new"]["files"], 2, "{preview}");
        assert_eq!(preview["copy_hidden"], false);
        assert_eq!(preview["excludes"], serde_json::json!(["*.tmp"]));
        let effective = preview["effective_excludes"].as_array().unwrap();
        assert_eq!(effective.first().unwrap(), ".DS_Store");
        assert_eq!(effective.last().unwrap(), "*.tmp");
        assert!(effective.iter().any(|p| p == ".*/**"));
    }

    // Hidden files copied; the junk still left behind.
    w.copy_hidden(true);
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, dir) in [(LOCAL, w.path("dst")), (ALIAS, w.alias_dir())] {
        let files: Vec<String> = contents(&dir).into_keys().collect();
        assert_eq!(
            files,
            [
                ".hidden",
                ".hiddendir/file",
                "Projects/.env",
                "Projects/plan.md",
                "report.docx"
            ],
            "{id}"
        );
        let preview = events(&lines, "preview", id)[0];
        assert_eq!(preview["copy_hidden"], true);
        assert!(!preview["effective_excludes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == ".*"));
    }
}

/// An existing two-way destination whose listings still hold system files
/// and hidden files (synced before the built-in list existed): the first
/// run with the new excludes deletes nothing on either side and needs no
/// merge, though bisync sees those entries gone from both listings; the
/// next run is clean too. Copying hidden files again later finds the same
/// files on both sides and makes no conflict copies.
#[test]
fn new_excludes_on_an_existing_two_way_destination_delete_nothing() {
    if !enabled() {
        return;
    }
    for kind in KINDS {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        w.copy_hidden(false);
        w.write("src/a.txt", "a");
        w.write("src/Projects/plan.md", "plan");
        for f in JUNK.iter().chain(HIDDEN.iter()) {
            w.write(&format!("src/{f}"), &format!("{f} content"));
        }
        w.old_merge(kind, id);
        let workdir = w.workdir(id);
        let listing = fs::read_dir(&workdir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().ends_with(".path1.lst"))
            .unwrap();
        assert!(fs::read_to_string(listing)
            .unwrap()
            .contains("\"Projects/.DS_Store\""));
        let before_src = contents(&w.path("src"));
        let before_dst = contents(&dst);
        assert_eq!(before_src, before_dst, "{kind:?}");

        // Normal changes on both sides travel as usual.
        tick();
        w.write("src/new-here.txt", "new here");
        put(&dst, "new-there.txt", "new there");
        let lines = w.sync_ok(id, &[]);
        assert!(events(&lines, "resync", id).is_empty(), "{lines:#?}");
        assert!(events(&lines, "folder_prompt", id).is_empty());
        let plan = events(&lines, "plan", id)[0];
        assert_eq!(plan["delete_destination"], serde_json::json!([]), "{plan}");
        assert_eq!(plan["delete_local"], serde_json::json!([]), "{plan}");
        // bisync counts the entries that left its listings against its
        // 50% limit: here they are most of them, so OneFolder lifts the
        // limit itself, as it does for holds, and asks nothing.
        assert_eq!(plan["lift_max_delete"], true, "{plan}");
        assert_eq!(plan["needs_confirmation"], serde_json::json!([]), "{plan}");
        let mut src = contents(&w.path("src"));
        let mut there = contents(&dst);
        for f in JUNK.iter().chain(HIDDEN.iter()) {
            assert_eq!(src.remove(*f), before_src.get(*f).cloned(), "{kind:?} {f}");
            assert_eq!(
                there.remove(*f),
                before_dst.get(*f).cloned(),
                "{kind:?} {f}"
            );
        }
        assert_eq!(src, there, "{kind:?}");
        assert_eq!(src["new-here.txt"], "new here");
        assert_eq!(src["new-there.txt"], "new there");
        assert_eq!(
            summary_of(&lines, id)["deleted"],
            serde_json::json!({"local": 0, "remote": 0})
        );

        // The next run: nothing to do, still nothing deleted.
        let lines = w.sync_ok(id, &[]);
        assert!(events(&lines, "resync", id).is_empty(), "{lines:#?}");
        assert_eq!(
            summary_of(&lines, id)["deleted"],
            serde_json::json!({"local": 0, "remote": 0})
        );
        assert_eq!(contents(&w.path("src")).len(), before_src.len() + 2);
        assert_eq!(contents(&dst).len(), before_dst.len() + 2);

        // Copying hidden files again: the same files on both sides, so no
        // conflict copies and nothing moves.
        w.copy_hidden(true);
        let lines = w.sync_ok(id, &[]);
        assert!(events(&lines, "resync", id).is_empty(), "{lines:#?}");
        assert!(events(&lines, "conflict", id).is_empty(), "{lines:#?}");
        assert_eq!(contents(&w.path("src")), contents(&dst), "{kind:?}");
        assert_eq!(contents(&w.path("src")).len(), before_src.len() + 2);
    }
}

/// Entries that left the sync (30 files under `.git` an older OneFolder
/// synced, now excluded) must not hide a real mass deletion behind them:
/// deleting 6 of the 10 real files still needs confirming on a terminal,
/// with the real counts, and nothing is deleted without it.
#[test]
fn a_mass_deletion_behind_newly_excluded_files_still_needs_confirming() {
    if !enabled() {
        return;
    }
    for kind in KINDS {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        w.copy_hidden(false);
        for i in 0..10 {
            w.write(&format!("src/f{i}.txt"), "x");
        }
        for i in 0..30 {
            w.write(&format!("src/.git/objects/o{i}"), "obj");
        }
        w.old_merge(kind, id);
        for i in 0..6 {
            w.remove(&format!("src/f{i}.txt"));
        }
        let (code, lines) = w.run_json(&["--yes"]);
        assert_eq!(code, Some(1), "{kind:?} {lines:#?}");
        let plan = events(&lines, "plan", id)[0];
        assert_eq!(
            plan["needs_confirmation"],
            serde_json::json!([{"kind": "deletions", "side": "destination",
                                "count": 6, "total": 10, "percent": 60}]),
            "{plan}"
        );
        let reason = events(&lines, "skipped", id)[0]["reason"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(reason.contains("6 of 10"), "{reason}");
        let visible: Vec<String> = contents(&dst)
            .into_keys()
            .filter(|k| !k.starts_with(".git"))
            .collect();
        let all: Vec<String> = (0..10).map(|i| format!("f{i}.txt")).collect();
        assert_eq!(visible, all, "{kind:?}: nothing was deleted");
    }
}

/// After the upgrade, a hidden folder and a folder holding only system
/// files, deleted locally, are gone from both listings: the sync can do
/// nothing with them, so whatever `--folder-deletes` says, nobody is asked,
/// nothing is held, restored or deleted, and both stay on the destination.
#[test]
fn folders_the_sync_no_longer_sees_are_not_asked_about() {
    if !enabled() {
        return;
    }
    for answer in ["keep", "delete", "restore"] {
        let w = World::new();
        let (id, dst) = w.two_way(Kind::Local);
        w.copy_hidden(false);
        filler(&w);
        w.write("src/.cache/a", "a");
        w.write("src/.cache/b", "b");
        w.write("src/Photos/.DS_Store", "ds");
        w.write("src/Photos/Thumbs.db", "th");
        w.old_merge(Kind::Local, id);
        w.remove("src/.cache");
        w.remove("src/Photos");
        let lines = w.sync_ok(id, &["--folder-deletes", answer]);
        assert!(
            events(&lines, "folder_prompt", id).is_empty(),
            "{answer} {lines:#?}"
        );
        assert_eq!(
            events(&lines, "plan", id)[0]["folders"],
            serde_json::json!([])
        );
        assert!(
            w.state()["holds"]
                .as_object()
                .is_none_or(|h| h.values().all(|v| v.as_array().unwrap().is_empty())),
            "{answer}: {}",
            w.state()["holds"]
        );
        let src = contents(&w.path("src"));
        assert!(
            !src.keys()
                .any(|k| k.starts_with(".cache") || k.starts_with("Photos")),
            "{answer}: nothing restored {src:?}"
        );
        let there: Vec<String> = contents(&dst)
            .into_keys()
            .filter(|k| !k.starts_with("filler"))
            .collect();
        assert_eq!(
            there,
            [
                ".cache/a",
                ".cache/b",
                "Photos/.DS_Store",
                "Photos/Thumbs.db"
            ],
            "{answer}"
        );
    }
}

/// Deleting a folder deletes it completely: the hidden and system files
/// bisync cannot see go too (Finder writes `.DS_Store` into folders it
/// opens), the summary says so, and the run log records it. A folder that
/// still holds a file the sync handles (here a conflict copy) is left.
/// Restoring a folder brings back only what the sync would.
#[test]
fn deleting_a_folder_removes_its_hidden_and_system_files_too() {
    if !enabled() {
        return;
    }
    for kind in KINDS {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        w.copy_hidden(false);
        filler(&w);
        w.write("src/Old/a.txt", "a");
        w.write("src/Kept/b.txt", "b");
        w.write("src/Back/c.txt", "c");
        w.sync_ok(id, &[]);
        put(&dst, "Old/.DS_Store", "finder");
        put(&dst, "Old/.env", "secret");
        put(&dst, "Old/Thumbs.db", "thumbs");
        put(&dst, "Kept/.DS_Store", "finder");
        put(&dst, "Kept/b.conflict-2026-01-01-1.txt", "a losing copy");
        put(&dst, "Back/.env", "hidden");
        w.remove("src/Old");
        w.remove("src/Kept");
        let lines = w.sync_ok(id, &["--folder-deletes", "delete"]);
        let there = contents(&dst);
        assert!(
            !there.keys().any(|k| k.starts_with("Old")),
            "{kind:?} {there:?}"
        );
        assert!(
            !dst.join("Old").exists(),
            "{kind:?}: the folder itself is gone"
        );
        assert_eq!(
            summary_of(&lines, id)["removed_folders"],
            serde_json::json!(["Old"]),
            "{kind:?}"
        );
        let notes: Vec<String> = events(&lines, "note", id)
            .iter()
            .map(|n| n["message"].as_str().unwrap().to_string())
            .collect();
        assert!(
            notes.iter().any(|n| n.contains("Removed \"Old\"")
                && n.contains("including the hidden and system files")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("\"Kept\" still has 1 file")),
            "{notes:?}"
        );
        assert!(there.contains_key("Kept/b.conflict-2026-01-01-1.txt"));
        let log = fs::read_dir(w.path(&format!("data/logs/documents/{id}")))
            .unwrap()
            .map(|e| fs::read_to_string(e.unwrap().path()).unwrap_or_default())
            .collect::<String>();
        assert!(log.contains("\"onefolder\":\"folder-removed\""), "{log}");

        // Restoring a folder brings back what the sync would, not the
        // hidden file only the destination has.
        w.remove("src/Back");
        w.sync_ok(id, &["--folder-deletes", "restore"]);
        let src = contents(&w.path("src"));
        assert_eq!(
            src.get("Back/c.txt").map(String::as_str),
            Some("c"),
            "{kind:?}"
        );
        assert!(!src.contains_key("Back/.env"), "{kind:?} {src:?}");
    }
}

/// Turning "Copy hidden files" on for a destination an older OneFolder
/// synced them to: the hidden files had stopped syncing, so a hidden file
/// deleted on one side meanwhile comes back, and one edited on one side
/// meets the other copy as a conflict (the local copy wins, the other is
/// kept as a conflict copy on the destination). A folder deleted with
/// Delete is not brought back by a hidden file that was left in it.
#[test]
fn turning_copy_hidden_on_later() {
    if !enabled() {
        return;
    }
    for kind in KINDS {
        let w = World::new();
        let (id, dst) = w.two_way(kind);
        w.copy_hidden(false);
        filler(&w);
        w.write("src/.hidden", "v1");
        w.write("src/.hd/gone", "g");
        w.write("src/Old/a.txt", "a");
        w.write("src/Old/.DS_Store", "ds");
        w.write("src/Old/.env", "env");
        w.old_merge(kind, id);
        w.sync_ok(id, &[]);
        tick();
        w.write("src/.hidden", "v2 local, longer");
        fs::remove_file(dst.join(".hd/gone")).unwrap();
        w.remove("src/Old");
        let lines = w.sync_ok(id, &["--folder-deletes", "delete"]);
        assert!(
            !contents(&dst).keys().any(|k| k.starts_with("Old")),
            "{kind:?}"
        );
        assert_eq!(
            summary_of(&lines, id)["removed_folders"],
            serde_json::json!(["Old"])
        );

        w.copy_hidden(true);
        let lines = w.sync_ok(id, &[]);
        assert!(events(&lines, "resync", id).is_empty(), "{lines:#?}");
        let src = contents(&w.path("src"));
        let there = contents(&dst);
        assert_eq!(src[".hidden"], "v2 local, longer");
        assert_eq!(there[".hidden"], "v2 local, longer");
        let conflicts = events(&lines, "conflict", id);
        assert_eq!(conflicts.len(), 1, "{kind:?} {lines:#?}");
        let kept_as = conflicts[0]["kept_as"].as_str().unwrap();
        assert!(kept_as.starts_with(".hidden.conflict-"), "{kept_as}");
        assert_eq!(there[kept_as], "v1");
        assert_eq!(
            there[".hd/gone"], "g",
            "deleted there while excluded, back again"
        );
        assert!(
            !src.keys().any(|k| k.starts_with("Old")),
            "{kind:?} {src:?}"
        );
        assert!(
            !there.keys().any(|k| k.starts_with("Old")),
            "{kind:?} {there:?}"
        );
    }
}

// ------------------------------------------------------------ moved files

/// Every file under `dir` with its QuickXorHash, from `rclone hashsum`,
/// to prove content on the destination is what the source has.
fn quickxor(w: &World, dir: &Path) -> BTreeMap<String, String> {
    let out = StdCommand::new(w.rclone_dir.join("rclone"))
        .env("RCLONE_CONFIG", w.path("rclone.conf"))
        .args(["hashsum", "quickxor"])
        .arg(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once("  "))
        .map(|(h, p)| (p.to_string(), h.to_string()))
        .collect()
}

/// The destinations of the usual job, by id, with their folder on disk.
fn both_dirs(w: &World) -> [(&'static str, PathBuf); 2] {
    [(LOCAL, w.path("dst")), (ALIAS, w.alias_dir())]
}

/// Seeds the source onto both destinations, so they hold the same files
/// with the same times.
fn seed_both(w: &World) {
    w.seed(&w.path("src"), &w.path("dst"));
    w.seed(&w.path("src"), &w.alias_dir());
}

fn moved_of(lines: &[Value], id: &str) -> Value {
    summary_of(lines, id)
        .get("moved")
        .cloned()
        .unwrap_or(Value::Null)
}

/// A file moved locally is moved on both destination types, not uploaded:
/// the destination has it only at the new path, with the same content and
/// time, and the next run finds it unchanged. `--dry-run` says what would
/// move and moves nothing.
#[test]
fn a_moved_file_is_moved_on_the_destination() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/Docs/report.pdf", "the quarterly report");
    w.write("src/same.txt", "same");
    seed_both(&w);
    fs::create_dir_all(w.path("src/Archive")).unwrap();
    fs::rename(
        w.path("src/Docs/report.pdf"),
        w.path("src/Archive/report.pdf"),
    )
    .unwrap();

    // A dry run shows the move and changes nothing.
    let before = (tree(&w.path("dst")), tree(&w.alias_dir()));
    let (code, lines) = w.run_json(&["--dry-run"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, _) in both_dirs(&w) {
        let p = events(&lines, "preview", id)[0];
        assert_eq!(p["moves"]["count"], 1, "{p}");
        assert_eq!(
            p["moves"]["choice"], "move",
            "the default without a terminal"
        );
        assert_eq!(p["new"]["files"], 0, "{p}");
        assert_eq!(moved_of(&lines, id)["files"], 1, "{lines:#?}");
        assert_eq!(summary_of(&lines, id)["dry_run"], true);
    }
    let out = w
        .onefolder()
        .args(["run", "documents", "--dry-run"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("  Moved      1 file    20 B   will be moved"),
        "{text}"
    );
    assert!(
        text.contains("    Docs/report.pdf  →  Archive/report.pdf"),
        "{text}"
    );
    assert!(
        text.contains("Would move: 1 file, 20 B (on the drive, no upload)"),
        "{text}"
    );
    assert!(
        text.contains("Would move: 1 file, 20 B (on OneDrive, no upload)"),
        "{text}"
    );
    assert_eq!(before, (tree(&w.path("dst")), tree(&w.alias_dir())));

    // The real run.
    let (code, lines) = w.run_json(&["--yes", "--moves", "move"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    let source_hash = quickxor(&w, &w.path("src"))["Archive/report.pdf"].clone();
    let source_time = tree(&w.path("src"))["Archive/report.pdf"].1;
    for (id, dir) in both_dirs(&w) {
        let decided = events(&lines, "moves", id);
        assert_eq!(decided.len(), 1, "{lines:#?}");
        assert_eq!(decided[0]["by"], "flag");
        assert!(
            events(&lines, "file", id).is_empty(),
            "nothing uploaded: {lines:#?}"
        );
        let s = summary_of(&lines, id);
        assert_eq!(s["added"]["remote"], 0);
        assert_eq!(s["bytes"], 0);
        assert_eq!(s["deleted"]["remote"], 0);
        assert_eq!(
            moved_of(&lines, id),
            serde_json::json!({"files": 1, "bytes": 20})
        );
        assert_eq!(s["destination_only"], 0);
        let files = tree(&dir);
        assert!(!files.contains_key("Docs/report.pdf"), "{files:?}");
        assert_eq!(
            files["Archive/report.pdf"].1, source_time,
            "the time is kept"
        );
        assert_eq!(quickxor(&w, &dir)["Archive/report.pdf"], source_hash);
        // The run log has the move, before and after.
        let record = &w.state()["runs"][format!("documents/{id}")];
        let log = fs::read_to_string(w.path("data").join(record["log"].as_str().unwrap())).unwrap();
        assert!(
            log.contains(r#""onefolder":"move","from":"Docs/report.pdf""#),
            "{log}"
        );
        assert!(
            log.contains(r#""onefolder":"moved","from":"Docs/report.pdf""#),
            "{log}"
        );
    }

    // The next run: everything unchanged, nothing to move.
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, _) in both_dirs(&w) {
        assert_eq!(events(&lines, "preview", id)[0]["moves"]["count"], 0);
        assert_eq!(summary_of(&lines, id)["unchanged"], 2, "{lines:#?}");
        assert!(events(&lines, "file", id).is_empty());
    }
}

/// A renamed folder is one folder move: every file is at the new path,
/// the old folder is gone, and nothing was uploaded.
#[test]
fn a_renamed_folder_is_one_folder_move() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/Projects/2025/plan.docx", "plan");
    w.write("src/Projects/2025/budget.xlsx", "budget numbers");
    w.write("src/Projects/2025/drafts/one.md", "draft one");
    w.write("src/keep.txt", "keep");
    seed_both(&w);
    fs::rename(w.path("src/Projects"), w.path("src/Archive")).unwrap();

    let (code, lines) = w.run_json(&["--yes", "--moves", "move"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    let source = quickxor(&w, &w.path("src"));
    for (id, dir) in both_dirs(&w) {
        let p = events(&lines, "preview", id)[0];
        assert_eq!(
            p["moves"]["groups"],
            serde_json::json!([{"from": "Projects", "to": "Archive", "files": 3,
                                "bytes": 27, "folder": true}]),
            "{p}"
        );
        assert!(events(&lines, "file", id).is_empty(), "{lines:#?}");
        assert_eq!(moved_of(&lines, id)["files"], 3);
        assert_eq!(summary_of(&lines, id)["bytes"], 0);
        assert!(!dir.join("Projects").exists(), "the old folder is gone");
        assert_eq!(quickxor(&w, &dir), source, "every file, the same content");
        let record = &w.state()["runs"][format!("documents/{id}")];
        let log = fs::read_to_string(w.path("data").join(record["log"].as_str().unwrap())).unwrap();
        let moves: Vec<&str> = log
            .lines()
            .filter(|l| l.contains(r#""onefolder":"move","#))
            .collect();
        assert_eq!(moves.len(), 1, "one moveto for the folder: {log}");
        assert!(moves[0].contains(r#""group":"folder""#), "{log}");
    }
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(0));
    for (id, _) in both_dirs(&w) {
        assert_eq!(summary_of(&lines, id)["unchanged"], 4, "{lines:#?}");
        assert!(events(&lines, "file", id).is_empty());
    }
}

/// `--moves copy` uploads the moved file again and leaves the old copy.
#[test]
fn moves_copy_uploads_again() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/Docs/report.pdf", "the quarterly report");
    seed_both(&w);
    fs::create_dir_all(w.path("src/Archive")).unwrap();
    fs::rename(
        w.path("src/Docs/report.pdf"),
        w.path("src/Archive/report.pdf"),
    )
    .unwrap();
    let (code, lines) = w.run_json(&["--yes", "--moves", "copy"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, dir) in both_dirs(&w) {
        let p = events(&lines, "preview", id)[0];
        assert_eq!(p["moves"]["count"], 1, "{p}");
        assert_eq!(p["moves"]["choice"], "copy");
        assert_eq!(p["new"]["files"], 1, "counted under New again: {p}");
        assert_eq!(events(&lines, "moves", id)[0]["answer"], "copy");
        let s = summary_of(&lines, id);
        assert_eq!(s["added"]["remote"], 1);
        assert!(s.get("moved").is_none(), "{s}");
        assert_eq!(s["destination_only"], 1);
        let files = contents(&dir);
        assert_eq!(
            files["Docs/report.pdf"], "the quarterly report",
            "old copy kept"
        );
        assert_eq!(files["Archive/report.pdf"], "the quarterly report");
    }
}

/// Two new files with the same content, name and time as one old copy
/// (moved, then copied keeping its time): which is the move is not
/// guessed, so both are uploaded and the old copy stays.
#[test]
fn identical_twins_are_uploaded_not_moved() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/a.txt", "twin content");
    seed_both(&w);
    let time = fs::metadata(w.path("src/a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    fs::create_dir_all(w.path("src/x")).unwrap();
    fs::create_dir_all(w.path("src/y")).unwrap();
    fs::rename(w.path("src/a.txt"), w.path("src/x/a.txt")).unwrap();
    fs::copy(w.path("src/x/a.txt"), w.path("src/y/a.txt")).unwrap();
    fs::File::options()
        .write(true)
        .open(w.path("src/y/a.txt"))
        .unwrap()
        .set_modified(time)
        .unwrap();
    let (code, lines) = w.run_json(&["--yes", "--moves", "move"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, dir) in both_dirs(&w) {
        assert_eq!(events(&lines, "preview", id)[0]["moves"]["count"], 0);
        assert!(events(&lines, "moves", id).is_empty());
        assert_eq!(summary_of(&lines, id)["added"]["remote"], 2);
        let files = contents(&dir);
        assert_eq!(files.len(), 3, "{files:?}");
        assert_eq!(files["a.txt"], "twin content", "the old copy stays");
    }
}

/// A file moved and changed (same size, other content) is a new file:
/// uploaded, with the old copy left alone.
#[test]
fn moved_and_changed_is_uploaded() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/notes.txt", "AAAA");
    seed_both(&w);
    w.remove("src/notes.txt");
    w.write("src/Moved/notes.txt", "BBBB");
    let (code, lines) = w.run_json(&["--yes", "--moves", "move"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, dir) in both_dirs(&w) {
        assert_eq!(events(&lines, "preview", id)[0]["moves"]["count"], 0);
        assert_eq!(summary_of(&lines, id)["added"]["remote"], 1);
        let files = contents(&dir);
        assert_eq!(files["notes.txt"], "AAAA");
        assert_eq!(files["Moved/notes.txt"], "BBBB");
    }
}

/// True when the file system holding `dir` tells names apart by case
/// alone (ext4, tmpfs, btrfs on Linux), as OneFolder's move detection
/// then does for a local folder there.
fn counts_case(dir: &Path) -> bool {
    let probe = dir.join("case-probe");
    fs::write(&probe, "x").unwrap();
    let counts = !dir.join("CASE-PROBE").exists();
    fs::remove_file(&probe).unwrap();
    counts
}

/// A rename that only changes case is the same file on a destination
/// that ignores case (OneDrive, every folder on a Mac): nothing is new,
/// nothing is left alone, nothing moves. An identical copy elsewhere is
/// not a move of the old spelling.
///
/// Where the local destination and the source tell case apart (two ext4
/// folders on Linux), the case-only renames are moves: the file and the
/// folder are renamed on the destination, which then holds the new
/// spellings only. The alias standing in for OneDrive is still compared
/// ignoring case, as OneDrive is, but rclone's local backend under it
/// counts case on Linux, so there the new spellings arrive beside the old.
#[test]
fn case_only_renames_are_not_moves() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/Report.pdf", "the quarterly report");
    w.write("src/Photos/a.jpg", "photo a");
    w.write("src/Photos/b.jpg", "photo b, longer");
    seed_both(&w);
    fs::rename(w.path("src/Report.pdf"), w.path("src/report.pdf")).unwrap();
    fs::rename(w.path("src/Photos"), w.path("src/photos")).unwrap();
    // The source and both destinations share one temp file system.
    let case_counts = !cfg!(target_os = "macos") && counts_case(&w.path(""));
    let (code, lines) = w.run_json(&["--yes", "--moves", "move"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, dir) in both_dirs(&w) {
        let p = events(&lines, "preview", id)[0];
        if id == LOCAL && case_counts {
            assert_eq!(p["moves"]["count"], 3, "{p}");
            assert_eq!(p["destination_only"], 3, "{p}");
            assert_eq!(moved_of(&lines, id)["files"], 3, "{lines:#?}");
            assert_eq!(summary_of(&lines, id)["destination_only"], 0);
            let files: Vec<String> = contents(&dir).into_keys().collect();
            assert_eq!(files, ["photos/a.jpg", "photos/b.jpg", "report.pdf"]);
            continue;
        }
        assert_eq!(p["moves"]["count"], 0, "{p}");
        assert_eq!(p["destination_only"], 0, "{p}");
        assert_eq!(summary_of(&lines, id)["destination_only"], 0);
    }

    // An identical copy at Backup/report.pdf: the old spelling Report.pdf
    // is still the source's report.pdf, so nothing moves; the copy is new.
    let time = fs::metadata(w.path("src/report.pdf"))
        .unwrap()
        .modified()
        .unwrap();
    fs::create_dir_all(w.path("src/Backup")).unwrap();
    fs::copy(w.path("src/report.pdf"), w.path("src/Backup/report.pdf")).unwrap();
    fs::File::options()
        .write(true)
        .open(w.path("src/Backup/report.pdf"))
        .unwrap()
        .set_modified(time)
        .unwrap();
    let (code, lines) = w.run_json(&["--yes", "--moves", "move"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, dir) in both_dirs(&w) {
        assert_eq!(events(&lines, "preview", id)[0]["moves"]["count"], 0);
        assert!(events(&lines, "moves", id).is_empty());
        let s = summary_of(&lines, id);
        assert_eq!(s["added"]["remote"], 1, "{s}");
        assert!(s.get("moved").is_none(), "{s}");
        let files = contents(&dir);
        assert_eq!(files["Backup/report.pdf"], "the quarterly report");
        let top: Vec<&String> = files.keys().filter(|k| !k.contains('/')).collect();
        if id == ALIAS && case_counts {
            assert_eq!(top, ["Report.pdf", "report.pdf"], "{files:?}");
        } else {
            assert_eq!(top.len(), 1, "the report is still at the top: {files:?}");
        }
    }
}

/// A renamed folder of packages whose LICENSE files are identical moves
/// as one folder: each LICENSE stays with its package, none is left
/// behind and none is uploaded.
#[test]
fn identical_files_in_a_renamed_folder_move_with_it() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    for i in 1..=5 {
        w.write(&format!("src/Proj/pkg{i}/LICENSE"), "MIT licence text");
        w.write(&format!("src/Proj/pkg{i}/index.js"), &format!("module {i}"));
    }
    seed_both(&w);
    fs::rename(w.path("src/Proj"), w.path("src/Arch")).unwrap();
    let (code, lines) = w.run_json(&["--yes", "--moves", "move"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    let source = quickxor(&w, &w.path("src"));
    for (id, dir) in both_dirs(&w) {
        let p = events(&lines, "preview", id)[0];
        assert_eq!(
            p["moves"]["groups"],
            serde_json::json!([{"from": "Proj", "to": "Arch", "files": 10,
                                "bytes": 5 * 16 + 5 * 8, "folder": true}]),
            "{p}"
        );
        assert_eq!(p["new"]["files"], 0, "{p}");
        assert!(events(&lines, "file", id).is_empty(), "{lines:#?}");
        assert!(!dir.join("Proj").exists(), "nothing left behind");
        assert_eq!(quickxor(&w, &dir), source);
    }
}

/// A folder move whose old folder holds a file the sync leaves out (a
/// hidden file) is split at run time: its files move one by one and the
/// hidden file stays where it was.
#[test]
fn a_folder_with_an_unseen_file_moves_file_by_file() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/Old/a.txt", "file a");
    w.write("src/Old/b.txt", "file b, longer");
    seed_both(&w);
    for (_, dir) in both_dirs(&w) {
        fs::write(dir.join("Old/.extra"), "not synced").unwrap();
    }
    fs::rename(w.path("src/Old"), w.path("src/New")).unwrap();
    let (code, lines) = w.run_json(&["--yes", "--moves", "move"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, dir) in both_dirs(&w) {
        let p = events(&lines, "preview", id)[0];
        assert_eq!(p["moves"]["groups"][0]["folder"], true, "{p}");
        assert_eq!(moved_of(&lines, id)["files"], 2, "{lines:#?}");
        assert!(events(&lines, "file", id).is_empty(), "{lines:#?}");
        let files = contents(&dir);
        assert_eq!(files["New/a.txt"], "file a");
        assert_eq!(files["New/b.txt"], "file b, longer");
        assert_eq!(files["Old/.extra"], "not synced", "left where it was");
        assert!(!files.contains_key("Old/a.txt"));
        let record = &w.state()["runs"][format!("documents/{id}")];
        let log = fs::read_to_string(w.path("data").join(record["log"].as_str().unwrap())).unwrap();
        assert!(log.contains(r#""onefolder":"move-folder-split""#), "{log}");
    }
}

/// A move that fails (the old folder cannot be written to) is listed as
/// an error, and the copy uploads the file under its new name instead.
#[test]
fn a_failed_move_is_uploaded_instead() {
    use std::os::unix::fs::PermissionsExt;
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/Docs/report.pdf", "the quarterly report");
    w.write("src/Docs/notes.txt", "notes");
    seed_both(&w);
    fs::create_dir_all(w.path("src/Archive")).unwrap();
    fs::rename(
        w.path("src/Docs/report.pdf"),
        w.path("src/Archive/report.pdf"),
    )
    .unwrap();
    let locked = |mode| {
        for (_, dir) in both_dirs(&w) {
            fs::set_permissions(dir.join("Docs"), fs::Permissions::from_mode(mode)).unwrap();
        }
    };
    locked(0o555);
    let (code, lines) = w.run_json(&["--yes", "--moves", "move"]);
    locked(0o755);
    assert_eq!(code, Some(1), "partial: {lines:#?}");
    for (id, dir) in both_dirs(&w) {
        let errors = events(&lines, "error", id);
        assert!(
            errors.iter().any(|e| e["path"] == "Archive/report.pdf"
                && e["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("not moved from Docs/report.pdf on ")),
            "{lines:#?}"
        );
        let s = summary_of(&lines, id);
        assert_eq!(s["outcome"], "partial", "{s}");
        let files = contents(&dir);
        assert_eq!(files["Archive/report.pdf"], "the quarterly report");
        assert_eq!(
            files["Docs/report.pdf"], "the quarterly report",
            "not deleted"
        );
    }
}

/// `--moves ask` with no terminal to ask on never moves: the file is
/// uploaded again and the decision says it was not asked.
#[test]
fn moves_ask_without_a_terminal_never_moves() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.both();
    w.write("src/Docs/report.pdf", "the quarterly report");
    seed_both(&w);
    fs::create_dir_all(w.path("src/Archive")).unwrap();
    fs::rename(
        w.path("src/Docs/report.pdf"),
        w.path("src/Archive/report.pdf"),
    )
    .unwrap();
    let (code, lines) = w.run_json(&["--yes", "--moves", "ask"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    for (id, dir) in both_dirs(&w) {
        let decided = events(&lines, "moves", id);
        assert_eq!(decided[0]["answer"], "copy", "{lines:#?}");
        assert_eq!(decided[0]["by"], "not-asked");
        assert_eq!(events(&lines, "preview", id)[0]["moves"]["choice"], "copy");
        assert!(summary_of(&lines, id).get("moved").is_none());
        let files = contents(&dir);
        assert!(files.contains_key("Docs/report.pdf") && files.contains_key("Archive/report.pdf"));
    }
}

/// A job exclude `folder/*` or a bare folder name leaves out the whole
/// folder, subfolders and all, while a file whose name only starts the
/// same is copied. rclone's own `*` never crosses a `/`, so OneFolder
/// widens the pattern; the job keeps it as typed.
#[test]
fn a_folder_exclude_leaves_out_the_whole_folder() {
    if !enabled() {
        return;
    }
    let w = World::new();
    fs::write(
        w.path("config/config.toml"),
        "version = 1\n\n[[accounts]]\nlabel = \"test\"\nremote = \"onefolder-test\"\ndrive_type = \"business\"\n",
    )
    .unwrap();
    let out = w
        .onefolder()
        .args(["jobs", "add", "documents", "--source"])
        .arg(w.path("src"))
        .args([
            "--exclude",
            "_0RAG/*",
            "--exclude",
            "node_modules",
            "--local",
        ])
        .arg(w.path("dst"))
        .args(["--onedrive", "test:Backups/Documents:one-way"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config = fs::read_to_string(w.path("config/config.toml")).unwrap();
    assert!(config.contains("\"_0RAG/*\""), "{config}");
    assert!(!config.contains("_0RAG/**"), "{config}");

    for f in [
        "_0RAG/top.py",
        "_0RAG/backend/app.py",
        "_0RAG/backend/models/db.py",
        "sub/_0RAG/deep.py",
        "node_modules/pkg/index.js",
        "lib/node_modules/a/b.js",
        "other/node_modules",
    ] {
        w.write(&format!("src/{f}"), "left out");
    }
    for f in ["_0RAG_file", "keep/x.txt", "node_modules_notes.txt"] {
        w.write(&format!("src/{f}"), "copied");
    }
    let kept = ["_0RAG_file", "keep/x.txt", "node_modules_notes.txt"];

    // The preview lists only the files that are kept, and the dry run
    // copies nothing.
    let (code, lines) = w.run_json(&["--dry-run"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    let mut seen = 0;
    for l in lines.iter().filter(|l| l["event"] == "preview") {
        seen += 1;
        let mut new: Vec<&str> = l["new"]["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        new.sort();
        assert_eq!(new, kept, "{l}");
        assert_eq!(l["new"]["files"], 3, "{l}");
        assert_eq!(
            l["excludes"],
            serde_json::json!(["_0RAG/*", "node_modules"])
        );
        let effective: Vec<&str> = l["effective_excludes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        assert_eq!(
            effective[effective.len() - 3..],
            ["_0RAG/**", "node_modules", "node_modules/**"]
        );
        assert!(!effective.contains(&"_0RAG/*"));
    }
    assert_eq!(seen, 2, "{lines:#?}");
    assert!(contents(&w.path("dst")).is_empty());
    assert!(contents(&w.alias_dir()).is_empty());

    // The real run copies the same files. `jobs add` chose the ids.
    let (code, lines) = w.run_json(&["--yes"]);
    assert_eq!(code, Some(0), "{lines:#?}");
    let id_of = |kind: &str| {
        lines
            .iter()
            .find(|l| l["event"] == "start" && l["type"] == kind)
            .unwrap_or_else(|| panic!("no {kind} start: {lines:#?}"))["destination"]
            .as_str()
            .unwrap()
            .to_string()
    };
    for (id, dir) in [
        (id_of("local"), w.path("dst")),
        (id_of("onedrive"), w.alias_dir()),
    ] {
        let id = id.as_str();
        assert_eq!(summary_of(&lines, id)["outcome"], "ok", "{lines:#?}");
        let files: Vec<String> = contents(&dir).into_keys().collect();
        assert_eq!(files, kept, "{id}");
        assert!(!dir.join("_0RAG").exists(), "{id}");
        assert!(!dir.join("node_modules").exists(), "{id}");
    }
}

impl World {
    /// Rewrites the job's excludes to exactly these patterns.
    fn excludes(&self, patterns: &[&str]) {
        let path = self.path("config/config.toml");
        let text = fs::read_to_string(&path).unwrap();
        let list: Vec<String> = patterns
            .iter()
            .map(|p| serde_json::to_string(p).unwrap())
            .collect();
        let mut out = String::new();
        for line in text.lines() {
            if line.starts_with("excludes = ") {
                out.push_str(&format!("excludes = [{}]\n", list.join(", ")));
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        fs::write(path, out).unwrap();
    }
}

/// A two-way destination synced with one file of a folder excluded, then
/// the whole folder excluded with `X/*` (widened to `X/**`): the folder
/// drops out of both of bisync's listings, which deletes nothing on
/// either side, asks no folder question and leaves the remote's copies
/// where they are, while changes elsewhere still travel.
#[test]
fn excluding_a_synced_folder_on_two_way_deletes_nothing() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let (id, dst) = w.two_way(Kind::Alias);
    w.excludes(&["X/secret.txt"]);
    filler(&w);
    w.write("src/top.txt", "top");
    w.write("src/X/a.txt", "a");
    w.write("src/X/sub/b.txt", "b");
    w.write("src/X/secret.txt", "secret");
    let lines = w.sync_ok(id, &[]);
    assert!(!events(&lines, "resync", id).is_empty(), "{lines:#?}");
    let remote = contents(&dst);
    assert_eq!(remote["X/a.txt"], "a");
    assert_eq!(remote["X/sub/b.txt"], "b");
    assert!(!remote.contains_key("X/secret.txt"), "{remote:?}");
    let before_dst = remote;

    // The whole folder excluded; a change inside it stays local, one
    // outside it travels.
    w.excludes(&["X/*"]);
    tick();
    w.write("src/X/a.txt", "a, edited here only");
    w.write("src/elsewhere.txt", "elsewhere");
    for run in 0..2 {
        let lines = w.sync_ok(id, &[]);
        assert!(events(&lines, "resync", id).is_empty(), "{lines:#?}");
        assert!(events(&lines, "folder_prompt", id).is_empty(), "{lines:#?}");
        let plan = events(&lines, "plan", id)[0];
        assert_eq!(plan["delete_destination"], serde_json::json!([]), "{plan}");
        assert_eq!(plan["delete_local"], serde_json::json!([]), "{plan}");
        assert_eq!(plan["folders"], serde_json::json!([]), "{plan}");
        assert_eq!(plan["needs_confirmation"], serde_json::json!([]), "{plan}");
        // The plan's own list also has the conflict pattern.
        assert_eq!(plan["excludes"][0], "X/*", "{plan}");
        let effective = plan["effective_excludes"].as_array().unwrap();
        assert!(effective.iter().any(|p| p == "X/**"), "{plan}");
        assert!(!effective.iter().any(|p| p == "X/*"), "{plan}");
        assert_eq!(
            summary_of(&lines, id)["deleted"],
            serde_json::json!({"local": 0, "remote": 0}),
            "run {run}"
        );
        let src = contents(&w.path("src"));
        let remote = contents(&dst);
        for f in ["X/a.txt", "X/sub/b.txt"] {
            assert_eq!(remote.get(f), before_dst.get(f), "run {run} {f}");
        }
        assert!(!remote.contains_key("X/secret.txt"), "run {run}");
        assert_eq!(src["X/a.txt"], "a, edited here only");
        assert_eq!(src["X/sub/b.txt"], "b");
        assert_eq!(src["X/secret.txt"], "secret");
        assert_eq!(remote["elsewhere.txt"], "elsewhere", "run {run}");
        assert_eq!(remote["top.txt"], "top");
        assert_eq!(remote.len(), before_dst.len() + 1, "run {run}");
    }
}

/// `onefolder` with no arguments under a pseudo-terminal (script(1)): the
/// session takes the alternate screen once and leaves it once, the job
/// picker and the run's summary are drawn inside it, the shell's earlier
/// output (a seed line printed before OneFolder starts) is never printed
/// again, and after the session the transcript with the summary block is
/// printed on the normal screen.
#[test]
fn the_menu_session_stays_on_the_alternate_screen() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.config(&w.local_destination(LOCAL, &w.path("dst")));
    w.write("src/a.txt", "alpha");
    w.write("src/sub/b.txt", "bravo");
    const SEED: &str = "SEED-LINE-4242";
    let mut cmd = StdCommand::new("/usr/bin/script");
    for (k, v) in w.env() {
        cmd.env(k, v);
    }
    let shell = format!(
        "stty rows 30 cols 100; printf '%s\\n' {SEED}; exec {}",
        env!("CARGO_BIN_EXE_onefolder")
    );
    let mut child = KillOnDrop(
        cmd.env("TERM", "xterm-256color")
            .args(script_args(&shell))
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut stdout = child.stdout.take().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                seen.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        })
    };
    let text = || String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
    let deadline = Instant::now() + Duration::from_secs(90);
    // Waits until `want` has been shown `times` times in all.
    let wait = |want: &str, times: usize| loop {
        let t = text();
        if t.matches(want).count() >= times {
            break;
        }
        assert!(Instant::now() < deadline, "never saw {want:?}: {t}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut stdin = child.stdin.take().unwrap();
    let mut press = |keys: &[u8]| {
        use std::io::Write;
        std::thread::sleep(Duration::from_millis(400));
        stdin.write_all(keys).unwrap();
        stdin.flush().unwrap();
    };
    wait("Run a job", 1);
    press(b"\r");
    wait("Run which job?", 1);
    press(b"\r");
    wait("Continue?", 1);
    press(b"y");
    wait("esc back", 1);
    press(b"\x1b");
    std::thread::sleep(Duration::from_millis(1000));
    press(b"q");
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "the session did not end: {}",
            text()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    drop(stdin);
    let _ = reader.join();
    let all = text();
    assert!(status.success(), "{all}");
    assert_eq!(all.matches("\x1b[?1049h").count(), 1, "entered once: {all}");
    assert_eq!(all.matches("\x1b[?1049l").count(), 1, "left once: {all}");
    let enter = all.find("\x1b[?1049h").unwrap();
    let leave = all.find("\x1b[?1049l").unwrap();
    let (before, session, after) = (&all[..enter], &all[enter..leave], &all[leave..]);
    // The shell's line is printed once, before the session, and never again.
    assert!(before.contains(SEED), "{before}");
    assert_eq!(all.matches(SEED).count(), 1, "{all}");
    // The picker, the summary and its output screen are inside the session.
    assert!(session.contains("Run which job?"), "{session}");
    assert!(session.contains("Log:"), "{session}");
    assert!(!before.contains("Run which job?") && !after.contains("Run which job?"));
    assert!(!after.contains("Press any key"), "{after}");
    // The transcript: the summary block, on the normal screen.
    assert!(
        after.contains("Log:") && after.contains("complete"),
        "{after}"
    );
    assert!(after.contains("documents →"), "{after}");
    assert_eq!(contents(&w.path("dst")).len(), 2);
}

/// SIGTERM at the menu, before anything has run: the signal dispatcher is
/// already in place, so the exit hook puts the terminal back before
/// OneFolder exits 143. The alternate screen is left, the cursor shown
/// again and raw mode turned off (checked with `stty -a` afterwards, on
/// the same pseudo-terminal).
#[test]
fn sigterm_at_the_menu_restores_the_terminal() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.config(&w.local_destination(LOCAL, &w.path("dst")));
    let mut cmd = StdCommand::new("/usr/bin/script");
    for (k, v) in w.env() {
        cmd.env(k, v);
    }
    // OneFolder runs in the foreground under the shell, which reports its
    // exit status and the terminal's settings afterwards.
    let shell = format!(
        "stty rows 30 cols 100; {}; echo EXIT=$?; stty -a",
        env!("CARGO_BIN_EXE_onefolder")
    );
    let mut child = KillOnDrop(
        cmd.env("TERM", "xterm-256color")
            .args(script_args(&shell))
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut stdout = child.stdout.take().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let reader = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                seen.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        })
    };
    let text = || String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !text().contains("Run a job") {
        assert!(Instant::now() < deadline, "no menu: {}", text());
        std::thread::sleep(Duration::from_millis(100));
    }
    // script(1), then the shell, then OneFolder.
    let child_of = |parent: u32| -> String {
        let out = StdCommand::new("/usr/bin/pgrep")
            .args(["-P", &parent.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let shell_pid = child_of(child.id());
    let pid = child_of(shell_pid.parse().unwrap());
    std::thread::sleep(Duration::from_millis(300));
    let killed = StdCommand::new("/bin/kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(killed.success(), "{}", text());
    let stdin = child.stdin.take().unwrap();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "did not end: {}", text());
        std::thread::sleep(Duration::from_millis(100));
    };
    drop(stdin);
    let _ = reader.join();
    let all = text();
    assert!(status.success(), "{all}");
    assert!(all.contains("EXIT=143"), "{all}");
    let enter = all
        .find("\x1b[?1049h")
        .expect("entered the alternate screen");
    let leave = all.rfind("\x1b[?1049l").expect("left the alternate screen");
    assert!(enter < leave, "{all}");
    let hidden = all.rfind("\x1b[?25l").unwrap_or(0);
    let shown = all.rfind("\x1b[?25h").expect("cursor shown again");
    assert!(shown > hidden, "the cursor is shown last: {all}");
    let stty = &all[all.find("EXIT=143").unwrap()..];
    assert!(
        stty.contains(" icanon") && !stty.contains("-icanon"),
        "{stty}"
    );
    assert!(
        stty.contains(" echo ") && !stty.contains("-echo "),
        "{stty}"
    );
}

/// The job form's folder fields inside the session (`onefolder` with no
/// arguments, Jobs, `a`): Tab completes the source to a scratch folder,
/// `b` opens the folder browser there, Enter drills into a folder whose
/// size is measured on screen, Space chooses its subfolder, and the form
/// comes back with it in the field, as saving the job shows.
#[test]
fn the_source_is_completed_and_browsed_in_the_job_form() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.config(&w.local_destination(LOCAL, &w.path("dst")));
    w.write("pick/alpha/deep/twelve.bin", &"x".repeat(12_000));
    w.write("pick/beta/seven.bin", &"y".repeat(7_000));
    let path = format!("{}:/usr/bin:/bin", w.rclone_dir.display());
    let mut p = Pty::start(&w, &path);
    p.wait("Run a job");
    p.press(b"j\r");
    p.wait("a add");
    p.press(b"a");
    p.wait("Add job");
    // The name, then the source: typed in part and completed with Tab.
    p.press(b"\r");
    p.press(b"picked\r");
    p.press(b"\t");
    p.wait("b browse");
    p.press(b"\r");
    let typed = format!("{}/pi", w.root.display());
    p.press(typed.as_bytes());
    p.press(b"\t");
    p.wait("1 of 1: [pick]");
    p.wait(&format!("{}/pick/", w.root.display()));
    p.press(b"\r");
    // Browse from there, drill into alpha and choose deep.
    p.press(b"b");
    p.wait("Choose a folder");
    p.wait("alpha");
    p.wait("12 KB");
    p.press(b"\r");
    p.wait("deep");
    p.wait("12 KB");
    p.press(b" ");
    // Back in the form, with the folder in the field.
    let chosen = w.path("pick/alpha/deep").display().to_string();
    p.wait("Add job");
    p.wait(&chosen);
    // Ctrl-S saves the job with it.
    p.press(b"\x13");
    let config = w.path("config/config.toml");
    let want = format!("source = \"{chosen}\"");
    loop {
        let saved = fs::read_to_string(&config).unwrap();
        if saved.contains("name = \"picked\"") {
            assert!(saved.contains(&want), "{saved}");
            break;
        }
        assert!(
            Instant::now() < p.deadline,
            "not saved: {}",
            p.screen().join("\n")
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Out of the jobs list and the menu.
    p.press(b"\x1b");
    p.wait("Run a job");
    p.press(b"q");
    let all = p.finish();
    assert!(all.contains("EXIT=0"), "{all}");
}

/// `onefolder` in a pseudo-terminal, for the folder browser tests: the
/// shell runs it as a child (not with exec), so OneFolder is not the
/// session leader and its children would outlive it unless it ends them.
struct Pty {
    child: KillOnDrop,
    stdin: std::process::ChildStdin,
    seen: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    deadline: Instant,
}

impl Pty {
    fn start(w: &World, path: &str) -> Pty {
        Pty::start_with(w, path, "")
    }

    /// `onefolder` with `args` (a shell word list) in the pseudo-terminal.
    fn start_with(w: &World, path: &str, args: &str) -> Pty {
        let mut cmd = StdCommand::new("/usr/bin/script");
        for (k, v) in w.env() {
            cmd.env(k, v);
        }
        cmd.env("PATH", path);
        let shell = format!(
            "stty rows 30 cols 120; {} {args}; echo EXIT=$?; sleep 5",
            env!("CARGO_BIN_EXE_onefolder")
        );
        let mut child = KillOnDrop(
            cmd.env("TERM", "xterm-256color")
                .args(script_args(&shell))
                .env("SHELL", "/bin/sh")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let reader = {
            let seen = seen.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    seen.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            })
        };
        Pty {
            child,
            stdin,
            seen,
            reader: Some(reader),
            deadline: Instant::now() + Duration::from_secs(90),
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.seen.lock().unwrap()).into_owned()
    }

    /// The screen as it stands, one string per row: the output replayed
    /// on a 30 by 120 grid (cursor moves, clears, line ends; colours and
    /// modes ignored). ratatui writes only the cells that changed, so the
    /// raw output cannot be searched for a phrase.
    fn screen(&self) -> Vec<String> {
        let (rows, cols) = (30usize, 120usize);
        let mut grid = vec![vec![' '; cols]; rows];
        let (mut r, mut c) = (0usize, 0usize);
        let t = self.text();
        let mut chars = t.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\x1b' => {
                    if chars.peek() != Some(&'[') {
                        chars.next();
                        continue;
                    }
                    chars.next();
                    let mut params = String::new();
                    let mut last = ' ';
                    for p in chars.by_ref() {
                        if p.is_ascii_alphabetic() || p == '~' || p == '@' {
                            last = p;
                            break;
                        }
                        params.push(p);
                    }
                    let nums: Vec<usize> = params
                        .split(';')
                        .map(|n| n.trim_start_matches('?').parse().unwrap_or(0))
                        .collect();
                    match last {
                        'H' | 'f' => {
                            r = nums.first().copied().unwrap_or(1).max(1) - 1;
                            c = nums.get(1).copied().unwrap_or(1).max(1) - 1;
                        }
                        'J' if nums.first() == Some(&2) => {
                            grid = vec![vec![' '; cols]; rows];
                        }
                        'h' | 'l' if params.starts_with("?1049") => {
                            grid = vec![vec![' '; cols]; rows];
                        }
                        _ => {}
                    }
                }
                '\r' => c = 0,
                '\n' => r = (r + 1).min(rows - 1),
                ch if ch.is_control() => {}
                ch => {
                    if r < rows && c < cols {
                        grid[r][c] = ch;
                    }
                    c += 1;
                }
            }
        }
        grid.into_iter()
            .map(|row| row.into_iter().collect::<String>().trim_end().to_string())
            .collect()
    }

    /// Waits until `want` is on the screen.
    fn wait(&self, want: &str) {
        while !self.screen().iter().any(|row| row.contains(want)) {
            assert!(
                Instant::now() < self.deadline,
                "never saw {want:?}:\n{}",
                self.screen().join("\n")
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Waits until `want` is in the output, as printed after the session.
    fn wait_printed(&self, want: &str) {
        while !self.text().contains(want) {
            assert!(Instant::now() < self.deadline, "never printed {want:?}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn press(&mut self, keys: &[u8]) {
        use std::io::Write;
        std::thread::sleep(Duration::from_millis(400));
        self.stdin.write_all(keys).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Waits for the shell to finish and gives back everything shown.
    fn finish(mut self) -> String {
        loop {
            if self.child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                Instant::now() < self.deadline,
                "did not end: {}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        self.text()
    }
}

/// The processes whose command line contains `marker`.
fn processes(marker: &str) -> usize {
    let out = StdCommand::new("/usr/bin/pgrep")
        .args(["-f", marker])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().count()
}

/// A folder `rclone` first on PATH that hands everything to the real one,
/// except `rclone size` of a folder named `Slow…`, which hangs as
/// `sleep <marker>` (exec'd, so it is the process OneFolder started).
fn slow_size_path(w: &World, marker: &str) -> String {
    let bin = w.path("fakebin");
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join("rclone");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$*\" in *size*Slow*) exec /bin/sleep {marker};; esac\nexec {} \"$@\"\n",
            w.rclone_dir.join("rclone").display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}:/usr/bin:/bin", bin.display(), w.rclone_dir.display())
}

/// Opens the folder browser on the OneDrive folder of a new destination
/// in the job form, from the menu, and waits for the folders to be
/// listed: `Measured13` (12 KB, measured by the real rclone) and
/// `Slow13`, whose `rclone size` hangs until it is killed.
fn browse_onedrive(w: &World, marker: &str) -> Pty {
    w.config(&w.local_destination(LOCAL, &w.path("dst")));
    w.write("remote/Measured13/twelve.bin", &"x".repeat(12_000));
    w.write("remote/Slow13/one.bin", "y");
    let mut p = Pty::start(w, &slow_size_path(w, marker));
    p.wait("Run a job");
    p.press(b"j\r");
    p.wait("a add");
    p.press(b"a");
    p.wait("Add job");
    p.press(b"\t\t\t\t");
    p.press(b"a");
    p.wait("New destination");
    p.press(b"\t\t");
    p.press(b"b");
    p.wait("Choose a folder");
    p.wait("Slow13");
    p.wait("12 KB");
    let deadline = Instant::now() + Duration::from_secs(20);
    while processes(&format!("sleep {marker}")) == 0 {
        assert!(Instant::now() < deadline, "the slow size never started");
        std::thread::sleep(Duration::from_millis(100));
    }
    p
}

/// The OneDrive browser measures folders with rclone in the background,
/// and Esc back to the form kills the size still running.
#[test]
fn leaving_the_onedrive_browser_ends_its_rclone_sizes() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let marker = "9813.25";
    let mut p = browse_onedrive(&w, marker);
    p.press(b"\x1b");
    p.wait("New destination");
    let deadline = Instant::now() + Duration::from_secs(10);
    while processes(&format!("sleep {marker}")) > 0 {
        assert!(
            Instant::now() < deadline,
            "rclone size outlived the browser"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Out: the destination form, the job form (asks first), the list, the menu.
    p.press(b"\x1b");
    p.press(b"\x1b");
    p.press(b"\x1b");
    p.press(b"\x1b");
    p.press(b"q");
    let all = p.finish();
    assert!(all.contains("EXIT=0"), "{all}");
}

/// Ctrl-C in the OneDrive browser quits OneFolder (130), and the
/// `rclone size` still running goes with it.
#[test]
fn ctrl_c_in_the_onedrive_browser_leaves_no_rclone_behind() {
    if !enabled() {
        return;
    }
    let w = World::new();
    let marker = "9813.75";
    let mut p = browse_onedrive(&w, marker);
    p.press(b"\x03");
    p.wait_printed("EXIT=130");
    // OneFolder has exited; the shell is still there (sleeping), so
    // nothing has been hung up yet: any survivor would be visible now.
    std::thread::sleep(Duration::from_millis(500));
    let left = processes(&format!("sleep {marker}"));
    let all = p.finish();
    assert_eq!(left, 0, "rclone size outlived OneFolder: {all}");
}

// ------------------------------------------------------- several jobs

impl World {
    /// Three jobs, run one after another: `documents` (the usual local
    /// destination), `records` (a local destination, then one on a drive
    /// that is not mounted) and `photos` (a local destination).
    fn three_jobs(&self) {
        for d in ["src2", "src3", "dst2", "dst3", "Backup/Records"] {
            fs::create_dir_all(self.path(d)).unwrap();
        }
        let text = format!(
            r#"version = 1

[[jobs]]
name = "documents"
source = "{src}"
{documents}
[[jobs]]
name = "records"
source = "{src2}"
{records}
  [[jobs.destinations]]
  id = "d-0003"
  type = "local"
  path = "{backup}/Records"
  volume_uuid = "A1B2C3D4-0000-4E5F-8A9B-000000000001"
  volume_name = "Backup"
  mount_point = "{backup}"

[[jobs]]
name = "photos"
source = "{src3}"
{photos}"#,
            src = self.path("src").display(),
            src2 = self.path("src2").display(),
            src3 = self.path("src3").display(),
            backup = self.path("Backup").display(),
            documents = self.local_destination(LOCAL, &self.path("dst")),
            records = self.local_destination("d-0002", &self.path("dst2")),
            photos = self.local_destination("d-0004", &self.path("dst3")),
        );
        fs::write(self.path("config/config.toml"), text).unwrap();
        self.write("src/a.txt", "a");
        self.write("src2/b.txt", "b");
        self.write("src3/c.txt", "c");
    }

    /// A live process whose name says OneFolder holds `job`'s run lock, as
    /// another run of it would. The lock goes with the returned guard.
    fn hold_lock(&self, job: &str) -> KillOnDrop {
        // A link, so ps names the process after it (a copy of a system
        // binary is not allowed to run on macOS).
        let holder = self.path("onefolder-holder");
        if !holder.exists() {
            std::os::unix::fs::symlink("/bin/sleep", &holder).unwrap();
        }
        let child = StdCommand::new(&holder).arg("120").spawn().unwrap();
        fs::create_dir_all(self.path("data/runs")).unwrap();
        fs::write(
            self.path(&format!("data/runs/{job}.lock")),
            format!(r#"{{"pid":{}}}"#, child.id()),
        )
        .unwrap();
        KillOnDrop(child)
    }
}

/// `run --all`: each job in turn, in config order, with the batch's place
/// on every event. A destination whose drive is not mounted fails and
/// the next one runs; a job whose run lock is held is skipped and its
/// lock left alone; the batch ends with `batch-done` and exit code 1.
#[test]
fn a_batch_runs_each_job_in_turn() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.three_jobs();
    let _held = w.hold_lock("photos");
    let out = w
        .onefolder()
        .args(["run", "--all", "--yes", "--json"])
        .output()
        .unwrap();
    let lines = ndjson(&out.stdout);
    for l in &lines {
        check_shape(l);
    }
    assert_eq!(out.status.code(), Some(1), "{lines:#?}");
    // Every line but the last carries the batch, jobs in config order.
    let places: Vec<(String, u64)> = lines[..lines.len() - 1]
        .iter()
        .map(|l| {
            assert_eq!(l["batch"]["of"], 3, "{l}");
            (
                l["job"].as_str().unwrap().to_string(),
                l["batch"]["index"].as_u64().unwrap(),
            )
        })
        .collect();
    let mut seen: Vec<(String, u64)> = places.clone();
    seen.dedup();
    assert_eq!(
        seen,
        [
            ("documents".to_string(), 1),
            ("records".to_string(), 2),
            ("photos".to_string(), 3)
        ]
    );
    assert_eq!(summary_of(&lines, LOCAL)["outcome"], "ok");
    assert_eq!(summary_of(&lines, "d-0002")["outcome"], "ok");
    assert_eq!(summary_of(&lines, "d-0003")["outcome"], "failed");
    let skipped: Vec<&Value> = lines
        .iter()
        .filter(|l| l["event"] == "job-skipped")
        .collect();
    assert_eq!(skipped.len(), 1, "{lines:#?}");
    assert_eq!(skipped[0]["job"], "photos");
    assert_eq!(skipped[0]["code"], "locked");
    assert_eq!(skipped[0]["reason"], "another run in progress");
    // One `done` per job that ran.
    let dones: Vec<&Value> = lines.iter().filter(|l| l["event"] == "done").collect();
    assert_eq!(dones.len(), 2);
    assert_eq!(dones[1]["exit_code"], 1);
    let last = lines.last().unwrap();
    assert_eq!(
        *last,
        serde_json::json!({"event": "batch-done", "ok": false, "exit_code": 1, "jobs": [
            {"job": "documents", "outcome": "ok", "reason": null, "destinations": [
                {"destination": LOCAL, "outcome": "ok", "reason": null}]},
            {"job": "records", "outcome": "partial", "reason": null, "destinations": [
                {"destination": "d-0002", "outcome": "ok", "reason": null},
                {"destination": "d-0003", "outcome": "failed",
                 "reason": "Backup drive not mounted"}]},
            {"job": "photos", "outcome": "skipped", "reason": "another run in progress",
             "destinations": [{"destination": "d-0004", "outcome": "skipped",
                               "reason": "another run in progress"}]},
        ]})
    );
    assert!(w.path("dst/a.txt").exists() && w.path("dst2/b.txt").exists());
    assert!(!w.path("dst3/c.txt").exists(), "the held job did not run");
    assert!(!w.path("Backup/Records/b.txt").exists());
    // Each lock went with its job; the held one is untouched.
    assert!(!w.path("data/runs/documents.lock").exists());
    assert!(!w.path("data/runs/records.lock").exists());
    assert!(w.path("data/runs/photos.lock").exists());
    assert_no_rclone_left(&w);
}

/// The same batch with plain output: `Job 2 of 3: records` before each
/// job, named in any order but run in config order, and the closing
/// summary of every job.
#[test]
fn a_batch_in_plain_text() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.three_jobs();
    let _held = w.hold_lock("photos");
    let out = w
        .onefolder()
        .args(["run", "photos", "records", "documents", "--yes"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("no {needle:?} in:\n{text}"))
    };
    assert!(
        at("Job 1 of 3: documents\nRun documents\n") < at("Job 2 of 3: records\nRun records\n")
    );
    assert!(
        at("Job 2 of 3: records") < at("Job 3 of 3: photos\n◎ skipped · another run in progress\n")
    );
    // records' own closing summary, then the batch's.
    assert!(
        at("records   2 destinations · 1 ok · 1 failed")
            < at("Several jobs   3 jobs · 3 destinations")
    );
    let closing = &text[at("Several jobs   3 jobs")..];
    for line in [
        "  documents   ✓ ok",
        "  records     ◎ partial",
        "  photos      ◎ skipped · another run in progress",
    ] {
        assert!(closing.contains(line), "{line:?} in:\n{closing}");
    }
    assert!(
        closing.contains("one-way   ✗ failed · Backup drive not mounted"),
        "{closing}"
    );
    assert!(
        text.ends_with("======================================================================\n"),
        "{text}"
    );

    // --dest belongs to one job; an unknown job refuses the whole batch.
    let out = w
        .onefolder()
        .args(["run", "documents", "records", "--dest", LOCAL, "--yes"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--dest runs one destination of one job"));
    fs::remove_file(w.path("dst/a.txt")).unwrap();
    let out = w
        .onefolder()
        .args(["run", "documents", "nope", "--yes", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let lines = ndjson(&out.stdout);
    assert_eq!(lines[0]["code"], "unknown-job");
    assert_eq!(lines[1]["event"], "batch-done");
    assert!(!w.path("dst/a.txt").exists(), "nothing ran");
}

/// Ctrl-C during the first job of a batch stops it as it stops a run of
/// it alone; the jobs not yet started are not run, and say so.
#[test]
fn ctrl_c_stops_the_batch() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.three_jobs();
    let big = fs::File::create(w.path("src/big.bin")).unwrap();
    big.set_len(64 * 1024 * 1024).unwrap();
    drop(big);
    let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_onefolder"));
    for (k, v) in w.env() {
        cmd.env(k, v);
    }
    let mut child = cmd
        .env("RCLONE_BWLIMIT", "2M")
        .env("RCLONE_DISABLE", "copy")
        .args(["run", "--all", "--yes", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "the run ended early"
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        lines.push(v.clone());
        if v["event"] == "stats" && v["bytes"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "no progress");
    }
    StdCommand::new("/bin/kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    for line in reader.lines() {
        lines.push(serde_json::from_str(&line.unwrap()).unwrap());
    }
    let status = child.wait().unwrap();
    for l in &lines {
        check_shape(l);
    }
    assert_eq!(status.code(), Some(1), "{lines:#?}");
    assert_eq!(summary_of(&lines, LOCAL)["outcome"], "cancelled");
    let skipped: Vec<(&str, &str)> = lines
        .iter()
        .filter(|l| l["event"] == "job-skipped")
        .map(|l| (l["job"].as_str().unwrap(), l["code"].as_str().unwrap()))
        .collect();
    assert_eq!(skipped, [("records", "stopped"), ("photos", "stopped")]);
    let last = lines.last().unwrap();
    assert_eq!(last["event"], "batch-done");
    assert_eq!(last["exit_code"], 1);
    let jobs = last["jobs"].as_array().unwrap();
    assert_eq!(jobs[0]["outcome"], "stopped");
    for j in &jobs[1..] {
        assert_eq!(j["outcome"], "skipped");
        assert_eq!(j["reason"], "not run, the batch was stopped");
        for d in j["destinations"].as_array().unwrap() {
            assert_eq!(d["reason"], "not run, the batch was stopped");
        }
    }
    assert!(!w.path("dst2/b.txt").exists() && !w.path("dst3/c.txt").exists());
    assert!(!w.path("data/runs/documents.lock").exists());
    assert_no_rclone_left(&w);
}

impl World {
    /// Two jobs: `documents` (src to dst, one new file) and `photos` (src3
    /// to dst3, one new file and one file moved locally whose old copy is
    /// on the destination).
    fn two_jobs_with_a_move(&self) {
        for d in ["src3", "dst3"] {
            fs::create_dir_all(self.path(d)).unwrap();
        }
        let text = format!(
            "version = 1\n\n[[jobs]]\nname = \"documents\"\nsource = \"{}\"\n{}\n[[jobs]]\n\
             name = \"photos\"\nsource = \"{}\"\n{}",
            self.path("src").display(),
            self.local_destination(LOCAL, &self.path("dst")),
            self.path("src3").display(),
            self.local_destination("d-0004", &self.path("dst3")),
        );
        fs::write(self.path("config/config.toml"), text).unwrap();
        self.write("src/a.txt", "a");
        self.write("src3/m.txt", "a file that moves, long enough to hash");
        self.seed(&self.path("src3"), &self.path("dst3"));
        fs::create_dir_all(self.path("src3/moved")).unwrap();
        fs::rename(self.path("src3/m.txt"), self.path("src3/moved/m.txt")).unwrap();
        self.write("src3/c.txt", "c");
    }
}

/// `run --all` on a terminal: `a` at the first preview is yes to the
/// rest, so the second job runs without its confirmation, while its
/// question about moved files still asks. A stale run lock removed for
/// the second job is said in the text printed afterwards, with the batch
/// lines and the closing summary.
#[test]
fn yes_to_the_rest_of_a_batch_on_a_terminal() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.two_jobs_with_a_move();
    fs::create_dir_all(w.path("data/runs")).unwrap();
    fs::write(w.path("data/runs/photos.lock"), r#"{"pid":99999999}"#).unwrap();
    let path = format!("{}:/usr/bin:/bin", w.rclone_dir.display());
    let mut p = Pty::start_with(&w, &path, "run --all");
    p.wait("Several jobs · job 1 of 2 · documents · photos next");
    p.wait("Continue? [Y/n/a]   n skips this destination · a yes to the rest of the batch");
    p.press(b"a");
    p.wait("Several jobs · job 2 of 2 · documents ✓");
    p.wait("instead of uploading them again? [Y/n]");
    p.press(b"y");
    // No other question: the batch ends on its own.
    p.wait_printed("Several jobs   2 jobs · 2 destinations");
    let all = p.finish();
    assert!(all.contains("EXIT=0"), "{all}");
    assert!(
        all.contains("Removed a stale run lock left by process 99999999"),
        "{all}"
    );
    assert!(w.path("dst/a.txt").exists());
    assert!(w.path("dst3/c.txt").exists());
    assert!(w.path("dst3/moved/m.txt").exists(), "moved, as answered");
    assert!(!w.path("dst3/m.txt").exists());
    assert!(!w.path("data/runs/photos.lock").exists());
}

/// `run --all --dry-run`: every job's preview, nothing changed, nothing
/// asked; a batch in which no job ran a destination exits 2; a name given
/// twice is a run of that job alone; `--all` with one job is still a
/// batch.
#[test]
fn a_batch_dry_run_and_a_batch_where_nothing_ran() {
    if !enabled() {
        return;
    }
    let w = World::new();
    w.three_jobs();
    let out = w
        .onefolder()
        .args(["run", "--all", "--dry-run", "--json"])
        .output()
        .unwrap();
    let lines = ndjson(&out.stdout);
    for l in &lines {
        check_shape(l);
    }
    assert_eq!(
        out.status.code(),
        Some(1),
        "the drive check fails: {lines:#?}"
    );
    for id in [LOCAL, "d-0002", "d-0004"] {
        let s = summary_of(&lines, id);
        assert_eq!(s["dry_run"], true);
        assert_eq!(s["outcome"], "ok");
    }
    assert_eq!(lines.last().unwrap()["event"], "batch-done");
    for f in ["dst/a.txt", "dst2/b.txt", "dst3/c.txt"] {
        assert!(!w.path(f).exists(), "{f} copied by a dry run");
    }
    // Plain text: the previews, the closing summary marks the dry run.
    let out = w
        .onefolder()
        .args(["run", "--all", "--dry-run"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Several jobs   3 jobs · 4 destinations · dry run"),
        "{text}"
    );

    // Every job skipped: nothing ran, exit 2.
    let _held = [
        w.hold_lock("documents"),
        w.hold_lock("records"),
        w.hold_lock("photos"),
    ];
    let out = w
        .onefolder()
        .args(["run", "--all", "--yes", "--json"])
        .output()
        .unwrap();
    let lines = ndjson(&out.stdout);
    assert_eq!(out.status.code(), Some(2), "{lines:#?}");
    let last = lines.last().unwrap();
    assert_eq!(last["event"], "batch-done");
    assert_eq!(last["exit_code"], 2);
    drop(_held);

    // A name twice is one job, with no batch.
    let out = w
        .onefolder()
        .args(["run", "photos", "photos", "--yes", "--json"])
        .output()
        .unwrap();
    let lines = ndjson(&out.stdout);
    assert!(lines.iter().all(|l| l.get("batch").is_none()), "{lines:#?}");
    assert_eq!(lines.last().unwrap()["event"], "done");
    // `--all` with one job is a batch of one.
    w.config(&w.local_destination(LOCAL, &w.path("dst")));
    let out = w
        .onefolder()
        .args(["run", "--all", "--yes", "--json"])
        .output()
        .unwrap();
    let lines = ndjson(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{lines:#?}");
    assert_eq!(lines[0]["batch"], serde_json::json!({"index": 1, "of": 1}));
    assert_eq!(lines.last().unwrap()["event"], "batch-done");
}
