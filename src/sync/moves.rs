//! Move detection for one-way runs: files moved or renamed locally, whose
//! old copy is still on the destination under the old name.
//!
//! `rclone copy` cannot see a move (`--track-renames` only works for
//! `sync`), so a moved file would be uploaded again at its new path and the
//! old copy left behind, since one-way never deletes. OneFolder pairs each
//! new file with a file found only on the destination that has the same
//! content, and offers to move that copy on the destination instead:
//!
//! 1. candidates: a new file (more than 0 bytes) and destination-only files
//!    of the same size ([`candidates`]);
//! 2. pairs: the same QuickXorHash on both sides; where one file matches
//!    several, the pair with the same modification time (2 s window), then
//!    the same name, wins; a tie is not guessed at, so the file is uploaded
//!    ([`pair`]);
//! 3. groups: when every file of a destination folder moves to one new
//!    folder with the same relative paths, it is one folder move;
//!    otherwise each file moves by itself ([`group`]).
//!
//! This module only decides; `sync::oneway` runs the listings, the hashing
//! and the moves.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use super::nfc;
use super::oneway::PlannedFile;
use crate::rclone::listing::Entry;

/// How far apart two modification times may be and still count as the
/// same, as rclone's `--modify-window 2s`.
pub const SAME_TIME_SECS: i64 = 2;

/// Folder moves need at least this many files; a folder holding a single
/// file reads better as that file's move.
pub const FOLDER_MIN_FILES: u64 = 2;

/// A file that will be moved on the destination instead of uploaded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PlannedMove {
    /// Its path on the destination now.
    pub from: String,
    /// Its new path, as the source has it.
    pub to: String,
    pub size: u64,
    /// The index of its group in [`super::oneway::Preview::move_groups`].
    pub group: usize,
}

/// One line of the Moved list, and one `rclone moveto`: a whole folder or
/// a single file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MoveGroup {
    /// The folder or file on the destination now (a folder without a
    /// trailing slash).
    pub from: String,
    /// Where it goes.
    pub to: String,
    pub files: u64,
    pub bytes: u64,
    /// A folder move (`rclone moveto` of a folder, one server-side move).
    pub folder: bool,
}

impl MoveGroup {
    /// `Projects/2025/` for a folder, the path itself for a file.
    pub fn old_shown(&self) -> String {
        shown(&self.from, self.folder)
    }

    pub fn new_shown(&self) -> String {
        shown(&self.to, self.folder)
    }
}

fn shown(path: &str, folder: bool) -> String {
    if folder {
        format!("{path}/")
    } else {
        path.to_string()
    }
}

/// What happens to the moved files: still to be asked, moved on the
/// destination, or uploaded again under their new names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MovesChoice {
    #[default]
    Undecided,
    Move,
    /// Uploaded again (`--moves copy`), named `copy` in the JSON as in
    /// the flag and the `moves` event.
    #[serde(rename = "copy")]
    Upload,
}

/// A confirmed pair, before grouping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub from: String,
    pub to: String,
    pub size: u64,
}

/// The files worth hashing: each new file (more than 0 bytes) with at
/// least one destination-only file of the same size, and those
/// destination-only files. Both lists are sorted and without repeats.
pub fn candidates(new: &[PlannedFile], dest_only: &[Entry]) -> (Vec<String>, Vec<String>) {
    let mut by_size: HashMap<u64, Vec<&Entry>> = HashMap::new();
    for e in dest_only {
        if let Some(size) = size_of(e) {
            by_size.entry(size).or_default().push(e);
        }
    }
    let mut source = Vec::new();
    let mut dest = HashSet::new();
    for f in new.iter().filter(|f| f.size > 0 && !f.modtime_only) {
        if let Some(entries) = by_size.get(&f.size) {
            source.push(f.path.clone());
            dest.extend(entries.iter().map(|e| e.path.clone()));
        }
    }
    source.sort();
    source.dedup();
    let mut dest: Vec<String> = dest.into_iter().collect();
    dest.sort();
    (source, dest)
}

/// A file's size, `None` for a folder or an empty file (never paired:
/// every empty file has the same content).
fn size_of(e: &Entry) -> Option<u64> {
    if e.is_dir {
        return None;
    }
    u64::try_from(e.size).ok().filter(|s| *s > 0)
}

/// A path as every destination OneFolder supports compares it: composed
/// (NFC) and case-folded. macOS drives, OneDrive and rclone's matching on
/// them ignore case, so `Report.pdf` and `report.pdf` are one file.
pub fn fold(path: &str) -> String {
    nfc(path).to_lowercase()
}

/// A time from `lsjson` (RFC 3339) in nanoseconds, `None` when unreadable.
fn nanos(time: &str) -> Option<i128> {
    time.parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_nanosecond())
}

/// Two times (see [`nanos`]) are within [`SAME_TIME_SECS`] of each other;
/// an unreadable time never matches.
fn same_nanos(a: Option<i128>, b: Option<i128>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() <= i128::from(SAME_TIME_SECS) * 1_000_000_000,
        _ => false,
    }
}

#[cfg(test)]
fn same_time(a: &str, b: &str) -> bool {
    same_nanos(nanos(a), nanos(b))
}

/// One file of a bucket, with what the tie-break needs worked out once:
/// its folded path components and its time.
struct Side {
    comps: Vec<String>,
    time: Option<i128>,
}

impl Side {
    fn new(path: &str, time: Option<i128>) -> Side {
        Side {
            comps: fold(path).split('/').map(str::to_string).collect(),
            time,
        }
    }

    /// The last `k` components, joined: the index key of a path suffix.
    fn suffix(&self, k: usize) -> String {
        self.comps[self.comps.len() - k..].join("/")
    }
}

/// The files of one side of a bucket, indexed by every path suffix (the
/// last 1, 2, ... components), so the files sharing the longest suffix
/// with a file of the other side are found without comparing every pair.
struct Index {
    sides: Vec<Side>,
    by_suffix: HashMap<(usize, String), Vec<usize>>,
    used: Vec<bool>,
}

impl Index {
    fn new(sides: Vec<Side>) -> Index {
        let mut by_suffix: HashMap<(usize, String), Vec<usize>> = HashMap::new();
        for (i, s) in sides.iter().enumerate() {
            for k in 1..=s.comps.len() {
                by_suffix.entry((k, s.suffix(k))).or_default().push(i);
            }
        }
        let used = vec![false; sides.len()];
        Index {
            sides,
            by_suffix,
            used,
        }
    }

    /// The single best match in this index for `x`, `None` on a tie.
    /// Best means, in order: the longest common path suffix (the same
    /// name, then the same name in the same folder, and so on, which keeps
    /// identical files of a renamed folder in their places), then the
    /// same modification time.
    fn best(&self, x: &Side) -> Option<usize> {
        for k in (1..=x.comps.len()).rev() {
            let Some(list) = self.by_suffix.get(&(k, x.suffix(k))) else {
                continue;
            };
            let live: Vec<usize> = list.iter().copied().filter(|i| !self.used[*i]).collect();
            if !live.is_empty() {
                return self.by_time(x, &live);
            }
        }
        let live: Vec<usize> = (0..self.sides.len()).filter(|i| !self.used[*i]).collect();
        self.by_time(x, &live)
    }

    /// Among files equally good by path, the only one, or the only one
    /// with the same modification time.
    fn by_time(&self, x: &Side, live: &[usize]) -> Option<usize> {
        if live.len() == 1 {
            return Some(live[0]);
        }
        let mut same = live
            .iter()
            .copied()
            .filter(|i| same_nanos(x.time, self.sides[*i].time));
        match (same.next(), same.next()) {
            (Some(i), None) => Some(i),
            _ => None,
        }
    }
}

/// Pairs new files with destination-only files of identical content.
///
/// `source_times` maps a source path (folded, see [`fold`]) to its
/// modification time from the source listing; `source_hashes` and
/// `dest_hashes` map paths to QuickXorHashes. A file without a hash on
/// either side is never paired. Where one file matches several of
/// identical content, the pair with the longest common path suffix wins,
/// then the one with the same modification time, but only when it is the
/// best for both files of the pair; a tie leaves the file to be uploaded.
/// Each destination file is used at most once. Sorted by `to`.
pub fn pair(
    new: &[PlannedFile],
    source_times: &HashMap<String, String>,
    dest_only: &[Entry],
    source_hashes: &HashMap<String, String>,
    dest_hashes: &HashMap<String, String>,
) -> Vec<Pair> {
    let source_hashes_nfc: HashMap<String, &String> =
        source_hashes.iter().map(|(p, h)| (nfc(p), h)).collect();
    let dest_hashes_nfc: HashMap<String, &String> =
        dest_hashes.iter().map(|(p, h)| (nfc(p), h)).collect();
    let hash_of = |map: &HashMap<String, String>, nfc_map: &HashMap<String, &String>, p: &str| {
        map.get(p)
            .or_else(|| nfc_map.get(&nfc(p)).copied())
            .filter(|h| !h.is_empty())
            .cloned()
    };

    // Same size and same hash: one bucket per content.
    let mut buckets: BTreeMap<(u64, String), Bucket> = BTreeMap::new();
    for f in new.iter().filter(|f| f.size > 0 && !f.modtime_only) {
        if let Some(h) = hash_of(source_hashes, &source_hashes_nfc, &f.path) {
            buckets.entry((f.size, h)).or_default().0.push(f);
        }
    }
    for e in dest_only {
        let Some(size) = size_of(e) else { continue };
        if let Some(h) = hash_of(dest_hashes, &dest_hashes_nfc, &e.path) {
            if let Some(b) = buckets.get_mut(&(size, h)) {
                b.1.push(e);
            }
        }
    }

    let mut pairs = Vec::new();
    for (_, (sources, dests)) in buckets {
        if dests.is_empty() {
            continue;
        }
        let mut s_index = Index::new(
            sources
                .iter()
                .map(|f| {
                    let time = source_times.get(&fold(&f.path)).and_then(|t| nanos(t));
                    Side::new(&f.path, time)
                })
                .collect(),
        );
        let mut d_index = Index::new(
            dests
                .iter()
                .map(|e| Side::new(&e.path, nanos(&e.mod_time)))
                .collect(),
        );
        // Accept every pair that is the unique best for both of its files,
        // then look again among those left, until nothing changes.
        loop {
            let mut taken: Vec<(usize, usize)> = Vec::new();
            for i in 0..sources.len() {
                if s_index.used[i] {
                    continue;
                }
                let Some(j) = d_index.best(&s_index.sides[i]) else {
                    continue;
                };
                if s_index.best(&d_index.sides[j]) == Some(i) {
                    taken.push((i, j));
                }
            }
            if taken.is_empty() {
                break;
            }
            for (i, j) in taken {
                s_index.used[i] = true;
                d_index.used[j] = true;
                pairs.push(Pair {
                    from: dests[j].path.clone(),
                    to: sources[i].path.clone(),
                    size: sources[i].size,
                });
            }
        }
    }
    pairs.sort_by(|a, b| a.to.cmp(&b.to));
    pairs
}

/// The new files and destination-only files of one content.
type Bucket<'a> = (Vec<&'a PlannedFile>, Vec<&'a Entry>);

/// `a/b` is `a`'s or `b`'s ancestor, or the same folder.
fn nested(a: &[String], b: &[String]) -> bool {
    let n = a.len().min(b.len());
    a[..n] == b[..n]
}

/// Groups the pairs into folder moves and single-file moves.
///
/// A folder move takes destination folder `A` to `B` when every file the
/// destination listing (`dest_paths`, every file listed there) has under
/// `A/` is in a pair that moves it to `B/` with the same relative path,
/// there are at least [`FOLDER_MIN_FILES`] of them, and nothing is listed
/// under `B/` yet. The shallowest such folder wins. Folder moves never
/// overlap one another (no `A` or `B` inside another's `A` or `B`), so
/// they can run side by side; single moves run after them.
///
/// Gives the groups (folders first, each kind sorted by `to`) and every
/// file's move, sorted by `to`, pointing at its group.
pub fn group(pairs: &[Pair], dest_paths: &[String]) -> (Vec<MoveGroup>, Vec<PlannedMove>) {
    let comps = |p: &str| -> Vec<String> { fold(p).split('/').map(str::to_string).collect() };
    // Files listed under each folder of the destination, by folded
    // components (see [`fold`]).
    let mut under: HashMap<Vec<String>, u64> = HashMap::new();
    for p in dest_paths {
        let c = comps(p);
        for k in 1..c.len() {
            *under.entry(c[..k].to_vec()).or_default() += 1;
        }
    }
    // For each pair, the folder moves that would explain it, shallowest
    // first, with the original spelling of `B` (as the source has it).
    struct Candidate {
        a: Vec<String>,
        b: Vec<String>,
        a_text: String,
        b_text: String,
    }
    let mut options: Vec<Vec<Candidate>> = Vec::with_capacity(pairs.len());
    let mut votes: HashMap<(Vec<String>, Vec<String>), u64> = HashMap::new();
    // The pairs each folder move would explain.
    let mut explains: HashMap<(Vec<String>, Vec<String>), Vec<usize>> = HashMap::new();
    for (pi, p) in pairs.iter().enumerate() {
        let (fa, ta) = (comps(&p.from), comps(&p.to));
        let (from_raw, to_raw): (Vec<&str>, Vec<&str>) =
            (p.from.split('/').collect(), p.to.split('/').collect());
        let mut common = 0;
        while common < fa.len().min(ta.len())
            && fa[fa.len() - 1 - common] == ta[ta.len() - 1 - common]
        {
            common += 1;
        }
        let mut list = Vec::new();
        for m in (1..=common).rev() {
            let (a, b) = (&fa[..fa.len() - m], &ta[..ta.len() - m]);
            if a.is_empty() || b.is_empty() || nested(a, b) {
                continue;
            }
            // The raw spelling has as many components as the NFC one.
            let a_text = from_raw[..from_raw.len().saturating_sub(m)].join("/");
            let b_text = to_raw[..to_raw.len().saturating_sub(m)].join("/");
            *votes.entry((a.to_vec(), b.to_vec())).or_default() += 1;
            explains
                .entry((a.to_vec(), b.to_vec()))
                .or_default()
                .push(pi);
            list.push(Candidate {
                a: a.to_vec(),
                b: b.to_vec(),
                a_text,
                b_text,
            });
        }
        options.push(list);
    }
    let valid = |a: &Vec<String>, b: &Vec<String>| -> bool {
        let n = votes.get(&(a.clone(), b.clone())).copied().unwrap_or(0);
        n >= FOLDER_MIN_FILES
            && under.get(a).copied().unwrap_or(0) == n
            && under.get(b).copied().unwrap_or(0) == 0
    };

    // Pairs in `to` order, so the choice does not depend on input order.
    let mut order: Vec<usize> = (0..pairs.len()).collect();
    order.sort_by(|x, y| pairs[*x].to.cmp(&pairs[*y].to));
    let mut chosen: Vec<(Vec<String>, Vec<String>, String, String)> = Vec::new();
    let mut claimed: Vec<Option<usize>> = vec![None; pairs.len()];
    for &i in &order {
        if claimed[i].is_some() {
            continue;
        }
        for o in &options[i] {
            if !valid(&o.a, &o.b) {
                continue;
            }
            let clash = chosen.iter().any(|(a, b, _, _)| {
                nested(&o.a, a) || nested(&o.a, b) || nested(&o.b, a) || nested(&o.b, b)
            });
            if clash {
                continue;
            }
            let g = chosen.len();
            chosen.push((o.a.clone(), o.b.clone(), o.a_text.clone(), o.b_text.clone()));
            for &k in explains
                .get(&(o.a.clone(), o.b.clone()))
                .into_iter()
                .flatten()
            {
                if claimed[k].is_none() {
                    claimed[k] = Some(g);
                }
            }
            break;
        }
    }

    // Folder groups in `to` order, then single files in `to` order.
    let mut folder_order: Vec<usize> = (0..chosen.len()).collect();
    folder_order.sort_by(|x, y| chosen[*x].3.cmp(&chosen[*y].3));
    let mut renumber = vec![0; chosen.len()];
    let mut groups: Vec<MoveGroup> = Vec::new();
    for (new_index, &old) in folder_order.iter().enumerate() {
        renumber[old] = new_index;
        let (_, _, a_text, b_text) = &chosen[old];
        groups.push(MoveGroup {
            from: a_text.clone(),
            to: b_text.clone(),
            files: 0,
            bytes: 0,
            folder: true,
        });
    }
    let mut moves = Vec::with_capacity(pairs.len());
    for &i in &order {
        let p = &pairs[i];
        let g = match claimed[i] {
            Some(g) => renumber[g],
            None => {
                groups.push(MoveGroup {
                    from: p.from.clone(),
                    to: p.to.clone(),
                    files: 0,
                    bytes: 0,
                    folder: false,
                });
                groups.len() - 1
            }
        };
        groups[g].files += 1;
        groups[g].bytes += p.size;
        moves.push(PlannedMove {
            from: p.from.clone(),
            to: p.to.clone(),
            size: p.size,
            group: g,
        });
    }
    (groups, moves)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "2026-10-01T10:00:00+11:00";

    fn new_file(path: &str, size: u64) -> PlannedFile {
        new(path, size)
    }

    fn new(path: &str, size: u64) -> PlannedFile {
        PlannedFile {
            path: path.into(),
            size,
            modtime_only: false,
        }
    }

    fn entry(path: &str, size: i64, time: &str) -> Entry {
        Entry {
            path: path.into(),
            size,
            mod_time: time.into(),
            ..Entry::default()
        }
    }

    fn map(items: &[(&str, &str)]) -> HashMap<String, String> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Source times all `T` unless given.
    fn times(paths: &[&str]) -> HashMap<String, String> {
        paths.iter().map(|p| (fold(p), T.to_string())).collect()
    }

    fn pairs_of(pairs: &[Pair]) -> Vec<(&str, &str)> {
        pairs
            .iter()
            .map(|p| (p.from.as_str(), p.to.as_str()))
            .collect()
    }

    #[test]
    fn candidates_are_new_files_with_a_destination_file_of_the_same_size() {
        let new = vec![
            new("b/a.txt", 5),
            new("b/empty.txt", 0),
            new("b/other.txt", 9),
            PlannedFile {
                path: "t.txt".into(),
                size: 5,
                modtime_only: true,
            },
        ];
        let dest = vec![
            entry("a.txt", 5, T),
            entry("twin.txt", 5, T),
            entry("empty.txt", 0, T),
        ];
        let (s, d) = candidates(&new, &dest);
        assert_eq!(s, ["b/a.txt"]);
        assert_eq!(d, ["a.txt", "twin.txt"]);
    }

    #[test]
    fn a_single_move_pairs_by_hash() {
        let new = vec![new("Archive/report.pdf", 100), new("fresh.txt", 100)];
        let dest = vec![entry("Old/report.pdf", 100, T)];
        let p = pair(
            &new,
            &times(&["Archive/report.pdf", "fresh.txt"]),
            &dest,
            &map(&[("Archive/report.pdf", "aa"), ("fresh.txt", "bb")]),
            &map(&[("Old/report.pdf", "aa")]),
        );
        assert_eq!(pairs_of(&p), [("Old/report.pdf", "Archive/report.pdf")]);
        assert_eq!(p[0].size, 100);
    }

    #[test]
    fn moved_and_changed_stays_new() {
        // Same size, different content: not the same file.
        let new = vec![new("New/a.txt", 10)];
        let dest = vec![entry("Old/a.txt", 10, T)];
        let p = pair(
            &new,
            &times(&["New/a.txt"]),
            &dest,
            &map(&[("New/a.txt", "11")]),
            &map(&[("Old/a.txt", "22")]),
        );
        assert!(p.is_empty());
    }

    #[test]
    fn empty_files_and_files_without_a_hash_are_never_paired() {
        let new = vec![
            new("x/empty", 0),
            new("x/nohash", 4),
            new("x/dest-nohash", 6),
        ];
        let dest = vec![
            entry("empty", 0, T),
            entry("nohash", 4, T),
            entry("dest-nohash", 6, T),
        ];
        let p = pair(
            &new,
            &times(&["x/empty", "x/nohash", "x/dest-nohash"]),
            &dest,
            &map(&[("x/empty", "00"), ("x/dest-nohash", "66")]),
            &map(&[("empty", "00"), ("nohash", "44"), ("dest-nohash", "")]),
        );
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn same_content_twins_are_not_guessed() {
        // One file moved, and a copy of it made with the same time and
        // name: two new files, one old copy. Which one is the move?
        let new = vec![new("x/a.txt", 7), new("y/a.txt", 7)];
        let dest = vec![entry("a.txt", 7, T)];
        let h = map(&[("x/a.txt", "77"), ("y/a.txt", "77")]);
        let p = pair(
            &new,
            &times(&["x/a.txt", "y/a.txt"]),
            &dest,
            &h,
            &map(&[("a.txt", "77")]),
        );
        assert!(p.is_empty(), "{p:?}");
        // Two old copies, one new file, the same everything: no guess.
        let p = pair(
            &new[..1],
            &times(&["x/a.txt"]),
            &[entry("p/a.txt", 7, T), entry("q/a.txt", 7, T)],
            &h,
            &map(&[("p/a.txt", "77"), ("q/a.txt", "77")]),
        );
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn the_path_then_the_time_settles_a_tie() {
        let h = map(&[("x/a.txt", "77"), ("y/b.txt", "77")]);
        let dest_h = map(&[("old/a.txt", "77"), ("old/b.txt", "77")]);
        // Names settle it: a goes to a, b to b.
        let new = vec![new("x/a.txt", 7), new("y/b.txt", 7)];
        let dest = vec![entry("old/a.txt", 7, T), entry("old/b.txt", 7, T)];
        let p = pair(&new, &times(&["x/a.txt", "y/b.txt"]), &dest, &h, &dest_h);
        assert_eq!(
            pairs_of(&p),
            [("old/a.txt", "x/a.txt"), ("old/b.txt", "y/b.txt")]
        );
        // Names beat times: b's old copy has a's time, a's old copy a
        // time far off, and still each goes to its own name.
        let dest = vec![
            entry("old/a.txt", 7, "2020-01-01T00:00:00Z"),
            entry("old/b.txt", 7, T),
        ];
        let mut t = times(&["x/a.txt"]);
        t.insert("y/b.txt".into(), "2019-01-01T00:00:00Z".into());
        let p = pair(&new, &t, &dest, &h, &dest_h);
        assert_eq!(
            pairs_of(&p),
            [("old/a.txt", "x/a.txt"), ("old/b.txt", "y/b.txt")]
        );
        // With no name in common, the time settles it.
        let renamed = vec![new_file("x/c.txt", 7), new_file("y/d.txt", 7)];
        let h2 = map(&[("x/c.txt", "77"), ("y/d.txt", "77")]);
        let mut t = times(&["x/c.txt"]);
        t.insert("y/d.txt".into(), "2019-01-01T00:00:00Z".into());
        let p = pair(&renamed, &t, &dest, &h2, &dest_h);
        assert_eq!(
            pairs_of(&p),
            [("old/b.txt", "x/c.txt"), ("old/a.txt", "y/d.txt")]
        );
        // Within the 2 s window counts as the same time.
        assert!(same_time(T, "2026-10-01T10:00:01.9+11:00"));
        assert!(!same_time(T, "2026-10-01T10:00:02.1+11:00"));
        assert!(same_time(T, "2026-09-30T23:00:00Z"));
        assert!(!same_time(T, "not a time"));
    }

    #[test]
    fn each_destination_file_is_used_once() {
        // Two new copies, two old copies, all the same: names settle each.
        let new = vec![new("n/a.txt", 3), new("n/b.txt", 3), new("n/c.txt", 3)];
        let dest = vec![entry("o/a.txt", 3, T), entry("o/b.txt", 3, T)];
        let h = map(&[("n/a.txt", "3"), ("n/b.txt", "3"), ("n/c.txt", "3")]);
        let dh = map(&[("o/a.txt", "3"), ("o/b.txt", "3")]);
        let p = pair(
            &new,
            &times(&["n/a.txt", "n/b.txt", "n/c.txt"]),
            &dest,
            &h,
            &dh,
        );
        assert_eq!(
            pairs_of(&p),
            [("o/a.txt", "n/a.txt"), ("o/b.txt", "n/b.txt")]
        );
    }

    #[test]
    fn identical_files_of_a_renamed_folder_stay_in_their_places() {
        // Proj/pkg1..5/LICENSE are identical; each goes to its own package.
        let mut new = Vec::new();
        let mut dest = Vec::new();
        let (mut h, mut dh) = (HashMap::new(), HashMap::new());
        for i in 1..=5 {
            let (to, from) = (
                format!("Arch/pkg{i}/LICENSE"),
                format!("Proj/pkg{i}/LICENSE"),
            );
            new.push(new_file(&to, 1000));
            dest.push(entry(&from, 1000, T));
            h.insert(to.clone(), "11".to_string());
            dh.insert(from, "11".to_string());
        }
        let paths: Vec<&str> = new.iter().map(|f| f.path.as_str()).collect();
        let p = pair(&new, &times(&paths), &dest, &h, &dh);
        assert_eq!(p.len(), 5, "{p:?}");
        assert!(p.iter().all(|x| x.from[5..] == x.to[5..]), "{p:?}");
        let dest_paths: Vec<String> = dest.iter().map(|e| e.path.clone()).collect();
        let (groups, _) = group(&p, &dest_paths);
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert!(groups[0].folder);
    }

    /// 3,000 identical files in a renamed folder, with distinct names and
    /// with one name in distinct folders, pair at once (debug build).
    #[test]
    fn many_identical_files_pair_quickly() {
        for shape in ["Old/f{i}.bin", "Old/d{i}/index.js"] {
            let mut new = Vec::new();
            let mut dest = Vec::new();
            let (mut h, mut dh) = (HashMap::new(), HashMap::new());
            let mut t = HashMap::new();
            for i in 0..3000 {
                let from = shape.replace("{i}", &i.to_string());
                let to = from.replacen("Old", "New", 1);
                new.push(new_file(&to, 64));
                dest.push(entry(&from, 64, T));
                h.insert(to.clone(), "aa".to_string());
                dh.insert(from, "aa".to_string());
                t.insert(fold(&to), T.to_string());
            }
            let started = std::time::Instant::now();
            let p = pair(&new, &t, &dest, &h, &dh);
            let dest_paths: Vec<String> = dest.iter().map(|e| e.path.clone()).collect();
            let (groups, moves) = group(&p, &dest_paths);
            let took = started.elapsed();
            assert_eq!(moves.len(), 3000, "{shape}");
            assert_eq!(groups.len(), 1, "{shape}");
            assert!(took.as_millis() < 900, "{shape} took {took:?}");
        }
    }

    #[test]
    fn case_only_differences_are_the_same_folder() {
        // Old/ holds a.txt and B.txt; the listing spells the folder
        // differently from the pairs: still one folder move.
        let pairs = vec![
            p("Old/a.txt", "New/a.txt", 1),
            p("Old/B.txt", "New/B.txt", 1),
        ];
        let (groups, _) = group(&pairs, &owned(&["old/a.txt", "OLD/b.txt"]));
        assert_eq!(groups.len(), 1, "{groups:?}");
        // Something under new/ in another case blocks the folder move.
        let (groups, _) = group(&pairs, &owned(&["Old/a.txt", "Old/B.txt", "new/other.txt"]));
        assert!(groups.iter().all(|g| !g.folder), "{groups:?}");
        assert_eq!(fold("Re\u{301}sume\u{301}/X.PDF"), "r\u{e9}sum\u{e9}/x.pdf");
    }

    #[test]
    fn nfc_paths_match() {
        let nfd = "Re\u{301}sume\u{301}.docx";
        let nfc_name = "R\u{e9}sum\u{e9}.docx";
        let new = vec![new(&format!("New/{nfd}"), 5)];
        let dest = vec![entry(&format!("Old/{nfc_name}"), 5, T)];
        let p = pair(
            &new,
            &times(&[&format!("New/{nfd}")]),
            &dest,
            // hashsum gave the composed spelling back
            &map(&[(&format!("New/{nfc_name}"), "55")]),
            &map(&[(&format!("Old/{nfc_name}"), "55")]),
        );
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].to, format!("New/{nfd}"), "kept as rclone gave it");
        // The leaf names are the same once composed, so a folder move.
        let p2 = Pair {
            from: format!("Old/x/{nfc_name}"),
            to: format!("New/x/{nfd}"),
            size: 5,
        };
        let p1 = Pair {
            from: "Old/y.txt".into(),
            to: "New/y.txt".into(),
            size: 1,
        };
        let (groups, _) = group(
            &[p1, p2],
            &["Old/y.txt".into(), format!("Old/x/{nfc_name}")],
        );
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(
            (groups[0].from.as_str(), groups[0].to.as_str()),
            ("Old", "New")
        );
    }

    fn p(from: &str, to: &str, size: u64) -> Pair {
        Pair {
            from: from.into(),
            to: to.into(),
            size,
        }
    }

    fn owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_renamed_folder_is_one_move() {
        let pairs = vec![
            p("Projects/2025/a.txt", "Archive/2025/a.txt", 1),
            p("Projects/2025/sub/b.txt", "Archive/2025/sub/b.txt", 2),
            p("Projects/2025/c.txt", "Archive/2025/c.txt", 3),
            p("loose.txt", "Elsewhere/loose.txt", 4),
        ];
        let dest = owned(&[
            "Projects/2025/a.txt",
            "Projects/2025/sub/b.txt",
            "Projects/2025/c.txt",
            "Projects/keep.txt",
            "loose.txt",
            "same.txt",
        ]);
        let (groups, moves) = group(&pairs, &dest);
        assert_eq!(
            groups,
            [
                MoveGroup {
                    from: "Projects/2025".into(),
                    to: "Archive/2025".into(),
                    files: 3,
                    bytes: 6,
                    folder: true,
                },
                MoveGroup {
                    from: "loose.txt".into(),
                    to: "Elsewhere/loose.txt".into(),
                    files: 1,
                    bytes: 4,
                    folder: false,
                },
            ]
        );
        assert_eq!(groups[0].old_shown(), "Projects/2025/");
        assert_eq!(groups[1].new_shown(), "Elsewhere/loose.txt");
        let order: Vec<(&str, usize)> = moves.iter().map(|m| (m.to.as_str(), m.group)).collect();
        assert_eq!(
            order,
            [
                ("Archive/2025/a.txt", 0),
                ("Archive/2025/c.txt", 0),
                ("Archive/2025/sub/b.txt", 0),
                ("Elsewhere/loose.txt", 1),
            ]
        );
        // With Projects/keep.txt gone the whole of Projects moves.
        let dest: Vec<String> = dest
            .into_iter()
            .filter(|d| d != "Projects/keep.txt")
            .collect();
        let (groups, _) = group(&pairs, &dest);
        assert_eq!(
            (groups[0].from.as_str(), groups[0].to.as_str()),
            ("Projects", "Archive")
        );
    }

    #[test]
    fn a_folder_with_another_file_on_the_destination_is_not_a_folder_move() {
        let pairs = vec![
            p("Old/a.txt", "New/a.txt", 1),
            p("Old/b.txt", "New/b.txt", 1),
        ];
        let (groups, moves) = group(&pairs, &owned(&["Old/a.txt", "Old/b.txt", "Old/extra.txt"]));
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| !g.folder && g.files == 1));
        assert_eq!(moves[1].group, 1);
        // Nor when something is already listed under the new folder.
        let (groups, _) = group(&pairs, &owned(&["Old/a.txt", "Old/b.txt", "New/c.txt"]));
        assert!(groups.iter().all(|g| !g.folder));
        // Nor for a folder of one file, nor a renamed file.
        let (groups, _) = group(&pairs[..1], &owned(&["Old/a.txt"]));
        assert!(!groups[0].folder);
        let (groups, _) = group(
            &[
                p("Old/a.txt", "New/z.txt", 1),
                p("Old/b.txt", "New/b.txt", 1),
            ],
            &owned(&["Old/a.txt", "Old/b.txt"]),
        );
        assert!(groups.iter().all(|g| !g.folder), "{groups:?}");
        // Nor into a folder of its own, or out to the top level.
        let (groups, _) = group(
            &[
                p("A/a.txt", "A/sub/a.txt", 1),
                p("A/b.txt", "A/sub/b.txt", 1),
            ],
            &owned(&["A/a.txt", "A/b.txt"]),
        );
        assert!(groups.iter().all(|g| !g.folder), "{groups:?}");
        let (groups, _) = group(
            &[p("A/a.txt", "a.txt", 1), p("A/b.txt", "b.txt", 1)],
            &owned(&["A/a.txt", "A/b.txt"]),
        );
        assert!(groups.iter().all(|g| !g.folder), "{groups:?}");
    }

    #[test]
    fn folder_moves_never_overlap() {
        // X/ moves to Y/, and Z/ moves to X/sub/, inside the first move's
        // old folder: only the first is a folder move.
        let pairs = vec![
            p("X/a.txt", "Y/a.txt", 1),
            p("X/b.txt", "Y/b.txt", 1),
            p("Z/c.txt", "X/sub/c.txt", 1),
            p("Z/d.txt", "X/sub/d.txt", 1),
        ];
        let (groups, _) = group(
            &pairs,
            &owned(&["X/a.txt", "X/b.txt", "Z/c.txt", "Z/d.txt"]),
        );
        let folders: Vec<(&str, &str)> = groups
            .iter()
            .filter(|g| g.folder)
            .map(|g| (g.from.as_str(), g.to.as_str()))
            .collect();
        assert_eq!(folders.len(), 1, "{groups:?}");
        assert_eq!(groups.iter().map(|g| g.files).sum::<u64>(), 4);
    }
}
