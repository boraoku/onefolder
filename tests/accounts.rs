//! End-to-end tests of `onefolder accounts` against the real rclone, with a
//! private `RCLONE_CONFIG` whose `onefolder-test` remote is an alias of a
//! temporary folder. `rclone about` works on an alias of a local path, so
//! the same health check that talks to OneDrive runs here without a
//! Microsoft account. Skipped (with a note) when rclone is not installed,
//! except under CI or with `ONEFOLDER_INTEGRATION=1`, where a missing
//! rclone fails the run instead of hiding these tests.
//!
//! The sign-in flows (`add`, `reconnect`) run under a pseudo-terminal
//! (`script`) with a fake `rclone` first on PATH that turns
//! `config create <name> onedrive ...` into an alias remote and logs every
//! call, so no browser or Microsoft account is involved.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// The directory holding a real rclone, if there is one.
fn rclone_dir() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .find(|dir| dir.join("rclone").is_file())
}

/// A temporary home: OneFolder's config and data directories, a private
/// rclone config, and the folder the alias remotes point at.
struct Home {
    dir: TempDir,
    rclone_dir: PathBuf,
}

impl Home {
    /// `None` (and a note on stderr) when rclone is not installed.
    fn new() -> Option<Home> {
        let Some(rclone_dir) = rclone_dir() else {
            let strict = ["CI", "ONEFOLDER_INTEGRATION"]
                .iter()
                .any(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()));
            assert!(
                !strict,
                "rclone is not installed, and CI or ONEFOLDER_INTEGRATION is set"
            );
            eprintln!("skipped: rclone is not installed");
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("config")).unwrap();
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/hello.txt"), "hello").unwrap();
        let home = Home { dir, rclone_dir };
        home.write_rclone_conf(&[("onefolder-test", &home.path("target"))]);
        Some(home)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn rclone_conf(&self) -> PathBuf {
        self.path("rclone.conf")
    }

    /// Writes alias remotes into the private rclone config.
    fn write_rclone_conf(&self, remotes: &[(&str, &Path)]) {
        let text: String = remotes
            .iter()
            .map(|(name, target)| {
                format!("[{name}]\ntype = alias\nremote = {}\n\n", target.display())
            })
            .collect();
        fs::write(self.rclone_conf(), text).unwrap();
    }

    fn write_config(&self, text: &str) {
        fs::write(self.path("config/config.toml"), text).unwrap();
    }

    fn config(&self) -> String {
        fs::read_to_string(self.path("config/config.toml")).unwrap()
    }

    fn cmd(&self) -> Command {
        let mut cmd = cargo_bin_cmd!("onefolder");
        cmd.env("ONEFOLDER_CONFIG_DIR", self.path("config"))
            .env("ONEFOLDER_DATA_DIR", self.path("data"))
            .env("RCLONE_CONFIG", self.rclone_conf())
            .env("NO_COLOR", "1")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.rclone_dir.display()),
            );
        cmd
    }
}

const TEST_ACCOUNT: &str = r#"version = 1

[[accounts]]
label = "test"
remote = "onefolder-test"
drive_type = "personal"
added = 2026-10-13T09:40:00+11:00
"#;

fn account(label: &str) -> String {
    format!("\n[[accounts]]\nlabel = \"{label}\"\nremote = \"onefolder-{label}\"\ndrive_type = \"business\"\n")
}

fn job_using(account: &str) -> String {
    format!(
        "\n[[jobs]]\nname = \"documents\"\nsource = \"/Users/someone/Documents\"\n\n  [[jobs.destinations]]\n  id = \"d-7f3a\"\n  type = \"onedrive\"\n  account = \"{account}\"\n  path = \"Backups/Documents\"\n  mode = \"two-way\"\n\n  [[jobs.destinations]]\n  id = \"d-91c0\"\n  type = \"local\"\n  path = \"/tmp/onefolder-backup\"\n"
    )
}

fn json(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).expect("stdout is one JSON document")
}

#[test]
fn list_marks_missing_remotes() {
    let Some(home) = Home::new() else { return };
    home.write_config(&format!("{TEST_ACCOUNT}{}", account("gone")));

    let out = home
        .cmd()
        .args(["accounts", "list", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out.stdout);
    assert_eq!(v["command"], "accounts list");
    assert_eq!(v["ok"], true);
    let data = v["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["label"], "test");
    assert_eq!(data[0]["remote"], "onefolder-test");
    assert_eq!(data[0]["drive_type"], "personal");
    assert_eq!(data[0]["added"], "2026-10-13T09:40:00+11:00");
    assert_eq!(data[0]["delta"], false);
    assert_eq!(data[0]["remote_exists"], true);
    assert_eq!(data[1]["label"], "gone");
    assert_eq!(data[1]["remote_exists"], false);

    // Bare `accounts` is the list.
    home.cmd()
        .arg("accounts")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("Accounts  2 accounts\n"))
        .stdout(predicate::str::contains(
            "  test        personal   onefolder-test   added 13 Oct 2026\n",
        ))
        .stdout(predicate::str::contains(
            "✗ remote missing from rclone's config",
        ))
        .stdout(predicate::str::contains(
            "run: onefolder accounts reconnect gone",
        ));
}

#[test]
fn list_without_a_config_is_empty() {
    let Some(home) = Home::new() else { return };
    let out = home.cmd().args(["accounts", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json(&out.stdout)["data"], serde_json::json!([]));
    home.cmd()
        .args(["accounts", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "none yet · add one with: onefolder accounts add",
        ));
}

#[test]
fn test_passes_against_the_alias() {
    let Some(home) = Home::new() else { return };
    home.write_config(TEST_ACCOUNT);
    home.cmd()
        .args(["accounts", "test"])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(
                r"  ✓ test        personal   connected · .+ used of .+ · .+ free\n",
            )
            .unwrap(),
        )
        .stdout(predicate::str::contains("1 of 1 account connected"));

    let out = home
        .cmd()
        .args(["accounts", "test", "test", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out.stdout);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"][0]["status"], "connected");
    assert!(v["data"][0]["quota"]["free"].as_u64().is_some());
}

#[test]
fn test_fails_for_a_broken_or_missing_remote() {
    let Some(home) = Home::new() else { return };
    home.write_rclone_conf(&[
        ("onefolder-test", &home.path("target")),
        ("onefolder-broken", &home.path("not-there")),
    ]);
    home.write_config(&format!(
        "{TEST_ACCOUNT}{}{}",
        account("broken"),
        account("gone")
    ));

    home.cmd()
        .args(["accounts", "test"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("✓ test "))
        .stdout(predicate::str::contains(
            "✗ broken      business   not reachable: about call failed: directory not found",
        ))
        .stdout(predicate::str::contains(
            "✗ gone        business   remote missing from rclone's config",
        ))
        .stdout(predicate::str::contains(
            "run: onefolder accounts reconnect gone",
        ))
        .stdout(predicate::str::contains("1 of 3 accounts connected"));

    let out = home
        .cmd()
        .args(["accounts", "test", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out.stdout);
    assert_eq!(v["ok"], false);
    assert_eq!(v["data"][0]["status"], "connected");
    assert_eq!(v["data"][1]["status"], "failed");
    assert_eq!(v["data"][1]["ok"], false);
    assert_eq!(v["data"][2]["status"], "remote-missing");

    // One account by name; an unknown name is refused.
    home.cmd()
        .args(["accounts", "test", "test"])
        .assert()
        .success();
    // Every account failing is exit 2, as a script needs to tell apart.
    home.cmd()
        .args(["accounts", "test", "broken"])
        .assert()
        .code(2);
    home.cmd()
        .args(["accounts", "test", "nobody"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "There is no account called nobody. The accounts are: test, broken, gone.",
        ));
}

#[test]
fn remove_deletes_the_remote_and_the_entry() {
    let Some(home) = Home::new() else { return };
    home.write_config(&format!("{TEST_ACCOUNT}{}", account("other")));

    // A dry run changes nothing.
    home.cmd()
        .args(["accounts", "remove", "test", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Dry run: would delete the rclone remote onefolder-test",
        ));
    assert!(fs::read_to_string(home.rclone_conf())
        .unwrap()
        .contains("[onefolder-test]"));
    assert!(home.config().contains("label = \"test\""));

    // Without a terminal, never silently: --yes (or --force) is needed.
    home.cmd()
        .args(["accounts", "remove", "test"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "accounts remove cannot ask for confirmation here (no terminal)",
        ));
    assert!(home.config().contains("label = \"test\""));

    home.cmd()
        .args(["accounts", "remove", "test", "--yes"])
        .assert()
        .success()
        .stdout("✓ Removed test and its rclone remote onefolder-test\n");
    assert!(!fs::read_to_string(home.rclone_conf())
        .unwrap()
        .contains("onefolder-test"));
    let config = home.config();
    assert!(!config.contains("label = \"test\""), "{config}");
    assert!(config.contains("label = \"other\""), "{config}");
    // The folder behind the alias is untouched.
    assert!(home.path("target/hello.txt").exists());

    // --json alone is not consent.
    let out = home
        .cmd()
        .args(["accounts", "remove", "other", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json(&out.stdout)["error"]["code"], "needs-yes");
    assert!(home.config().contains("label = \"other\""));

    // An entry whose remote is already gone is still removed.
    let out = home
        .cmd()
        .args(["accounts", "remove", "other", "--json", "--yes"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json(&out.stdout);
    assert_eq!(v["data"]["remote_was_missing"], true);
    assert_eq!(v["data"]["remote_deleted"], false);
    assert!(!home.config().contains("other"));
}

#[test]
fn remove_refuses_an_account_a_job_uses() {
    let Some(home) = Home::new() else { return };
    home.write_config(&format!("{TEST_ACCOUNT}{}", job_using("test")));

    home.cmd()
        .args(["accounts", "remove", "test"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "test is still used by a job destination: documents (test:Backups/Documents)",
        ))
        .stderr(predicate::str::contains(
            "onefolder accounts remove test --force",
        ));
    let out = home
        .cmd()
        .args(["accounts", "remove", "test", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v = json(&out.stdout);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "account-in-use");
    assert_eq!(v["data"]["destinations"][0]["id"], "d-7f3a");
    assert!(fs::read_to_string(home.rclone_conf())
        .unwrap()
        .contains("[onefolder-test]"));
    assert!(home.config().contains("label = \"test\""));

    // --force deletes the destinations that used it, keeps the rest of the
    // job, and drops their run records.
    fs::create_dir_all(home.path("data")).unwrap();
    fs::write(
        home.path("data/state.json"),
        r#"{"version": 1,
            "holds": {"documents/d-7f3a": ["Old"], "documents/d-91c0": ["Keep"]},
            "bisync": {"documents/d-7f3a": {"initialised": true}}}"#,
    )
    .unwrap();
    home.cmd()
        .args(["accounts", "remove", "test", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "✓ Removed test and its rclone remote onefolder-test",
        ))
        .stdout(predicate::str::contains(
            "→ removed destination test:Backups/Documents from job documents",
        ));
    let config = home.config();
    assert!(!config.contains("d-7f3a"), "{config}");
    assert!(config.contains("d-91c0"), "{config}");
    assert!(config.contains("name = \"documents\""), "{config}");
    assert!(!fs::read_to_string(home.rclone_conf())
        .unwrap()
        .contains("onefolder-test"));
    let state = fs::read_to_string(home.path("data/state.json")).unwrap();
    assert!(!state.contains("d-7f3a"), "{state}");
    assert!(state.contains("documents/d-91c0"), "{state}");
}

#[test]
fn remove_force_says_which_jobs_are_left_empty() {
    let Some(home) = Home::new() else { return };
    let photos = "\n[[jobs]]\nname = \"photos\"\nsource = \"/Users/someone/Pictures\"\n\n  [[jobs.destinations]]\n  id = \"d-0001\"\n  type = \"onedrive\"\n  account = \"test\"\n  path = \"Photos\"\n  mode = \"one-way\"\n";
    home.write_config(&format!("{TEST_ACCOUNT}{photos}"));
    home.cmd()
        .args(["accounts", "remove", "test", "--force", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "job photos would be left with no destinations",
        ));
    home.cmd()
        .args(["accounts", "remove", "test", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "job photos has no destinations now · add one with: onefolder jobs edit photos",
        ));
    // doctor warns about a job with no destinations.
    let out = home.cmd().args(["doctor", "--json"]).output().unwrap();
    let v = json(&out.stdout);
    assert_eq!(v["data"]["jobs"][0]["name"], "photos");
    assert_eq!(v["data"]["jobs"][0]["destinations"], serde_json::json!([]));
    assert_ne!(v["data"]["jobs"][0]["status"], "ok");
}

#[test]
fn dry_runs_work_without_a_terminal() {
    let Some(home) = Home::new() else { return };
    home.write_config(&format!("{TEST_ACCOUNT}{}", account("gone")));
    home.cmd()
        .args(["accounts", "add", "work", "--drive-type", "business", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Dry run: would run rclone config create --no-output onefolder-work onedrive drive_type=business",
        ));
    home.cmd()
        .args(["accounts", "reconnect", "test", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Dry run: would run rclone config reconnect onefolder-test:",
        ));
    home.cmd()
        .args(["accounts", "reconnect", "gone", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Dry run: would make the missing rclone remote again",
        ));
    assert!(!home.config().contains("work"));
}

#[test]
fn remove_and_reconnect_need_a_label_without_a_terminal() {
    let Some(home) = Home::new() else { return };
    home.write_config(TEST_ACCOUNT);
    for action in ["remove", "reconnect"] {
        home.cmd()
            .args(["accounts", action])
            .assert()
            .code(2)
            .stderr(predicate::str::contains(format!(
                "Say which account to {action}, for example: onefolder accounts {action} test"
            )));
    }
    home.cmd()
        .args(["accounts", "reconnect", "test"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("needs a terminal"));
}

#[test]
fn add_refuses_without_a_terminal() {
    let Some(home) = Home::new() else { return };
    home.write_config(TEST_ACCOUNT);
    let refusals = [
        (
            vec!["accounts", "add"],
            "needs-label",
            "accounts add needs a label when it cannot ask for one",
        ),
        (
            vec!["accounts", "add", "work"],
            "needs-drive-type",
            "accounts add needs --drive-type personal or --drive-type business",
        ),
        (
            vec!["accounts", "add", "work", "--drive-type", "business"],
            "needs-terminal",
            "needs a terminal to start from",
        ),
        (
            vec!["accounts", "add", "Work", "--drive-type", "business"],
            "invalid-label",
            "\"Work\" is not a valid label",
        ),
        (
            vec!["accounts", "add", "test", "--drive-type", "business"],
            "invalid-label",
            "There is already an account called test",
        ),
    ];
    for (args, code, message) in &refusals {
        home.cmd()
            .args(args)
            .assert()
            .code(2)
            .stdout("")
            .stderr(predicate::str::contains(*message));
        let out = home.cmd().args(args).arg("--json").output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let v = json(&out.stdout);
        assert_eq!(v["command"], "accounts add");
        assert_eq!(v["error"]["code"], *code, "{args:?}");
    }

    // A remote of the same name already in rclone's config blocks the label.
    home.write_rclone_conf(&[
        ("onefolder-test", &home.path("target")),
        ("onefolder-taken", &home.path("target")),
    ]);
    home.cmd()
        .args(["accounts", "add", "taken", "--drive-type", "personal"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "Rclone already has a remote called onefolder-taken",
        ));
    // Nothing was written.
    assert_eq!(home.config(), TEST_ACCOUNT);
}

#[test]
fn doctor_checks_the_accounts() {
    let Some(home) = Home::new() else { return };
    home.write_config(&format!("{TEST_ACCOUNT}{}", account("gone")));
    let out = home.cmd().args(["doctor", "--json"]).output().unwrap();
    let v = json(&out.stdout);
    let accounts = &v["data"]["accounts"];
    assert_eq!(accounts[0]["status"], "ok");
    assert_eq!(accounts[0]["health"], "connected");
    assert!(accounts[0]["free"].as_u64().is_some());
    assert_eq!(accounts[1]["status"], "problem");
    assert_eq!(accounts[1]["health"], "remote-missing");
    assert_eq!(out.status.code(), Some(1));

    home.cmd()
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(
            predicate::str::is_match(r"  ✓ test        personal   token ok   .+ free\n").unwrap(),
        )
        .stdout(predicate::str::contains(
            "  ✗ gone        business   remote onefolder-gone missing from rclone's config",
        ));
}

/// The sign-in flows, under a pseudo-terminal with a fake rclone.
#[cfg(target_os = "macos")]
mod sign_in {
    use super::*;
    use std::io::Write;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    /// A Home plus a fake rclone in `fake/` that logs every call to
    /// `rclone.log`, turns `config create <n> onedrive ...` into an alias of
    /// `$FAKE_TARGET` after `$FAKE_SLEEP` seconds (running `$FAKE_DURING`
    /// first, if set), and answers `config reconnect` with success. Every
    /// other command goes to the real rclone.
    struct Pty {
        home: Home,
    }

    impl Pty {
        fn new() -> Option<Pty> {
            let home = Home::new()?;
            let real = home.rclone_dir.join("rclone");
            fs::create_dir_all(home.path("fake")).unwrap();
            let script = format!(
                r#"#!/bin/sh
echo "$*" >> "{log}"
if [ "$1" = config ] && [ "$2" = create ]; then
  echo "FAKE: sign-in for $4" >&2
  if [ -n "$FAKE_DURING" ]; then sh -c "$FAKE_DURING"; fi
  sleep "${{FAKE_SLEEP:-0}}"
  exec "{real}" config create "$4" alias remote="$FAKE_TARGET" >/dev/null
fi
if [ "$1" = config ] && [ "$2" = reconnect ]; then
  echo "FAKE: signed in again to $3" >&2
  exit 0
fi
exec "{real}" "$@"
"#,
                log = home.path("rclone.log").display(),
                real = real.display()
            );
            let bin = home.path("fake/rclone");
            fs::write(&bin, script).unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            Some(Pty { home })
        }

        fn log(&self) -> String {
            fs::read_to_string(self.home.path("rclone.log")).unwrap_or_default()
        }

        fn remotes(&self) -> String {
            fs::read_to_string(self.home.rclone_conf()).unwrap_or_default()
        }

        /// Runs onefolder under `script`, optionally pressing Ctrl-C after
        /// `ctrl_c`. Returns the exit code and everything shown on the terminal.
        fn run(
            &self,
            args: &[&str],
            env: &[(&str, String)],
            ctrl_c: Option<Duration>,
        ) -> (i32, String) {
            let bin = assert_cmd::cargo::cargo_bin!("onefolder");
            let mut cmd = std::process::Command::new("/usr/bin/script");
            cmd.args(["-q", "/dev/null"])
                .arg(bin)
                .args(args)
                .env("ONEFOLDER_CONFIG_DIR", self.home.path("config"))
                .env("ONEFOLDER_DATA_DIR", self.home.path("data"))
                .env("RCLONE_CONFIG", self.home.rclone_conf())
                .env("NO_COLOR", "1")
                .env("FAKE_TARGET", self.home.path("target"))
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", self.home.path("fake").display()),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (k, v) in env {
                cmd.env(k, v);
            }
            let mut child = cmd.spawn().unwrap();
            let mut stdin = child.stdin.take().unwrap();
            if let Some(after) = ctrl_c {
                std::thread::sleep(after);
                stdin.write_all(b"\x03").unwrap();
                stdin.flush().unwrap();
            }
            // Keep the terminal's input open until the command is done.
            let out = child.wait_with_output().unwrap();
            drop(stdin);
            let text = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
            (out.status.code().unwrap_or(-1), text)
        }
    }

    #[test]
    fn add_signs_in_and_saves_the_account() {
        let Some(pty) = Pty::new() else { return };
        pty.home.write_config(TEST_ACCOUNT);
        // An account hand-added while the browser is open must survive.
        let during = format!(
            "printf '\\n[[accounts]]\\nlabel = \"manual\"\\nremote = \"onefolder-manual\"\\ndrive_type = \"personal\"\\n' >> '{}'",
            pty.home.path("config/config.toml").display()
        );
        let (code, shown) = pty.run(
            &["accounts", "add", "work", "--drive-type", "business"],
            &[("FAKE_DURING", during)],
            None,
        );
        assert_eq!(code, 0, "{shown}");
        assert!(pty
            .log()
            .contains("config create --no-output onefolder-work onedrive drive_type=business"));
        assert!(
            shown.contains("FAKE: sign-in for onefolder-work"),
            "{shown}"
        );
        assert!(
            shown.contains("✓ Added work · business · onefolder-work · "),
            "{shown}"
        );
        assert!(pty.remotes().contains("[onefolder-work]"));
        let config = pty.home.config();
        assert!(config.contains("label = \"work\""), "{config}");
        assert!(config.contains("label = \"manual\""), "{config}");
        assert!(config.contains("label = \"test\""), "{config}");
        assert!(config.contains("drive_type = \"business\""), "{config}");
        let added = config
            .lines()
            .rfind(|l| l.starts_with("added = "))
            .unwrap_or_default();
        // Local time with its offset, such as 2026-10-13T09:40:00+11:00.
        assert!(
            predicate::str::is_match(r"^added = \d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(Z|[+-]\d\d:\d\d)$")
                .unwrap()
                .eval(added),
            "{added}"
        );
    }

    #[test]
    fn add_cleans_up_when_onedrive_does_not_answer() {
        let Some(pty) = Pty::new() else { return };
        pty.home.write_config(TEST_ACCOUNT);
        let (code, shown) = pty.run(
            &["accounts", "add", "work", "--drive-type", "personal"],
            &[(
                "FAKE_TARGET",
                pty.home.path("missing").display().to_string(),
            )],
            None,
        );
        assert_eq!(code, 2, "{shown}");
        assert!(
            shown.contains(
                "OneFolder removed the half-made rclone remote onefolder-work, so nothing changed."
            ),
            "{shown}"
        );
        assert!(!pty.remotes().contains("onefolder-work"));
        assert_eq!(pty.home.config(), TEST_ACCOUNT);
    }

    #[test]
    fn ctrl_c_during_sign_in_cleans_up() {
        let Some(pty) = Pty::new() else { return };
        pty.home.write_config(TEST_ACCOUNT);
        let started = Instant::now();
        let (code, shown) = pty.run(
            &["accounts", "add", "work", "--drive-type", "personal"],
            &[("FAKE_SLEEP", "20".into())],
            Some(Duration::from_millis(2000)),
        );
        assert!(started.elapsed() < Duration::from_secs(15), "{shown}");
        assert_eq!(code, 2, "{shown}");
        assert!(
            shown.contains("You stopped the sign-in with Ctrl-C."),
            "{shown}"
        );
        assert!(!pty.remotes().contains("onefolder-work"));
        assert_eq!(pty.home.config(), TEST_ACCOUNT);
    }

    #[test]
    fn reconnect_signs_in_again() {
        let Some(pty) = Pty::new() else { return };
        pty.home.write_config(TEST_ACCOUNT);
        let (code, shown) = pty.run(&["accounts", "reconnect", "test"], &[], None);
        assert_eq!(code, 0, "{shown}");
        assert!(pty.log().contains("config reconnect onefolder-test:"));
        assert!(shown.contains("✓ test is connected again · "), "{shown}");
    }

    #[test]
    fn reconnect_makes_a_missing_remote_again() {
        let Some(pty) = Pty::new() else { return };
        let config = format!("{TEST_ACCOUNT}{}{}", account("work"), job_using("work"));
        pty.home.write_config(&config);
        let (code, shown) = pty.run(&["accounts", "reconnect", "work"], &[], None);
        assert_eq!(code, 0, "{shown}");
        assert!(
            shown.contains("rclone has no remote called onefolder-work any more"),
            "{shown}"
        );
        assert!(pty
            .log()
            .contains("config create --no-output onefolder-work onedrive drive_type=business"));
        assert!(pty.remotes().contains("[onefolder-work]"));
        // The account and the job that uses it are untouched.
        assert_eq!(pty.home.config(), config);
        let (code, _) = pty.run(&["accounts", "test", "work"], &[], None);
        assert_eq!(code, 0);
    }
}
