//! `lsjson` and `size` helpers: the destination listing for the one-way
//! preview and the two-way merge, folder sizes for the folder-deletion
//! prompt, and the existence checks and `mkdir` around a first two-way run.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::detect::Rclone;
use super::exec::{self, ExecError, ExitKind};
use super::remotes::clean_message;

/// One entry of `rclone lsjson`. Only the fields OneFolder uses are read;
/// `MimeType` and the rest are ignored. `Hashes` is read leniently: only
/// string values are kept, under lower-case names, and anything else in
/// it is dropped rather than failing the listing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Path relative to the listed root, with `/` separators.
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Size", default)]
    pub size: i64,
    #[serde(rename = "ModTime", default)]
    pub mod_time: String,
    #[serde(rename = "IsDir", default)]
    pub is_dir: bool,
    /// Hashes by type (`quickxor`), present only when the listing asked
    /// for them (`--hash-type`).
    #[serde(
        rename = "Hashes",
        default,
        deserialize_with = "lenient_hashes",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub hashes: BTreeMap<String, String>,
}

impl Entry {
    /// The entry's QuickXorHash, when the listing carried one.
    pub fn quickxor(&self) -> Option<&str> {
        self.hashes
            .get(QUICKXOR)
            .map(String::as_str)
            .filter(|h| !h.is_empty())
    }
}

/// The hash OneDrive keeps for every file, and the one move detection
/// compares (`rclone hashsum quickxor` gives the same for a local file).
pub const QUICKXOR: &str = "quickxor";

fn lenient_hashes<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    let mut out = BTreeMap::new();
    if let serde_json::Value::Object(map) = value {
        for (k, v) in map {
            if let serde_json::Value::String(h) = v {
                out.insert(k.to_ascii_lowercase(), h.to_ascii_lowercase());
            }
        }
    }
    Ok(out)
}

/// What `rclone size --json` reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub bytes: i64,
}

#[derive(Debug, Error)]
pub enum ListError {
    #[error(transparent)]
    Exec(#[from] ExecError),
    /// rclone exited 3: the folder does not exist (yet).
    #[error("{target} does not exist")]
    NotFound { target: String },
    #[error("rclone {command} failed: {message}")]
    Failed {
        command: &'static str,
        message: String,
    },
    #[error("could not read the output of rclone {command}: {detail}")]
    Unreadable {
        command: &'static str,
        detail: String,
    },
}

/// Parses `rclone lsjson` output.
pub fn parse_lsjson(text: &str) -> Result<Vec<Entry>, String> {
    serde_json::from_str(text.trim()).map_err(|e| e.to_string())
}

/// Parses `rclone size --json` output.
pub fn parse_size(text: &str) -> Result<Size, String> {
    serde_json::from_str(text.trim()).map_err(|e| e.to_string())
}

/// Every file under `target` (a local path or `remote:path`), recursively:
/// `rclone lsjson -R --files-only`, with the job's excludes passed the same
/// way as to the copy (flags, or the same `--exclude-from` file) so the
/// listing sees what the copy sees, and `--fast-list` for a OneDrive
/// destination.
///
/// rclone prints one entry per line while it lists, so the output is read
/// as it comes and `listed`, when given, counts the entries so far: a long
/// listing shows progress instead of a spinner. The result is parsed once
/// the listing is complete, as before.
///
/// With `hash`, each entry carries its QuickXorHash (`--hash-type
/// quickxor`). Only for a OneDrive destination, which keeps the hash with
/// the file and lists it at no cost; on a local folder it would read every
/// file.
pub async fn list_files(
    rclone: &Rclone,
    target: &str,
    excludes: &[String],
    exclude_file: Option<&Path>,
    fast_list: bool,
    hash: bool,
    listed: Option<&AtomicU64>,
) -> Result<Vec<Entry>, ListError> {
    let mut args: Vec<String> = vec![
        "lsjson".into(),
        "-R".into(),
        "--files-only".into(),
        "--color".into(),
        "never".into(),
    ];
    if fast_list {
        args.push("--fast-list".into());
    }
    if hash {
        args.push("--hash-type".into());
        args.push(QUICKXOR.into());
    }
    args.extend(super::exclude_args(excludes, exclude_file));
    args.push(target.to_string());
    list_streaming(Path::new(&rclone.path), &args, target, listed).await
}

/// Every file under `target` that `filters` (`--filter` arguments, such
/// as [`super::folder_filter_args`]) let through: `rclone lsjson -R
/// --files-only`.
pub async fn list_filtered(
    rclone: &Rclone,
    target: &str,
    filters: &[String],
    fast_list: bool,
) -> Result<Vec<Entry>, ListError> {
    let mut args: Vec<String> = vec![
        "lsjson".into(),
        "-R".into(),
        "--files-only".into(),
        "--color".into(),
        "never".into(),
    ];
    if fast_list {
        args.push("--fast-list".into());
    }
    args.extend(filters.iter().cloned());
    args.push(target.to_string());
    list_streaming(Path::new(&rclone.path), &args, target, None).await
}

/// Deletes the folder `target` and everything in it, with no filter at
/// all: `rclone purge`. Only for a folder the user chose to delete.
pub async fn purge(rclone: &Rclone, target: &str) -> Result<(), ListError> {
    let args: Vec<String> = vec![
        "purge".into(),
        "--color".into(),
        "never".into(),
        target.into(),
    ];
    let out = capture(rclone, &args).await?;
    match ExitKind::from_code(out.status.code()) {
        ExitKind::Success => Ok(()),
        ExitKind::DirectoryNotFound => Err(ListError::NotFound {
            target: target.to_string(),
        }),
        _ => Err(ListError::Failed {
            command: "purge",
            message: clean_message(&out.stderr),
        }),
    }
}

/// True when a line of `rclone lsjson` output is one entry. rclone writes
/// `[`, then one `{...},` per line, then `]`.
pub fn is_entry_line(line: &str) -> bool {
    line.trim_start().starts_with('{')
}

/// Runs an `lsjson` command line, counting its entries into `listed` as
/// they arrive, and reads the result like [`list_files`].
async fn list_streaming(
    program: &Path,
    args: &[String],
    target: &str,
    listed: Option<&AtomicU64>,
) -> Result<Vec<Entry>, ListError> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut count = |line: &str| {
        if let (Some(listed), true) = (listed, is_entry_line(line)) {
            listed.fetch_add(1, Ordering::Relaxed);
        }
    };
    let out = exec::run_capture_lines(program, &args, &mut count).await?;
    match ExitKind::from_code(out.status.code()) {
        ExitKind::Success => {}
        ExitKind::DirectoryNotFound => {
            return Err(ListError::NotFound {
                target: target.to_string(),
            })
        }
        _ => {
            return Err(ListError::Failed {
                command: "lsjson",
                message: clean_message(&out.stderr),
            })
        }
    }
    parse_lsjson(&out.stdout).map_err(|detail| ListError::Unreadable {
        command: "lsjson",
        detail,
    })
}

/// The QuickXorHash of each of `paths` (relative to `root`, a local
/// folder or `remote:path`): `rclone hashsum quickxor --files-from-raw -
/// <root>`, with the paths on standard input, so only those files are
/// read. A file that is missing or cannot be hashed is simply absent from
/// the result. Paths with a line break cannot be listed this way and are
/// left out.
pub async fn hashsum(
    rclone: &Rclone,
    root: &str,
    paths: &[String],
) -> Result<HashMap<String, String>, ListError> {
    let mut input = String::new();
    for p in paths.iter().filter(|p| !p.contains(['\n', '\r'])) {
        input.push_str(p);
        input.push('\n');
    }
    if input.is_empty() {
        return Ok(HashMap::new());
    }
    let args = [
        "hashsum",
        QUICKXOR,
        "--files-from-raw",
        "-",
        "--color",
        "never",
        root,
    ];
    let out = exec::run_capture_input(Path::new(&rclone.path), &args, input.as_bytes()).await?;
    let hashes = parse_hashsum(&out.stdout);
    if hashes.is_empty() && !out.status.success() {
        return Err(ListError::Failed {
            command: "hashsum",
            message: clean_message(&out.stderr),
        });
    }
    Ok(hashes)
}

/// Reads `rclone hashsum` output: `<hash>  <path>` per line. The path is
/// everything after the first two spaces, kept as it is (leading spaces
/// included). Lines whose hash is not hexadecimal (`UNSUPPORTED`,
/// `ERROR`) are skipped.
pub fn parse_hashsum(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some((hash, path)) = line.split_once("  ") else {
            continue;
        };
        if hash.is_empty() || path.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        out.insert(path.to_string(), hash.to_ascii_lowercase());
    }
    out
}

/// File count and bytes under `target` (a folder or one file) that
/// `filters` (`--filter` arguments, or none) let through: `rclone size
/// --json`.
pub async fn size(rclone: &Rclone, target: &str, filters: &[String]) -> Result<Size, ListError> {
    let mut args: Vec<String> = vec![
        "size".into(),
        "--json".into(),
        "--color".into(),
        "never".into(),
    ];
    args.extend(filters.iter().cloned());
    args.push(target.into());
    let out = capture(rclone, &args).await?;
    match ExitKind::from_code(out.status.code()) {
        ExitKind::Success => {}
        ExitKind::DirectoryNotFound => {
            return Err(ListError::NotFound {
                target: target.to_string(),
            })
        }
        _ => {
            return Err(ListError::Failed {
                command: "size",
                message: clean_message(&out.stderr),
            })
        }
    }
    parse_size(&out.stdout).map_err(|detail| ListError::Unreadable {
        command: "size",
        detail,
    })
}

/// The file names directly in the folder `target`, with no filters at all
/// (so conflict copies and excluded files count): `rclone lsjson
/// --files-only --max-depth 1`. A folder that does not exist is empty.
pub async fn names_in(rclone: &Rclone, target: &str) -> Result<Vec<String>, ListError> {
    let args: Vec<String> = vec![
        "lsjson".into(),
        "--files-only".into(),
        "--max-depth".into(),
        "1".into(),
        "--color".into(),
        "never".into(),
        target.into(),
    ];
    let out = capture(rclone, &args).await?;
    match ExitKind::from_code(out.status.code()) {
        ExitKind::Success => {}
        ExitKind::DirectoryNotFound => return Ok(Vec::new()),
        _ => {
            return Err(ListError::Failed {
                command: "lsjson",
                message: clean_message(&out.stderr),
            })
        }
    }
    parse_lsjson(&out.stdout)
        .map(|entries| entries.into_iter().map(|e| e.path).collect())
        .map_err(|detail| ListError::Unreadable {
            command: "lsjson",
            detail,
        })
}

/// The top level of the folder `target`, unfiltered, for the folder
/// browser and the OneDrive folder completion: `rclone lsjson
/// --no-mimetype --no-modtime --color never [--dirs-only] <target>`
/// (lsjson lists one level unless asked for more). With `dirs_only`,
/// folders only; otherwise folders and files, so the browser can say how
/// many files a folder holds from the same call (OneDrive lists both in
/// one request either way).
pub async fn top_level(
    rclone: &Rclone,
    target: &str,
    dirs_only: bool,
) -> Result<Vec<Entry>, ListError> {
    let mut args: Vec<String> = vec![
        "lsjson".into(),
        "--no-mimetype".into(),
        "--no-modtime".into(),
        "--color".into(),
        "never".into(),
    ];
    if dirs_only {
        args.push("--dirs-only".into());
    }
    args.push(target.into());
    let out = capture(rclone, &args).await?;
    match ExitKind::from_code(out.status.code()) {
        ExitKind::Success => {}
        ExitKind::DirectoryNotFound => {
            return Err(ListError::NotFound {
                target: target.to_string(),
            })
        }
        _ => {
            return Err(ListError::Failed {
                command: "lsjson",
                message: clean_message(&out.stderr),
            })
        }
    }
    parse_lsjson(&out.stdout).map_err(|detail| ListError::Unreadable {
        command: "lsjson",
        detail,
    })
}

/// True when the folder `target` exists: `rclone lsjson --max-depth 1
/// --dirs-only`, which lists only its top level (exit 3 when it is missing).
pub async fn exists(rclone: &Rclone, target: &str) -> Result<bool, ListError> {
    let args: Vec<String> = vec![
        "lsjson".into(),
        "--max-depth".into(),
        "1".into(),
        "--dirs-only".into(),
        "--color".into(),
        "never".into(),
        target.into(),
    ];
    let out = capture(rclone, &args).await?;
    match ExitKind::from_code(out.status.code()) {
        ExitKind::Success => Ok(true),
        ExitKind::DirectoryNotFound => Ok(false),
        _ => Err(ListError::Failed {
            command: "lsjson",
            message: clean_message(&out.stderr),
        }),
    }
}

/// Creates the folder `target` (and its parents): `rclone mkdir`.
pub async fn mkdir(rclone: &Rclone, target: &str) -> Result<(), ListError> {
    let args: Vec<String> = vec![
        "mkdir".into(),
        "--color".into(),
        "never".into(),
        target.into(),
    ];
    let out = capture(rclone, &args).await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ListError::Failed {
            command: "mkdir",
            message: clean_message(&out.stderr),
        })
    }
}

/// A path under a destination: `remote:` + `a/b`, or `remote:x` + `/a/b`,
/// or a local folder and a relative path.
pub fn join(target: &str, rel: &str) -> String {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return target.to_string();
    }
    if target.ends_with(':') || target.ends_with('/') {
        format!("{target}{rel}")
    } else {
        format!("{target}/{rel}")
    }
}

async fn capture(rclone: &Rclone, args: &[String]) -> Result<exec::Captured, ExecError> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    exec::run_capture(Path::new(&rclone.path), &args).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rclone")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn lsjson_fixture() {
        let mut entries = parse_lsjson(&fixture("lsjson.json")).unwrap();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Projects/2026/budget.xlsx",
                "only-on-destination.txt",
                "same.txt",
                "touched.txt",
            ]
        );
        assert!(entries.iter().all(|e| !e.is_dir));
        assert_eq!(entries[0].size, 18);
        assert!(entries[3].mod_time.starts_with("2020-01-01T00:00:00"));
        let alias = parse_lsjson(&fixture("lsjson-alias.json")).unwrap();
        assert_eq!(alias.len(), 5);
    }

    #[test]
    fn lsjson_is_lenient_and_strict_where_it_matters() {
        assert_eq!(parse_lsjson("[]").unwrap(), vec![]);
        // Unknown fields are ignored; hashes are read, leniently.
        let e =
            parse_lsjson(r#"[{"Path":"a","Size":1,"Hashes":{"md5":"x"},"Tier":"hot"}]"#).unwrap();
        assert_eq!(e[0].path, "a");
        assert_eq!(e[0].hashes.get("md5").map(String::as_str), Some("x"));
        assert_eq!(e[0].quickxor(), None);
        let e = parse_lsjson(
            r#"[{"Path":"a","Hashes":{"QuickXorHash":1,"quickxor":"AB12"}},
                {"Path":"b","Hashes":null},{"Path":"c","Hashes":"odd"},{"Path":"d"},
                {"Path":"e","Hashes":{"quickxor":""}}]"#,
        )
        .unwrap();
        assert_eq!(e[0].quickxor(), Some("ab12"));
        assert_eq!(e[0].hashes.len(), 1, "a number is not a hash");
        assert!(e[1..].iter().all(|x| x.quickxor().is_none()));
        assert!(parse_lsjson("not json").is_err());
        assert!(
            parse_lsjson(r#"[{"Size":1}]"#).is_err(),
            "a path is required"
        );
    }

    /// Runs a shell script as if it were rclone's `lsjson`.
    async fn fake_listing(script: &str, listed: &AtomicU64) -> Result<Vec<Entry>, ListError> {
        let args = vec!["-c".to_string(), script.to_string()];
        list_streaming(Path::new("/bin/sh"), &args, "/fixture/dst", Some(listed)).await
    }

    #[tokio::test]
    async fn streaming_listing_counts_entries_as_they_come() {
        // The real shape, one entry per line, and a last line with no line
        // break: every entry is counted and the result parses.
        let listed = AtomicU64::new(0);
        let entries = fake_listing(
            r#"printf '[\n{"Path":"a.txt","Size":1},\n{"Path":"b/c.txt","Size":2}\n]'"#,
            &listed,
        )
        .await
        .unwrap();
        assert_eq!(listed.load(Ordering::Relaxed), 2);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b/c.txt"]);

        // A partial last line (rclone stopped mid-entry): what arrived is
        // counted, and the result is reported unreadable, not half-used.
        let listed = AtomicU64::new(0);
        let result = fake_listing(
            r#"printf '[\n{"Path":"a.txt","Size":1},\n{"Path":"b.t'"#,
            &listed,
        )
        .await;
        assert_eq!(listed.load(Ordering::Relaxed), 2);
        assert!(
            matches!(result, Err(ListError::Unreadable { .. })),
            "{result:?}"
        );

        // An empty destination.
        let listed = AtomicU64::new(0);
        let entries = fake_listing(r#"printf '[\n]\n'"#, &listed).await.unwrap();
        assert!(entries.is_empty());
        assert_eq!(listed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn streaming_listing_reports_rclone_errors() {
        // Exit 3: the folder does not exist yet.
        let listed = AtomicU64::new(0);
        let result = fake_listing(
            r#"printf '[\n'; echo 'ERROR : error listing: directory not found' >&2; exit 3"#,
            &listed,
        )
        .await;
        assert!(
            matches!(result, Err(ListError::NotFound { .. })),
            "{result:?}"
        );
        // Any other failure, after some entries: rclone's message, and the
        // entries seen so far were still counted.
        let listed = AtomicU64::new(0);
        let result = fake_listing(
            r#"printf '[\n{"Path":"a","Size":1},\n'; echo 'Failed to lsjson: token expired' >&2; exit 1"#,
            &listed,
        )
        .await;
        match result {
            Err(ListError::Failed { message, .. }) => {
                assert!(message.contains("token expired"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(listed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn hashsum_output() {
        let text = "6828031bd8f00600000000000500000000000000  a/x.txt\n\
                    7100000000000000000000000000000001000000    lead.txt\n\
                    UNSUPPORTED  odd.bin\n\
                    \n\
                    garbage\n\
                    ABCDEF  win.txt\r\n";
        let h = parse_hashsum(text);
        assert_eq!(h.len(), 3, "{h:?}");
        assert_eq!(h["a/x.txt"], "6828031bd8f00600000000000500000000000000");
        assert!(h.contains_key("  lead.txt"), "leading spaces are kept");
        assert_eq!(h["win.txt"], "abcdef");
    }

    #[test]
    fn entry_lines() {
        assert!(is_entry_line(r#"{"Path":"a"},"#));
        assert!(is_entry_line(r#"  {"Path":"a"}"#));
        assert!(!is_entry_line("["));
        assert!(!is_entry_line("]"));
        assert!(!is_entry_line(""));
    }

    #[test]
    fn joins() {
        assert_eq!(join("onefolder-work:", "a/b"), "onefolder-work:a/b");
        assert_eq!(
            join("onefolder-work:Docs", "a b"),
            "onefolder-work:Docs/a b"
        );
        assert_eq!(join("/Volumes/B/Docs/", "/x"), "/Volumes/B/Docs/x");
        assert_eq!(join("/d", ""), "/d");
    }

    #[test]
    fn size_fixture() {
        let s = parse_size(&fixture("size.json")).unwrap();
        assert_eq!(
            s,
            Size {
                count: 6,
                bytes: 45
            }
        );
        assert!(parse_size("{").is_err());
    }
}
