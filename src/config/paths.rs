//! Where OneFolder keeps its files.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

/// Environment variable that replaces the config directory (`~/.config/onefolder`).
pub const CONFIG_DIR_ENV: &str = "ONEFOLDER_CONFIG_DIR";
/// Environment variable that replaces the data directory (`~/.local/share/onefolder`).
pub const DATA_DIR_ENV: &str = "ONEFOLDER_DATA_DIR";

/// The config and data directories.
///
/// Defaults follow DESIGN.md on every platform: `~/.config/onefolder` for
/// the hand-editable config and `~/.local/share/onefolder` for machine-written
/// state, logs and bisync working directories. The home directory comes from
/// the `directories` crate.
///
/// Engineering default: `ONEFOLDER_CONFIG_DIR` and `ONEFOLDER_DATA_DIR`
/// replace those two directories when set to a non-empty value. The tests
/// use them to run against temporary directories, and they are handy for
/// trying a separate setup without touching the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    /// Resolves the directories from the environment and the home directory.
    pub fn from_env() -> Result<Paths> {
        let config_override = non_empty_env(CONFIG_DIR_ENV);
        let data_override = non_empty_env(DATA_DIR_ENV);
        let home = match (&config_override, &data_override) {
            (Some(_), Some(_)) => None,
            _ => Some(
                directories::BaseDirs::new()
                    .map(|b| b.home_dir().to_path_buf())
                    .ok_or_else(|| anyhow!("could not find your home directory"))?,
            ),
        };
        let from_home = |rel: &str| home.as_ref().map(|h| h.join(rel)).unwrap_or_default();
        Ok(Paths {
            config_dir: config_override.unwrap_or_else(|| from_home(".config/onefolder")),
            data_dir: data_override.unwrap_or_else(|| from_home(".local/share/onefolder")),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn bisync_dir(&self) -> PathBuf {
        self.data_dir.join("bisync")
    }

    /// Run locks: `runs/<job>.lock`, one per job while it runs.
    pub fn runs_dir(&self) -> PathBuf {
        self.data_dir.join("runs")
    }
}

fn non_empty_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Shortens a path under the home directory to `~/...` for display.
pub fn tilde(path: &Path) -> String {
    if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Expands a leading `~` or `~/` to the home directory. Other text, and
/// `~user` forms, are returned unchanged.
pub fn expand_tilde(text: &str) -> String {
    let rest = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => return text.to_string(),
    };
    match home_dir() {
        Some(home) => format!("{}{rest}", home.display()),
        None => text.to_string(),
    }
}

/// Turns a path the user typed into the absolute form the config stores:
/// surrounding spaces trimmed, `~` expanded, a relative path joined to the
/// current directory, and `.` and `..` resolved lexically (symlinks are
/// kept as typed). `None` for empty text.
pub fn absolute(text: &str) -> Option<PathBuf> {
    use std::path::Component;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let expanded = PathBuf::from(expand_tilde(text));
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir().ok()?.join(expanded)
    };
    let mut out = PathBuf::from("/");
    for component in joined.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let home = home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home.display().to_string());
        assert_eq!(
            expand_tilde("~/Documents"),
            home.join("Documents").display().to_string()
        );
        assert_eq!(expand_tilde("~bora/x"), "~bora/x");
        assert_eq!(expand_tilde("/a/~"), "/a/~");
        assert_eq!(tilde(&home.join("Documents")), "~/Documents");
    }

    #[test]
    fn absolute_paths() {
        assert_eq!(absolute("  "), None);
        assert_eq!(absolute("/a/./b/../c/"), Some(PathBuf::from("/a/c")));
        assert_eq!(absolute("/.."), Some(PathBuf::from("/")));
        assert_eq!(absolute("~/x"), Some(home_dir().unwrap().join("x")));
        let rel = absolute("some/where").unwrap();
        assert!(rel.is_absolute() && rel.ends_with("some/where"), "{rel:?}");
    }
}
