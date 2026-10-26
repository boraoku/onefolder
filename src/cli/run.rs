//! `onefolder run [job…] [--all] [--dest id]`, with `--dry-run`, `--yes`,
//! `--json`, `--folder-deletes`, `--resync` and `--moves`.
//!
//! Runs a job's destinations strictly in config order: one-way destinations
//! with a preview, a confirmation and a copy; two-way destinations with a
//! plan, the folder-deletion prompt, a confirmation and bisync (a first run
//! merges). A destination that fails, its checks included (a drive not
//! mounted, the wrong drive, an account that does not answer), is recorded
//! as failed with the reason and the next one still runs; Ctrl-C stops the
//! run. After the last destination, a job with more than one prints a
//! closing summary with one line per destination.
//!
//! Only one run of a job at a time: `runs/<job>.lock` under the data
//! directory holds the process id while it runs (see `state::lock`).
//!
//! Several jobs (`run documents records`, `run --all`, or `Several jobs,
//! one after another…` in the job picker) make a batch: each job runs as
//! a run of it alone would, in config order, the next one starting when
//! the one before has finished. A job that fails its checks, has no
//! destinations or whose run lock is busy (taken only when its turn
//! comes) is recorded and the next one starts; only Ctrl-C stops the
//! batch, and the jobs not yet started are recorded as not run. `--dest`
//! belongs to one job and is refused in a batch; an unknown job refuses
//! the whole batch before anything runs. One job given alone is never a
//! batch, and a name given twice is one job; `--all` is a batch even with
//! one job. The batch ends with a closing summary of every job.
//!
//! Exit codes: 0 every destination ok; 1 partial (errors, or a destination
//! that failed, was skipped, declined or cancelled); 2 a refusal before
//! anything ran (another run of the job in progress included), or every
//! destination failed. A batch follows the same rule over the
//! destinations of every job; a job skipped or not run counts as skipped,
//! and a batch in which no job ran a destination (every job skipped for
//! its lock or for having none) exits 2, as a refusal does.

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};

use crate::config::{tilde, Config, ConfigFile, Paths};
use crate::jsonout::run::{DestinationResult, JobResult, JsonReporter};
use crate::model::{Destination, Job, Mode, Outcome};
use crate::rclone::detect::{self, Rclone};
use crate::rclone::exec::{self, InterruptGuard};
use crate::state::lock::{self, LockError};
use crate::state::State;
use crate::sync::oneway::{self, reason_of, Ended};
use crate::sync::reporter::{
    DecidedBy, DestinationInfo, FolderAnswer, FolderPolicy, MovesAnswer, MovesPolicy, Reporter,
};
use crate::sync::twoway;
use crate::ui::accounts::{pick, show_inline, Picker};
use crate::ui::batch::{self, BatchLine, ChooseJobs, JobChoice, JobEnd, JobLine};
use crate::ui::plain::PlainReporter;
use crate::ui::progress::TuiReporter;
use crate::ui::summary::{render_job_summary, DestinationLine};
use crate::ui::theme;

/// The answer to the folder-deletion prompt in two-way runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FolderDeletes {
    /// Ask for each folder (the default on a terminal)
    Ask,
    /// Delete the folder on the destination too
    Delete,
    /// Keep it on the destination and stop syncing it (the default without a terminal)
    Keep,
    /// Restore it locally from the destination
    Restore,
}

/// What to do with files moved locally whose old copy is on the
/// destination (one-way).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Moves {
    /// Ask (the default on a terminal)
    Ask,
    /// Move them on the destination instead of uploading them again (the default with --yes)
    Move,
    /// Upload them again under the new name; the old copies stay
    Copy,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// The job to run; several run one after another, in config order;
    /// without one, pick from a list
    #[arg(value_name = "JOB")]
    pub jobs: Vec<String>,
    /// Run every job, one after another, in config order
    #[arg(long, conflicts_with = "jobs")]
    pub all: bool,
    /// Run only this destination id (one job only)
    #[arg(long, value_name = "ID")]
    pub dest: Option<String>,
    /// Skip the preview confirmation (never the folder prompt)
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// How to answer the folder-deletion prompt
    #[arg(long, value_enum, value_name = "ANSWER")]
    pub folder_deletes: Option<FolderDeletes>,
    /// Rebuild two-way sync state from both sides before running
    #[arg(long)]
    pub resync: bool,
    /// One-way: files moved locally, already on the destination under the old name
    #[arg(long, value_enum, value_name = "ANSWER")]
    pub moves: Option<Moves>,
}

/// How a run was asked for.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dest: Option<String>,
    pub dry_run: bool,
    pub yes: bool,
    pub json: bool,
    pub folder_deletes: Option<FolderDeletes>,
    pub resync: bool,
    pub moves: Option<Moves>,
}

/// How the folder-deletion prompt is answered: the flag, or else ask on a
/// terminal and keep without one (DESIGN.md decision 4). `--yes` never
/// answers it.
pub fn folder_policy(flag: Option<FolderDeletes>, interactive: bool) -> FolderPolicy {
    match flag {
        Some(FolderDeletes::Delete) => FolderPolicy::Answer(FolderAnswer::Delete),
        Some(FolderDeletes::Keep) => FolderPolicy::Answer(FolderAnswer::Keep),
        Some(FolderDeletes::Restore) => FolderPolicy::Answer(FolderAnswer::Restore),
        Some(FolderDeletes::Ask) | None if interactive => FolderPolicy::Ask,
        Some(FolderDeletes::Ask) | None => FolderPolicy::DefaultKeep,
    }
}

/// How the question about moved files is answered: the flag, or else ask
/// on a terminal, and move with `--yes` or without a terminal (where a run
/// needs `--yes` or `--dry-run` anyway). `--moves ask` asks on a terminal
/// even with `--yes`; without a terminal it cannot ask, and since an
/// explicit `ask` means "not without me", the files are uploaded again
/// (decided `NotAsked`).
pub fn moves_policy(flag: Option<Moves>, yes: bool, interactive: bool) -> MovesPolicy {
    match flag {
        Some(Moves::Move) => MovesPolicy::Answer(MovesAnswer::Move, DecidedBy::Flag),
        Some(Moves::Copy) => MovesPolicy::Answer(MovesAnswer::Copy, DecidedBy::Flag),
        Some(Moves::Ask) if interactive => MovesPolicy::Ask,
        Some(Moves::Ask) => MovesPolicy::Answer(MovesAnswer::Copy, DecidedBy::NotAsked),
        None if interactive && !yes => MovesPolicy::Ask,
        None => MovesPolicy::Answer(MovesAnswer::Move, DecidedBy::Default),
    }
}

pub async fn run(args: &RunArgs, json: bool, dry_run: bool) -> Result<ExitCode> {
    // A name given twice is one job: `run documents documents` is a run of
    // it alone. `--all` is a batch even with one job, so a script always
    // gets `batch-done`.
    let mut jobs: Vec<String> = Vec::new();
    for name in &args.jobs {
        if !jobs.contains(name) {
            jobs.push(name.clone());
        }
    }
    let several = args.all || jobs.len() > 1;
    // Detected here rather than by the dispatcher, so `run --json` answers
    // in NDJSON even when rclone is missing.
    let rclone = match detect::detect().await {
        Ok(rclone) => rclone,
        Err(e) => {
            let message = e.user_message();
            if json && several {
                let mut r = JsonReporter::stdout("");
                r.refused(e.code(), message.trim_end());
                r.batch_done(2, &[]);
            } else if json {
                let mut r = JsonReporter::stdout(jobs.first().map_or("", String::as_str));
                r.refused(e.code(), message.trim_end());
                r.done(2, &[]);
            }
            eprint!("{message}");
            return Ok(ExitCode::from(2));
        }
    };
    let rclone = &rclone;
    let paths = Paths::from_env()?;
    let opts = Options {
        dest: args.dest.clone(),
        dry_run,
        yes: args.yes,
        json,
        folder_deletes: args.folder_deletes,
        resync: args.resync,
        moves: args.moves,
    };
    if several {
        let wanted = (!args.all).then_some(jobs);
        return run_batch(&paths, rclone, wanted, &opts).await;
    }
    run_job(&paths, rclone, jobs.first().map(String::as_str), &opts).await
}

/// What happened to one destination, for the exit code and the `done` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestResult {
    Ran(Outcome),
    Skipped,
    Declined,
}

/// One destination's result, as the closing summary and `done` show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub id: String,
    pub display: String,
    pub mode: Mode,
    pub result: DestResult,
    pub reason: Option<String>,
}

impl DestResult {
    fn word(self) -> &'static str {
        match self {
            DestResult::Ran(o) => o.as_str(),
            DestResult::Skipped => "skipped",
            DestResult::Declined => "declined",
        }
    }
}

/// 0 when every destination is ok, 2 when every one failed, 1 otherwise.
pub fn exit_code(results: &[DestResult]) -> u8 {
    if results.iter().all(|r| *r == DestResult::Ran(Outcome::Ok)) {
        0
    } else if !results.is_empty()
        && results
            .iter()
            .all(|r| *r == DestResult::Ran(Outcome::Failed))
    {
        2
    } else {
        1
    }
}

fn interactive_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Reports a refusal before anything ran and gives exit code 2.
fn refuse(job: &str, json: bool, code: &str, message: &str) -> ExitCode {
    if json {
        let mut r = JsonReporter::stdout(job);
        r.refused(code, message);
        r.done(2, &[]);
    }
    crate::ui::err(&format!("{} {message}\n", theme::FAILED));
    ExitCode::from(2)
}

/// Reports a batch refused before any job ran and gives exit code 2: with
/// `--json`, `refused` and a `batch-done` with no jobs.
fn refuse_batch(json: bool, code: &str, message: &str) -> ExitCode {
    if json {
        let mut r = JsonReporter::stdout("");
        r.refused(code, message);
        r.batch_done(2, &[]);
    }
    crate::ui::err(&format!("{} {message}\n", theme::FAILED));
    ExitCode::from(2)
}

/// The job's closing summary, for a job with more than one destination.
fn job_closing(job: &str, dry_run: bool, results: &[Ran], colour: bool) -> Option<String> {
    (results.len() > 1).then(|| {
        render_job_summary(
            job,
            dry_run,
            &destination_lines(results),
            colour,
            crate::ui::terminal_width(),
        )
    })
}

fn destination_lines(results: &[Ran]) -> Vec<DestinationLine> {
    results
        .iter()
        .map(|r| DestinationLine {
            display: r.display.clone(),
            mode: r.mode,
            outcome: r.result.word().to_string(),
            reason: r.reason.clone(),
        })
        .collect()
}

/// The destinations as the `done` and `batch-done` lines list them.
fn destination_results(results: &[Ran]) -> Vec<DestinationResult> {
    results
        .iter()
        .map(|r| DestinationResult {
            destination: r.id.clone(),
            outcome: r.result.word().to_string(),
            reason: r.reason.clone(),
        })
        .collect()
}

/// The output of a run: the screen, plain text, or NDJSON.
enum Output {
    Screen(Box<TuiReporter>),
    Plain(PlainReporter<std::io::Stdout>),
    Json(JsonReporter<std::io::Stdout>),
}

impl Output {
    fn reporter(&mut self) -> &mut dyn Reporter {
        match self {
            Output::Screen(r) => r.as_mut(),
            Output::Plain(r) => r,
            Output::Json(r) => r,
        }
    }

    fn finish(self, job: &str, dry_run: bool, exit: u8, results: &[Ran]) -> Result<()> {
        let closing = job_closing(job, dry_run, results, theme::colour_on_stdout());
        match self {
            Output::Screen(r) => r.finish(closing),
            Output::Plain(mut r) => {
                if let Some(text) = closing {
                    r.closing(&text);
                }
                Ok(())
            }
            Output::Json(mut r) => {
                r.done(exit, &destination_results(results));
                Ok(())
            }
        }
    }

    /// The next job of a batch (`index` from 0) starts.
    fn begin_job(&mut self, index: usize, of: usize, job: &str, line: &BatchLine) {
        match self {
            Output::Screen(r) => r.begin_job(job, line),
            Output::Plain(r) => r.begin_job(index + 1, of, job),
            Output::Json(r) => r.begin_job(job, index + 1, of),
        }
    }

    /// A job of a batch that does not run: skipped (`code` says why, for
    /// `--json`) or not run once the batch was stopped.
    fn job_not_run(
        &mut self,
        index: usize,
        of: usize,
        job: &str,
        code: &str,
        end: &JobEnd,
        line: &BatchLine,
    ) {
        match self {
            Output::Screen(r) => r.job_not_run(job, end, line),
            Output::Plain(r) => {
                r.begin_job(index + 1, of, job);
                r.job_not_run(end);
            }
            Output::Json(r) => {
                r.begin_job(job, index + 1, of);
                let reason = match end {
                    JobEnd::Skipped(reason) => reason.clone(),
                    _ => batch::STOPPED.to_string(),
                };
                r.job_skipped(code, &reason);
            }
        }
    }

    /// A remark about the batch, such as a stale run lock removed: on the
    /// run screen when there is one, since it hides the shell's text.
    fn remark(&mut self, text: &str) {
        match self {
            Output::Screen(r) => r.remark(text),
            _ => crate::ui::err(&format!("{} {text}\n", theme::ACTION)),
        }
    }

    /// A job of a batch is over: its closing summary (more than one
    /// destination) or its `done` line. `line` is the batch line with this
    /// job's end.
    fn end_job(&mut self, job: &str, dry_run: bool, exit: u8, results: &[Ran], line: &BatchLine) {
        let closing = || job_closing(job, dry_run, results, theme::colour_on_stdout());
        match self {
            Output::Screen(r) => r.end_job(closing(), line),
            Output::Plain(r) => {
                if let Some(text) = closing() {
                    r.closing(&text);
                }
            }
            Output::Json(r) => r.done(exit, &destination_results(results)),
        }
    }

    /// The batch is over: its closing summary, or `batch-done`.
    fn finish_batch(self, dry_run: bool, exit: u8, jobs: &[JobRan], elapsed_ms: u64) -> Result<()> {
        let lines: Vec<JobLine> = jobs
            .iter()
            .map(|j| JobLine {
                name: j.name.clone(),
                end: j.end.clone(),
                destinations: match j.end {
                    JobEnd::Skipped(_) | JobEnd::NotRun => Vec::new(),
                    _ => destination_lines(&j.results),
                },
            })
            .collect();
        let closing = |colour: bool| {
            batch::render_closing(
                &lines,
                dry_run,
                elapsed_ms,
                colour,
                crate::ui::terminal_width(),
            )
        };
        match self {
            Output::Screen(r) => r.finish(Some(closing(theme::colour_on_stdout()))),
            Output::Plain(mut r) => {
                r.closing(&closing(theme::colour_on_stdout()));
                Ok(())
            }
            Output::Json(mut r) => {
                let list: Vec<JobResult> = jobs
                    .iter()
                    .map(|j| JobResult {
                        job: j.name.clone(),
                        outcome: match &j.end {
                            JobEnd::Ok => "ok",
                            JobEnd::Partial => "partial",
                            JobEnd::Failed => "failed",
                            JobEnd::Stopped => "stopped",
                            JobEnd::Skipped(_) | JobEnd::NotRun => "skipped",
                        }
                        .to_string(),
                        reason: match &j.end {
                            JobEnd::Skipped(reason) => Some(reason.clone()),
                            JobEnd::NotRun => Some(batch::STOPPED.to_string()),
                            _ => None,
                        },
                        destinations: destination_results(&j.results),
                    })
                    .collect();
                r.batch_done(exit, &list);
                Ok(())
            }
        }
    }
}

/// One job of a batch: how it ended and its destinations' results (for a
/// job that did not run, each destination skipped with the reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRan {
    pub name: String,
    pub end: JobEnd,
    pub results: Vec<Ran>,
}

/// How a job of a batch that ran ended: from its exit code, or stopped
/// when Ctrl-C came before everything was ok.
pub fn job_end(exit: u8, interrupted: bool) -> JobEnd {
    match exit {
        0 => JobEnd::Ok,
        _ if interrupted => JobEnd::Stopped,
        2 => JobEnd::Failed,
        _ => JobEnd::Partial,
    }
}

/// The exit code of a batch: [`exit_code`] over the destinations of every
/// job, a job that did not run counting each of its destinations as
/// skipped (and itself as one, when it has none). When no job ran a
/// destination at all (every one skipped for its lock or for having
/// none), nothing ran, as when a run of one job is refused: 2.
pub fn batch_exit_code(jobs: &[JobRan]) -> u8 {
    if jobs
        .iter()
        .all(|j| matches!(j.end, JobEnd::Skipped(_) | JobEnd::NotRun))
    {
        return 2;
    }
    let results: Vec<DestResult> = jobs
        .iter()
        .flat_map(|j| {
            if j.results.is_empty() {
                vec![DestResult::Skipped]
            } else {
                j.results.iter().map(|r| r.result).collect()
            }
        })
        .collect();
    exit_code(&results)
}

/// What the job picker chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chosen {
    Job(String),
    /// Several jobs, one after another, in config order.
    Several(Vec<String>),
}

/// Asks which job to run, with a picker on the terminal. With two jobs or
/// more its last row, `Several jobs, one after another…`, asks for all of
/// them or a choice (ticked as last time). `None` when the user cancels.
pub async fn pick_job(paths: &Paths, config: &Config) -> Result<Option<Chosen>> {
    let paths = paths.clone();
    let config = config.clone();
    tokio::task::spawn_blocking(move || choose(&paths, &config, &mut OnTerminal))
        .await
        .context("the job picker stopped unexpectedly")?
}

/// Where the job picker's lists are shown: the terminal, or a script in
/// tests.
trait Lists {
    /// A picker: the row chosen, `None` for Esc.
    fn pick(&mut self, picker: Picker) -> Result<Option<usize>>;
    /// The selection screen: the jobs ticked, `None` for Esc.
    fn choose(&mut self, chooser: ChooseJobs) -> Result<Option<Vec<String>>>;
}

/// The lists on the terminal, inline or under the session's page. Ctrl-C
/// on any of them quits OneFolder at once (exit 130) before the list sees
/// the key, so it never just steps back.
struct OnTerminal;

impl Lists for OnTerminal {
    fn pick(&mut self, picker: Picker) -> Result<Option<usize>> {
        pick(picker)
    }
    fn choose(&mut self, chooser: ChooseJobs) -> Result<Option<Vec<String>>> {
        show_inline(chooser)
    }
}

/// The picker, then for several jobs all or some, then the selection
/// screen. Esc on each goes back to the one before. The ticks are saved
/// when the selection starts, never on the way back.
fn choose(paths: &Paths, config: &Config, lists: &mut dyn Lists) -> Result<Option<Chosen>> {
    let names: Vec<String> = config.jobs.iter().map(|j| j.name.clone()).collect();
    let mut picker = Picker::new(
        "Run which job?",
        config
            .jobs
            .iter()
            .map(|j| (j.name.clone(), tilde(Path::new(&j.source))))
            .collect(),
    );
    if names.len() > 1 {
        picker.extra = Some(batch::SEVERAL.to_string());
        picker.esc = "back";
    }
    let mut choices: Option<Vec<JobChoice>> = None;
    loop {
        let Some(i) = lists.pick(picker.clone())? else {
            return Ok(None);
        };
        if let Some(name) = names.get(i) {
            return Ok(Some(Chosen::Job(name.clone())));
        }
        picker.cursor = i;
        let mut at = 0;
        loop {
            let mut all = batch::all_or_some(names.len());
            all.cursor = at;
            match lists.pick(all)? {
                None => break,
                Some(0) => return Ok(Some(Chosen::Several(names.clone()))),
                Some(_) => at = 1,
            }
            let state = State::load(&paths.state_file()).unwrap_or_default();
            let choices = choices.get_or_insert_with(|| {
                crate::cli::jobs::job_views(&config.jobs, &state)
                    .iter()
                    .map(|v| JobChoice {
                        name: v.name.clone(),
                        source: v.source.clone(),
                        destinations: v.destinations.len(),
                        note: crate::ui::jobs::warning(v),
                    })
                    .collect()
            });
            let chooser = ChooseJobs::new(choices.clone(), &state.batch_unticked);
            let Some(chosen) = lists.choose(chooser)? else {
                continue;
            };
            let unticked: Vec<String> = names
                .iter()
                .filter(|n| !chosen.contains(n))
                .cloned()
                .collect();
            // Remembered for next time; a state file that cannot be
            // written only loses the ticks.
            let _ = State::update(paths, |s| s.batch_unticked = unticked);
            return Ok(Some(match <[String; 1]>::try_from(chosen) {
                Ok([one]) => Chosen::Job(one),
                Err(several) => Chosen::Several(several),
            }));
        }
    }
}

/// Runs a job, or asks which one on a terminal. Used by `onefolder run`,
/// the menu's Run item and Enter in the jobs list.
pub async fn run_job(
    paths: &Paths,
    rclone: &Rclone,
    job: Option<&str>,
    opts: &Options,
) -> Result<ExitCode> {
    let interactive = !opts.json && interactive_terminal();
    let shown = job.unwrap_or("");
    let file = match ConfigFile::load(&paths.config_file()) {
        Ok(file) => file,
        Err(e) => return Ok(refuse(shown, opts.json, "config-invalid", &e.to_string())),
    };
    let config = &file.config;
    if config.jobs.is_empty() {
        return Ok(refuse(
            shown,
            opts.json,
            "no-jobs",
            "There are no jobs yet. Add one with: onefolder jobs add",
        ));
    }
    let name =
        match job {
            Some(name) => name.to_string(),
            None if interactive => match pick_job(paths, config).await? {
                Some(Chosen::Job(name)) => name,
                Some(Chosen::Several(names)) => {
                    return run_batch(paths, rclone, Some(names), opts).await
                }
                None => return Ok(ExitCode::SUCCESS),
            },
            None => return Ok(refuse(
                "",
                opts.json,
                "needs-job",
                "Name the job to run, such as: onefolder run documents (see onefolder jobs list)",
            )),
        };
    let Some(job) = config.job(&name) else {
        return Ok(refuse(
            &name,
            opts.json,
            "unknown-job",
            &format!("There is no job called {name}. See onefolder jobs list."),
        ));
    };
    let destinations: Vec<&Destination> = match &opts.dest {
        Some(id) => match job.destinations.iter().find(|d| d.id() == id) {
            Some(d) => vec![d],
            None => {
                return Ok(refuse(
                    &name,
                    opts.json,
                    "unknown-destination",
                    &format!(
                        "Job {name} has no destination {id}. See onefolder jobs list --json for the ids."
                    ),
                ))
            }
        },
        None => job.destinations.iter().collect(),
    };
    if destinations.is_empty() {
        return Ok(refuse(
            &name,
            opts.json,
            "no-destinations",
            &format!(
                "Job {name} has no destinations yet. Add one with: onefolder jobs edit {name}"
            ),
        ));
    }
    if !interactive && !opts.yes && !opts.dry_run {
        return Ok(refuse(
            &name,
            opts.json,
            "needs-yes",
            "onefolder run cannot ask for confirmation here (no terminal, or --json). \
             Add --yes to run without asking, or --dry-run to see the preview only.",
        ));
    }

    // Held for the whole run: Ctrl-C stops rclone instead of OneFolder.
    // Taken before the run lock, so the signal dispatcher (which removes
    // the lock if a signal ends the process outright) is already in place.
    let mut interrupts = exec::interrupt_guard();
    // One run of a job at a time. Held until this function returns; a
    // signal that ends the process removes it too.
    let _lock = match lock::acquire(paths, &name).await {
        Ok((held, removed)) => {
            if let Some(r) = removed {
                crate::ui::err(&format!(
                    "{} {} ({}).\n",
                    theme::ACTION,
                    r.note(),
                    tilde(&r.path)
                ));
            }
            held
        }
        Err(LockError::Busy { pid, path }) => {
            return Ok(refuse(
                &name,
                opts.json,
                "locked",
                &format!(
                    "another OneFolder run of {name} is in progress (process {pid}). Wait for it \
                     to finish, then run again. If no OneFolder is running, delete {}.",
                    tilde(&path)
                ),
            ))
        }
        Err(LockError::Io { path, error }) => {
            return Ok(refuse(
                &name,
                opts.json,
                "lock-failed",
                &format!("Could not take the run lock {}: {error}", tilde(&path)),
            ))
        }
    };

    let mut output = if opts.json {
        Output::Json(JsonReporter::stdout(&name))
    } else if interactive {
        match TuiReporter::start(&name, opts.dry_run) {
            Ok(screen) => Output::Screen(Box::new(screen)),
            // Inside the interactive session plain text would land on the
            // alternate screen in raw mode: say why instead.
            Err(e) if crate::ui::session::is_open() => return Err(e),
            Err(_) => Output::Plain(PlainReporter::stdout(&name)),
        }
    } else {
        Output::Plain(PlainReporter::stdout(&name))
    };

    let turn = Turn {
        paths,
        rclone,
        config,
        opts,
        interactive,
    };
    let mut yes_rest = false;
    let results = run_destinations(
        &turn,
        job,
        destinations,
        &mut output,
        &mut interrupts,
        &mut yes_rest,
    )
    .await;
    drop(interrupts);

    let outcomes: Vec<DestResult> = results.iter().map(|r| r.result).collect();
    let exit = exit_code(&outcomes);
    output.finish(&name, opts.dry_run, exit, &results)?;
    Ok(ExitCode::from(exit))
}

/// What every job of a run shares.
struct Turn<'a> {
    paths: &'a Paths,
    rclone: &'a Rclone,
    config: &'a Config,
    opts: &'a Options,
    interactive: bool,
}

/// Runs one job's destinations strictly in config order, each recorded as
/// it ends; once Ctrl-C has stopped the run the rest are skipped. Once a
/// batch's confirmation was answered `a`, `yes_rest` is set and the rest
/// run as `--yes` would.
async fn run_destinations(
    turn: &Turn<'_>,
    job: &Job,
    destinations: Vec<&Destination>,
    output: &mut Output,
    interrupts: &mut InterruptGuard,
    yes_rest: &mut bool,
) -> Vec<Ran> {
    let opts = turn.opts;
    let interactive = turn.interactive;
    let mut results: Vec<Ran> = Vec::new();
    let of = destinations.len();
    for (i, dest) in destinations.into_iter().enumerate() {
        let run = oneway::Run {
            rclone: turn.rclone,
            paths: turn.paths,
            accounts: &turn.config.accounts,
            job,
            dry_run: opts.dry_run,
            yes: opts.yes || *yes_rest,
            interactive,
            folders: folder_policy(opts.folder_deletes, interactive),
            resync: opts.resync,
            moves: moves_policy(opts.moves, opts.yes, interactive),
        };
        let info = DestinationInfo {
            job: job.name.clone(),
            id: dest.id().to_string(),
            kind: dest.type_name(),
            display: dest.display(),
            mode: dest.mode(),
            index: i + 1,
            of,
            dry_run: opts.dry_run,
        };
        let reporter = output.reporter();
        let mut ran = Ran {
            id: info.id.clone(),
            display: info.display.clone(),
            mode: info.mode,
            result: DestResult::Skipped,
            reason: None,
        };
        if interrupts.interrupted() {
            let reason = "not run, the run was stopped";
            reporter.destination_skipped(&info, reason);
            ran.reason = Some(reason.to_string());
            results.push(ran);
            continue;
        }
        let ended = match dest.mode() {
            Mode::TwoWay => twoway::run_destination(&run, dest, &info, reporter, interrupts).await,
            Mode::OneWay => oneway::run_destination(&run, dest, &info, reporter, interrupts).await,
        };
        (ran.result, ran.reason) = match ended {
            Ended::Finished(summary) => (DestResult::Ran(summary.outcome), reason_of(&summary)),
            Ended::Declined => (DestResult::Declined, Some("not confirmed".to_string())),
        };
        *yes_rest |= output.reporter().yes_to_rest();
        results.push(ran);
    }
    results
}

/// Runs several jobs one after another (`run a b`, `run --all`, or the
/// picker's `Several jobs, one after another…`): `wanted` names them, or
/// `None` for every job. The config is read once, here. Each job runs as a
/// run of it alone would, in config order, with its run lock taken when
/// its turn comes and given back when it ends; the batch line tells which
/// job of how many is running. Nothing stops the batch but Ctrl-C.
pub async fn run_batch(
    paths: &Paths,
    rclone: &Rclone,
    wanted: Option<Vec<String>>,
    opts: &Options,
) -> Result<ExitCode> {
    let clock = Instant::now();
    let interactive = !opts.json && interactive_terminal();
    let file = match ConfigFile::load(&paths.config_file()) {
        Ok(file) => file,
        Err(e) => return Ok(refuse_batch(opts.json, "config-invalid", &e.to_string())),
    };
    let config = &file.config;
    if config.jobs.is_empty() {
        return Ok(refuse_batch(
            opts.json,
            "no-jobs",
            "There are no jobs yet. Add one with: onefolder jobs add",
        ));
    }
    if opts.dest.is_some() {
        return Ok(refuse_batch(
            opts.json,
            "dest-needs-one-job",
            "--dest runs one destination of one job. Name a single job with it, such as: \
             onefolder run documents --dest d-7f3a",
        ));
    }
    let jobs: Vec<&Job> = match &wanted {
        None => config.jobs.iter().collect(),
        Some(names) => {
            if let Some(unknown) = names.iter().find(|n| config.job(n).is_none()) {
                return Ok(refuse_batch(
                    opts.json,
                    "unknown-job",
                    &format!(
                        "There is no job called {unknown}. See onefolder jobs list. \
                         No job was run."
                    ),
                ));
            }
            // Config order, whatever order they were named in.
            config
                .jobs
                .iter()
                .filter(|j| names.contains(&j.name))
                .collect()
        }
    };
    if !interactive && !opts.yes && !opts.dry_run {
        return Ok(refuse_batch(
            opts.json,
            "needs-yes",
            "onefolder run cannot ask for confirmation here (no terminal, or --json). \
             Add --yes to run without asking, or --dry-run to see the previews only.",
        ));
    }

    // Held for the whole batch: Ctrl-C stops rclone instead of OneFolder,
    // and once pressed, the jobs not yet started are not run.
    let mut interrupts = exec::interrupt_guard();
    let names: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
    let first = names.first().map_or("", String::as_str);
    let mut output = if opts.json {
        Output::Json(JsonReporter::stdout(first))
    } else if interactive {
        match TuiReporter::start(first, opts.dry_run) {
            Ok(screen) => Output::Screen(Box::new(screen)),
            Err(e) if crate::ui::session::is_open() => return Err(e),
            Err(_) => Output::Plain(PlainReporter::stdout(first)),
        }
    } else {
        Output::Plain(PlainReporter::stdout(first))
    };

    let turn = Turn {
        paths,
        rclone,
        config,
        opts,
        interactive,
    };
    let of = jobs.len();
    let mut line = BatchLine::new(&names);
    let mut done: Vec<JobRan> = Vec::new();
    let mut yes_rest = false;
    for (i, job) in jobs.into_iter().enumerate() {
        line.current = i;
        // Every destination of a job that does not run, skipped with why.
        let skipped = |reason: &str| -> Vec<Ran> {
            job.destinations
                .iter()
                .map(|d| Ran {
                    id: d.id().to_string(),
                    display: d.display(),
                    mode: d.mode(),
                    result: DestResult::Skipped,
                    reason: Some(reason.to_string()),
                })
                .collect()
        };
        let not_run = if interrupts.interrupted() {
            Some(("stopped", JobEnd::NotRun))
        } else if job.destinations.is_empty() {
            Some((
                "no-destinations",
                JobEnd::Skipped("no destinations yet".to_string()),
            ))
        } else {
            None
        };
        if let Some((code, end)) = not_run {
            output.job_not_run(i, of, &job.name, code, &end, &line);
            let reason = match &end {
                JobEnd::Skipped(reason) => reason.clone(),
                _ => batch::STOPPED.to_string(),
            };
            line.jobs[i].1 = Some(end.clone());
            done.push(JobRan {
                name: job.name.clone(),
                end,
                results: skipped(&reason),
            });
            continue;
        }
        // Taken only now, so a job another run holds is skipped without
        // holding up the rest; given back when the job ends.
        let mut removed = None;
        let held = match lock::acquire(paths, &job.name).await {
            Ok((held, stale)) => {
                removed = stale;
                Ok(held)
            }
            Err(LockError::Busy { .. }) => Err(("locked", "another run in progress".to_string())),
            Err(LockError::Io { path, error }) => Err((
                "lock-failed",
                format!("could not take the run lock {}: {error}", tilde(&path)),
            )),
        };
        let held = match held {
            Ok(held) => held,
            Err((code, reason)) => {
                let end = JobEnd::Skipped(reason.clone());
                output.job_not_run(i, of, &job.name, code, &end, &line);
                line.jobs[i].1 = Some(end.clone());
                done.push(JobRan {
                    name: job.name.clone(),
                    end,
                    results: skipped(&reason),
                });
                continue;
            }
        };
        output.begin_job(i, of, &job.name, &line);
        // Said once the job has the screen, so the run screen keeps it.
        if let Some(r) = removed {
            output.remark(&format!("{} ({}).", r.note(), tilde(&r.path)));
        }
        let destinations: Vec<&Destination> = job.destinations.iter().collect();
        let results = run_destinations(
            &turn,
            job,
            destinations,
            &mut output,
            &mut interrupts,
            &mut yes_rest,
        )
        .await;
        drop(held);
        let outcomes: Vec<DestResult> = results.iter().map(|r| r.result).collect();
        let exit = exit_code(&outcomes);
        let end = job_end(exit, interrupts.interrupted());
        line.jobs[i].1 = Some(end.clone());
        output.end_job(&job.name, opts.dry_run, exit, &results, &line);
        done.push(JobRan {
            name: job.name.clone(),
            end,
            results,
        });
    }
    drop(interrupts);

    let exit = batch_exit_code(&done);
    let elapsed = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
    output.finish_batch(opts.dry_run, exit, &done, elapsed)?;
    Ok(ExitCode::from(exit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        use DestResult::*;
        assert_eq!(exit_code(&[Ran(Outcome::Ok)]), 0);
        assert_eq!(exit_code(&[Ran(Outcome::Ok), Ran(Outcome::Ok)]), 0);
        assert_eq!(exit_code(&[Ran(Outcome::Ok), Ran(Outcome::Partial)]), 1);
        assert_eq!(exit_code(&[Ran(Outcome::Ok), Skipped]), 1);
        assert_eq!(exit_code(&[Ran(Outcome::Ok), Ran(Outcome::Failed)]), 1);
        assert_eq!(exit_code(&[Ran(Outcome::Cancelled)]), 1);
        assert_eq!(exit_code(&[Declined]), 1);
        assert_eq!(exit_code(&[Skipped]), 1);
        assert_eq!(exit_code(&[Ran(Outcome::Failed)]), 2);
        assert_eq!(exit_code(&[Ran(Outcome::Failed), Ran(Outcome::Failed)]), 2);
        assert_eq!(exit_code(&[Ran(Outcome::Failed), Skipped]), 1);
    }

    #[test]
    fn folder_policies() {
        use FolderDeletes::*;
        assert_eq!(folder_policy(None, true), FolderPolicy::Ask);
        assert_eq!(folder_policy(None, false), FolderPolicy::DefaultKeep);
        assert_eq!(folder_policy(Some(Ask), false), FolderPolicy::DefaultKeep);
        assert_eq!(
            folder_policy(Some(Delete), false),
            FolderPolicy::Answer(FolderAnswer::Delete)
        );
        assert_eq!(
            folder_policy(Some(Restore), true),
            FolderPolicy::Answer(FolderAnswer::Restore)
        );
        assert_eq!(
            folder_policy(Some(Keep), true),
            FolderPolicy::Answer(FolderAnswer::Keep)
        );
    }

    #[test]
    fn moves_policies() {
        use MovesAnswer::{Copy as Up, Move as Mv};
        let ask = MovesPolicy::Ask;
        let default_move = MovesPolicy::Answer(Mv, DecidedBy::Default);
        // (flag, --yes, terminal)
        assert_eq!(moves_policy(None, false, true), ask);
        assert_eq!(moves_policy(None, true, true), default_move);
        assert_eq!(moves_policy(None, true, false), default_move);
        assert_eq!(moves_policy(None, false, false), default_move, "a dry run");
        assert_eq!(moves_policy(Some(Moves::Ask), true, true), ask);
        // Asked for, with no terminal to ask on: never moved without a yes.
        let not_asked = MovesPolicy::Answer(Up, DecidedBy::NotAsked);
        assert_eq!(moves_policy(Some(Moves::Ask), true, false), not_asked);
        assert_eq!(moves_policy(Some(Moves::Ask), false, false), not_asked);
        for (yes, terminal) in [(false, true), (true, true), (true, false)] {
            assert_eq!(
                moves_policy(Some(Moves::Move), yes, terminal),
                MovesPolicy::Answer(Mv, DecidedBy::Flag)
            );
            assert_eq!(
                moves_policy(Some(Moves::Copy), yes, terminal),
                MovesPolicy::Answer(Up, DecidedBy::Flag)
            );
        }
    }

    #[test]
    fn moves_flag_parses() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            run: RunArgs,
        }
        let t = T::try_parse_from(["t", "documents", "--moves", "copy"]).unwrap();
        assert_eq!(t.run.moves, Some(Moves::Copy));
        let t = T::try_parse_from(["t", "--moves=move", "--yes"]).unwrap();
        assert_eq!(t.run.moves, Some(Moves::Move));
        let t = T::try_parse_from(["t", "--moves", "ask"]).unwrap();
        assert_eq!(t.run.moves, Some(Moves::Ask));
        assert!(T::try_parse_from(["t", "documents"])
            .unwrap()
            .run
            .moves
            .is_none());
        assert!(T::try_parse_from(["t", "--moves", "delete"]).is_err());
    }

    #[test]
    fn result_words() {
        assert_eq!(DestResult::Ran(Outcome::Partial).word(), "partial");
        assert_eq!(DestResult::Skipped.word(), "skipped");
        assert_eq!(DestResult::Declined.word(), "declined");
    }

    fn ran(result: DestResult) -> Ran {
        Ran {
            id: "d-0001".into(),
            display: "/Volumes/Backup/Documents".into(),
            mode: Mode::OneWay,
            result,
            reason: None,
        }
    }

    fn job(end: JobEnd, results: Vec<DestResult>) -> JobRan {
        JobRan {
            name: "documents".into(),
            end,
            results: results.into_iter().map(ran).collect(),
        }
    }

    #[test]
    fn batch_exit_codes() {
        use DestResult::*;
        let ok = Ran(Outcome::Ok);
        let failed = Ran(Outcome::Failed);
        assert_eq!(
            batch_exit_code(&[job(JobEnd::Ok, vec![ok, ok]), job(JobEnd::Ok, vec![ok])]),
            0
        );
        assert_eq!(
            batch_exit_code(&[
                job(JobEnd::Ok, vec![ok]),
                job(JobEnd::Partial, vec![ok, failed])
            ]),
            1
        );
        assert_eq!(
            batch_exit_code(&[
                job(JobEnd::Failed, vec![failed]),
                job(JobEnd::Failed, vec![failed, failed])
            ]),
            2
        );
        // A job skipped or not run counts as skipped, with or without
        // destinations.
        let busy = JobEnd::Skipped("another run in progress".into());
        assert_eq!(
            batch_exit_code(&[job(JobEnd::Ok, vec![ok]), job(busy.clone(), vec![Skipped])]),
            1
        );
        assert_eq!(
            batch_exit_code(&[
                job(JobEnd::Ok, vec![ok]),
                job(JobEnd::Skipped("no destinations yet".into()), vec![])
            ]),
            1
        );
        assert_eq!(
            batch_exit_code(&[
                job(JobEnd::Failed, vec![failed]),
                job(JobEnd::NotRun, vec![Skipped])
            ]),
            1
        );
    }

    #[test]
    fn how_a_job_of_a_batch_ended() {
        assert_eq!(job_end(0, false), JobEnd::Ok);
        assert_eq!(job_end(0, true), JobEnd::Ok, "Ctrl-C after the last one");
        assert_eq!(job_end(1, false), JobEnd::Partial);
        assert_eq!(job_end(2, false), JobEnd::Failed);
        assert_eq!(job_end(1, true), JobEnd::Stopped);
        assert_eq!(job_end(2, true), JobEnd::Stopped);
    }

    #[test]
    fn several_jobs_and_all_parse() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            run: RunArgs,
        }
        let t = T::try_parse_from(["t", "records", "documents", "--yes"]).unwrap();
        assert_eq!(t.run.jobs, ["records", "documents"]);
        assert!(!t.run.all);
        let t = T::try_parse_from(["t", "--all", "--yes"]).unwrap();
        assert!(t.run.all && t.run.jobs.is_empty());
        assert!(T::try_parse_from(["t", "--all", "documents"]).is_err());
        assert!(T::try_parse_from(["t"]).unwrap().run.jobs.is_empty());
    }

    #[test]
    fn a_batch_where_nothing_ran_exits_2() {
        let busy = JobEnd::Skipped("another run in progress".into());
        let empty = JobEnd::Skipped("no destinations yet".into());
        assert_eq!(
            batch_exit_code(&[
                job(busy.clone(), vec![DestResult::Skipped]),
                job(empty, vec![])
            ]),
            2
        );
        assert_eq!(batch_exit_code(&[job(busy, vec![DestResult::Skipped])]), 2);
    }

    /// The job picker's lists, answered from a script: each answer is a
    /// picker row (`Some(i)`), Esc (`None`), or the selection screen's
    /// names.
    enum Step {
        Pick(Option<usize>),
        Tick(Option<Vec<&'static str>>),
    }

    struct Scripted {
        steps: Vec<Step>,
        /// The ticks each selection screen started with.
        seen: Vec<Vec<bool>>,
    }

    impl Lists for Scripted {
        fn pick(&mut self, _: Picker) -> Result<Option<usize>> {
            match self.steps.remove(0) {
                Step::Pick(i) => Ok(i),
                Step::Tick(_) => panic!("a picker, not the selection screen"),
            }
        }
        fn choose(&mut self, chooser: ChooseJobs) -> Result<Option<Vec<String>>> {
            self.seen.push(chooser.ticked.clone());
            match self.steps.remove(0) {
                Step::Tick(names) => Ok(names.map(|n| n.iter().map(|s| s.to_string()).collect())),
                Step::Pick(_) => panic!("the selection screen, not a picker"),
            }
        }
    }

    fn three_jobs() -> (tempfile::TempDir, Paths, Config) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
        };
        let mut text = String::from("version = 1\n");
        for (name, id) in [
            ("documents", "d-0001"),
            ("records", "d-0002"),
            ("photos", "d-0003"),
        ] {
            text.push_str(&format!(
                "\n[[jobs]]\nname = \"{name}\"\nsource = \"{src}\"\n\n  [[jobs.destinations]]\n  \
                 id = \"{id}\"\n  type = \"local\"\n  path = \"{dst}\"\n",
                src = dir.path().join(name).display(),
                dst = dir.path().join(format!("{name}-copy")).display(),
            ));
        }
        let config = ConfigFile::parse(&paths.config_file(), &text)
            .unwrap()
            .config;
        (dir, paths, config)
    }

    #[test]
    fn choosing_several_jobs_goes_back_a_level_and_remembers_on_enter() {
        use Step::*;
        let (_dir, paths, config) = three_jobs();
        let unticked = || State::load(&paths.state_file()).unwrap().batch_unticked;
        // A job of the list is a single run.
        let mut lists = Scripted {
            steps: vec![Pick(Some(1))],
            seen: vec![],
        };
        assert_eq!(
            choose(&paths, &config, &mut lists).unwrap(),
            Some(Chosen::Job("records".into()))
        );
        // Several, Esc back to the list, Esc again leaves.
        let mut lists = Scripted {
            steps: vec![Pick(Some(3)), Pick(None), Pick(None)],
            seen: vec![],
        };
        assert_eq!(choose(&paths, &config, &mut lists).unwrap(), None);
        // Several, all.
        let mut lists = Scripted {
            steps: vec![Pick(Some(3)), Pick(Some(0))],
            seen: vec![],
        };
        assert_eq!(
            choose(&paths, &config, &mut lists).unwrap(),
            Some(Chosen::Several(vec![
                "documents".into(),
                "records".into(),
                "photos".into()
            ]))
        );
        // Choose, Esc back to all-or-some (nothing saved), choose again
        // and start two: saved on Enter.
        let mut lists = Scripted {
            steps: vec![
                Pick(Some(3)),
                Pick(Some(1)),
                Tick(None),
                Pick(Some(1)),
                Tick(Some(vec!["documents", "photos"])),
            ],
            seen: vec![],
        };
        assert_eq!(
            choose(&paths, &config, &mut lists).unwrap(),
            Some(Chosen::Several(vec!["documents".into(), "photos".into()]))
        );
        assert_eq!(lists.seen, [[true, true, true], [true, true, true]]);
        assert_eq!(unticked(), ["records"]);
        // Next time the ticks start there; going back saves nothing.
        let mut lists = Scripted {
            steps: vec![
                Pick(Some(3)),
                Pick(Some(1)),
                Tick(None),
                Pick(None),
                Pick(None),
            ],
            seen: vec![],
        };
        assert_eq!(choose(&paths, &config, &mut lists).unwrap(), None);
        assert_eq!(lists.seen, [[true, false, true]]);
        assert_eq!(unticked(), ["records"]);
        // A single tick is a run of that job alone.
        let mut lists = Scripted {
            steps: vec![Pick(Some(3)), Pick(Some(1)), Tick(Some(vec!["photos"]))],
            seen: vec![],
        };
        assert_eq!(
            choose(&paths, &config, &mut lists).unwrap(),
            Some(Chosen::Job("photos".into()))
        );
        assert_eq!(unticked(), ["documents", "records"]);
    }
}
