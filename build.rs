//! Records the short git commit for `onefolder version`. Falls back to
//! `unknown` outside a git checkout or before the first commit.

use std::path::Path;
use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ONEFOLDER_COMMIT={commit}");

    // Rebuild when HEAD moves. Only existing paths are watched, so a missing
    // .git does not force a rebuild every time.
    for path in [".git/HEAD", ".git/refs/heads", ".git/packed-refs"] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
