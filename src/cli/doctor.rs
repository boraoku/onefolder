//! `onefolder doctor`: checks rclone, the config, the jobs' sources and the
//! data directory, and says what needs attention.

use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use super::accounts;
use crate::config::{self, ConfigFile, Paths};
use crate::jsonout::accounts::{Health, TestEntry};
use crate::jsonout::Envelope;
use crate::model::Destination;
use crate::rclone::detect::{self, DetectError, Rclone};
use crate::rclone::remotes;
use crate::state::{lock, prune, State};
use crate::ui::summary::format_bytes;
use crate::ui::theme::{self, Tone};
use crate::ui::Segment;
use crate::volume::{self, Identity, VolumeError};

#[derive(Debug, Clone, Default, Args)]
pub struct DoctorArgs {
    /// Remove run logs older than 90 days or beyond the newest 50 of each
    /// destination (the last run's log always stays); with --dry-run, list them
    #[arg(long)]
    pub prune_logs: bool,
}

/// The result of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Ok,
    /// Worth a look, but nothing is broken.
    Warning,
    /// Needs fixing before OneFolder can work properly.
    Problem,
    /// Not checked, with a reason.
    Skipped,
}

impl Status {
    fn glyph(self) -> (&'static str, Tone) {
        match self {
            Status::Ok => (theme::DONE, Tone::Success),
            Status::Warning => (theme::SKIPPED, Tone::Warning),
            Status::Problem => (theme::FAILED, Tone::Error),
            Status::Skipped => (theme::SKIPPED, Tone::Dim),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RcloneReport {
    pub status: Status,
    pub path: Option<String>,
    pub version: Option<String>,
    pub minimum: String,
    pub message: Option<String>,
    /// The newest rclone Homebrew offers, when `brew` is on PATH and
    /// answered within 5 seconds.
    pub latest: Option<String>,
    /// A recommendation (never a warning): a newer rclone is available.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountReport {
    pub status: Status,
    pub label: String,
    pub drive_type: String,
    pub remote: String,
    pub message: String,
    /// `connected`, `token-expired`, `remote-missing`, `timeout` or `failed`;
    /// null when the account was not checked.
    pub health: Option<Health>,
    /// Free space reported by OneDrive, when connected.
    pub free: Option<u64>,
    /// The command that fixes the problem, when there is one.
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DestinationReport {
    pub status: Status,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub target: String,
    pub mode: String,
    pub message: String,
    /// Local destinations with a recorded volume: `mounted`, or why not
    /// (`not-mounted`, `wrong-volume`, `read-only`, `missing`, ...).
    pub volume: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobReport {
    pub status: Status,
    pub name: String,
    pub source: String,
    pub message: String,
    /// For a source on an external drive: `mounted`, or why not, with the
    /// same codes as a destination's `volume` (and `status`'s
    /// `source_volume.state`); null when no drive is recorded.
    pub source_volume: Option<String>,
    /// The job's run lock when there is one: a run in progress or a stale
    /// lock (the shape of `status`'s `run_lock`).
    pub run_lock: Option<lock::Seen>,
    pub destinations: Vec<DestinationReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigReport {
    pub status: Status,
    pub path: String,
    pub exists: bool,
    pub loads: bool,
    pub version: Option<i64>,
    pub accounts: usize,
    pub jobs: usize,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathReport {
    pub status: Status,
    pub path: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogsReport {
    pub status: Status,
    pub path: String,
    /// Run logs.
    pub files: u64,
    /// Everything under the logs folder.
    pub bytes: u64,
    /// RFC 3339 times of the oldest and the newest log.
    pub oldest: Option<String>,
    pub newest: Option<String>,
    /// Logs `doctor --prune-logs` would remove now.
    pub prunable: u64,
}

/// Everything doctor found. Printed as text, or as the `data` of the JSON envelope.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub rclone: RcloneReport,
    pub config: ConfigReport,
    /// Empty when the config does not load.
    pub accounts: Vec<AccountReport>,
    /// Empty when the config does not load.
    pub jobs: Vec<JobReport>,
    pub state: PathReport,
    pub data_dir: PathReport,
    pub logs: LogsReport,
    pub problems: usize,
    pub warnings: usize,
}

pub async fn run(args: &DoctorArgs, json: bool, dry_run: bool) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    if args.prune_logs {
        return prune_logs(&paths, json, dry_run);
    }
    let rclone = detect::detect().await;
    let mut report = gather(&paths, &rclone).await;
    check_accounts(&mut report, &paths, &rclone).await;
    check_latest(&mut report.rclone, &rclone).await;
    if json {
        Envelope::success("doctor", report.problems == 0, &report).print()?;
    } else {
        let text = render(
            &report,
            theme::colour_on_stdout(),
            crate::ui::terminal_width(),
        );
        crate::ui::out(&text)?;
    }
    Ok(if report.problems == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Runs every check. Only reads, except that the data directory is created
/// if missing and probed with a short-lived file.
pub async fn gather(paths: &Paths, rclone: &Result<Rclone, DetectError>) -> Report {
    let rclone = rclone_report(rclone);

    let config_path = paths.config_file();
    let loaded = ConfigFile::load(&config_path);
    let config = match &loaded {
        Ok(file) => ConfigReport {
            status: if file.existed {
                Status::Ok
            } else {
                Status::Skipped
            },
            path: config_path.display().to_string(),
            exists: file.existed,
            loads: true,
            version: Some(file.config.version),
            accounts: file.config.accounts.len(),
            jobs: file.config.jobs.len(),
            message: (!file.existed)
                .then(|| "not created yet, it appears when you add an account or a job".into()),
        },
        Err(e) => ConfigReport {
            status: Status::Problem,
            path: config_path.display().to_string(),
            exists: config_path.exists(),
            loads: false,
            version: None,
            accounts: 0,
            jobs: 0,
            message: Some(e.detail()),
        },
    };

    let mut jobs = Vec::new();
    if let Ok(file) = &loaded {
        for job in &file.config.jobs {
            jobs.push(job_report(paths, job).await);
        }
    }
    let accounts = match &loaded {
        Ok(file) => file
            .config
            .accounts
            .iter()
            .map(|a| AccountReport {
                status: Status::Skipped,
                label: a.label.clone(),
                drive_type: a.drive_type.as_str().into(),
                remote: a.remote.clone(),
                message: "connection not checked".into(),
                health: None,
                free: None,
                hint: None,
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    let state_path = paths.state_file();
    let loaded_state = State::load(&state_path);
    let state = match &loaded_state {
        Ok(_) if state_path.exists() => PathReport {
            status: Status::Ok,
            path: state_path.display().to_string(),
            message: None,
        },
        Ok(_) => PathReport {
            status: Status::Skipped,
            path: state_path.display().to_string(),
            message: Some("not created yet, it is written after the first run".into()),
        },
        Err(e) => PathReport {
            status: Status::Problem,
            path: state_path.display().to_string(),
            message: Some(format!("{e:#}")),
        },
    };

    let data_dir = match probe_writable(&paths.data_dir) {
        Ok(()) => PathReport {
            status: Status::Ok,
            path: paths.data_dir.display().to_string(),
            message: None,
        },
        Err(e) => PathReport {
            status: Status::Problem,
            path: paths.data_dir.display().to_string(),
            message: Some(format!("not writable: {e}")),
        },
    };

    // Logs a run still points at are never counted as prunable; with an
    // unreadable state or config none would be safe to remove, so none
    // are offered.
    let known = match &loaded {
        Ok(file) => prune::Known::from_config(&file.config),
        Err(_) => prune::Known::default(),
    };
    let usage = match &loaded_state {
        Ok(state) => prune::usage(paths, state, &known, now()),
        Err(_) => prune::Usage {
            prunable: 0,
            ..prune::usage(paths, &State::default(), &prune::Known::default(), now())
        },
    };
    let logs = LogsReport {
        status: Status::Ok,
        path: paths.logs_dir().display().to_string(),
        files: usage.files,
        bytes: usage.bytes,
        oldest: usage.oldest,
        newest: usage.newest,
        prunable: usage.prunable,
    };

    let mut report = Report {
        rclone,
        config,
        accounts,
        jobs,
        state,
        data_dir,
        logs,
        problems: 0,
        warnings: 0,
    };
    count(&mut report);
    report
}

/// Checks every account with `rclone about`, all at once, each with a
/// 10 second limit, and fills in the Accounts block. Without a usable
/// rclone the accounts stay "not checked".
pub async fn check_accounts(
    report: &mut Report,
    paths: &Paths,
    rclone: &Result<Rclone, DetectError>,
) {
    let Ok(rclone) = rclone else {
        for a in &mut report.accounts {
            a.message = "not checked, rclone is not available".into();
        }
        return;
    };
    let Ok(file) = ConfigFile::load(&paths.config_file()) else {
        return;
    };
    let results = accounts::check_all(rclone, &file.config.accounts, remotes::ABOUT_TIMEOUT).await;
    for a in &mut report.accounts {
        if let Some(result) = results.iter().find(|t| t.label == a.label) {
            apply_check(a, result);
        }
    }
    count(report);
}

/// Turns one `accounts test` result into the doctor's account line.
fn apply_check(a: &mut AccountReport, t: &TestEntry) {
    a.health = Some(t.status);
    a.hint.clone_from(&t.hint);
    a.free = t.quota.and_then(|q| q.free);
    let dot = theme::DOT;
    (a.status, a.message) = match t.status {
        Health::Connected => (
            Status::Ok,
            match a.free {
                Some(free) => format!("token ok   {} free", format_bytes(free)),
                None => "token ok".into(),
            },
        ),
        Health::TokenExpired => (
            Status::Problem,
            format!(
                "token expired {dot} run: onefolder accounts reconnect {}",
                a.label
            ),
        ),
        Health::RemoteMissing => (
            Status::Problem,
            format!(
                "remote {} missing from rclone's config {dot} run: onefolder accounts reconnect {}",
                a.remote, a.label
            ),
        ),
        Health::Timeout => (
            Status::Warning,
            format!(
                "no answer within {} seconds {dot} check your network connection",
                remotes::ABOUT_TIMEOUT.as_secs()
            ),
        ),
        Health::Failed => (
            Status::Problem,
            format!(
                "not reachable: {}",
                t.message.as_deref().unwrap_or("rclone gave no reason")
            ),
        ),
    };
}

fn rclone_report(rclone: &Result<Rclone, DetectError>) -> RcloneReport {
    let minimum = detect::MINIMUM.to_string();
    match rclone {
        Ok(r) => RcloneReport {
            status: Status::Ok,
            path: Some(r.path.display().to_string()),
            version: Some(r.version.to_string()),
            minimum,
            message: None,
            latest: None,
            note: None,
        },
        Err(e) => RcloneReport {
            status: Status::Problem,
            path: e.path().map(|p| p.display().to_string()),
            version: match e {
                DetectError::TooOld { found, .. } => Some(found.to_string()),
                _ => None,
            },
            minimum,
            message: Some(match e {
                DetectError::NotFound => format!(
                    "not found on PATH · {}",
                    crate::platform::rclone_advice(crate::platform::RcloneAction::Install)
                ),
                DetectError::TooOld { .. } => format!(
                    "too old · {}",
                    crate::platform::rclone_advice(crate::platform::RcloneAction::Upgrade)
                ),
                DetectError::Unreadable { detail, .. } => {
                    format!("could not read its version: {detail}")
                }
            }),
            latest: None,
            note: None,
        },
    }
}

/// Asks Homebrew for its newest rclone, and notes when the one in use is
/// older. Never a warning: an older rclone that meets the minimum works.
/// Without `brew`, offline, or slow, nothing is said.
pub async fn check_latest(report: &mut RcloneReport, rclone: &Result<Rclone, DetectError>) {
    let found = match rclone {
        Ok(r) => r.version,
        Err(DetectError::TooOld { found, .. }) => *found,
        Err(_) => return,
    };
    let Some(latest) = detect::latest_from_homebrew().await else {
        return;
    };
    report.latest = Some(latest.to_string());
    report.note = latest_note(found, latest);
}

/// The note for an rclone older than Homebrew's.
pub fn latest_note(found: detect::Version, latest: detect::Version) -> Option<String> {
    (latest > found)
        .then(|| format!("rclone {latest} is available · update with: brew upgrade rclone"))
}

async fn job_report(paths: &Paths, job: &crate::model::Job) -> JobReport {
    let (mut status, mut message) = match fs::metadata(&job.source) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Status::Problem, "source missing".into()),
        Err(e) => (Status::Problem, format!("source not readable: {e}")),
        Ok(m) if !m.is_dir() => (Status::Problem, "source is not a folder".into()),
        Ok(_) => match fs::read_dir(&job.source) {
            Ok(_) if job.destinations.is_empty() => {
                (Status::Warning, "source ok · no destinations yet".into())
            }
            Ok(_) => (Status::Ok, "source ok".into()),
            Err(e) => (Status::Problem, format!("source not readable: {e}")),
        },
    };
    // A source on an external drive gets the same volume check, for reading.
    let mut source_volume = None;
    if let Some(identity) = job.source_identity() {
        match volume::check(Path::new(&job.source), &identity, false).await {
            Ok(()) => source_volume = Some("mounted".to_string()),
            Err(e) => {
                let (s, m) = volume_status(&e, &identity);
                status = s;
                message = format!("source {m}");
                source_volume = Some(e.code().to_string());
            }
        }
    }
    // A stale run lock is worth a look; a run in progress is not a fault.
    let run_lock = lock::inspect(paths, &job.name).await;
    if run_lock.as_ref().is_some_and(|l| !l.is_running()) && status == Status::Ok {
        status = Status::Warning;
    }
    let mut destinations = Vec::new();
    for d in &job.destinations {
        let (status, message, volume) = match d {
            Destination::Onedrive { .. } => (Status::Skipped, "not checked yet".to_string(), None),
            Destination::Local { path, .. } => local_destination(path, d.identity()).await,
        };
        destinations.push(DestinationReport {
            status,
            id: d.id().into(),
            kind: d.type_name().into(),
            target: d.display(),
            mode: d.mode().as_str().into(),
            message,
            volume,
        });
    }
    JobReport {
        status,
        name: job.name.clone(),
        source: job.source.clone(),
        message,
        source_volume,
        run_lock,
        destinations,
    }
}

/// The volume check for a local destination, or a plain folder check when
/// no volume was recorded.
async fn local_destination(
    path: &str,
    identity: Option<Identity>,
) -> (Status, String, Option<String>) {
    let Some(identity) = identity else {
        let (severity, message, _) = volume::folder_line(Path::new(path));
        return (status_of(severity), message, None);
    };
    let result = volume::check(Path::new(path), &identity, true).await;
    let (severity, message) = volume::state_line(&result, &identity);
    let code = match &result {
        Ok(()) => "mounted",
        Err(e) => e.code(),
    };
    (status_of(severity), message, Some(code.to_string()))
}

/// Doctor's word for a failed volume check, shared with `status`: not
/// mounted is a warning (the drive is simply unplugged), anything else is a
/// problem.
fn volume_status(e: &VolumeError, expected: &Identity) -> (Status, String) {
    let (severity, message) = volume::state_line(&Err(e.clone()), expected);
    (status_of(severity), message)
}

pub(super) fn status_of(severity: volume::Severity) -> Status {
    match severity {
        volume::Severity::Ok => Status::Ok,
        volume::Severity::Warning => Status::Warning,
        volume::Severity::Problem => Status::Problem,
        volume::Severity::Skipped => Status::Skipped,
    }
}

fn count(report: &mut Report) {
    let mut statuses = vec![
        report.rclone.status,
        report.config.status,
        report.state.status,
        report.data_dir.status,
        report.logs.status,
    ];
    statuses.extend(report.accounts.iter().map(|a| a.status));
    for job in &report.jobs {
        statuses.push(job.status);
        statuses.extend(job.destinations.iter().map(|d| d.status));
    }
    report.problems = statuses.iter().filter(|s| **s == Status::Problem).count();
    report.warnings = statuses.iter().filter(|s| **s == Status::Warning).count();
}

/// Creates the directory if needed, then writes and removes a small file in it.
fn probe_writable(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".doctor-probe-{}", std::process::id()));
    fs::write(&probe, b"ok")?;
    fs::remove_file(&probe)
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// `13 Oct 2026` from an RFC 3339 time.
fn day(rfc3339: &str) -> String {
    crate::ui::accounts::format_added(Some(rfc3339))
}

/// The lines of the report, in the shape of the doctor mockup. With a
/// `width`, paths are shortened in the middle and long lines wrapped so
/// every line fits.
pub fn lines(report: &Report, width: Option<usize>) -> Vec<Vec<Segment>> {
    let tilde = |path: &str| config::tilde(Path::new(path));
    let mark = |status: Status| {
        let (glyph, tone) = status.glyph();
        (glyph.to_string(), tone)
    };
    let fit = |text: String, room: usize| match width {
        Some(w) => crate::ui::shorten_path(&text, w.saturating_sub(room).max(16)),
        None => text,
    };
    let mut out: Vec<Vec<Segment>> = vec![vec![("Doctor".into(), Tone::Bold)]];
    let section = |out: &mut Vec<Vec<Segment>>, title: &str| {
        out.push(Vec::new());
        out.push(vec![
            (theme::CURSOR.into(), Tone::Accent),
            (format!(" {title}"), Tone::Bold),
        ]);
    };
    let row = |status: Status, text: String, tone: Tone| {
        vec![
            ("  ".to_string(), Tone::Plain),
            mark(status),
            (format!(" {text}"), tone),
        ]
    };

    section(&mut out, "rclone");
    let r = &report.rclone;
    let min = format!("(minimum {})", detect::MINIMUM.short());
    let line = match (&r.path, &r.version, &r.message) {
        (Some(path), Some(version), None) => format!("{} · {version} {min}", tilde(path)),
        (Some(path), Some(version), Some(msg)) => {
            format!("{} · {version} {min} · {msg}", tilde(path))
        }
        (Some(path), None, Some(msg)) => format!("{} · {msg}", tilde(path)),
        (_, _, Some(msg)) => msg.clone(),
        (_, _, None) => String::new(),
    };
    out.push(row(r.status, line, Tone::Plain));
    if let Some(note) = &r.note {
        out.push(vec![(format!("      {note}"), Tone::Dim)]);
    }

    let config_loads = report.config.loads;

    section(&mut out, "Accounts");
    if !config_loads {
        out.push(row(
            Status::Skipped,
            "not checked, the config does not load".into(),
            Tone::Plain,
        ));
    } else if report.accounts.is_empty() {
        out.push(row(
            Status::Skipped,
            "none yet · add one with: onefolder accounts add".into(),
            Tone::Plain,
        ));
    }
    let label_w = report
        .accounts
        .iter()
        .map(|a| crate::ui::width(&a.label))
        .max()
        .unwrap_or(0)
        .max(9)
        + 3;
    for a in &report.accounts {
        out.push(vec![
            ("  ".to_string(), Tone::Plain),
            mark(a.status),
            (
                format!(" {}{:<11}", crate::ui::pad(&a.label, label_w), a.drive_type),
                Tone::Plain,
            ),
            (a.message.clone(), Tone::Dim),
        ]);
    }

    section(&mut out, "Jobs");
    if !config_loads {
        out.push(row(
            Status::Skipped,
            "not checked, the config does not load".into(),
            Tone::Plain,
        ));
    } else if report.jobs.is_empty() {
        out.push(row(
            Status::Skipped,
            "none yet · add one with: onefolder jobs add".into(),
            Tone::Plain,
        ));
    }
    let name_w = report
        .jobs
        .iter()
        .map(|j| crate::ui::width(&j.name))
        .max()
        .unwrap_or(0)
        .max(9)
        + 3;
    // One destination column for all jobs, so every status lines up;
    // narrower than the terminal, leaving room for a short state.
    let natural = report
        .jobs
        .iter()
        .flat_map(|j| &j.destinations)
        .map(|d| crate::ui::width(&d.target))
        .max()
        .unwrap_or(0);
    let target_w = width.map_or(natural, |w| natural.min(w.saturating_sub(8 + 20).max(16)));
    for j in &report.jobs {
        out.push(vec![
            ("  ".to_string(), Tone::Plain),
            mark(j.status),
            (
                format!(" {}{}", crate::ui::pad(&j.name, name_w), j.message),
                Tone::Plain,
            ),
        ]);
        if let Some(l) = &j.run_lock {
            let (status, tone) = if l.is_running() {
                (Status::Ok, Tone::Plain)
            } else {
                (Status::Warning, Tone::Warning)
            };
            out.push(vec![
                ("      ".to_string(), Tone::Plain),
                mark(status),
                (format!(" {}", l.words(crate::ui::status::absolute)), tone),
            ]);
        }
        for d in &j.destinations {
            let target = match width {
                Some(_) => crate::ui::shorten_path(&d.target, target_w),
                None => d.target.clone(),
            };
            out.push(vec![
                ("      ".to_string(), Tone::Plain),
                mark(d.status),
                (
                    format!(" {}", crate::ui::pad(&target, target_w + 4)),
                    Tone::Plain,
                ),
                (d.message.clone(), Tone::Dim),
            ]);
        }
    }

    section(&mut out, "Data");
    let c = &report.config;
    let detail = match (c.loads, &c.message) {
        (true, None) => format!(
            "(version {} · {} · {})",
            c.version.unwrap_or(config::CONFIG_VERSION),
            plural(c.accounts, "account"),
            plural(c.jobs, "job")
        ),
        (true, Some(msg)) => msg.clone(),
        (false, _) => "does not load".into(),
    };
    out.push(row(
        c.status,
        format!("config {} {detail}", fit(tilde(&c.path), 14)),
        Tone::Plain,
    ));
    if let (false, Some(msg)) = (c.loads, &c.message) {
        for line in msg.lines() {
            out.push(vec![(format!("      {line}"), Tone::Plain)]);
        }
    }
    let st = &report.state;
    let mut text = format!("state {}", fit(tilde(&st.path), 12));
    if let Some(msg) = &st.message {
        text.push_str(&format!(" {msg}"));
    }
    out.push(row(st.status, text, Tone::Plain));
    let d = &report.data_dir;
    out.push(row(
        d.status,
        format!(
            "data {} {}",
            fit(tilde(&d.path), 20),
            d.message.as_deref().unwrap_or("writable")
        ),
        Tone::Plain,
    ));
    let l = &report.logs;
    let files = if l.files == 1 { "log" } else { "logs" };
    let mut text = format!("logs {} {files}, {}", l.files, format_bytes(l.bytes));
    match (&l.oldest, &l.newest) {
        (Some(a), Some(b)) if day(a) == day(b) => text.push_str(&format!(" · {}", day(a))),
        (Some(a), Some(b)) => text.push_str(&format!(" · {} to {}", day(a), day(b))),
        _ => {}
    }
    if l.prunable > 0 {
        text.push_str(&format!(
            " · onefolder doctor --prune-logs removes {} (older than {} days, or beyond the \
             newest {} of a destination)",
            l.prunable,
            prune::MAX_AGE_DAYS,
            prune::KEEP_PER_DESTINATION
        ));
    }
    out.push(row(l.status, text, Tone::Plain));

    let tone = if report.problems > 0 {
        Tone::Error
    } else if report.warnings > 0 {
        Tone::Warning
    } else {
        Tone::Success
    };
    out.push(Vec::new());
    out.push(vec![(theme::rule_for(width), Tone::Dim)]);
    out.push(vec![(
        format!(
            "{} · {}",
            plural(report.problems, "problem"),
            plural(report.warnings, "warning")
        ),
        tone,
    )]);
    out.push(vec![(theme::rule_for(width), Tone::Dim)]);
    crate::ui::fit_lines(out, width)
}

/// Renders the report as text for a terminal `width` columns wide, or in
/// full for a file or a pipe.
pub fn render(report: &Report, colour: bool, width: Option<usize>) -> String {
    crate::ui::paint_lines(&lines(report, width), colour)
}

/// `onefolder doctor --prune-logs`, with `--dry-run` and `--json`.
fn prune_logs(paths: &Paths, json: bool, dry_run: bool) -> Result<ExitCode> {
    let state = match State::load(&paths.state_file()) {
        Ok(state) => state,
        Err(e) => {
            // Without the state, the logs a run still points at are unknown.
            let message = format!("Nothing was removed: {e:#}");
            if json {
                Envelope::failure("doctor", "state-unreadable", &message).print()?;
            }
            crate::ui::err(&format!("{} {message}\n", theme::FAILED));
            return Ok(ExitCode::from(2));
        }
    };
    // Without the config, every log would count as a removed job's: refuse
    // rather than say nothing can go.
    let known = match ConfigFile::load(&paths.config_file()) {
        Ok(file) => prune::Known::from_config(&file.config),
        Err(e) => {
            let message = format!("Nothing was removed: {e}");
            if json {
                Envelope::failure("doctor", "config-invalid", &message).print()?;
            }
            crate::ui::err(&format!("{} {message}\n", theme::FAILED));
            return Ok(ExitCode::from(2));
        }
    };
    let outcome = prune::prune(paths, &state, &known, now(), dry_run);
    let ok = outcome.errors.is_empty();
    if json {
        Envelope::success("doctor", ok, &outcome).print()?;
    } else {
        let text = render_prune(
            &outcome,
            paths,
            theme::colour_on_stdout(),
            crate::ui::terminal_width(),
        );
        crate::ui::out(&text)?;
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// The text of `doctor --prune-logs`: each log that goes, then the totals.
pub fn render_prune(
    outcome: &prune::Outcome,
    paths: &Paths,
    colour: bool,
    width: Option<usize>,
) -> String {
    let dry = outcome.dry_run;
    let mut out: Vec<Vec<Segment>> = vec![
        vec![(
            if dry {
                "Prune logs (dry run, nothing is removed)".to_string()
            } else {
                "Prune logs".to_string()
            },
            Tone::Bold,
        )],
        Vec::new(),
    ];
    let logs = paths.logs_dir();
    if outcome.removed.is_empty() {
        out.push(vec![(
            format!(
                "  {} nothing to remove: no log is older than {} days or beyond the newest {} \
                 of its destination, other than those kept below",
                theme::DONE,
                prune::MAX_AGE_DAYS,
                prune::KEEP_PER_DESTINATION
            ),
            Tone::Plain,
        )]);
    }
    let shorten = |path: &str, room: usize| {
        let shown = Path::new(path)
            .strip_prefix(&logs)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.to_string());
        match width {
            Some(w) => crate::ui::shorten_path(&shown, w.saturating_sub(room).max(20)),
            None => shown,
        }
    };
    for r in &outcome.removed {
        let shown = shorten(&r.path, 34);
        out.push(vec![
            ("  ".to_string(), Tone::Plain),
            (theme::ACTION.to_string(), Tone::Accent),
            (format!(" {shown}   "), Tone::Plain),
            (
                format!("{} · {}", r.reason.words(), format_bytes(r.bytes)),
                Tone::Dim,
            ),
        ]);
    }
    if !outcome.skipped.is_empty() {
        out.push(Vec::new());
        out.push(vec![(
            "  Old enough to go, or one too many, but kept:".to_string(),
            Tone::Bold,
        )]);
    }
    for k in &outcome.skipped {
        out.push(vec![
            ("  ".to_string(), Tone::Plain),
            (theme::SKIPPED.to_string(), Tone::Warning),
            (format!(" {}   ", shorten(&k.path, 36)), Tone::Plain),
            (
                format!("{} ({})", k.kept.words(), k.candidate.words()),
                Tone::Dim,
            ),
        ]);
    }
    for e in &outcome.errors {
        out.push(vec![
            ("  ".to_string(), Tone::Plain),
            (theme::FAILED.to_string(), Tone::Error),
            (format!(" could not remove {e}"), Tone::Plain),
        ]);
    }
    out.push(Vec::new());
    let n = outcome.removed.len();
    let verb = if dry { "Would remove" } else { "Removed" };
    out.push(vec![(
        format!(
            "{verb} {n} file{}, {} · kept {} log{}, {}",
            if n == 1 { "" } else { "s" },
            format_bytes(outcome.removed_bytes),
            outcome.kept,
            if outcome.kept == 1 { "" } else { "s" },
            format_bytes(outcome.kept_bytes)
        ),
        if outcome.errors.is_empty() {
            Tone::Success
        } else {
            Tone::Warning
        },
    )]);
    crate::ui::paint_lines(&crate::ui::fit_lines(out, width), colour)
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rclone::detect::Version;
    use std::path::PathBuf;

    macro_rules! snapshot {
        ($name:expr, $value:expr) => {
            insta::with_settings!({
                snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
                prepend_module_to_snapshot => false,
            }, {
                insta::assert_snapshot!($name, $value);
            });
        };
    }

    fn rclone_ok() -> Result<Rclone, DetectError> {
        Ok(Rclone {
            path: PathBuf::from("/opt/homebrew/bin/rclone"),
            version: Version {
                major: 1,
                minor: 75,
                patch: 2,
            },
        })
    }

    /// Replaces the temp directory in rendered output so snapshots are stable.
    fn scrub(text: &str, root: &Path) -> String {
        text.replace(&root.display().to_string(), "/TMP")
    }

    fn paths(root: &Path) -> Paths {
        Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
        }
    }

    #[test]
    fn a_newer_rclone_is_a_note_not_a_warning() {
        let v = |minor, patch| Version {
            major: 1,
            minor,
            patch,
        };
        assert_eq!(
            latest_note(v(75, 2), v(76, 0)).as_deref(),
            Some("rclone 1.76.0 is available · update with: brew upgrade rclone")
        );
        assert_eq!(latest_note(v(75, 2), v(75, 2)), None);
        assert_eq!(latest_note(v(76, 0), v(75, 2)), None);
        let mut r = rclone_report(&rclone_ok());
        r.note = latest_note(v(75, 2), v(76, 0));
        assert_eq!(r.status, Status::Ok);
    }

    /// Every line fits at 40, 60, 80 and 120 columns, the rule included.
    #[tokio::test]
    async fn lines_fit_the_terminal() {
        let root = tempfile::tempdir().unwrap();
        let mut report = gather(&paths(root.path()), &rclone_ok()).await;
        report.rclone.note =
            Some("rclone 1.76.0 is available · update with: brew upgrade rclone".into());
        report.jobs.push(JobReport {
            status: Status::Warning,
            name: "documents-with-a-long-name".into(),
            source: "/Users/bora/Documents".into(),
            message: "source ok".into(),
            source_volume: Some("mounted".into()),
            run_lock: Some(lock::Seen {
                state: "stale".into(),
                pid: Some(4242),
                started: Some("2026-10-13T09:41:02+11:00".into()),
                path: "/tmp/runs/documents.lock".into(),
            }),
            destinations: vec![DestinationReport {
                status: Status::Warning,
                id: "d-91c0".into(),
                kind: "local".into(),
                target: "/Volumes/Backup/Archive/Deeply/Nested/Folder/Structure/Documents/2026"
                    .into(),
                mode: "one-way".into(),
                message: "not mounted (expects volume \"Backup\")".into(),
                volume: Some("not-mounted".into()),
            }],
        });
        report.logs.files = 48;
        report.logs.prunable = 3;
        // Fixed paths, so the shortened ones are the same on every machine.
        report.config.path = "/srv/home/someone/.config/onefolder/config.toml".into();
        report.state.path = "/srv/home/someone/.local/share/onefolder/state.json".into();
        report.data_dir.path = "/srv/home/someone/.local/share/onefolder".into();
        for width in [40, 60, 80, 120] {
            let text = render(&report, false, Some(width));
            for line in text.lines() {
                assert!(
                    crate::ui::width(line) <= width,
                    "{width}: {} > {width}: {line}",
                    crate::ui::width(line)
                );
            }
            let rule = "=".repeat(width.min(70));
            assert!(text.contains(&format!("\n{rule}\n")), "{text}");
            if width >= 80 {
                assert!(
                    text.contains("stale run lock left by process 4242"),
                    "{text}"
                );
            }
            snapshot!(format!("doctor_width_{width}"), scrub(&text, root.path()));
        }
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["jobs"][0]["source_volume"], "mounted");
        assert_eq!(json["jobs"][0]["run_lock"]["state"], "stale");
        assert_eq!(json["jobs"][0]["run_lock"]["pid"], 4242);
    }

    #[test]
    fn prune_text() {
        let root = tempfile::tempdir().unwrap();
        let p = paths(root.path());
        let outcome = prune::Outcome {
            dry_run: true,
            removed: vec![prune::Pruned {
                path: p
                    .logs_dir()
                    .join("documents/d-7f3a/2026-06-01T094102.jsonl")
                    .display()
                    .to_string(),
                destination: "documents/d-7f3a".into(),
                bytes: 12_000,
                reason: prune::Reason::Old,
                time: None,
            }],
            removed_bytes: 12_000,
            skipped: vec![prune::Skipped {
                path: p
                    .logs_dir()
                    .join("photos/d-0001/2026-01-01T094102.jsonl")
                    .display()
                    .to_string(),
                destination: "photos/d-0001".into(),
                bytes: 5_000,
                candidate: prune::Reason::Old,
                kept: prune::Kept::JobRemoved,
            }],
            kept: 47,
            kept_bytes: 12_400_000,
            errors: Vec::new(),
        };
        let text = render_prune(&outcome, &p, false, None);
        snapshot!("doctor_prune_dry_run", text);
        let empty = render_prune(&prune::Outcome::default(), &p, false, Some(40));
        assert!(empty.contains("nothing to remove"), "{empty}");
    }

    #[tokio::test]
    async fn fresh_install() {
        let root = tempfile::tempdir().unwrap();
        let report = gather(&paths(root.path()), &rclone_ok()).await;
        assert_eq!(report.problems, 0);
        assert!(!report.config.exists);
        snapshot!(
            "doctor_fresh",
            scrub(&render(&report, false, None), root.path())
        );
    }

    /// macOS only: the volume check of the Backup destination runs for real.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn configured() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Documents");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(root.path().join("config")).unwrap();
        let config = format!(
            r#"version = 1

[[accounts]]
label = "work"
remote = "onefolder-work"
drive_type = "business"

[[accounts]]
label = "personal"
remote = "onefolder-personal"
drive_type = "personal"

[[jobs]]
name = "documents"
source = "{source}"

  [[jobs.destinations]]
  id = "d-7f3a"
  type = "onedrive"
  account = "work"
  path = "Backups/Documents"
  mode = "two-way"

  [[jobs.destinations]]
  id = "d-91c0"
  type = "local"
  path = "/Volumes/Backup/Documents"
  volume_uuid = "A1B2C3D4-0000-4E5F-8A9B-000000000001"
  volume_name = "Backup"

[[jobs]]
name = "photos"
source = "/nowhere/Pictures/Photos"

  [[jobs.destinations]]
  id = "d-0001"
  type = "onedrive"
  account = "personal"
  path = "Photos"
  mode = "one-way"

[[jobs]]
name = "notes"
source = "{source}"
"#,
            source = source.display()
        );
        fs::write(root.path().join("config/config.toml"), config).unwrap();
        let logs = root.path().join("data/logs/documents/d-7f3a");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("a.jsonl"), vec![b'x'; 2500]).unwrap();

        let mut report = gather(&paths(root.path()), &rclone_ok()).await;
        // The log was written just now, so its date is today in the
        // system's time zone; a fixed time stands in for it so the snapshot
        // does not move with the day or the zone the tests run in.
        assert!(report.logs.newest.is_some() && report.logs.oldest.is_some());
        let fixed = Some("2026-10-13T12:00:00+11:00".to_string());
        report.logs.oldest = fixed.clone();
        report.logs.newest = fixed;
        assert_eq!(report.problems, 1, "missing photos source");
        assert_eq!(
            report.warnings, 2,
            "notes has no destinations; the Backup drive is not mounted"
        );
        assert_eq!(report.config.accounts, 2);
        assert_eq!(report.config.jobs, 3);
        assert_eq!(report.logs.files, 1);
        snapshot!(
            "doctor_configured",
            scrub(&render(&report, false, None), root.path())
        );
    }

    #[tokio::test]
    async fn accounts_block_after_checks() {
        use crate::model::DriveType;
        use crate::rclone::remotes::Quota;
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("config")).unwrap();
        fs::write(
            root.path().join("config/config.toml"),
            "version = 1\n\n[[accounts]]\nlabel = \"work\"\nremote = \"onefolder-work\"\ndrive_type = \"business\"\n\n\
             [[accounts]]\nlabel = \"personal\"\nremote = \"onefolder-personal\"\ndrive_type = \"personal\"\n\n\
             [[accounts]]\nlabel = \"old\"\nremote = \"onefolder-old\"\ndrive_type = \"personal\"\n",
        )
        .unwrap();
        let mut report = gather(&paths(root.path()), &rclone_ok()).await;
        let entry = |label: &str, drive_type, status, quota, hint: Option<&str>| TestEntry {
            label: label.into(),
            remote: format!("onefolder-{label}"),
            drive_type,
            ok: status == Health::Connected,
            status,
            quota,
            message: None,
            hint: hint.map(Into::into),
        };
        let results = [
            entry(
                "work",
                DriveType::Business,
                Health::Connected,
                Some(Quota {
                    free: Some(1_200_000_000_000),
                    ..Quota::default()
                }),
                None,
            ),
            entry(
                "personal",
                DriveType::Personal,
                Health::TokenExpired,
                None,
                Some("onefolder accounts reconnect personal"),
            ),
            entry(
                "old",
                DriveType::Personal,
                Health::RemoteMissing,
                None,
                None,
            ),
        ];
        for (a, t) in report.accounts.iter_mut().zip(&results) {
            apply_check(a, t);
        }
        count(&mut report);
        assert_eq!(report.problems, 2);
        let text = scrub(&render(&report, false, None), root.path());
        assert!(
            text.contains("  ✓ work        business   token ok   1.2 TB free\n"),
            "{text}"
        );
        assert!(text.contains(
            "  ✗ personal    personal   token expired · run: onefolder accounts reconnect personal\n"
        ), "{text}");
        snapshot!("doctor_accounts", text);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["accounts"][0]["health"], "connected");
        assert_eq!(json["accounts"][0]["free"], 1_200_000_000_000u64);
        assert_eq!(
            json["accounts"][1]["hint"],
            "onefolder accounts reconnect personal"
        );
    }

    #[tokio::test]
    async fn accounts_without_rclone_are_not_checked() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("config")).unwrap();
        fs::write(
            root.path().join("config/config.toml"),
            "version = 1\n[[accounts]]\nlabel = \"work\"\nremote = \"onefolder-work\"\ndrive_type = \"business\"\n",
        )
        .unwrap();
        let rclone = Err(DetectError::NotFound);
        let mut report = gather(&paths(root.path()), &rclone).await;
        check_accounts(&mut report, &paths(root.path()), &rclone).await;
        assert_eq!(report.accounts[0].status, Status::Skipped);
        assert_eq!(
            report.accounts[0].message,
            "not checked, rclone is not available"
        );
    }

    /// A local destination on the recorded volume is mounted; one whose
    /// recorded mount point is now a plain folder is not; one whose UUID
    /// differs is the wrong volume.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn local_destinations_get_the_volume_check() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        let here = root.path().join("here");
        let unplugged = root.path().join("Backup/Documents");
        let wrong = root.path().join("wrong");
        for dir in [&source, &here, &unplugged, &wrong] {
            fs::create_dir_all(dir).unwrap();
        }
        // An identity of the startup disk, as an older config could hold.
        let id = volume::identify_any(&here).await.unwrap().0;
        let uuid = id.volume_uuid.clone().unwrap();
        fs::create_dir_all(root.path().join("config")).unwrap();
        let config = format!(
            r#"[[jobs]]
name = "docs"
source = "{source}"

[[jobs.destinations]]
id = "d-0001"
type = "local"
path = "{here}"
volume_uuid = "{uuid}"
volume_name = "{name}"
mount_point = "{mp}"

[[jobs.destinations]]
id = "d-0002"
type = "local"
path = "{unplugged}"
volume_uuid = "A1B2C3D4-0000-4E5F-8A9B-000000000001"
volume_name = "Backup"
mount_point = "{backup}"

[[jobs.destinations]]
id = "d-0003"
type = "local"
path = "{wrong}"
volume_uuid = "00000000-0000-0000-0000-000000000000"
volume_name = "Other"
mount_point = "{mp}"
"#,
            source = source.display(),
            here = here.display(),
            name = id.volume_name,
            mp = id.mount_point,
            unplugged = unplugged.display(),
            backup = root.path().join("Backup").display(),
            wrong = wrong.display(),
        );
        fs::write(root.path().join("config/config.toml"), config).unwrap();
        let report = gather(&paths(root.path()), &rclone_ok()).await;
        let d = &report.jobs[0].destinations;
        assert_eq!(d[0].status, Status::Ok, "{:?}", d[0]);
        assert!(d[0].message.starts_with("mounted · volume"), "{:?}", d[0]);
        assert_eq!(d[1].status, Status::Warning, "{:?}", d[1]);
        assert_eq!(d[1].message, "not mounted (expects volume \"Backup\")");
        assert_eq!(d[2].status, Status::Problem, "{:?}", d[2]);
        assert!(
            d[2].message.starts_with(
                "wrong volume (expects volume \"Other\" (UUID 00000000-...), found volume"
            ),
            "{:?}",
            d[2]
        );
        assert_eq!((report.problems, report.warnings), (1, 1));
        let json = serde_json::to_value(&report).unwrap();
        let volumes: Vec<_> = json["jobs"][0]["destinations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["volume"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(volumes, vec!["mounted", "not-mounted", "wrong-volume"]);
    }

    #[tokio::test]
    async fn rclone_missing_and_config_broken() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("config")).unwrap();
        fs::write(
            root.path().join("config/config.toml"),
            "version = 1\n[[jobs]]\nname = \"Bad Name\"\nsource = \"relative/path\"\n",
        )
        .unwrap();
        let report = gather(&paths(root.path()), &Err(DetectError::NotFound)).await;
        assert_eq!(report.problems, 2);
        assert!(!report.config.loads);
        snapshot!(
            "doctor_problems",
            scrub(&render(&report, false, None), root.path())
        );

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["rclone"]["status"], "problem");
        assert_eq!(json["config"]["loads"], false);
    }
}
