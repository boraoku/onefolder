//! The bisync flag set, plan-line parsing, `.path1.lst` listing parsing,
//! lock files, and classification of bisync abort messages.
//!
//! Everything here was checked against rclone 1.75.2 (the recordings under
//! `tests/fixtures/rclone/bisync-*.jsonl`, made by
//! `scripts/record-fixtures.sh`):
//!
//! - plan lines read `- Path1             File was deleted                            - sub/f1.txt`:
//!   a side column, an action padded to a column, ` - `, and a path relative
//!   to the root, Go-quoted (`"say \"hi\".txt"`) only when it needs it;
//! - the same shape carries the queued operations (`Queue copy to Path1`,
//!   `Queue delete`, `Renaming Path2 copy`, with absolute paths), the start
//!   of each side's operations (`Do queued copies to - Path1`) and conflicts
//!   (`- WARNING New or changed in both paths - conf.txt`);
//! - a listing line reads `-        2 - - 2026-10-13T04:46:33.858493642+0000 "conf.txt"`
//!   (`d` and size -1 for a folder), the path always Go-quoted;
//! - the lock file is JSON with the PID as a string:
//!   `{"Session":"...","PID":"63775","TimeRenewed":"...","TimeExpires":"..."}`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::events::Event;

/// The conflict suffix bisync uses on its own. OneFolder renames the
/// destination's copy itself before a run (see `sync::twoway`), with a name
/// it has checked is free on both sides, so bisync only meets a conflict
/// when a file changes between the plan and the run. For that case its
/// names are deliberately outside [`CONFLICT_PATTERN`]
/// (`plan.race-conflict-2026-10-13-1.md` with `--suffix-keep-extension`):
/// bisync numbers past only the copies it can see, and with the excluded
/// shape it would reuse `-1` and replace an earlier copy (seen live on
/// 1.75.2). Such a copy syncs to both sides like any file; nothing is lost.
pub const CONFLICT_SUFFIX: &str = "race-conflict-{DateOnly}-";

/// Excludes every conflict copy from the sync, so it stays on the
/// destination. Only the dated shape (`name.conflict-YYYY-MM-DD-n.ext`), so
/// an ordinary name such as `talk.conflict-resolution.md` still syncs.
pub const CONFLICT_PATTERN: &str = "*.conflict-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]-*";

/// bisync's own safety limit, in percent of a side's files.
pub const MAX_DELETE: u32 = 50;

/// The limit once the user has confirmed a preview with more deletions.
pub const MAX_DELETE_LIFTED: u32 = 100;

/// Which side of the pair: Path1 is always the local source, Path2 the
/// destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Path1,
    Path2,
}

impl Side {
    pub fn parse(text: &str) -> Option<Side> {
        match text.trim() {
            "Path1" => Some(Side::Path1),
            "Path2" => Some(Side::Path2),
            _ => None,
        }
    }

    pub fn other(self) -> Side {
        match self {
            Side::Path1 => Side::Path2,
            Side::Path2 => Side::Path1,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Side::Path1 => "Path1",
            Side::Path2 => "Path2",
        }
    }
}

/// What a plan line says happened to a file since the last run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanVerb {
    /// `File is new`
    New,
    /// `File changed: size (larger), time (newer)`
    Changed,
    /// `File was deleted`
    Deleted,
}

/// One plan line. Folders appear too (bisync runs with
/// `--create-empty-src-dirs`); the listings tell them apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanLine {
    pub side: Side,
    pub verb: PlanVerb,
    /// Relative to the side's root.
    pub path: String,
    /// For a change: rclone's words, such as `size (larger), time (newer)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// bisync's milestones, which the run screen shows as phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BisyncPhase {
    /// `Building Path1 and Path2 listings`
    Listing,
    /// `Path1 checking for diffs`, `Path2 checking for diffs`
    Checking,
    /// `Applying changes`
    Applying,
    /// `Updating listings`, `Resync updating listings`
    Updating,
    /// `Copying Path2 files to Path1` (a resync)
    Resyncing,
}

impl BisyncPhase {
    /// As the run screen names it.
    pub fn label(self) -> &'static str {
        match self {
            BisyncPhase::Listing => "Listing",
            BisyncPhase::Checking => "Comparing",
            BisyncPhase::Applying => "Applying changes",
            BisyncPhase::Updating => "Updating listings",
            BisyncPhase::Resyncing => "Merging",
        }
    }
}

/// `Path1:   10 changes:    1 new,    2 modified,    7 deleted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SideChanges {
    pub side: Side,
    pub changes: u64,
    pub new: u64,
    pub modified: u64,
    pub deleted: u64,
}

/// An operation bisync queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueuedAction {
    Copy,
    Delete,
    Rename,
}

/// `- Path2 Queue copy to Path1 - /abs/path`: the copy happens on Path1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Queued {
    /// The side the operation changes.
    pub on: Side,
    pub action: QueuedAction,
    /// As rclone prints it: absolute (`/Users/..` or `remote:path/..`).
    pub path: String,
}

/// Decodes one of bisync's own messages, or `None` for anything else.
pub fn parse_line(message: &str) -> Option<Event> {
    // Only a line break is taken off: bisync prints paths as they are, and
    // a name may start or end with spaces.
    let msg = message.strip_suffix('\n').unwrap_or(message);
    let msg = msg.strip_suffix('\r').unwrap_or(msg);
    if let Some(event) = phase(msg) {
        return Some(event);
    }
    if is_fatal(msg) {
        return Some(Event::Fatal {
            message: msg.to_string(),
        });
    }
    if let Some(changes) = side_changes(msg) {
        return Some(Event::Changes(changes));
    }
    for prefix in [
        "Files are equal! Skipping: ",
        "This is a directory, not a file. Skipping equality check and will not rename: ",
    ] {
        if let Some(path) = msg.strip_prefix(prefix) {
            return Some(Event::NoConflict {
                path: unquote(path),
            });
        }
    }
    let rest = msg.strip_prefix("- ")?;
    let (column, rest) = rest.split_once(' ')?;
    let (action, target) = rest.trim_start().split_once(" - ")?;
    let action = action.trim_end();
    let target = unquote(target);
    if column == "WARNING" {
        return (action == "New or changed in both paths")
            .then_some(Event::Conflict { path: target });
    }
    let side = Side::parse(column)?;
    let plan = |verb, detail: Option<String>| {
        Some(Event::Plan(PlanLine {
            side,
            verb,
            path: target.clone(),
            detail,
        }))
    };
    match action {
        "File is new" => plan(PlanVerb::New, None),
        "File was deleted" => plan(PlanVerb::Deleted, None),
        a if a.starts_with("File changed") => {
            let detail = a["File changed".len()..]
                .trim_start_matches(':')
                .trim()
                .to_string();
            plan(PlanVerb::Changed, (!detail.is_empty()).then_some(detail))
        }
        "Queue delete" => Some(Event::Queued(Queued {
            on: side,
            action: QueuedAction::Delete,
            path: target,
        })),
        a if a.starts_with("Queue copy to ") => Some(Event::Queued(Queued {
            on: Side::parse(&a["Queue copy to ".len()..])?,
            action: QueuedAction::Copy,
            path: target,
        })),
        a if a.starts_with("Renaming ") => Some(Event::Queued(Queued {
            on: side,
            action: QueuedAction::Rename,
            path: target,
        })),
        "Do queued copies to" | "Resync is copying files to" => Some(Event::Applying {
            to: Side::parse(&target)?,
        }),
        _ => None,
    }
}

fn phase(msg: &str) -> Option<Event> {
    let phase = match msg {
        "Building Path1 and Path2 listings" => BisyncPhase::Listing,
        "Path1 checking for diffs" | "Path2 checking for diffs" => BisyncPhase::Checking,
        "Applying changes" => BisyncPhase::Applying,
        "Updating listings" | "Resync updating listings" => BisyncPhase::Updating,
        "Copying Path2 files to Path1" => BisyncPhase::Resyncing,
        _ => return None,
    };
    Some(Event::Phase { phase })
}

/// The messages with which bisync stops.
fn is_fatal(msg: &str) -> bool {
    msg.starts_with("Bisync critical error")
        || msg.starts_with("Bisync aborted")
        || msg.starts_with("Failed to bisync")
        || msg.starts_with("Safety abort")
        || msg.starts_with("Empty current Path")
        || msg.starts_with("too many deletes")
}

fn side_changes(msg: &str) -> Option<SideChanges> {
    let (side, rest) = msg.split_once(':')?;
    let side = Side::parse(side)?;
    let (changes, rest) = rest.split_once("changes:")?;
    let mut counts = [0u64; 3];
    for part in rest.split(',') {
        let mut words = part.split_whitespace();
        let n: u64 = words.next()?.parse().ok()?;
        match words.next()? {
            "new" => counts[0] = n,
            "modified" => counts[1] = n,
            "deleted" => counts[2] = n,
            _ => return None,
        }
    }
    Some(SideChanges {
        side,
        changes: changes.trim().parse().ok()?,
        new: counts[0],
        modified: counts[1],
        deleted: counts[2],
    })
}

/// A path as rclone prints it: Go-quoted when it needs to be, otherwise
/// exactly as it is (spaces at either end belong to the name).
pub fn unquote(text: &str) -> String {
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        if let Some(s) = go_unquote(&text[1..text.len() - 1]) {
            return s;
        }
    }
    text.to_string()
}

/// Reads the inside of a Go `%q` string. `None` when it is not one.
fn go_unquote(inner: &str) -> Option<String> {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    let hex = |chars: &mut std::iter::Peekable<std::str::Chars>, n: usize| -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = v * 16 + chars.next()?.to_digit(16)?;
        }
        Some(v)
    };
    let mut bytes: Vec<u8> = Vec::new();
    let flush = |bytes: &mut Vec<u8>, out: &mut String| {
        if !bytes.is_empty() {
            out.push_str(&String::from_utf8_lossy(bytes));
            bytes.clear();
        }
    };
    while let Some(c) = chars.next() {
        if c == '"' {
            return None;
        }
        if c != '\\' {
            flush(&mut bytes, &mut out);
            out.push(c);
            continue;
        }
        let escaped = chars.next()?;
        if escaped == 'x' {
            bytes.push(u8::try_from(hex(&mut chars, 2)?).ok()?);
            continue;
        }
        if escaped.is_digit(8) {
            let mut v = escaped.to_digit(8)?;
            for _ in 0..2 {
                v = v * 8 + chars.next()?.to_digit(8)?;
            }
            bytes.push(u8::try_from(v).ok()?);
            continue;
        }
        flush(&mut bytes, &mut out);
        let ch = match escaped {
            'a' => '\u{7}',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\u{b}',
            '\\' => '\\',
            '"' => '"',
            '\'' => '\'',
            'u' => char::from_u32(hex(&mut chars, 4)?)?,
            'U' => char::from_u32(hex(&mut chars, 8)?)?,
            _ => return None,
        };
        out.push(ch);
    }
    flush(&mut bytes, &mut out);
    Some(out)
}

// ------------------------------------------------------------------ flags

/// Which bisync call to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// The planning pass: `--dry-run --force`, so the too-many-deletes
    /// check does not stop it before the plan is listed.
    Plan,
    /// The real run. `lift` passes `--max-delete 100` (only after the user
    /// confirmed a preview with those deletions); `force` passes `--force`
    /// (only after the user confirmed a preview where every file changed).
    Sync { lift: bool, force: bool },
    /// A first run or a recovery: `--resync --resync-mode path1`.
    Resync { dry_run: bool },
}

/// Everything a bisync call needs.
#[derive(Debug, Clone)]
pub struct Pair<'a> {
    /// The local source, absolute.
    pub path1: &'a str,
    /// The destination: an absolute path or `remote:path`.
    pub path2: &'a str,
    /// `bisync/<job>/<dest-id>` under the data directory.
    pub workdir: &'a Path,
    /// The excludes in force (built-in, hidden, the job's), the holds and
    /// [`CONFLICT_PATTERN`]. Always passed as repeated `--exclude` flags:
    /// never a `--filters-file`, whose hash bisync checks, and never an
    /// `--exclude-from` file either.
    pub excludes: &'a [String],
    /// `--fast-list`, for a OneDrive destination.
    pub fast_list: bool,
}

/// `rclone bisync` with OneFolder's flag set (DESIGN.md, Two-way sync).
pub fn args(pair: &Pair, call: Call) -> Vec<String> {
    let mut args = vec![
        "bisync".to_string(),
        pair.path1.to_string(),
        pair.path2.to_string(),
    ];
    let planning = call == Call::Plan || call == Call::Resync { dry_run: true };
    let mut retries = false;
    for flag in super::GLOBAL_FLAGS {
        // A planning pass gets one attempt, as the one-way preview does.
        let value = if retries && planning { "1" } else { flag };
        retries = flag == "--retries";
        args.push(value.to_string());
    }
    let lifted = matches!(call, Call::Sync { lift: true, .. });
    let max_delete = if lifted {
        MAX_DELETE_LIFTED
    } else {
        MAX_DELETE
    };
    for flag in [
        "--workdir".to_string(),
        pair.workdir.display().to_string(),
        "--conflict-resolve".into(),
        "path1".into(),
        "--conflict-loser".into(),
        "num".into(),
        "--conflict-suffix".into(),
        CONFLICT_SUFFIX.into(),
        "--suffix-keep-extension".into(),
        "--create-empty-src-dirs".into(),
        "--resilient".into(),
        "--recover".into(),
        "--max-delete".into(),
        max_delete.to_string(),
        "--compare".into(),
        "size,modtime".into(),
    ] {
        args.push(flag);
    }
    args.extend(super::exclude_args(pair.excludes, None));
    if pair.fast_list {
        args.push("--fast-list".into());
    }
    match call {
        Call::Plan => {
            args.push("--dry-run".into());
            args.push("--force".into());
        }
        Call::Sync { force, .. } => {
            if force {
                args.push("--force".into());
            }
        }
        Call::Resync { dry_run } => {
            args.extend(["--resync".into(), "--resync-mode".into(), "path1".into()]);
            if dry_run {
                args.push("--dry-run".into());
            }
        }
    }
    args
}

/// The exclude that keeps a held folder out of the sync: `/<folder>/**`,
/// anchored at the root, with rclone's glob characters escaped.
pub fn hold_exclude(folder: &str) -> String {
    format!("/{}/**", escape_glob(folder.trim_matches('/')))
}

/// Escapes the characters rclone's filter globs give a meaning.
pub fn escape_glob(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']' | '{' | '}') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The excludes of a bisync call: the job's, one per held folder, and
/// the conflict pattern.
pub fn excludes(job: &[String], holds: &[String]) -> Vec<String> {
    let mut out: Vec<String> = job.to_vec();
    out.extend(holds.iter().map(|h| hold_exclude(h)));
    out.push(CONFLICT_PATTERN.to_string());
    out
}

/// True when a path is a copy bisync made itself for a conflict it met in
/// the run ([`CONFLICT_SUFFIX`]: `.race-conflict-YYYY-MM-DD-n`).
pub fn is_race_conflict_copy(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.contains(".race-conflict-")
}

/// True when a path is a conflict copy: its name matches
/// [`CONFLICT_PATTERN`] (`.conflict-` then a date `YYYY-MM-DD` then `-`).
pub fn is_conflict_copy(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.match_indices(".conflict-").any(|(i, m)| {
        let rest = &name.as_bytes()[i + m.len()..];
        rest.len() >= 11
            && rest[..10].iter().enumerate().all(|(j, c)| {
                if j == 4 || j == 7 {
                    *c == b'-'
                } else {
                    c.is_ascii_digit()
                }
            })
            && rest[10] == b'-'
    })
}

// ------------------------------------------------------- abort messages

/// Why bisync stopped, from its messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Abort {
    /// The listings are missing, out of sync, or a critical error left them
    /// unusable: only a resync helps.
    NeedsResync { message: String },
    /// More than `percent`% of a side's files would be deleted.
    TooManyDeletes {
        side: Option<Side>,
        percent: u32,
        deleted: u64,
        total: u64,
    },
    /// Every file on a side changed (bisync's other safety abort).
    AllChanged { side: Option<Side> },
    /// A lock file is in the way.
    Locked { lock: Option<String> },
    /// A filters file is new or changed (OneFolder never passes one).
    FiltersChanged,
    /// A side is empty: an unmounted drive, or everything really went.
    EmptySide { side: Option<Side> },
    /// bisync names its working files after both paths, and together they
    /// pass the 255-byte limit on a file name (it then also misreports a
    /// lock file, so this is checked first).
    NamesTooLong,
}

/// Classifies bisync's messages (every [`Event::Fatal`] and error line of
/// a run), most specific first. `None` when nothing says bisync aborted.
pub fn classify<'a>(messages: impl IntoIterator<Item = &'a str>) -> Option<Abort> {
    let messages: Vec<&str> = messages.into_iter().collect();
    let find = |needle: &str| messages.iter().copied().find(|m| m.contains(needle));
    if find("file name too long").is_some() {
        return Some(Abort::NamesTooLong);
    }
    if let Some(m) = find("prior lock file found") {
        // `prior lock file found: <path>.lck` and then a tip.
        let lock = m
            .split_once("prior lock file found:")
            .and_then(|(_, p)| p.find(".lck").map(|end| p[..end + 4].trim().to_string()))
            .filter(|p| !p.is_empty());
        return Some(Abort::Locked { lock });
    }
    if let Some(m) = find("mpty current Path") {
        return Some(Abort::EmptySide {
            side: side_after(m, "mpty current "),
        });
    }
    if let Some(m) = find("too many deletes") {
        return Some(too_many_deletes(m));
    }
    if let Some(m) = find("all files were changed") {
        return Some(Abort::AllChanged {
            side: side_after(m, "changed on "),
        });
    }
    if find("filters file md5 hash not found").is_some()
        || find("filters file has changed").is_some()
    {
        return Some(Abort::FiltersChanged);
    }
    for needle in [
        "cannot find prior Path1 or Path2 listings",
        "out of sync",
        "Bisync critical error",
        "Must run --resync",
    ] {
        if let Some(m) = find(needle) {
            return Some(Abort::NeedsResync {
                message: m.to_string(),
            });
        }
    }
    None
}

/// `too many deletes (>50%, 6 of 10) on Path1 "/x/a/". Run with --force if desired.`
fn too_many_deletes(m: &str) -> Abort {
    let inside = m
        .split_once('(')
        .and_then(|(_, r)| r.split_once(')'))
        .map(|(i, _)| i)
        .unwrap_or("");
    let percent = inside
        .split_once('>')
        .and_then(|(_, r)| r.split_once('%'))
        .and_then(|(p, _)| p.trim().parse().ok())
        .unwrap_or(MAX_DELETE);
    let counts = inside.split_once(',').map(|(_, r)| r.trim()).unwrap_or("");
    let (deleted, total) = counts
        .split_once(" of ")
        .map(|(d, t)| (d.trim().parse().unwrap_or(0), t.trim().parse().unwrap_or(0)))
        .unwrap_or((0, 0));
    Abort::TooManyDeletes {
        side: side_after(m, ") on "),
        percent,
        deleted,
        total,
    }
}

fn side_after(m: &str, marker: &str) -> Option<Side> {
    let rest = m.split_once(marker)?.1;
    Side::parse(rest.get(..5)?)
}

/// rclone's rule for its too-many-deletes abort: strictly more than `limit`
/// percent of the side's previous listing (files and folders), with no
/// rounding (101 of 200 is over 50%: confirmed live on 1.75.2).
pub fn exceeds_max_delete(deleted: u64, total: u64, limit: u32) -> bool {
    total > 0 && deleted > 0 && deleted * 100 > total * u64::from(limit)
}

// --------------------------------------------------------------- listings

/// The longest suffix bisync adds to its working file names:
/// `.path1.lst-dry-new` (the listings are `.path1.lst`, with `-new`,
/// `-old`, `-err` and `-dry` forms; the lock is `.lck`).
pub const LONGEST_SUFFIX: &str = ".path1.lst-dry-new";

/// The longest file name macOS file systems allow, in bytes.
pub const NAME_MAX: usize = 255;

/// The base of bisync's working file names for a pair, as rclone makes it
/// (`bilib.SessionName`): each path with its leading and trailing slashes
/// trimmed and every slash, backslash, colon, `?`, `*` and white space
/// turned into `_`, joined by `..`. A remote keeps its name:
/// `onefolder-work:Backups` becomes `onefolder-work_Backups`.
pub fn session_name(path1: &str, path2: &str) -> String {
    fn canonical(path: &str) -> String {
        path.trim_matches(|c| c == '/' || c == '\\')
            .chars()
            .map(|c| {
                if c.is_whitespace() || matches!(c, '/' | '\\' | ':' | '?' | '*') {
                    '_'
                } else {
                    c
                }
            })
            .collect()
    }
    format!("{}..{}", canonical(path1), canonical(path2))
}

/// True when bisync's working file names for this pair would pass
/// [`NAME_MAX`] bytes, which makes it fail with "file name too long".
pub fn names_too_long(path1: &str, path2: &str) -> bool {
    session_name(path1, path2).len() + LONGEST_SUFFIX.len() > NAME_MAX
}

/// One line of a `.path1.lst` or `.path2.lst` listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListingEntry {
    pub path: String,
    /// Bytes; -1 for a folder.
    pub size: i64,
    pub is_dir: bool,
    pub modtime: String,
}

/// Parses a listing: `# bisync listing v1 from ...` then one line per
/// entry, `<type> <size> <hash> <id> <modtime> "<path>"`.
pub fn parse_listing(text: &str) -> Result<Vec<ListingEntry>, String> {
    let mut entries = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("line {}: not a bisync listing line: {line}", n + 1);
        let quote = line.find('"').ok_or_else(bad)?;
        let fields: Vec<&str> = line[..quote].split_whitespace().collect();
        if fields.len() != 5 {
            return Err(bad());
        }
        let path_text = line[quote..].trim_end();
        if path_text.len() < 2 || !path_text.ends_with('"') {
            return Err(bad());
        }
        let path = go_unquote(&path_text[1..path_text.len() - 1]).ok_or_else(bad)?;
        entries.push(ListingEntry {
            path,
            size: fields[1].parse().map_err(|_| bad())?,
            is_dir: fields[0] == "d",
            modtime: fields[4].to_string(),
        });
    }
    Ok(entries)
}

/// The prior listing of one side in a working directory (the newest, if a
/// renamed pair left more than one), or else bisync's backup of it
/// (`.lst-old`), which `--recover` reverts to by itself when the listing
/// is missing (confirmed on 1.75.2).
pub fn find_listing(workdir: &Path, side: Side) -> Option<PathBuf> {
    let suffix = match side {
        Side::Path1 => ".path1.lst",
        Side::Path2 => ".path2.lst",
    };
    let backup = format!("{suffix}-old");
    newest(workdir, |name| name.ends_with(suffix))
        .or_else(|| newest(workdir, |name| name.ends_with(&backup)))
}

/// True when the working directory has both prior listings.
pub fn has_listings(workdir: &Path) -> bool {
    find_listing(workdir, Side::Path1).is_some() && find_listing(workdir, Side::Path2).is_some()
}

/// Reads and parses one side's prior listing.
pub fn read_listing(workdir: &Path, side: Side) -> Result<Vec<ListingEntry>, String> {
    let path = find_listing(workdir, side)
        .ok_or_else(|| format!("no {} listing in {}", side.as_str(), workdir.display()))?;
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_listing(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn newest(dir: &Path, wanted: impl Fn(&str) -> bool) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        if !wanted(&name.to_string_lossy()) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| modified >= *t) {
            best = Some((modified, entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

// ------------------------------------------------------------------ locks

/// The lock in a working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lock {
    /// No lock file.
    Free,
    /// A lock held by a running rclone.
    Held { pid: i32, path: PathBuf },
    /// A lock whose process is gone, is not rclone (the id was reused), or
    /// was last renewed before the Mac started: safe to remove.
    Stale { pid: i32, path: PathBuf },
    /// A lock whose process id cannot be read: left for the user.
    Unreadable { path: PathBuf },
}

/// What a lock file says: the process id and when it was last renewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockInfo {
    pub pid: i32,
    /// Unix seconds of `TimeRenewed`, when it reads.
    pub renewed: Option<i64>,
}

/// Reads a lock file: rclone 1.75 writes JSON with `"PID"` as a string and
/// `"TimeRenewed"` in RFC 3339; older releases wrote the bare number.
pub fn lock_info(text: &str) -> Option<LockInfo> {
    let text = text.trim();
    let (value, renewed) = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(map)) => (
            map.get("PID")?.clone(),
            map.get("TimeRenewed")
                .and_then(|t| t.as_str())
                .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                .map(|t| t.as_second()),
        ),
        Ok(other) => (other, None),
        Err(_) => (serde_json::Value::String(text.to_string()), None),
    };
    let pid = match value {
        serde_json::Value::String(s) => s.trim().parse().ok(),
        serde_json::Value::Number(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()),
        _ => None,
    };
    pid.filter(|p: &i32| *p > 0)
        .map(|pid| LockInfo { pid, renewed })
}

/// The process id in a lock file.
#[cfg(test)]
pub fn lock_pid(text: &str) -> Option<i32> {
    lock_info(text).map(|l| l.pid)
}

/// True when a process with this id exists (`kill(pid, 0)`): a process we
/// may not signal still exists. Never true for 0 or less, which `kill`
/// reads as process groups.
#[cfg(unix)]
pub fn pid_alive(pid: i32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    if pid <= 0 {
        return false;
    }
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}

#[cfg(not(unix))]
pub fn pid_alive(pid: i32) -> bool {
    pid > 0
}

/// Whether a lock still belongs to a running bisync. `command` is the
/// process's name (`ps -o comm=`) and `boot` the Mac's start, in Unix
/// seconds, each when known. A process id is reused after a restart, so a
/// lock renewed before the start, or one whose process is not rclone, is
/// stale whatever `kill` says.
pub fn lock_is_live(info: LockInfo, alive: bool, command: Option<&str>, boot: Option<i64>) -> bool {
    if !alive {
        return false;
    }
    if let (Some(renewed), Some(boot)) = (info.renewed, boot) {
        if renewed < boot {
            return false;
        }
    }
    command.is_none_or(|c| c.to_ascii_lowercase().contains("rclone"))
}

/// The name of a running process (`ps -p <pid> -o comm=`).
pub(crate) async fn process_name(pid: i32) -> Option<String> {
    let pid = pid.to_string();
    let out = super::exec::run_capture(Path::new("/bin/ps"), &["-p", &pid, "-o", "comm="])
        .await
        .ok()?;
    let name = out.stdout.trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// When the Mac started (`sysctl -n kern.boottime`: `{ sec = 1760313600,
/// usec = 0 } ...`), in Unix seconds.
pub(crate) async fn boot_time() -> Option<i64> {
    let out = super::exec::run_capture(Path::new("/usr/sbin/sysctl"), &["-n", "kern.boottime"])
        .await
        .ok()?;
    parse_boot_time(&out.stdout)
}

/// Reads `sysctl -n kern.boottime` output.
pub fn parse_boot_time(text: &str) -> Option<i64> {
    let rest = text.split_once("sec =")?.1;
    rest.trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Looks for a lock file in the working directory and says whose it is.
pub async fn check_lock(workdir: &Path) -> Lock {
    let Some(path) = newest(workdir, |name| name.ends_with(".lck")) else {
        return Lock::Free;
    };
    let Some(info) = fs::read_to_string(&path)
        .ok()
        .as_deref()
        .and_then(lock_info)
    else {
        return Lock::Unreadable { path };
    };
    let alive = pid_alive(info.pid);
    let (command, boot) = if alive {
        (process_name(info.pid).await, boot_time().await)
    } else {
        (None, None)
    };
    if lock_is_live(info, alive, command.as_deref(), boot) {
        Lock::Held {
            pid: info.pid,
            path,
        }
    } else {
        Lock::Stale {
            pid: info.pid,
            path,
        }
    }
}

/// Removes a stale lock file.
pub fn remove_lock(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rclone::events::{self, Event};

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rclone")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn plan(side: Side, verb: PlanVerb, path: &str, detail: Option<&str>) -> Option<Event> {
        Some(Event::Plan(PlanLine {
            side,
            verb,
            path: path.into(),
            detail: detail.map(String::from),
        }))
    }

    #[test]
    fn plan_lines() {
        assert_eq!(
            parse_line(
                "- Path1             File was deleted                            - sub/f1.txt"
            ),
            plan(Side::Path1, PlanVerb::Deleted, "sub/f1.txt", None)
        );
        assert_eq!(
            parse_line(
                "- Path2             File is new                                 - newremote.txt"
            ),
            plan(Side::Path2, PlanVerb::New, "newremote.txt", None)
        );
        assert_eq!(
            parse_line(
                "- Path1             File changed: size (larger), time (newer)   - conf.txt"
            ),
            plan(
                Side::Path1,
                PlanVerb::Changed,
                "conf.txt",
                Some("size (larger), time (newer)")
            )
        );
        // A path with spaces, and one with " - " in it, split at the first.
        assert_eq!(
            parse_line("- Path1             File was deleted                            - Old projects/2019/a - b.txt"),
            plan(Side::Path1, PlanVerb::Deleted, "Old projects/2019/a - b.txt", None)
        );
        // Spaces at either end of a name are kept (bisync prints the path
        // as it is), here a folder "Old " and one " Lead".
        assert_eq!(
            parse_line(
                "- Path1             File was deleted                            - Old /a.txt"
            ),
            plan(Side::Path1, PlanVerb::Deleted, "Old /a.txt", None)
        );
        assert_eq!(
            parse_line("- Path1             File was deleted                            -  Lead"),
            plan(Side::Path1, PlanVerb::Deleted, " Lead", None)
        );
        assert_eq!(
            parse_line("- Path1             File was deleted                            -  c \n"),
            plan(Side::Path1, PlanVerb::Deleted, " c ", None)
        );
        assert_eq!(
            parse_line("Files are equal! Skipping:  c "),
            Some(Event::NoConflict { path: " c ".into() })
        );
        // Go-quoted when the name needs it.
        assert_eq!(
            parse_line(
                r#"- Path1             File was deleted                            - "say \"hi\".txt""#
            ),
            plan(Side::Path1, PlanVerb::Deleted, r#"say "hi".txt"#, None)
        );
        assert_eq!(
            parse_line(
                "- WARNING           New or changed in both paths                - conf.txt"
            ),
            Some(Event::Conflict {
                path: "conf.txt".into()
            })
        );
        assert_eq!(
            parse_line("- Path2             Queue copy to Path1                         - /x/a/conf.txt.conflict-2026-10-131"),
            Some(Event::Queued(Queued {
                on: Side::Path1,
                action: QueuedAction::Copy,
                path: "/x/a/conf.txt.conflict-2026-10-131".into()
            }))
        );
        assert_eq!(
            parse_line(
                "- Path2             Queue delete                                - /x/b/del.txt"
            ),
            Some(Event::Queued(Queued {
                on: Side::Path2,
                action: QueuedAction::Delete,
                path: "/x/b/del.txt".into()
            }))
        );
        assert_eq!(
            parse_line("- Path2             Renaming Path2 copy                         - /x/b/plan.conflict-2026-10-13-1.md"),
            Some(Event::Queued(Queued {
                on: Side::Path2,
                action: QueuedAction::Rename,
                path: "/x/b/plan.conflict-2026-10-13-1.md".into()
            }))
        );
        assert_eq!(
            parse_line("- Path2             Do queued copies to                         - Path1"),
            Some(Event::Applying { to: Side::Path1 })
        );
        assert_eq!(
            parse_line("- Path1             Resync is copying files to                  - Path2"),
            Some(Event::Applying { to: Side::Path2 })
        );
        // Other bisync lines are left as notices.
        assert_eq!(
            parse_line("- Path1             Not renaming Path1 copy, as it was determined the winner - /x/a/conf.txt"),
            None
        );
        assert_eq!(parse_line("- nothing here"), None);
        assert_eq!(parse_line("Copied (new)"), None);
    }

    #[test]
    fn phases_counts_and_fatal_lines() {
        assert_eq!(
            parse_line("Building Path1 and Path2 listings"),
            Some(Event::Phase {
                phase: BisyncPhase::Listing
            })
        );
        assert_eq!(
            parse_line("Path2 checking for diffs"),
            Some(Event::Phase {
                phase: BisyncPhase::Checking
            })
        );
        assert_eq!(
            parse_line("Resync updating listings"),
            Some(Event::Phase {
                phase: BisyncPhase::Updating
            })
        );
        assert_eq!(
            parse_line("Path1:   10 changes:    1 new,    2 modified,    7 deleted"),
            Some(Event::Changes(SideChanges {
                side: Side::Path1,
                changes: 10,
                new: 1,
                modified: 2,
                deleted: 7
            }))
        );
        assert!(matches!(
            parse_line("Bisync aborted. Please try again."),
            Some(Event::Fatal { .. })
        ));
        assert!(matches!(
            parse_line(
                r#"too many deletes (>50%, 6 of 10) on Path1 "/x/a/". Run with --force if desired."#
            ),
            Some(Event::Fatal { .. })
        ));
        assert_eq!(parse_line("Bisync successful"), None);
        assert_eq!(
            parse_line("Files are equal! Skipping: Old projects/2019/a.txt"),
            Some(Event::NoConflict {
                path: "Old projects/2019/a.txt".into()
            })
        );
        assert_eq!(
            parse_line(
                "This is a directory, not a file. Skipping equality check and will not rename: Old projects"
            ),
            Some(Event::NoConflict {
                path: "Old projects".into()
            })
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(unquote("plain name.txt"), "plain name.txt");
        assert_eq!(unquote(r#""back\\slash.txt""#), r"back\slash.txt");
        assert_eq!(unquote(r#""tab\tname""#), "tab\tname");
        assert_eq!(unquote(r#""café""#), "café");
        assert_eq!(unquote(r#""caf\xc3\xa9""#), "café");
        // Not a Go string: kept as it is.
        assert_eq!(unquote(r#""a"b""#), r#""a"b""#);
        assert_eq!(unquote(r#""bad\q""#), r#""bad\q""#);
    }

    /// Every abort message recorded from rclone 1.75.2.
    #[test]
    fn abort_classification_of_the_recorded_messages() {
        let fatal = |name: &str| -> Vec<String> {
            events::decode_all(&fixture(name))
                .into_iter()
                .filter_map(|e| match e {
                    Event::Fatal { message } => Some(message),
                    Event::Error(err) => Some(err.message),
                    _ => None,
                })
                .collect()
        };
        let class = |name: &str| {
            let messages = fatal(name);
            classify(messages.iter().map(String::as_str))
        };
        assert_eq!(
            class("bisync-too-many-deletes.jsonl"),
            Some(Abort::TooManyDeletes {
                side: Some(Side::Path1),
                percent: 50,
                deleted: 6,
                total: 10
            })
        );
        assert!(matches!(
            class("bisync-missing-listings.jsonl"),
            Some(Abort::NeedsResync { message }) if message.contains("cannot find prior Path1 or Path2 listings")
        ));
        assert!(matches!(
            class("bisync-locked.jsonl"),
            Some(Abort::Locked { lock: Some(l) }) if l.ends_with(".lck")
        ));
        assert_eq!(
            class("bisync-filters-file.jsonl"),
            Some(Abort::FiltersChanged)
        );
        assert_eq!(
            class("bisync-empty-side.jsonl"),
            Some(Abort::EmptySide {
                side: Some(Side::Path2)
            })
        );
        assert_eq!(
            class("bisync-all-changed.jsonl"),
            Some(Abort::AllChanged {
                side: Some(Side::Path1)
            })
        );
        assert!(matches!(
            class("bisync-missing-destination.jsonl"),
            Some(Abort::NeedsResync { .. })
        ));
        assert_eq!(
            class("bisync-name-too-long.jsonl"),
            Some(Abort::NamesTooLong)
        );
        // A forced run, a clean run and an interrupted run are not aborts.
        for clean in [
            "bisync-too-many-deletes-force.jsonl",
            "bisync-plan.jsonl",
            "bisync-sync.jsonl",
            "bisync-resync.jsonl",
            "bisync-sigint.jsonl",
            "bisync-recover.jsonl",
            "bisync-own-conflict.jsonl",
            "bisync-race-conflict.jsonl",
        ] {
            assert_eq!(class(clean), None, "{clean}");
        }
    }

    #[test]
    fn classification_without_fixtures() {
        assert_eq!(
            classify(["path1 and path2 are out of sync, run --resync to recover"]),
            Some(Abort::NeedsResync {
                message: "path1 and path2 are out of sync, run --resync to recover".into()
            })
        );
        assert_eq!(classify(["Failed to bisync: context canceled"]), None);
        assert_eq!(
            classify(["too many deletes (>50%, 51 of 100) on Path2 \"x\""]),
            Some(Abort::TooManyDeletes {
                side: Some(Side::Path2),
                percent: 50,
                deleted: 51,
                total: 100
            })
        );
        assert_eq!(
            classify(["too many deletes"]),
            Some(Abort::TooManyDeletes {
                side: None,
                percent: 50,
                deleted: 0,
                total: 0
            })
        );
    }

    #[test]
    fn max_delete_rule() {
        assert!(!exceeds_max_delete(5, 10, 50));
        assert!(exceeds_max_delete(6, 10, 50));
        assert!(!exceeds_max_delete(4, 10, 50));
        // Strictly more, no rounding: 50.5% is over 50%.
        assert!(exceeds_max_delete(101, 200, 50));
        assert!(!exceeds_max_delete(100, 200, 50));
        assert!(!exceeds_max_delete(0, 0, 50));
        assert!(!exceeds_max_delete(10, 10, 100));
    }

    #[test]
    fn listing_fixture() {
        let entries = parse_listing(&fixture("bisync.path1.lst")).unwrap();
        assert!(!entries.is_empty());
        let dir = entries.iter().find(|e| e.path == "Old projects").unwrap();
        assert!(dir.is_dir && dir.size == -1);
        let file = entries
            .iter()
            .find(|e| e.path == "Old projects/2019/a b.txt")
            .unwrap();
        assert!(!file.is_dir && file.size > 0);
        assert!(file.modtime.starts_with("20"));
        assert!(entries.iter().any(|e| e.path == r#"say "hi".txt"#));
        assert!(entries.iter().any(|e| e.path == r"back\slash.txt"));
    }

    #[test]
    fn listing_lines() {
        let text = "# bisync listing v1 from 2026-10-13T04:46:45.429907000+0000\n\
            -        2 - - 2026-10-13T04:46:33.858493642+0000 \"conf.txt\"\n\
            d       -1 - - 2026-10-13T04:46:33.861391598+0000 \"keep\"\n\
            -       10 md5:abc - 2026-10-13T04:46:33.858493642+0000 \"with hash.txt\"\n\n";
        let entries = parse_listing(text).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "conf.txt");
        assert_eq!(entries[0].size, 2);
        assert!(entries[1].is_dir);
        assert_eq!(entries[2].path, "with hash.txt");
        assert!(parse_listing("- 2 - - t no-quotes").is_err());
        assert!(parse_listing("- x - - t \"a\"").is_err());
        assert!(parse_listing("- 2 - t \"a\"").is_err());
        assert!(parse_listing("- 2 - - t \"open").is_err());
        assert_eq!(parse_listing("").unwrap(), vec![]);
    }

    #[test]
    fn listings_in_a_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_listings(dir.path()));
        fs::write(dir.path().join("a..b.path1.lst"), "").unwrap();
        fs::write(dir.path().join("a..b.path1.lst-new"), "").unwrap();
        assert!(!has_listings(dir.path()));
        // A backup counts: --recover reverts to it.
        fs::write(dir.path().join("a..b.path2.lst-old"), "").unwrap();
        assert!(has_listings(dir.path()));
        assert_eq!(
            find_listing(dir.path(), Side::Path2).unwrap(),
            dir.path().join("a..b.path2.lst-old")
        );
        fs::write(dir.path().join("a..b.path2.lst"), "").unwrap();
        assert_eq!(
            find_listing(dir.path(), Side::Path2).unwrap(),
            dir.path().join("a..b.path2.lst")
        );
        assert_eq!(
            find_listing(dir.path(), Side::Path1).unwrap(),
            dir.path().join("a..b.path1.lst")
        );
        assert!(!has_listings(&dir.path().join("missing")));
    }

    #[test]
    fn lock_pids() {
        assert_eq!(
            lock_pid(r#"{"Session":"/x","PID":"63775","TimeRenewed":"t","TimeExpires":"t"}"#),
            Some(63775)
        );
        assert_eq!(lock_pid(r#"{"PID":42}"#), Some(42));
        assert_eq!(lock_pid("1234\n"), Some(1234));
        assert_eq!(lock_pid(r#"{"PID":"0"}"#), None);
        assert_eq!(lock_pid("-1"), None);
        assert_eq!(lock_pid(r#"{"Session":"x"}"#), None);
        assert_eq!(lock_pid("garbage"), None);
        // The recorded lock file.
        assert!(lock_pid(&fixture("bisync.lck")).is_some());
    }

    #[test]
    fn lock_liveness_rules() {
        let info = LockInfo {
            pid: 42,
            renewed: Some(2_000),
        };
        assert!(lock_is_live(
            info,
            true,
            Some("/opt/homebrew/bin/rclone"),
            Some(1_000)
        ));
        assert!(
            !lock_is_live(info, false, Some("rclone"), Some(1_000)),
            "gone"
        );
        assert!(
            !lock_is_live(info, true, Some("/usr/bin/vim"), Some(1_000)),
            "the id was reused by another program"
        );
        assert!(
            !lock_is_live(info, true, Some("rclone"), Some(3_000)),
            "renewed before the Mac started"
        );
        // Unknown name or start time: trust kill(pid, 0).
        assert!(lock_is_live(info, true, None, None));
        assert_eq!(
            parse_boot_time("{ sec = 1791756928, usec = 112129 } Mon Oct 12 09:15:28 2026\n"),
            Some(1_791_756_928)
        );
        assert_eq!(parse_boot_time("nonsense"), None);
        let recorded = lock_info(&fixture("bisync.lck")).unwrap();
        assert!(recorded.renewed.is_some());
    }

    #[tokio::test]
    async fn alive_and_stale_locks() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(check_lock(dir.path()).await, Lock::Free);
        let lock = dir.path().join("a..b.lck");
        // A running process that is not rclone (this test): its id was reused.
        let me = i32::try_from(std::process::id()).unwrap();
        fs::write(&lock, format!(r#"{{"PID":"{me}"}}"#)).unwrap();
        assert_eq!(
            check_lock(dir.path()).await,
            Lock::Stale {
                pid: me,
                path: lock.clone()
            }
        );
        // A process that has exited.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let gone = i32::try_from(child.id()).unwrap();
        child.wait().unwrap();
        fs::write(&lock, format!(r#"{{"PID":"{gone}"}}"#)).unwrap();
        assert_eq!(
            check_lock(dir.path()).await,
            Lock::Stale {
                pid: gone,
                path: lock.clone()
            }
        );
        // A running rclone holds it (when rclone is installed).
        if let Ok(rclone) = crate::rclone::detect::detect().await {
            let mut rcd = std::process::Command::new(&rclone.path)
                .args(["rcd", "--rc-addr", "127.0.0.1:0", "--rc-no-auth"])
                .env("RCLONE_CONFIG", "/dev/null")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let pid = i32::try_from(rcd.id()).unwrap();
            fs::write(
                &lock,
                format!(
                    r#"{{"PID":"{pid}","TimeRenewed":"{}"}}"#,
                    jiff::Timestamp::now()
                ),
            )
            .unwrap();
            let held = check_lock(dir.path()).await;
            let _ = rcd.kill();
            let _ = rcd.wait();
            assert_eq!(
                held,
                Lock::Held {
                    pid,
                    path: lock.clone()
                }
            );
        }
        remove_lock(&lock).unwrap();
        assert_eq!(check_lock(dir.path()).await, Lock::Free);
        remove_lock(&lock).unwrap();
        fs::write(&lock, "not a lock").unwrap();
        assert_eq!(
            check_lock(dir.path()).await,
            Lock::Unreadable { path: lock }
        );
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
    }

    #[test]
    fn working_file_names() {
        assert_eq!(
            session_name(
                "/Users/bora/My Documents",
                "onefolder-work:Backups/Documents"
            ),
            "Users_bora_My_Documents..onefolder-work_Backups_Documents"
        );
        // The recorded fixture: bisync failed on these paths.
        let dir = format!("/fixture/bl/{}", "long-folder-name-".repeat(7));
        let (src, dst) = (format!("{dir}/src"), format!("{dir}/dst"));
        assert!(names_too_long(&src, &dst));
        assert!(!names_too_long(
            "/Users/bora/Documents",
            "onefolder-work:Backups/Documents"
        ));
        let edge = "a".repeat(NAME_MAX - LONGEST_SUFFIX.len() - 3);
        assert!(!names_too_long(&edge, "b"));
        assert!(names_too_long(&format!("{edge}x"), "b"));
    }

    #[test]
    fn hold_excludes() {
        assert_eq!(hold_exclude("Old projects/2019"), "/Old projects/2019/**");
        assert_eq!(hold_exclude("/trimmed/"), "/trimmed/**");
        assert_eq!(hold_exclude("Photos [2019]"), r"/Photos \[2019\]/**");
        assert_eq!(hold_exclude("a*b?{c}"), r"/a\*b\?\{c\}/**");
        assert_eq!(hold_exclude(r"x\y"), r"/x\\y/**");
        assert_eq!(
            excludes(&[".DS_Store".into()], &["Old".into()]),
            [".DS_Store", "/Old/**", CONFLICT_PATTERN]
        );
        assert!(is_conflict_copy("Notes/plan.conflict-2026-10-13-1.md"));
        assert!(is_conflict_copy("noext.conflict-2026-10-13-12"));
        assert!(!is_conflict_copy("conflict-notes/plan.md"));
        assert!(!is_conflict_copy("talk.conflict-resolution.md"));
        assert!(!is_conflict_copy("a.conflict-2026-10-13.md"));
    }

    fn pair(dir: &Path) -> Pair<'_> {
        Pair {
            path1: "/Users/bora/Documents",
            path2: "onefolder-work:Backups/Documents",
            workdir: dir,
            excludes: &[],
            fast_list: true,
        }
    }

    #[test]
    fn flag_sets() {
        let wd = Path::new("/d/bisync/documents/d-7f3a");
        let ex = excludes(&[".DS_Store".into()], &["Old projects/2019".into()]);
        let mut p = pair(wd);
        p.excludes = &ex;
        let snap = |call| args(&p, call).join(" ");
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!("bisync_flags", [
                snap(Call::Plan),
                snap(Call::Sync { lift: false, force: false }),
                snap(Call::Sync { lift: true, force: true }),
                snap(Call::Resync { dry_run: false }),
                snap(Call::Resync { dry_run: true }),
            ].join("\n\n"));
        });
        // However many patterns, bisync gets them as flags, never a file.
        let many: Vec<String> = crate::rclone::effective_excludes(
            &(0..30).map(|i| format!("*.x{i}")).collect::<Vec<_>>(),
            false,
        );
        let local = Pair {
            path2: "/Volumes/Backup/Documents",
            fast_list: false,
            excludes: &many,
            ..pair(wd)
        };
        let a = args(
            &local,
            Call::Sync {
                lift: false,
                force: false,
            },
        );
        assert!(!a.contains(&"--fast-list".to_string()));
        assert!(!a.contains(&"--filters-file".to_string()));
        assert!(!a.join(" ").contains("--exclude-from"));
        assert_eq!(
            a.iter().filter(|x| *x == "--exclude").count(),
            many.len(),
            "one flag per pattern"
        );
        assert!(a.join(" ").contains("--exclude .DS_Store --exclude ._*"));
        assert!(a.join(" ").contains("--max-delete 50"));
        assert!(!a.contains(&"--dry-run".to_string()));
    }
}
