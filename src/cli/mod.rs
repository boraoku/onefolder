//! Command-line parsing and dispatch. Each subcommand has its own file with
//! its flags; handlers stay thin and call into the modules that do the work.
//! With no arguments, `onefolder` opens the interactive main menu.

pub mod accounts;
pub mod completion;
pub mod doctor;
pub mod jobs;
pub mod run;
pub mod status;

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::config::{ConfigFile, Paths};
use crate::jsonout::{write_stdout, Envelope};
use crate::rclone::detect::{self, Rclone};
use crate::ui;

/// Sync local folders to OneDrive and external drives, one-way or two-way, with rclone.
#[derive(Debug, Parser)]
#[command(name = "onefolder", version, propagate_version = true)]
pub struct Cli {
    /// Print one JSON document instead of text, and never prompt
    #[arg(long, global = true)]
    pub json: bool,

    /// Show what would change without changing anything
    #[arg(long, global = true)]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage OneDrive accounts
    Accounts(accounts::AccountsArgs),
    /// Manage sync jobs
    Jobs(jobs::JobsArgs),
    /// Run a job: all its destinations in order, or one
    Run(run::RunArgs),
    /// Show the last run of each destination
    Status(status::StatusArgs),
    /// Check rclone, accounts, paths, volumes, state and logs
    Doctor(doctor::DoctorArgs),
    /// Print a shell completion script
    Completion(completion::CompletionArgs),
    /// Show the OneFolder version and the rclone it found
    Version,
}

/// Runs the parsed command and returns the process exit code.
pub async fn dispatch(cli: Cli) -> Result<ExitCode> {
    let json = cli.json;
    match &cli.command {
        None => menu(json).await,
        Some(Command::Version) => version(json).await,
        Some(Command::Completion(args)) => completion::run(args),
        Some(Command::Doctor(args)) => doctor::run(args, json, cli.dry_run).await,
        Some(Command::Accounts(args)) => match require_rclone("accounts", json).await? {
            Ok(rclone) => accounts::run(args, &rclone, json, cli.dry_run).await,
            Err(code) => Ok(code),
        },
        Some(Command::Jobs(args)) => match require_rclone("jobs", json).await? {
            Ok(rclone) => jobs::run(args, &rclone, json, cli.dry_run).await,
            Err(code) => Ok(code),
        },
        Some(Command::Run(args)) => run::run(args, json, cli.dry_run).await,
        // Status reads the config and state; rclone is optional there.
        Some(Command::Status(args)) => status::run(args, json).await,
    }
}

/// Detects rclone. On failure, reports it and gives back the exit code to use.
pub async fn require_rclone(command: &str, json: bool) -> Result<Result<Rclone, ExitCode>> {
    match detect::detect().await {
        Ok(rclone) => Ok(Ok(rclone)),
        Err(e) => {
            let message = e.user_message();
            if json {
                Envelope::failure(command, e.code(), message.trim_end()).print()?;
            }
            eprint!("{message}");
            Ok(Err(ExitCode::from(2)))
        }
    }
}

#[derive(Debug, Serialize)]
struct VersionData {
    version: &'static str,
    commit: &'static str,
    rclone: VersionRclone,
}

#[derive(Debug, Serialize)]
struct VersionRclone {
    found: bool,
    path: Option<String>,
    version: Option<String>,
    supported: bool,
}

/// Short git commit the binary was built from, or `unknown` (set by build.rs).
pub const COMMIT: &str = env!("ONEFOLDER_COMMIT");

/// `onefolder version`: always succeeds, even without rclone.
async fn version(json: bool) -> Result<ExitCode> {
    let version = env!("CARGO_PKG_VERSION");
    let commit = COMMIT;
    let detected = detect::detect().await;
    if json {
        let rclone = match &detected {
            Ok(r) => VersionRclone {
                found: true,
                path: Some(r.path.display().to_string()),
                version: Some(r.version.to_string()),
                supported: true,
            },
            Err(e) => VersionRclone {
                found: e.path().is_some(),
                path: e.path().map(|p| p.display().to_string()),
                version: match e {
                    detect::DetectError::TooOld { found, .. } => Some(found.to_string()),
                    _ => None,
                },
                supported: false,
            },
        };
        Envelope::success(
            "version",
            true,
            VersionData {
                version,
                commit,
                rclone,
            },
        )
        .print()?;
        return Ok(ExitCode::SUCCESS);
    }
    let rclone = match detected {
        Ok(r) => format!("rclone {} at {}", r.version, r.path.display()),
        Err(detect::DetectError::NotFound) => "rclone not found on PATH".to_string(),
        Err(detect::DetectError::TooOld { path, found }) => format!(
            "rclone {found} at {}, older than {} · update it with: brew upgrade rclone",
            path.display(),
            detect::MINIMUM.short()
        ),
        Err(detect::DetectError::Unreadable { path, .. }) => format!(
            "rclone at {}, version unreadable · reinstall it with: brew reinstall rclone",
            path.display()
        ),
    };
    write_stdout(format!("onefolder {version} (commit {commit})\n{rclone}\n").as_bytes())?;
    Ok(ExitCode::SUCCESS)
}

/// `onefolder` with no arguments: the interactive main menu. The session
/// stays on the alternate screen from the menu's first draw until it is
/// left: each item runs, its output is shown on a screen until a key is
/// pressed, and the menu comes back; q or Esc quits. The summary blocks of
/// the session's runs are printed on the normal screen when it ends.
async fn menu(json: bool) -> Result<ExitCode> {
    if json {
        let message = "--json needs a command. Run onefolder --help to see them.";
        Envelope::failure("menu", "needs-command", message).print()?;
        eprintln!("{message}");
        return Ok(ExitCode::from(2));
    }
    let rclone = match require_rclone("menu", json).await? {
        Ok(rclone) => rclone,
        Err(code) => return Ok(code),
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("OneFolder's menu needs a terminal. Run onefolder --help to see the commands.");
        return Ok(ExitCode::from(2));
    }
    let paths = Paths::from_env()?;
    // Ends, putting the terminal back and printing the transcript, when
    // this function returns for any reason.
    let _session = ui::session::start()?;
    let mut cursor = 0;
    loop {
        show_output().await?;
        let status = menu_status(&paths, &rclone).await;
        let (choice, at) = tokio::task::spawn_blocking(move || ui::menu::run(status, cursor))
            .await
            .map_err(|e| anyhow::anyhow!("the menu stopped unexpectedly: {e}"))??;
        cursor = at;
        use ui::menu::Choice;
        if choice == Choice::Quit {
            return Ok(ExitCode::SUCCESS);
        }
        let pause = match menu_item(choice, &paths, &rclone).await {
            Ok(pause) => pause,
            // A command that failed says why on the output screen, and the
            // menu comes back, as it would after any other output.
            Err(e) => {
                ui::err(&format!("{} {e:#}\n", ui::theme::FAILED));
                true
            }
        };
        if pause {
            tokio::task::spawn_blocking(|| {
                ui::pause(&ui::theme::paint(
                    "\nPress any key to go back to the menu.",
                    ui::theme::Tone::Dim,
                    ui::theme::colour_on_stdout(),
                ))
            })
            .await
            .map_err(|e| anyhow::anyhow!("the menu stopped unexpectedly: {e}"))??;
        }
    }
}

/// Runs one menu item. True when it printed output to read before the
/// menu comes back.
async fn menu_item(choice: ui::menu::Choice, paths: &Paths, rclone: &Rclone) -> Result<bool> {
    use ui::menu::Choice;
    Ok(match choice {
        Choice::Quit => false,
        Choice::RunJob => match ConfigFile::load(&paths.config_file()) {
            Ok(file) if !file.config.jobs.is_empty() => match run::pick_job(&file.config).await? {
                Some(name) => {
                    run::run_job(paths, rclone, Some(&name), &run::Options::default()).await?;
                    true
                }
                None => false,
            },
            // Says why there is nothing to run.
            _ => {
                run::run_job(paths, rclone, None, &run::Options::default()).await?;
                true
            }
        },
        Choice::Jobs => {
            jobs::interactive(paths, rclone).await?;
            false
        }
        Choice::Accounts => {
            accounts::interactive(rclone).await?;
            false
        }
        Choice::Status => {
            ui::session::busy("Checking…")?;
            status::run(&status::StatusArgs { job: None }, false).await?;
            true
        }
        Choice::Doctor => {
            ui::session::busy("Checking…")?;
            doctor::run(&doctor::DoctorArgs::default(), false, false).await?;
            true
        }
    })
}

/// Inside the interactive session, shows any output printed since it was
/// last shown (a note, a refusal) before a list or the menu takes the
/// screen back. Does nothing elsewhere.
pub async fn show_output() -> Result<()> {
    tokio::task::spawn_blocking(ui::session::show_page)
        .await
        .map_err(|e| anyhow::anyhow!("the output screen stopped unexpectedly: {e}"))?
}

/// What the menu says about each item, without the network: the config,
/// the state file, rclone's list of remotes, the drives' mount points,
/// the run locks and the logs folder.
async fn menu_status(paths: &Paths, rclone: &Rclone) -> ui::menu::MenuStatus {
    use crate::model::Outcome;
    use crate::state::{lock, prune, State};
    use ui::theme::Tone;
    let none = (String::new(), Tone::Plain);
    let file = match ConfigFile::load(&paths.config_file()) {
        Ok(file) => file,
        Err(_) => {
            let fix = (
                "the config needs attention · see Doctor".to_string(),
                Tone::Error,
            );
            return ui::menu::MenuStatus {
                header: format!("rclone {} · the config does not load", rclone.version),
                items: vec![
                    fix.clone(),
                    fix.clone(),
                    fix,
                    none.clone(),
                    none.clone(),
                    none,
                ],
            };
        }
    };
    let config = &file.config;
    let state = State::load(&paths.state_file()).ok();
    let now = jiff::Timestamp::now().as_second();
    let seconds = |r: &crate::model::RunRecord| ui::jobs::unix_seconds(&r.finished);

    // The newest real run of any job.
    let mut last: Option<(i64, String, Outcome)> = None;
    if let Some(state) = &state {
        for (key, r) in &state.runs {
            if r.dry_run {
                continue;
            }
            let job = key.split('/').next().unwrap_or(key).to_string();
            if let Some(t) = seconds(r) {
                if last.as_ref().is_none_or(|(l, _, _)| t > *l) {
                    last = Some((t, job, r.outcome));
                }
            }
        }
    }
    let mut header = format!(
        "rclone {} · {} · {}",
        rclone.version,
        plural(config.accounts.len(), "account"),
        plural(config.jobs.len(), "job")
    );
    if let Some((t, _, _)) = &last {
        header.push_str(&format!(" · last run {}", ui::jobs::ago(*t, now)));
    }

    let run_item = match (&last, config.jobs.is_empty()) {
        (_, true) => ("no jobs yet · add one in Jobs".to_string(), Tone::Plain),
        (None, false) => ("nothing has run yet".to_string(), Tone::Plain),
        (Some((t, job, outcome)), false) => {
            let (glyph, tone) = match outcome {
                Outcome::Ok => (ui::theme::DONE, Tone::Plain),
                Outcome::Partial | Outcome::Cancelled => (ui::theme::SKIPPED, Tone::Warning),
                Outcome::Failed | Outcome::NeedsResync => (ui::theme::FAILED, Tone::Error),
            };
            (
                format!("last: {job} {glyph} {}", ui::jobs::ago(*t, now)),
                tone,
            )
        }
    };

    let dests: usize = config.jobs.iter().map(|j| j.destinations.len()).sum();
    let states: Vec<Option<bool>> = config
        .jobs
        .iter()
        .flat_map(|j| &j.destinations)
        .filter_map(|d| d.identity())
        .map(|id| crate::volume::mount_state(&id))
        .collect();
    let unplugged = states.iter().filter(|s| **s == Some(false)).count();
    let hanging = states.iter().filter(|s| s.is_none()).count();
    let mut jobs_item = (
        format!(
            "{} · {}",
            plural(config.jobs.len(), "job"),
            plural(dests, "destination")
        ),
        Tone::Plain,
    );
    let drives = |n: usize| {
        if n == 1 {
            "1 drive".to_string()
        } else {
            format!("{n} drives")
        }
    };
    if unplugged > 0 {
        jobs_item
            .0
            .push_str(&format!(" · {} not mounted", drives(unplugged)));
        jobs_item.1 = Tone::Warning;
    }
    if hanging > 0 {
        jobs_item
            .0
            .push_str(&format!(" · {} not answering", drives(hanging)));
        jobs_item.1 = Tone::Warning;
    }

    let accounts_item = if config.accounts.is_empty() {
        ("none yet · add one here".to_string(), Tone::Plain)
    } else {
        let n = plural(config.accounts.len(), "account");
        match crate::rclone::remotes::list(rclone).await {
            Ok(names) => {
                let missing = config
                    .accounts
                    .iter()
                    .filter(|a| !names.contains(&a.remote))
                    .count();
                if missing == 0 {
                    (format!("{n} · all in rclone's config"), Tone::Plain)
                } else {
                    (
                        format!("{n} · {missing} missing from rclone's config, reconnect it"),
                        Tone::Error,
                    )
                }
            }
            Err(_) => (format!("{n} · remotes not checked"), Tone::Warning),
        }
    };

    let mut status_item = match &state {
        None => (
            "the state file does not load · see Doctor".to_string(),
            Tone::Error,
        ),
        Some(state) => {
            let attention: Vec<String> = config
                .jobs
                .iter()
                .filter(|j| {
                    j.destinations.iter().any(|d| {
                        state
                            .last_run(&j.name, d.id())
                            .is_some_and(|r| r.outcome != Outcome::Ok)
                    })
                })
                .map(|j| j.name.clone())
                .collect();
            if attention.is_empty() {
                if state.runs.is_empty() {
                    ("no runs recorded yet".to_string(), Tone::Plain)
                } else {
                    ("every last run ok".to_string(), Tone::Plain)
                }
            } else {
                (
                    format!("needs a look: {}", attention.join(", ")),
                    Tone::Warning,
                )
            }
        }
    };
    let mut running = Vec::new();
    for job in &config.jobs {
        if lock::inspect(paths, &job.name)
            .await
            .is_some_and(|l| l.is_running())
        {
            running.push(job.name.clone());
        }
    }
    if !running.is_empty() {
        status_item = (format!("running now: {}", running.join(", ")), Tone::Accent);
    }

    let usage = prune::usage(
        paths,
        &state.clone().unwrap_or_default(),
        &prune::Known::from_config(config),
        now,
    );
    let mut doctor_item = (
        format!(
            "checks rclone, accounts and drives · {} log{}, {}",
            usage.files,
            if usage.files == 1 { "" } else { "s" },
            ui::summary::format_bytes(usage.bytes)
        ),
        Tone::Plain,
    );
    if usage.prunable > 0 && state.is_some() {
        doctor_item
            .0
            .push_str(&format!(" · {} to prune", usage.prunable));
    }

    ui::menu::MenuStatus {
        header,
        items: vec![
            run_item,
            jobs_item,
            accounts_item,
            status_item,
            doctor_item,
            none,
        ],
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_run_flags() {
        let cli = Cli::try_parse_from([
            "onefolder",
            "run",
            "documents",
            "--dry-run",
            "--yes",
            "--json",
            "--folder-deletes=keep",
            "--dest",
            "d-7f3a",
        ])
        .unwrap();
        assert!(cli.json && cli.dry_run);
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.job.as_deref(), Some("documents"));
        assert!(args.yes);
        assert_eq!(args.folder_deletes, Some(run::FolderDeletes::Keep));
    }

    #[test]
    fn rejects_unknown_folder_deletes() {
        assert!(Cli::try_parse_from(["onefolder", "run", "--folder-deletes=maybe"]).is_err());
    }
}
