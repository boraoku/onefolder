//! `--json` output: every non-interactive command prints one envelope.
//!
//! ```json
//! {
//!   "onefolder": "0.1.0",
//!   "command": "doctor",
//!   "ok": true,
//!   "data": { ... },
//!   "error": null
//! }
//! ```
//!
//! `ok` is false when the command failed or found problems. `data` holds the
//! command's own document; `error` is set when the command could not do its
//! work, with a stable `code` and a human `message`. `run` streams
//! newline-delimited events instead; [`run`] documents their shapes.

pub mod accounts;
pub mod run;
pub mod status;

use anyhow::{Context, Result};
use serde::Serialize;

/// The JSON document a command prints on stdout.
#[derive(Debug, Serialize)]
pub struct Envelope<T: Serialize> {
    /// Version of the OneFolder binary that wrote this.
    pub onefolder: &'static str,
    pub command: String,
    pub ok: bool,
    pub data: Option<T>,
    pub error: Option<ErrorBody>,
}

/// Why a command could not do its work.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl<T: Serialize> Envelope<T> {
    pub fn success(command: &str, ok: bool, data: T) -> Self {
        Envelope {
            onefolder: env!("CARGO_PKG_VERSION"),
            command: command.to_string(),
            ok,
            data: Some(data),
            error: None,
        }
    }
}

impl Envelope<()> {
    pub fn failure(command: &str, code: &str, message: &str) -> Self {
        Envelope {
            onefolder: env!("CARGO_PKG_VERSION"),
            command: command.to_string(),
            ok: false,
            data: None,
            error: Some(ErrorBody {
                code: code.to_string(),
                message: message.to_string(),
            }),
        }
    }
}

impl<T: Serialize> Envelope<T> {
    /// Pretty-printed JSON with a trailing newline.
    pub fn to_json(&self) -> Result<String> {
        let mut text = serde_json::to_string_pretty(self).context("could not write JSON")?;
        text.push('\n');
        Ok(text)
    }

    /// Prints the envelope on stdout.
    pub fn print(&self) -> Result<()> {
        write_stdout(self.to_json()?.as_bytes())
    }
}

/// Writes to stdout and flushes. A closed pipe (`onefolder doctor | head -1`)
/// is not an error: the reader has what it wanted.
pub fn write_stdout(bytes: &[u8]) -> Result<()> {
    use std::io::{ErrorKind, Write};
    let mut out = std::io::stdout().lock();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => {
            Err(e).context("could not write to standard output")
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_shape() {
        let json = Envelope::failure("run", "rclone-missing", "rclone is not on PATH")
            .to_json()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["command"], "run");
        assert_eq!(v["ok"], false);
        assert!(v["data"].is_null());
        assert_eq!(v["error"]["code"], "rclone-missing");
        assert_eq!(v["onefolder"], env!("CARGO_PKG_VERSION"));
    }
}
