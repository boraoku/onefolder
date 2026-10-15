//! Deleted-folder detection from a bisync plan and the prior `.path1.lst`
//! listing (DESIGN.md, Folder-deletion prompt):
//!
//! 1. take the set D of paths the plan says were deleted locally (Path1
//!    `File was deleted`; folders appear too, as bisync runs with
//!    `--create-empty-src-dirs`);
//! 2. for each, walk up its parent folders: the highest one that no longer
//!    exists on local disk is that path's deleted folder; a path whose
//!    parent still exists is a plain deletion;
//! 3. a folder qualifies when everything the previous listing had under it
//!    is in D (otherwise some of it survived, and it is file deletions);
//!    nested deleted folders prompt once, for the highest;
//! 4. the rename hint: the plan also adds as many new local files as the
//!    folder had.
//!
//! Paths the plan says are gone from both sides are left out first, from
//! D and from the previous listing alike: they are entries that are now
//! excluded (hidden files, system files an older OneFolder synced) or were
//! deleted on both sides, and the sync can do nothing with them. A folder
//! whose content is all of that kind is never asked about.

use std::collections::{BTreeMap, HashSet};

use crate::rclone::bisync::{ListingEntry, PlanLine, PlanVerb, Side};

use super::plan::under;

/// More deleted folders than this and the prompt becomes one list with a
/// single answer for all, plus per-folder overrides.
pub const LIST_PROMPT_OVER: usize = 20;

/// A folder that qualifies for the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Relative to the source.
    pub path: String,
    /// Files and bytes under it in the previous listing.
    pub listed_files: u64,
    pub listed_bytes: u64,
    pub maybe_rename: bool,
}

/// Finds the deleted folders. `exists` says whether a relative path is on
/// local disk now; `new_local_files` is how many files the plan adds
/// locally (for the rename hint).
pub fn detect(
    lines: &[PlanLine],
    prior: &[ListingEntry],
    new_local_files: u64,
    exists: impl Fn(&str) -> bool,
) -> Vec<Found> {
    let deleted_on = |side: Side| -> HashSet<&str> {
        lines
            .iter()
            .filter(|l| l.side == side && l.verb == PlanVerb::Deleted)
            .map(|l| l.path.as_str())
            .collect()
    };
    let (local, remote) = (deleted_on(Side::Path1), deleted_on(Side::Path2));
    let gone_both: HashSet<&str> = local.intersection(&remote).copied().collect();
    let deleted: HashSet<&str> = local.difference(&gone_both).copied().collect();
    let prior: Vec<&ListingEntry> = prior
        .iter()
        .filter(|e| !gone_both.contains(e.path.as_str()))
        .collect();
    let prior_dirs: HashSet<&str> = prior
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| e.path.as_str())
        .collect();

    // 2: the highest missing folder of each deleted path.
    let mut candidates: BTreeMap<String, ()> = BTreeMap::new();
    for path in &deleted {
        let mut chain: Vec<&str> = path
            .match_indices('/')
            .map(|(i, _)| &path[..i])
            .filter(|p| !p.is_empty())
            .collect();
        if prior_dirs.contains(path) {
            chain.push(path);
        }
        if let Some(top) = chain.into_iter().find(|p| !exists(p)) {
            candidates.insert(top.to_string(), ());
        }
    }

    // 3: whole folders only, the highest of nested ones.
    let mut found: Vec<Found> = Vec::new();
    for folder in candidates.keys() {
        if found.iter().any(|f| under(folder, &f.path)) {
            continue;
        }
        let inside: Vec<&ListingEntry> = prior
            .iter()
            .copied()
            .filter(|e| e.path != *folder && under(&e.path, folder))
            .collect();
        let files: Vec<&&ListingEntry> = inside.iter().filter(|e| !e.is_dir).collect();
        if files.is_empty() || !inside.iter().all(|e| deleted.contains(e.path.as_str())) {
            continue;
        }
        let listed_files = files.len() as u64;
        found.push(Found {
            path: folder.clone(),
            listed_files,
            listed_bytes: files
                .iter()
                .map(|e| u64::try_from(e.size).unwrap_or(0))
                .sum(),
            maybe_rename: new_local_files > 0 && new_local_files == listed_files,
        });
    }
    // The hint only makes sense when one folder alone matches the count.
    if found.iter().filter(|f| f.maybe_rename).count() > 1 {
        for f in &mut found {
            f.maybe_rename = false;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rclone::bisync;
    use crate::rclone::events::{self, Event};

    fn line(side: Side, verb: PlanVerb, path: &str) -> PlanLine {
        PlanLine {
            side,
            verb,
            path: path.into(),
            detail: None,
        }
    }

    fn del(path: &str) -> PlanLine {
        line(Side::Path1, PlanVerb::Deleted, path)
    }

    fn listing(paths: &[&str]) -> Vec<ListingEntry> {
        paths
            .iter()
            .map(|p| {
                let dir = p.ends_with('/');
                ListingEntry {
                    path: p.trim_end_matches('/').to_string(),
                    size: if dir { -1 } else { 10 },
                    is_dir: dir,
                    modtime: String::new(),
                }
            })
            .collect()
    }

    /// Local disk after the deletions: these paths are gone.
    fn gone(missing: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |p: &str| !missing.iter().any(|m| under(p, m))
    }

    #[test]
    fn whole_folder() {
        let prior = listing(&["Old/", "Old/a", "Old/b", "keep.txt"]);
        let plan = [del("Old"), del("Old/a"), del("Old/b")];
        let found = detect(&plan, &prior, 0, gone(&["Old"]));
        assert_eq!(
            found,
            [Found {
                path: "Old".into(),
                listed_files: 2,
                listed_bytes: 20,
                maybe_rename: false
            }]
        );
    }

    #[test]
    fn partial_folder_is_file_deletions() {
        let prior = listing(&["Photos/", "Photos/1", "Photos/2", "Photos/3"]);
        let plan = [del("Photos/1"), del("Photos/2")];
        assert!(detect(&plan, &prior, 0, gone(&["Photos/1", "Photos/2"])).is_empty());
        // A folder that is gone locally but whose listing had more than
        // the plan deletes (an excluded file, say) is not whole either.
        let prior = listing(&["A/", "A/1", "A/2"]);
        assert!(detect(&[del("A"), del("A/1")], &prior, 0, gone(&["A"])).is_empty());
    }

    #[test]
    fn nested_folders_prompt_once_for_the_highest() {
        let prior = listing(&[
            "Old projects/",
            "Old projects/2019/",
            "Old projects/2019/a",
            "Old projects/2019/drafts/",
            "Old projects/2019/drafts/d",
            "Old projects/2018/",
            "Old projects/2018/e",
        ]);
        let plan = [
            del("Old projects/2019"),
            del("Old projects/2019/a"),
            del("Old projects/2019/drafts"),
            del("Old projects/2019/drafts/d"),
        ];
        let found = detect(&plan, &prior, 0, gone(&["Old projects/2019"]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "Old projects/2019");
        assert_eq!(found[0].listed_files, 2);
        // The whole tree gone: one prompt, for the top.
        let mut plan = plan.to_vec();
        plan.extend([
            del("Old projects"),
            del("Old projects/2018"),
            del("Old projects/2018/e"),
        ]);
        let found = detect(&plan, &prior, 0, gone(&["Old projects"]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "Old projects");
        assert_eq!(found[0].listed_files, 3);
    }

    #[test]
    fn rename_shaped_plan() {
        let prior = listing(&["Draft/", "Draft/a", "Draft/b"]);
        let plan = [
            del("Draft"),
            del("Draft/a"),
            del("Draft/b"),
            line(Side::Path1, PlanVerb::New, "Final/a"),
            line(Side::Path1, PlanVerb::New, "Final/b"),
        ];
        let found = detect(&plan, &prior, 2, gone(&["Draft"]));
        assert!(found[0].maybe_rename);
        assert!(!detect(&plan, &prior, 3, gone(&["Draft"]))[0].maybe_rename);
        // Two folders of two files each: no hint for either.
        let prior = listing(&["Draft/", "Draft/a", "Draft/b", "Old/", "Old/x", "Old/y"]);
        let mut plan = plan.to_vec();
        plan.extend([del("Old"), del("Old/x"), del("Old/y")]);
        let found = detect(&plan, &prior, 2, gone(&["Draft", "Old"]));
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| !f.maybe_rename));
    }

    /// Names with spaces at either end, and quotes: the plan's paths and
    /// the listing's must match exactly, or a whole folder would look
    /// partial and be deleted without a prompt.
    #[test]
    fn names_with_spaces_and_quotes() {
        let text = "# bisync listing v1\n\
            d       -1 - - 2026-10-13T00:00:00.000000000+0000 \"Old \"\n\
            -        2 - - 2026-10-13T00:00:00.000000000+0000 \"Old /a.txt\"\n\
            -        2 - - 2026-10-13T00:00:00.000000000+0000 \"Old / c \"\n\
            d       -1 - - 2026-10-13T00:00:00.000000000+0000 \" Lead\"\n\
            -        2 - - 2026-10-13T00:00:00.000000000+0000 \" Lead/say \\\"hi\\\".txt\"\n\
            -        2 - - 2026-10-13T00:00:00.000000000+0000 \"Old2/x\"\n";
        let prior = crate::rclone::bisync::parse_listing(text).unwrap();
        assert!(prior.iter().any(|e| e.path == " Lead/say \"hi\".txt"));
        let lines: Vec<PlanLine> = [
            "- Path1             File was deleted                            - Old ",
            "- Path1             File was deleted                            - Old /a.txt",
            "- Path1             File was deleted                            - Old / c ",
            "- Path1             File was deleted                            -  Lead",
            "- Path1             File was deleted                            - \" Lead/say \\\"hi\\\".txt\"",
        ]
        .iter()
        .filter_map(|m| match crate::rclone::bisync::parse_line(m) {
            Some(Event::Plan(l)) => Some(l),
            _ => None,
        })
        .collect();
        assert_eq!(lines.len(), 5);
        let found = detect(&lines, &prior, 0, gone(&["Old ", " Lead"]));
        let paths: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, [" Lead", "Old "]);
    }

    #[test]
    fn more_than_twenty_folders() {
        let mut paths = Vec::new();
        let mut plan = Vec::new();
        for i in 0..25 {
            paths.push(format!("F{i:02}/"));
            paths.push(format!("F{i:02}/x"));
            plan.push(del(&format!("F{i:02}")));
            plan.push(del(&format!("F{i:02}/x")));
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let prior = listing(&refs);
        let found = detect(&plan, &prior, 0, |p: &str| !p.starts_with('F'));
        assert_eq!(found.len(), 25);
        assert!(found.len() > LIST_PROMPT_OVER);
        assert_eq!(found[0].path, "F00");
    }

    /// After the built-in excludes arrive, a hidden folder or a folder of
    /// system files only, deleted locally, is gone from both listings: no
    /// prompt. A folder with real files and some junk still is asked
    /// about, sized by its real files.
    #[test]
    fn folders_the_sync_cannot_touch_are_not_asked_about() {
        let gone_there = |p: &str| line(Side::Path2, PlanVerb::Deleted, p);
        let prior = listing(&[
            ".cache/",
            ".cache/a",
            ".cache/b",
            "Photos/",
            "Photos/.DS_Store",
            "Photos/Thumbs.db",
            "Old/",
            "Old/a.txt",
            "Old/.DS_Store",
            "keep.txt",
        ]);
        let mut plan = Vec::new();
        for p in [
            ".cache",
            ".cache/a",
            ".cache/b",
            "Photos/.DS_Store",
            "Photos/Thumbs.db",
            "Old/.DS_Store",
        ] {
            plan.push(del(p));
            plan.push(gone_there(p));
        }
        plan.push(del("Photos"));
        plan.push(del("Old"));
        plan.push(del("Old/a.txt"));
        let found = detect(&plan, &prior, 0, gone(&[".cache", "Photos", "Old"]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "Old");
        assert_eq!(found[0].listed_files, 1);
        // A folder deleted here whose files are partly gone there too: the
        // rest is still on the destination, so it is asked about.
        let prior = listing(&["Docs/", "Docs/a", "Docs/b"]);
        let plan = vec![
            del("Docs"),
            del("Docs/a"),
            del("Docs/b"),
            gone_there("Docs/b"),
        ];
        let found = detect(&plan, &prior, 0, gone(&["Docs"]));
        assert_eq!((found[0].path.as_str(), found[0].listed_files), ("Docs", 1));
    }

    #[test]
    fn plain_file_deletions_and_empty_folders_need_no_prompt() {
        let prior = listing(&["a.txt", "Empty/"]);
        assert!(detect(&[del("a.txt")], &prior, 0, gone(&["a.txt"])).is_empty());
        assert!(detect(&[del("Empty")], &prior, 0, gone(&["Empty"])).is_empty());
        // Deletions on the destination never prompt.
        let prior = listing(&["Old/", "Old/a"]);
        let plan = [
            line(Side::Path2, PlanVerb::Deleted, "Old"),
            line(Side::Path2, PlanVerb::Deleted, "Old/a"),
        ];
        assert!(detect(&plan, &prior, 0, gone(&[])).is_empty());
    }

    /// The recorded plan and listing: Old projects/2019 is a whole folder
    /// (with a nested one), Photos lost two of three files.
    #[test]
    fn recorded_folder_plan() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rclone");
        let prior =
            bisync::parse_listing(&std::fs::read_to_string(dir.join("bisync.path1.lst")).unwrap())
                .unwrap();
        let events = events::decode_all(
            &std::fs::read_to_string(dir.join("bisync-folder-plan.jsonl")).unwrap(),
        );
        let lines: Vec<PlanLine> = events
            .into_iter()
            .filter_map(|e| match e {
                Event::Plan(l) => Some(l),
                _ => None,
            })
            .collect();
        let found = detect(
            &lines,
            &prior,
            1,
            gone(&["Old projects/2019", "Photos/p1.jpg", "Photos/p2.jpg"]),
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "Old projects/2019");
        assert_eq!(found[0].listed_files, 3);
        assert!(!found[0].maybe_rename);
    }
}
