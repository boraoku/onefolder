use super::*;

#[test]
fn short_dates() {
    assert_eq!(short_date("2026-10-13T09:41:02+11:00"), "13 Oct");
    assert_eq!(short_date("2026-01-05T09:41:02+11:00"), "5 Jan");
    assert_eq!(short_date("odd"), "odd");
}

#[test]
fn conflict_lines() {
    assert_eq!(
        conflict_line("Notes/plan.md", Some("Notes/plan.conflict-2026-10-13-1.md")),
        "Notes/plan.md → Notes/plan.conflict-2026-10-13-1.md"
    );
    assert_eq!(conflict_line("a", None), "a");
    assert_eq!(place("onedrive"), "OneDrive");
    assert_eq!(place("local"), "the drive");
}
