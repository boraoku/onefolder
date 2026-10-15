//! `onefolder accounts [list|add|remove|test|reconnect] [label]`.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

use crate::config::{Config, ConfigFile, Paths};
use crate::jsonout::accounts::{
    AccountEntry, Added, Health, InUse, Reconnected, Removed, RemovedDestination, TestEntry,
};
use crate::jsonout::Envelope;
use crate::model::{self, Account, Datetime, Destination, DriveType};
use crate::rclone::detect::Rclone;
use crate::rclone::exec::{self, Attached, AttachedOutput, InterruptGuard};
use crate::rclone::remotes::{self, FailureKind, Quota, RemoteError};
use crate::state::State;
use crate::ui;
use crate::ui::theme::{self, Tone};

#[derive(Debug, Args)]
pub struct AccountsArgs {
    #[command(subcommand)]
    pub action: Option<AccountsAction>,
}

/// The kind of OneDrive account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DriveTypeArg {
    /// OneDrive for home (outlook.com, hotmail.com, live.com)
    Personal,
    /// OneDrive for work or school (Microsoft 365)
    Business,
}

impl From<DriveTypeArg> for DriveType {
    fn from(arg: DriveTypeArg) -> Self {
        match arg {
            DriveTypeArg::Personal => DriveType::Personal,
            DriveTypeArg::Business => DriveType::Business,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AccountsAction {
    /// List accounts (the default)
    List,
    /// Add a OneDrive account; rclone opens the browser to sign in
    Add {
        /// Short name for the account, such as work or personal
        label: Option<String>,
        /// personal or business; without it, choose from a list
        #[arg(long, value_enum)]
        drive_type: Option<DriveTypeArg>,
        /// Turn on OneDrive delta listing, which makes --fast-list list from
        /// the drive root; worth it only for folders near the root of a large drive
        #[arg(long)]
        delta: bool,
    },
    /// Remove an account and its rclone remote
    Remove {
        /// The account to remove; without it, pick from a list
        label: Option<String>,
        /// Also delete the job destinations that use this account (and
        /// remove without asking)
        #[arg(long)]
        force: bool,
        /// Remove without asking; needed when there is no terminal to ask on
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Check that an account can still reach OneDrive
    Test {
        /// The account to check; without it, check them all
        label: Option<String>,
    },
    /// Sign in again when a token has expired
    Reconnect {
        /// The account to sign in to again; without it, pick from a list
        label: Option<String>,
    },
}

/// Options every accounts command shares.
#[derive(Debug, Clone, Copy)]
struct Ctx<'a> {
    rclone: &'a Rclone,
    command: &'static str,
    json: bool,
    dry_run: bool,
}

impl Ctx<'_> {
    /// True when the user can be asked questions: a terminal on both ends and no `--json`.
    fn interactive(&self) -> bool {
        !self.json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }

    fn colour(&self) -> bool {
        theme::colour_on_stdout()
    }

    /// Prints a progress line. With `--json` it goes to stderr so stdout
    /// holds only the JSON document.
    fn say(&self, line: &str) -> Result<()> {
        if self.json {
            crate::ui::err(&format!("{line}\n"));
            Ok(())
        } else {
            crate::ui::out(&format!("{line}\n"))
        }
    }

    fn paint(&self, text: &str, tone: Tone) -> String {
        if self.json {
            text.to_string()
        } else {
            theme::paint(text, tone, self.colour())
        }
    }

    /// Reports a refusal or failure and gives back exit code 2.
    fn fail(&self, code: &str, message: &str) -> Result<ExitCode> {
        if self.json {
            Envelope::failure(self.command, code, message).print()?;
        }
        let colour = std::io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        crate::ui::err(&format!(
            "{} {message}\n",
            theme::paint(theme::FAILED, Tone::Error, colour)
        ));
        Ok(ExitCode::from(2))
    }

    /// The user backed out of a prompt: nothing changed.
    fn cancelled(&self) -> Result<ExitCode> {
        self.say(&format!(
            "{} Cancelled, nothing changed.",
            self.paint(theme::SKIPPED, Tone::Warning)
        ))?;
        Ok(ExitCode::from(2))
    }

    fn print<T: Serialize>(&self, ok: bool, data: T) -> Result<()> {
        Envelope::success(self.command, ok, data).print()
    }
}

pub async fn run(
    args: &AccountsArgs,
    rclone: &Rclone,
    json: bool,
    dry_run: bool,
) -> Result<ExitCode> {
    let ctx = |command| Ctx {
        rclone,
        command,
        json,
        dry_run,
    };
    match &args.action {
        // On a terminal, `onefolder accounts` opens the list to act on.
        None if !json && ctx("accounts").interactive() => interactive(rclone).await,
        None | Some(AccountsAction::List) => list(ctx("accounts list")).await,
        Some(AccountsAction::Add {
            label,
            drive_type,
            delta,
        }) => {
            add(
                ctx("accounts add"),
                label.clone(),
                drive_type.map(DriveType::from),
                *delta,
            )
            .await
        }
        Some(AccountsAction::Remove { label, force, yes }) => {
            remove(ctx("accounts remove"), label.clone(), *force, *yes).await
        }
        Some(AccountsAction::Test { label }) => test(ctx("accounts test"), label.as_deref()).await,
        Some(AccountsAction::Reconnect { label }) => {
            reconnect(ctx("accounts reconnect"), label.clone()).await
        }
    }
}

/// The accounts as the list shows them, with whether rclone still has
/// each remote (no network). `Err` with the reason when the config does
/// not load.
async fn entries(rclone: &Rclone) -> Result<Result<Vec<AccountEntry>, String>> {
    let paths = Paths::from_env()?;
    let file = match ConfigFile::load(&paths.config_file()) {
        Ok(f) => f,
        Err(e) => return Ok(Err(e.to_string())),
    };
    let known = remotes::list(rclone).await.ok();
    Ok(Ok(file
        .config
        .accounts
        .iter()
        .map(|a| {
            let exists = known.as_ref().map(|names| names.contains(&a.remote));
            AccountEntry::new(a, exists)
        })
        .collect()))
}

/// The interactive accounts list: loops until the user goes back. Each
/// action runs on the normal terminal, whose output stays until a key
/// (inside the interactive session: on the session's screens).
pub async fn interactive(rclone: &Rclone) -> Result<ExitCode> {
    let ctx = |command| Ctx {
        rclone,
        command,
        json: false,
        dry_run: false,
    };
    let list = match entries(rclone).await? {
        Ok(list) => list,
        Err(e) => return ctx("accounts").fail("config-invalid", &e),
    };
    let mut screen = ui::accounts::Screen::new(list);
    loop {
        // Inside the session: anything the last action printed, first.
        super::show_output().await?;
        let (back, action) = tokio::task::spawn_blocking(move || {
            let action = ui::accounts::run_screen(&mut screen);
            (screen, action)
        })
        .await
        .map_err(|e| anyhow::anyhow!("the accounts screen stopped unexpectedly: {e}"))?;
        screen = back;
        use ui::accounts::Action;
        let (code, select, what) = match action? {
            Action::Back => return Ok(ExitCode::SUCCESS),
            Action::Add => (
                add(ctx("accounts add"), None, None, false).await?,
                None,
                "Added".to_string(),
            ),
            Action::Test(label) => (
                test(ctx("accounts test"), Some(&label)).await?,
                Some(label.clone()),
                format!("Tested {label}"),
            ),
            Action::TestAll => (
                test(ctx("accounts test"), None).await?,
                None,
                "Tested every account".to_string(),
            ),
            Action::Reconnect(label) => (
                reconnect(ctx("accounts reconnect"), Some(label.clone())).await?,
                Some(label.clone()),
                format!("Signed in to {label} again"),
            ),
            // Confirmed on the screen, which said the destinations go too.
            Action::Remove(label) => (
                remove(ctx("accounts remove"), Some(label.clone()), true, true).await?,
                None,
                format!("Removed {label}"),
            ),
        };
        tokio::task::spawn_blocking(|| {
            ui::pause(&theme::paint(
                "\nPress any key to go back to the accounts list.",
                Tone::Dim,
                theme::colour_on_stdout(),
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!("the accounts screen stopped unexpectedly: {e}"))??;
        screen.message = Some(if code == ExitCode::SUCCESS {
            (format!("{} {what}", theme::DONE), Tone::Success)
        } else {
            (
                format!(
                    "{} Not everything went to plan; the output said why",
                    theme::SKIPPED
                ),
                Tone::Warning,
            )
        });
        match entries(rclone).await? {
            Ok(list) => screen.set_entries(list, select.as_deref()),
            Err(e) => return ctx("accounts").fail("config-invalid", &e),
        }
    }
}

/// Loads the config, or reports why it does not load.
fn load_config(ctx: &Ctx) -> Result<Result<ConfigFile, ExitCode>> {
    let paths = Paths::from_env()?;
    match ConfigFile::load(&paths.config_file()) {
        Ok(file) => Ok(Ok(file)),
        Err(e) => Ok(Err(ctx.fail("config-invalid", &e.to_string())?)),
    }
}

/// Checks a label for a new account: the shape, unique among accounts, and
/// no rclone remote of the same name already.
pub fn validate_new_label(label: &str, config: &Config, remotes: &[String]) -> Result<(), String> {
    if !model::is_valid_name(label) {
        return Err(format!(
            "\"{label}\" is not a valid label: use lowercase letters, digits and hyphens, starting with a letter or digit"
        ));
    }
    if config.account(label).is_some() {
        return Err(format!("there is already an account called {label}"));
    }
    let remote = Account::remote_for(label);
    if remotes.iter().any(|r| r == &remote) {
        return Err(format!(
            "rclone already has a remote called {remote}. Pick another label, or delete that remote with: rclone config delete {remote}"
        ));
    }
    Ok(())
}

/// The current local time as a TOML date-time with the UTC offset, such as
/// `2026-10-13T09:40:00+11:00`, so the date shown later is the user's date.
pub fn now_datetime() -> Option<Datetime> {
    local_datetime(&jiff::Zoned::now())
}

fn local_datetime(time: &jiff::Zoned) -> Option<Datetime> {
    time.strftime("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
        .parse()
        .ok()
}

/// The command that fixes a failure, if there is one.
fn hint_for(kind: FailureKind, label: &str) -> Option<String> {
    match kind {
        FailureKind::TokenExpired => Some(format!("onefolder accounts reconnect {label}")),
        FailureKind::RemoteMissing => Some(format!("onefolder accounts reconnect {label}")),
        FailureKind::Timeout | FailureKind::Other => None,
    }
}

/// Turns the outcome of `about` into a test result.
pub fn test_entry(account: &Account, result: Result<Quota, RemoteError>) -> TestEntry {
    let mut entry = TestEntry {
        label: account.label.clone(),
        remote: account.remote.clone(),
        drive_type: account.drive_type,
        ok: false,
        status: Health::Failed,
        quota: None,
        message: None,
        hint: None,
    };
    match result {
        Ok(quota) => {
            entry.ok = true;
            entry.status = Health::Connected;
            entry.quota = Some(quota);
        }
        Err(e) => {
            let kind = e.kind();
            entry.status = match kind {
                FailureKind::TokenExpired => Health::TokenExpired,
                FailureKind::RemoteMissing => Health::RemoteMissing,
                FailureKind::Timeout => Health::Timeout,
                FailureKind::Other => Health::Failed,
            };
            entry.message = Some(match &e {
                RemoteError::Timeout { limit, .. } => format!(
                    "rclone did not answer within {} seconds, check your network connection",
                    limit.as_secs()
                ),
                other if remotes::is_network_error(&other.to_string()) => {
                    format!("{other} {} check your network connection", theme::DOT)
                }
                other => other.to_string(),
            });
            entry.hint = hint_for(kind, &account.label);
        }
    }
    entry
}

/// Runs `about` for every account at once and returns the results in order.
pub async fn check_all(rclone: &Rclone, accounts: &[Account], limit: Duration) -> Vec<TestEntry> {
    let mut set = tokio::task::JoinSet::new();
    for (i, account) in accounts.iter().enumerate() {
        let rclone = rclone.clone();
        let remote = account.remote.clone();
        set.spawn(async move { (i, remotes::about(&rclone, &remote, limit).await) });
    }
    let mut results: Vec<Option<Result<Quota, RemoteError>>> =
        accounts.iter().map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        if let Ok((i, result)) = joined {
            if let Some(slot) = results.get_mut(i) {
                *slot = Some(result);
            }
        }
    }
    accounts
        .iter()
        .zip(results)
        .map(|(account, result)| {
            let result = result.unwrap_or_else(|| {
                Err(RemoteError::Unreadable {
                    command: "about",
                    detail: "the check stopped unexpectedly".into(),
                })
            });
            test_entry(account, result)
        })
        .collect()
}

async fn list(ctx: Ctx<'_>) -> Result<ExitCode> {
    let file = match load_config(&ctx)? {
        Ok(f) => f,
        Err(code) => return Ok(code),
    };
    let (known, problem) = match remotes::list(ctx.rclone).await {
        Ok(names) => (Some(names), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let entries: Vec<AccountEntry> = file
        .config
        .accounts
        .iter()
        .map(|a| {
            let exists = known.as_ref().map(|names| names.contains(&a.remote));
            AccountEntry::new(a, exists)
        })
        .collect();
    if ctx.json {
        ctx.print(true, &entries)?;
    } else {
        crate::ui::out(&ui::accounts::render_list(
            &entries,
            problem.as_deref(),
            ctx.colour(),
        ))?;
    }
    Ok(ExitCode::SUCCESS)
}

async fn test(ctx: Ctx<'_>, label: Option<&str>) -> Result<ExitCode> {
    let file = match load_config(&ctx)? {
        Ok(f) => f,
        Err(code) => return Ok(code),
    };
    let accounts: Vec<Account> = match label {
        Some(label) => match file.config.account(label) {
            Some(a) => vec![a.clone()],
            None => return no_such_account(&ctx, &file.config, label),
        },
        None => file.config.accounts.clone(),
    };
    let entries = check_all(ctx.rclone, &accounts, remotes::ABOUT_TIMEOUT).await;
    let all_ok = entries.iter().all(|e| e.ok);
    if ctx.json {
        ctx.print(all_ok, &entries)?;
    } else {
        crate::ui::out(&ui::accounts::render_test(&entries, ctx.colour()))?;
    }
    Ok(ExitCode::from(test_exit_code(&entries)))
}

/// 0 when every account connects (or there are none), 2 when every one
/// fails, 1 when some do.
pub fn test_exit_code(entries: &[TestEntry]) -> u8 {
    let failed = entries.iter().filter(|e| !e.ok).count();
    if failed == 0 {
        0
    } else if failed == entries.len() {
        2
    } else {
        1
    }
}

fn no_such_account(ctx: &Ctx, config: &Config, label: &str) -> Result<ExitCode> {
    let known: Vec<&str> = config.accounts.iter().map(|a| a.label.as_str()).collect();
    let message = if known.is_empty() {
        format!("There is no account called {label}. There are no accounts yet; add one with: onefolder accounts add")
    } else {
        format!(
            "There is no account called {label}. The accounts are: {}.",
            known.join(", ")
        )
    };
    ctx.fail("no-such-account", &message)
}

/// The label to act on: given, picked from a list on a terminal, or an error.
fn choose_account(
    ctx: &Ctx,
    config: &Config,
    label: Option<String>,
    verb: &str,
) -> Result<Result<String, ExitCode>> {
    if let Some(label) = label {
        return Ok(match config.account(&label) {
            Some(_) => Ok(label),
            None => Err(no_such_account(ctx, config, &label)?),
        });
    }
    if config.accounts.is_empty() {
        return Ok(Err(ctx.fail(
            "no-accounts",
            "There are no accounts yet; add one with: onefolder accounts add",
        )?));
    }
    if !ctx.interactive() {
        return Ok(Err(ctx.fail(
            "needs-label",
            &format!(
                "Say which account to {verb}, for example: onefolder {} {}",
                ctx.command, config.accounts[0].label
            ),
        )?));
    }
    let choices: Vec<(String, DriveType)> = config
        .accounts
        .iter()
        .map(|a| (a.label.clone(), a.drive_type))
        .collect();
    let title = format!("Account to {verb}");
    Ok(match ui::accounts::pick_account(&title, &choices)? {
        Some(label) => Ok(label),
        None => Err(ctx.cancelled()?),
    })
}

/// Why an attached rclone run did not succeed, in words.
fn attached_problem(result: &Attached) -> Option<String> {
    if result.interrupted {
        Some("You stopped the sign-in with Ctrl-C.".into())
    } else if !result.status.success() {
        Some(format!(
            "rclone stopped before the sign-in finished ({}).",
            result.status
        ))
    } else {
        None
    }
}

/// Deletes a remote left behind by a failed sign-in. Returns a sentence
/// saying what happened, for the error message.
async fn clean_up(rclone: &Rclone, remote: &str) -> String {
    match remotes::list(rclone).await {
        Ok(names) if !names.iter().any(|n| n == remote) => {
            "Nothing was saved, so nothing changed.".into()
        }
        _ => match remotes::delete(rclone, remote).await {
            Ok(()) => format!(
                "OneFolder removed the half-made rclone remote {remote}, so nothing changed."
            ),
            Err(e) => format!(
                "OneFolder could not remove the half-made rclone remote {remote} ({e}); remove it with: rclone config delete {remote}"
            ),
        },
    }
}

/// Where rclone's own output goes: the terminal, or stderr under `--json`.
fn attached_output(ctx: &Ctx) -> AttachedOutput {
    if ctx.json {
        AttachedOutput::Stderr
    } else {
        AttachedOutput::Inherit
    }
}

/// Runs `about` on a remote that was just signed in to, unless Ctrl-C comes
/// first. `Err(())` means the user pressed Ctrl-C.
async fn verify(
    ctx: &Ctx<'_>,
    guard: &mut InterruptGuard,
    remote: &str,
) -> Result<Result<Quota, RemoteError>, ()> {
    let result = tokio::select! {
        r = remotes::about(ctx.rclone, remote, remotes::VERIFY_TIMEOUT) => r,
        () = guard.next() => return Err(()),
    };
    match result {
        // rclone killed by a signal while we hold the guard: that was Ctrl-C,
        // whose notice may not have arrived yet.
        Err(RemoteError::Failed { code: None, .. }) => Err(()),
        _ if guard.interrupted() => Err(()),
        other => Ok(other),
    }
}

/// What a sign-in proved: the quota, or a warning when the sign-in worked
/// but the check after it did not answer in time.
type Verified = (Option<Quota>, Option<String>);

/// The warning for a check that timed out after a sign-in that worked.
fn verify_timeout_warning(label: &str) -> String {
    format!(
        "OneDrive did not answer the check within {} seconds, so the account is kept without \
         it. Check it later with: onefolder accounts test {label}",
        remotes::VERIFY_TIMEOUT.as_secs()
    )
}

/// The browser sign-in that makes a new remote: `rclone config create`
/// with the terminal attached, then `about` to prove it works. On a
/// failure or Ctrl-C the half-made remote is removed and the failure is
/// reported; the exit code comes back in `Err`. When rclone signed in but
/// the check only timed out (a slow network), the remote is kept and a
/// warning comes back instead: the sign-in itself worked.
async fn sign_in(
    ctx: &Ctx<'_>,
    guard: &mut InterruptGuard,
    label: &str,
    remote: &str,
    drive_type: DriveType,
    delta: bool,
) -> Result<Result<Verified, ExitCode>> {
    let output = attached_output(ctx);
    // Inside the session, rclone talks to the user on the session's screen;
    // it is taken back once rclone has finished.
    let lent = ui::session::hand_over()?;
    let attached = match remotes::create_onedrive(
        ctx.rclone, remote, drive_type, delta, output, guard,
    )
    .await
    {
        Ok(a) => a,
        Err(e) => {
            drop(lent);
            return Ok(Err(
                ctx.fail("rclone-failed", &format!("Could not start rclone: {e}"))?
            ));
        }
    };
    drop(lent);
    if let Some(problem) = attached_problem(&attached) {
        let cleanup = clean_up(ctx.rclone, remote).await;
        let code = if attached.interrupted {
            "cancelled"
        } else {
            "sign-in-failed"
        };
        return Ok(Err(ctx.fail(code, &format!("{problem} {cleanup}"))?));
    }
    ctx.say(&format!(
        "\n{} Signed in. Checking the connection...",
        ctx.paint(theme::ACTION, Tone::Accent)
    ))?;
    match verify(ctx, guard, remote).await {
        Ok(Ok(quota)) => Ok(Ok((Some(quota), None))),
        Ok(Err(RemoteError::Timeout { .. })) => Ok(Ok((None, Some(verify_timeout_warning(label))))),
        Ok(Err(e)) => {
            let cleanup = clean_up(ctx.rclone, remote).await;
            Ok(Err(ctx.fail(
                "verify-failed",
                &format!("rclone made the remote {remote}, but OneDrive did not answer through it: {e}. {cleanup}"),
            )?))
        }
        Err(()) => {
            let cleanup = clean_up(ctx.rclone, remote).await;
            Ok(Err(ctx.fail(
                "cancelled",
                &format!(
                    "You stopped OneFolder with Ctrl-C while it checked the new sign-in. {cleanup}"
                ),
            )?))
        }
    }
}

async fn add(
    ctx: Ctx<'_>,
    label: Option<String>,
    drive_type: Option<DriveType>,
    delta: bool,
) -> Result<ExitCode> {
    let file = match load_config(&ctx)? {
        Ok(f) => f,
        Err(code) => return Ok(code),
    };
    let known = match remotes::list(ctx.rclone).await {
        Ok(names) => names,
        Err(e) => {
            return ctx.fail(
                "rclone-failed",
                &format!("Could not read rclone's list of remotes: {e}"),
            )
        }
    };
    let check = |l: &str| validate_new_label(l, &file.config, &known);
    let interactive = ctx.interactive();

    // The questions are asked when something was left out on a terminal;
    // flags alone never lead to a question.
    let asking = interactive && (label.is_none() || drive_type.is_none());
    if asking {
        ctx.say(&format!(
            "{}\n",
            ctx.paint("Add a OneDrive account", Tone::Bold)
        ))?;
    }
    let label = match label {
        Some(label) => match check(&label) {
            Ok(()) => label,
            Err(reason) => return ctx.fail("invalid-label", &capitalise(&reason)),
        },
        None if interactive => match ui::accounts::ask_label(check)? {
            Some(label) => label,
            None => return ctx.cancelled(),
        },
        None => {
            return ctx.fail(
                "needs-label",
                "accounts add needs a label when it cannot ask for one, for example: onefolder accounts add work --drive-type business",
            )
        }
    };
    let drive_type = match drive_type {
        Some(d) => d,
        None if interactive => match ui::accounts::pick_drive_type()? {
            Some(d) => d,
            None => return ctx.cancelled(),
        },
        None => {
            return ctx.fail(
                "needs-drive-type",
                &format!("accounts add needs --drive-type personal or --drive-type business when it cannot ask, for example: onefolder accounts add {label} --drive-type business"),
            )
        }
    };

    // Delta listing: asked along with the other questions when the flag
    // was not given (the default is off; DESIGN.md decision 5).
    let delta = if !delta && asking {
        match ui::accounts::pick_delta()? {
            Some(d) => d,
            None => return ctx.cancelled(),
        }
    } else {
        delta
    };
    let remote = Account::remote_for(&label);
    let mut account = Account {
        label: label.clone(),
        remote: remote.clone(),
        drive_type,
        added: now_datetime(),
        delta,
    };
    if ctx.dry_run {
        let args = remotes::create_args(&remote, drive_type, delta).join(" ");
        if ctx.json {
            ctx.print(
                true,
                Added {
                    account: AccountEntry::new(&account, Some(false)),
                    quota: None,
                    dry_run: true,
                    note: None,
                    warning: None,
                },
            )?;
        } else {
            ctx.say(&format!(
                "{} Dry run: would run rclone {args}, then add {label} ({}) to the config.",
                ctx.paint(theme::SKIPPED, Tone::Warning),
                drive_type.as_str()
            ))?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !std::io::stdin().is_terminal() {
        return ctx.fail(
            "needs-terminal",
            "Signing in to OneDrive happens in your browser and needs a terminal to start from. Run onefolder accounts add in Terminal.",
        );
    }

    // Held until the account is saved or cleaned up, so Ctrl-C at any
    // point is a cancellation rather than an abrupt exit.
    let mut guard = exec::interrupt_guard();
    ctx.say(&format!(
        "\n{} Signing in to {label} ({}). rclone opens your browser; sign in there, then come back here.\n  If the browser does not open, copy the link rclone prints below.\n",
        ctx.paint(theme::ACTION, Tone::Accent),
        drive_type.as_str()
    ))?;
    let (quota, warning) =
        match sign_in(&ctx, &mut guard, &label, &remote, drive_type, delta).await? {
            Ok(v) => v,
            Err(code) => return Ok(code),
        };

    let mut note = None;
    if let Ok(Some(shown)) = remotes::show(ctx.rclone, &remote).await {
        if let Some(found) = shown.drive_type {
            if found != drive_type {
                note = Some(format!(
                    "Microsoft reports this as a {} account, so it is recorded as {}.",
                    found.as_str(),
                    found.as_str()
                ));
            }
            account.drive_type = found;
        }
        account.delta = shown.delta;
    }

    // The sign-in took minutes: read the config again, so edits made
    // meanwhile are kept.
    let path = Paths::from_env()?.config_file();
    let mut file = match ConfigFile::load(&path) {
        Ok(f) => f,
        Err(e) => {
            let cleanup = clean_up(ctx.rclone, &remote).await;
            return ctx.fail(
                "config-invalid",
                &format!("Signed in, but the config no longer loads: {e} {cleanup}"),
            );
        }
    };
    if file.config.account(&label).is_some() {
        // Another add of the same label finished first, with the same rclone
        // remote name; deleting the remote would break that account.
        return ctx.fail(
            "account-exists",
            &format!("An account called {label} was added while you were signing in, so OneFolder kept that one. Check it with: onefolder accounts test {label}"),
        );
    }
    file.config.accounts.push(account.clone());
    if let Err(e) = file.save() {
        let cleanup = clean_up(ctx.rclone, &remote).await;
        return ctx.fail(
            "config-save-failed",
            &format!("Signed in, but could not save the config: {e:#}. {cleanup}"),
        );
    }
    drop(guard);

    if ctx.json {
        ctx.print(
            true,
            Added {
                account: AccountEntry::new(&account, Some(true)),
                quota,
                dry_run: false,
                note,
                warning: warning.clone(),
            },
        )?;
        if let Some(w) = &warning {
            crate::ui::err(&format!("{} {w}\n", theme::SKIPPED));
        }
    } else {
        let mut line = format!(
            "{} Added {label} {dot} {} {dot} {remote}",
            ctx.paint(theme::DONE, Tone::Success),
            account.drive_type.as_str(),
            dot = theme::DOT
        );
        if let Some(q) = quota.as_ref().and_then(ui::accounts::format_quota) {
            line.push_str(&format!(" {} {q}", theme::DOT));
        }
        ctx.say(&line)?;
        if let Some(note) = &note {
            ctx.say(&format!("  {note}"))?;
        }
        if let Some(w) = &warning {
            ctx.say(&format!(
                "  {} {}",
                ctx.paint(theme::SKIPPED, Tone::Warning),
                ctx.paint(w, Tone::Warning)
            ))?;
        }
        ctx.say(&ctx.paint(
            "  Next: add a job that syncs a folder to it with: onefolder jobs add",
            Tone::Dim,
        ))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The OneDrive destinations that use `label`, across all jobs.
fn destinations_using(config: &Config, label: &str) -> Vec<RemovedDestination> {
    config
        .jobs
        .iter()
        .flat_map(|job| {
            job.destinations.iter().filter_map(move |d| match d {
                Destination::Onedrive {
                    id, account, path, ..
                } if account == label => Some(RemovedDestination {
                    job: job.name.clone(),
                    id: id.clone(),
                    path: path.clone(),
                }),
                _ => None,
            })
        })
        .collect()
}

/// Jobs that would have no destinations left once `label`'s are removed.
fn jobs_left_empty(config: &Config, label: &str) -> Vec<String> {
    config
        .jobs
        .iter()
        .filter(|job| {
            !job.destinations.is_empty()
                && job
                    .destinations
                    .iter()
                    .all(|d| matches!(d, Destination::Onedrive { account, .. } if account == label))
        })
        .map(|job| job.name.clone())
        .collect()
}

/// Drops the state records (runs, holds, bisync) of removed destinations.
/// Returns how many entries went.
fn prune_state(state: &mut State, removed: &[RemovedDestination]) -> usize {
    let mut count = 0;
    for d in removed {
        count += state.drop_destination(&d.job, &d.id);
    }
    count
}

fn count_words(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

async fn remove(ctx: Ctx<'_>, label: Option<String>, force: bool, yes: bool) -> Result<ExitCode> {
    let mut file = match load_config(&ctx)? {
        Ok(f) => f,
        Err(code) => return Ok(code),
    };
    let label = match choose_account(&ctx, &file.config, label, "remove")? {
        Ok(l) => l,
        Err(code) => return Ok(code),
    };
    let remote = file
        .config
        .account(&label)
        .map(|a| a.remote.clone())
        .unwrap_or_else(|| Account::remote_for(&label));

    let used = destinations_using(&file.config, &label);
    if !used.is_empty() && !force {
        let list = used
            .iter()
            .map(|d| format!("{} ({label}:{})", d.job, d.path))
            .collect::<Vec<_>>()
            .join(", ");
        let message = format!(
            "{label} is still used by {}: {list}. Edit those jobs first, or run onefolder accounts remove {label} --force to delete those destinations too.",
            if used.len() == 1 {
                "a job destination".to_string()
            } else {
                format!("{} job destinations", used.len())
            }
        );
        if ctx.json {
            Envelope {
                onefolder: env!("CARGO_PKG_VERSION"),
                command: ctx.command.to_string(),
                ok: false,
                data: Some(InUse {
                    label: label.clone(),
                    destinations: used,
                }),
                error: Some(crate::jsonout::ErrorBody {
                    code: "account-in-use".into(),
                    message: message.clone(),
                }),
            }
            .print()?;
            crate::ui::err(&format!("{} {message}\n", theme::FAILED));
            return Ok(ExitCode::from(2));
        }
        return ctx.fail("account-in-use", &message);
    }
    let emptied = jobs_left_empty(&file.config, &label);
    // Never remove silently: without a terminal to ask on (and `--json`
    // never asks), the consent is a flag. `--json` alone is not consent.
    if !ctx.interactive() && !ctx.dry_run && !force && !yes {
        let why = if ctx.json {
            "accounts remove --json does not ask for confirmation"
        } else {
            "accounts remove cannot ask for confirmation here (no terminal)"
        };
        return ctx.fail(
            "needs-yes",
            &format!(
                "{why}. Add --yes to remove {label}, or --dry-run to see what would go: \
                 onefolder accounts remove {label} --yes"
            ),
        );
    }

    let exists = match remotes::list(ctx.rclone).await {
        Ok(names) => names.iter().any(|n| n == &remote),
        Err(e) => {
            return ctx.fail(
                "rclone-failed",
                &format!("Could not read rclone's list of remotes: {e}"),
            )
        }
    };

    if ctx.interactive() && !ctx.dry_run && !yes {
        let mut question = if exists {
            format!("Remove {label} and its rclone remote {remote}?")
        } else {
            format!("Remove {label}? Its rclone remote {remote} is already gone.")
        };
        if !used.is_empty() {
            question.push_str(&format!(
                " This also deletes {} from your jobs.",
                count_words(used.len(), "destination", "destinations")
            ));
        }
        if !emptied.is_empty() {
            question.push_str(&format!(
                " {} will be left with no destinations.",
                emptied.join(", ")
            ));
        }
        if !ui::accounts::confirm(&question, false)? {
            return ctx.cancelled();
        }
    }

    let mut result = Removed {
        label: label.clone(),
        remote: remote.clone(),
        remote_deleted: exists,
        remote_was_missing: !exists,
        destinations_removed: used.clone(),
        jobs_left_empty: emptied.clone(),
        state_entries_removed: 0,
        dry_run: ctx.dry_run,
    };
    if ctx.dry_run {
        if ctx.json {
            ctx.print(true, &result)?;
        } else {
            let what = if exists {
                format!("delete the rclone remote {remote} and remove {label} from the config")
            } else {
                format!("remove {label} from the config (its rclone remote is already gone)")
            };
            let mut extra = String::new();
            if !used.is_empty() {
                extra.push_str(&format!(
                    ", and delete {} from your jobs",
                    count_words(used.len(), "destination", "destinations")
                ));
            }
            ctx.say(&format!(
                "{} Dry run: would {what}{extra}.",
                ctx.paint(theme::SKIPPED, Tone::Warning)
            ))?;
            for job in &emptied {
                ctx.say(&format!(
                    "  {} job {job} would be left with no destinations",
                    ctx.paint(theme::SKIPPED, Tone::Warning)
                ))?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Config first: if saving fails, the account still points at a working remote.
    file.config.accounts.retain(|a| a.label != label);
    for job in &mut file.config.jobs {
        job.destinations
            .retain(|d| !matches!(d, Destination::Onedrive { account, .. } if account == &label));
    }
    if let Err(e) = file.save() {
        return ctx.fail(
            "config-save-failed",
            &format!("Could not save the config, so nothing changed: {e:#}"),
        );
    }

    let mut state_problem = None;
    if !used.is_empty() {
        let paths = Paths::from_env()?;
        match State::update(&paths, |state| prune_state(state, &used)) {
            Ok(removed) => result.state_entries_removed = removed,
            Err(e) => state_problem = Some(format!("{e:#}")),
        }
    }

    if exists {
        if let Err(e) = remotes::delete(ctx.rclone, &remote).await {
            return ctx.fail(
                "rclone-failed",
                &format!("Removed {label} from the config, but could not delete the rclone remote {remote}: {e}. Delete it with: rclone config delete {remote}"),
            );
        }
    }

    if ctx.json {
        ctx.print(true, &result)?;
    } else {
        let mut line = format!("{} Removed {label}", ctx.paint(theme::DONE, Tone::Success));
        if exists {
            line.push_str(&format!(" and its rclone remote {remote}"));
        } else {
            line.push_str(&format!(
                " {} its rclone remote {remote} was already gone",
                theme::DOT
            ));
        }
        ctx.say(&line)?;
        for d in &used {
            ctx.say(&format!(
                "  {} removed destination {label}:{} from job {}",
                ctx.paint(theme::ACTION, Tone::Accent),
                d.path,
                d.job
            ))?;
        }
        for job in &emptied {
            ctx.say(&format!(
                "  {} job {job} has no destinations now {} add one with: onefolder jobs edit {job}",
                ctx.paint(theme::SKIPPED, Tone::Warning),
                theme::DOT
            ))?;
        }
    }
    if let Some(problem) = state_problem {
        crate::ui::err(&format!(
            "{} Could not tidy the run records of the removed destinations: {problem}\n",
            theme::SKIPPED
        ));
    }
    Ok(ExitCode::SUCCESS)
}

async fn reconnect(ctx: Ctx<'_>, label: Option<String>) -> Result<ExitCode> {
    let file = match load_config(&ctx)? {
        Ok(f) => f,
        Err(code) => return Ok(code),
    };
    let label = match choose_account(&ctx, &file.config, label, "reconnect")? {
        Ok(l) => l,
        Err(code) => return Ok(code),
    };
    let Some(account) = file.config.account(&label).cloned() else {
        return no_such_account(&ctx, &file.config, &label);
    };
    let remote = account.remote.clone();
    let exists = match remotes::list(ctx.rclone).await {
        Ok(names) => names.iter().any(|n| n == &remote),
        Err(e) => {
            return ctx.fail(
                "rclone-failed",
                &format!("Could not read rclone's list of remotes: {e}"),
            )
        }
    };
    if ctx.dry_run {
        if ctx.json {
            ctx.print(
                true,
                Reconnected {
                    label,
                    remote,
                    recreated: !exists,
                    quota: None,
                    dry_run: true,
                    warning: None,
                },
            )?;
        } else {
            let what = if exists {
                format!("run rclone config reconnect {remote}:")
            } else {
                format!(
                    "make the missing rclone remote again with rclone {}",
                    remotes::create_args(&remote, account.drive_type, account.delta).join(" ")
                )
            };
            ctx.say(&format!(
                "{} Dry run: would {what} and check the connection.",
                ctx.paint(theme::SKIPPED, Tone::Warning)
            ))?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !std::io::stdin().is_terminal() {
        return ctx.fail(
            "needs-terminal",
            "Signing in again happens in your browser and needs a terminal to start from. Run onefolder accounts reconnect in Terminal.",
        );
    }

    let mut guard = exec::interrupt_guard();
    let quota = if exists {
        ctx.say(&format!(
            "{} Signing in to {label} again. rclone opens your browser; sign in there, then come back here.\n  If rclone asks questions, the suggested answers are fine.\n",
            ctx.paint(theme::ACTION, Tone::Accent)
        ))?;
        // Inside the session, rclone talks to the user on the session's
        // screen; it is taken back once rclone has finished.
        let lent = ui::session::hand_over()?;
        let attached = match remotes::reconnect(
            ctx.rclone,
            &remote,
            attached_output(&ctx),
            &mut guard,
        )
        .await
        {
            Ok(a) => a,
            Err(e) => return ctx.fail("rclone-failed", &format!("Could not start rclone: {e}")),
        };
        drop(lent);
        if let Some(problem) = attached_problem(&attached) {
            let code = if attached.interrupted {
                "cancelled"
            } else {
                "sign-in-failed"
            };
            return ctx.fail(
                code,
                &format!("{problem} The account keeps its old sign-in; try onefolder accounts reconnect {label} again."),
            );
        }
        match verify(&ctx, &mut guard, &remote).await {
            Ok(Ok(q)) => (Some(q), None),
            Ok(Err(RemoteError::Timeout { .. })) => (None, Some(verify_timeout_warning(&label))),
            Ok(Err(e)) => {
                return ctx.fail(
                    "verify-failed",
                    &format!("Signed in, but OneDrive still did not answer: {e}"),
                )
            }
            Err(()) => {
                return ctx.fail(
                    "cancelled",
                    &format!("You stopped OneFolder with Ctrl-C while it checked the sign-in. Check it with: onefolder accounts test {label}"),
                )
            }
        }
    } else {
        // The remote is gone from rclone's config (deleted by hand, or a new
        // Mac): make it again under the same name, so the account and the
        // jobs that use it carry on unchanged.
        ctx.say(&format!(
            "{} rclone has no remote called {remote} any more, so OneFolder makes it again.\n  rclone opens your browser; sign in to {label} ({}) there, then come back here.\n",
            ctx.paint(theme::ACTION, Tone::Accent),
            account.drive_type.as_str()
        ))?;
        match sign_in(
            &ctx,
            &mut guard,
            &label,
            &remote,
            account.drive_type,
            account.delta,
        )
        .await?
        {
            Ok(v) => v,
            Err(code) => return Ok(code),
        }
    };
    let (quota, warning) = quota;
    drop(guard);

    if ctx.json {
        ctx.print(
            true,
            Reconnected {
                label,
                remote,
                recreated: !exists,
                quota,
                dry_run: false,
                warning: warning.clone(),
            },
        )?;
        if let Some(w) = &warning {
            crate::ui::err(&format!("{} {w}\n", theme::SKIPPED));
        }
    } else {
        let mut line = format!(
            "\n{} {label} is connected again",
            ctx.paint(theme::DONE, Tone::Success)
        );
        if let Some(q) = quota.as_ref().and_then(ui::accounts::format_quota) {
            line.push_str(&format!(" {} {q}", theme::DOT));
        }
        ctx.say(&line)?;
        if let Some(w) = &warning {
            ctx.say(&format!(
                "  {} {}",
                ctx.paint(theme::SKIPPED, Tone::Warning),
                ctx.paint(w, Tone::Warning)
            ))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Job, Mode};

    fn config() -> Config {
        Config {
            version: 1,
            accounts: vec![Account {
                label: "work".into(),
                remote: "onefolder-work".into(),
                drive_type: DriveType::Business,
                added: None,
                delta: false,
            }],
            jobs: vec![Job {
                name: "documents".into(),
                source: "/Users/bora/Documents".into(),
                excludes: vec![],
                destinations: vec![
                    Destination::onedrive(
                        "d-7f3a".into(),
                        "work".into(),
                        "Backups/Documents".into(),
                        Mode::TwoWay,
                    ),
                    Destination::local("d-91c0".into(), "/tmp/backup".into()),
                ],
                ..Job::default()
            }],
        }
    }

    #[test]
    fn test_exit_codes() {
        let entry = |ok| TestEntry {
            label: "work".into(),
            remote: "onefolder-work".into(),
            drive_type: DriveType::Business,
            ok,
            status: if ok {
                Health::Connected
            } else {
                Health::Failed
            },
            quota: None,
            message: None,
            hint: None,
        };
        assert_eq!(test_exit_code(&[]), 0);
        assert_eq!(test_exit_code(&[entry(true), entry(true)]), 0);
        assert_eq!(test_exit_code(&[entry(true), entry(false)]), 1);
        assert_eq!(test_exit_code(&[entry(false)]), 2);
        assert_eq!(test_exit_code(&[entry(false), entry(false)]), 2);
    }

    #[test]
    fn a_verify_timeout_warns_and_names_the_test() {
        let w = verify_timeout_warning("work");
        assert!(w.contains("within 30 seconds"), "{w}");
        assert!(w.ends_with("onefolder accounts test work"), "{w}");
    }

    #[test]
    fn label_validation() {
        let c = config();
        let remotes = vec!["onefolder-old".to_string(), "other".to_string()];
        assert!(validate_new_label("home", &c, &remotes).is_ok());
        assert!(validate_new_label("home-2", &c, &remotes).is_ok());
        for bad in ["Home", "-home", "my home", "", "home_2", "hé"] {
            let err = validate_new_label(bad, &c, &remotes).unwrap_err();
            assert!(err.contains("not a valid label"), "{bad}: {err}");
        }
        let err = validate_new_label("work", &c, &remotes).unwrap_err();
        assert_eq!(err, "there is already an account called work");
        let err = validate_new_label("old", &c, &remotes).unwrap_err();
        assert!(
            err.contains("rclone already has a remote called onefolder-old"),
            "{err}"
        );
        assert!(err.contains("rclone config delete onefolder-old"), "{err}");
        // A remote that is not OneFolder's does not block a label.
        assert!(validate_new_label("other", &c, &remotes).is_ok());
    }

    #[test]
    fn added_is_local_time_with_its_offset() {
        use jiff::tz::{self, TimeZone};
        let sydney = jiff::civil::date(2026, 10, 13)
            .at(9, 40, 0, 0)
            .to_zoned(TimeZone::fixed(tz::offset(11)))
            .unwrap();
        assert_eq!(
            local_datetime(&sydney).unwrap().to_string(),
            "2026-10-13T09:40:00+11:00"
        );
        let utc = jiff::civil::date(2026, 10, 12)
            .at(22, 40, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        assert_eq!(
            local_datetime(&utc).unwrap().to_string(),
            "2026-10-12T22:40:00+00:00"
        );
        let now = now_datetime().unwrap();
        assert!(now.offset.is_some() && now.date.is_some());
    }

    #[test]
    fn test_entries_classify_failures() {
        let account = &config().accounts[0];
        let ok = test_entry(account, Ok(Quota::default()));
        assert!(ok.ok);
        assert_eq!(ok.status, Health::Connected);
        assert_eq!(ok.hint, None);

        let expired = test_entry(
            account,
            Err(RemoteError::Failed {
                command: "about",
                kind: remotes::classify("couldn't fetch token: invalid_grant"),
                message: "couldn't fetch token: invalid_grant".into(),
                code: Some(1),
            }),
        );
        assert!(!expired.ok);
        assert_eq!(expired.status, Health::TokenExpired);
        assert_eq!(
            expired.hint.as_deref(),
            Some("onefolder accounts reconnect work")
        );

        let timeout = test_entry(
            account,
            Err(RemoteError::Timeout {
                command: "about",
                limit: Duration::from_secs(10),
            }),
        );
        assert_eq!(timeout.status, Health::Timeout);
        assert!(timeout.message.unwrap().contains("within 10 seconds"));

        let missing = test_entry(
            account,
            Err(RemoteError::Failed {
                command: "about",
                kind: FailureKind::RemoteMissing,
                message: "didn't find section in config file".into(),
                code: Some(1),
            }),
        );
        assert_eq!(missing.status, Health::RemoteMissing);
        assert_eq!(
            missing.hint.as_deref(),
            Some("onefolder accounts reconnect work")
        );

        let offline = test_entry(
            account,
            Err(RemoteError::Failed {
                command: "about",
                kind: FailureKind::Other,
                message:
                    "couldn't fetch token: dial tcp: lookup login.microsoftonline.com: no such host"
                        .into(),
                code: Some(1),
            }),
        );
        assert_eq!(offline.status, Health::Failed);
        assert_eq!(offline.hint, None);
        assert!(offline
            .message
            .unwrap()
            .ends_with("no such host · check your network connection"));
    }

    #[test]
    fn destinations_that_use_an_account() {
        let mut c = config();
        let used = destinations_using(&c, "work");
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].id, "d-7f3a");
        assert!(destinations_using(&c, "home").is_empty());
        // documents keeps its local destination.
        assert!(jobs_left_empty(&c, "work").is_empty());
        c.jobs[0].destinations.truncate(1);
        assert_eq!(jobs_left_empty(&c, "work"), vec!["documents"]);
    }

    #[test]
    fn state_records_of_removed_destinations_go() {
        let mut state: State = serde_json::from_str(
            r#"{"version": 1,
                "holds": {"documents/d-7f3a": ["Old"], "documents/d-91c0": ["Keep"]},
                "bisync": {"documents/d-7f3a": {"initialised": true}}}"#,
        )
        .unwrap();
        let removed = destinations_using(&config(), "work");
        assert_eq!(prune_state(&mut state, &removed), 2);
        assert!(state.holds.contains_key("documents/d-91c0"));
        assert!(state.bisync.is_empty());
        assert_eq!(prune_state(&mut state, &removed), 0);
    }
}
