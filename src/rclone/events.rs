//! Decodes rclone's `--use-json-log` stderr lines into typed [`Event`]s.
//!
//! Every line rclone prints with `--use-json-log` is one JSON object with
//! `level`, `msg`, `time` and `source`; per-file lines add `object`,
//! `objectType` and `size`; dry-run lines add `skipped`; stats lines add a
//! `stats` object. One serde struct per shape reads them, and the decoder
//! is lenient: unknown fields are ignored, a line of an unknown shape
//! becomes [`Event::Notice`], a line that is not JSON at all becomes
//! [`Event::Unparsed`], and nothing here panics.
//!
//! The fixtures under `tests/fixtures/rclone/` are real rclone 1.75.2
//! output, recorded by `scripts/record-fixtures.sh`.

use serde::{Deserialize, Serialize};

use super::bisync;
pub use super::bisync::{BisyncPhase, PlanLine, PlanVerb, Queued, Side, SideChanges};

/// One decoded line of rclone's stderr.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    /// A line carrying a `stats` object: progress so far.
    Stats(Stats),
    /// A file copied, or its modification time set (`-v` lines).
    Transferred(Transferred),
    /// What a dry run would have done (the `skipped` field).
    Skipped(Skipped),
    /// A file deleted on the destination (two-way runs, milestone 5).
    Deleted { object: String },
    /// A directory removed on the destination (two-way runs, milestone 5).
    RemovedDir { object: String },
    /// A line at level `error` or `critical`.
    Error(ErrorLine),
    /// bisync: one change it found on one side since the last run
    /// (`- Path1 File was deleted - Old projects/a.txt`).
    Plan(PlanLine),
    /// bisync: the count line after a side's changes
    /// (`Path1:   10 changes:    1 new,    2 modified,    7 deleted`).
    Changes(SideChanges),
    /// bisync: a milestone (`Building Path1 and Path2 listings`, ...).
    Phase { phase: BisyncPhase },
    /// bisync: the same file is new or changed on both sides.
    Conflict { path: String },
    /// bisync: after a [`Event::Conflict`], the two copies turned out equal
    /// (`Files are equal! Skipping: x`) or the path is a folder: nothing is
    /// renamed, so it is no conflict after all.
    NoConflict { path: String },
    /// bisync: an operation queued for one side (`Queue copy to Path1`,
    /// `Queue delete`, `Renaming Path2 copy`), with the absolute path
    /// rclone prints.
    Queued(Queued),
    /// bisync: the queued operations on one side start
    /// (`Do queued copies to - Path1`, `Resync is copying files to - Path2`).
    /// The per-file lines that follow happen on that side.
    Applying { to: Side },
    /// bisync stopped: `Bisync critical error`, `Bisync aborted`, a safety
    /// abort, a lock, `Failed to bisync`. [`bisync::classify`] reads these.
    Fatal { message: String },
    /// Any other JSON line, kept as text.
    Notice {
        level: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        object: Option<String>,
    },
    /// A line that is not JSON, kept verbatim.
    Unparsed { line: String },
}

/// rclone's stats object. Every field defaults, so a release that drops
/// one still decodes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Stats {
    /// Bytes transferred so far.
    pub bytes: u64,
    /// Bytes to transfer in all, as far as rclone knows yet.
    pub total_bytes: u64,
    pub transfers: u64,
    pub total_transfers: u64,
    pub checks: u64,
    pub total_checks: u64,
    /// Entries listed so far on both sides (source and destination), when
    /// rclone says: 1.75.2 does, older releases may not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listed: Option<u64>,
    pub errors: u64,
    pub deletes: u64,
    /// Seconds left, when rclone can tell.
    pub eta: Option<f64>,
    /// Bytes per second.
    pub speed: f64,
    /// Seconds since the command started.
    pub elapsed_time: f64,
    pub fatal_error: bool,
    pub last_error: Option<String>,
    /// Files in flight.
    pub transferring: Vec<InFlight>,
}

/// A file in flight, from the stats `transferring` list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InFlight {
    pub name: String,
    pub bytes: u64,
    pub size: u64,
    pub percentage: f64,
}

/// What a per-file `-v` line says happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferKind {
    /// `Copied (new)`: the file was not on the destination.
    New,
    /// `Copied (replaced existing)`.
    Replaced,
    /// `Copied (server-side copy)`: local to local, or within one remote.
    /// rclone does not say whether the file was new.
    ServerSide,
    /// `Updated modification time in destination`: same content, only the time moved.
    ModTime,
    /// Another `Copied (...)` wording.
    Other,
}

/// A file copied, or its modification time set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Transferred {
    pub kind: TransferKind,
    /// Path relative to the source root.
    pub object: String,
    /// Bytes, when rclone reports them (not for a modification time).
    pub size: Option<u64>,
    pub message: String,
}

/// A dry-run line: what rclone would have done.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    /// rclone's words: `copy`, `update modification time`,
    /// `set directory modification time`, `delete`...
    pub action: String,
    pub object: String,
    pub size: Option<u64>,
    /// True for a directory (`objectType` names a Directory).
    pub is_dir: bool,
}

/// A line at level `error` or `critical`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ErrorLine {
    /// The file it is about, when it is about one file.
    pub object: Option<String>,
    pub message: String,
    /// True for `critical`, and for an error about a whole file system
    /// (`objectType` `*local.Fs` and the like) rather than one file.
    pub fatal: bool,
}

impl ErrorLine {
    /// The file this error is about, when it is about a single file.
    pub fn file(&self) -> Option<&str> {
        if self.fatal {
            None
        } else {
            self.object.as_deref()
        }
    }
}

/// The fields of one JSON log line that the decoder reads.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Line {
    level: Option<String>,
    msg: Option<String>,
    object: Option<String>,
    object_type: Option<String>,
    size: Option<i64>,
    skipped: Option<String>,
    stats: Option<serde_json::Value>,
}

/// Decodes one line of rclone's stderr. Never fails: see the module docs.
pub fn decode(raw: &str) -> Event {
    let text = raw.trim_end_matches(['\n', '\r']);
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        _ => {
            return Event::Unparsed {
                line: text.to_string(),
            }
        }
    };
    let line: Line = match serde_json::from_value(value.clone()) {
        Ok(line) => line,
        // A known field with an unexpected type: keep what text there is.
        Err(_) => return notice_from(&value),
    };
    let level = line.level.clone().unwrap_or_default().to_ascii_lowercase();
    let message = line.msg.clone().unwrap_or_default();
    let size = line.size.and_then(|s| u64::try_from(s).ok());
    let is_dir = line
        .object_type
        .as_deref()
        .is_some_and(|t| t.contains("Directory"));

    if let Some(stats) = &line.stats {
        if let Ok(stats) = serde_json::from_value::<Stats>(stats.clone()) {
            return Event::Stats(stats);
        }
    }
    // bisync's own lines, at whatever level they come.
    if let Some(event) = bisync::parse_line(&message) {
        return event;
    }
    if level == "error" || level == "critical" {
        let whole_fs = line
            .object_type
            .as_deref()
            .is_some_and(|t| t.ends_with(".Fs"));
        return Event::Error(ErrorLine {
            object: line.object,
            message,
            fatal: level == "critical" || whole_fs,
        });
    }
    if let (Some(action), Some(object)) = (&line.skipped, &line.object) {
        return Event::Skipped(Skipped {
            action: action.clone(),
            object: object.clone(),
            size,
            is_dir,
        });
    }
    if let Some(object) = &line.object {
        if !is_dir {
            if let Some(kind) = transfer_kind(&message) {
                return Event::Transferred(Transferred {
                    kind,
                    object: object.clone(),
                    size: if kind == TransferKind::ModTime {
                        None
                    } else {
                        size
                    },
                    message,
                });
            }
            if message == "Deleted" {
                return Event::Deleted {
                    object: object.clone(),
                };
            }
        }
        if message.starts_with("Removing directory") || message == "Removed directory" {
            return Event::RemovedDir {
                object: object.clone(),
            };
        }
    }
    Event::Notice {
        level: if level.is_empty() {
            "notice".into()
        } else {
            level
        },
        message,
        object: line.object,
    }
}

/// The kind of a per-file line, from its message.
fn transfer_kind(message: &str) -> Option<TransferKind> {
    if message.starts_with("Updated modification time") {
        return Some(TransferKind::ModTime);
    }
    // "Copied (new)", "Multi-thread Copied (replaced existing)",
    // "Copied (Rcat, new)", "Copied (server-side copy)".
    let rest = &message[message.find("Copied (")? + "Copied (".len()..];
    let detail = rest.split(')').next().unwrap_or("");
    Some(if detail.contains("server-side") {
        TransferKind::ServerSide
    } else if detail.contains("replaced existing") {
        TransferKind::Replaced
    } else if detail.contains("new") {
        TransferKind::New
    } else {
        TransferKind::Other
    })
}

/// A notice from a JSON object whose fields did not have the expected types.
fn notice_from(value: &serde_json::Value) -> Event {
    let text = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_string);
    Event::Notice {
        level: text("level").unwrap_or_else(|| "notice".into()),
        message: text("msg").unwrap_or_else(|| value.to_string()),
        object: text("object"),
    }
}

/// Decodes every line of a recorded log.
#[cfg(test)]
pub fn decode_all(text: &str) -> Vec<Event> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(decode)
        .collect()
}

/// True when `just fixtures` has just recorded the fixtures on Linux.
/// rclone's local backend says different things there (a plain copy where
/// macOS clones the file server-side, a folder's size, the error for an
/// unreadable file), so those recordings are checked against their own
/// snapshots, `events_<name>@linux.snap`. The committed fixtures are macOS
/// recordings, checked against the plain snapshots on every platform.
#[cfg(test)]
pub(crate) fn fixtures_recorded_on_linux() -> bool {
    std::env::var("ONEFOLDER_FIXTURES_RECORDED_ON").is_ok_and(|v| v == "linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn fixture(name: &str) -> String {
        let path = crate::rclone::fixture_path(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// Makes a decoded sequence independent of timing, so a re-recording
    /// with the same rclone gives the same snapshot (`just fixtures`):
    ///
    /// - stats lines in a row become the last of them, and the fields that
    ///   depend on the clock (speed, ETA, elapsed time) are zeroed; with
    ///   `progress` the byte and listed counts of an interrupted run are
    ///   zeroed too;
    /// - between stats lines, per-file lines are sorted, because rclone's
    ///   transfers run in parallel and finish in any order.
    fn normalise(events: Vec<Event>, progress: bool) -> Vec<Event> {
        let mut out: Vec<Event> = Vec::new();
        let mut run: Vec<Event> = Vec::new();
        let key = |e: &Event| serde_json::to_string(e).unwrap_or_default();
        let flush = |run: &mut Vec<Event>, out: &mut Vec<Event>| {
            run.sort_by_key(key);
            out.append(run);
        };
        for event in events {
            match event {
                Event::Stats(mut s) => {
                    flush(&mut run, &mut out);
                    s.speed = 0.0;
                    s.eta = None;
                    s.elapsed_time = 0.0;
                    if progress {
                        s.bytes = 0;
                        // How far the listing got depends on the timing too.
                        s.listed = s.listed.map(|_| 0);
                    }
                    for f in &mut s.transferring {
                        if progress {
                            f.bytes = 0;
                            f.percentage = 0.0;
                        }
                    }
                    if let Some(Event::Stats(_)) = out.last() {
                        out.pop();
                    }
                    out.push(Event::Stats(s));
                }
                other => run.push(other),
            }
        }
        flush(&mut run, &mut out);
        out
    }

    /// For bisync recordings, also: notices (which are only logged) have
    /// their digits masked, since they carry times and countdowns, and a
    /// notice repeated in a row is kept once.
    /// Each run of digits becomes one `#` (times and countdowns vary in
    /// length), and a time zone name after an offset (`+1000 AEST`)
    /// becomes `<zone>`, so recordings made in any zone compare equal.
    pub fn mask_text(text: &str) -> String {
        let mut collapsed = String::with_capacity(text.len());
        let mut in_digits = false;
        for c in text.chars() {
            if c.is_ascii_digit() {
                if !in_digits {
                    collapsed.push('#');
                }
                in_digits = true;
            } else {
                collapsed.push(c);
                in_digits = false;
            }
        }
        // A UTC offset in a timestamp: `+11:00`, `-05:00` or `Z`.
        let collapsed = collapsed
            .replace("+#:#", "<offset>")
            .replace("-#:#", "<offset>");
        let mut chars = collapsed.chars().peekable();
        let mut offsets = String::with_capacity(collapsed.len());
        while let Some(c) = chars.next() {
            offsets.push(c);
            if c == '#' && chars.peek() == Some(&'Z') {
                let mut ahead = chars.clone();
                ahead.next();
                if !ahead.peek().is_some_and(|n| n.is_alphanumeric()) {
                    chars.next();
                    offsets.push_str("<offset>");
                }
            }
        }
        let collapsed = offsets;
        let words: Vec<&str> = collapsed.split(' ').collect();
        let mut out: Vec<String> = Vec::with_capacity(words.len());
        for (i, word) in words.iter().enumerate() {
            let after_offset = i > 0 && matches!(words[i - 1], "+#" | "-#");
            let zone = word.trim_end_matches(['.', ',', ')']);
            if after_offset
                && (2..=5).contains(&zone.len())
                && zone.chars().all(|c| c.is_ascii_uppercase())
            {
                out.push(format!("<zone>{}", &word[zone.len()..]));
            } else {
                out.push((*word).to_string());
            }
        }
        out.join(" ")
    }

    #[test]
    fn masking() {
        assert_eq!(
            mask_text("Valid lock file found. Expires at 2226-08-26 14:49:23.348051 +1000 AEST. (1751999h59m50s from now)"),
            "Valid lock file found. Expires at #-#-# #:#:#.# +# <zone>. (#h#m#s from now)"
        );
        assert_eq!(
            mask_text("Expires at 2226-08-26 03:49:23.3 +0000 UTC. x"),
            "Expires at #-#-# #:#:#.# +# <zone>. x"
        );
        assert_eq!(mask_text("Path1 OK USA"), "Path# OK USA");
        assert_eq!(
            mask_text("(mtime=2026-10-13T15:46:33.861391598+11:00)"),
            mask_text("(mtime=2026-10-13T04:46:33.86Z)")
        );
        assert_eq!(mask_text("Zone Zebra 5Zq"), "Zone Zebra #Zq");
    }

    fn mask_notices(events: Vec<Event>) -> Vec<Event> {
        let mut out: Vec<Event> = Vec::new();
        for event in events {
            let event = match event {
                Event::Notice {
                    level,
                    message,
                    object,
                } => Event::Notice {
                    level,
                    message: mask_text(&message),
                    object,
                },
                other => other,
            };
            if out.last() != Some(&event) {
                out.push(event);
            }
        }
        out
    }

    /// bisync's order matters (phases, the side each operation runs on),
    /// so its recordings keep it: only runs of per-file lines, which finish
    /// in any order, are sorted, and of the stats only the last is kept.
    /// With `progress` (an interrupted run) rclone's clean-up chatter,
    /// notices and per-file errors, depends on timing and is dropped; the
    /// typed events (plan, phases, sides, the abort lines) stay.
    fn normalise_bisync(events: Vec<Event>, progress: bool) -> Vec<Event> {
        let events: Vec<Event> = events
            .into_iter()
            .filter(|e| !progress || !matches!(e, Event::Notice { .. } | Event::Error(_)))
            .collect();
        let per_file = |e: &Event| {
            matches!(
                e,
                Event::Transferred(_)
                    | Event::Deleted { .. }
                    | Event::RemovedDir { .. }
                    | Event::Skipped(_)
                    | Event::Error(_)
                    | Event::Notice {
                        object: Some(_),
                        ..
                    }
            )
        };
        let key = |e: &Event| serde_json::to_string(e).unwrap_or_default();
        let last_stats = events.iter().rev().find_map(|e| match e {
            Event::Stats(s) => Some(s.clone()),
            _ => None,
        });
        let mut out: Vec<Event> = Vec::new();
        let mut run: Vec<Event> = Vec::new();
        for event in events {
            if matches!(event, Event::Stats(_)) {
                continue;
            }
            if per_file(&event) {
                run.push(event);
                continue;
            }
            run.sort_by_key(key);
            out.append(&mut run);
            out.push(event);
        }
        run.sort_by_key(key);
        out.append(&mut run);
        if let Some(mut stats) = last_stats {
            // An interrupted run's total counts what was sent before the stop.
            if progress {
                stats.total_bytes = 0;
            }
            out.extend(normalise(vec![Event::Stats(stats)], progress));
        }
        mask_notices(out)
    }

    fn snapshot(name: &str, progress: bool) {
        let events = decode_all(&fixture(&format!("{name}.jsonl")));
        let events = if name.starts_with("bisync") {
            normalise_bisync(events, progress)
        } else {
            normalise(events, progress)
        };
        let events = redact_listed(events);
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"));
        settings.set_prepend_module_to_snapshot(false);
        if fixtures_recorded_on_linux() {
            settings.set_snapshot_suffix("linux");
        }
        settings.bind(|| {
            insta::assert_json_snapshot!(format!("events_{name}"), events);
        });
    }

    /// The marker that stands in for a stats line's `listed` in snapshots.
    const LISTED: &str = "[listed]";

    /// Replaces each stats line's `listed` count with [`LISTED`]. How many
    /// entries rclone has listed when it prints its final stats depends on
    /// timing: two recordings with the same rclone give different counts
    /// (bisync-plan gave 67, 68 and 75). OneFolder only shows the count as
    /// a live figure, so the snapshots keep that it is there, not its value;
    /// `listed_is_decoded` checks the value from a fixed line.
    fn redact_listed(events: Vec<Event>) -> serde_json::Value {
        let mut value = serde_json::to_value(events).expect("events serialise");
        for event in value.as_array_mut().into_iter().flatten() {
            if event["event"] == "stats" {
                if let Some(listed) = event.get_mut("listed").filter(|l| !l.is_null()) {
                    *listed = LISTED.into();
                }
            }
        }
        value
    }

    #[test]
    fn copy_dry_run() {
        snapshot("copy-dry-run", false);
    }

    #[test]
    fn copy_real() {
        snapshot("copy-real", false);
    }

    #[test]
    fn copy_upload() {
        snapshot("copy-upload", false);
    }

    #[test]
    fn copy_nothing() {
        snapshot("copy-nothing", false);
    }

    #[test]
    fn copy_exclude() {
        snapshot("copy-exclude", false);
    }

    #[test]
    fn copy_file_error() {
        snapshot("copy-file-error", false);
    }

    #[test]
    fn copy_missing_parent() {
        snapshot("copy-missing-parent", false);
    }

    #[test]
    fn copy_missing_source() {
        snapshot("copy-missing-source", false);
    }

    #[test]
    fn copy_alias() {
        snapshot("copy-alias", false);
    }

    #[test]
    fn copy_sigint() {
        snapshot("copy-sigint", true);
    }

    #[test]
    fn bisync_recordings() {
        for name in [
            "bisync-resync-dry-run",
            "bisync-resync",
            "bisync-plan",
            "bisync-sync",
            "bisync-after-conflict",
            "bisync-own-conflict",
            "bisync-race-conflict",
            "bisync-folder-plan",
            "bisync-too-many-deletes",
            "bisync-too-many-deletes-force",
            "bisync-missing-listings",
            "bisync-filters-file",
            "bisync-empty-side",
            "bisync-missing-destination",
            "bisync-all-changed",
            "bisync-locked",
            "bisync-recover",
            "bisync-name-too-long",
        ] {
            snapshot(name, false);
        }
        snapshot("bisync-sigint", true);
    }

    #[test]
    fn bisync_plan_lines_decode() {
        let events = decode_all(&fixture("bisync-plan.jsonl"));
        let plan: Vec<(Side, PlanVerb, &str)> = events
            .iter()
            .filter_map(|e| match e {
                Event::Plan(p) => Some((p.side, p.verb, p.path.as_str())),
                _ => None,
            })
            .collect();
        assert!(plan.contains(&(Side::Path1, PlanVerb::Deleted, "del.txt")));
        assert!(plan.contains(&(Side::Path1, PlanVerb::Deleted, "sub")));
        assert!(plan.contains(&(Side::Path1, PlanVerb::New, "newlocal.txt")));
        assert!(plan.contains(&(Side::Path1, PlanVerb::Changed, "mod.txt")));
        assert!(plan.contains(&(Side::Path2, PlanVerb::New, "newremote.txt")));
        assert!(plan.contains(&(Side::Path2, PlanVerb::Changed, "rmod.txt")));
        assert!(plan.contains(&(Side::Path2, PlanVerb::Deleted, "rdel.txt")));
        assert!(events.contains(&Event::Conflict {
            path: "Notes/plan.md".into()
        }));
        let phases: Vec<BisyncPhase> = events
            .iter()
            .filter_map(|e| match e {
                Event::Phase { phase } => Some(*phase),
                _ => None,
            })
            .collect();
        assert_eq!(
            phases,
            [
                BisyncPhase::Listing,
                BisyncPhase::Checking,
                BisyncPhase::Checking,
                BisyncPhase::Applying,
                BisyncPhase::Updating
            ]
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::Applying { to: Side::Path1 })));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Changes(SideChanges {
                side: Side::Path2,
                new: 1,
                modified: 2,
                deleted: 1,
                ..
            })
        )));
    }

    #[test]
    fn dry_run_lines_are_skipped_events() {
        let events = decode_all(&fixture("copy-dry-run.jsonl"));
        let copies: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Skipped(s) if s.action == "copy" => Some((s.object.as_str(), s.size)),
                _ => None,
            })
            .collect();
        assert_eq!(copies.len(), 3, "{events:#?}");
        assert!(copies.contains(&("new.txt", Some(6))));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Skipped(Skipped { action, is_dir: false, .. }) if action == "update modification time"
        )));
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::Skipped(Skipped { is_dir: true, .. }))));
    }

    #[test]
    fn upload_lines_say_new_or_replaced() {
        let kinds: Vec<_> = decode_all(&fixture("copy-upload.jsonl"))
            .into_iter()
            .filter_map(|e| match e {
                Event::Transferred(t) => Some((t.object, t.kind)),
                _ => None,
            })
            .collect();
        assert!(kinds.contains(&("new.txt".into(), TransferKind::New)));
        assert!(kinds.contains(&("Projects/2026/budget.xlsx".into(), TransferKind::Replaced)));
        assert!(kinds.contains(&("touched.txt".into(), TransferKind::ModTime)));
    }

    #[test]
    fn listed_is_decoded() {
        // A stats line as rclone 1.75.2 prints it, with `listed`; the
        // snapshots redact the count, since it depends on timing.
        let line = r#"{"time":"2026-10-15T18:17:45.272652+11:00","level":"notice","msg":"\nTransferred:   \t          0 B / 0 B, -, 0 B/s, ETA -\nChecks:                 5 / 5, 100%, Listed 16\nElapsed time:         0.0s\n\n","stats":{"bytes":0,"checks":5,"deletedDirs":0,"deletes":0,"elapsedTime":0.000475167,"errors":0,"eta":null,"fatalError":false,"listed":16,"renames":0,"retryError":false,"serverSideCopies":0,"serverSideCopyBytes":0,"serverSideMoveBytes":0,"serverSideMoves":0,"speed":0,"totalBytes":0,"totalChecks":5,"totalTransfers":0,"transferTime":0.000036125,"transfers":0},"source":"accounting/stats.go:549"}"#;
        match decode(line) {
            Event::Stats(s) => {
                assert_eq!((s.listed, s.checks, s.total_checks), (Some(16), 5, 5));
            }
            other => panic!("{other:?}"),
        }
        // Older releases leave it out.
        match decode(r#"{"level":"notice","msg":"s","stats":{"checks":5}}"#) {
            Event::Stats(s) => assert_eq!(s.listed, None),
            other => panic!("{other:?}"),
        }
        // In a snapshot the count becomes the marker; a missing one stays null.
        let redacted = redact_listed(vec![
            decode(line),
            decode(r#"{"level":"notice","msg":"s","stats":{"checks":5}}"#),
        ]);
        assert_eq!(redacted[0]["listed"], LISTED);
        assert!(redacted[1]["listed"].is_null());
    }

    #[test]
    fn interrupted_run_has_files_in_flight() {
        let events = decode_all(&fixture("copy-sigint.jsonl"));
        let flying = events.iter().any(|e| match e {
            Event::Stats(s) => s
                .transferring
                .iter()
                .any(|f| f.name == "big.bin" && f.size > 0),
            _ => false,
        });
        assert!(flying, "{events:#?}");
    }

    #[test]
    fn errors_know_files_from_file_systems() {
        let events = decode_all(&fixture("copy-file-error.jsonl"));
        let files: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Error(err) => err.file(),
                _ => None,
            })
            .collect();
        assert!(!files.is_empty() && files.iter().all(|f| *f == "locked.txt"));
        let events = decode_all(&fixture("copy-missing-source.jsonl"));
        assert!(events
            .iter()
            .all(|e| !matches!(e, Event::Error(err) if err.file().is_some())));
        assert!(events.iter().any(|e| matches!(e, Event::Error(_))));
    }

    #[test]
    fn lenient_on_odd_lines() {
        assert_eq!(
            decode("not json at all\n"),
            Event::Unparsed {
                line: "not json at all".into()
            }
        );
        assert_eq!(decode("42"), Event::Unparsed { line: "42".into() });
        assert_eq!(decode(""), Event::Unparsed { line: "".into() });
        // Unknown fields are ignored; an unknown shape is a notice.
        assert_eq!(
            decode(r#"{"level":"info","msg":"hello","brandNew":[1,2]}"#),
            Event::Notice {
                level: "info".into(),
                message: "hello".into(),
                object: None
            }
        );
        // A known field with the wrong type still gives a notice, not a panic.
        assert!(matches!(
            decode(r#"{"level":"info","msg":"x","size":"big","object":"a"}"#),
            Event::Notice { .. }
        ));
        // A stats object with odd values falls back to a notice too.
        assert!(matches!(
            decode(r#"{"level":"notice","msg":"s","stats":{"bytes":"many"}}"#),
            Event::Notice { .. }
        ));
        // Missing stats fields default.
        assert_eq!(
            decode(r#"{"level":"notice","msg":"s","stats":{"bytes":5}}"#),
            Event::Stats(Stats {
                bytes: 5,
                ..Stats::default()
            })
        );
        assert!(matches!(
            decode(
                r#"{"level":"info","msg":"Deleted","object":"a.txt","objectType":"*local.Object"}"#
            ),
            Event::Deleted { .. }
        ));
        assert!(matches!(
            decode(
                r#"{"level":"info","msg":"Removing directory","object":"old","objectType":"*local.Directory"}"#
            ),
            Event::RemovedDir { .. }
        ));
        assert!(matches!(
            decode(r#"{"level":"critical","msg":"Fatal error"}"#),
            Event::Error(ErrorLine { fatal: true, .. })
        ));
        // A negative size is not a size.
        assert!(matches!(
            decode(r#"{"level":"info","msg":"Copied (new)","object":"a","size":-1}"#),
            Event::Transferred(Transferred { size: None, .. })
        ));
    }

    #[test]
    fn transfer_kinds_from_messages() {
        assert_eq!(transfer_kind("Copied (new)"), Some(TransferKind::New));
        assert_eq!(
            transfer_kind("Multi-thread Copied (replaced existing)"),
            Some(TransferKind::Replaced)
        );
        assert_eq!(transfer_kind("Copied (Rcat, new)"), Some(TransferKind::New));
        assert_eq!(
            transfer_kind("Copied (server-side copy)"),
            Some(TransferKind::ServerSide)
        );
        assert_eq!(transfer_kind("Copied (odd)"), Some(TransferKind::Other));
        assert_eq!(
            transfer_kind("Updated modification time in destination"),
            Some(TransferKind::ModTime)
        );
        assert_eq!(transfer_kind("There was nothing to transfer"), None);
    }
}
