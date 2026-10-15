//! `onefolder completion <shell>`: prints a completion script.

use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, CommandFactory, ValueEnum};
use clap_complete::Shell as ClapShell;

use super::Cli;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug, Args)]
pub struct CompletionArgs {
    /// The shell to generate for
    pub shell: Shell,
}

pub fn run(args: &CompletionArgs) -> Result<ExitCode> {
    let shell = match args.shell {
        Shell::Bash => ClapShell::Bash,
        Shell::Zsh => ClapShell::Zsh,
        Shell::Fish => ClapShell::Fish,
    };
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "onefolder", &mut script);
    crate::jsonout::write_stdout(&script)?;
    Ok(ExitCode::SUCCESS)
}
