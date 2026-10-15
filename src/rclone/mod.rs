//! Everything that touches the rclone binary. rclone is always a
//! subprocess, never a library, and only [`exec`] starts it.

pub mod bisync;
pub mod detect;
pub mod events;
pub mod exec;
pub mod listing;
pub mod remotes;

use std::path::Path;

/// The flags on every transfer command (DESIGN.md, Driving rclone): JSON
/// log lines, a stats line every second at NOTICE whatever the verbosity,
/// per-file lines, no ANSI codes in messages, OneDrive's 1 s timestamps,
/// and rclone's default retries stated so they are explicit.
pub const GLOBAL_FLAGS: [&str; 14] = [
    "--use-json-log",
    "--stats",
    "1s",
    "--stats-log-level",
    "NOTICE",
    "-v",
    "--color",
    "never",
    "--modify-window",
    "2s",
    "--retries",
    "3",
    "--low-level-retries",
    "10",
];

/// Patterns left out of every sync, always: every job, every destination,
/// one-way and two-way, in the preview, the destination listing, the copy
/// and bisync, whatever the job's own excludes and `copy_hidden` say.
/// They are files and folders that macOS and Windows make for themselves,
/// which are no use on another machine. rclone filter syntax: a pattern
/// that does not start with `/` matches a whole name at any depth, `*`
/// never matches a `/`, and `name/**` is everything inside a folder of
/// that name, and `[Tt]` is either letter (all confirmed on rclone 1.75.2
/// with a real copy).
pub const BUILTIN_EXCLUDES: [&str; 14] = [
    // Finder's folder view settings, in every folder it opens.
    ".DS_Store",
    // AppleDouble resource forks, written beside files on non-Mac disks.
    "._*",
    // Marks a folder whose name Finder translates.
    ".localized",
    // Spotlight's index at the root of a volume.
    ".Spotlight-V100/**",
    // The Trash of a removable volume.
    ".Trashes/**",
    // The file system event log of a volume.
    ".fseventsd/**",
    // Scratch files apps keep at the root of a volume.
    ".TemporaryItems/**",
    // Saved versions of documents (Versions), at the root of a volume.
    ".DocumentRevisions-V100/**",
    // Windows Explorer's thumbnail cache. The first letter either way:
    // patterns are case-sensitive, while macOS and OneDrive are not, and
    // both spellings turn up.
    "[Tt]humbs.db",
    // Older Windows thumbnail caches (Media Center, Vista).
    "[Ee]hthumbs.db",
    "[Ee]hthumbs_vista.db",
    // Windows Explorer's folder view settings.
    "[Dd]esktop.ini",
    // The Windows Recycle Bin of a drive.
    "$RECYCLE.BIN/**",
    // Windows system data at the root of a drive.
    "System Volume Information/**",
];

/// What leaves out hidden files when a job does not copy them: every name
/// that starts with a dot, at any depth (`.*`), and everything inside a
/// folder whose name starts with a dot (`.*/**`).
pub const HIDDEN_EXCLUDES: [&str; 2] = [".*", ".*/**"];

/// The patterns rclone gets for one of the job's own patterns, so that a
/// folder named in it is left out whole. rclone's `*` never matches a
/// `/`, so `folder/*` leaves out only the files directly in the folder,
/// and a name or path with no wildcard matches only a file. So, after
/// trimming the spaces around the pattern:
///
/// - a pattern ending in `/*` ends in `/**` instead (`_0RAG/*` becomes
///   `_0RAG/**`, which also matches the direct children), except `/*`
///   itself (the files at the root, which as `/**` would be everything);
/// - a name or path with none of `*`, `?`, `[`, `{`, `\` and no
///   trailing `/` gives itself, so a file of that name is still left
///   out, and itself with `/**` (`_0RAG` gives `_0RAG` and `_0RAG/**`,
///   `/Archive/2019` gives `/Archive/2019` and `/Archive/2019/**`);
/// - anything else (wildcards, a trailing `/` or `/**`) is passed on as
///   it is.
///
/// The trim makes a pattern mean one thing: rclone 1.75.2 trims the
/// trailing spaces of the last `--exclude` flag only (and of every line
/// of a file), so `trail ` would otherwise name different files by its
/// place in the list.
pub fn widen_exclude(pattern: &str) -> Vec<String> {
    let pattern = pattern.trim();
    if let Some(stem) = pattern.strip_suffix("/*") {
        // `/*` (or `//*`) means the files at the root; `/**` is all.
        if stem.trim_matches('/').is_empty() {
            return vec![pattern.to_string()];
        }
        return vec![format!("{stem}/**")];
    }
    let path = pattern.strip_prefix('/').unwrap_or(pattern);
    let plain = !path.is_empty()
        && !matches!(path, "." | "..")
        && !path.ends_with('/')
        && !path.contains(['*', '?', '[', '{', '\\']);
    if plain {
        vec![pattern.to_string(), format!("{pattern}/**")]
    } else {
        vec![pattern.to_string()]
    }
}

/// The excludes in force for a job: the built-in list, then the hidden
/// patterns unless the job copies hidden files, then the job's own, each
/// widened by [`widen_exclude`] so a folder it names is left out whole.
/// Every rclone call that filters (dry run, listing, copy, bisync) gets
/// this. A pattern already in the list (such as `.DS_Store` in an older
/// job) is not repeated.
pub fn effective_excludes(job_excludes: &[String], copy_hidden: bool) -> Vec<String> {
    let mut out: Vec<String> = BUILTIN_EXCLUDES.iter().map(|p| p.to_string()).collect();
    if !copy_hidden {
        out.extend(HIDDEN_EXCLUDES.iter().map(|p| p.to_string()));
    }
    for p in job_excludes.iter().flat_map(|p| widen_exclude(p)) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// The job's own patterns that are not already built in (or hidden
/// patterns in force), as the preview lists them.
pub fn own_excludes(job_excludes: &[String], copy_hidden: bool) -> Vec<String> {
    job_excludes
        .iter()
        .filter(|p| {
            !BUILTIN_EXCLUDES.contains(&p.as_str())
                && (copy_hidden || !HIDDEN_EXCLUDES.contains(&p.as_str()))
        })
        .cloned()
        .collect()
}

/// Filter arguments that keep one folder of a destination, as the sync
/// sees it: `--filter "- <pattern>"` for each of `excludes`, then
/// `--filter "+ /<folder>/**"` and `--filter "- **"`, for a command run on
/// the destination's root. Run there, every pattern means what it means in
/// the sync (a folder used as the root itself would escape a pattern
/// such as `.*/**` that matches one of its parents, and `/`-anchored
/// patterns would move). Only `--filter` flags, so rclone keeps their order.
pub fn folder_filter_args(excludes: &[String], folder: &str) -> Vec<String> {
    let mut args = Vec::with_capacity(excludes.len() * 2 + 4);
    for p in excludes {
        args.push("--filter".to_string());
        args.push(format!("- {p}"));
    }
    args.push("--filter".to_string());
    args.push(format!("+ {}", bisync::hold_exclude(folder)));
    args.push("--filter".to_string());
    args.push("- **".to_string());
    args
}

/// Above this many patterns the excludes go in a file (`--exclude-from`).
pub const MAX_EXCLUDE_FLAGS: usize = 20;

/// True when the patterns can go in an `--exclude-from` file and mean the
/// same there: rclone reads a line starting with `#` or `;` as a comment,
/// and trims the spaces around each line (an `--exclude` flag keeps its
/// leading spaces, and its trailing ones unless it is the last), so a
/// pattern with either, or with a line break, stays a flag. The job's own
/// patterns arrive trimmed ([`widen_exclude`]).
pub fn excludes_fit_a_file(excludes: &[String]) -> bool {
    excludes.iter().all(|p| {
        !p.starts_with('#')
            && !p.starts_with(';')
            && !p.contains(['\n', '\r'])
            && p.as_str() == p.trim()
    })
}

/// The filter arguments for a job's excludes: repeated `--exclude`, or
/// `--exclude-from file` when given one. The copy and the destination
/// listing use the same, so they see the same files.
pub fn exclude_args(excludes: &[String], exclude_file: Option<&Path>) -> Vec<String> {
    match exclude_file {
        Some(file) => vec!["--exclude-from".into(), file.display().to_string()],
        None => excludes
            .iter()
            .flat_map(|p| ["--exclude".to_string(), p.clone()])
            .collect(),
    }
}

/// `rclone copy <source> <target>` with the global flags, the excludes
/// (repeated `--exclude`, or `--exclude-from file` when given one),
/// `--fast-list` for a OneDrive destination, and `--dry-run` for a preview.
/// Paths must be absolute (or `remote:path`).
///
/// A dry run gets `--retries 1`: retrying a plan changes nothing, and each
/// attempt would add its checks to the stats the preview counts from.
pub fn copy_args(
    source: &str,
    target: &str,
    excludes: &[String],
    exclude_file: Option<&Path>,
    fast_list: bool,
    dry_run: bool,
) -> Vec<String> {
    let mut args = vec!["copy".to_string(), source.to_string(), target.to_string()];
    let mut retries = false;
    for flag in GLOBAL_FLAGS {
        let value = if retries && dry_run { "1" } else { flag };
        retries = flag == "--retries";
        args.push(value.to_string());
    }
    args.extend(exclude_args(excludes, exclude_file));
    if fast_list {
        args.push("--fast-list".into());
    }
    if dry_run {
        args.push("--dry-run".into());
    }
    args
}

/// `rclone moveto <from> <to>` with the global flags: a rename within one
/// destination (a conflict copy set aside before a two-way run, or a file
/// or folder moved locally, moved on a one-way destination too).
pub fn moveto_args(from: &str, to: &str) -> Vec<String> {
    let mut args = vec!["moveto".to_string(), from.to_string(), to.to_string()];
    args.extend(GLOBAL_FLAGS.iter().map(|f| f.to_string()));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_arguments() {
        let args = copy_args(
            "/Users/bora/Documents",
            "onefolder-work:Backups/Documents",
            &[".DS_Store".into(), "node_modules/**".into()],
            None,
            true,
            true,
        );
        assert_eq!(
            args.join(" "),
            "copy /Users/bora/Documents onefolder-work:Backups/Documents \
             --use-json-log --stats 1s --stats-log-level NOTICE -v --color never \
             --modify-window 2s --retries 1 --low-level-retries 10 \
             --exclude .DS_Store --exclude node_modules/** --fast-list --dry-run"
        );
        let args = copy_args(
            "/a",
            "/b",
            &["x".into()],
            Some(Path::new("/l/x.excludes")),
            false,
            false,
        );
        assert!(args.ends_with(&["--exclude-from".to_string(), "/l/x.excludes".to_string()]));
        assert!(!args.contains(&"--exclude".to_string()));
        assert!(!args.contains(&"--dry-run".to_string()));
        assert!(args
            .join(" ")
            .contains("--retries 3 --low-level-retries 10"));
    }

    #[test]
    fn effective_excludes_put_built_ins_first_then_hidden_then_the_job() {
        let job = vec!["node_modules/**".to_string(), "*.tmp".to_string()];
        let all = effective_excludes(&job, false);
        assert_eq!(all.len(), BUILTIN_EXCLUDES.len() + 2 + 2);
        assert_eq!(&all[..BUILTIN_EXCLUDES.len()], BUILTIN_EXCLUDES.as_slice());
        assert_eq!(
            &all[BUILTIN_EXCLUDES.len()..],
            [".*", ".*/**", "node_modules/**", "*.tmp"]
        );
        // Copying hidden files drops only the hidden patterns.
        let all = effective_excludes(&job, true);
        assert_eq!(&all[..BUILTIN_EXCLUDES.len()], BUILTIN_EXCLUDES.as_slice());
        assert_eq!(&all[BUILTIN_EXCLUDES.len()..], ["node_modules/**", "*.tmp"]);
        assert!(!all.iter().any(|p| p == ".*"));
        // A job pattern that is built in already is not repeated.
        let older = vec![".DS_Store".to_string(), "*.tmp".to_string()];
        let all = effective_excludes(&older, false);
        assert_eq!(all.iter().filter(|p| *p == ".DS_Store").count(), 1);
        assert_eq!(all.last().unwrap(), "*.tmp");
        assert_eq!(own_excludes(&older, false), ["*.tmp"]);
        let dots = vec![".*".to_string()];
        assert!(own_excludes(&dots, false).is_empty());
        assert_eq!(own_excludes(&dots, true), [".*"]);
        // With no job excludes the built-ins are still there.
        assert_eq!(effective_excludes(&[], true).len(), BUILTIN_EXCLUDES.len());
        // The built-ins and the hidden patterns are 16: five patterns of
        // the job's own take the list past the flag limit, into a file,
        // and every built-in pattern means the same in a file.
        assert_eq!(effective_excludes(&[], false).len(), 16);
        let five: Vec<String> = (0..5).map(|i| format!("*.x{i}")).collect();
        assert!(effective_excludes(&five[..4], false).len() <= MAX_EXCLUDE_FLAGS);
        assert!(effective_excludes(&five, false).len() > MAX_EXCLUDE_FLAGS);
        assert!(excludes_fit_a_file(&effective_excludes(&five, false)));
    }

    fn widen(p: &str) -> Vec<String> {
        widen_exclude(p)
    }

    #[test]
    fn a_folder_star_pattern_becomes_double_star() {
        assert_eq!(widen("_0RAG/*"), ["_0RAG/**"]);
        assert_eq!(widen("/x/y/*"), ["/x/y/**"]);
        assert_eq!(widen("a b/*"), ["a b/**"]);
        // The files at the root stay just those, never everything.
        assert_eq!(widen("/*"), ["/*"]);
        assert_eq!(widen("//*"), ["//*"]);
    }

    #[test]
    fn a_plain_name_also_leaves_out_the_folder() {
        assert_eq!(widen("_0RAG"), ["_0RAG", "_0RAG/**"]);
        assert_eq!(
            widen("/node_modules"),
            ["/node_modules", "/node_modules/**"]
        );
        assert_eq!(widen("notes.txt"), ["notes.txt", "notes.txt/**"]);
        assert_eq!(widen(".git"), [".git", ".git/**"]);
        // `.` and `..` name no folder rclone would match.
        assert_eq!(widen("."), ["."]);
        assert_eq!(widen("/.."), ["/.."]);
    }

    #[test]
    fn a_path_without_wildcards_also_leaves_out_the_folder() {
        assert_eq!(widen("a/b"), ["a/b", "a/b/**"]);
        assert_eq!(widen("/a/b"), ["/a/b", "/a/b/**"]);
        assert_eq!(
            widen("/Archive/2019"),
            ["/Archive/2019", "/Archive/2019/**"]
        );
        assert_eq!(
            widen("Projects/old notes.txt"),
            ["Projects/old notes.txt", "Projects/old notes.txt/**"]
        );
    }

    #[test]
    fn spaces_around_a_pattern_are_trimmed_first() {
        assert_eq!(widen("trail "), ["trail", "trail/**"]);
        assert_eq!(widen("  lead"), ["lead", "lead/**"]);
        assert_eq!(widen(" _0RAG/* "), ["_0RAG/**"]);
        assert_eq!(widen("\t*.tmp\r\n"), ["*.tmp"]);
        assert_eq!(widen("in side"), ["in side", "in side/**"]);
        let all = effective_excludes(&["x ".to_string(), "x".to_string()], false);
        assert_eq!(&all[all.len() - 2..], ["x", "x/**"]);
        assert!(excludes_fit_a_file(&effective_excludes(
            &[" lead.txt".to_string(), "trail.txt ".to_string()],
            false
        )));
    }

    #[test]
    fn a_trailing_slash_is_left_alone_and_never_doubled() {
        assert_eq!(widen("_0RAG/"), ["_0RAG/"]);
        assert_eq!(widen("/_0RAG/"), ["/_0RAG/"]);
        assert_eq!(widen("a/b/"), ["a/b/"]);
        assert_eq!(widen("/"), ["/"]);
        assert_eq!(widen(""), [""]);
        for p in ["_0RAG/", "/", "a/", "/a/b/"] {
            assert!(widen(p).iter().all(|w| !w.ends_with("//**")), "{p}");
        }
    }

    #[test]
    fn wildcards_and_trailing_slashes_pass_through() {
        for p in [
            "*.tmp",
            "build*",
            "[Tt]humbs.db",
            "file?.txt",
            "{a,b}",
            "a\\*b",
            "a/*.tmp/x",
            "node_modules/**",
            "/_0RAG/**",
            "a/*.tmp",
            "**",
        ] {
            assert_eq!(widen(p), [p], "{p}");
        }
    }

    #[test]
    fn effective_excludes_widen_only_the_jobs_own_patterns() {
        let base = effective_excludes(&[], false).len();
        let job: Vec<String> = ["_0RAG/*", "*.tmp", "_0RAG/**", "cache", "/out"]
            .iter()
            .map(|p| p.to_string())
            .collect();
        let all = effective_excludes(&job, false);
        assert_eq!(
            &all[base..],
            ["_0RAG/**", "*.tmp", "cache", "cache/**", "/out", "/out/**"]
        );
        // The built-in and hidden patterns stay as they are.
        assert_eq!(&all[..BUILTIN_EXCLUDES.len()], BUILTIN_EXCLUDES.as_slice());
        assert_eq!(
            &all[BUILTIN_EXCLUDES.len()..base],
            HIDDEN_EXCLUDES.as_slice()
        );
        // A widened form already in force is not repeated.
        let all = effective_excludes(&[".*".to_string()], true);
        assert_eq!(&all[BUILTIN_EXCLUDES.len()..], [".*"]);
        let all = effective_excludes(&[".DS_Store".to_string()], false);
        assert_eq!(all.last().unwrap(), ".DS_Store/**");
        assert_eq!(all.iter().filter(|p| *p == ".DS_Store").count(), 1);
        // The preview's own list keeps the patterns as typed.
        assert_eq!(own_excludes(&job, false), job);
    }

    #[test]
    fn folder_filters_keep_one_folder_and_its_excludes() {
        let args = folder_filter_args(&[".DS_Store".into(), ".*/**".into()], "Old projects/[2019]");
        assert_eq!(
            args.join(" "),
            "--filter - .DS_Store --filter - .*/** --filter + /Old projects/\\[2019\\]/** \
             --filter - **"
        );
    }

    #[test]
    fn comment_like_patterns_stay_flags() {
        assert!(excludes_fit_a_file(&["*.tmp".into()]));
        assert!(!excludes_fit_a_file(&["#notes".into()]));
        assert!(!excludes_fit_a_file(&[";x".into()]));
        assert!(!excludes_fit_a_file(&[" lead.txt".into()]));
        assert!(!excludes_fit_a_file(&["trail.txt ".into()]));
        assert!(!excludes_fit_a_file(&["cr\r.txt".into()]));
        assert!(!excludes_fit_a_file(&["two\nlines".into()]));
    }
}
