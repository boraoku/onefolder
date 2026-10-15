//! OneFolder: syncs local folders to OneDrive and external drives by
//! driving rclone. See DESIGN.md for the full design.

mod cli;
mod config;
mod jsonout;
mod model;
mod platform;
mod rclone;
mod state;
mod sync;
mod ui;
mod volume;

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match cli::dispatch(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {e:#}", ui::theme::FAILED);
            ExitCode::from(2)
        }
    }
}
