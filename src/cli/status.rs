//! `onefolder status [job] [--json]`: the last run of each destination:
//! when, the outcome, its counts and log, folders on hold, what the next
//! two-way run will do first, the drive's state for a local destination
//! and the account's remote for a OneDrive one.
//!
//! Nothing here touches the network or runs rclone transfers: drives get
//! the volume check (`statfs` and `diskutil`, as `doctor` does), and
//! accounts are looked up in rclone's config with `rclone listremotes`
//! (`accounts test` checks the connection). Works without rclone, saying
//! the remotes were not checked.
//!
//! Exit codes: 0 whatever the runs' outcomes; 2 when the config does not
//! load or names no such job.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use clap::Args;

use crate::config::{ConfigFile, Paths};
use crate::jsonout::status::{DestinationStatus, JobStatus, Pending, Report, VolumeStatus};
use crate::jsonout::Envelope;
use crate::model::{Destination, Job, Mode, Outcome};
use crate::rclone::{detect, remotes};
use crate::state::State;
use crate::ui;
use crate::ui::theme;
use crate::volume::{self, Identity, Severity, VolumeError};

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Show only this job
    pub job: Option<String>,
}

pub async fn run(args: &StatusArgs, json: bool) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let file = match ConfigFile::load(&paths.config_file()) {
        Ok(file) => file,
        Err(e) => return Ok(fail(json, "config-invalid", &e.to_string())),
    };
    let jobs: Vec<&Job> = match &args.job {
        Some(name) => match file.config.job(name) {
            Some(job) => vec![job],
            None => {
                return Ok(fail(
                    json,
                    "unknown-job",
                    &format!("There is no job called {name}. See onefolder jobs list."),
                ))
            }
        },
        None => file.config.jobs.iter().collect(),
    };
    // A state file that does not load: still worth showing the jobs, and
    // saying why their runs are missing.
    let (state, state_error) = match State::load(&paths.state_file()) {
        Ok(state) => (state, None),
        Err(e) => (State::default(), Some(format!("{e:#}"))),
    };
    // The remotes rclone knows, if rclone is here and answers; no network.
    let remotes = match detect::detect().await {
        Ok(rclone) => remotes::list(&rclone).await.ok(),
        Err(_) => None,
    };
    let mut report = Report {
        state_error,
        jobs: Vec::new(),
    };
    for job in jobs {
        report.jobs.push(
            job_status(
                &paths,
                job,
                &state,
                remotes.as_deref(),
                &file.config.accounts,
            )
            .await,
        );
    }
    if json {
        let ok = report.state_error.is_none();
        Envelope::success("status", ok, &report).print()?;
    } else {
        let text = ui::status::render_text(
            &report,
            now(),
            theme::colour_on_stdout(),
            ui::terminal_width(),
            &|p| crate::config::tilde(Path::new(p)),
        );
        crate::ui::out(&text)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn fail(json: bool, code: &str, message: &str) -> ExitCode {
    if json {
        // Printing can only fail if stdout is gone; the message still goes to stderr.
        let _ = Envelope::failure("status", code, message).print();
    }
    crate::ui::err(&format!("{} {message}\n", theme::FAILED));
    ExitCode::from(2)
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn severity_word(s: Severity) -> &'static str {
    match s {
        Severity::Ok => "ok",
        Severity::Warning => "warning",
        Severity::Problem => "problem",
        Severity::Skipped => "skipped",
    }
}

/// The volume check of a recorded drive, in doctor's words.
async fn drive_status(path: &Path, identity: &Identity, needs_write: bool) -> VolumeStatus {
    let result = volume::check(path, identity, needs_write).await;
    let (severity, message) = volume::state_line(&result, identity);
    let elsewhere = match &result {
        Err(VolumeError::NotMounted { elsewhere, .. }) => elsewhere.clone(),
        _ => None,
    };
    VolumeStatus {
        state: match &result {
            Ok(()) => "mounted".to_string(),
            Err(e) => e.code().to_string(),
        },
        severity: severity_word(severity).to_string(),
        message,
        mount_point: Some(identity.mount_point.clone()),
        elsewhere,
    }
}

/// What the next two-way run of a destination does first, if anything
/// unusual, from the state file alone.
pub fn pending(job: &Job, dest: &Destination, state: &State) -> Option<Pending> {
    if dest.mode() != Mode::TwoWay {
        return None;
    }
    let key = State::key(&job.name, dest.id());
    let bisync = state.bisync.get(&key);
    let last = state.last_run(&job.name, dest.id());
    if let Some(detail) = bisync.and_then(|b| b.needs_resync.clone()) {
        return Some(Pending {
            state: "needs-resync".into(),
            detail: Some(detail),
        });
    }
    if last.is_some_and(|r| r.outcome == Outcome::NeedsResync) {
        return Some(Pending {
            state: "needs-resync".into(),
            detail: last.and_then(|r| r.reason.clone()),
        });
    }
    match bisync {
        Some(b) if b.initialised && !b.matches(&job.source, &dest.display()) => {
            return Some(Pending {
                state: "pair-changed".into(),
                detail: Some(
                    "the source or the destination changed since the last two-way run".into(),
                ),
            })
        }
        Some(b) if b.initialised => {}
        _ => {
            return Some(Pending {
                state: "first-run".into(),
                detail: None,
            })
        }
    }
    if let Some(r) = last.filter(|r| r.mode == Mode::TwoWay && r.outcome == Outcome::Cancelled) {
        return Some(Pending {
            state: "recovering".into(),
            detail: Some(r.started.clone()),
        });
    }
    None
}

async fn job_status(
    paths: &Paths,
    job: &Job,
    state: &State,
    remotes: Option<&[String]>,
    accounts: &[crate::model::Account],
) -> JobStatus {
    let source_volume = match job.source_identity() {
        Some(identity) => Some(drive_status(Path::new(&job.source), &identity, false).await),
        None => None,
    };
    let mut destinations = Vec::new();
    for d in &job.destinations {
        let last_run = state.last_run(&job.name, d.id()).cloned();
        let last_dry_run = state.last_dry_run(&job.name, d.id()).cloned();
        let seconds = |r: &crate::model::RunRecord| ui::jobs::unix_seconds(&r.finished);
        let dry_run_newer = match (&last_dry_run, &last_run) {
            (Some(_), None) => true,
            (Some(dry), Some(real)) => match (seconds(dry), seconds(real)) {
                (Some(a), Some(b)) => a > b,
                (Some(_), None) => true,
                _ => false,
            },
            (None, _) => false,
        };
        let (account, remote, remote_exists, volume) = match d {
            Destination::Onedrive { account, .. } => {
                let remote = accounts
                    .iter()
                    .find(|a| &a.label == account)
                    .map(|a| a.remote.clone());
                let exists = match (&remote, remotes) {
                    (Some(r), Some(list)) => Some(list.iter().any(|x| x == r)),
                    _ => None,
                };
                (Some(account.clone()), remote, exists, None)
            }
            Destination::Local { path, .. } => {
                let p = Path::new(path);
                let volume = match d.identity() {
                    Some(identity) => drive_status(p, &identity, true).await,
                    None => {
                        let (severity, message, code) = volume::folder_line(p);
                        VolumeStatus {
                            state: code.to_string(),
                            severity: severity_word(severity).to_string(),
                            message,
                            mount_point: None,
                            elsewhere: None,
                        }
                    }
                };
                (None, None, None, Some(volume))
            }
        };
        let log = last_run.as_ref().filter(|r| !r.log.is_empty()).map(|r| {
            let p = Path::new(&r.log);
            if p.is_absolute() {
                p.display().to_string()
            } else {
                paths.data_dir.join(p).display().to_string()
            }
        });
        destinations.push(DestinationStatus {
            id: d.id().to_string(),
            kind: d.type_name().to_string(),
            display: d.display(),
            path: d.path().to_string(),
            mode: d.mode(),
            account,
            remote,
            remote_exists,
            volume,
            last_run,
            last_dry_run,
            dry_run_newer,
            log,
            holds: state.holds_of(&job.name, d.id()),
            pending: pending(job, d, state),
        });
    }
    JobStatus {
        name: job.name.clone(),
        source: job.source.clone(),
        source_volume,
        run_lock: crate::state::lock::inspect(paths, &job.name).await,
        destinations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RunRecord, SideCounts};
    use crate::state::BisyncState;

    fn record(outcome: Outcome, mode: Mode) -> RunRecord {
        RunRecord {
            started: "2026-10-13T09:41:02+11:00".into(),
            finished: "2026-10-13T09:43:40+11:00".into(),
            mode,
            dry_run: false,
            outcome,
            added: SideCounts::default(),
            updated: SideCounts::default(),
            deleted: SideCounts::default(),
            bytes: 0,
            errors: 0,
            log: String::new(),
            rclone_version: "1.75.2".into(),
            reason: None,
        }
    }

    #[test]
    fn pending_states() {
        let mut job = Job::new("documents", "/Users/bora/Documents");
        let two =
            Destination::onedrive("d-7f3a".into(), "work".into(), "Docs".into(), Mode::TwoWay);
        let one =
            Destination::onedrive("d-0001".into(), "work".into(), "Docs".into(), Mode::OneWay);
        job.destinations = vec![two.clone(), one.clone()];
        let mut state = State::default();
        assert_eq!(pending(&job, &one, &state), None, "one-way has none");
        assert_eq!(pending(&job, &two, &state).unwrap().state, "first-run");
        state.bisync.insert(
            "documents/d-7f3a".into(),
            BisyncState {
                initialised: true,
                source: Some(job.source.clone()),
                destination: Some(two.display()),
                ..BisyncState::default()
            },
        );
        assert_eq!(pending(&job, &two, &state), None);
        state.record_run(
            "documents",
            "d-7f3a",
            record(Outcome::Cancelled, Mode::TwoWay),
        );
        assert_eq!(pending(&job, &two, &state).unwrap().state, "recovering");
        state
            .bisync
            .get_mut("documents/d-7f3a")
            .unwrap()
            .destination = Some("work:Other".into());
        assert_eq!(pending(&job, &two, &state).unwrap().state, "pair-changed");
        state
            .bisync
            .get_mut("documents/d-7f3a")
            .unwrap()
            .needs_resync = Some("bisync stopped on a critical error".into());
        let p = pending(&job, &two, &state).unwrap();
        assert_eq!(p.state, "needs-resync");
        assert_eq!(
            p.detail.as_deref(),
            Some("bisync stopped on a critical error")
        );
    }
}
