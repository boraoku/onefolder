//! Tab completion for the form's folder fields: the source, a local
//! destination folder and a OneDrive folder.
//!
//! The logic is pure and works over a listing of names: [`split`] cuts
//! the typed value into the part kept as typed and the start of the name
//! being completed, [`matches`] picks the folders that fit, and [`Cycle`]
//! steps through them with Tab and Shift-Tab. Where the names come from
//! is the caller's business: [`local_folders`] reads one local folder (a
//! single `read_dir`, never recursive), and the OneDrive names arrive from
//! a background `rclone lsjson --dirs-only` run by the job form.

use std::io;
use std::path::{Path, PathBuf};

use crate::config;
use crate::sync::nfc;
use crate::ui;

/// A typed value cut where completion works on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    /// The text kept as typed, up to and including its last `/`: `~/`,
    /// `/Volumes/`, `Backups/` or empty.
    pub base: String,
    /// The start of the folder name being completed.
    pub prefix: String,
}

/// Cuts `value` for completion. A local field that is empty, or holds
/// just `~`, completes in the home folder (`~/`). A value ending in `/`
/// lists that folder's children.
pub fn split(value: &str, local: bool) -> Split {
    if local && (value.trim().is_empty() || value == "~") {
        return Split {
            base: "~/".to_string(),
            prefix: String::new(),
        };
    }
    match value.rfind('/') {
        Some(at) => Split {
            base: value[..=at].to_string(),
            prefix: value[at + 1..].to_string(),
        },
        None => Split {
            base: String::new(),
            prefix: value.to_string(),
        },
    }
}

/// The local folder a [`Split::base`] names, `~` expanded as
/// [`config::expand_tilde`] does. `None` when it is not a full path.
pub fn local_parent(base: &str) -> Option<PathBuf> {
    let path = PathBuf::from(config::expand_tilde(base));
    path.is_absolute().then_some(path)
}

/// The OneDrive folder a [`Split::base`] names, relative to the drive
/// root: no slashes around it, empty for the root.
pub fn remote_parent(base: &str) -> String {
    base.trim().trim_matches('/').to_string()
}

/// How names compare: composed (macOS stores names decomposed) and
/// case-folded, since the file systems and OneDrive ignore case.
fn fold(name: &str) -> String {
    nfc(name).to_lowercase()
}

/// The names in `names` that start with `prefix`, ignoring case, sorted
/// by name. Hidden folders (a leading dot) only when `prefix` starts with
/// a dot.
pub fn matches(names: &[String], prefix: &str) -> Vec<String> {
    let want = fold(prefix);
    let hidden = prefix.starts_with('.');
    let mut out: Vec<(String, String)> = names
        .iter()
        .filter(|n| hidden || !n.starts_with('.'))
        .map(|n| (fold(n), n.clone()))
        .filter(|(folded, _)| folded.starts_with(&want))
        .collect();
    out.sort();
    out.dedup_by(|a, b| a.1 == b.1);
    out.into_iter().map(|(_, n)| n).collect()
}

/// A completion in progress: the folders that matched and the one shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    /// [`Split::base`], kept exactly as typed (a `~` stays a `~`).
    pub base: String,
    pub matches: Vec<String>,
    pub index: usize,
}

impl Cycle {
    /// Starts on the first match (the last when going backwards). `None`
    /// when nothing matches.
    pub fn start(split: &Split, names: &[String], backwards: bool) -> Option<Cycle> {
        let matches = matches(names, &split.prefix);
        if matches.is_empty() {
            return None;
        }
        let index = if backwards { matches.len() - 1 } else { 0 };
        Some(Cycle {
            base: split.base.clone(),
            matches,
            index,
        })
    }

    /// The next match, or the previous one, wrapping at either end.
    pub fn step(&mut self, forward: bool) {
        let n = self.matches.len();
        self.index = if forward {
            (self.index + 1) % n
        } else {
            (self.index + n - 1) % n
        };
    }

    /// The field's text for the match shown: the base, the name and a `/`.
    pub fn text(&self) -> String {
        format!("{}{}/", self.base, self.matches[self.index])
    }

    /// The hint under the field, fitted to `width` columns: `2 of 3:
    /// Desktop, [Documents], Downloads`, the matches in their sorted order
    /// with the one shown in brackets. When they do not all fit, the
    /// list starts late enough (after `…,`) that the one shown is whole.
    pub fn hint(&self, width: usize) -> String {
        let n = self.matches.len();
        let names: Vec<String> = self
            .matches
            .iter()
            .enumerate()
            .map(|(i, m)| {
                if i == self.index {
                    format!("[{m}]")
                } else {
                    m.clone()
                }
            })
            .collect();
        let head = format!("{} of {n}: ", self.index + 1);
        let current = &names[self.index];
        for start in 0..=self.index {
            let lead = if start > 0 { "…, " } else { "" };
            let text = format!("{head}{lead}{}", names[start..].join(", "));
            let fitted = ui::fit_text(&text, width);
            if fitted.contains(current.as_str()) || start == self.index {
                return fitted;
            }
        }
        ui::fit_text(&head, width)
    }
}

/// The hint when nothing matches.
pub const NO_MATCH: &str = "no matching folder";
/// The hint while the OneDrive names are being listed.
pub const LISTING: &str = "listing…";

/// The names of the folders directly in `dir`, hidden ones included (a
/// symbolic link to a folder counts as one). Names that are not valid
/// UTF-8 are left out, since they could not be typed.
pub fn local_folders(dir: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let folder = kind.is_dir()
            || (kind.is_symlink() && std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()));
        if folder {
            if let Ok(name) = entry.file_name().into_string() {
                out.push(name);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn values_are_split_at_the_last_slash() {
        let s = split("~/Doc", true);
        assert_eq!((s.base.as_str(), s.prefix.as_str()), ("~/", "Doc"));
        let s = split("/Volumes/Backup/", true);
        assert_eq!(
            (s.base.as_str(), s.prefix.as_str()),
            ("/Volumes/Backup/", "")
        );
        // An empty local field, or just ~, completes in the home folder.
        assert_eq!(split("", true).base, "~/");
        assert_eq!(split("~", true).base, "~/");
        // OneDrive: relative to the drive root, empty is the root.
        let s = split("Backups/Doc", false);
        assert_eq!((s.base.as_str(), s.prefix.as_str()), ("Backups/", "Doc"));
        assert_eq!(split("", false).base, "");
        assert_eq!(split("Ba", false).prefix, "Ba");
        assert_eq!(remote_parent("/Backups/"), "Backups");
        assert_eq!(remote_parent(""), "");
    }

    #[test]
    fn the_parent_is_expanded_and_must_be_absolute() {
        let home = config::expand_tilde("~");
        assert_eq!(local_parent("~/"), Some(PathBuf::from(format!("{home}/"))));
        assert_eq!(local_parent("/Volumes/"), Some(PathBuf::from("/Volumes/")));
        assert_eq!(local_parent("relative/"), None);
        assert_eq!(local_parent(""), None);
    }

    #[test]
    fn prefixes_match_ignoring_case_and_hidden_folders_need_a_dot() {
        let list = names(&[
            "Downloads",
            "documents",
            "Desktop",
            ".dotfiles",
            "Music",
            "Docs",
        ]);
        assert_eq!(
            matches(&list, "d"),
            names(&["Desktop", "Docs", "documents", "Downloads"])
        );
        assert_eq!(matches(&list, "DOC"), names(&["Docs", "documents"]));
        assert_eq!(matches(&list, "x"), Vec::<String>::new());
        // Hidden folders only when the typed segment starts with a dot.
        assert!(!matches(&list, "").contains(&".dotfiles".to_string()));
        assert_eq!(matches(&list, "."), names(&[".dotfiles"]));
        // A decomposed é on disk matches a typed composed one.
        let list = names(&["Re\u{301}sume\u{301}s"]);
        assert_eq!(matches(&list, "Ré").len(), 1);
    }

    #[test]
    fn tab_cycles_forward_and_back_and_wraps() {
        let list = names(&["Downloads", "Documents", "Desktop", "Music"]);
        let s = split("~/D", true);
        let mut c = Cycle::start(&s, &list, false).unwrap();
        assert_eq!(c.text(), "~/Desktop/", "the tilde is kept");
        c.step(true);
        assert_eq!(c.text(), "~/Documents/");
        c.step(true);
        c.step(true);
        assert_eq!(c.text(), "~/Desktop/", "wraps forward");
        c.step(false);
        assert_eq!(c.text(), "~/Downloads/", "wraps back");
        // Shift-Tab first starts on the last match.
        let c = Cycle::start(&s, &list, true).unwrap();
        assert_eq!(c.text(), "~/Downloads/");
        assert_eq!(Cycle::start(&split("~/Z", true), &list, false), None);
    }

    #[test]
    fn a_trailing_slash_lists_the_children() {
        let list = names(&["b", "a"]);
        let c = Cycle::start(&split("/Volumes/Backup/", true), &list, false).unwrap();
        assert_eq!(c.text(), "/Volumes/Backup/a/");
        let c = Cycle::start(&split("Backups/", false), &list, false).unwrap();
        assert_eq!(c.text(), "Backups/a/");
    }

    #[test]
    fn the_hint_lists_the_matches_in_order_and_marks_the_one_shown() {
        let list = names(&[
            "Desktop",
            "Documents",
            "Downloads",
            "Drafts",
            "Dropbox",
            "Dev",
            "Data",
        ]);
        let mut c = Cycle::start(&split("~/D", true), &list, false).unwrap();
        c.step(true);
        // Sorted: Data, Desktop, Dev, Documents, Downloads, Drafts, Dropbox.
        assert_eq!(c.matches[c.index], "Desktop");
        assert_eq!(
            c.hint(200),
            "2 of 7: Data, [Desktop], Dev, Documents, Downloads, Drafts, Dropbox"
        );
        let short = c.hint(30);
        assert!(ui::width(&short) <= 30, "{short}");
        assert!(
            short.starts_with("2 of 7: Data, [Desktop]") && short.ends_with('…'),
            "{short}"
        );
        // Further on, the list starts late enough to show the current one.
        for _ in 0..4 {
            c.step(true);
        }
        let short = c.hint(30);
        assert!(ui::width(&short) <= 30, "{short}");
        assert!(
            short.starts_with("6 of 7: …, ") && short.contains("[Drafts]"),
            "{short}"
        );
    }

    #[test]
    fn local_folders_lists_folders_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Alpha")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("file.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("Alpha"), dir.path().join("link")).unwrap();
        let mut found = local_folders(dir.path()).unwrap();
        found.sort();
        let mut want = vec![".hidden", "Alpha"];
        if cfg!(unix) {
            want.push("link");
        }
        assert_eq!(found, want);
    }
}
