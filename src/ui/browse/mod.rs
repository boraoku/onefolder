//! The folder browser, opened from a folder field of the job form (`b`,
//! or Ctrl-B while typing): a list of the folders in one folder, to drill
//! into and choose from, with each folder's size measured in the
//! background and shown as it grows, after Mole's disk browser.
//!
//! [`Browser`] is the screen's state: the folder shown, its subfolders,
//! their sizes, the cursor, the `/` filter and the hidden-folders toggle.
//! It never blocks. Opening a folder hands the work to a [`Source`]
//! ([`local::LocalSource`] for this Mac, [`remote::RemoteSource`] for a
//! OneDrive account), which answers with [`Update`]s on a channel; the
//! job form's key loop drains them ([`Browser::tick`]) about ten times a
//! second and redraws. Every update carries the generation of the folder
//! it is for, so the answers about a folder the user has already left are
//! dropped, and a source cancels the work of the folder before when it is
//! asked for the next.

pub mod local;
pub mod remote;

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::config;
use crate::sync::nfc;
use crate::ui::prompt::thousands;
use crate::ui::summary::format_bytes;
use crate::ui::theme::{self, Tone};
use crate::ui::{self, Segment};

/// Sizes measured so far in this job form, keyed by local path or
/// `remote:path`: bytes, and whether some of it could not be read.
pub type SizeCache = Arc<Mutex<HashMap<String, (u64, bool)>>>;

/// A new, empty [`SizeCache`].
pub fn new_cache() -> SizeCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// What a folder holds, besides its subfolders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// The names of the subfolders, in any order.
    pub dirs: Vec<String>,
    /// Files directly in the folder, and their bytes.
    pub files: u64,
    pub file_bytes: u64,
    /// Free space on the drive, when the listing could tell.
    pub free: Option<u64>,
}

/// Why a folder could not be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListFailure {
    /// The folder does not exist (rclone's exit code 3).
    pub missing: bool,
    pub message: String,
}

/// A folder's size as far as it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// Still being measured: the bytes counted so far.
    Measuring {
        bytes: u64,
        partial: bool,
    },
    /// Measured; `partial` when something in it could not be read.
    Done {
        bytes: u64,
        partial: bool,
    },
    /// A drive mounted here: the space used on it, from `statvfs`, in
    /// place of walking it.
    Volume {
        used: u64,
    },
    Failed,
}

impl Size {
    fn bytes(self) -> Option<u64> {
        match self {
            Size::Measuring { bytes, .. } | Size::Done { bytes, .. } => Some(bytes),
            Size::Volume { used } => Some(used),
            Size::Failed => None,
        }
    }
}

/// An answer from a [`Source`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Listed {
        generation: u64,
        result: Result<Listing, ListFailure>,
    },
    Size {
        generation: u64,
        name: String,
        size: Size,
    },
    /// Free space on the drive (OneDrive's, from `rclone about`).
    Free(u64),
}

/// Lists folders and measures their subfolders in the background.
pub trait Source: Send {
    /// Lists `folder` and measures each subfolder, answering with
    /// updates tagged `generation`. Cancels the work for any folder
    /// opened before.
    fn open(&mut self, folder: &str, generation: u64);
    /// Forgets the cached sizes of the subfolders of `folder`, so the
    /// next [`Source::open`] measures them again.
    fn forget(&mut self, folder: &str);
}

/// Where the browser browses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// This Mac: folders are absolute paths.
    Local,
    /// A OneDrive account: folders are relative to the drive root, empty
    /// for the root. `label` is the account's name, as the form shows it.
    Remote { label: String },
}

/// How the browser ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The folder chosen, as the field should hold it: tilde form on this
    /// Mac, relative to the drive root on OneDrive.
    Chosen(String),
    /// Esc: back to the form, nothing changed.
    Back,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Load {
    Listing,
    Ready,
    Failed(String),
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The folder browser's state.
pub struct Browser {
    place: Place,
    /// The folder shown: an absolute path, or the remote-relative path.
    folder: String,
    source: Option<Box<dyn Source>>,
    updates: Option<Receiver<Update>>,
    generation: u64,
    load: Load,
    /// Every subfolder, sorted by name, hidden ones included.
    dirs: Vec<String>,
    sizes: HashMap<String, Size>,
    files: (u64, u64),
    free: Option<u64>,
    hidden: bool,
    filter: String,
    filtering: bool,
    /// Indices into `dirs` of the rows shown (hidden toggle and filter).
    shown: Vec<usize>,
    cursor: usize,
    scroll: std::cell::Cell<usize>,
    height: std::cell::Cell<usize>,
    /// Going up from a folder that does not exist until one does (the
    /// OneDrive starting folder, which cannot be checked beforehand).
    seeking: bool,
    /// The folder just left by going up, for the cursor to land on.
    came_from: Option<String>,
    tick: usize,
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser")
            .field("place", &self.place)
            .field("folder", &self.folder)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// The local folder the browser starts in for a field holding `value`:
/// the folder itself when it exists, else its nearest existing ancestor,
/// else the home folder.
pub fn local_start(value: &str) -> String {
    let home = config::expand_tilde("~");
    let typed = value.trim();
    if typed.is_empty() {
        return home;
    }
    if !Path::new(&config::expand_tilde(typed)).is_absolute() {
        return home;
    }
    config::absolute(typed)
        .and_then(|p| {
            p.ancestors()
                .find(|a| a.is_dir())
                .map(|a| a.display().to_string())
        })
        .unwrap_or(home)
}

/// `folder` and `name` joined for the place: `/a` + `b` is `/a/b`, `/` +
/// `b` is `/b`; on OneDrive the root is empty.
fn join(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.to_string()
    } else if folder.ends_with('/') {
        format!("{folder}{name}")
    } else {
        format!("{folder}/{name}")
    }
}

/// The folder above `folder`, and the name of `folder` in it. `None` at
/// the top (`/`, or the OneDrive root).
fn parent(folder: &str, local: bool) -> Option<(String, String)> {
    if local {
        let path = Path::new(folder);
        let up = path.parent()?;
        let name = path.file_name()?.to_string_lossy().into_owned();
        Some((up.display().to_string(), name))
    } else if folder.is_empty() {
        None
    } else {
        Some(match folder.rsplit_once('/') {
            Some((up, name)) => (up.to_string(), name.to_string()),
            None => (String::new(), folder.to_string()),
        })
    }
}

fn fold(name: &str) -> String {
    nfc(name).to_lowercase()
}

impl Browser {
    /// A browser over `place` starting at `start` (see [`local_start`];
    /// on OneDrive the field's folder, which is left upwards until one
    /// exists). Opens it at once through `source`; with no source the
    /// updates are applied by the caller ([`Browser::apply`]), as tests do.
    pub fn new(
        place: Place,
        start: &str,
        source: Option<Box<dyn Source>>,
        updates: Option<Receiver<Update>>,
    ) -> Browser {
        let seeking = matches!(place, Place::Remote { .. });
        let mut b = Browser {
            place,
            folder: String::new(),
            source,
            updates,
            generation: 0,
            load: Load::Listing,
            dirs: Vec::new(),
            sizes: HashMap::new(),
            files: (0, 0),
            free: None,
            hidden: false,
            filter: String::new(),
            filtering: false,
            shown: Vec::new(),
            cursor: 0,
            scroll: std::cell::Cell::new(0),
            height: std::cell::Cell::new(10),
            seeking,
            came_from: None,
            tick: 0,
        };
        b.open(start.to_string());
        b.seeking = seeking;
        b
    }

    fn is_local(&self) -> bool {
        self.place == Place::Local
    }

    /// The folder shown.
    #[cfg(test)]
    pub fn folder(&self) -> &str {
        &self.folder
    }

    /// The generation of the folder shown; updates for any other are
    /// dropped.
    #[cfg(test)]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Shows `folder`: everything known about the one before is dropped
    /// and its work cancelled by the source.
    fn open(&mut self, folder: String) {
        self.generation += 1;
        self.folder = folder;
        self.load = Load::Listing;
        self.dirs.clear();
        self.sizes.clear();
        self.files = (0, 0);
        self.filter.clear();
        self.filtering = false;
        self.shown.clear();
        self.cursor = 0;
        self.scroll.set(0);
        self.seeking = false;
        if let Some(source) = &mut self.source {
            source.open(&self.folder, self.generation);
        }
    }

    /// Takes in the answers that have arrived, without waiting. True when
    /// anything changed on screen (the spinner turns while listing).
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        let mut got = Vec::new();
        if let Some(rx) = &self.updates {
            while let Ok(update) = rx.try_recv() {
                got.push(update);
            }
        }
        for update in got {
            changed |= self.apply(update);
        }
        if self.load == Load::Listing {
            self.tick += 1;
            changed = true;
        }
        changed
    }

    /// Applies one answer. Answers about a folder no longer shown, and a
    /// running total arriving after the final size, are dropped. True when
    /// it changed anything.
    pub fn apply(&mut self, update: Update) -> bool {
        match update {
            Update::Free(bytes) => {
                self.free = Some(bytes);
                true
            }
            Update::Listed { generation, .. } | Update::Size { generation, .. }
                if generation != self.generation =>
            {
                false
            }
            Update::Listed { result, .. } => {
                match result {
                    Ok(listing) => {
                        let mut dirs = listing.dirs;
                        dirs.sort_by_cached_key(|d| (fold(d), d.clone()));
                        dirs.dedup();
                        self.dirs = dirs;
                        self.files = (listing.files, listing.file_bytes);
                        if listing.free.is_some() {
                            self.free = listing.free;
                        }
                        self.load = Load::Ready;
                        self.seeking = false;
                        self.refilter();
                        if let Some(name) = self.came_from.take() {
                            if let Some(at) = self.shown.iter().position(|&i| self.dirs[i] == name)
                            {
                                self.cursor = at;
                            }
                        }
                    }
                    Err(failure) if failure.missing && self.seeking => {
                        // The starting folder is not there: try the one above.
                        match parent(&self.folder, self.is_local()) {
                            Some((up, _)) => {
                                self.open(up);
                                self.seeking = true;
                            }
                            None => self.load = Load::Failed(failure.message),
                        }
                    }
                    Err(failure) => {
                        self.seeking = false;
                        self.load = Load::Failed(failure.message);
                    }
                }
                true
            }
            Update::Size { name, size, .. } => {
                let old = self.sizes.get(&name).copied();
                if matches!(old, Some(Size::Done { .. })) && matches!(size, Size::Measuring { .. })
                {
                    return false;
                }
                if old == Some(size) {
                    return false;
                }
                self.sizes.insert(name, size);
                true
            }
        }
    }

    /// Recomputes the rows shown from the hidden toggle and the filter,
    /// keeping the cursor on the same folder where it can.
    fn refilter(&mut self) {
        let current = self.shown.get(self.cursor).copied();
        let want = fold(&self.filter);
        self.shown = self
            .dirs
            .iter()
            .enumerate()
            .filter(|(_, d)| self.hidden || !d.starts_with('.'))
            .filter(|(_, d)| want.is_empty() || fold(d).contains(&want))
            .map(|(i, _)| i)
            .collect();
        self.cursor = current
            .and_then(|c| self.shown.iter().position(|&i| i == c))
            .unwrap_or(0)
            .min(self.shown.len().saturating_sub(1));
    }

    /// The folder under the cursor.
    pub fn highlighted(&self) -> Option<&str> {
        self.shown.get(self.cursor).map(|&i| self.dirs[i].as_str())
    }

    /// `folder` as the field holds it.
    fn value(&self, folder: &str) -> String {
        if self.is_local() {
            config::tilde(Path::new(folder))
        } else {
            folder.to_string()
        }
    }

    fn page(&self) -> usize {
        self.height.get().max(1)
    }

    /// Applies one key. Returns the outcome once the browser is done.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        // Keys with Ctrl or Alt mean nothing here, except Ctrl-H, which is
        // Backspace on some terminals. (Ctrl-C never gets here: the form's
        // loop quits on it first.)
        let key = if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('h') {
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            } else {
                return None;
            }
        } else {
            key
        };
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.refilter();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refilter();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.refilter();
                }
                _ => {}
            }
            return None;
        }
        let last = self.shown.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(last),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(self.page()),
            KeyCode::PageDown => self.cursor = (self.cursor + self.page()).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = last,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(name) = self.highlighted() {
                    let next = join(&self.folder, name);
                    self.came_from = None;
                    self.open(next);
                }
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                if let Some((up, name)) = parent(&self.folder, self.is_local()) {
                    self.came_from = Some(name);
                    self.open(up);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(name) = self.highlighted() {
                    return Some(Outcome::Chosen(self.value(&join(&self.folder, name))));
                }
            }
            KeyCode::Char('c') => return Some(Outcome::Chosen(self.value(&self.folder))),
            KeyCode::Char('~') if self.is_local() => {
                self.came_from = None;
                self.open(config::expand_tilde("~"));
            }
            KeyCode::Char('v') if self.is_local() => {
                self.came_from = None;
                self.open(crate::platform::drives_folder());
            }
            KeyCode::Char('.') => {
                self.hidden = !self.hidden;
                self.refilter();
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('r') => {
                if let Some(source) = &mut self.source {
                    source.forget(&self.folder);
                }
                self.came_from = self.highlighted().map(str::to_string);
                let folder = self.folder.clone();
                self.open(folder);
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Esc => return Some(Outcome::Back),
            _ => {}
        }
        None
    }

    /// The key hints, fitted to `width` columns.
    pub fn hints(&self, width: usize) -> String {
        let d = theme::DOT;
        let text = if self.filtering {
            format!("type to filter {d} enter keep {d} esc clear")
        } else {
            let esc = if self.filter.is_empty() {
                "esc back"
            } else {
                "esc clear"
            };
            format!(
                "enter open {d} h up {d} space choose {d} c here {d} . hidden {d} / filter {d} {esc}"
            )
        };
        ui::fit_text(&text, width)
    }

    /// The folder shown, as the user knows it: tilde form on this Mac,
    /// `account:path` on OneDrive.
    fn folder_text(&self) -> String {
        match &self.place {
            Place::Local => config::tilde(Path::new(&self.folder)),
            Place::Remote { label } if self.folder.is_empty() => format!("{label}: (drive root)"),
            Place::Remote { label } => format!("{label}:{}", self.folder),
        }
    }

    /// The screen's lines for an area `width` by `height`, the key hints
    /// on the last.
    pub fn lines(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        // The title, with the free space on the right.
        let title = "Choose a folder";
        let mut head = vec![Span::styled(title, theme::style(Tone::Bold))];
        if let Some(free) = self.free {
            let free = format!("{} free", format_bytes(free));
            let gap = width.saturating_sub(ui::width(title) + ui::width(&free));
            if gap >= 2 {
                head.push(Span::raw(" ".repeat(gap)));
                head.push(Span::styled(free, theme::style(Tone::Dim)));
            }
        }
        lines.push(Line::from(head));
        // The folder, with the filter and the hidden toggle.
        let mut extra: Vec<Segment> = Vec::new();
        if self.hidden {
            extra.push((format!(" {} hidden folders shown", theme::DOT), Tone::Dim));
        }
        if self.filtering || !self.filter.is_empty() {
            let cursor = if self.filtering { "▏" } else { "" };
            extra.push((
                format!(" {} /{}{cursor}", theme::DOT, self.filter),
                Tone::Accent,
            ));
        }
        let room = width.saturating_sub(extra.iter().map(|(t, _)| ui::width(t)).sum());
        let mut place = vec![Span::styled(
            ui::fit_text(&self.folder_text(), room.max(8)),
            theme::style(Tone::Dim),
        )];
        place.extend(
            extra
                .into_iter()
                .map(|(t, tone)| Span::styled(t, theme::style(tone))),
        );
        lines.push(Line::from(place));
        lines.push(Line::default());

        // Title, folder, blank; blank, files, hints.
        let rows = height.saturating_sub(6).max(1);
        self.height.set(rows);
        match &self.load {
            Load::Listing => {
                let spin = SPINNER[self.tick % SPINNER.len()];
                lines.push(Line::from(vec![
                    Span::styled(format!("  {spin} "), theme::style(Tone::Accent)),
                    Span::styled("listing…", theme::style(Tone::Dim)),
                ]));
            }
            Load::Failed(message) => {
                let text = format!("  {} could not list: {message}", theme::FAILED);
                lines.push(Line::from(Span::styled(
                    ui::fit_text(&text, width),
                    theme::style(Tone::Error),
                )));
            }
            Load::Ready if self.shown.is_empty() => {
                let hidden = self.dirs.iter().filter(|d| d.starts_with('.')).count();
                let text = if !self.filter.is_empty() {
                    format!(
                        "  {} no folder matches {} esc clears the filter",
                        theme::SKIPPED,
                        theme::DOT
                    )
                } else if hidden > 0 && !self.hidden {
                    format!(
                        "  no folders here {} {hidden} hidden, . shows them",
                        theme::DOT
                    )
                } else {
                    "  no folders here".to_string()
                };
                lines.push(Line::from(Span::styled(text, theme::style(Tone::Dim))));
            }
            Load::Ready => {
                let mut scroll = self.scroll.get();
                if self.cursor < scroll {
                    scroll = self.cursor;
                } else if self.cursor >= scroll + rows {
                    scroll = self.cursor + 1 - rows;
                }
                scroll = scroll.min(self.shown.len().saturating_sub(rows));
                self.scroll.set(scroll);
                // Bars are relative to the largest size among the rows
                // shown (so a filter rescales them), running totals
                // included. Drives mounted here are scaled among
                // themselves, so a 2 TB disk does not flatten every
                // folder's bar.
                let largest = |volumes: bool| {
                    self.shown
                        .iter()
                        .filter_map(|&i| self.sizes.get(&self.dirs[i]))
                        .filter(|s| matches!(s, Size::Volume { .. }) == volumes)
                        .filter_map(|s| s.bytes())
                        .max()
                        .unwrap_or(0)
                };
                let largest = (largest(false), largest(true));
                for (n, &i) in self.shown.iter().enumerate().skip(scroll).take(rows) {
                    lines.push(self.row(i, n == self.cursor, largest, width));
                }
            }
        }
        while lines.len() < 3 + rows {
            lines.push(Line::default());
        }
        lines.push(Line::default());
        if self.load == Load::Ready && self.files.0 > 0 {
            let (count, bytes) = self.files;
            let noun = if count == 1 { "file" } else { "files" };
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} {noun} here, {}",
                    thousands(count),
                    format_bytes(bytes)
                ),
                theme::style(Tone::Dim),
            )));
        } else {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            self.hints(width),
            theme::style(Tone::Dim),
        )));
        lines
    }

    /// One folder's row: cursor, bar, name, size.
    fn row(&self, i: usize, here: bool, largest: (u64, u64), width: usize) -> Line<'static> {
        const SIZE: usize = 14;
        let bar_w = (width / 6).clamp(6, 20);
        let name_w = width.saturating_sub(2 + bar_w + 2 + 2 + SIZE).max(4);
        let name = &self.dirs[i];
        let size = self.sizes.get(name).copied();
        let fraction = match size.and_then(Size::bytes) {
            Some(b) => {
                let scale = if matches!(size, Some(Size::Volume { .. })) {
                    largest.1
                } else {
                    largest.0
                };
                if scale > 0 {
                    b as f64 / scale as f64
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        let bar = theme::bar(fraction, bar_w);
        let full: String = bar.chars().take_while(|c| *c == theme::BAR_FULL).collect();
        let empty: String = bar.chars().skip_while(|c| *c == theme::BAR_FULL).collect();
        let used = matches!(size, Some(Size::Volume { .. }));
        let (number, mark, mark_tone) = match size {
            None => (String::new(), "…", Tone::Dim),
            Some(Size::Measuring { bytes, partial }) => (
                format_bytes(bytes),
                if partial { "+…" } else { "…" },
                Tone::Dim,
            ),
            Some(Size::Done { bytes, partial }) => (
                format_bytes(bytes),
                if partial { "+" } else { "" },
                Tone::Plain,
            ),
            Some(Size::Volume { used }) => (format_bytes(used), "", Tone::Plain),
            Some(Size::Failed) => (String::new(), "?", Tone::Dim),
        };
        let lead = if used { "used " } else { "" };
        let shown_w = ui::width(lead) + ui::width(&number) + ui::width(mark);
        let mut spans = vec![if here {
            Span::styled(format!("{} ", theme::CURSOR), theme::style(Tone::Accent))
        } else {
            Span::raw("  ")
        }];
        spans.push(Span::styled(full, theme::style(Tone::Accent)));
        spans.push(Span::styled(empty, theme::style(Tone::Dim)));
        spans.push(Span::raw("  "));
        let fitted = ui::fit_text(name, name_w);
        let tone = if here { Tone::Accent } else { Tone::Plain };
        spans.push(Span::styled(ui::pad(&fitted, name_w), theme::style(tone)));
        spans.push(Span::raw(" ".repeat(2 + SIZE.saturating_sub(shown_w))));
        spans.push(Span::styled(lead, theme::style(Tone::Dim)));
        spans.push(Span::raw(number));
        spans.push(Span::styled(mark.to_string(), theme::style(mark_tone)));
        Line::from(spans)
    }

    /// Draws the browser over the whole frame, inside the usual margin.
    pub fn render(&self, frame: &mut Frame) {
        let area: Rect = frame.area().inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        if area.height < 3 || area.width < 10 {
            return;
        }
        let lines = self.lines(usize::from(area.width), usize::from(area.height));
        let (body, hints) = lines.split_at(lines.len() - 1);
        frame.render_widget(
            Paragraph::new(body.to_vec()),
            Rect {
                height: area.height - 1,
                ..area
            },
        );
        frame.render_widget(
            Paragraph::new(hints.to_vec()),
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
        );
    }
}

#[cfg(test)]
mod tests;
