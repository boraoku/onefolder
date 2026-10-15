//! `config.toml`: load, validate and save the user's accounts and jobs.
//!
//! The file is parsed twice over: once into a `toml_edit` document that keeps
//! comments, formatting and unknown keys, and once into the typed
//! [`Config`]. Saving writes typed changes back into that document, touching
//! only values that changed, so a hand-edited file survives a round trip.

mod paths;
mod sync;

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml_edit::DocumentMut;

use crate::model::{self, Account, Destination, Job};

pub use paths::{absolute, expand_tilde, tilde, Paths};

/// The config schema version this binary reads and writes.
pub const CONFIG_VERSION: i64 = 1;

/// The typed view of `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: i64,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub jobs: Vec<Job>,
}

fn default_version() -> i64 {
    CONFIG_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            accounts: Vec::new(),
            jobs: Vec::new(),
        }
    }
}

impl Config {
    pub fn account(&self, label: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.label == label)
    }

    pub fn job(&self, name: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.name == name)
    }

    /// Number of destinations across all jobs.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by status, milestone 6"))]
    pub fn destination_count(&self) -> usize {
        self.jobs.iter().map(|j| j.destinations.len()).sum()
    }
}

/// Why a config could not be loaded.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is not valid TOML: {message}")]
    Parse { path: PathBuf, message: String },
    #[error(
        "{path} was written by a newer OneFolder (config version {found}). \
         This version understands config version {supported}. \
         Update OneFolder, then run it again."
    )]
    NewerVersion {
        path: PathBuf,
        found: i64,
        supported: i64,
    },
    #[error("{path} has an unsupported config version {found}. Expected {supported}.")]
    BadVersion {
        path: PathBuf,
        found: String,
        supported: i64,
    },
    #[error("{path} does not match the expected shape: {message}")]
    Schema { path: PathBuf, message: String },
    #[error("{path} has {} to fix:\n{}", plural(.problems.len(), "problem"), bullet(.problems))]
    Invalid {
        path: PathBuf,
        problems: Vec<String>,
    },
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn bullet(problems: &[String]) -> String {
    problems
        .iter()
        .map(|p| format!("  ✗ {p}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl ConfigError {
    /// The message without the file path, for places that already show it.
    pub fn detail(&self) -> String {
        match self {
            ConfigError::Read { source, .. } => format!("could not read it: {source}"),
            ConfigError::Parse { message, .. } => format!("not valid TOML: {message}"),
            ConfigError::NewerVersion {
                found, supported, ..
            } => format!(
                "written by a newer OneFolder (config version {found}). \
                 This version understands config version {supported}. \
                 Update OneFolder, then run it again."
            ),
            ConfigError::BadVersion {
                found, supported, ..
            } => format!("unsupported config version {found}. Expected {supported}."),
            ConfigError::Schema { message, .. } => {
                format!("does not match the expected shape: {message}")
            }
            ConfigError::Invalid { problems, .. } => format!(
                "{} to fix:\n{}",
                plural(problems.len(), "problem"),
                bullet(problems)
            ),
        }
    }
}

/// A loaded config plus the document it came from.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    pub path: PathBuf,
    pub config: Config,
    /// False when the file did not exist and this is the empty default.
    pub existed: bool,
    doc: DocumentMut,
}

impl ConfigFile {
    /// Loads and validates the config at `path`. A missing file is an empty
    /// config with version 1.
    pub fn load(path: &Path) -> Result<ConfigFile, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(path, &text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ConfigFile {
                path: path.to_path_buf(),
                config: Config::default(),
                existed: false,
                doc: DocumentMut::new(),
            }),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parses and validates config text. `path` is used in messages only.
    pub fn parse(path: &Path, text: &str) -> Result<ConfigFile, ConfigError> {
        let doc: DocumentMut =
            text.parse()
                .map_err(|e: toml_edit::TomlError| ConfigError::Parse {
                    path: path.to_path_buf(),
                    message: e.to_string().trim().to_string(),
                })?;
        check_version(path, &doc)?;
        let config: Config =
            toml_edit::de::from_document(doc.clone()).map_err(|e| ConfigError::Schema {
                path: path.to_path_buf(),
                message: e.to_string().trim().to_string(),
            })?;
        let problems = validate(&config);
        if !problems.is_empty() {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                problems,
            });
        }
        Ok(ConfigFile {
            path: path.to_path_buf(),
            config,
            existed: true,
            doc,
        })
    }

    /// The config as it would be written, with the typed values merged into
    /// the original document.
    pub fn render(&mut self) -> anyhow::Result<String> {
        sync::sync_document(&mut self.doc, &self.config)?;
        Ok(self.doc.to_string())
    }

    /// Validates, then writes the config atomically (temp file and rename).
    pub fn save(&mut self) -> anyhow::Result<()> {
        use anyhow::Context;
        let problems = validate(&self.config);
        if !problems.is_empty() {
            return Err(ConfigError::Invalid {
                path: self.path.clone(),
                problems,
            }
            .into());
        }
        let text = self.render()?;
        crate::state::write_atomic(&self.path, text.as_bytes())
            .with_context(|| format!("could not save {}", self.path.display()))?;
        self.existed = true;
        Ok(())
    }
}

fn check_version(path: &Path, doc: &DocumentMut) -> Result<(), ConfigError> {
    let Some(item) = doc.get("version") else {
        // A hand-written file without a version is read as the current one;
        // saving writes the version out.
        return Ok(());
    };
    match item.as_integer() {
        Some(v) if v > CONFIG_VERSION => Err(ConfigError::NewerVersion {
            path: path.to_path_buf(),
            found: v,
            supported: CONFIG_VERSION,
        }),
        Some(v) if v == CONFIG_VERSION => migrate(doc, v),
        _ => Err(ConfigError::BadVersion {
            path: path.to_path_buf(),
            found: item.to_string().trim().to_string(),
            supported: CONFIG_VERSION,
        }),
    }
}

/// Migration hook: upgrades older documents in memory before they are read.
/// Version 1 is the first version, so there is nothing to migrate yet.
fn migrate(_doc: &DocumentMut, _from: i64) -> Result<(), ConfigError> {
    Ok(())
}

/// Every rule from the Data model section of DESIGN.md. Returns one plain
/// sentence per problem; empty means the config is valid.
pub fn validate(config: &Config) -> Vec<String> {
    let mut problems = Vec::new();

    let mut labels = HashSet::new();
    for account in &config.accounts {
        let label = &account.label;
        if !model::is_valid_name(label) {
            problems.push(format!(
                "account label \"{label}\" must be lowercase letters, digits and dashes, starting with a letter or digit"
            ));
        }
        if !labels.insert(label.as_str()) {
            problems.push(format!("account label \"{label}\" is used more than once"));
        }
        let expected = Account::remote_for(label);
        if account.remote != expected {
            problems.push(format!(
                "account \"{label}\" has remote \"{}\", expected \"{expected}\"",
                account.remote
            ));
        }
    }

    let mut names = HashSet::new();
    for job in &config.jobs {
        if !names.insert(job.name.as_str()) {
            problems.push(format!("job name \"{}\" is used more than once", job.name));
        }
        problems.extend(validate_job(job, &config.accounts));
    }

    problems
}

/// The rules for one job and its destinations, without the uniqueness of
/// its name (which needs the other jobs). Shared by config loading, the job
/// form and `jobs add`/`jobs edit` with flags.
pub fn validate_job(job: &Job, accounts: &[Account]) -> Vec<String> {
    let mut problems = Vec::new();
    let name = &job.name;
    if !model::is_valid_name(name) {
        problems.push(format!(
            "job name \"{name}\" must be lowercase letters, digits and dashes, starting with a letter or digit"
        ));
    }
    let source = Path::new(&job.source);
    if !source.is_absolute() {
        problems.push(format!(
            "job \"{name}\" has source \"{}\", which is not an absolute path",
            job.source
        ));
    } else if let Some(drive) = crate::platform::drive_path(source).filter(|_| {
        !identity_recorded(
            job.source_volume_uuid.as_deref(),
            job.source_volume_partition_uuid.as_deref(),
            job.source_volume_name.as_deref(),
            job.source_volume_device.as_deref(),
        )
    }) {
        problems.push(format!(
            "job \"{name}\" has its source under {} but no source_volume_uuid \
             (or source_volume_partition_uuid, or source_volume_device and source_volume_name) recorded",
            drive.root
        ));
    }
    if job.excludes.iter().any(|e| e.trim().is_empty()) {
        problems.push(format!("job \"{name}\" has an empty exclude pattern"));
    }

    let mut ids = HashSet::new();
    let mut targets = HashSet::new();
    for dest in &job.destinations {
        let id = dest.id();
        if !model::is_valid_destination_id(id) {
            problems.push(format!(
                "job \"{name}\" has destination id \"{id}\", expected \"d-\" and four hex characters"
            ));
        }
        if !ids.insert(id) {
            problems.push(format!(
                "job \"{name}\" uses destination id \"{id}\" more than once"
            ));
        }
        let target = match dest {
            Destination::Onedrive { account, path, .. } => {
                format!("onedrive:{account}:{}", path.trim_matches('/'))
            }
            Destination::Local { path, .. } => format!("local:{}", lexical_key(Path::new(path))),
        };
        if !targets.insert(target) {
            problems.push(format!(
                "job \"{name}\" lists {} more than once",
                dest.display()
            ));
        }
        match dest {
            // An empty path is the drive root, which the design allows.
            Destination::Onedrive { account, .. } => {
                if !accounts.iter().any(|a| &a.label == account) {
                    problems.push(format!(
                        "job \"{name}\" destination {id} uses account \"{account}\", which does not exist"
                    ));
                }
            }
            Destination::Local {
                path,
                volume_uuid,
                volume_partition_uuid,
                volume_name,
                volume_device,
                ..
            } => {
                let p = Path::new(path);
                if !p.is_absolute() {
                    problems.push(format!(
                        "job \"{name}\" destination {id} has path \"{path}\", which is not an absolute path"
                    ));
                    continue;
                }
                let unrecorded = !identity_recorded(
                    volume_uuid.as_deref(),
                    volume_partition_uuid.as_deref(),
                    volume_name.as_deref(),
                    volume_device.as_deref(),
                );
                if let Some(drive) = crate::platform::drive_path(p).filter(|_| unrecorded) {
                    problems.push(format!(
                        "job \"{name}\" destination {id} is under {} but has no volume_uuid \
                         (or volume_partition_uuid, or volume_device and volume_name) recorded",
                        drive.root
                    ));
                }
                if source.is_absolute() {
                    if let Some(how) = overlap(source, p) {
                        problems.push(format!("job \"{name}\" destination {id} ({path}) {how}"));
                    }
                }
            }
        }
    }
    problems
}

/// A volume UUID, a partition UUID, or (the weak fallback) a device node
/// plus a volume name.
fn identity_recorded(
    uuid: Option<&str>,
    partition_uuid: Option<&str>,
    name: Option<&str>,
    device: Option<&str>,
) -> bool {
    let set = |v: Option<&str>| v.is_some_and(|v| !v.trim().is_empty());
    set(uuid) || set(partition_uuid) || (set(device) && set(name))
}

/// The overlap rule again, on the paths as the disk resolves them
/// (symlinks such as `/tmp` to `/private/tmp` followed), for the form and
/// the flags, where the folders are at hand. Config loading keeps to the
/// lexical check so an unplugged drive never makes a config unreadable.
/// Only reports overlaps the lexical check in [`validate_job`] misses.
pub fn overlaps_on_disk(job: &Job) -> Vec<String> {
    let source = Path::new(&job.source);
    if !source.is_absolute() {
        return Vec::new();
    }
    let resolved_source = resolve_existing(source);
    job.destinations
        .iter()
        .filter_map(|d| match d {
            Destination::Local { id, path, .. } => {
                let p = Path::new(path);
                if !p.is_absolute() || overlap(source, p).is_some() {
                    return None;
                }
                overlap(&resolved_source, &resolve_existing(p)).map(|how| {
                    format!(
                        "job \"{}\" destination {id} ({path}) {how} (after following symlinks)",
                        job.name
                    )
                })
            }
            Destination::Onedrive { .. } => None,
        })
        .collect()
}

/// `path` with its closest existing ancestor canonicalised and the rest
/// appended, so a folder that does not exist yet still resolves.
pub fn resolve_existing(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut current = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(current) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Normalised components of an absolute path: `.` and `..` resolved
/// lexically, lowercased (APFS and HFS+ are usually case insensitive).
fn lexical_parts(path: &Path) -> Vec<String> {
    use std::path::Component;
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_lowercase()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    parts
}

fn lexical_key(path: &Path) -> String {
    format!("/{}", lexical_parts(path).join("/"))
}

/// Says how a local destination overlaps the source, if it does: syncing a
/// folder into itself would copy the copy on every run.
fn overlap(source: &Path, dest: &Path) -> Option<&'static str> {
    let s = lexical_parts(source);
    let d = lexical_parts(dest);
    if s == d {
        Some("is the source folder itself")
    } else if d.starts_with(&s) {
        Some("is inside the source folder")
    } else if s.starts_with(&d) {
        Some("contains the source folder")
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
