//! What a two-way run will do, read from bisync's dry runs: the plan of a
//! normal run (step 3 of DESIGN.md's sequence) and the merge of a first run
//! or recovery (step 2).
//!
//! The dry run says two things: the plan lines (what changed on each side
//! since the last run) and, after `Do queued copies to - PathN`, the
//! operations it would carry out on that side (`Skipped copy`, `Skipped
//! delete` with sizes). The operations give the counts, bytes and
//! deletions; the plan lines tell new from changed files, and give the
//! numbers bisync's own safety checks use.

use std::collections::HashSet;

use serde::Serialize;

use crate::rclone::bisync::{
    self, exceeds_max_delete, ListingEntry, PlanLine, PlanVerb, Side, MAX_DELETE,
};
use crate::rclone::events::{Event, Skipped};

use super::oneway::PlannedFile;
use super::reporter::{DecidedBy, DeletedFolder, FolderAnswer, Where};

/// Files going one way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Direction {
    pub new: Vec<PlannedFile>,
    pub changed: Vec<PlannedFile>,
    pub bytes: u64,
}

impl Direction {
    pub fn files(&self) -> usize {
        self.new.len() + self.changed.len()
    }

    fn add(&mut self, file: PlannedFile, new: bool) {
        self.bytes += file.size;
        if new {
            self.new.push(file);
        } else {
            self.changed.push(file);
        }
    }
}

/// A file new or changed on both sides: the local copy wins, the
/// destination's is kept there under `kept_as`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedConflict {
    pub path: String,
    /// The destination copy's new name, when known.
    pub kept_as: Option<String>,
}

/// A deleted folder and what was decided for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FolderDecision {
    pub folder: DeletedFolder,
    pub answer: FolderAnswer,
    pub by: DecidedBy,
}

/// Why a plan needs an explicit confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BigKind {
    /// More than half of a side's files and folders would be deleted.
    Deletions,
    /// Every file on a side changed.
    AllChanged,
}

/// A change large enough that bisync's safety checks stop it, and that the
/// user must confirm with the exact counts in front of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BigChange {
    pub kind: BigKind,
    /// The side the deletions (or changes) happen on.
    pub side: Where,
    pub count: u64,
    /// Files and folders on the side the change came from, as last listed.
    pub total: u64,
}

impl BigChange {
    pub fn percent(&self) -> u64 {
        (self.count * 100).checked_div(self.total).unwrap_or(0)
    }

    /// The share for messages: see [`percent_text`].
    pub fn percent_text(&self) -> String {
        percent_text(self.count, self.total)
    }
}

/// `count` of `total` as a percentage with one decimal place, rounded
/// down, such as `62.5%`. Just over half never reads as a rounded `50%`
/// (or `50.0%`): it is `>50%`, since the limit is strictly more than half.
pub fn percent_text(count: u64, total: u64) -> String {
    if total == 0 {
        return "0%".to_string();
    }
    let tenths = u128::from(count) * 1000 / u128::from(total);
    if count.saturating_mul(2) > total && tenths <= 500 {
        return ">50%".to_string();
    }
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// A two-way preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub job: String,
    pub destination_id: String,
    pub destination: String,
    /// `onedrive` or `local`.
    pub destination_type: String,
    pub to_remote: Direction,
    pub to_local: Direction,
    /// Files bisync will delete on the destination (all listed).
    pub delete_remote: Vec<String>,
    /// Files bisync will delete on this Mac.
    pub delete_local: Vec<String>,
    pub conflicts: Vec<PlannedConflict>,
    pub folders: Vec<FolderDecision>,
    /// Changes that need confirming (and lift a safety limit once confirmed).
    pub big: Vec<BigChange>,
    /// bisync's too-many-deletes check would stop the real run (counting
    /// the entries a hold or a conflict copy takes out of its listings,
    /// which delete nothing), so it needs `--max-delete 100`.
    pub lift: bool,
    /// bisync's all-files-changed check would stop it: `--force`.
    pub force: bool,
    /// Plan lines that do something (deletions on both sides do nothing).
    pub changes: u64,
    /// The job's own exclude patterns and the conflict pattern.
    pub excludes: Vec<String>,
    /// The job copies hidden files (`copy_hidden`).
    pub copy_hidden: bool,
    /// Every pattern bisync gets: the built-in list, the hidden patterns
    /// unless `copy_hidden`, the job's own, the holds and the conflict
    /// pattern.
    pub effective_excludes: Vec<String>,
    pub holds: Vec<String>,
}

impl Plan {
    /// Nothing to do: no real run is needed.
    pub fn is_empty(&self) -> bool {
        self.changes == 0 && self.folders.is_empty()
    }

    pub fn needs_confirmation(&self) -> bool {
        !self.big.is_empty()
    }
}

/// What a bisync dry run printed, gathered.
#[derive(Debug, Clone, Default)]
pub struct DryRun {
    pub lines: Vec<PlanLine>,
    pub conflicts: Vec<PlannedConflict>,
    /// Each would-be operation with the side it happens on.
    pub ops: Vec<(Side, Skipped)>,
}

impl DryRun {
    pub fn collect(events: &[Event]) -> DryRun {
        let mut out = DryRun::default();
        let mut on: Option<Side> = None;
        for event in events {
            match event {
                Event::Plan(line) => out.lines.push(line.clone()),
                Event::Conflict { path } => out.conflicts.push(PlannedConflict {
                    path: path.clone(),
                    kept_as: None,
                }),
                // The name bisync would give the losing copy is not used:
                // OneFolder picks its own (see `conflict_name`).
                Event::NoConflict { path } => out.conflicts.retain(|c| &c.path != path),
                Event::Applying { to } => on = Some(*to),
                Event::Skipped(s) => {
                    if let Some(side) = on {
                        out.ops.push((side, s.clone()));
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn set(&self, side: Side, verb: PlanVerb) -> HashSet<&str> {
        self.lines
            .iter()
            .filter(|l| l.side == side && l.verb == verb)
            .map(|l| l.path.as_str())
            .collect()
    }

    /// Files new on this Mac since the last run (for the rename hint).
    pub fn new_local(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|l| l.side == Side::Path1 && l.verb == PlanVerb::New)
            .map(|l| l.path.as_str())
            .collect()
    }
}

/// The loser's name relative to the root, from the absolute path bisync
/// printed: the known root (the source, or the destination as rclone
/// spells it) is taken off; failing that, the conflict's folder and the
/// printed file name. A `:` in a file name is kept.
pub fn loser_name(conflict: &str, printed: &str, roots: &[&str]) -> String {
    for root in roots.iter().filter(|r| !r.is_empty()) {
        let root = root.trim_end_matches('/');
        if let Some(rest) = printed.strip_prefix(root) {
            if let Some(rel) = rest.strip_prefix('/') {
                return rel.to_string();
            }
        }
    }
    let name = printed.rsplit('/').next().unwrap_or(printed);
    match conflict.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_string(),
    }
}

/// True when `path` is `folder` or inside it.
pub fn under(path: &str, folder: &str) -> bool {
    path == folder
        || (path.len() > folder.len()
            && path.starts_with(folder)
            && path.as_bytes()[folder.len()] == b'/')
}

impl Plan {
    /// Builds the plan from a dry run, the prior listings and the folder
    /// decisions. A kept folder is excluded from the real run; a restored
    /// one is copied back first: neither is deleted on the destination.
    pub fn build(
        dry: &DryRun,
        prior1: &[ListingEntry],
        prior2: &[ListingEntry],
        folders: Vec<FolderDecision>,
    ) -> Plan {
        let spared: Vec<&str> = folders
            .iter()
            .filter(|d| matches!(d.answer, FolderAnswer::Keep | FolderAnswer::Restore))
            .map(|d| d.folder.path.as_str())
            .collect();
        let kept: Vec<&str> = folders
            .iter()
            .filter(|d| d.answer == FolderAnswer::Keep)
            .map(|d| d.folder.path.as_str())
            .collect();
        let restored: Vec<&str> = folders
            .iter()
            .filter(|d| d.answer == FolderAnswer::Restore)
            .map(|d| d.folder.path.as_str())
            .collect();
        let spared_path = |p: &str| spared.iter().any(|f| under(p, f));

        let new1 = dry.set(Side::Path1, PlanVerb::New);
        let new2 = dry.set(Side::Path2, PlanVerb::New);
        let del1 = dry.set(Side::Path1, PlanVerb::Deleted);
        let del2 = dry.set(Side::Path2, PlanVerb::Deleted);
        let conflicted: HashSet<&str> = dry.conflicts.iter().map(|c| c.path.as_str()).collect();

        let mut plan = Plan {
            conflicts: dry.conflicts.clone(),
            ..Plan::default()
        };
        // Only operations on paths the plan lines name: the rest is bisync's
        // own conflict handling (renaming the losing copy, deleting an
        // existing copy of that name), which OneFolder replaces by setting
        // the copy aside itself before the run.
        let planned: HashSet<&str> = dry.lines.iter().map(|l| l.path.as_str()).collect();
        let mut seen: HashSet<(Side, &str, &str)> = HashSet::new();
        for (side, op) in &dry.ops {
            if op.is_dir
                || !planned.contains(op.object.as_str())
                || !seen.insert((*side, op.action.as_str(), op.object.as_str()))
            {
                continue;
            }
            let size = op.size.unwrap_or(0);
            let file = |modtime_only| PlannedFile {
                path: op.object.clone(),
                size: if modtime_only { 0 } else { size },
                modtime_only,
            };
            let path = op.object.as_str();
            match (side, op.action.as_str()) {
                // A conflict's winning copy goes up as a change.
                (Side::Path2, "copy") => plan.to_remote.add(
                    file(false),
                    new1.contains(path) && !conflicted.contains(path),
                ),
                (Side::Path1, "copy") => {
                    if !bisync::is_conflict_copy(path) {
                        let new = new2.contains(path) || del1.contains(path);
                        plan.to_local.add(file(false), new);
                    }
                }
                (Side::Path2, "update modification time") => plan.to_remote.add(file(true), false),
                (Side::Path1, "update modification time") => plan.to_local.add(file(true), false),
                (Side::Path2, "delete") => {
                    if !spared_path(path) {
                        plan.delete_remote.push(op.object.clone());
                    }
                }
                (Side::Path1, "delete") => plan.delete_local.push(op.object.clone()),
                _ => {}
            }
        }
        plan.delete_remote.sort();
        plan.delete_local.sort();

        // The numbers bisync's checks use: plan lines (files and folders)
        // against the previous listing of the same side.
        let total1 = prior1.len() as u64;
        let total2 = prior2.len() as u64;
        let restored_path = |p: &str| restored.iter().any(|f| under(p, f));
        let rclone_del1 = del1.iter().filter(|p| !restored_path(p)).count() as u64;
        let held_entries = prior2
            .iter()
            .filter(|e| kept.iter().any(|f| under(&e.path, f)) && !del2.contains(e.path.as_str()))
            .count() as u64;
        // OneFolder sets each conflict's destination copy aside before the
        // run, which bisync then sees as one more deletion there.
        let rclone_del2 = del2.len() as u64 + held_entries + dry.conflicts.len() as u64;
        plan.lift = exceeds_max_delete(rclone_del1, total1, MAX_DELETE)
            || exceeds_max_delete(rclone_del2, total2, MAX_DELETE);

        // Real deletions: on the destination, local deletions that are not
        // also gone there and not spared; on this Mac, the converse.
        let real_on_remote = del1
            .iter()
            .filter(|p| !del2.contains(*p) && !spared_path(p))
            .count() as u64;
        let real_on_local = del2.iter().filter(|p| !del1.contains(*p)).count() as u64;
        // Measured against what is really there: entries gone from both
        // listings (newly excluded, such as hidden files or system files
        // an older OneFolder synced, or deleted on both sides) delete
        // nothing, and a folder kept on the destination this run is not
        // touched. Counting them would hide a real mass deletion behind
        // them, and the limit would be lifted without asking.
        let gone_both: HashSet<&str> = del1.intersection(&del2).copied().collect();
        let real_total = |prior: &[ListingEntry]| {
            prior
                .iter()
                .filter(|e| {
                    !gone_both.contains(e.path.as_str()) && !kept.iter().any(|f| under(&e.path, f))
                })
                .count() as u64
        };
        let (real_total1, real_total2) = (real_total(prior1), real_total(prior2));
        if exceeds_max_delete(real_on_remote, real_total1, MAX_DELETE) {
            plan.big.push(BigChange {
                kind: BigKind::Deletions,
                side: Where::Destination,
                count: real_on_remote,
                total: real_total1,
            });
        }
        if exceeds_max_delete(real_on_local, real_total2, MAX_DELETE) {
            plan.big.push(BigChange {
                kind: BigKind::Deletions,
                side: Where::Local,
                count: real_on_local,
                total: real_total2,
            });
        }
        // bisync stops when every entry of a side changed.
        for (side, total, to) in [
            (Side::Path1, total1, Where::Destination),
            (Side::Path2, total2, Where::Local),
        ] {
            let changed = dry.set(side, PlanVerb::Changed).len() as u64;
            if total > 0 && changed == total {
                plan.force = true;
                plan.big.push(BigChange {
                    kind: BigKind::AllChanged,
                    side: to,
                    count: changed,
                    total,
                });
            }
        }

        plan.changes = dry
            .lines
            .iter()
            .filter(|l| {
                !(l.verb == PlanVerb::Deleted
                    && del1.contains(l.path.as_str())
                    && del2.contains(l.path.as_str()))
            })
            .count() as u64;
        plan.folders = folders;
        plan
    }
}

// ------------------------------------------------------------- resync

/// Why a two-way run starts with a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResyncReason {
    /// No sync state yet.
    FirstRun,
    /// The job's source or this destination's folder changed since.
    PairChanged,
    /// The working directory lost its listings.
    Damaged,
    /// The last run stopped on a bisync critical error.
    Critical,
    /// `onefolder run --resync`.
    Requested,
}

/// The merge a first run (or a recovery) would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResyncOffer {
    pub job: String,
    pub destination_id: String,
    pub destination: String,
    pub destination_type: String,
    pub reason: Option<ResyncReason>,
    /// What happened, in words (a recovery), with the log to look at.
    pub detail: Option<String>,
    pub to_local: Direction,
    pub to_remote: Direction,
    /// Destination copies that differ from the local file: renamed there
    /// with the conflict suffix before the merge, since `--resync` would
    /// overwrite them (confirmed on 1.75.2).
    pub kept: Vec<PlannedConflict>,
    /// The destination folder does not exist yet and will be created.
    pub destination_missing: bool,
    pub dry_run: bool,
}

impl ResyncOffer {
    /// Fills the counts from a `--resync --dry-run`. `listed` is every
    /// file the destination holds now, unfiltered (conflict copies too), to
    /// tell overwrites from new files and to keep conflict names free;
    /// `local_taken` says whether a name is in use on this Mac; `date` is
    /// the day for conflict names (`2026-10-13`).
    pub fn fill(
        &mut self,
        events: &[Event],
        listed: &HashSet<String>,
        date: &str,
        local_taken: impl Fn(&str) -> bool,
    ) {
        let dry = DryRun::collect(events);
        let mut chosen: HashSet<String> = HashSet::new();
        let mut seen: HashSet<(Side, String)> = HashSet::new();
        for (side, op) in &dry.ops {
            if op.is_dir || op.action != "copy" || !seen.insert((*side, op.object.clone())) {
                continue;
            }
            let file = PlannedFile {
                path: op.object.clone(),
                size: op.size.unwrap_or(0),
                modtime_only: false,
            };
            match side {
                Side::Path1 => self.to_local.add(file, true),
                Side::Path2 => {
                    let overwrite = listed.contains(&op.object);
                    if overwrite {
                        let name = conflict_name(&op.object, date, |n| {
                            listed.contains(n) || chosen.contains(n) || local_taken(n)
                        });
                        chosen.insert(name.clone());
                        self.kept.push(PlannedConflict {
                            path: op.object.clone(),
                            kept_as: Some(name),
                        });
                    }
                    self.to_remote.add(file, !overwrite);
                }
            }
        }
    }
}

/// The name a conflict's destination copy is kept under:
/// `name.conflict-<YYYY-MM-DD>-<n>.ext`, `n` the first number `taken` says
/// is free. `Notes/plan.md` becomes `Notes/plan.conflict-2026-10-13-1.md`.
/// The extension is the part after the last dot (not a leading one), so
/// `arch.tar.gz` becomes `arch.tar.conflict-2026-10-13-1.gz`, and a name
/// without one gets no extension. The caller checks both sides with
/// unfiltered listings: conflict copies are excluded from the sync, so
/// bisync's own numbering cannot see them.
pub fn conflict_name(path: &str, date: &str, taken: impl Fn(&str) -> bool) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, n)) => (format!("{d}/"), n),
        None => (String::new(), path),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| format!("{dir}{stem}.conflict-{date}-{n}{ext}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_default()
}

/// The destination-side deletions per folder, for tests and the summary.
#[cfg(test)]
pub fn by_folder(paths: &[String]) -> std::collections::HashMap<String, usize> {
    let mut out = std::collections::HashMap::new();
    for p in paths {
        let folder = p.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        *out.entry(folder).or_insert(0) += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_never_round_to_half() {
        assert_eq!(percent_text(6, 10), "60.0%");
        assert_eq!(percent_text(5, 10), "50.0%");
        assert_eq!(percent_text(5001, 10_000), ">50%");
        assert_eq!(percent_text(501, 1000), "50.1%");
        assert_eq!(percent_text(2, 3), "66.6%");
        assert_eq!(percent_text(1, 3), "33.3%");
        assert_eq!(percent_text(10, 10), "100.0%");
        assert_eq!(percent_text(0, 0), "0%");
    }
    use crate::rclone::events;

    fn fixture(name: &str) -> String {
        let path = crate::rclone::fixture_path(name);
        std::fs::read_to_string(path).unwrap()
    }

    fn entries(paths: &[(&str, bool)]) -> Vec<ListingEntry> {
        paths
            .iter()
            .map(|(p, dir)| ListingEntry {
                path: p.to_string(),
                size: if *dir { -1 } else { 5 },
                is_dir: *dir,
                modtime: "2026-10-13T00:00:00+0000".into(),
            })
            .collect()
    }

    /// The listing the recorded plan was made against (scenario 13 of
    /// the recording script): 18 entries on each side.
    fn prior() -> Vec<ListingEntry> {
        let mut p: Vec<(&str, bool)> = vec![
            ("Notes", true),
            ("Notes/plan.md", false),
            ("del.txt", false),
            ("keep", true),
            ("mod.txt", false),
            ("only-on-destination.txt", false),
            ("rdel.txt", false),
            ("rmod.txt", false),
            ("sub", true),
            ("sub/deep", true),
            ("sub/deep/d.txt", false),
            ("sub/f1.txt", false),
            ("sub/f2.txt", false),
            ("sub/f3.txt", false),
        ];
        let keep: Vec<String> = (1..=6).map(|i| format!("keep/k{i}.txt")).collect();
        let mut v = entries(&std::mem::take(&mut p));
        v.extend(entries(
            &keep.iter().map(|k| (k.as_str(), false)).collect::<Vec<_>>(),
        ));
        v
    }

    #[test]
    fn plan_from_the_recorded_dry_run() {
        let dry = DryRun::collect(&events::decode_all(&fixture("bisync-plan.jsonl")));
        let plan = Plan::build(&dry, &prior(), &prior(), vec![]);
        let paths =
            |files: &[PlannedFile]| files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&plan.to_remote.new), ["newlocal.txt"]);
        let mut changed = paths(&plan.to_remote.changed);
        changed.sort();
        assert_eq!(changed, ["Notes/plan.md", "mod.txt"]);
        assert_eq!(paths(&plan.to_local.new), ["newremote.txt"]);
        assert_eq!(paths(&plan.to_local.changed), ["rmod.txt"]);
        assert_eq!(
            plan.delete_remote,
            [
                "del.txt",
                "sub/deep/d.txt",
                "sub/f1.txt",
                "sub/f2.txt",
                "sub/f3.txt"
            ]
        );
        assert_eq!(plan.delete_local, ["rdel.txt"]);
        assert_eq!(
            plan.conflicts,
            [PlannedConflict {
                path: "Notes/plan.md".into(),
                kept_as: None
            }]
        );
        // The winner's bytes go up with the rest: newlocal 9 + mod 12 + plan 15.
        assert_eq!(plan.to_remote.bytes, 36);
        assert!(!plan.lift && !plan.force && plan.big.is_empty());
        assert_eq!(plan.changes, 14);
        assert!(!plan.is_empty());
        assert_eq!(by_folder(&plan.delete_remote)["sub"], 3);
    }

    fn folder(path: &str, answer: FolderAnswer) -> FolderDecision {
        FolderDecision {
            folder: DeletedFolder {
                path: path.into(),
                files: 4,
                bytes: 20,
                measured: true,
                maybe_rename: false,
            },
            answer,
            by: DecidedBy::Asked,
        }
    }

    #[test]
    fn kept_and_restored_folders_are_not_deleted() {
        let dry = DryRun::collect(&events::decode_all(&fixture("bisync-plan.jsonl")));
        for answer in [FolderAnswer::Keep, FolderAnswer::Restore] {
            let plan = Plan::build(&dry, &prior(), &prior(), vec![folder("sub", answer)]);
            assert_eq!(plan.delete_remote, ["del.txt"], "{answer:?}");
            assert_eq!(plan.folders.len(), 1);
        }
        let plan = Plan::build(
            &dry,
            &prior(),
            &prior(),
            vec![folder("sub", FolderAnswer::Delete)],
        );
        assert_eq!(plan.delete_remote.len(), 5);
    }

    #[test]
    fn more_than_half_deleted_needs_confirmation() {
        // The plan lines of the recorded forced run; its deletions as a
        // dry run would list them.
        let mut dry = DryRun::collect(&events::decode_all(&fixture(
            "bisync-too-many-deletes-force.jsonl",
        )));
        dry.ops = dry
            .lines
            .iter()
            .map(|l| {
                (
                    Side::Path2,
                    Skipped {
                        action: "delete".into(),
                        object: l.path.clone(),
                        size: Some(3),
                        is_dir: false,
                    },
                )
            })
            .collect();
        let ten: Vec<String> = (1..=10).map(|i| format!("f{i}.txt")).collect();
        let prior = entries(&ten.iter().map(|p| (p.as_str(), false)).collect::<Vec<_>>());
        let plan = Plan::build(&dry, &prior, &prior, vec![]);
        assert_eq!(plan.delete_remote.len(), 6);
        assert!(plan.lift);
        assert_eq!(
            plan.big,
            [BigChange {
                kind: BigKind::Deletions,
                side: Where::Destination,
                count: 6,
                total: 10
            }]
        );
        assert_eq!(plan.big[0].percent(), 60);
        assert_eq!(plan.big[0].percent_text(), "60.0%");
        assert!(plan.needs_confirmation());
    }

    #[test]
    fn phantom_deletions_lift_without_confirmation() {
        // A held folder of six entries out of ten on the destination: the
        // real run excludes it, so bisync sees it gone from both listings.
        let lines = vec![];
        let dry = DryRun {
            lines,
            ..DryRun::default()
        };
        let prior1 = entries(&[("a", false), ("b", false), ("c", false), ("d", false)]);
        let prior2 = entries(&[
            ("Old", true),
            ("Old/1", false),
            ("Old/2", false),
            ("Old/3", false),
            ("Old/4", false),
            ("Old/5", false),
            ("a", false),
            ("b", false),
            ("c", false),
            ("d", false),
        ]);
        let plan = Plan::build(
            &dry,
            &prior1,
            &prior2,
            vec![folder("Old", FolderAnswer::Keep)],
        );
        assert!(plan.lift, "bisync counts the held entries as deleted");
        assert!(plan.big.is_empty(), "nothing is really deleted");
        assert!(!plan.is_empty(), "the folder decision still needs a run");
    }

    /// Entries gone from both listings (here 30 hidden files an older
    /// OneFolder synced, now excluded) must not hide a real mass deletion:
    /// 6 of the 10 real files deleted locally needs confirming, with the
    /// real counts, though bisync sees 36 of 42.
    #[test]
    fn entries_gone_from_both_sides_do_not_dilute_a_mass_deletion() {
        let line = |side, path: &str| PlanLine {
            side,
            verb: PlanVerb::Deleted,
            path: path.into(),
            detail: None,
        };
        let mut paths: Vec<(String, bool)> =
            vec![(".git".into(), true), (".git/objects".into(), true)];
        paths.extend((0..30).map(|i| (format!(".git/objects/o{i}"), false)));
        let hidden: Vec<String> = paths.iter().map(|(p, _)| p.clone()).collect();
        paths.extend((0..10).map(|i| (format!("f{i}.txt"), false)));
        let prior = entries(
            &paths
                .iter()
                .map(|(p, d)| (p.as_str(), *d))
                .collect::<Vec<_>>(),
        );
        let mut lines: Vec<PlanLine> = Vec::new();
        for h in &hidden {
            lines.push(line(Side::Path1, h));
            lines.push(line(Side::Path2, h));
        }
        let mut dry = DryRun {
            lines,
            ..DryRun::default()
        };
        // Only the hidden entries: nothing real is deleted, the limit is
        // lifted without asking.
        let plan = Plan::build(&dry, &prior, &prior, vec![]);
        assert!(plan.lift, "bisync counts 32 of 42 as deleted");
        assert!(plan.big.is_empty(), "{:?}", plan.big);
        // Six real deletions on top: 60% of what is really there.
        for i in 0..6 {
            let path = format!("f{i}.txt");
            dry.lines.push(line(Side::Path1, &path));
            dry.ops.push((
                Side::Path2,
                Skipped {
                    action: "delete".into(),
                    object: path,
                    size: Some(5),
                    is_dir: false,
                },
            ));
        }
        let plan = Plan::build(&dry, &prior, &prior, vec![]);
        assert_eq!(
            plan.big,
            [BigChange {
                kind: BigKind::Deletions,
                side: Where::Destination,
                count: 6,
                total: 10
            }]
        );
        assert!(plan.needs_confirmation());
        // Four real deletions: 40%, lifted without asking.
        dry.lines.truncate(dry.lines.len() - 2);
        let plan = Plan::build(&dry, &prior, &prior, vec![]);
        assert!(plan.lift && plan.big.is_empty(), "{:?}", plan.big);
    }

    #[test]
    fn every_file_changed_needs_force() {
        let dry = DryRun::collect(&events::decode_all(&fixture("bisync-all-changed.jsonl")));
        let prior = entries(&[("g1.txt", false), ("g2.txt", false), ("g3.txt", false)]);
        let plan = Plan::build(&dry, &prior, &prior, vec![]);
        assert!(plan.force);
        assert_eq!(plan.big[0].kind, BigKind::AllChanged);
    }

    /// New on both sides but equal (a folder restored from one destination
    /// meeting the same files on the next): bisync warns, then says they
    /// are equal or folders, and renames nothing. Lines as rclone 1.75.2
    /// printed them in such a run.
    #[test]
    fn equal_copies_on_both_sides_are_no_conflict() {
        let lines = [
            r#"{"level":"notice","msg":"- WARNING           New or changed in both paths                - Old projects"}"#,
            r#"{"level":"info","msg":"This is a directory, not a file. Skipping equality check and will not rename: Old projects"}"#,
            r#"{"level":"notice","msg":"- WARNING           New or changed in both paths                - Old projects/2019/a.txt"}"#,
            r#"{"level":"info","msg":"Files are equal! Skipping: Old projects/2019/a.txt"}"#,
            r#"{"level":"notice","msg":"- WARNING           New or changed in both paths                - Notes/plan.md"}"#,
            r#"{"level":"notice","msg":"- Path2             Queue copy to Path1                         - /x/src/Notes/plan.conflict-2026-10-13-1.md"}"#,
        ];
        let events: Vec<Event> = lines.iter().map(|l| events::decode(l)).collect();
        let dry = DryRun::collect(&events);
        assert_eq!(
            dry.conflicts,
            [PlannedConflict {
                path: "Notes/plan.md".into(),
                kept_as: None
            }]
        );
        let mut tally = crate::sync::summary::TwoWayTally::default();
        for e in &events {
            tally.apply(e);
        }
        assert_eq!(tally.conflicts.len(), 1);
        assert_eq!(tally.conflicts[0].path, "Notes/plan.md");
    }

    #[test]
    fn deleted_on_both_sides_is_nothing() {
        let dry = DryRun::collect(&events::decode_all(&fixture("bisync-after-conflict.jsonl")));
        let plan = Plan::build(&dry, &prior(), &prior(), vec![]);
        assert!(plan.is_empty(), "{plan:#?}");
        assert!(plan.delete_remote.is_empty() && plan.delete_local.is_empty());
    }

    #[test]
    fn resync_offer_from_the_recorded_dry_run() {
        let events = events::decode_all(&fixture("bisync-resync-dry-run.jsonl"));
        // Unfiltered: an earlier conflict copy is on the destination, and
        // this Mac has a file of the next name.
        let listed: HashSet<String> = [
            "only-on-destination.txt",
            "mod.txt",
            "mod.conflict-2026-10-13-1.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut offer = ResyncOffer::default();
        offer.fill(&events, &listed, "2026-10-13", |n| {
            n == "mod.conflict-2026-10-13-2.txt"
        });
        assert_eq!(offer.to_local.files(), 1, "only-on-destination.txt");
        assert_eq!(offer.to_remote.changed.len(), 1, "mod.txt is overwritten");
        assert_eq!(
            offer.kept,
            [PlannedConflict {
                path: "mod.txt".into(),
                kept_as: Some("mod.conflict-2026-10-13-3.txt".into())
            }]
        );
        assert!(offer.to_remote.new.len() > 10);
    }

    #[test]
    fn conflict_names() {
        let none = |_: &str| false;
        assert_eq!(
            conflict_name("Notes/plan.md", "2026-10-13", none),
            "Notes/plan.conflict-2026-10-13-1.md"
        );
        assert_eq!(
            conflict_name("arch.tar.gz", "2026-10-13", none),
            "arch.tar.conflict-2026-10-13-1.gz"
        );
        assert_eq!(
            conflict_name("noext", "2026-10-13", none),
            "noext.conflict-2026-10-13-1"
        );
        assert_eq!(
            conflict_name(".hidden", "2026-10-13", none),
            ".hidden.conflict-2026-10-13-1"
        );
        assert_eq!(
            conflict_name(" a b .txt", "2026-10-13", none),
            " a b .conflict-2026-10-13-1.txt"
        );
        let taken: HashSet<String> = [
            "a.conflict-2026-10-13-1.txt".to_string(),
            "a.conflict-2026-10-13-2.txt".to_string(),
        ]
        .into();
        assert_eq!(
            conflict_name("a.txt", "2026-10-13", |n| taken.contains(n)),
            "a.conflict-2026-10-13-3.txt"
        );
        assert!(bisync::is_conflict_copy(&conflict_name(
            "x/y.md",
            "2026-10-13",
            none
        )));
    }

    #[test]
    fn conflict_winners_go_up_as_changes() {
        let dry = DryRun::collect(&events::decode_all(&fixture("bisync-plan.jsonl")));
        let plan = Plan::build(&dry, &prior(), &prior(), vec![]);
        assert!(plan
            .to_remote
            .changed
            .iter()
            .any(|f| f.path == "Notes/plan.md"));
        assert_eq!(
            plan.conflicts[0].kept_as, None,
            "named by OneFolder, not here"
        );
    }

    #[test]
    fn loser_names_and_folders() {
        let roots = ["/x/src", "onefolder-work:Docs"];
        assert_eq!(
            loser_name(
                "Notes/plan.md",
                "/x/src/Notes/plan.conflict-2026-10-13-1.md",
                &roots
            ),
            "Notes/plan.conflict-2026-10-13-1.md"
        );
        assert_eq!(
            loser_name(
                "a:b.md",
                "onefolder-work:Docs/a:b.conflict-2026-10-13-1.md",
                &roots
            ),
            "a:b.conflict-2026-10-13-1.md"
        );
        assert_eq!(
            loser_name(
                "N/plan.md",
                "elsewhere/plan.conflict-2026-10-13-1.md",
                &roots
            ),
            "N/plan.conflict-2026-10-13-1.md"
        );
        assert!(under("a/b", "a"));
        assert!(under("a", "a"));
        assert!(!under("ab/c", "a"));
    }
}
