//! `onefolder run [job] [--dest id]`, with `--dry-run`, `--yes`, `--json`,
//! `--folder-deletes`, `--resync` and `--moves`.
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
//! Exit codes: 0 every destination ok; 1 partial (errors, or a destination
//! that failed, was skipped, declined or cancelled); 2 a refusal before
//! anything ran (another run of the job in progress included), or every
//! destination failed.

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};

use crate::config::{tilde, ConfigFile, Paths};
use crate::jsonout::run::{DestinationResult, JsonReporter};
use crate::model::{Destination, Mode, Outcome};
use crate::rclone::detect::{self, Rclone};
use crate::rclone::exec;
use crate::state::lock::{self, LockError};
use crate::sync::oneway::{self, reason_of, Ended};
use crate::sync::reporter::{
    DecidedBy, DestinationInfo, FolderAnswer, FolderPolicy, MovesAnswer, MovesPolicy, Reporter,
};
use crate::sync::twoway;
use crate::ui::accounts::{pick, Picker};
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
    /// The job to run; without it, pick from a list
    pub job: Option<String>,
    /// Run only this destination id
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
    // Detected here rather than by the dispatcher, so `run --json` answers
    // in NDJSON even when rclone is missing.
    let rclone = match detect::detect().await {
        Ok(rclone) => rclone,
        Err(e) => {
            let message = e.user_message();
            if json {
                let mut r = JsonReporter::stdout(args.job.as_deref().unwrap_or(""));
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
    run_job(&paths, rclone, args.job.as_deref(), &opts).await
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
        // The closing summary, for a job with more than one destination.
        let closing = |colour: bool| {
            (results.len() > 1).then(|| {
                let lines: Vec<DestinationLine> = results
                    .iter()
                    .map(|r| DestinationLine {
                        display: r.display.clone(),
                        mode: r.mode,
                        outcome: r.result.word().to_string(),
                        reason: r.reason.clone(),
                    })
                    .collect();
                render_job_summary(job, dry_run, &lines, colour, crate::ui::terminal_width())
            })
        };
        match self {
            Output::Screen(r) => r.finish(closing(theme::colour_on_stdout())),
            Output::Plain(mut r) => {
                if let Some(text) = closing(theme::colour_on_stdout()) {
                    r.closing(&text);
                }
                Ok(())
            }
            Output::Json(mut r) => {
                let list: Vec<DestinationResult> = results
                    .iter()
                    .map(|r| DestinationResult {
                        destination: r.id.clone(),
                        outcome: r.result.word().to_string(),
                        reason: r.reason.clone(),
                    })
                    .collect();
                r.done(exit, &list);
                Ok(())
            }
        }
    }
}

/// Asks which job to run, with a picker on the terminal. `None` when the
/// user cancels.
pub async fn pick_job(config: &crate::config::Config) -> Result<Option<String>> {
    let picker = Picker::new(
        "Run which job?",
        config
            .jobs
            .iter()
            .map(|j| (j.name.clone(), tilde(Path::new(&j.source))))
            .collect(),
    );
    let picked = tokio::task::spawn_blocking(move || pick(picker))
        .await
        .context("the job picker stopped unexpectedly")??;
    Ok(picked
        .and_then(|i| config.jobs.get(i))
        .map(|j| j.name.clone()))
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
            None if interactive => match pick_job(config).await? {
                Some(name) => name,
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

    let run = oneway::Run {
        rclone,
        paths,
        accounts: &config.accounts,
        job,
        dry_run: opts.dry_run,
        yes: opts.yes,
        interactive,
        folders: folder_policy(opts.folder_deletes, interactive),
        resync: opts.resync,
        moves: moves_policy(opts.moves, opts.yes, interactive),
    };
    let mut results: Vec<Ran> = Vec::new();
    let of = destinations.len();
    for (i, dest) in destinations.into_iter().enumerate() {
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
            Mode::TwoWay => {
                twoway::run_destination(&run, dest, &info, reporter, &mut interrupts).await
            }
            Mode::OneWay => {
                oneway::run_destination(&run, dest, &info, reporter, &mut interrupts).await
            }
        };
        (ran.result, ran.reason) = match ended {
            Ended::Finished(summary) => (DestResult::Ran(summary.outcome), reason_of(&summary)),
            Ended::Declined => (DestResult::Declined, Some("not confirmed".to_string())),
        };
        results.push(ran);
    }
    drop(interrupts);

    let outcomes: Vec<DestResult> = results.iter().map(|r| r.result).collect();
    let exit = exit_code(&outcomes);
    output.finish(&name, opts.dry_run, exit, &results)?;
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
}
