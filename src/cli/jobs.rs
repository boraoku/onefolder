//! `onefolder jobs [list|add|edit|remove|release] [name]`.
//!
//! On a terminal, `jobs` opens the interactive list and `add`/`edit` open
//! the form. With flags, or with `--json`, or when stdin or stdout is not a
//! terminal, `add` and `edit` work from flags instead, so scripts and tests
//! can manage jobs. Exit codes: 0 done (or nothing to do), 1 the input was
//! not valid (unknown job, bad flag value, a rule from the config), 2 the
//! command could not run (no terminal for the form, `remove` without
//! `--force` when it cannot ask, the config does not load).

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::config::{self, ConfigFile, Paths};
use crate::jsonout::Envelope;
use crate::model::{Account, Destination, Job, Mode, RunRecord};
use crate::rclone::detect::Rclone;
use crate::state::{self, lock, State};
use crate::ui::jobform::{self, Note, Outcome};
use crate::ui::jobs::{self as screen, DestinationView, JobView, LastRun};
use crate::ui::theme::{self, Tone};
use crate::volume::{self, Identity, VolumeError};

#[derive(Debug, Args)]
pub struct JobsArgs {
    #[command(subcommand)]
    pub action: Option<JobsAction>,
}

const DESTINATION_HELP: &str = "\
Destinations from flags:
  --onedrive ACCOUNT:PATH:MODE     a folder on a OneDrive account, such as
                                   --onedrive work:Backups/Documents:two-way
  --local PATH[:MODE]              a local folder or external drive, such as
                                   --local /Volumes/Backup/Documents
  MODE is one-way (copies new and changed files, never deletes) or two-way.
  A OneDrive destination must say which; a local one defaults to one-way.
  An empty OneDrive PATH (work::one-way) is the drive root. Repeat either
  flag for more destinations; OneDrive ones are added first, then local
  ones, each in the order given. Paths may start with ~. For a local folder
  on an external drive, the drive's identity (mount point, name, UUID) is
  recorded so a run can tell it is plugged in.

Without flags, on a terminal, the job form opens instead.";

/// [`DESTINATION_HELP`] with this platform's example drive path
/// (`/Volumes/Backup/Documents` on a Mac, `/media/<you>/...` on Linux).
fn destination_help() -> String {
    DESTINATION_HELP.replace(
        "/Volumes/Backup/Documents",
        &crate::platform::placeholder_drive_path(),
    )
}

#[derive(Debug, Subcommand)]
pub enum JobsAction {
    /// Print jobs and their destinations (bare `jobs` opens the interactive list on a terminal)
    List,
    /// Add a job with the form, or from flags
    #[command(after_help = destination_help())]
    Add {
        /// Name for the new job, such as documents
        name: Option<String>,
        #[command(flatten)]
        spec: JobFlags,
    },
    /// Edit a job with the form, or from flags
    #[command(after_help = destination_help())]
    Edit {
        /// The job to edit; without it, pick from a list
        name: Option<String>,
        /// Rename the job; its destination ids, logs and sync state go with it
        #[arg(long, value_name = "NEW")]
        rename: Option<String>,
        #[command(flatten)]
        spec: JobFlags,
        /// Remove the destination with this id (see jobs list --json); repeat for more
        #[arg(long = "remove-dest", value_name = "ID")]
        remove_dest: Vec<String>,
        /// Remove every exclude pattern
        #[arg(long, conflicts_with = "excludes")]
        no_excludes: bool,
        /// Look up the drive of this local destination again and record it,
        /// keeping its id (after reformatting or replacing a drive); repeat for more
        #[arg(long, value_name = "ID")]
        rerecord: Vec<String>,
        /// Drop the drive recorded for this local destination, so its folder
        /// is no longer checked against a drive (only when it really moved
        /// off the drive for good); repeat for more
        #[arg(long = "forget-drive", value_name = "ID")]
        forget_drive: Vec<String>,
    },
    /// Remove a job; its files and run logs are left where they are
    Remove {
        /// The job to remove; without it, pick from a list
        name: Option<String>,
        /// Do not ask first
        #[arg(long)]
        force: bool,
    },
    /// Start syncing a folder that is on hold again
    Release {
        /// The job that holds the folder
        job: String,
        /// The held folder, relative to the job's source
        folder: String,
    },
}

/// The job fields that `add` and `edit` take as flags.
#[derive(Debug, Clone, Default, Args)]
pub struct JobFlags {
    /// The folder to sync: an absolute path, or one starting with ~
    #[arg(long, value_name = "PATH")]
    pub source: Option<String>,
    /// An rclone exclude pattern; repeat for more (edit: replaces the list). A folder's name or
    /// path, or folder/*, leaves out the whole folder; *.tmp matches files anywhere. System files
    /// such as .DS_Store and Thumbs.db are always excluded
    #[arg(long = "exclude", value_name = "PATTERN", allow_hyphen_values = true)]
    pub excludes: Vec<String>,
    /// Copy hidden files and folders (names starting with a dot); the default is no
    #[arg(
        long = "copy-hidden",
        value_name = "yes|no",
        value_parser = clap::builder::TypedValueParser::map(
            clap::builder::PossibleValuesParser::new(["yes", "no"]),
            |v: String| v == "yes"
        )
    )]
    pub copy_hidden: Option<bool>,
    /// Add a OneDrive destination: ACCOUNT:PATH:one-way|two-way; repeat for more
    #[arg(long = "onedrive", value_name = "ACCOUNT:PATH:MODE")]
    pub onedrive: Vec<String>,
    /// Add a local destination: PATH[:one-way|two-way]; repeat for more
    #[arg(long = "local", value_name = "PATH[:MODE]")]
    pub local: Vec<String>,
}

impl JobFlags {
    fn any(&self) -> bool {
        self.source.is_some()
            || !self.excludes.is_empty()
            || self.copy_hidden.is_some()
            || !self.onedrive.is_empty()
            || !self.local.is_empty()
    }
}

/// A user-facing failure: what to print, the JSON code and the exit code.
#[derive(Debug)]
struct Failure {
    code: &'static str,
    message: String,
    exit: u8,
}

impl Failure {
    fn invalid(message: impl Into<String>) -> Failure {
        Failure {
            code: "invalid",
            message: message.into(),
            exit: 1,
        }
    }

    fn refused(code: &'static str, message: impl Into<String>) -> Failure {
        Failure {
            code,
            message: message.into(),
            exit: 2,
        }
    }
}

/// Either the value, or the failure to report to the user.
type Checked<T> = std::result::Result<T, Failure>;

/// Prints a failure (and its JSON envelope) and gives the exit code. With
/// `--json` every failure exits 2, as the design's JSON contract says
/// (0 success, 1 partial, 2 failure or refusal).
fn report(command: &str, json: bool, f: Failure) -> Result<ExitCode> {
    if json {
        Envelope::failure(command, f.code, &f.message).print()?;
    }
    crate::ui::err(&format!("{} {}\n", theme::FAILED, f.message));
    Ok(ExitCode::from(if json { 2 } else { f.exit }))
}

fn interactive_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub async fn run(args: &JobsArgs, rclone: &Rclone, json: bool, dry_run: bool) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    match &args.action {
        None if !json && interactive_terminal() => interactive(&paths, rclone).await,
        None | Some(JobsAction::List) => list(&paths, json),
        Some(JobsAction::Add { name, spec }) => {
            let result = add(&paths, rclone, name.as_deref(), spec, json, dry_run).await;
            finish("jobs add", json, result)
        }
        Some(JobsAction::Edit {
            name,
            rename,
            spec,
            remove_dest,
            no_excludes,
            rerecord,
            forget_drive,
        }) => {
            let edits = Edits {
                rename: rename.clone(),
                spec: spec.clone(),
                remove_dest: remove_dest.clone(),
                no_excludes: *no_excludes,
                rerecord: rerecord.clone(),
                forget_drive: forget_drive.clone(),
            };
            let result = edit(&paths, rclone, name.as_deref(), &edits, json, dry_run).await;
            finish("jobs edit", json, result)
        }
        Some(JobsAction::Remove { name, force }) => {
            let result = remove(&paths, rclone, name.as_deref(), *force, json, dry_run).await;
            finish("jobs remove", json, result)
        }
        Some(JobsAction::Release { job, folder }) => {
            let result = release(&paths, job, folder, dry_run).await;
            finish("jobs release", json, result)
        }
    }
}

fn finish(command: &str, json: bool, result: Result<Checked<Done>>) -> Result<ExitCode> {
    match result? {
        Ok(Done::Interactive) => Ok(ExitCode::SUCCESS),
        Ok(Done::Report(r)) => {
            if json {
                Envelope::success(command, !r.data.partial, &r.data).print()?;
            } else {
                crate::ui::out(&r.text(theme::colour_on_stdout()))?;
            }
            // A partial result (some data did not move) exits 1.
            Ok(if r.data.partial {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Err(f) => report(command, json, f),
    }
}

/// What a finished command has to say.
enum Done {
    /// The interactive screen already showed everything.
    Interactive,
    Report(Box<Report>),
}

/// The text and JSON result of add, edit and remove.
struct Report {
    lines: Vec<(String, Tone)>,
    data: ReportData,
}

#[derive(Debug, Serialize)]
struct ReportData {
    /// `added`, `edited`, `removed`, `released` or `cancelled`.
    action: &'static str,
    dry_run: bool,
    job: Option<JobJson>,
    /// The name before a rename.
    renamed_from: Option<String>,
    notes: Vec<String>,
    /// True when the change was saved but some of the job's data did not
    /// move with it (listed in `notes`).
    partial: bool,
}

impl Report {
    fn text(&self, colour: bool) -> String {
        self.lines
            .iter()
            .map(|(text, tone)| format!("{}\n", theme::paint(text, *tone, colour)))
            .collect()
    }

    /// The lines as one message for the interactive screen.
    fn message(&self) -> (String, Tone) {
        let tone = self
            .lines
            .iter()
            .map(|(_, t)| *t)
            .find(|t| *t == Tone::Error || *t == Tone::Warning)
            .unwrap_or(Tone::Success);
        let text = self
            .lines
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        (text, tone)
    }
}

fn load(paths: &Paths) -> Checked<ConfigFile> {
    ConfigFile::load(&paths.config_file())
        .map_err(|e| Failure::refused("config-invalid", e.to_string()))
}

fn load_state(paths: &Paths) -> State {
    match State::load(&paths.state_file()) {
        Ok(state) => state,
        Err(e) => {
            crate::ui::err(&format!(
                "{} {e:#} · showing jobs without their last runs\n",
                theme::SKIPPED
            ));
            State::default()
        }
    }
}

// ---------------------------------------------------------------- list

#[derive(Debug, Clone, Serialize)]
struct VolumeJson {
    mount_point: String,
    volume_name: String,
    volume_uuid: Option<String>,
    partition_uuid: Option<String>,
    device: Option<String>,
}

impl From<&Identity> for VolumeJson {
    fn from(id: &Identity) -> Self {
        VolumeJson {
            mount_point: id.mount_point.clone(),
            volume_name: id.volume_name.clone(),
            volume_uuid: id.volume_uuid.clone(),
            partition_uuid: id.partition_uuid.clone(),
            device: id.device.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DestinationJson {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    path: String,
    mode: Mode,
    display: String,
    volume: Option<VolumeJson>,
    /// For a local destination with a recorded volume: whether it is mounted now
    /// (false too when its mount point did not answer; see `not_answering`).
    mounted: Option<bool>,
    /// The recorded mount point did not answer within 2 seconds (a hung
    /// network drive).
    not_answering: bool,
    /// The last real run.
    last_run: Option<RunRecord>,
    /// The last dry run, kept apart since milestone 6.
    last_dry_run: Option<RunRecord>,
    holds: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JobJson {
    name: String,
    source: String,
    excludes: Vec<String>,
    /// Hidden files are copied (`copy_hidden`); false leaves them out.
    copy_hidden: bool,
    source_volume: Option<VolumeJson>,
    destinations: Vec<DestinationJson>,
}

fn job_json(job: &Job, state: &State) -> JobJson {
    JobJson {
        name: job.name.clone(),
        source: job.source.clone(),
        excludes: job.excludes.clone(),
        copy_hidden: job.copy_hidden,
        source_volume: job.source_identity().as_ref().map(VolumeJson::from),
        destinations: job
            .destinations
            .iter()
            .map(|d| {
                let key = State::key(&job.name, d.id());
                let identity = d.identity();
                let drive = identity.as_ref().map(volume::mount_state);
                DestinationJson {
                    id: d.id().to_string(),
                    kind: d.type_name(),
                    account: match d {
                        Destination::Onedrive { account, .. } => Some(account.clone()),
                        Destination::Local { .. } => None,
                    },
                    path: d.path().to_string(),
                    mode: d.mode(),
                    display: d.display(),
                    mounted: drive.map(|s| s == Some(true)),
                    not_answering: drive.is_some_and(|s| s.is_none()),
                    volume: identity.as_ref().map(VolumeJson::from),
                    last_run: state.last_run(&job.name, d.id()).cloned(),
                    last_dry_run: state.last_dry_run(&job.name, d.id()).cloned(),
                    holds: state.holds.get(&key).cloned().unwrap_or_default(),
                }
            })
            .collect(),
    }
}

fn last_run_view(r: &RunRecord) -> LastRun {
    LastRun {
        outcome: r.outcome,
        dry_run: r.dry_run,
        finished: screen::unix_seconds(&r.finished),
    }
}

fn job_views(jobs: &[Job], state: &State) -> Vec<JobView> {
    jobs.iter()
        .map(|job| JobView {
            name: job.name.clone(),
            source: config::tilde(Path::new(&job.source)),
            copy_hidden: job.copy_hidden,
            destinations: job
                .destinations
                .iter()
                .map(|d| {
                    let key = State::key(&job.name, d.id());
                    let drive = d.identity().map(|id| volume::mount_state(&id));
                    DestinationView {
                        target: d.display(),
                        mode: d.mode(),
                        mounted: drive.is_none_or(|s| s != Some(false)),
                        not_answering: drive.is_some_and(|s| s.is_none()),
                        last_run: state.last_run(&job.name, d.id()).map(last_run_view),
                        last_dry_run: state.last_dry_run(&job.name, d.id()).map(last_run_view),
                        holds: state.holds.get(&key).map_or(0, Vec::len),
                    }
                })
                .collect(),
        })
        .collect()
}

fn list(paths: &Paths, json: bool) -> Result<ExitCode> {
    let file = match load(paths) {
        Ok(file) => file,
        Err(f) => return report("jobs list", json, f),
    };
    let state = load_state(paths);
    if json {
        let jobs: Vec<JobJson> = file
            .config
            .jobs
            .iter()
            .map(|j| job_json(j, &state))
            .collect();
        Envelope::success("jobs list", true, jobs).print()?;
    } else {
        let views = job_views(&file.config.jobs, &state);
        let text = screen::render_text(
            &views,
            now(),
            theme::colour_on_stdout(),
            crate::ui::terminal_width(),
        );
        crate::ui::out(&text)?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------- interactive

/// The interactive jobs list: loops until the user goes back.
pub async fn interactive(paths: &Paths, rclone: &Rclone) -> Result<ExitCode> {
    let file = match load(paths) {
        Ok(file) => file,
        Err(f) => return report("jobs", false, f),
    };
    let mut screen = screen::Screen::new(job_views(&file.config.jobs, &load_state(paths)), now());
    loop {
        // Inside the session: anything the last action printed, first.
        super::show_output().await?;
        // The screen blocks on the terminal, so it runs on a blocking thread.
        let (back, action) = tokio::task::spawn_blocking(move || {
            let action = screen::run(&mut screen);
            (screen, action)
        })
        .await
        .context("the jobs screen stopped unexpectedly")?;
        screen = back;
        let action = action?;
        let mut select = None;
        let result = match &action {
            screen::Action::Back => return Ok(ExitCode::SUCCESS),
            screen::Action::Run(name) => {
                let code =
                    super::run::run_job(paths, rclone, Some(name), &super::run::Options::default())
                        .await?;
                // The summaries are on the normal screen (or, inside the
                // session, on the output screen); let them be read before
                // the list takes the screen back.
                tokio::task::spawn_blocking(|| {
                    crate::ui::pause(&theme::paint(
                        "Press any key to go back to the jobs list.",
                        Tone::Dim,
                        theme::colour_on_stdout(),
                    ))
                })
                .await
                .context("the jobs screen stopped unexpectedly")??;
                screen.message = Some(if code == ExitCode::SUCCESS {
                    (format!("{} Ran {name}", theme::DONE), Tone::Success)
                } else {
                    (
                        format!(
                            "{} Ran {name}, not everything went across: see onefolder jobs list",
                            theme::SKIPPED
                        ),
                        Tone::Warning,
                    )
                });
                let file = match load(paths) {
                    Ok(file) => file,
                    Err(f) => return report("jobs", false, f),
                };
                screen.now = now();
                screen.set_jobs(job_views(&file.config.jobs, &load_state(paths)), Some(name));
                continue;
            }
            screen::Action::Add => add_with_form(paths, rclone, None, false).await,
            screen::Action::Edit(name) => edit_with_form(paths, rclone, name, false).await,
            screen::Action::Remove(name) => remove_job(paths, name, false).await,
        };
        screen.message = Some(match result? {
            Ok(Done::Report(r)) => {
                select = r.data.job.as_ref().map(|j| j.name.clone());
                r.message()
            }
            Ok(Done::Interactive) => ("Nothing changed.".to_string(), Tone::Dim),
            Err(f) => (format!("{} {}", theme::FAILED, f.message), Tone::Error),
        });
        let file = match load(paths) {
            Ok(file) => file,
            Err(f) => return report("jobs", false, f),
        };
        screen.now = now();
        screen.set_jobs(
            job_views(&file.config.jobs, &load_state(paths)),
            select.as_deref(),
        );
    }
}

// ------------------------------------------------------------ capture

/// Runs the job form on a blocking thread (it waits on the terminal); the
/// drive lookups it asks for run on this runtime through its handle.
async fn run_form(ctx: jobform::Context) -> Result<Outcome> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        jobform::run(ctx, move |path: &Path| {
            handle.block_on(volume::identify(path))
        })
    })
    .await
    .context("the job form stopped unexpectedly")?
}

// ---------------------------------------------------------------- add

async fn add(
    paths: &Paths,
    rclone: &Rclone,
    name: Option<&str>,
    spec: &JobFlags,
    json: bool,
    dry_run: bool,
) -> Result<Checked<Done>> {
    if !spec.any() {
        if !json && interactive_terminal() {
            return add_with_form(paths, rclone, name, dry_run).await;
        }
        return Ok(Err(Failure::refused(
            "needs-terminal",
            "jobs add needs a terminal for the form, or flags, such as:\n  \
             onefolder jobs add documents --source ~/Documents --onedrive work:Backups/Documents:one-way\n  \
             See onefolder jobs add --help.",
        )));
    }
    let mut file = match load(paths) {
        Ok(file) => file,
        Err(f) => return Ok(Err(f)),
    };
    let Some(name) = name.map(str::trim) else {
        return Ok(Err(Failure::invalid(
            "jobs add needs a name with flags, such as: onefolder jobs add documents --source ~/Documents",
        )));
    };
    if spec.source.is_none() {
        return Ok(Err(Failure::invalid("jobs add needs --source with flags")));
    }
    if file.config.job(name).is_some() {
        return Ok(Err(Failure::invalid(format!(
            "there is already a job called {name}; change it with: onefolder jobs edit {name}"
        ))));
    }
    let mut job = Job::new(name, "");
    let mut notes = Vec::new();
    if let Err(f) = apply_flags(&mut job, spec, &mut notes).await {
        return Ok(Err(f));
    }
    if let Err(f) = check(&job, &file.config.accounts) {
        return Ok(Err(f));
    }
    notes.extend(long_name_notes(&job, &file.config.accounts));
    commit(paths, &mut file, None, job, notes, dry_run).await
}

async fn add_with_form(
    paths: &Paths,
    rclone: &Rclone,
    name: Option<&str>,
    dry_run: bool,
) -> Result<Checked<Done>> {
    let mut file = match load(paths) {
        Ok(file) => file,
        Err(f) => return Ok(Err(f)),
    };
    let ctx = jobform::Context {
        accounts: file.config.accounts.clone(),
        taken: file.config.jobs.iter().map(|j| j.name.clone()).collect(),
        existing: None,
        name: name.map(str::to_string),
        rclone: Some(rclone.clone()),
    };
    match run_form(ctx).await? {
        Outcome::Cancelled => Ok(Ok(Done::Interactive)),
        Outcome::Saved { job, notes } => commit(paths, &mut file, None, *job, notes, dry_run).await,
    }
}

/// Splits a trailing `:one-way` or `:two-way` off a flag value.
fn split_mode(value: &str) -> (&str, Option<Mode>) {
    for mode in Mode::ALL {
        if let Some(rest) = value.strip_suffix(&format!(":{}", mode.as_str())) {
            return (rest, Some(mode));
        }
    }
    (value, None)
}

/// `ACCOUNT:PATH:MODE`, where the mode must be given and an empty PATH is
/// the drive root (`work::two-way`, or `work:two-way`).
pub fn parse_onedrive(value: &str) -> std::result::Result<(String, String, Mode), String> {
    let (rest, mode) = split_mode(value.trim());
    let Some(mode) = mode else {
        return Err(format!(
            "--onedrive {value}: say how to sync it, such as {}:one-way or {}:two-way \
             (OneDrive destinations have no default mode)",
            value.trim(),
            value.trim()
        ));
    };
    let (account, path) = rest.split_once(':').unwrap_or((rest, ""));
    let account = account.trim();
    if account.is_empty() {
        return Err(format!(
            "--onedrive {value}: the account is missing; expected ACCOUNT:PATH:MODE"
        ));
    }
    Ok((account.to_string(), jobform::clean_remote(path), mode))
}

/// `PATH[:MODE]`, with the path made absolute. Local defaults to one-way.
pub fn parse_local(value: &str) -> std::result::Result<(PathBuf, Mode), String> {
    let (rest, mode) = split_mode(value.trim());
    let path = config::absolute(rest)
        .ok_or_else(|| format!("--local {value}: expected PATH[:one-way|two-way]"))?;
    Ok((path, mode.unwrap_or(Mode::OneWay)))
}

/// Looks up the volume of a path and turns it into the identity to record
/// and notes, or the failure for a drive that cannot be proven.
async fn record_volume(
    path: &Path,
    what: &str,
    destination: bool,
    notes: &mut Vec<Note>,
) -> Checked<Option<Identity>> {
    match jobform::record(path, volume::identify(path).await, destination) {
        Ok((identity, found)) => {
            notes.extend(found);
            Ok(identity)
        }
        Err(e) => Err(Failure::invalid(format!(
            "could not record the drive for {what} {}:\n  {e}",
            path.display()
        ))),
    }
}

/// Applies `--source`, `--exclude`, `--onedrive` and `--local` to a job.
async fn apply_flags(job: &mut Job, spec: &JobFlags, notes: &mut Vec<Note>) -> Checked<()> {
    if let Some(source) = &spec.source {
        let path =
            config::absolute(source).ok_or_else(|| Failure::invalid("--source needs a folder"))?;
        if !path.is_dir() {
            return Err(Failure::invalid(format!(
                "--source {}: that folder does not exist",
                path.display()
            )));
        }
        if job.source != path.display().to_string() {
            job.source = path.display().to_string();
            let id = record_volume(&path, "the source", false, notes).await?;
            job.set_source_identity(id.as_ref());
        }
    }
    if !spec.excludes.is_empty() {
        job.excludes = spec.excludes.iter().map(|e| e.trim().to_string()).collect();
    }
    if let Some(copy_hidden) = spec.copy_hidden {
        job.copy_hidden = copy_hidden;
    }
    for value in &spec.onedrive {
        let (account, path, mode) = parse_onedrive(value).map_err(Failure::invalid)?;
        let id = job.new_destination_id();
        job.destinations
            .push(Destination::onedrive(id, account, path, mode));
    }
    for value in &spec.local {
        let (path, mode) = parse_local(value).map_err(Failure::invalid)?;
        let mut dest = Destination::local(job.new_destination_id(), path.display().to_string());
        dest.set_mode(mode);
        let id = record_volume(&path, "the destination", true, notes).await?;
        dest.set_identity(id.as_ref());
        job.destinations.push(dest);
    }
    Ok(())
}

/// Every rule for one job, as one failure listing them all: the config's
/// rules, and the overlap rule again on the paths as the disk resolves them.
fn check(job: &Job, accounts: &[Account]) -> Checked<()> {
    let mut problems = config::validate_job(job, accounts);
    problems.extend(config::overlaps_on_disk(job));
    if problems.is_empty() {
        return Ok(());
    }
    let mut message = if problems.len() == 1 {
        "the job is not valid:".to_string()
    } else {
        format!("the job has {} problems:", problems.len())
    };
    for p in problems {
        message.push_str(&format!("\n  {} {p}", theme::FAILED));
    }
    Err(Failure::invalid(message))
}

// -------------------------------------------------------------- commit

/// Destinations whose two-way listings no longer describe their pair: the
/// target changed (path, account or folder), the mode became two-way, or
/// the source moved. Their bisync state is moved aside so the next two-way
/// run starts with a merge.
fn replaced_pairs(original: &Job, job: &Job) -> Vec<(String, String)> {
    job.destinations
        .iter()
        .filter_map(|d| {
            let before = original.destinations.iter().find(|o| o.id() == d.id())?;
            let why = if before.display() != d.display() {
                "its folder changed"
            } else if original.source != job.source {
                "the job's source changed"
            } else if before.mode() == Mode::OneWay && d.mode() == Mode::TwoWay {
                "it became two-way"
            } else {
                return None;
            };
            Some((d.id().to_string(), why.to_string()))
        })
        .collect()
}

/// Saves a new or edited job, in an order that never leaves the job's data
/// keyed by a name the config does not have:
///
/// 1. `state.json` is read first; a rename or a replaced pair is refused
///    if it cannot be read;
/// 2. the state is rewritten (renamed keys, log paths, dropped records of
///    replaced pairs, the pair of each surviving bisync record);
/// 3. replaced pairs' bisync folders are moved aside, then a rename moves
///    the job's logs and bisync folders;
/// 4. the config is saved;
/// 5. local folders that are new in this edit are created, on a verified
///    drive only.
///
/// Anything that did not move is listed and makes the result partial.
/// The form's warning about bisync's file names, as notes for a change
/// made with flags (saved anyway: the flags were explicit).
fn long_name_notes(job: &Job, accounts: &[Account]) -> Vec<Note> {
    jobform::long_names(job, accounts)
        .into_iter()
        .map(|d| {
            (
                format!(
                    "{} {d}: the source and this destination together are too long for \
                     bisync's working file names ({} allows {} bytes in a file name), so \
                     two-way runs would stop with \"names too long\". Use a shorter source or \
                     destination folder.",
                    theme::SKIPPED,
                    crate::platform::words().file_system,
                    crate::rclone::bisync::NAME_MAX
                ),
                Tone::Warning,
            )
        })
        .collect()
}

async fn commit(
    paths: &Paths,
    file: &mut ConfigFile,
    original: Option<Job>,
    job: Job,
    mut notes: Vec<Note>,
    dry_run: bool,
) -> Result<Checked<Done>> {
    let renamed = original
        .as_ref()
        .filter(|o| o.name != job.name)
        .map(|o| o.name.clone());
    let replaced = original
        .as_ref()
        .map(|o| replaced_pairs(o, &job))
        .unwrap_or_default();
    let old_name = original
        .as_ref()
        .map_or(job.name.as_str(), |o| o.name.as_str());
    let mut failed = Vec::new();

    if dry_run {
        for (id, why) in &replaced {
            notes.push((
                format!(
                    "{} {id}: {why}; its two-way sync state would be set aside",
                    theme::ACTION
                ),
                Tone::Plain,
            ));
        }
        let action = if original.is_some() {
            "edited"
        } else {
            "added"
        };
        return Ok(Ok(report_for(
            action,
            &job,
            renamed,
            notes,
            Vec::new(),
            true,
        )));
    }

    // 1 and 2: the state.
    let state_file = paths.state_file();
    if original.is_some() {
        // Held across load, change and save, so a run's record written
        // meanwhile is not lost.
        let _state_lock = match State::lock(paths) {
            Ok(lock) => lock,
            Err(e) => {
                return Ok(Err(Failure::refused(
                    "state-unwritable",
                    format!("nothing was changed: {e:#}"),
                )))
            }
        };
        let mut state = match State::load(&state_file) {
            Ok(state) => state,
            Err(e) if renamed.is_some() || !replaced.is_empty() => {
                return Ok(Err(Failure::refused(
                    "state-unreadable",
                    format!(
                        "nothing was changed: {e:#}. Fix or move {} first, so the job's \
                         sync state can follow the change.",
                        config::tilde(&state_file)
                    ),
                )))
            }
            Err(_) => State::default(),
        };
        let before = state.clone();
        for (id, why) in &replaced {
            let key = State::key(old_name, id);
            let dropped = state.bisync.remove(&key).is_some();
            let aside = state::move_aside(&paths.bisync_dir().join(old_name).join(id), "replaced");
            match aside {
                Ok(moved) if dropped || moved.is_some() => notes.push((
                    format!(
                        "{} {id}: {why}, so its two-way sync state was set aside{}; \
                         the next two-way run starts with a merge",
                        theme::ACTION,
                        moved
                            .map(|p| format!(" ({})", config::tilde(&p)))
                            .unwrap_or_default()
                    ),
                    Tone::Plain,
                )),
                Ok(_) => {}
                Err(e) => failed.push(format!(
                    "the two-way sync state of {id} could not be set aside: {e}"
                )),
            }
        }
        if let Some(old) = &renamed {
            let moved = state.rename_job(old, &job.name);
            if moved > 0 {
                notes.push((
                    format!(
                        "{} renamed {moved} state {} for {old} to {}",
                        theme::ACTION,
                        if moved == 1 { "entry" } else { "entries" },
                        job.name
                    ),
                    Tone::Plain,
                ));
            }
        }
        // Remember which pair each remaining set of listings belongs to.
        for d in &job.destinations {
            if let Some(b) = state.bisync.get_mut(&State::key(&job.name, d.id())) {
                b.source.get_or_insert_with(|| job.source.clone());
                b.destination.get_or_insert_with(|| d.display());
            }
        }
        if state != before {
            if let Err(e) = state.save(&state_file) {
                return Ok(Err(Failure::refused(
                    "state-unwritable",
                    format!("nothing was changed: {e:#}"),
                )));
            }
        }
    }

    // 3: the folders of a rename.
    if let Some(old) = &renamed {
        let moves = state::move_job_dirs(paths, old, &job.name);
        notes.extend(
            moves
                .done
                .into_iter()
                .map(|m| (format!("{} {m}", theme::ACTION), Tone::Plain)),
        );
        failed.extend(moves.failed);
    }

    // 4: the config.
    match &original {
        Some(o) => {
            if let Some(slot) = file.config.jobs.iter_mut().find(|j| j.name == o.name) {
                *slot = job.clone();
            }
        }
        None => file.config.jobs.push(job.clone()),
    }
    if let Err(e) = file.save() {
        let mut message = format!("{e:#}");
        if renamed.is_some() || !replaced.is_empty() {
            message.push_str(&format!(
                "\n  The job's sync state and folders were already updated for the change; \
                 save the config again (or rename the job back) so they agree. Done so far:\n{}",
                notes
                    .iter()
                    .map(|(n, _)| format!("    {n}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        return Ok(Err(Failure::refused("config-unwritable", message)));
    }

    // 5: new local folders, on a verified drive only.
    create_new_folders(original.as_ref(), &job, &mut notes).await;

    let action = if original.is_some() {
        "edited"
    } else {
        "added"
    };
    Ok(Ok(report_for(action, &job, renamed, notes, failed, false)))
}

/// Creates the folders of local destinations that are new or re-pathed in
/// this change and do not exist yet. A recorded drive must pass the volume
/// check first (so an impostor or a stale `/Volumes/<name>` folder never
/// gets one); a folder with no recorded drive is created only inside an
/// existing parent. Anything else is left for the user, and said so.
async fn create_new_folders(original: Option<&Job>, job: &Job, notes: &mut Vec<Note>) {
    for d in &job.destinations {
        let Destination::Local { path, .. } = d else {
            continue;
        };
        let fresh = original
            .and_then(|o| o.destinations.iter().find(|p| p.id() == d.id()))
            .is_none_or(|p| p.path() != path);
        let p = Path::new(path);
        if !fresh || p.exists() {
            continue;
        }
        let allowed = match d.identity() {
            Some(identity) => match volume::check(p, &identity, true).await {
                Err(VolumeError::Missing { .. }) => Ok(()),
                Ok(()) => continue,
                Err(e) => Err(e.headline()),
            },
            None if p.parent().is_some_and(Path::is_dir) => Ok(()),
            None => Err("its parent folder does not exist".to_string()),
        };
        let result = allowed.and_then(|()| std::fs::create_dir(p).map_err(|e| e.to_string()));
        match result {
            Ok(()) => notes.push((
                format!("{} created folder {path}", theme::ACTION),
                Tone::Plain,
            )),
            Err(why) => notes.push((
                format!("{} did not create {path}: {why}", theme::SKIPPED),
                Tone::Warning,
            )),
        }
    }
}

fn report_for(
    action: &'static str,
    job: &Job,
    renamed_from: Option<String>,
    notes: Vec<Note>,
    failed: Vec<String>,
    dry_run: bool,
) -> Done {
    let verb = match action {
        "added" => "Added",
        "edited" => "Saved",
        _ => "Removed",
    };
    let partial = !failed.is_empty();
    let (glyph, tone) = if partial {
        (theme::SKIPPED, Tone::Warning)
    } else {
        (theme::DONE, Tone::Success)
    };
    let mut lines = vec![(
        format!(
            "{glyph} {verb} job {}{}{}",
            job.name,
            renamed_from
                .as_ref()
                .map(|old| format!(" (was {old})"))
                .unwrap_or_default(),
            if dry_run {
                " (dry run, nothing saved)"
            } else {
                ""
            }
        ),
        tone,
    )];
    if action != "removed" {
        lines.push((
            format!("  source {}", config::tilde(Path::new(&job.source))),
            Tone::Plain,
        ));
        if !job.excludes.is_empty() {
            lines.push((
                format!("  excludes {}", job.excludes.join(", ")),
                Tone::Plain,
            ));
        }
        lines.push((
            format!(
                "  hidden files {}",
                if job.copy_hidden {
                    "copied"
                } else {
                    "not copied"
                }
            ),
            Tone::Plain,
        ));
        if job.destinations.is_empty() {
            lines.push((
                format!("  {} no destinations yet", theme::SKIPPED),
                Tone::Warning,
            ));
        }
        let width = job
            .destinations
            .iter()
            .map(|d| d.display().chars().count())
            .max()
            .unwrap_or(0)
            + 3;
        for d in &job.destinations {
            lines.push((
                format!(
                    "  {} {:<width$}{:<10}{}",
                    theme::ACTION,
                    d.display(),
                    d.mode().as_str(),
                    d.id()
                ),
                Tone::Plain,
            ));
        }
    }
    let mut note_texts: Vec<String> = notes.iter().map(|(t, _)| t.clone()).collect();
    lines.extend(notes.into_iter().map(|(t, tone)| (format!("  {t}"), tone)));
    for f in &failed {
        lines.push((format!("  {} {f}", theme::FAILED), Tone::Error));
        note_texts.push(format!("{} {f}", theme::FAILED));
    }
    Done::Report(Box::new(Report {
        lines,
        data: ReportData {
            action,
            dry_run,
            job: Some(job_json(job, &State::default())),
            renamed_from,
            notes: note_texts,
            partial,
        },
    }))
}

// --------------------------------------------------------------- edit

/// Takes the job's run lock without waiting, so the job is not edited,
/// renamed, released or removed while it runs. A run in progress is a
/// refusal; a stale lock is taken over quietly.
async fn job_lock(paths: &Paths, name: &str) -> Checked<lock::RunLock> {
    match lock::acquire(paths, name).await {
        Ok((held, _)) => Ok(held),
        Err(lock::LockError::Busy { pid, .. }) => Err(Failure::refused(
            "locked",
            format!(
                "a run of {name} is in progress (process {pid}); nothing was changed. Try again \
                 when it has finished."
            ),
        )),
        Err(lock::LockError::Io { path, error }) => Err(Failure::refused(
            "lock-failed",
            format!(
                "could not take the run lock {}: {error}",
                config::tilde(&path)
            ),
        )),
    }
}

struct Edits {
    rename: Option<String>,
    spec: JobFlags,
    remove_dest: Vec<String>,
    no_excludes: bool,
    rerecord: Vec<String>,
    forget_drive: Vec<String>,
}

impl Edits {
    fn any(&self) -> bool {
        self.rename.is_some()
            || self.spec.any()
            || !self.remove_dest.is_empty()
            || self.no_excludes
            || !self.rerecord.is_empty()
            || !self.forget_drive.is_empty()
    }
}

// ------------------------------------------------------------- release

/// `jobs release <job> <folder>`: lifts a hold on every destination of the
/// job that has one for this folder. The folder is then only on the
/// destination, so the next two-way run copies it back to this Mac.
async fn release(paths: &Paths, name: &str, folder: &str, dry_run: bool) -> Result<Checked<Done>> {
    let _run_lock = match job_lock(paths, name).await {
        Ok(lock) => lock,
        Err(f) => return Ok(Err(f)),
    };
    let file = match load(paths) {
        Ok(file) => file,
        Err(f) => return Ok(Err(f)),
    };
    let Some(job) = file.config.job(name) else {
        return Ok(Err(unknown_job(name, &file)));
    };
    let state_file = paths.state_file();
    let _state_lock = match State::lock(paths) {
        Ok(lock) => lock,
        Err(e) => {
            return Ok(Err(Failure::refused(
                "state-unwritable",
                format!("nothing was changed: {e:#}"),
            )))
        }
    };
    let mut state = match State::load(&state_file) {
        Ok(state) => state,
        Err(e) => {
            return Ok(Err(Failure::refused(
                "state-unreadable",
                format!("nothing was changed: {e:#}"),
            )))
        }
    };
    let folder = state::clean_folder(folder);
    let released = state.release(&job.name, &folder);
    if released.is_empty() {
        let held: Vec<String> = job
            .destinations
            .iter()
            .flat_map(|d| state.holds_of(&job.name, d.id()))
            .collect();
        let known = if held.is_empty() {
            format!("job {name} holds no folders")
        } else {
            format!("held folders: {}", crate::ui::quoted_list(&held))
        };
        return Ok(Err(Failure::invalid(format!(
            "{} is not on hold for job {name} ({known})",
            crate::ui::quoted(&folder)
        ))));
    }
    if !dry_run {
        if let Err(e) = state.save(&state_file) {
            return Ok(Err(Failure::refused(
                "state-unwritable",
                format!("nothing was changed: {e:#}"),
            )));
        }
    }
    let mut lines = vec![(
        format!(
            "{} {} {}{}",
            theme::DONE,
            if dry_run { "Would release" } else { "Released" },
            crate::ui::quoted(&folder),
            if dry_run {
                " (dry run, nothing saved)"
            } else {
                ""
            }
        ),
        Tone::Success,
    )];
    let mut notes = Vec::new();
    for id in &released {
        let shown = job
            .destinations
            .iter()
            .find(|d| d.id() == id)
            .map(|d| (d.display(), d.type_name()));
        let (display, kind) = shown.unwrap_or_else(|| (id.clone(), "local"));
        let place = crate::sync::twoway::place(kind);
        let note = format!(
            "{} {display} ({id}): syncing it again. The next two-way run copies {} back to \
             {here} from {place}, because {here} no longer has it.",
            theme::ACTION,
            crate::ui::quoted(&folder),
            here = crate::platform::words().this_computer
        );
        lines.push((format!("  {note}"), Tone::Plain));
        notes.push(note);
    }
    lines.push((
        format!("  Run it with: onefolder run {}", job.name),
        Tone::Dim,
    ));
    Ok(Ok(Done::Report(Box::new(Report {
        lines,
        data: ReportData {
            action: "released",
            dry_run,
            job: Some(job_json(job, &state)),
            renamed_from: None,
            notes,
            partial: false,
        },
    }))))
}

fn unknown_job(name: &str, file: &ConfigFile) -> Failure {
    let names: Vec<&str> = file.config.jobs.iter().map(|j| j.name.as_str()).collect();
    let known = if names.is_empty() {
        "there are no jobs yet".to_string()
    } else {
        format!("jobs: {}", names.join(", "))
    };
    Failure::invalid(format!("there is no job called {name} ({known})"))
}

fn ids_of(job: &Job) -> String {
    let ids: Vec<&str> = job.destinations.iter().map(|d| d.id()).collect();
    if ids.is_empty() {
        "none".to_string()
    } else {
        ids.join(", ")
    }
}

async fn edit(
    paths: &Paths,
    rclone: &Rclone,
    name: Option<&str>,
    edits: &Edits,
    json: bool,
    dry_run: bool,
) -> Result<Checked<Done>> {
    let terminal = !json && interactive_terminal();
    let Some(name) = name else {
        if terminal && !edits.any() {
            interactive(paths, rclone).await?;
            return Ok(Ok(Done::Interactive));
        }
        return Ok(Err(Failure::invalid(
            "jobs edit needs the job's name, such as: onefolder jobs edit documents --rename docs",
        )));
    };
    if !edits.any() {
        if terminal {
            return edit_with_form(paths, rclone, name, dry_run).await;
        }
        return Ok(Err(Failure::refused(
            "needs-terminal",
            "jobs edit needs a terminal for the form, or flags such as --rename, --source, \
             --exclude, --onedrive, --local, --remove-dest, --rerecord or --forget-drive. \
             See onefolder jobs edit --help.",
        )));
    }
    // Not while the job runs: a run reads the config and writes the state
    // that an edit changes or moves.
    let _run_lock = match job_lock(paths, name).await {
        Ok(lock) => lock,
        Err(f) => return Ok(Err(f)),
    };
    let mut file = match load(paths) {
        Ok(file) => file,
        Err(f) => return Ok(Err(f)),
    };
    let Some(original) = file.config.job(name).cloned() else {
        return Ok(Err(unknown_job(name, &file)));
    };
    let mut job = original.clone();
    let mut notes = Vec::new();
    if let Some(new) = &edits.rename {
        job.name = new.trim().to_string();
    }
    if let Err(f) = apply_flags(&mut job, &edits.spec, &mut notes).await {
        return Ok(Err(f));
    }
    if edits.no_excludes {
        job.excludes.clear();
    }
    for id in &edits.remove_dest {
        let Some(i) = job.destinations.iter().position(|d| d.id() == id) else {
            return Ok(Err(Failure::invalid(format!(
                "job {name} has no destination {id} (it has: {})",
                ids_of(&original)
            ))));
        };
        let gone = job.destinations.remove(i);
        notes.push((
            format!(
                "{} removed destination {} ({}); nothing on it is deleted",
                theme::ACTION,
                gone.id(),
                gone.display()
            ),
            Tone::Plain,
        ));
    }
    for id in &edits.rerecord {
        let Some(dest) = job.destinations.iter_mut().find(|d| d.id() == id) else {
            return Ok(Err(Failure::invalid(format!(
                "job {name} has no destination {id} to re-record (it has: {})",
                ids_of(&original)
            ))));
        };
        if dest.type_name() != "local" {
            return Ok(Err(Failure::invalid(format!(
                "destination {id} is on OneDrive; only a local destination has a drive to re-record"
            ))));
        }
        let path = PathBuf::from(dest.path());
        let mut found = Vec::new();
        match record_volume(&path, "the destination", true, &mut found).await {
            // A recorded drive is never dropped by a re-record: the folder
            // now being on the startup disk means the drive looks unplugged.
            Ok(None) if dest.identity().is_some() && !edits.forget_drive.contains(id) => {
                return Ok(Err(Failure::invalid(format!(
                    "destination {id}: {} is now a folder on {}, so the drive \
                     looks unplugged and its record was kept. Plug the drive in and run this \
                     again. If the folder really is no longer on a drive, drop the record with: \
                     onefolder jobs edit {name} --forget-drive {id}",
                    path.display(),
                    crate::platform::words().system_disk
                ))));
            }
            Ok(identity) => {
                dest.set_identity(identity.as_ref());
                notes.extend(found);
            }
            Err(f) => return Ok(Err(f)),
        }
    }
    for id in &edits.forget_drive {
        let Some(dest) = job.destinations.iter_mut().find(|d| d.id() == id) else {
            return Ok(Err(Failure::invalid(format!(
                "job {name} has no destination {id} (it has: {})",
                ids_of(&original)
            ))));
        };
        if dest.type_name() != "local" {
            return Ok(Err(Failure::invalid(format!(
                "destination {id} is on OneDrive; only a local destination has a drive to forget"
            ))));
        }
        if dest.identity().is_some() {
            dest.set_identity(None);
            notes.push((
                format!(
                    "{} {id}: forgot the drive recorded for {}; its folder is no longer \
                     checked against a drive",
                    theme::SKIPPED,
                    dest.path()
                ),
                Tone::Warning,
            ));
        }
    }
    if job.name != original.name && file.config.job(&job.name).is_some() {
        return Ok(Err(Failure::invalid(format!(
            "there is already a job called {}",
            job.name
        ))));
    }
    if let Err(f) = check(&job, &file.config.accounts) {
        return Ok(Err(f));
    }
    notes.extend(long_name_notes(&job, &file.config.accounts));
    commit(paths, &mut file, Some(original), job, notes, dry_run).await
}

async fn edit_with_form(
    paths: &Paths,
    rclone: &Rclone,
    name: &str,
    dry_run: bool,
) -> Result<Checked<Done>> {
    let _run_lock = match job_lock(paths, name).await {
        Ok(lock) => lock,
        Err(f) => return Ok(Err(f)),
    };
    let mut file = match load(paths) {
        Ok(file) => file,
        Err(f) => return Ok(Err(f)),
    };
    let Some(original) = file.config.job(name).cloned() else {
        return Ok(Err(unknown_job(name, &file)));
    };
    let ctx = jobform::Context {
        accounts: file.config.accounts.clone(),
        taken: file
            .config
            .jobs
            .iter()
            .filter(|j| j.name != name)
            .map(|j| j.name.clone())
            .collect(),
        existing: Some(original.clone()),
        name: None,
        rclone: Some(rclone.clone()),
    };
    match run_form(ctx).await? {
        Outcome::Cancelled => Ok(Ok(Done::Interactive)),
        Outcome::Saved { job, notes } => {
            commit(paths, &mut file, Some(original), *job, notes, dry_run).await
        }
    }
}

// ------------------------------------------------------------- remove

async fn remove(
    paths: &Paths,
    rclone: &Rclone,
    name: Option<&str>,
    force: bool,
    json: bool,
    dry_run: bool,
) -> Result<Checked<Done>> {
    let terminal = !json && interactive_terminal();
    let Some(name) = name else {
        if terminal {
            // The interactive list asks before it removes.
            interactive(paths, rclone).await?;
            return Ok(Ok(Done::Interactive));
        }
        return Ok(Err(Failure::invalid(
            "jobs remove needs the job's name, such as: onefolder jobs remove documents --force",
        )));
    };
    let file = match load(paths) {
        Ok(file) => file,
        Err(f) => return Ok(Err(f)),
    };
    if file.config.job(name).is_none() {
        return Ok(Err(unknown_job(name, &file)));
    }
    if !force {
        if !terminal {
            return Ok(Err(Failure::refused(
                "needs-force",
                format!(
                    "jobs remove asks before removing; add --force to remove {name} without asking"
                ),
            )));
        }
        if !confirm(&format!(
            "Remove job {name}? Files on its destinations and its run logs are left where they are. [y/N] "
        ))? {
            crate::ui::out("Kept it.\n")?;
            return Ok(Ok(Done::Interactive));
        }
    }
    if dry_run {
        let job = file.config.job(name).cloned().unwrap_or_default();
        return Ok(Ok(removed_report(
            paths,
            &job,
            Vec::new(),
            Vec::new(),
            true,
        )));
    }
    remove_job(paths, name, false).await
}

/// Removes the job from the config, then sets its two-way sync state aside
/// (`bisync/<job>` to `bisync/<job>.removed-<stamp>`) and drops its entries
/// from `state.json`, so a later job of the same name starts clean. Run
/// logs and files on the destinations are left alone.
async fn remove_job(paths: &Paths, name: &str, dry_run: bool) -> Result<Checked<Done>> {
    let _run_lock = match job_lock(paths, name).await {
        Ok(lock) => lock,
        Err(f) => return Ok(Err(f)),
    };
    let mut file = match load(paths) {
        Ok(file) => file,
        Err(f) => return Ok(Err(f)),
    };
    let Some(i) = file.config.jobs.iter().position(|j| j.name == name) else {
        return Ok(Err(unknown_job(name, &file)));
    };
    let job = file.config.jobs.remove(i);
    if dry_run {
        return Ok(Ok(removed_report(
            paths,
            &job,
            Vec::new(),
            Vec::new(),
            true,
        )));
    }
    if let Err(e) = file.save() {
        return Ok(Err(Failure::refused("config-unwritable", format!("{e:#}"))));
    }
    let mut notes = Vec::new();
    let mut failed = Vec::new();
    match state::move_aside(&paths.bisync_dir().join(name), "removed") {
        Ok(Some(aside)) => notes.push((
            format!(
                "{} its two-way sync state was set aside in {}",
                theme::ACTION,
                config::tilde(&aside)
            ),
            Tone::Plain,
        )),
        Ok(None) => {}
        Err(e) => failed.push(format!("bisync/{name} could not be set aside: {e}")),
    }
    match State::update(paths, |state| state.drop_job(name)) {
        Ok(0) => {}
        Ok(dropped) => notes.push((
            format!(
                "{} removed {dropped} state {} for {name}",
                theme::ACTION,
                if dropped == 1 { "entry" } else { "entries" }
            ),
            Tone::Plain,
        )),
        Err(e) => failed.push(format!("its state entries were not removed: {e:#}")),
    }
    Ok(Ok(removed_report(paths, &job, notes, failed, false)))
}

fn removed_report(
    paths: &Paths,
    job: &Job,
    mut notes: Vec<Note>,
    failed: Vec<String>,
    dry_run: bool,
) -> Done {
    let logs = paths.logs_dir().join(&job.name);
    let mut note = format!(
        "Its run logs are kept{}; files on its destinations are not touched.",
        if logs.exists() {
            format!(" in {}", config::tilde(&logs))
        } else {
            String::new()
        }
    );
    if dry_run {
        note = format!("Nothing was removed. {note}");
    }
    notes.push((note, Tone::Plain));
    report_for("removed", job, None, notes, failed, dry_run)
}

/// Asks a yes/no question on the terminal; anything but y or yes is no.
fn confirm(question: &str) -> Result<bool> {
    let mut out = std::io::stdout().lock();
    out.write_all(question.as_bytes())?;
    out.flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onedrive_flag_values() {
        assert_eq!(
            parse_onedrive("work:Backups/Documents:one-way").unwrap(),
            ("work".into(), "Backups/Documents".into(), Mode::OneWay)
        );
        assert_eq!(
            parse_onedrive("work:/Backups/Documents/:two-way").unwrap(),
            ("work".into(), "Backups/Documents".into(), Mode::TwoWay)
        );
        // A colon inside the path is kept.
        assert_eq!(
            parse_onedrive("work:a:b:one-way").unwrap().1,
            "a:b".to_string()
        );
        // The drive root, either way of writing it.
        assert_eq!(
            parse_onedrive("work::two-way").unwrap(),
            ("work".into(), String::new(), Mode::TwoWay)
        );
        assert_eq!(parse_onedrive("work:one-way").unwrap().1, "");
        // The mode is required for OneDrive.
        let err = parse_onedrive("work:Backups/Documents").unwrap_err();
        assert!(err.contains("work:Backups/Documents:one-way"), "{err}");
        assert!(err.contains("no default mode"), "{err}");
        assert!(parse_onedrive("work")
            .unwrap_err()
            .contains("say how to sync"));
        assert!(parse_onedrive(":Docs:one-way")
            .unwrap_err()
            .contains("account is missing"));
    }

    #[test]
    fn replaced_pairs_are_found() {
        let mut original = Job::new("a", "/src");
        original.destinations = vec![
            Destination::onedrive("d-0001".into(), "work".into(), "A".into(), Mode::TwoWay),
            Destination::local("d-0002".into(), "/b".into()),
            Destination::local("d-0003".into(), "/c".into()),
            Destination::onedrive("d-0004".into(), "work".into(), "D".into(), Mode::TwoWay),
        ];
        let mut job = original.clone();
        job.name = "renamed".into();
        if let Destination::Onedrive { path, .. } = &mut job.destinations[0] {
            *path = "A2".into();
        }
        job.destinations[1].set_mode(Mode::TwoWay);
        job.destinations[3].set_mode(Mode::OneWay);
        let found = replaced_pairs(&original, &job);
        let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["d-0001", "d-0002"], "{found:?}");
        assert!(replaced_pairs(&original, &original).is_empty());
        // Moving the source replaces every pair.
        let mut moved = original.clone();
        moved.source = "/elsewhere".into();
        assert_eq!(replaced_pairs(&original, &moved).len(), 4);
    }

    #[test]
    fn local_flag_values() {
        assert_eq!(
            parse_local("/Volumes/Backup/Docs:two-way").unwrap(),
            (PathBuf::from("/Volumes/Backup/Docs"), Mode::TwoWay)
        );
        assert_eq!(
            parse_local("/tmp/x/../y").unwrap(),
            (PathBuf::from("/tmp/y"), Mode::OneWay)
        );
        assert!(parse_local("  ").is_err());
    }
}
