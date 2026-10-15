//! Sync semantics: the planner and runner for one-way and two-way
//! destinations. They talk to the UI only through the [`reporter::Reporter`]
//! trait.
//!

pub mod folders;
pub mod moves;
pub mod oneway;
pub mod plan;
pub mod reporter;
pub mod summary;
pub mod twoway;

/// A path in Unicode normalisation form C. macOS file systems can hand
/// names back decomposed (NFD: `e` plus a combining accent) where OneDrive
/// and rclone listings use the composed form, so a dry-run path and a
/// listing entry for the same file can differ byte for byte. Used only to
/// match them for labels (new or changed); the counts come from rclone.
pub fn nfc(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    path.nfc().collect()
}

/// A listing's paths in NFC, for [`nfc`] matching.
pub fn nfc_set(paths: &std::collections::HashSet<String>) -> std::collections::HashSet<String> {
    paths.iter().map(|p| nfc(p)).collect()
}
