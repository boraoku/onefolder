//! The job add and edit form, with typed destinations.
//!
//! The main form holds the name, the source, the excludes, whether hidden
//! files are copied, and the list of destinations; adding or editing a destination opens a second form
//! (OneDrive: account, folder, mode; local: folder, mode). When a local
//! destination is kept, the volume that holds it is looked up through the
//! `capture` callback the caller passes in (the CLI runs
//! `volume::identify`), and what was recorded is shown on the form.
//!
//! Looking a volume up runs diskutil, which can take a moment, so a key
//! that needs it only marks the work as pending (and the form says
//! "Checking drive..."); the event loop draws that, then calls
//! [`JobForm::resolve`].
//!
//! The folder fields (the source, a local destination's folder and a
//! OneDrive folder) complete with Tab and open the folder browser
//! ([`browse`]) with `b`. The OneDrive names for completion, and the
//! browser's listings and sizes, come from work on the tokio runtime and
//! on threads; the event loop never waits for them, and takes in what has
//! arrived ([`JobForm::tick`]) about ten times a second.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use anyhow::{Context as _, Result};
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use ratatui::Frame;
use tokio::runtime::Handle;

use crate::config;
use crate::model::{self, generate_destination_id, Account, Destination, Job, Mode};
use crate::rclone;
use crate::rclone::detect::Rclone;
use crate::rclone::listing::{self, ListError};
use crate::ui::browse::local::LocalSource;
use crate::ui::browse::remote::{self as remote_source, RemoteSource};
use crate::ui::browse::{self, Browser, Place, SizeCache};
use crate::ui::form::{ButtonAction, Field, Folder, Form, FormEvent, RowAction, Segment};
use crate::ui::theme::{self, Tone};
use crate::volume::{self, Found, Identity, VolumeError};

/// What the form needs to know about the rest of the config.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub accounts: Vec<Account>,
    /// Names of the other jobs, which this one may not take.
    pub taken: Vec<String>,
    /// The job being edited; `None` when adding.
    pub existing: Option<Job>,
    /// A name to start with when adding.
    pub name: Option<String>,
    /// rclone, for listing OneDrive folders (completion and browsing).
    pub rclone: Option<Rclone>,
}

/// Looks up the volume that holds a path.
pub type Capture<'a> = dyn FnMut(&Path) -> Result<Found, VolumeError> + 'a;

/// A note for the user, with its tone.
pub type Note = (String, Tone);

/// How the form ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The job to save, and notes to show the user (volume identities).
    Saved {
        job: Box<Job>,
        notes: Vec<Note>,
    },
    Cancelled,
}

const TYPES: [&str; 2] = ["OneDrive", "Local folder or drive"];
/// The "Copy hidden files" choices: No (the default) and Yes.
const HIDDEN: [&str; 2] = ["No", "Yes"];
const ONEDRIVE: usize = 0;
const LOCAL: usize = 1;
const CHECKING: &str = "Checking drive...";

fn name_rule(name: &str) -> Result<(), String> {
    if model::is_valid_name(name.trim()) {
        Ok(())
    } else if name.trim().is_empty() {
        Err("give the job a name, such as documents".into())
    } else {
        Err("use lowercase letters, digits and dashes, starting with a letter or digit".into())
    }
}

fn required(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err("needs a value".into())
    } else {
        Ok(())
    }
}

/// The open destination form.
#[derive(Debug, Clone)]
struct DestForm {
    form: Form,
    /// Index in the destination list; `None` for a new one.
    index: Option<usize>,
}

/// Work that needs the drive looked up, done on the next [`JobForm::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// Keep the open destination; `rerecord` looks the drive up again even
    /// when the folder did not change.
    Destination {
        rerecord: bool,
    },
    Finish,
}

/// OneDrive folder names for completion, as listed in the background:
/// the account's remote, the folder, and the names or why not.
type Names = (String, String, Result<Vec<String>, String>);

/// The whole job form.
#[derive(Debug)]
pub struct JobForm {
    ctx: Context,
    form: Form,
    dests: Vec<Destination>,
    sub: Option<DestForm>,
    /// Notes so far, keyed by destination id (`None` for the source), so a
    /// destination removed later in the form takes its notes with it.
    notes: Vec<(Option<String>, Note)>,
    pending: Option<Pending>,
    /// The destination whose recorded drive the user asked to look up
    /// again and which turned out to be on the startup disk: a second
    /// "Re-record drive" on it confirms forgetting the drive.
    forget_armed: Option<String>,
    /// The warning about names too long for bisync was shown for this
    /// job: saving again saves it anyway.
    long_names_warned: Option<Job>,
    /// The folder browser, when open, and the field it fills.
    browser: Option<(Browser, &'static str)>,
    /// Folder sizes measured while this form is open.
    sizes: SizeCache,
    /// The runtime the OneDrive listings run on (the CLI's).
    runtime: Option<Handle>,
    /// OneDrive folder names listed so far, by `remote:folder`.
    names: HashMap<String, Vec<String>>,
    names_tx: Sender<Names>,
    names_rx: Receiver<Names>,
}

/// One row of the destination list in the form.
pub fn destination_row(d: &Destination) -> Vec<Segment> {
    let mut row = vec![
        (
            format!("{} {:<30}", theme::ACTION, d.display()),
            Tone::Plain,
        ),
        (format!("  {:<9}", d.mode().as_str()), Tone::Dim),
    ];
    if let Destination::Local { .. } = d {
        match d.identity() {
            Some(id) if id.is_weak() => row.push((
                format!("{} no UUID, weaker drive check", theme::SKIPPED),
                Tone::Warning,
            )),
            Some(id) => row.push((format!("volume \"{}\"", id.volume_name), Tone::Dim)),
            None => {}
        }
    }
    row
}

/// What looking up the volume of a path comes to: the identity to record
/// (`None` on the startup disk, which is never recorded) and notes for the
/// user. A failure is refused for a path under `/Volumes`, where the drive
/// must be proven; elsewhere it is a warning and nothing is recorded.
pub fn record(
    path: &Path,
    result: Result<Found, VolumeError>,
    destination: bool,
) -> Result<(Option<Identity>, Vec<Note>), VolumeError> {
    let shown = path.display().to_string();
    let mut notes = Vec::new();
    match result {
        Ok(found) => {
            match &found.identity {
                Some(id) => notes.push(identity_note(&shown, id)),
                None if destination => notes.push((
                    format!(
                        "{} {shown}: {}, so there is no drive to record.",
                        theme::DONE,
                        crate::platform::system_disk_note(&found.mount_point)
                    ),
                    Tone::Plain,
                )),
                None => {}
            }
            if destination && found.read_only {
                notes.push((
                    format!(
                        "{} {shown}: {} is mounted read-only, so OneFolder cannot write to it until it is writable.",
                        theme::SKIPPED,
                        found.mount_point
                    ),
                    Tone::Warning,
                ));
            }
            Ok((found.identity, notes))
        }
        // A path that does not answer counts as leading onto a drive.
        Err(e)
            if {
                let owned = path.to_path_buf();
                volume::guarded_blocking(path, volume::DEADLINE, move || {
                    volume::leads_onto_drive(&owned)
                })
                .unwrap_or(true)
            } =>
        {
            Err(e)
        }
        Err(e) => {
            notes.push((
                format!(
                    "{} {shown}: no drive recorded ({}).",
                    theme::SKIPPED,
                    e.headline()
                ),
                Tone::Warning,
            ));
            Ok((None, notes))
        }
    }
}

impl JobForm {
    pub fn new(ctx: Context) -> JobForm {
        let job = ctx.existing.clone().unwrap_or_default();
        let adding = ctx.existing.is_none();
        let name = if adding {
            ctx.name.clone().unwrap_or_default()
        } else {
            job.name.clone()
        };
        let source = if job.source.is_empty() {
            String::new()
        } else {
            config::tilde(Path::new(&job.source))
        };
        let mut name_field = Field::text("name", "Name", &name)
            .with_placeholder("documents")
            .with_validator(name_rule);
        name_field = if adding {
            name_field.with_help("Used on the command line: onefolder run <name>")
        } else {
            name_field.with_help("Renaming takes its run logs and sync state with it")
        };
        let fields = vec![
            name_field,
            Field::path("source", "Source", &source, false)
                .with_placeholder("~/Documents")
                .with_validator(required)
                .with_help(&format!(
                    "The folder on {} to sync",
                    crate::platform::words().this_computer
                )),
            Field::list(
                "excludes",
                "Excludes",
                job.excludes.clone(),
                "add a pattern",
            )
            .with_help("A folder, its path or folder/* leaves out all of it"),
            Field::picker(
                "hidden",
                "Copy hidden files",
                HIDDEN.iter().map(|s| s.to_string()).collect(),
                usize::from(job.copy_hidden),
            )
            .with_help("Files and folders whose names start with a dot, such as .git or .env"),
            Field::rows(
                "dests",
                "Destinations",
                job.destinations.iter().map(destination_row).collect(),
                "add a destination",
            )
            .with_help("enter to open the list, then a add, d remove, J/K reorder"),
            Field::button(
                "save",
                if adding { "Add job" } else { "Save job" },
                ButtonAction::Submit,
            ),
            Field::button("cancel", "Cancel", ButtonAction::Cancel),
        ];
        let title = if adding {
            "Add job".to_string()
        } else {
            format!("Edit {}", job.name)
        };
        let form = Form::new(&title, fields);
        let (names_tx, names_rx) = mpsc::channel();
        JobForm {
            dests: job.destinations.clone(),
            ctx,
            form,
            sub: None,
            notes: Vec::new(),
            pending: None,
            forget_armed: None,
            long_names_warned: None,
            browser: None,
            sizes: browse::new_cache(),
            runtime: Handle::try_current().ok(),
            names: HashMap::new(),
            names_tx,
            names_rx,
        }
    }

    /// Draws the folder browser when it is open, otherwise the form.
    pub fn render(&self, frame: &mut Frame) {
        match &self.browser {
            Some((browser, _)) => browser.render(frame),
            None => self.current().render(frame),
        }
    }

    /// True while the folder browser is open.
    #[cfg(test)]
    pub fn browsing(&self) -> Option<&Browser> {
        self.browser.as_ref().map(|(b, _)| b)
    }

    /// Takes in what the background work has sent so far: OneDrive names
    /// for completion and the browser's listings and sizes. Never waits.
    /// True when the screen should be drawn again.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok((remote, folder, result)) = self.names_rx.try_recv() {
            if let Ok(names) = &result {
                let target = remote_source::target(&remote, &folder);
                self.names.insert(target, names.clone());
            }
            // Only for the account the destination form is on now.
            let account = self.sub_account().map(|a| a.remote.clone());
            if let (Some(sub), Some(account)) = (&mut self.sub, account) {
                if account == remote {
                    sub.form.set_listing(&folder, result);
                    changed = true;
                }
            }
        }
        if let Some((browser, _)) = &mut self.browser {
            changed |= browser.tick();
        }
        changed
    }

    /// The account picked in the open destination form.
    fn sub_account(&self) -> Option<&Account> {
        let sub = self.sub.as_ref()?;
        self.ctx.accounts.get(sub.form.picked("account"))
    }

    /// Lists the OneDrive folder the destination form asked for, in the
    /// background, unless it was listed before.
    fn list_remote(&mut self) {
        let account = self.sub_account().cloned();
        let Some(sub) = &mut self.sub else {
            return;
        };
        let Some(folder) = sub.form.take_list_request() else {
            return;
        };
        let Some(account) = account else {
            sub.form
                .set_listing(&folder, Err("add an account first".to_string()));
            return;
        };
        let target = remote_source::target(&account.remote, &folder);
        if let Some(names) = self.names.get(&target) {
            sub.form.set_listing(&folder, Ok(names.clone()));
            return;
        }
        let (Some(rclone), Some(runtime)) = (self.ctx.rclone.clone(), self.runtime.clone()) else {
            sub.form
                .set_listing(&folder, Err("rclone is not available".to_string()));
            return;
        };
        let tx = self.names_tx.clone();
        runtime.spawn(async move {
            let result = match listing::top_level(&rclone, &target, true).await {
                Ok(entries) => Ok(entries
                    .into_iter()
                    .filter(|e| e.is_dir)
                    .map(|e| e.path.rsplit('/').next().unwrap_or_default().to_string())
                    .collect()),
                // A folder that is not there has no folders to offer.
                Err(ListError::NotFound { .. }) => Ok(Vec::new()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send((account.remote, folder, result));
        });
    }

    /// Opens the folder browser for the folder field `key` of the form on
    /// screen, starting from what the field holds.
    fn open_browser(&mut self, key: &'static str) {
        let form = self.current();
        let value = form.text(key);
        let Some(folder) = form.field(key).and_then(|f| f.folder) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let browser = match folder {
            Folder::Local => {
                let source = LocalSource::new(tx, self.sizes.clone());
                Browser::new(
                    Place::Local,
                    &browse::local_start(&value),
                    Some(Box::new(source)),
                    Some(rx),
                )
            }
            Folder::Remote => {
                let Some(account) = self.sub_account().cloned() else {
                    self.current_mut().message = Some((
                        "Add a OneDrive account first: onefolder accounts add".into(),
                        Tone::Error,
                    ));
                    return;
                };
                let (Some(rclone), Some(runtime)) = (self.ctx.rclone.clone(), self.runtime.clone())
                else {
                    self.current_mut().message = Some((
                        "rclone is not available to browse OneDrive.".into(),
                        Tone::Error,
                    ));
                    return;
                };
                let source = RemoteSource::new(
                    runtime,
                    rclone,
                    account.remote.clone(),
                    tx,
                    self.sizes.clone(),
                );
                Browser::new(
                    Place::Remote {
                        label: account.label.clone(),
                    },
                    &clean_remote(&value),
                    Some(Box::new(source)),
                    Some(rx),
                )
            }
        };
        self.browser = Some((browser, key));
    }

    /// The form on screen: the destination form when one is open.
    pub fn current(&self) -> &Form {
        match &self.sub {
            Some(sub) => &sub.form,
            None => &self.form,
        }
    }

    fn current_mut(&mut self) -> &mut Form {
        match &mut self.sub {
            Some(sub) => &mut sub.form,
            None => &mut self.form,
        }
    }

    #[cfg(test)]
    pub fn destinations(&self) -> &[Destination] {
        &self.dests
    }

    /// True when a key asked for work that needs the drive looked up.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn refresh_rows(&mut self) {
        let rows = self.dests.iter().map(destination_row).collect();
        self.form.set_rows("dests", rows);
    }

    /// Marks work for [`JobForm::resolve`], saying "Checking drive..."
    /// when it will look a drive up.
    fn defer(&mut self, pending: Pending, checks_drive: bool) {
        self.pending = Some(pending);
        if checks_drive {
            self.current_mut().message = Some((CHECKING.to_string(), Tone::Dim));
        }
    }

    /// Applies one key. Returns the outcome once the form is done.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if self.pending.is_some() {
            return None;
        }
        if let Some((browser, field)) = &mut self.browser {
            let field = *field;
            match browser.handle_key(key) {
                Some(browse::Outcome::Chosen(value)) => {
                    // Closing the browser cancels its work.
                    self.browser = None;
                    self.current_mut().set_value(field, &value);
                }
                Some(browse::Outcome::Back) => self.browser = None,
                None => {}
            }
            return None;
        }
        if let Some(sub) = &mut self.sub {
            match sub.form.handle_key(key) {
                FormEvent::Changed("type") => {
                    let picked = sub.form.picked("type");
                    show_type(&mut sub.form, picked, &self.ctx.accounts);
                }
                FormEvent::Changed("account") => sub.form.clear_listings(),
                FormEvent::List(_) => self.list_remote(),
                FormEvent::Browse(field) => self.open_browser(field),
                FormEvent::Submit => {
                    let local = sub.form.picked("type") == LOCAL;
                    self.defer(Pending::Destination { rerecord: false }, local);
                }
                FormEvent::Pressed("rerecord") => {
                    self.defer(Pending::Destination { rerecord: true }, true);
                }
                FormEvent::Cancel => self.sub = None,
                _ => {}
            }
            return None;
        }
        match self.form.handle_key(key) {
            FormEvent::Row(_, RowAction::Add) => self.open_destination(None),
            FormEvent::Row(_, RowAction::Edit(i)) => self.open_destination(Some(i)),
            FormEvent::Row(_, RowAction::Remove(i)) if i < self.dests.len() => {
                let gone = self.dests.remove(i);
                self.notes
                    .retain(|(id, _)| id.as_deref() != Some(gone.id()));
                self.form.mark_dirty();
                self.refresh_rows();
                self.form.message = Some((
                    format!("Removed {}. Nothing on it is deleted.", gone.display()),
                    Tone::Dim,
                ));
            }
            FormEvent::Row(_, RowAction::MoveUp(i)) if i > 0 && i < self.dests.len() => {
                self.dests.swap(i, i - 1);
                self.form.mark_dirty();
                self.refresh_rows();
                self.form.set_row_cursor(i - 1);
            }
            FormEvent::Row(_, RowAction::MoveDown(i)) if i + 1 < self.dests.len() => {
                self.dests.swap(i, i + 1);
                self.form.mark_dirty();
                self.refresh_rows();
                self.form.set_row_cursor(i + 1);
            }
            FormEvent::Submit => {
                let source = local_path(&self.form.text("source")).ok();
                let changed = source.as_ref().is_some_and(|s| {
                    self.ctx
                        .existing
                        .as_ref()
                        .is_none_or(|j| Path::new(&j.source) != s)
                });
                self.defer(Pending::Finish, changed);
            }
            FormEvent::Browse(field) => self.open_browser(field),
            FormEvent::Cancel => return Some(Outcome::Cancelled),
            _ => {}
        }
        None
    }

    /// Does the pending work, looking drives up through `capture`.
    pub fn resolve(&mut self, capture: &mut Capture) -> Option<Outcome> {
        let pending = self.pending.take()?;
        if self
            .current()
            .message
            .as_ref()
            .is_some_and(|(m, _)| m == CHECKING)
        {
            self.current_mut().message = None;
        }
        match pending {
            Pending::Destination { rerecord } => {
                self.keep_destination(capture, rerecord);
                None
            }
            Pending::Finish => self.finish(capture),
        }
    }

    fn open_destination(&mut self, index: Option<usize>) {
        let existing = index.and_then(|i| self.dests.get(i));
        let accounts: Vec<String> = self.ctx.accounts.iter().map(|a| a.label.clone()).collect();
        let (kind, account, remote, path, mode) = match existing {
            Some(Destination::Onedrive {
                account,
                path,
                mode,
                ..
            }) => (
                ONEDRIVE,
                account.clone(),
                path.clone(),
                String::new(),
                *mode,
            ),
            Some(Destination::Local { path, mode, .. }) => (
                LOCAL,
                String::new(),
                String::new(),
                config::tilde(Path::new(path)),
                *mode,
            ),
            // A new destination starts as OneDrive when there is an account to use.
            None if accounts.is_empty() => (
                LOCAL,
                String::new(),
                String::new(),
                String::new(),
                Mode::OneWay,
            ),
            None => (
                ONEDRIVE,
                String::new(),
                String::new(),
                String::new(),
                Mode::OneWay,
            ),
        };
        let account_options = if accounts.is_empty() {
            vec!["none yet".to_string()]
        } else {
            accounts.clone()
        };
        let account_index = accounts.iter().position(|a| *a == account).unwrap_or(0);
        let mode_index = Mode::ALL.iter().position(|m| *m == mode).unwrap_or(0);
        let fields = vec![
            Field::picker("type", "Type", TYPES.iter().map(|s| s.to_string()).collect(), kind),
            Field::picker("account", "Account", account_options, account_index),
            Field::text("remote", "Folder", &remote)
                .with_folder(Folder::Remote)
                .with_placeholder("drive root")
                .with_help("A folder on the OneDrive account, such as Backups/Documents; empty for the whole drive"),
            Field::path("path", "Folder", &path, true)
                .with_placeholder(&crate::platform::placeholder_drive_path())
                .with_help("On an external drive, the drive is recorded so a run can tell it is plugged in"),
            Field::picker(
                "mode",
                "Mode",
                Mode::ALL.iter().map(|m| m.as_str().to_string()).collect(),
                mode_index,
            )
            .with_help("one-way copies new and changed files and never deletes; two-way syncs both ways"),
            Field::button("keep", "Keep destination", ButtonAction::Submit),
            Field::button("rerecord", "Re-record drive", ButtonAction::Press)
                .with_help("Looks the drive up again and records it, keeping this destination's id"),
            Field::button("back", "Back", ButtonAction::Cancel),
        ];
        let title = match index {
            Some(i) => format!("Destination {} of {}", i + 1, self.dests.len()),
            None => "New destination".to_string(),
        };
        let mut form = Form::new(&title, fields);
        let job_name = self.form.text("name");
        if !job_name.trim().is_empty() {
            form.subtitle = Some(format!("for {}", job_name.trim()));
        }
        show_type(&mut form, kind, &self.ctx.accounts);
        // The type of a saved destination is fixed: its id names its logs.
        // Only a saved local destination has a drive to re-record.
        form.set_visible("type", existing.is_none());
        form.set_visible("rerecord", existing.is_some() && kind == LOCAL);
        self.sub = Some(DestForm { form, index });
    }

    /// Builds the destination from the open form, looking up the drive for
    /// a local folder, and puts it in the list.
    fn keep_destination(&mut self, capture: &mut Capture, rerecord: bool) {
        let Some(sub) = &mut self.sub else {
            return;
        };
        let form = &mut sub.form;
        form.clear_errors();
        form.message = None;
        let previous = sub.index.and_then(|i| self.dests.get(i)).cloned();
        let id = match &previous {
            Some(d) => d.id().to_string(),
            None => {
                let taken: Vec<String> = self.dests.iter().map(|d| d.id().to_string()).collect();
                generate_destination_id(|c| taken.iter().any(|t| t == c))
            }
        };
        let mode = Mode::ALL[form.picked("mode").min(1)];
        let mut notes = Vec::new();
        let dest = if form.picked("type") == ONEDRIVE {
            let Some(account) = self.ctx.accounts.get(form.picked("account")) else {
                form.set_error(
                    "account",
                    Some("no accounts yet · add one with: onefolder accounts add".into()),
                );
                form.focus("account");
                return;
            };
            // An empty folder is the drive root.
            let remote = clean_remote(&form.text("remote"));
            Destination::onedrive(id.clone(), account.label.clone(), remote, mode)
        } else {
            let typed = form.text("path");
            let path = match local_path(&typed) {
                Ok(p) => p,
                Err(e) => {
                    form.set_error("path", Some(e));
                    form.focus("path");
                    return;
                }
            };
            let shown = path.display().to_string();
            let mut dest = Destination::local(id.clone(), shown.clone());
            dest.set_mode(mode);
            let unchanged = previous.as_ref().filter(|p| p.path() == shown && !rerecord);
            match unchanged {
                Some(p) => dest.set_identity(p.identity().as_ref()),
                None => match record(&path, capture(&path), true) {
                    Ok((None, _))
                        if previous
                            .as_ref()
                            .is_some_and(|p| p.path() == shown && p.identity().is_some())
                            && self.forget_armed.as_deref() != Some(id.as_str()) =>
                    {
                        // A recorded drive is never dropped by accident: the
                        // same folder now on the startup disk means the drive
                        // looks unplugged. A second press confirms.
                        self.forget_armed = Some(id.clone());
                        form.set_error("path", Some("the drive looks unplugged".into()));
                        form.message = Some((
                            format!(
                                "{shown} is now a folder on {disk}, so the drive looks \
                                 unplugged. Plug it in and re-record, or press Re-record drive \
                                 again to forget the drive (runs would then write to {disk}).",
                                disk = crate::platform::words().system_disk
                            ),
                            Tone::Error,
                        ));
                        return;
                    }
                    Ok((identity, mut found_notes)) => {
                        if identity.is_none()
                            && previous.as_ref().is_some_and(|p| p.identity().is_some())
                            && self.forget_armed.as_deref() == Some(id.as_str())
                        {
                            found_notes.push((
                                format!(
                                    "{} {shown}: forgot the recorded drive, as asked.",
                                    theme::ACTION
                                ),
                                Tone::Warning,
                            ));
                        }
                        self.forget_armed = None;
                        dest.set_identity(identity.as_ref());
                        notes = found_notes;
                    }
                    Err(e) => {
                        form.set_error("path", Some(e.headline()));
                        form.message = Some((e.to_string(), Tone::Error));
                        form.focus("path");
                        return;
                    }
                },
            }
            dest
        };
        let duplicate = self
            .dests
            .iter()
            .enumerate()
            .any(|(i, d)| Some(i) != sub.index && d.display() == dest.display());
        if duplicate {
            let key = if dest.type_name() == "local" {
                "path"
            } else {
                "remote"
            };
            form.set_error(key, Some("this job already has that destination".into()));
            form.focus(key);
            return;
        }
        match sub.index {
            Some(i) if i < self.dests.len() => self.dests[i] = dest,
            _ => self.dests.push(dest),
        }
        self.sub = None;
        self.form.mark_dirty();
        self.refresh_rows();
        self.form.message = join_notes(&notes);
        if rerecord || !notes.is_empty() {
            self.notes
                .retain(|(n, _)| n.as_deref() != Some(id.as_str()));
        }
        self.notes
            .extend(notes.into_iter().map(|n| (Some(id.clone()), n)));
    }

    /// Checks the whole job and builds it, or marks what to fix.
    fn finish(&mut self, capture: &mut Capture) -> Option<Outcome> {
        self.form.clear_errors();
        self.form.message = None;
        let name = self.form.text("name").trim().to_string();
        if self.ctx.taken.contains(&name) {
            self.form.set_error(
                "name",
                Some(format!("there is already a job called {name}")),
            );
            self.form.focus("name");
            return None;
        }
        let source = match local_path(&self.form.text("source")) {
            Ok(p) => p,
            Err(e) => {
                self.form.set_error("source", Some(e));
                self.form.focus("source");
                return None;
            }
        };
        if !source.is_dir() {
            self.form.set_error(
                "source",
                Some(format!(
                    "that folder does not exist on {}",
                    crate::platform::words().this_computer
                )),
            );
            self.form.focus("source");
            return None;
        }
        let mut job = self.ctx.existing.clone().unwrap_or_default();
        let old_source = job.source.clone();
        job.name = name;
        job.source = source.display().to_string();
        job.excludes = self.form.items("excludes");
        job.copy_hidden = self.form.picked("hidden") == 1;
        job.destinations = self.dests.clone();

        if old_source != job.source {
            self.notes.retain(|(id, _)| id.is_some());
            match record(&source, capture(&source), false) {
                Ok((identity, notes)) => {
                    job.set_source_identity(identity.as_ref());
                    self.notes.extend(notes.into_iter().map(|n| (None, n)));
                }
                Err(e) => {
                    self.form.set_error("source", Some(e.headline()));
                    self.form.message = Some((e.to_string(), Tone::Error));
                    self.form.focus("source");
                    return None;
                }
            }
        }

        let mut problems = config::validate_job(&job, &self.ctx.accounts);
        problems.extend(config::overlaps_on_disk(&job));
        if problems.is_empty() {
            let long = long_names(&job, &self.ctx.accounts);
            if !long.is_empty() && self.long_names_warned.as_ref() != Some(&job) {
                self.long_names_warned = Some(job.clone());
                let text = format!(
                    "{} The source and {} together are too long for bisync's working file \
                     names ({} allows {} bytes in a file name), so two-way runs would stop \
                     with \"names too long\". Use a shorter source or destination folder, or \
                     save again to keep it as it is.",
                    theme::SKIPPED,
                    long.join(", "),
                    crate::platform::words().file_system,
                    rclone::bisync::NAME_MAX
                );
                self.form.message = Some((text, Tone::Warning));
                return None;
            }
        }
        if !problems.is_empty() {
            let text = problems
                .iter()
                .map(|p| format!("{} {p}", theme::FAILED))
                .collect::<Vec<_>>()
                .join("\n");
            self.form.message = Some((text, Tone::Error));
            return None;
        }
        Some(Outcome::Saved {
            job: Box::new(job),
            notes: self.notes.iter().map(|(_, n)| n.clone()).collect(),
        })
    }
}

/// The two-way destinations whose pair with the source would make
/// bisync's working file names too long, as shown.
pub fn long_names(job: &Job, accounts: &[Account]) -> Vec<String> {
    job.destinations
        .iter()
        .filter(|d| d.mode() == Mode::TwoWay)
        .filter(|d| {
            let target = match d {
                Destination::Onedrive { account, path, .. } => {
                    let remote = accounts
                        .iter()
                        .find(|a| &a.label == account)
                        .map_or_else(|| Account::remote_for(account), |a| a.remote.clone());
                    format!("{remote}:{path}")
                }
                Destination::Local { path, .. } => path.clone(),
            };
            rclone::bisync::names_too_long(&job.source, &target)
        })
        .map(Destination::display)
        .collect()
}

/// Several notes as one message for the form, in the tone of the most
/// serious.
fn join_notes(notes: &[Note]) -> Option<Note> {
    if notes.is_empty() {
        return None;
    }
    let tone = notes
        .iter()
        .map(|(_, t)| *t)
        .find(|t| matches!(t, Tone::Warning | Tone::Error))
        .unwrap_or(notes[0].1);
    let text = notes
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Some((text, tone))
}

/// Shows the fields that belong to the chosen destination type.
fn show_type(form: &mut Form, kind: usize, accounts: &[Account]) {
    let onedrive = kind == ONEDRIVE;
    form.set_visible("account", onedrive);
    form.set_visible("remote", onedrive);
    form.set_visible("path", !onedrive);
    form.set_error(
        "account",
        (onedrive && accounts.is_empty())
            .then(|| "no accounts yet · add one with: onefolder accounts add".to_string()),
    );
}

/// A OneDrive folder as stored: no surrounding spaces or slashes. Empty is
/// the drive root.
pub fn clean_remote(text: &str) -> String {
    text.trim().trim_matches('/').trim().to_string()
}

/// A local path from the form: `~` expanded, must then be absolute.
fn local_path(typed: &str) -> Result<PathBuf, String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Err("needs a folder".into());
    }
    if !Path::new(&config::expand_tilde(typed)).is_absolute() {
        return Err("give the full path, starting with / or ~".into());
    }
    config::absolute(typed).ok_or_else(|| "needs a folder".to_string())
}

/// The note shown when a volume identity is recorded.
pub fn identity_note(path: &str, id: &Identity) -> Note {
    if id.is_weak() {
        (
            format!(
                "{} {path}: recorded {} at {}. This volume reports no UUID, so OneFolder can only check its device and name, a weaker check.",
                theme::SKIPPED,
                id.describe(),
                id.mount_point
            ),
            Tone::Warning,
        )
    } else {
        (
            format!(
                "{} {path}: recorded {} at {}.",
                theme::DONE,
                id.describe(),
                id.mount_point
            ),
            Tone::Success,
        )
    }
}

/// Shows the form until the user saves or cancels. The terminal is back
/// to normal when this returns. Blocks: the CLI runs it on a blocking
/// thread, and `capture` may block on the runtime to look a drive up.
pub fn run(
    ctx: Context,
    mut capture: impl FnMut(&Path) -> Result<Found, VolumeError>,
) -> Result<Outcome> {
    let mut terminal = super::screen()?;
    let mut form = JobForm::new(ctx);
    let mut dirty = true;
    loop {
        if dirty {
            terminal
                .draw(|f| form.render(f))
                .context("could not draw the form")?;
            dirty = false;
        }
        if form.is_pending() {
            if let Some(outcome) = form.resolve(&mut capture) {
                return Ok(outcome);
            }
            dirty = true;
            continue;
        }
        // Wait for a key at most a tenth of a second, so what the
        // background work sends is shown while it comes.
        if event::poll(Duration::from_millis(100)).context("could not read a key")? {
            dirty = true;
            if let Event::Key(key) = event::read().context("could not read a key")? {
                if super::is_ctrl_c(&key) {
                    super::quit_on_ctrl_c();
                }
                if key.kind != KeyEventKind::Release {
                    if let Some(outcome) = form.handle_key(key) {
                        return Ok(outcome);
                    }
                }
            }
        }
        if form.tick() {
            dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DriveType;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn work() -> Account {
        Account {
            label: "work".into(),
            remote: Account::remote_for("work"),
            drive_type: DriveType::Business,
            added: None,
            delta: false,
        }
    }

    fn backup_identity(weak: bool) -> Identity {
        Identity {
            mount_point: "/Volumes/Backup".into(),
            volume_name: "Backup".into(),
            volume_uuid: (!weak).then(|| "A1B2C3D4-0000-4E5F-8A9B-000000000001".into()),
            partition_uuid: None,
            device: weak.then(|| "/dev/disk4s1".into()),
            mount_root: None,
        }
    }

    /// Drives the form the way `run` does: a key, then any pending drive
    /// lookup through a fake capture that knows `/Volumes/Backup` and
    /// treats everything else as the startup disk.
    struct Driver {
        form: JobForm,
        captured: Vec<PathBuf>,
        weak: bool,
        fail: bool,
        read_only: bool,
        /// The message the form showed while a lookup was pending.
        checking: Vec<String>,
    }

    impl Driver {
        fn new(ctx: Context) -> Driver {
            Driver {
                form: JobForm::new(ctx),
                captured: Vec::new(),
                weak: false,
                fail: false,
                read_only: false,
                checking: Vec::new(),
            }
        }

        fn send(&mut self, key: KeyEvent) -> Option<Outcome> {
            if let Some(outcome) = self.form.handle_key(key) {
                return Some(outcome);
            }
            if !self.form.is_pending() {
                return None;
            }
            if let Some((m, _)) = &self.form.current().message {
                self.checking.push(m.clone());
            }
            let (captured, weak, fail, read_only) =
                (&mut self.captured, self.weak, self.fail, self.read_only);
            let mut capture = |p: &Path| {
                captured.push(p.to_path_buf());
                if fail {
                    return Err(VolumeError::OnStartupDisk {
                        path: p.display().to_string(),
                        mount_point: "/System/Volumes/Data".into(),
                    });
                }
                let external = volume::is_under_drives(p);
                Ok(Found {
                    identity: external.then(|| backup_identity(weak)),
                    mount_point: if external {
                        "/Volumes/Backup".into()
                    } else {
                        "/System/Volumes/Data".into()
                    },
                    read_only,
                })
            };
            self.form.resolve(&mut capture)
        }

        fn press(&mut self, code: KeyCode) -> Option<Outcome> {
            self.send(key(code))
        }

        fn ctrl(&mut self, c: char) -> Option<Outcome> {
            self.send(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
        }

        fn ctrl_s(&mut self) -> Option<Outcome> {
            self.ctrl('s')
        }

        fn typed(&mut self, text: &str) {
            for c in text.chars() {
                self.press(KeyCode::Char(c));
            }
        }

        fn edit(&mut self, field: &str, text: &str) {
            self.focus(field);
            self.press(KeyCode::Enter);
            self.ctrl('u');
            self.typed(text);
            self.press(KeyCode::Enter);
        }

        fn focus(&mut self, field: &str) {
            match &mut self.form.sub {
                Some(sub) => sub.form.focus(field),
                None => self.form.form.focus(field),
            }
        }
    }

    fn ctx() -> Context {
        Context {
            accounts: vec![work()],
            taken: vec!["photos".into()],
            existing: None,
            name: Some("documents".into()),
            rclone: None,
        }
    }

    #[test]
    fn adds_a_job_with_both_destination_types() {
        let source = tempfile::tempdir().unwrap();
        let mut d = Driver::new(ctx());
        d.edit("source", &source.path().display().to_string());

        // A OneDrive destination: the type starts on OneDrive.
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        assert!(d.form.sub.is_some());
        d.edit("remote", "/Backups/Documents/");
        d.focus("mode");
        d.press(KeyCode::Right);
        d.focus("keep");
        d.press(KeyCode::Enter);
        assert!(
            d.form.sub.is_none(),
            "{:?}",
            d.form.sub.as_ref().map(|s| &s.form.message)
        );

        // A local one.
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.focus("type");
        d.press(KeyCode::Right);
        d.edit("path", "/Volumes/Backup/Documents");
        d.focus("keep");
        d.press(KeyCode::Enter);
        assert!(d.form.sub.is_none());
        assert_eq!(d.captured, vec![PathBuf::from("/Volumes/Backup/Documents")]);
        let message = d.form.form.message.clone().unwrap();
        assert!(
            message
                .0
                .contains("recorded volume \"Backup\" (UUID A1B2C3D4-...)"),
            "{message:?}"
        );

        let Some(Outcome::Saved { job, notes }) = d.ctrl_s() else {
            panic!("expected a save: {:?}", d.form.form.message);
        };
        assert_eq!(job.name, "documents");
        assert_eq!(notes.len(), 1);
        assert_eq!(job.destinations.len(), 2);
        assert_eq!(job.destinations[0].display(), "work:Backups/Documents");
        assert_eq!(job.destinations[0].mode(), Mode::TwoWay);
        assert_eq!(
            job.destinations[1].mode(),
            Mode::OneWay,
            "local defaults to one-way"
        );
        assert_eq!(job.destinations[1].identity(), Some(backup_identity(false)));
        assert_ne!(job.destinations[0].id(), job.destinations[1].id());
    }

    /// Paths too long together for bisync's file names: the form warns
    /// before saving, and a second save keeps the job as it is.
    #[test]
    fn names_too_long_for_bisync_are_warned_about_before_saving() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("a-long-folder-name-".repeat(6));
        std::fs::create_dir_all(&source).unwrap();
        let mut d = Driver::new(ctx());
        d.edit("source", &source.display().to_string());
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.edit(
            "remote",
            &"Backups/a-long-folder-name-on-onedrive".repeat(3),
        );
        d.focus("mode");
        d.press(KeyCode::Right);
        d.focus("keep");
        d.press(KeyCode::Enter);
        assert!(d.form.sub.is_none());
        assert_eq!(d.ctrl_s(), None, "the first save only warns");
        let (text, tone) = d.form.form.message.clone().unwrap();
        assert_eq!(tone, Tone::Warning);
        assert!(
            text.contains("too long for bisync's working file names"),
            "{text}"
        );
        assert!(
            matches!(d.ctrl_s(), Some(Outcome::Saved { .. })),
            "saved on the second"
        );
    }

    #[test]
    fn a_drive_without_uuid_warns() {
        let mut d = Driver::new(ctx());
        d.weak = true;
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.focus("type");
        d.press(KeyCode::Right);
        d.edit("path", "/Volumes/Backup/Documents");
        d.focus("keep");
        d.press(KeyCode::Enter);
        let (text, tone) = d.form.form.message.clone().unwrap();
        assert_eq!(tone, Tone::Warning);
        assert!(text.contains("weaker check"), "{text}");
        assert_eq!(
            d.form.destinations()[0].identity(),
            Some(backup_identity(true))
        );
    }

    #[test]
    fn an_unplugged_drive_keeps_the_destination_form_open() {
        let mut d = Driver::new(ctx());
        d.fail = true;
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.focus("type");
        d.press(KeyCode::Right);
        d.edit("path", "/Volumes/Backup/Documents");
        d.focus("keep");
        d.press(KeyCode::Enter);
        let sub = d.form.sub.as_ref().expect("still open");
        assert_eq!(sub.form.error("path"), Some("Drive not mounted"));
        assert!(d.form.destinations().is_empty());
        // Esc goes back to the job without it.
        d.press(KeyCode::Esc);
        d.press(KeyCode::Esc);
        assert!(d.form.sub.is_none());
    }

    #[test]
    fn validation_marks_name_and_source() {
        let mut d = Driver::new(ctx());
        d.edit("name", "photos");
        d.edit("source", "/nonexistent/folder");
        assert_eq!(d.ctrl_s(), None);
        assert_eq!(
            d.form.form.error("name"),
            Some("there is already a job called photos")
        );
        d.edit("name", "Bad Name");
        assert!(d
            .form
            .form
            .error("name")
            .is_some_and(|e| e.contains("lowercase")));
        d.edit("name", "docs");
        assert_eq!(d.ctrl_s(), None);
        assert_eq!(
            d.form.form.error("source"),
            Some("that folder does not exist on this Mac")
        );
        d.edit("source", "relative/path");
        assert_eq!(d.ctrl_s(), None);
        assert_eq!(
            d.form.form.error("source"),
            Some("give the full path, starting with / or ~")
        );
    }

    #[test]
    fn a_destination_inside_the_source_is_refused() {
        let source = tempfile::tempdir().unwrap();
        let mut d = Driver::new(ctx());
        d.edit("source", &source.path().display().to_string());
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.focus("type");
        d.press(KeyCode::Right);
        d.edit("path", &source.path().join("copy").display().to_string());
        d.focus("keep");
        d.press(KeyCode::Enter);
        assert_eq!(d.ctrl_s(), None);
        let (text, _) = d.form.form.message.clone().unwrap();
        assert!(text.contains("is inside the source folder"), "{text}");
    }

    fn existing() -> Job {
        let mut job = Job::new("documents", "/usr");
        job.excludes = vec![".DS_Store".into()];
        job.destinations = vec![
            Destination::onedrive("d-7f3a".into(), "work".into(), "Docs".into(), Mode::TwoWay),
            {
                let mut d = Destination::local("d-91c0".into(), "/Volumes/Backup/Documents".into());
                d.set_identity(Some(&backup_identity(false)));
                d
            },
        ];
        job
    }

    #[test]
    fn editing_keeps_ids_and_reorders_and_removes() {
        let mut d = Driver::new(Context {
            existing: Some(existing()),
            ..ctx()
        });
        d.edit("name", "docs");
        d.focus("dests");
        d.press(KeyCode::Enter);
        d.press(KeyCode::Char('J'));
        assert_eq!(d.form.destinations()[0].id(), "d-91c0");
        // Edit the local one without changing its path: no new capture.
        d.press(KeyCode::Char('k'));
        d.press(KeyCode::Enter);
        assert!(d.form.sub.is_some());
        d.focus("mode");
        d.press(KeyCode::Right);
        d.focus("keep");
        d.press(KeyCode::Enter);
        assert!(d.captured.is_empty());
        assert_eq!(d.form.destinations()[0].mode(), Mode::TwoWay);
        assert_eq!(
            d.form.destinations()[0].identity(),
            Some(backup_identity(false))
        );
        // Remove the OneDrive one.
        d.focus("dests");
        d.press(KeyCode::Enter);
        d.press(KeyCode::Down);
        d.press(KeyCode::Char('d'));
        assert_eq!(d.form.destinations().len(), 1);
        d.press(KeyCode::Esc);

        let Some(Outcome::Saved { job, .. }) = d.ctrl_s() else {
            panic!("{:?}", d.form.form.message);
        };
        assert_eq!(job.name, "docs");
        assert_eq!(job.destinations.len(), 1);
        assert_eq!(job.destinations[0].id(), "d-91c0", "rename keeps ids");
        assert_eq!(job.excludes, vec![".DS_Store"]);
    }

    #[test]
    fn copy_hidden_files_is_a_picker_after_the_excludes() {
        let mut d = Driver::new(Context {
            existing: Some(existing()),
            ..ctx()
        });
        assert_eq!(d.form.form.picked("hidden"), 0, "No by default");
        d.focus("excludes");
        d.press(KeyCode::Tab);
        assert_eq!(d.form.form.focused_key(), Some("hidden"));
        d.press(KeyCode::Right);
        let Some(Outcome::Saved { job, .. }) = d.ctrl_s() else {
            panic!("{:?}", d.form.form.message);
        };
        assert!(job.copy_hidden);
        // An existing job that copies them opens on Yes, and No turns it off.
        let mut on = existing();
        on.copy_hidden = true;
        let mut d = Driver::new(Context {
            existing: Some(on),
            ..ctx()
        });
        assert_eq!(d.form.form.picked("hidden"), 1);
        d.focus("hidden");
        d.press(KeyCode::Left);
        let Some(Outcome::Saved { job, .. }) = d.ctrl_s() else {
            panic!("{:?}", d.form.form.message);
        };
        assert!(!job.copy_hidden);
    }

    /// Adds a local destination at `path` through the sub-form.
    fn add_local(d: &mut Driver, path: &str) {
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.focus("type");
        d.press(KeyCode::Right);
        d.edit("path", path);
        d.focus("keep");
        d.press(KeyCode::Enter);
    }

    #[test]
    fn a_drive_lookup_says_checking_first() {
        let mut d = Driver::new(ctx());
        add_local(&mut d, "/Volumes/Backup/Documents");
        assert_eq!(d.checking, vec![CHECKING.to_string()]);
        assert!(!d.form.is_pending());
        // OneDrive needs no lookup and says nothing.
        d.checking.clear();
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.edit("remote", "Docs");
        d.focus("mode");
        d.press(KeyCode::Right);
        d.focus("keep");
        d.press(KeyCode::Enter);
        assert!(d.checking.is_empty(), "{:?}", d.checking);
    }

    #[test]
    fn a_onedrive_folder_left_empty_is_the_drive_root() {
        let mut d = Driver::new(ctx());
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.focus("keep");
        d.press(KeyCode::Enter);
        assert!(d.form.sub.is_none());
        assert_eq!(d.form.destinations()[0].display(), "work: (drive root)");
    }

    #[test]
    fn the_startup_disk_is_not_recorded() {
        let mut d = Driver::new(ctx());
        let dir = tempfile::tempdir().unwrap();
        add_local(&mut d, &dir.path().join("backup").display().to_string());
        assert_eq!(d.form.destinations()[0].identity(), None);
        let (text, _) = d.form.form.message.clone().unwrap();
        assert!(text.contains("on the startup disk"), "{text}");
    }

    #[test]
    fn a_read_only_drive_is_flagged() {
        let mut d = Driver::new(ctx());
        d.read_only = true;
        add_local(&mut d, "/Volumes/Backup/Documents");
        let (text, tone) = d.form.form.message.clone().unwrap();
        assert_eq!(tone, Tone::Warning);
        assert!(text.contains("mounted read-only"), "{text}");
    }

    #[test]
    fn notes_of_a_removed_destination_are_dropped() {
        let source = tempfile::tempdir().unwrap();
        let mut d = Driver::new(ctx());
        d.edit("source", &source.path().display().to_string());
        add_local(&mut d, "/Volumes/Backup/Documents");
        d.focus("dests");
        d.press(KeyCode::Enter);
        d.press(KeyCode::Char('d'));
        d.press(KeyCode::Esc);
        let Some(Outcome::Saved { notes, job }) = d.ctrl_s() else {
            panic!("{:?}", d.form.form.message);
        };
        assert!(job.destinations.is_empty());
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn re_record_looks_the_drive_up_again_and_keeps_the_id() {
        let mut d = Driver::new(Context {
            existing: Some(existing()),
            ..ctx()
        });
        d.focus("dests");
        d.press(KeyCode::Enter);
        d.press(KeyCode::Down);
        d.press(KeyCode::Enter);
        let sub = d.form.sub.as_ref().expect("open");
        assert!(sub.form.field("rerecord").is_some_and(|f| f.visible));
        d.weak = true;
        d.focus("rerecord");
        d.press(KeyCode::Enter);
        assert_eq!(d.captured, vec![PathBuf::from("/Volumes/Backup/Documents")]);
        let dest = &d.form.destinations()[1];
        assert_eq!(dest.id(), "d-91c0");
        assert_eq!(dest.identity(), Some(backup_identity(true)));
        // A OneDrive destination has no drive to re-record.
        d.focus("dests");
        d.press(KeyCode::Enter);
        d.press(KeyCode::Enter);
        let sub = d.form.sub.as_ref().expect("open");
        assert!(sub.form.field("rerecord").is_some_and(|f| !f.visible));
    }

    #[test]
    fn re_record_never_drops_a_drive_without_a_second_press() {
        let mut job = existing();
        let mut local = Destination::local("d-91c0".into(), "/private/mnt/Backup/Documents".into());
        local.set_identity(Some(&Identity {
            mount_point: "/private/mnt/Backup".into(),
            ..backup_identity(false)
        }));
        job.destinations[1] = local;
        let mut d = Driver::new(Context {
            existing: Some(job),
            ..ctx()
        });
        d.focus("dests");
        d.press(KeyCode::Enter);
        d.press(KeyCode::Down);
        d.press(KeyCode::Enter);
        // The fake capture puts anything outside /Volumes on the startup disk.
        d.focus("rerecord");
        d.press(KeyCode::Enter);
        let sub = d.form.sub.as_ref().expect("still open");
        let (text, tone) = sub.form.message.clone().unwrap();
        assert_eq!(tone, Tone::Error);
        assert!(text.contains("the drive looks unplugged"), "{text}");
        assert!(
            d.form.destinations()[1].identity().is_some(),
            "the recorded drive is kept"
        );
        // The second press confirms.
        d.focus("rerecord");
        d.press(KeyCode::Enter);
        assert!(d.form.sub.is_none());
        assert_eq!(d.form.destinations()[1].identity(), None);
        let (text, _) = d.form.form.message.clone().unwrap();
        assert!(text.contains("forgot the recorded drive"), "{text}");
    }

    #[test]
    fn ctrl_c_in_the_destination_form_asks_like_esc() {
        let mut d = Driver::new(ctx());
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.edit("remote", "Docs");
        assert_eq!(d.ctrl('c'), None);
        assert!(d.form.sub.is_some(), "the first Ctrl-C asks");
        assert_eq!(d.ctrl('c'), None);
        assert!(d.form.sub.is_none(), "the second goes back to the job");
        assert!(d.form.destinations().is_empty());
        // In the job form the same two presses cancel it.
        d.form.form.mark_dirty();
        assert_eq!(d.ctrl('c'), None);
        assert_eq!(d.ctrl('c'), Some(Outcome::Cancelled));
    }

    #[test]
    fn esc_cancels() {
        let mut d = Driver::new(ctx());
        assert_eq!(d.press(KeyCode::Esc), Some(Outcome::Cancelled));
    }

    /// Ticks the form until `done`, failing after five seconds.
    fn tick_until(d: &mut Driver, mut done: impl FnMut(&JobForm) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(&d.form) {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                d.form.browsing()
            );
            d.form.tick();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn b_browses_and_space_puts_the_folder_in_the_field() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("pick/alpha/deep")).unwrap();
        std::fs::write(root.join("pick/alpha/deep/f"), vec![0u8; 2000]).unwrap();
        let pick = root.join("pick").display().to_string();
        let mut d = Driver::new(ctx());
        d.edit("source", &pick);
        d.focus("source");
        assert!(d.form.form.hints().contains("b browse"));
        d.press(KeyCode::Char('b'));
        assert!(d.form.browsing().is_some());
        tick_until(&mut d, |f| {
            f.browsing().and_then(|b| b.highlighted()) == Some("alpha")
        });
        // Esc goes back without a change.
        d.press(KeyCode::Esc);
        assert!(d.form.browsing().is_none());
        assert_eq!(d.form.form.text("source"), pick);
        // Again, into alpha, and choose deep.
        d.press(KeyCode::Char('b'));
        tick_until(&mut d, |f| {
            f.browsing().and_then(|b| b.highlighted()) == Some("alpha")
        });
        d.press(KeyCode::Enter);
        tick_until(&mut d, |f| {
            f.browsing().and_then(|b| b.highlighted()) == Some("deep")
        });
        d.press(KeyCode::Char(' '));
        assert!(d.form.browsing().is_none());
        let chosen = root.join("pick/alpha/deep").display().to_string();
        assert_eq!(d.form.form.text("source"), chosen);
        assert_eq!(d.form.form.error("source"), None);
        assert!(d.form.form.is_dirty());
    }

    #[test]
    fn ctrl_b_while_typing_browses_from_the_text_so_far() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("one/two")).unwrap();
        let mut d = Driver::new(ctx());
        d.focus("source");
        d.press(KeyCode::Enter);
        d.typed(&root.join("one/not-yet").display().to_string());
        d.ctrl('b');
        let start = d.form.browsing().map(|b| b.folder().to_string());
        assert_eq!(start, Some(root.join("one").display().to_string()));
        tick_until(&mut d, |f| {
            f.browsing().and_then(|b| b.highlighted()) == Some("two")
        });
        d.press(KeyCode::Char('c'));
        assert_eq!(
            d.form.form.text("source"),
            root.join("one").display().to_string()
        );
    }

    #[test]
    fn a_onedrive_folder_without_rclone_says_it_could_not_list() {
        let mut d = Driver::new(ctx());
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.focus("remote");
        d.press(KeyCode::Enter);
        d.press(KeyCode::Tab);
        let (lines, _, _) = d.form.current().lines(100);
        let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
        assert!(
            text.contains("could not list: rclone is not available"),
            "{text}"
        );
        assert!(d.form.current().hints().contains("tab complete"));
        // b in the field list asks for the browser, which needs rclone too.
        d.press(KeyCode::Enter);
        d.press(KeyCode::Char('b'));
        assert!(d.form.browsing().is_none());
        let (message, tone) = d.form.current().message.clone().unwrap();
        assert_eq!(tone, Tone::Error);
        assert!(message.contains("rclone is not available"), "{message}");
    }

    #[test]
    fn onedrive_names_listed_in_the_background_complete_the_folder() {
        let mut d = Driver::new(ctx());
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        d.focus("remote");
        d.press(KeyCode::Enter);
        d.typed("Ba");
        // As if rclone were there: the form asks, the answer comes later.
        let sub = d.form.sub.as_mut().unwrap();
        assert_eq!(
            sub.form.handle_key(key(KeyCode::Tab)),
            FormEvent::List("remote")
        );
        assert_eq!(sub.form.take_list_request(), Some(String::new()));
        let remote = work().remote;
        d.form
            .names_tx
            .send((
                remote.clone(),
                String::new(),
                Ok(vec!["Backups".into(), "Music".into()]),
            ))
            .unwrap();
        // Another account's answer is not used.
        d.form
            .names_tx
            .send((
                "onefolder-other".into(),
                String::new(),
                Ok(vec!["Bad".into()]),
            ))
            .unwrap();
        assert!(d.form.tick());
        assert_eq!(d.form.current().text("remote"), "Backups/");
        assert!(d.form.names.contains_key(&format!("{remote}:")));
    }

    fn render(form: &JobForm, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 22)).unwrap();
        terminal.draw(|f| form.current().render(f)).unwrap();
        terminal.backend().to_string()
    }

    fn snapshot(name: &str, value: String) {
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!(name, value);
        });
    }

    #[test]
    fn job_form_renders_at_80_and_120_columns() {
        let form = JobForm::new(Context {
            existing: Some(existing()),
            ..ctx()
        });
        snapshot("jobform_80", render(&form, 80));
        snapshot("jobform_120", render(&form, 120));
    }

    #[test]
    fn destination_form_renders_at_80_and_120_columns() {
        let mut d = Driver::new(ctx());
        d.focus("dests");
        d.press(KeyCode::Char('a'));
        snapshot("jobform_destination_80", render(&d.form, 80));
        d.focus("type");
        d.press(KeyCode::Right);
        snapshot("jobform_destination_local_120", render(&d.form, 120));
    }
}
