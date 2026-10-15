//! The previews and questions of a run: the one-way preview, the two-way
//! plan, the first-run merge, the yes/no questions, and the folder-deletion
//! prompt.
//!
//! The `*_lines` functions decide the text and tone of every piece, so the
//! plain output, the `--dry-run` printout and the screen say the same thing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::rclone::bisync::Abort;
use crate::sync::folders::LIST_PROMPT_OVER;
use crate::sync::moves::{MoveGroup, MovesChoice};
use crate::sync::oneway::{PlannedFile, Preview, PREVIEW_PATHS};
use crate::sync::plan::{BigKind, Direction, Plan, ResyncOffer, ResyncReason};
use crate::sync::reporter::{
    Answer, DecidedBy, DeletedFolder, FolderAnswer, MovesAnswer, Question, Where,
};
use crate::sync::twoway::place;
use crate::ui::progress::fit;
use crate::ui::summary::format_bytes;
use crate::ui::theme::{self, Tone};

/// A run of text with one tone.
pub type Segment = (String, Tone);

fn seg(text: impl Into<String>, tone: Tone) -> Segment {
    (text.into(), tone)
}

fn files(n: u64) -> &'static str {
    if n == 1 {
        "file "
    } else {
        "files"
    }
}

/// The preview, as in the design's mockup, followed by the first
/// [`PREVIEW_PATHS`] paths of each kind. The question is not included.
pub fn preview_lines(p: &Preview) -> Vec<Vec<Segment>> {
    preview_body(p, false)
}

/// The preview as the confirmation screen shows it, a shortened list
/// saying which key shows the whole of it, together with those lists whole,
/// so the key never comes without them.
pub fn preview_screen(p: &Preview) -> (Vec<Vec<Segment>>, Vec<FullList>) {
    (preview_body(p, true), preview_full_lists(p))
}

/// What follows `and 27 more` on the screen, where `l` opens the lists.
pub const MORE_KEY: &str = "l whole list";

/// `and 27 more`, for a list `len` long cut to [`PREVIEW_PATHS`], with the
/// key when `keyed`.
fn more_line(len: usize, keyed: bool) -> Vec<Segment> {
    let mut text = format!("    and {} more", thousands((len - PREVIEW_PATHS) as u64));
    if keyed {
        text.push_str(&format!(" {} {MORE_KEY}", theme::DOT));
    }
    vec![seg(text, Tone::Dim)]
}

/// The count rows of the preview, in a column: New, Changed, Unchanged,
/// and Moved when moved files were found.
fn count_rows(p: &Preview) -> Vec<(&'static str, Vec<Segment>)> {
    let new = p.new.len() as u64;
    let changed = p.changed.len() as u64;
    let moved = p.moves.len() as u64;
    let count_w = [new, changed, p.unchanged, moved]
        .iter()
        .map(|n| thousands(*n).len())
        .max()
        .unwrap_or(1);
    let mut sizes = vec![p.new_bytes, p.changed_bytes];
    if moved > 0 {
        sizes.push(p.move_bytes);
    }
    let bytes_w = sizes
        .iter()
        .map(|b| format_bytes(*b).len())
        .max()
        .unwrap_or(1);
    let row = |label: &str, n: u64, bytes: Option<u64>, tone: Tone| {
        let mut text = format!("  {label:<11}{:>count_w$} {}", thousands(n), files(n));
        if let Some(b) = bytes {
            text.push_str(&format!("   {:>bytes_w$}", format_bytes(b)));
        }
        vec![seg(text.trim_end(), tone)]
    };
    let mut rows = vec![
        (
            "New",
            row(
                "New",
                new,
                Some(p.new_bytes),
                if new > 0 { Tone::Plain } else { Tone::Dim },
            ),
        ),
        (
            "Changed",
            row(
                "Changed",
                changed,
                Some(p.changed_bytes),
                if changed > 0 { Tone::Plain } else { Tone::Dim },
            ),
        ),
        ("Unchanged", row("Unchanged", p.unchanged, None, Tone::Dim)),
    ];
    if moved > 0 {
        let mut line = row("Moved", moved, Some(p.move_bytes), Tone::Plain);
        line.push(seg(format!("   {}", moved_note(p)), Tone::Dim));
        rows.push(("Moved", line));
    }
    rows
}

/// What the Moved row says about the moved files: found, to be moved, or
/// to be uploaded again.
pub fn moved_note(p: &Preview) -> String {
    match p.moves_choice {
        MovesChoice::Undecided => format!(
            "already on {} under another name",
            place(&p.destination_type)
        ),
        MovesChoice::Move => "will be moved".to_string(),
        MovesChoice::Upload => "will be uploaded again".to_string(),
    }
}

/// The question about moved files.
pub fn moves_question(p: &Preview) -> String {
    format!(
        "  Move them on {} instead of uploading them again? [Y/n] ",
        place(&p.destination_type)
    )
}

/// The rows that changed once the question about moved files was
/// answered (New and Moved), for plain output, where the preview above is
/// already printed.
pub fn moves_answer_lines(p: &Preview) -> Vec<Vec<Segment>> {
    count_rows(p)
        .into_iter()
        .filter(|(label, _)| matches!(*label, "New" | "Moved"))
        .map(|(_, line)| line)
        .collect()
}

/// What was decided about the moved files, and how, in words.
pub fn moves_decision(p: &Preview, answer: MovesAnswer, by: DecidedBy) -> String {
    let n = p.moves.len() as u64;
    let place = place(&p.destination_type);
    let files = if n == 1 { "file" } else { "files" };
    let (will, stays) = match (p.dry_run, n == 1) {
        (true, _) => ("would be", "would stay"),
        (false, true) => ("will be", "stays"),
        (false, false) => ("will be", "stay"),
    };
    let what = match answer {
        MovesAnswer::Move => format!(
            "{} moved {files}: {will} moved on {place}, not uploaded again",
            thousands(n)
        ),
        MovesAnswer::Copy => format!(
            "{} moved {files}: {will} uploaded again, the old {} {stays} on {place}",
            thousands(n),
            if n == 1 { "copy" } else { "copies" }
        ),
    };
    match by {
        DecidedBy::Flag => format!("{what}   (--moves)"),
        DecidedBy::Default => format!("{what}   (the default with --yes or without a terminal)"),
        DecidedBy::NotAsked => {
            format!("{what}   (--moves ask, and there is no terminal to ask on)")
        }
        DecidedBy::Asked => what,
    }
}

/// One line of the Moved list: `Projects/2025/  →  Archive/2025/   1,198
/// files`, the paths in columns `from_w` and `to_w` wide.
fn move_line(g: &MoveGroup, from_w: usize, to_w: usize) -> Vec<Segment> {
    vec![
        seg(
            format!(
                "    {}",
                move_paths(&g.old_shown(), &g.new_shown(), from_w, to_w)
            ),
            Tone::Plain,
        ),
        seg(format!("   {:>11}", move_detail(g)), Tone::Dim),
    ]
}

/// `old  →  new`, each path fitted to its column (shortened from the
/// front with `…` by display width when it is longer).
fn move_paths(old: &str, new: &str, from_w: usize, to_w: usize) -> String {
    format!(
        "{}  {}  {}",
        fit(old, from_w),
        theme::ACTION,
        fit(new, to_w)
    )
}

/// The columns a Moved row adds around its two paths: the indent, the
/// arrow and the size column.
const MOVE_ROW_FIXED: usize = 4 + 5 + 14;

/// The widest a Moved row may be: the run screen's text at 80 columns
/// (76), which also keeps plain output within 80.
pub const MOVE_ROW_WIDTH: usize = 76;

/// The widths of the old and new path columns, from the longest of each
/// (`old_w`, `new_w`) and the room the two share: each keeps its full
/// width when they fit, the shorter one keeps it when it is under half,
/// and otherwise they split the room.
pub fn move_columns(old_w: usize, new_w: usize, room: usize) -> (usize, usize) {
    let room = room.max(2);
    if old_w + new_w <= room {
        return (old_w, new_w);
    }
    let half = room / 2;
    if old_w <= half {
        (old_w, room - old_w)
    } else if new_w <= room - half {
        (room - new_w, new_w)
    } else {
        (half, room - half)
    }
}

/// A group's size column: the file count for a folder, the size for a file.
fn move_detail(g: &MoveGroup) -> String {
    if g.folder {
        plural(g.files, "file", "files")
    } else {
        format_bytes(g.bytes)
    }
}

fn preview_body(p: &Preview, keyed: bool) -> Vec<Vec<Segment>> {
    let mut lines = vec![
        vec![
            seg(theme::ACTION, Tone::Accent),
            seg(format!(" {}   ", p.destination), Tone::Bold),
            seg("one-way", Tone::Dim),
        ],
        vec![],
    ];
    lines.extend(count_rows(p).into_iter().map(|(_, line)| line));
    lines.extend(excluded_lines(&p.excludes, p.copy_hidden, 11));

    for (title, list, more) in [
        ("New files", &p.new, p.new.len()),
        ("Changed files", &p.changed, p.changed.len()),
    ] {
        if list.is_empty() {
            continue;
        }
        lines.push(vec![]);
        lines.push(vec![seg(format!("  {title}"), Tone::Bold)]);
        let list = sorted(list);
        let shown = &list[..list.len().min(PREVIEW_PATHS)];
        let width = shown
            .iter()
            .map(|f| crate::ui::width(&f.path))
            .max()
            .unwrap_or(0)
            .min(50);
        for f in shown {
            lines.push(path_line(f, width));
        }
        if more > PREVIEW_PATHS {
            lines.push(more_line(more, keyed));
        }
    }

    if !p.move_groups.is_empty() {
        lines.push(vec![]);
        lines.push(vec![seg(format!("  {MOVED_TITLE}"), Tone::Bold)]);
        let shown = &p.move_groups[..p.move_groups.len().min(PREVIEW_PATHS)];
        let column = |f: fn(&MoveGroup) -> String| {
            shown
                .iter()
                .map(|g| crate::ui::width(&f(g)))
                .max()
                .unwrap_or(0)
        };
        let (from_w, to_w) = move_columns(
            column(MoveGroup::old_shown),
            column(MoveGroup::new_shown),
            MOVE_ROW_WIDTH - MOVE_ROW_FIXED,
        );
        for g in shown {
            lines.push(move_line(g, from_w, to_w));
        }
        if p.move_groups.len() > PREVIEW_PATHS {
            lines.push(more_line(p.move_groups.len(), keyed));
        }
    }

    lines.push(vec![]);
    if p.moving() {
        paragraph(
            &mut lines,
            "Nothing will be deleted on the destination; moved files keep their content \
             under the new name.",
            Tone::Plain,
        );
    } else {
        lines.push(vec![seg(
            "  Nothing will be deleted on the destination.",
            Tone::Plain,
        )]);
    }
    // Files that move are not left alone: they get their new names.
    let moving = if p.moving() { p.moves.len() as u64 } else { 0 };
    let left_alone = p.destination_only.saturating_sub(moving);
    if left_alone > 0 {
        let n = left_alone;
        lines.push(vec![seg(
            format!(
                "  {} {} only on the destination {} left alone.",
                thousands(n),
                files(n).trim_end(),
                if n == 1 { "is" } else { "are" }
            ),
            Tone::Dim,
        )]);
    }
    lines
}

/// What a run leaves out, in three parts under an `Excluded` label `label_w`
/// wide: the job's own patterns, whether hidden files are copied, and a
/// short summary of the built-in list (the JSON carries every pattern).
pub fn excluded_lines(own: &[String], copy_hidden: bool, label_w: usize) -> Vec<Vec<Segment>> {
    // A pattern that is built in already is said once, in the summary.
    let own = crate::rclone::own_excludes(own, copy_hidden);
    let own = if own.is_empty() {
        "no patterns of the job's own".to_string()
    } else {
        own.join(", ")
    };
    let hidden = if copy_hidden { "copied" } else { "not copied" };
    let pad = " ".repeat(label_w);
    [
        format!("  {:<label_w$}{own}", "Excluded"),
        format!("  {pad}Hidden files: {hidden}"),
        format!("  {pad}Always excluded: {ALWAYS_EXCLUDED}"),
        format!("  {pad}{ALWAYS_EXCLUDED_MORE}"),
    ]
    .into_iter()
    .map(|text| vec![seg(text, Tone::Dim)])
    .collect()
}

/// The built-in excludes the preview names, over two lines so they fit 80
/// columns beside the two-way labels; the rest of
/// [`crate::rclone::BUILTIN_EXCLUDES`] is "other system files".
pub const ALWAYS_EXCLUDED: &str = ".DS_Store, ._*, Thumbs.db,";
pub const ALWAYS_EXCLUDED_MORE: &str = "desktop.ini and other system files";

/// Files in path order, so a preview reads like a folder listing.
fn sorted(list: &[PlannedFile]) -> Vec<PlannedFile> {
    let mut out = list.to_vec();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// A path with its size in a column `width` characters to the right.
fn path_line(f: &PlannedFile, width: usize) -> Vec<Segment> {
    let detail = if f.modtime_only {
        "time only".to_string()
    } else {
        format_bytes(f.size)
    };
    vec![
        seg(
            format!("    {}", crate::ui::pad(&f.path, width)),
            Tone::Plain,
        ),
        seg(format!("   {detail:>9}"), Tone::Dim),
    ]
}

/// The question under the preview.
pub const QUESTION: &str = "  Continue? [Y/n] ";

/// The title of the list of moved files.
pub const MOVED_TITLE: &str = "Moved files";

/// The preview as text with the given colour setting.
pub fn render_text(p: &Preview, colour: bool) -> String {
    let mut out = String::new();
    for line in preview_lines(p) {
        for (text, tone) in line {
            out.push_str(&theme::paint(&text, tone, colour));
        }
        out.push('\n');
    }
    out
}

/// Reads an answer typed at [`QUESTION`]: empty, `y` or `yes` go ahead.
pub fn parse_answer(text: &str) -> Answer {
    parse_answer_or(text, Answer::Yes)
}

/// Reads a yes or no, with `default` for an empty answer.
pub fn parse_answer_or(text: &str, default: Answer) -> Answer {
    match text.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => Answer::Yes,
        _ => Answer::No,
    }
}

// ------------------------------------------------------------ two-way

/// `1,284`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

/// Wraps a paragraph to `width` columns, each line indented by `indent`.
pub fn wrap(text: &str, width: usize, indent: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty()
            && indent.len() + line.chars().count() + 1 + word.chars().count() > width
        {
            lines.push(format!("{indent}{line}"));
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(format!("{indent}{line}"));
    }
    lines
}

/// Width plain-text paragraphs are wrapped to.
const PARAGRAPH: usize = 76;

fn paragraph(lines: &mut Vec<Vec<Segment>>, text: &str, tone: Tone) {
    for l in wrap(text, PARAGRAPH, "  ") {
        lines.push(vec![seg(l, tone)]);
    }
}

fn direction_row(label: &str, d: &Direction, label_w: usize) -> Vec<Segment> {
    let n = d.files() as u64;
    let tone = if n > 0 { Tone::Plain } else { Tone::Dim };
    let mut text = format!(
        "  {label:<label_w$}{:>6} {}   {:>9}",
        thousands(n),
        files(n),
        format_bytes(d.bytes)
    );
    if n > 0 {
        text.push_str(&format!(
            "   {} new, {} changed",
            thousands(d.new.len() as u64),
            thousands(d.changed.len() as u64)
        ));
    }
    vec![seg(text, tone)]
}

fn count_row(label: &str, n: u64, label_w: usize, tone: Tone) -> Vec<Segment> {
    let tone = if n > 0 { tone } else { Tone::Dim };
    vec![seg(
        format!("  {label:<label_w$}{:>6} {}", thousands(n), files(n))
            .trim_end()
            .to_string(),
        tone,
    )]
}

fn file_list(lines: &mut Vec<Vec<Segment>>, title: &str, list: &[PlannedFile], keyed: bool) {
    if list.is_empty() {
        return;
    }
    lines.push(vec![]);
    lines.push(vec![seg(format!("  {title}"), Tone::Bold)]);
    let list = sorted(list);
    let shown = &list[..list.len().min(PREVIEW_PATHS)];
    let width = shown
        .iter()
        .map(|f| f.path.chars().count())
        .max()
        .unwrap_or(0)
        .min(50);
    for f in shown {
        lines.push(path_line(f, width));
    }
    if list.len() > PREVIEW_PATHS {
        lines.push(more_line(list.len(), keyed));
    }
}

/// What a folder answer means, in words.
pub fn folder_outcome(answer: FolderAnswer, kind: &str) -> String {
    let place = place(kind);
    match answer {
        FolderAnswer::Delete => format!("delete it on {place} too"),
        FolderAnswer::Keep => format!("keep it on {place}, stop syncing it"),
        FolderAnswer::Restore => format!("restore it to {} from {place}", this_computer()),
        FolderAnswer::CancelRun => "stop the run".to_string(),
    }
}

/// What was decided for a deleted folder, and how, in words.
pub fn folder_decision(answer: FolderAnswer, by: DecidedBy, kind: &str) -> String {
    let what = folder_outcome(answer, kind);
    match by {
        DecidedBy::Asked => what,
        DecidedBy::Flag => format!("{what}   (--folder-deletes)"),
        DecidedBy::Default => format!("{what}   (the default without a terminal)"),
        DecidedBy::NotAsked => format!(
            "a real run asks whether to delete it on {}, keep it there, or restore it here",
            place(kind)
        ),
    }
}

/// The two-way preview: counts and bytes per direction, deletions per side
/// (every destination-side deletion listed), conflicts and folder
/// decisions. The question is not included.
pub fn plan_lines(p: &Plan) -> Vec<Vec<Segment>> {
    plan_body(p, false)
}

/// The plan as the confirmation screen shows it, a shortened list saying
/// which key shows the whole of it, together with those lists whole, so
/// the key never comes without them.
pub fn plan_screen(p: &Plan) -> (Vec<Vec<Segment>>, Vec<FullList>) {
    (plan_body(p, true), plan_full_lists(p))
}

/// The files the plan sends each way, new and changed together, with the
/// titles the plan gives them and their bytes.
fn plan_directions(p: &Plan) -> [(String, Vec<PlannedFile>, u64); 2] {
    let place = place(&p.destination_type);
    let mut up = p.to_remote.new.clone();
    up.extend(p.to_remote.changed.iter().cloned());
    let mut down = p.to_local.new.clone();
    down.extend(p.to_local.changed.iter().cloned());
    [
        (
            format!("New and changed, to {place}"),
            up,
            p.to_remote.bytes,
        ),
        (
            format!("New and changed, to {}", this_computer()),
            down,
            p.to_local.bytes,
        ),
    ]
}

/// `this Mac`, or `this computer` on Linux.
fn this_computer() -> &'static str {
    crate::platform::words().this_computer
}

/// Title of the plan's list of deletions on this Mac.
fn delete_local() -> String {
    format!("Will be deleted on {}", this_computer())
}

fn plan_body(p: &Plan, keyed: bool) -> Vec<Vec<Segment>> {
    let place = place(&p.destination_type);
    let mut lines = vec![
        vec![
            seg(theme::ACTION, Tone::Accent),
            seg(format!(" {}   ", p.destination), Tone::Bold),
            seg("two-way", Tone::Dim),
        ],
        vec![],
    ];
    let to_place = format!("To {place}");
    let del_place = format!("Delete on {place}");
    let del_here = format!("Delete on {}", this_computer());
    let to_here = format!("To {}", this_computer());
    let label_w = del_place.len().max(del_here.len()) + 2;
    lines.push(direction_row(&to_place, &p.to_remote, label_w));
    lines.push(direction_row(&to_here, &p.to_local, label_w));
    lines.push(count_row(
        &del_place,
        p.delete_remote.len() as u64,
        label_w,
        Tone::Warning,
    ));
    lines.push(count_row(
        &del_here,
        p.delete_local.len() as u64,
        label_w,
        Tone::Warning,
    ));
    lines.push(count_row(
        "Conflicts",
        p.conflicts.len() as u64,
        label_w,
        Tone::Plain,
    ));
    lines.extend(excluded_lines(&p.excludes, p.copy_hidden, label_w));
    if !p.holds.is_empty() {
        lines.push(vec![seg(
            format!(
                "  {:<label_w$}{}",
                "On hold",
                crate::ui::quoted_list(&p.holds)
            ),
            Tone::Dim,
        )]);
    }

    if !p.folders.is_empty() {
        lines.push(vec![]);
        lines.push(vec![seg(
            format!("  Folders deleted on {}", this_computer()),
            Tone::Bold,
        )]);
        for d in &p.folders {
            lines.push(vec![
                seg(
                    format!("    {}   ", crate::ui::quoted(&d.folder.path)),
                    Tone::Plain,
                ),
                seg(
                    folder_decision(d.answer, d.by, &p.destination_type),
                    Tone::Dim,
                ),
            ]);
        }
    }
    if !p.delete_remote.is_empty() {
        lines.push(vec![]);
        lines.push(vec![seg(
            format!("  Will be deleted on {place}"),
            Tone::Bold,
        )]);
        let mut remote = p.delete_remote.clone();
        remote.sort();
        for path in &remote {
            lines.push(vec![seg(format!("    {path}"), Tone::Warning)]);
        }
    }
    if !p.delete_local.is_empty() {
        lines.push(vec![]);
        lines.push(vec![seg(format!("  {}", delete_local()), Tone::Bold)]);
        let mut local = p.delete_local.clone();
        local.sort();
        for path in local.iter().take(PREVIEW_PATHS) {
            lines.push(vec![seg(format!("    {path}"), Tone::Warning)]);
        }
        if p.delete_local.len() > PREVIEW_PATHS {
            lines.push(more_line(p.delete_local.len(), keyed));
        }
    }
    if !p.conflicts.is_empty() {
        lines.push(vec![]);
        lines.push(vec![seg(
            format!("  Conflicts: the local copy wins, {place} keeps its copy under a new name"),
            Tone::Bold,
        )]);
        for c in &p.conflicts {
            let kept = c
                .kept_as
                .as_deref()
                .map(|k| format!("   {} {k}", theme::ACTION))
                .unwrap_or_default();
            lines.push(vec![
                seg(format!("    {}", c.path), Tone::Plain),
                seg(kept, Tone::Dim),
            ]);
        }
    }
    for (title, list, _) in plan_directions(p) {
        file_list(&mut lines, &title, &list, keyed);
    }

    lines.push(vec![]);
    if p.delete_remote.is_empty() && p.delete_local.is_empty() {
        lines.push(vec![seg("  Nothing will be deleted.", Tone::Plain)]);
    }
    for big in &p.big {
        let side = match big.side {
            Where::Destination => place.to_string(),
            Where::Local => this_computer().to_string(),
        };
        let text = match big.kind {
            BigKind::Deletions => format!(
                "More than half would be deleted on {side}: {} of {} files and folders ({}). \
                 Confirming lifts bisync's safety limit for this run.",
                thousands(big.count),
                thousands(big.total),
                big.percent_text()
            ),
            BigKind::AllChanged => format!(
                "Every file changed ({}), so every one goes to {side}. Confirming lifts \
                 bisync's safety check for this run.",
                thousands(big.count)
            ),
        };
        paragraph(&mut lines, &text, Tone::Error);
    }
    lines
}

/// The design's words for a first run.
pub const FIRST_RUN: &str = "This is the first two-way run for this destination. OneFolder will \
    merge both sides: files that exist on only one side are copied to the other, and where the \
    same file differs the local copy wins (the other copy is kept with a .conflict suffix). \
    Nothing is deleted during a first run.";

/// The merge explanation for any other reason.
const MERGE: &str = "OneFolder will merge both sides: files that exist on only one side are \
    copied to the other, and where the same file differs the local copy wins (the other copy is \
    kept with a .conflict suffix). Nothing is deleted by a merge.";

/// A first run or a recovery: why, what the merge does, and its counts.
pub fn resync_lines(o: &ResyncOffer) -> Vec<Vec<Segment>> {
    let place = place(&o.destination_type);
    let mut lines = vec![
        vec![
            seg(theme::ACTION, Tone::Accent),
            seg(format!(" {}   ", o.destination), Tone::Bold),
            seg("two-way   ", Tone::Dim),
            seg(
                match o.reason {
                    Some(ResyncReason::FirstRun) | None => "first run",
                    _ => "merge",
                },
                Tone::Warning,
            ),
        ],
        vec![],
    ];
    let why = match o.reason {
        Some(ResyncReason::FirstRun) | None => FIRST_RUN.to_string(),
        Some(ResyncReason::PairChanged) => format!(
            "The job's source or this destination's folder changed since the last two-way run, \
             so the sync state describes another pair. {MERGE}"
        ),
        Some(ResyncReason::Damaged) => {
            format!("The sync state for this destination was lost or damaged. {MERGE}")
        }
        Some(ResyncReason::Critical) => format!(
            "The last two-way run stopped on a critical error, so the sync state has to be \
             rebuilt. {MERGE}"
        ),
        Some(ResyncReason::Requested) => format!("A fresh merge was asked for (--resync). {MERGE}"),
    };
    paragraph(&mut lines, &why, Tone::Plain);
    if let Some(detail) = &o.detail {
        paragraph(&mut lines, detail, Tone::Dim);
    }
    lines.push(vec![]);
    let label = format!("To {place}");
    let to_here = format!("To {}", this_computer());
    let label_w = label.len().max(to_here.len()) + 2;
    if o.destination_missing {
        paragraph(
            &mut lines,
            &format!(
                "The destination folder does not exist yet, so it is empty: it will be \
                 created, and everything here copied to {place}."
            ),
            Tone::Plain,
        );
        lines.push(vec![]);
        lines.push(direction_row(&label, &o.to_remote, label_w));
        return lines;
    }
    lines.push(direction_row(&label, &o.to_remote, label_w));
    lines.push(direction_row(&to_here, &o.to_local, label_w));
    if !o.kept.is_empty() {
        lines.push(vec![]);
        lines.push(vec![seg(
            format!("  Differ on {place}: the local copy wins, {place} keeps its copy as"),
            Tone::Bold,
        )]);
        for k in &o.kept {
            lines.push(vec![
                seg(format!("    {}", k.path), Tone::Plain),
                seg(
                    format!(
                        "   {} {}",
                        theme::ACTION,
                        k.kept_as.as_deref().unwrap_or("?")
                    ),
                    Tone::Dim,
                ),
            ]);
        }
    }
    lines
}

/// The lines above a yes/no question (empty for the plan and the merge,
/// which were shown already), the question, and its answer on Enter.
pub fn question(q: &Question, kind: &str) -> (Vec<Vec<Segment>>, String, Answer) {
    match q {
        Question::Moves(p) => (Vec::new(), moves_question(p), Answer::Yes),
        Question::Plan(p) if p.needs_confirmation() => {
            let deletes = p.big.iter().any(|b| b.kind == BigKind::Deletions);
            let q = if deletes {
                "  Go ahead, with those deletions? [y/N] "
            } else {
                "  Go ahead, copying every changed file? [y/N] "
            };
            (Vec::new(), q.into(), Answer::No)
        }
        Question::Plan(_) => (Vec::new(), QUESTION.into(), Answer::Yes),
        Question::Resync(_) => (
            Vec::new(),
            "  Merge both sides now? [Y/n] ".into(),
            Answer::Yes,
        ),
        Question::StaleLock { pid, path } => (
            vec![
                vec![
                    seg(theme::SKIPPED, Tone::Warning),
                    seg(" A stale lock is in the way", Tone::Bold),
                ],
                vec![seg(
                    format!("  Process {pid} left {path} and is no longer running."),
                    Tone::Plain,
                )],
                vec![],
            ],
            "  Remove it and carry on? [Y/n] ".into(),
            Answer::Yes,
        ),
        Question::Retry(abort) => {
            let what = match abort {
                Abort::AllChanged { .. } => "bisync's all-files-changed check",
                _ => "bisync's safety limit on deletions",
            };
            let _ = kind;
            (
                Vec::new(),
                format!("  Run again with {what} lifted? [y/N] "),
                Answer::No,
            )
        }
    }
}

/// Lines as text with the given colour setting.
pub fn lines_text(lines: &[Vec<Segment>], colour: bool) -> String {
    let mut out = String::new();
    for line in lines {
        for (text, tone) in line {
            out.push_str(&theme::paint(text, *tone, colour));
        }
        out.push('\n');
    }
    out
}

pub fn render_plan_text(p: &Plan, colour: bool) -> String {
    lines_text(&plan_lines(p), colour)
}

pub fn render_resync_text(o: &ResyncOffer, colour: bool) -> String {
    lines_text(&resync_lines(o), colour)
}

fn to_line(segments: Vec<Segment>) -> Line<'static> {
    Line::from(
        segments
            .into_iter()
            .map(|(text, tone)| Span::styled(text, theme::style(tone)))
            .collect::<Vec<_>>(),
    )
}

// ------------------------------------------------------- whole lists

/// A list the preview shortened to [`PREVIEW_PATHS`], whole, for the
/// confirmation screen's `l` view.
#[derive(Debug, Clone)]
pub struct FullList {
    /// The title the preview gives the list, such as `New files`.
    pub title: String,
    /// The size of the whole list, where sizes are known.
    pub total_bytes: Option<u64>,
    /// Every file, sorted as the preview sorts them.
    pub files: Vec<PlannedFile>,
    /// False for a list of paths only (deletions), which has no size column.
    pub sized: bool,
    /// The tone of the paths: plain, or a warning for deletions.
    pub tone: Tone,
    /// The paths in lower case, made once, for the filter.
    lower: Vec<String>,
    /// The longest path, in characters.
    width: usize,
    /// The size column's text per file, when it is not the file's size
    /// (the Moved list: a size, or a folder's file count).
    details: Vec<String>,
    /// The number the heading gives, when it is not the number of rows
    /// (the Moved list counts files, and a folder is one row).
    count: Option<u64>,
    /// The Moved list: each row's old and new path, fitted to the width
    /// when drawn, and the longest of each.
    moves: Vec<(String, String)>,
    move_widths: (usize, usize),
}

impl FullList {
    pub fn new(
        title: impl Into<String>,
        total_bytes: Option<u64>,
        files: Vec<PlannedFile>,
        sized: bool,
        tone: Tone,
    ) -> Self {
        let mut files = files;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let lower = files.iter().map(|f| filter_key(&f.path)).collect();
        let width = files
            .iter()
            .map(|f| crate::ui::width(&f.path))
            .max()
            .unwrap_or(0);
        FullList {
            title: title.into(),
            total_bytes,
            files,
            sized,
            tone,
            lower,
            width,
            details: Vec::new(),
            count: None,
            moves: Vec::new(),
            move_widths: (0, 0),
        }
    }

    /// The Moved list: one row per group, `from  →  to`, in the order the
    /// preview shows them, with the size or the folder's file count.
    pub fn moves(p: &Preview) -> Self {
        let moves: Vec<(String, String)> = p
            .move_groups
            .iter()
            .map(|g| (g.old_shown(), g.new_shown()))
            .collect();
        // The text the filter searches: both paths.
        let files: Vec<PlannedFile> = moves
            .iter()
            .zip(&p.move_groups)
            .map(|((old, new), g)| PlannedFile {
                path: format!("{old}  {}  {new}", theme::ACTION),
                size: g.bytes,
                modtime_only: false,
            })
            .collect();
        let lower = files.iter().map(|f| filter_key(&f.path)).collect();
        let column = |f: fn(&(String, String)) -> &String| {
            moves
                .iter()
                .map(|m| crate::ui::width(f(m)))
                .max()
                .unwrap_or(0)
        };
        let move_widths = (column(|m| &m.0), column(|m| &m.1));
        FullList {
            moves,
            move_widths,
            title: MOVED_TITLE.to_string(),
            total_bytes: Some(p.move_bytes),
            details: p.move_groups.iter().map(move_detail).collect(),
            count: Some(p.moves.len() as u64),
            files,
            sized: true,
            tone: Tone::Plain,
            lower,
            width: 0,
        }
    }

    /// A list of paths without sizes.
    fn paths(title: impl Into<String>, paths: &[String], tone: Tone) -> Self {
        let files = paths
            .iter()
            .map(|p| PlannedFile {
                path: p.clone(),
                ..PlannedFile::default()
            })
            .collect();
        FullList::new(title, None, files, false, tone)
    }

    /// `New files · 29,152 files · 9.4 GB`.
    pub fn heading(&self) -> String {
        let d = theme::DOT;
        let mut text = format!(
            "{} {d} {}",
            self.title,
            plural(
                self.count.unwrap_or(self.files.len() as u64),
                "file",
                "files"
            )
        );
        if let Some(b) = self.total_bytes {
            text.push_str(&format!(" {d} {}", format_bytes(b)));
        }
        text
    }
}

/// The one-way preview's lists that [`preview_lines`] shortens, whole.
pub fn preview_full_lists(p: &Preview) -> Vec<FullList> {
    let mut out: Vec<FullList> = [
        ("New files", &p.new, p.new_bytes),
        ("Changed files", &p.changed, p.changed_bytes),
    ]
    .into_iter()
    .filter(|(_, list, _)| list.len() > PREVIEW_PATHS)
    .map(|(title, list, bytes)| FullList::new(title, Some(bytes), list.clone(), true, Tone::Plain))
    .collect();
    if p.move_groups.len() > PREVIEW_PATHS {
        out.push(FullList::moves(p));
    }
    out
}

/// The two-way plan's lists that [`plan_lines`] shortens, whole, in the
/// order the plan shows them.
pub fn plan_full_lists(p: &Plan) -> Vec<FullList> {
    let mut out = Vec::new();
    if p.delete_local.len() > PREVIEW_PATHS {
        out.push(FullList::paths(
            delete_local(),
            &p.delete_local,
            Tone::Warning,
        ));
    }
    for (title, list, bytes) in plan_directions(p) {
        if list.len() > PREVIEW_PATHS {
            out.push(FullList::new(title, Some(bytes), list, true, Tone::Plain));
        }
    }
    out
}

/// A path or filter text as the filter compares them: composed (NFC, so a
/// decomposed `é` from macOS matches a typed one) and in lower case.
fn filter_key(text: &str) -> String {
    crate::sync::nfc(text).to_lowercase()
}

/// One row of the whole-lists view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullRow {
    Blank,
    /// The heading of a list.
    Title(usize),
    /// A file of a list; `nth` counts the files shown, from 0.
    File {
        list: usize,
        file: usize,
        nth: usize,
    },
}

/// The rows of `lists` whose paths contain `filter` (lower case), with a
/// heading over each list that has any, and the number of files shown.
pub fn full_rows(lists: &[FullList], filter: &str) -> (Vec<FullRow>, usize) {
    let mut rows = Vec::new();
    let mut nth = 0;
    for (i, l) in lists.iter().enumerate() {
        let start = rows.len();
        for (j, lower) in l.lower.iter().enumerate() {
            if !filter.is_empty() && !lower.contains(filter) {
                continue;
            }
            if rows.len() == start {
                if start > 0 {
                    rows.push(FullRow::Blank);
                }
                rows.push(FullRow::Title(i));
            }
            rows.push(FullRow::File {
                list: i,
                file: j,
                nth,
            });
            nth += 1;
        }
    }
    (rows, nth)
}

/// The text of `rows` (only the rows on screen), `width` columns wide:
/// headings in bold, then each path with its size in a column, long paths
/// shortened from the front so the column stays put.
pub fn full_lines(lists: &[FullList], rows: &[FullRow], width: usize) -> Vec<Vec<Segment>> {
    rows.iter()
        .map(|row| match *row {
            FullRow::Blank => vec![],
            FullRow::Title(i) => vec![seg(format!("  {}", lists[i].heading()), Tone::Bold)],
            FullRow::File { list, file, .. } => {
                let l = &lists[list];
                let f = &l.files[file];
                if !l.moves.is_empty() {
                    let room = width.saturating_sub(MOVE_ROW_FIXED);
                    let (old_w, new_w) = move_columns(l.move_widths.0, l.move_widths.1, room);
                    let (old, new) = &l.moves[file];
                    vec![
                        seg(
                            format!("    {}", move_paths(old, new, old_w, new_w)),
                            l.tone,
                        ),
                        seg(format!("   {:>11}", l.details[file]), Tone::Dim),
                    ]
                } else if l.sized {
                    let room = width.saturating_sub(4 + 12).max(1);
                    let w = l.width.clamp(1, room);
                    let mut line = path_line(f, 0);
                    line[0] = seg(format!("    {}", fit(&f.path, w)), l.tone);
                    line
                } else {
                    let w = width.saturating_sub(4).max(1);
                    vec![seg(format!("    {}", fit(&f.path, w).trim_end()), l.tone)]
                }
            }
        })
        .collect()
}

/// Where the whole-lists view is: `1–38 of 29,152`, or with a filter
/// `1–12 of 12 matches · 29,152 in all`. `first` and `last` count from 1.
pub fn position_text(
    first: usize,
    last: usize,
    shown: usize,
    total: usize,
    filtered: bool,
) -> String {
    let d = theme::DOT;
    let all = thousands(total as u64);
    let range = if first == last {
        thousands(first as u64)
    } else {
        format!("{}–{}", thousands(first as u64), thousands(last as u64))
    };
    if !filtered {
        return format!("{range} of {all}");
    }
    if shown == 0 {
        return format!("no matches {d} {all} in all");
    }
    let matches = if shown == 1 { "match" } else { "matches" };
    format!(
        "{range} of {} {matches} {d} {all} in all",
        thousands(shown as u64)
    )
}

/// The whole-lists view over a confirmation screen: its rows, scroll and
/// filter.
#[derive(Debug, Clone)]
struct FullView {
    /// Rows scrolled off the top.
    scroll: usize,
    /// Text the paths are filtered by (`/`), as typed.
    filter: String,
    /// The filter is being typed.
    filtering: bool,
    rows: Vec<FullRow>,
    /// Files the filter lets through.
    shown: usize,
    /// Rows of the list as last drawn, for paging and the end.
    height: std::cell::Cell<usize>,
}

impl FullView {
    fn new(lists: &[FullList]) -> Self {
        let (rows, shown) = full_rows(lists, "");
        FullView {
            scroll: 0,
            filter: String::new(),
            filtering: false,
            rows,
            shown,
            height: std::cell::Cell::new(10),
        }
    }

    fn refilter(&mut self, lists: &[FullList]) {
        let (rows, shown) = full_rows(lists, &filter_key(&self.filter));
        self.rows = rows;
        self.shown = shown;
        self.scroll = 0;
    }

    /// Rows a page moves: one less than the list's height, as a heading
    /// pinned at the top can take a row.
    fn page(&self) -> usize {
        self.height.get().saturating_sub(1).max(1)
    }

    /// The furthest the rows scroll: none when they all fit, otherwise far
    /// enough that the last row shows under a pinned heading.
    fn max_scroll(&self) -> usize {
        let len = self.rows.len();
        if len <= self.height.get() {
            0
        } else {
            len - self.page()
        }
    }

    /// Applies one key; true when the view should close.
    fn handle_key(&mut self, key: KeyEvent, lists: &[FullList]) -> bool {
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.refilter(lists);
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refilter(lists);
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.refilter(lists);
                }
                _ => {}
            }
            return false;
        }
        let page = self.page();
        let max = self.max_scroll();
        match key.code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refilter(lists);
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => return true,
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Down | KeyCode::Char('j') => self.scroll = (self.scroll + 1).min(max),
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.min(max).saturating_sub(1)
            }
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll = (self.scroll + page).min(max),
            KeyCode::PageUp => self.scroll = self.scroll.min(max).saturating_sub(page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = max,
            _ => {}
        }
        false
    }

    /// The first row on screen.
    fn start(&self) -> usize {
        self.scroll.min(self.max_scroll())
    }

    /// The rows on screen, `height` of them from the scroll offset, and
    /// the list whose heading is pinned above them when the first row is a
    /// file in the middle of a list (that heading takes one row).
    fn window(&self, height: usize) -> (Option<usize>, &[FullRow]) {
        let start = self.start().min(self.rows.len());
        let pinned = match self.rows.get(start) {
            Some(FullRow::File { list, .. }) if height >= 2 => Some(*list),
            _ => None,
        };
        let room = if pinned.is_some() { height - 1 } else { height };
        let end = (start + room).min(self.rows.len());
        (pinned, &self.rows[start..end])
    }

    /// The position line: the first file at or after the scroll offset,
    /// and the last file on screen.
    fn position(&self, height: usize, total: usize) -> String {
        let nth = |r: &FullRow| match r {
            FullRow::File { nth, .. } => Some(*nth + 1),
            _ => None,
        };
        let first = self.rows[self.start().min(self.rows.len())..]
            .iter()
            .find_map(nth)
            .unwrap_or(0);
        let last = self
            .window(height)
            .1
            .iter()
            .rev()
            .find_map(nth)
            .unwrap_or(first)
            .max(first);
        position_text(first, last, self.shown, total, !self.filter.is_empty())
    }

    fn hints(&self) -> String {
        let d = theme::DOT;
        let (up, down) = (theme::UP, theme::DOWN);
        if self.filtering {
            format!("type to filter {d} enter keep {d} esc clear")
        } else if self.filter.is_empty() {
            format!("esc/l back {d} {up}{down} pgup/pgdn scroll {d} g/G top/bottom {d} / filter")
        } else {
            format!(
                "esc clear {d} l back {d} {up}{down} pgup/pgdn scroll {d} g/G top/bottom {d} / filter"
            )
        }
    }
}

/// A confirmation screen: lines (a preview, a plan), a question and the
/// keys. Enter gives the question's default. When the preview shortened a
/// list, `l` opens every such list whole ([`FullList`]), to scroll and
/// filter; the question waits until the view is closed again.
#[derive(Debug, Clone)]
pub struct PreviewPrompt {
    /// The run screen's title, such as `Run documents`.
    pub title: String,
    pub body: Vec<Vec<Segment>>,
    pub question: String,
    /// The answer on Enter.
    pub default: Answer,
    /// What yes does, for the key hints: `copy`, `sync`, `merge`...
    pub verb: String,
    /// Lines scrolled off the top, for previews taller than the screen.
    pub scroll: u16,
    /// The furthest the body can scroll, as last drawn (wrapped lines
    /// counted), so the keys never scroll past the end.
    pub max_scroll: std::cell::Cell<u16>,
    /// The lists the body shortened, whole, for `l`.
    pub full: Vec<FullList>,
    /// The whole-lists view, while it is open.
    view: Option<FullView>,
    /// What the hints say no does, when it is not "skip this
    /// destination" (the move question: `n upload again`).
    pub no_hint: Option<String>,
}

impl PreviewPrompt {
    /// The one-way preview.
    pub fn new(title: String, preview: Preview) -> Self {
        let (body, full) = preview_screen(&preview);
        PreviewPrompt::with_lines(title, body, QUESTION.to_string(), Answer::Yes, "copy")
            .with_full_lists(full)
    }

    /// Gives `l` the lists the body shortened, such as
    /// the lists [`plan_screen`] gives with its lines.
    pub fn with_full_lists(mut self, full: Vec<FullList>) -> Self {
        self.full = full;
        self
    }

    pub fn with_lines(
        title: String,
        body: Vec<Vec<Segment>>,
        question: String,
        default: Answer,
        verb: &str,
    ) -> Self {
        PreviewPrompt {
            title,
            body,
            question,
            default,
            verb: verb.to_string(),
            scroll: 0,
            max_scroll: std::cell::Cell::new(u16::MAX),
            full: Vec::new(),
            view: None,
            no_hint: None,
        }
    }

    /// The question about moved files, over the preview: yes moves them,
    /// no (also Esc and q) uploads them again; Ctrl-C stops the run.
    pub fn moves(title: String, preview: &Preview) -> Self {
        let (body, full) = preview_screen(preview);
        let mut prompt =
            PreviewPrompt::with_lines(title, body, moves_question(preview), Answer::Yes, "move")
                .with_full_lists(full);
        prompt.no_hint = Some("n upload again".to_string());
        prompt
    }

    /// Applies one key. Enter gives the default; y yes; n, Esc, q and
    /// Ctrl-C no; l opens the whole lists. While they are open only
    /// Ctrl-C answers; Esc, q and l close them.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Answer> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Answer::No);
        }
        if let Some(view) = &mut self.view {
            if view.handle_key(key, &self.full) {
                self.view = None;
            }
            return None;
        }
        match key.code {
            KeyCode::Char('l') if !self.full.is_empty() => {
                self.view = Some(FullView::new(&self.full));
                None
            }
            KeyCode::Enter => Some(self.default),
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(Answer::Yes),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                Some(Answer::No)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1).min(self.max_scroll.get());
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.min(self.max_scroll.get()).saturating_sub(1);
                None
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll = self.scroll.saturating_add(10).min(self.max_scroll.get());
                None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.min(self.max_scroll.get()).saturating_sub(10);
                None
            }
            _ => None,
        }
    }

    /// The scrolling part: the title and the body.
    pub fn lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(Span::styled(self.title.clone(), theme::style(Tone::Bold))),
            Line::default(),
        ];
        lines.extend(self.body.iter().cloned().map(to_line));
        lines
    }

    /// The question, pinned under the scrolling part so it stays in view
    /// however long the list above it is.
    pub fn pinned(&self) -> Vec<Line<'static>> {
        vec![
            Line::default(),
            Line::from(Span::styled(
                self.question.trim_end().to_string(),
                theme::style(Tone::Accent),
            )),
        ]
    }

    fn hints(&self) -> String {
        let d = theme::DOT;
        let (yes, no) = if self.default == Answer::Yes {
            ("y/enter", "n/esc")
        } else {
            ("y", "n/enter/esc")
        };
        let full = if self.full.is_empty() {
            String::new()
        } else {
            format!("l whole list {d} ")
        };
        let no = match &self.no_hint {
            Some(hint) => hint.clone(),
            None => format!("{no} skip this destination"),
        };
        format!(
            "{yes} {} {d} {no} {d} {full}{}{} scroll",
            self.verb,
            theme::UP,
            theme::DOWN
        )
    }

    pub fn render(&self, frame: &mut Frame) {
        if let Some(view) = &self.view {
            self.render_full(frame, view);
            return;
        }
        let max = render_scrolling(
            frame,
            self.lines(),
            self.pinned(),
            self.scroll,
            &self.hints(),
        );
        self.max_scroll.set(max);
    }

    /// The whole-lists view: the title, where the view is (and the filter),
    /// the rows on screen under the heading of their list, and the keys.
    /// Only the rows on screen are made, so a list of 100,000 paths draws
    /// as fast as one of ten. On a tiny terminal the keys still show.
    fn render_full(&self, frame: &mut Frame, view: &FullView) {
        let area: Rect = frame.area().inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        if area.height == 0 {
            return;
        }
        // Title, position, blank, the rows, blank, the keys.
        let height = usize::from(area.height).saturating_sub(5);
        view.height.set(height);
        let total = self.full.iter().map(|l| l.files.len()).sum();
        let mut place = vec![seg(
            format!("  {}", view.position(height, total)),
            Tone::Dim,
        )];
        if view.filtering || !view.filter.is_empty() {
            let cursor = if view.filtering { "▏" } else { "" };
            place.push(seg(
                format!(" {} /{}{cursor}", theme::DOT, view.filter),
                Tone::Accent,
            ));
        }
        let mut lines = vec![
            Line::from(Span::styled(self.title.clone(), theme::style(Tone::Bold))),
            to_line(place),
            Line::default(),
        ];
        if view.shown == 0 {
            lines.push(to_line(vec![seg(
                format!(
                    "  {} no path matches {} esc clears the filter",
                    theme::SKIPPED,
                    theme::DOT
                ),
                Tone::Dim,
            )]));
        }
        let (pinned, rows) = view.window(height);
        if let Some(list) = pinned {
            lines.extend(
                full_lines(&self.full, &[FullRow::Title(list)], 0)
                    .into_iter()
                    .map(to_line),
            );
        }
        lines.extend(
            full_lines(&self.full, rows, usize::from(area.width))
                .into_iter()
                .map(to_line),
        );
        if area.height > 1 {
            frame.render_widget(
                Paragraph::new(lines),
                Rect {
                    height: area.height.saturating_sub(2).max(1),
                    ..area
                },
            );
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                view.hints(),
                theme::style(Tone::Dim),
            ))),
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
        );
    }
}

/// Draws lines with a scroll offset, `pinned` lines under them that never
/// scroll, and a hint line at the bottom. Gives back the furthest the
/// lines can scroll, counting the rows wrapped lines take.
fn render_scrolling(
    frame: &mut Frame,
    lines: Vec<Line<'static>>,
    pinned: Vec<Line<'static>>,
    scroll: u16,
    hints: &str,
) -> u16 {
    let area: Rect = frame.area().inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    if area.height < 3 {
        return 0;
    }
    let pinned_height = u16::try_from(pinned.len())
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(3));
    let body = Rect {
        height: area.height - 2 - pinned_height,
        ..area
    };
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let rows = u16::try_from(paragraph.line_count(body.width)).unwrap_or(u16::MAX);
    let max_scroll = rows.saturating_sub(body.height);
    frame.render_widget(paragraph.scroll((scroll.min(max_scroll), 0)), body);
    if pinned_height > 0 {
        frame.render_widget(
            Paragraph::new(pinned).wrap(Wrap { trim: false }),
            Rect {
                y: body.y + body.height,
                height: pinned_height,
                ..area
            },
        );
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hints.to_string(),
            theme::style(Tone::Dim),
        ))),
        Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        },
    );
    max_scroll
}

// ---------------------------------------------------- folder prompt

const CHOICES: [FolderAnswer; 3] = [
    FolderAnswer::Delete,
    FolderAnswer::Keep,
    FolderAnswer::Restore,
];

/// The three choices, as the mockup words them.
pub fn choice_labels(kind: &str) -> [String; 3] {
    let place = place(kind);
    [
        format!("Delete it on {place} too, with the hidden and system files in it"),
        format!("Keep it on {place} and stop syncing this folder"),
        format!("Restore it to {} from {place}", this_computer()),
    ]
}

/// One deleted folder: its name, its size on the destination, the rename
/// hint. Shared by the screen and the plain prompt.
pub fn folder_lines(kind: &str, f: &DeletedFolder, n: usize, of: usize) -> Vec<Vec<Segment>> {
    let place = place(kind);
    let title = if of > 1 {
        format!(" Folder deleted locally ({n} of {of})")
    } else {
        " Folder deleted locally".to_string()
    };
    let mut lines = vec![
        vec![seg(theme::CURSOR, Tone::Accent), seg(title, Tone::Bold)],
        vec![],
        vec![seg(format!("  {}", f.path), Tone::Bold)],
    ];
    let size = format!(
        "{}, {}",
        plural(f.files, "file", "files"),
        format_bytes(f.bytes)
    );
    let on = if f.measured {
        format!("  On {place}: {size}")
    } else {
        format!("  As last synced: {size}")
    };
    lines.push(vec![seg(on, Tone::Plain)]);
    if f.maybe_rename {
        paragraph(
            &mut lines,
            &format!(
                "This may be a rename: the plan also adds {} on {}. Deleting is then \
                 right, as the renamed copy goes up too.",
                plural(f.files, "new file", "new files"),
                this_computer()
            ),
            Tone::Dim,
        );
    }
    lines.push(vec![]);
    lines.push(vec![seg(
        format!(
            "  Not on {} any more. What should happen on {place}?",
            crate::platform::words().your_computer
        ),
        Tone::Plain,
    )]);
    lines
}

/// The folder-deletion prompt: one folder at a time, or, for more than
/// [`LIST_PROMPT_OVER`], one list with an answer for all and per-folder
/// overrides.
#[derive(Debug, Clone)]
pub struct FolderPrompt {
    pub title: String,
    pub kind: String,
    pub folders: Vec<DeletedFolder>,
    /// One per folder: an answer given, or (list mode) an override.
    pub answers: Vec<Option<FolderAnswer>>,
    /// The highlighted choice (one-at-a-time), or the answer for all (list).
    pub choice: usize,
    /// The current folder.
    pub current: usize,
    pub scroll: u16,
}

impl FolderPrompt {
    pub fn new(title: String, kind: &str, folders: Vec<DeletedFolder>) -> Self {
        let n = folders.len();
        FolderPrompt {
            title,
            kind: kind.to_string(),
            folders,
            answers: vec![None; n],
            // Keep is highlighted first, the safe answer: deleting needs a
            // deliberate move.
            choice: 1,
            current: 0,
            scroll: 0,
        }
    }

    pub fn list_mode(&self) -> bool {
        self.folders.len() > LIST_PROMPT_OVER
    }

    /// Applies one key; the answers once every folder has one. Esc or
    /// Ctrl-C cancel the run.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Vec<FolderAnswer>> {
        let cancel = vec![FolderAnswer::CancelRun; self.folders.len()];
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(cancel);
        }
        if key.code == KeyCode::Esc || key.code == KeyCode::Char('q') {
            return Some(cancel);
        }
        if self.list_mode() {
            let last = self.folders.len() - 1;
            match key.code {
                KeyCode::Left | KeyCode::Char('h') => self.choice = self.choice.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') => self.choice = (self.choice + 1).min(2),
                KeyCode::Up | KeyCode::Char('k') => self.current = self.current.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.current = (self.current + 1).min(last),
                KeyCode::Char('d') => self.answers[self.current] = Some(FolderAnswer::Delete),
                KeyCode::Char('K') => self.answers[self.current] = Some(FolderAnswer::Keep),
                KeyCode::Char('r') => self.answers[self.current] = Some(FolderAnswer::Restore),
                KeyCode::Char('x') | KeyCode::Backspace => self.answers[self.current] = None,
                KeyCode::Enter => {
                    let all = CHOICES[self.choice];
                    return Some(self.answers.iter().map(|a| a.unwrap_or(all)).collect());
                }
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.choice = self.choice.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.choice = (self.choice + 1).min(2),
            KeyCode::Enter => {
                self.answers[self.current] = Some(CHOICES[self.choice]);
                self.current += 1;
                self.choice = 1;
                if self.current == self.folders.len() {
                    return Some(
                        self.answers
                            .iter()
                            .map(|a| a.unwrap_or(FolderAnswer::Keep))
                            .collect(),
                    );
                }
            }
            _ => {}
        }
        None
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(Span::styled(self.title.clone(), theme::style(Tone::Bold))),
            Line::default(),
        ];
        if !self.list_mode() {
            let Some(f) = self.folders.get(self.current) else {
                return lines;
            };
            lines.extend(
                folder_lines(&self.kind, f, self.current + 1, self.folders.len())
                    .into_iter()
                    .map(to_line),
            );
            lines.push(Line::default());
            for (i, label) in choice_labels(&self.kind).into_iter().enumerate() {
                let (glyph, tone) = if i == self.choice {
                    (theme::SELECTED, Tone::Accent)
                } else {
                    (theme::UNSELECTED, Tone::Plain)
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(glyph, theme::style(tone)),
                    Span::raw(format!(" {label}")),
                ]));
            }
            return lines;
        }
        let place = place(&self.kind);
        lines.push(Line::from(vec![
            Span::styled(theme::CURSOR, theme::style(Tone::Accent)),
            Span::styled(
                format!(
                    " {} folders deleted locally",
                    thousands(self.folders.len() as u64)
                ),
                theme::style(Tone::Bold),
            ),
        ]));
        lines.push(Line::default());
        lines.push(Line::from(format!(
            "  Not on {} any more. What should happen to them on {place}?",
            crate::platform::words().your_computer
        )));
        lines.push(Line::default());
        let mut all = vec![Span::raw("  For all:  ")];
        for (i, word) in ["Delete", "Keep", "Restore"].iter().enumerate() {
            let (glyph, tone) = if i == self.choice {
                (theme::SELECTED, Tone::Accent)
            } else {
                (theme::UNSELECTED, Tone::Plain)
            };
            all.push(Span::styled(glyph, theme::style(tone)));
            all.push(Span::raw(format!(" {word}   ")));
        }
        lines.push(Line::from(all));
        lines.push(Line::default());
        let width = self
            .folders
            .iter()
            .map(|f| crate::ui::width(&f.path))
            .max()
            .unwrap_or(0)
            .min(40);
        for (i, f) in self.folders.iter().enumerate() {
            let cursor = if i == self.current {
                theme::CURSOR
            } else {
                " "
            };
            let answer = self.answers[i];
            let word = match answer.unwrap_or(CHOICES[self.choice]) {
                FolderAnswer::Delete => "delete",
                FolderAnswer::Keep => "keep",
                FolderAnswer::Restore => "restore",
                FolderAnswer::CancelRun => "",
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {cursor} "), theme::style(Tone::Accent)),
                Span::raw(format!("{:<width$}", f.path)),
                Span::styled(
                    format!(
                        "  {:>13}  {:>9}   ",
                        plural(f.files, "file", "files"),
                        format_bytes(f.bytes)
                    ),
                    theme::style(Tone::Dim),
                ),
                Span::styled(
                    format!(
                        "{word}{}",
                        if answer.is_some() {
                            " (this folder)"
                        } else {
                            ""
                        }
                    ),
                    theme::style(if answer.is_some() {
                        Tone::Warning
                    } else {
                        Tone::Plain
                    }),
                ),
            ]));
        }
        lines
    }

    pub fn hints(&self) -> String {
        let d = theme::DOT;
        if self.list_mode() {
            format!(
                "←{} for all {d} {}{} move {d} d/K/r this folder {d} enter confirm {d} esc cancel run",
                theme::ACTION,
                theme::UP,
                theme::DOWN
            )
        } else {
            format!(
                "{}{} choose {d} enter confirm {d} esc cancel run",
                theme::UP,
                theme::DOWN
            )
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        // Keep the current folder in view in list mode.
        let scroll = if self.list_mode() {
            (self.current as u16 + 9).saturating_sub(frame.area().height.saturating_sub(4))
        } else {
            0
        };
        render_scrolling(
            frame,
            self.lines(),
            Vec::new(),
            scroll.max(self.scroll),
            &self.hints(),
        );
    }
}

/// Reads one answer typed at the plain folder prompt: d, k or r (or the
/// words); `None` for anything else.
pub fn parse_folder_answer(text: &str) -> Option<FolderAnswer> {
    match text.trim().to_ascii_lowercase().as_str() {
        "d" | "delete" => Some(FolderAnswer::Delete),
        "k" | "keep" | "" => Some(FolderAnswer::Keep),
        "r" | "restore" => Some(FolderAnswer::Restore),
        _ => None,
    }
}

/// Reads per-folder overrides typed at the plain list prompt, such as
/// `3k 7r`: folder numbers from 1. `None` when something does not read.
pub fn parse_overrides(text: &str, count: usize) -> Option<Vec<(usize, FolderAnswer)>> {
    let mut out = Vec::new();
    for word in text.split([' ', ',']).filter(|w| !w.is_empty()) {
        let split = word.find(|c: char| !c.is_ascii_digit())?;
        let n: usize = word[..split].parse().ok()?;
        if n == 0 || n > count {
            return None;
        }
        let answer = match &word[split..].to_ascii_lowercase()[..] {
            "d" => FolderAnswer::Delete,
            "k" => FolderAnswer::Keep,
            "r" => FolderAnswer::Restore,
            _ => return None,
        };
        out.push((n - 1, answer));
    }
    Some(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The preview from the design's mockup, with a few paths.
    pub fn mockup() -> Preview {
        let f = |path: &str, size| PlannedFile {
            path: path.into(),
            size,
            modtime_only: false,
        };
        let mut new: Vec<PlannedFile> = (0..37)
            .map(|i| f(&format!("Projects/2026/file-{i:02}.docx"), 11_150_000))
            .collect();
        new[0] = f("Projects/2026/brief.docx", 1_200_000);
        Preview {
            job: "documents".into(),
            destination_id: "d-91c0".into(),
            destination: "/Volumes/Backup/Documents".into(),
            new,
            changed: vec![
                f("Projects/2026/budget.xlsx", 248_000),
                f("Notes/plan.md", 14_000),
                f("Notes/ideas.md", 17_938_000),
                PlannedFile {
                    path: "Notes/old.md".into(),
                    size: 0,
                    modtime_only: true,
                },
                f("Notes/todo.md", 0),
            ],
            new_bytes: 412_600_000,
            changed_bytes: 18_200_000,
            unchanged: 981,
            destination_only: 12,
            excludes: vec!["node_modules/**".into(), "*.tmp".into()],
            copy_hidden: false,
            effective_excludes: crate::rclone::effective_excludes(
                &["node_modules/**".into(), "*.tmp".into()],
                false,
            ),
            destination_type: "local".into(),
            ..Preview::default()
        }
    }

    #[test]
    fn counts_match_the_mockup() {
        let text = render_text(&mockup(), false);
        let head: Vec<&str> = text.lines().take(10).collect();
        assert_eq!(
            head,
            [
                "→ /Volumes/Backup/Documents   one-way",
                "",
                "  New         37 files   412.6 MB",
                "  Changed      5 files    18.2 MB",
                "  Unchanged  981 files",
                "  Excluded   node_modules/**, *.tmp",
                "             Hidden files: not copied",
                "             Always excluded: .DS_Store, ._*, Thumbs.db,",
                "             desktop.ini and other system files",
                "",
            ]
        );
        // No patterns of its own, hidden files copied.
        let mut p = mockup();
        p.excludes.clear();
        p.copy_hidden = true;
        let text = render_text(&p, false);
        assert!(
            text.contains(
                "  Excluded   no patterns of the job's own\n             Hidden files: copied\n"
            ),
            "{text}"
        );
        // A built-in pattern among the job's own is not shown twice, and
        // big counts read with separators.
        let mut p = mockup();
        p.excludes = vec![".DS_Store".into(), "*.tmp".into()];
        p.unchanged = 200_000;
        let text = render_text(&p, false);
        assert!(text.contains("  Excluded   *.tmp\n"), "{text}");
        assert!(text.contains("  Unchanged  200,000 files\n"), "{text}");
        assert!(text.contains("  New             37 files"), "{text}");
        // Every line fits 80 columns.
        for p in [mockup(), p] {
            for line in render_text(&p, false).lines().take(10) {
                assert!(line.chars().count() <= 80, "{line}");
            }
        }
        assert!(text.contains("    and 27 more\n"));
        assert!(text.contains("  Nothing will be deleted on the destination.\n"));
        assert!(text.contains("  12 files only on the destination are left alone.\n"));
        assert!(
            text.lines()
                .any(|l| l.starts_with("    Notes/old.md ") && l.ends_with("   time only")),
            "{text}"
        );
    }

    #[test]
    fn answers() {
        assert_eq!(parse_answer("\n"), Answer::Yes);
        assert_eq!(parse_answer("Y"), Answer::Yes);
        assert_eq!(parse_answer("yes"), Answer::Yes);
        assert_eq!(parse_answer("n"), Answer::No);
        assert_eq!(parse_answer("whatever"), Answer::No);
        let mut prompt = PreviewPrompt::new("Run documents".into(), mockup());
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(prompt.handle_key(key(KeyCode::Down)), None);
        assert_eq!(prompt.scroll, 1);
        assert_eq!(prompt.handle_key(key(KeyCode::Enter)), Some(Answer::Yes));
        assert_eq!(prompt.handle_key(key(KeyCode::Esc)), Some(Answer::No));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(prompt.handle_key(ctrl_c), Some(Answer::No));
    }

    fn render(width: u16) -> String {
        let prompt = PreviewPrompt::new("Run documents".into(), mockup());
        let mut terminal = Terminal::new(TestBackend::new(width, 42)).unwrap();
        terminal.draw(|f| prompt.render(f)).unwrap();
        terminal.backend().to_string()
    }

    fn snapshot(name: &str, value: String) {
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!(name, value);
        });
    }

    #[test]
    fn renders_at_80_columns() {
        snapshot("preview_prompt_80", render(80));
    }

    #[test]
    fn renders_at_120_columns() {
        snapshot("preview_prompt_120", render(120));
    }

    #[test]
    fn plain_text() {
        snapshot("preview_plain", render_text(&mockup(), false));
    }

    /// The mockup with moved files found on OneDrive: a renamed folder of
    /// 1,198 files and eleven notes moved one by one, so the Moved list is
    /// cut after ten groups.
    pub fn moves_mockup() -> Preview {
        use crate::sync::moves::PlannedMove;
        let mut p = mockup();
        p.destination = "work:Backups/Documents".into();
        p.destination_type = "onedrive".into();
        let mut groups = vec![MoveGroup {
            from: "Projects/2025".into(),
            to: "Archive/2025".into(),
            files: 1198,
            bytes: 1198 * 2_500_000,
            folder: true,
        }];
        let mut moves: Vec<PlannedMove> = (0..1198)
            .map(|i| PlannedMove {
                from: format!("Projects/2025/scan-{i:04}.pdf"),
                to: format!("Archive/2025/scan-{i:04}.pdf"),
                size: 2_500_000,
                group: 0,
            })
            .collect();
        for i in 0..11u64 {
            let (from, to) = (
                format!("Notes/n-{i:02}.md"),
                format!("Notes/old/n-{i:02}.md"),
            );
            groups.push(MoveGroup {
                from: from.clone(),
                to: to.clone(),
                files: 1,
                bytes: 14_000 * (i + 1),
                folder: false,
            });
            moves.push(PlannedMove {
                from,
                to,
                size: 14_000 * (i + 1),
                group: groups.len() - 1,
            });
        }
        p.move_bytes = moves.iter().map(|m| m.size).sum();
        p.moves = moves;
        p.move_groups = groups;
        p.destination_only = 12 + 1209;
        p
    }

    #[test]
    fn moved_row_and_list() {
        let p = moves_mockup();
        let text = render_text(&p, false);
        let head: Vec<&str> = text.lines().take(7).collect();
        assert_eq!(
            head,
            [
                "→ work:Backups/Documents   one-way",
                "",
                "  New           37 files   412.6 MB",
                "  Changed        5 files    18.2 MB",
                "  Unchanged    981 files",
                "  Moved      1,209 files     3.0 GB   already on OneDrive under another name",
                "  Excluded   node_modules/**, *.tmp",
            ]
        );
        assert!(
            text.contains(
                "  Moved files\n    Projects/2025/  →  Archive/2025/       1,198 files\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("    Notes/n-00.md   →  Notes/old/n-00.md         14 KB\n"),
            "{text}"
        );
        assert!(text.contains("    and 2 more\n"), "{text}");
        assert!(
            text.contains(
                "  Nothing will be deleted on the destination; moved files keep their content\n  under the new name.\n"
            ),
            "{text}"
        );
        // The files that move are not left alone.
        assert!(
            text.contains("  12 files only on the destination are left alone.\n"),
            "{text}"
        );
        for line in text.lines() {
            assert!(crate::ui::width(line) <= 80, "{line}");
        }
        // On a drive.
        let mut local = p.clone();
        local.destination_type = "local".into();
        assert!(render_text(&local, false).contains("already on the drive under another name"));
        assert_eq!(
            moves_question(&local),
            "  Move them on the drive instead of uploading them again? [Y/n] "
        );
        assert_eq!(
            question(&Question::Moves(&p), "onedrive"),
            (
                Vec::new(),
                "  Move them on OneDrive instead of uploading them again? [Y/n] ".to_string(),
                Answer::Yes
            )
        );
        // Without moves nothing changes: no row, no list, the short line.
        let plain = render_text(&mockup(), false);
        assert!(!plain.contains("Moved"));
        assert!(plain.contains("  Nothing will be deleted on the destination.\n"));
    }

    #[test]
    fn the_answer_changes_the_rows() {
        let mut p = moves_mockup();
        p.choose_moves(MovesChoice::Move);
        let text = render_text(&p, false);
        assert!(
            text.contains("  Moved      1,209 files     3.0 GB   will be moved\n"),
            "{text}"
        );
        assert!(
            text.contains("  New           37 files   412.6 MB\n"),
            "{text}"
        );
        assert_eq!(
            lines_text(&moves_answer_lines(&p), false),
            "  New           37 files   412.6 MB\n  Moved      1,209 files     3.0 GB   will be moved\n"
        );
        // Uploaded again: they are new files once more, with their bytes,
        // nothing is moved and every destination-only file stays.
        p.choose_moves(MovesChoice::Upload);
        let text = render_text(&p, false);
        assert!(
            text.contains("  New        1,246 files    3.4 GB\n"),
            "{text}"
        );
        assert!(
            text.contains("  Moved      1,209 files    3.0 GB   will be uploaded again\n"),
            "{text}"
        );
        assert!(text.contains("  Nothing will be deleted on the destination.\n"));
        assert!(text.contains("  1,221 files only on the destination are left alone.\n"));
        assert_eq!(p.new.len(), 37 + 1209);
        assert_eq!(p.new_bytes, 412_600_000 + p.move_bytes);
        // And back: the same as before, whatever the order of answers.
        p.choose_moves(MovesChoice::Move);
        p.choose_moves(MovesChoice::Move);
        assert_eq!((p.new.len(), p.new_bytes), (37, 412_600_000));
        snapshot("preview_plain_moves", render_text(&moves_mockup(), false));
    }

    #[test]
    fn moves_decisions_in_words() {
        let p = moves_mockup();
        assert_eq!(
            moves_decision(&p, MovesAnswer::Move, DecidedBy::Flag),
            "1,209 moved files: will be moved on OneDrive, not uploaded again   (--moves)"
        );
        assert_eq!(
            moves_decision(&p, MovesAnswer::Copy, DecidedBy::Asked),
            "1,209 moved files: will be uploaded again, the old copies stay on OneDrive"
        );
        assert_eq!(
            moves_decision(&p, MovesAnswer::Copy, DecidedBy::NotAsked),
            "1,209 moved files: will be uploaded again, the old copies stay on OneDrive   \
             (--moves ask, and there is no terminal to ask on)"
        );
        let mut dry = p.clone();
        dry.dry_run = true;
        assert_eq!(
            moves_decision(&dry, MovesAnswer::Move, DecidedBy::Default),
            "1,209 moved files: would be moved on OneDrive, not uploaded again   \
             (the default with --yes or without a terminal)"
        );
        assert_eq!(
            moves_decision(&dry, MovesAnswer::Copy, DecidedBy::Flag),
            "1,209 moved files: would be uploaded again, the old copies would stay on OneDrive   \
             (--moves)"
        );
        let mut one = p.clone();
        one.moves.truncate(1);
        one.destination_type = "local".into();
        assert_eq!(
            moves_decision(&one, MovesAnswer::Move, DecidedBy::Default),
            "1 moved file: will be moved on the drive, not uploaded again   \
             (the default with --yes or without a terminal)"
        );
        assert_eq!(
            moves_decision(&one, MovesAnswer::Copy, DecidedBy::Asked),
            "1 moved file: will be uploaded again, the old copy stays on the drive"
        );
    }

    #[test]
    fn the_move_question_keys() {
        let p = moves_mockup();
        let prompt = PreviewPrompt::moves("Run documents".into(), &p);
        assert_eq!(
            prompt.hints(),
            "y/enter move · n upload again · l whole list · ↑↓ scroll"
        );
        assert_eq!(
            prompt.question,
            "  Move them on OneDrive instead of uploading them again? [Y/n] "
        );
        for (k, answer) in [
            (KeyCode::Enter, Answer::Yes),
            (KeyCode::Char('y'), Answer::Yes),
            (KeyCode::Char('Y'), Answer::Yes),
            (KeyCode::Char('n'), Answer::No),
            // Esc and q upload again, they never skip the destination.
            (KeyCode::Esc, Answer::No),
            (KeyCode::Char('q'), Answer::No),
        ] {
            let mut prompt = PreviewPrompt::moves("Run documents".into(), &p);
            assert_eq!(prompt.handle_key(key(k)), Some(answer), "{k:?}");
        }
        let mut prompt = PreviewPrompt::moves("Run documents".into(), &p);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(prompt.handle_key(ctrl_c), Some(Answer::No));
        // l opens the lists, the Moved list among them, and answers nothing.
        let mut prompt = PreviewPrompt::moves("Run documents".into(), &p);
        assert_eq!(prompt.handle_key(key(KeyCode::Char('l'))), None);
        assert!(prompt.view.is_some());
        let titles: Vec<String> = prompt.full.iter().map(FullList::heading).collect();
        assert_eq!(
            titles,
            [
                "New files · 37 files · 412.6 MB",
                "Moved files · 1,209 files · 3.0 GB"
            ]
        );
        assert_eq!(prompt.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(prompt.handle_key(key(KeyCode::Esc)), None);
        assert!(prompt.view.is_none());
        assert_eq!(prompt.handle_key(key(KeyCode::Esc)), Some(Answer::No));
        // A short Moved list is not in the whole-list view.
        let mut short = moves_mockup();
        short.move_groups.truncate(3);
        assert!(preview_full_lists(&short)
            .iter()
            .all(|l| l.title != MOVED_TITLE));
    }

    #[test]
    fn long_moved_paths_fit_80_columns() {
        let mut p = moves_mockup();
        p.move_groups[0] = MoveGroup {
            from: "Clients".into(),
            to: "Archive/Clients (closed)".into(),
            files: 2,
            bytes: 9_000,
            folder: true,
        };
        p.move_groups[1].from =
            "Projects/2026/Very long client name Pty Ltd/Quarterly reports/q3.xlsx".into();
        p.move_groups[1].to =
            "Archive/2026/Very long client name Pty Ltd/Quarterly reports/final/q3.xlsx".into();
        let text = render_text(&p, false);
        for line in text.lines() {
            assert!(
                crate::ui::width(line) <= 80,
                "{} {line}",
                crate::ui::width(line)
            );
        }
        assert!(
            text.lines().any(|l| l.starts_with("    Clients/ ")
                && l.contains("→  Archive/Clients (closed)/")
                && l.ends_with("2 files")),
            "{text}"
        );
        assert!(text.contains("…"), "{text}");
        let prompt = PreviewPrompt::moves("Run documents".into(), &p);
        let screen = render_sized(&prompt, 80, 56);
        snapshot("preview_moves_long_80", screen.clone());
        // No row wraps: every count or size sits on its path's row.
        assert!(
            !screen
                .lines()
                .any(|l| matches!(l.trim_matches(['"', ' ']), "2 files" | "14 KB")),
            "{screen}"
        );
        let mut prompt = PreviewPrompt::moves("Run documents".into(), &p);
        prompt.handle_key(key(KeyCode::Char('l')));
        prompt.handle_key(key(KeyCode::Char('G')));
        snapshot("full_list_moves_long_80", render_sized(&prompt, 80, 30));
        assert_eq!(move_columns(10, 20, 53), (10, 20));
        assert_eq!(move_columns(10, 60, 53), (10, 43));
        assert_eq!(move_columns(60, 12, 53), (41, 12));
        assert_eq!(move_columns(60, 60, 53), (26, 27));
    }

    #[test]
    fn moves_screens() {
        let p = moves_mockup();
        let prompt = PreviewPrompt::moves("Run documents".into(), &p);
        snapshot("preview_moves_80", render_sized(&prompt, 80, 56));
        snapshot("preview_moves_120", render_sized(&prompt, 120, 56));
        // After the answer, the usual question over the updated rows.
        let mut moved = p.clone();
        moved.choose_moves(MovesChoice::Move);
        let screen = render_sized(&PreviewPrompt::new("Run documents".into(), moved), 80, 56);
        assert!(screen.contains("will be moved"), "{screen}");
        assert!(screen.contains("Continue? [Y/n]"), "{screen}");
        let mut prompt = PreviewPrompt::moves("Run documents".into(), &p);
        prompt.handle_key(key(KeyCode::Char('l')));
        prompt.handle_key(key(KeyCode::Char('G')));
        snapshot("full_list_moves_80", render_sized(&prompt, 80, 30));
        snapshot("full_list_moves_120", render_sized(&prompt, 120, 30));
        // The filter finds a move by either of its paths.
        let mut prompt = PreviewPrompt::moves("Run documents".into(), &p);
        prompt.handle_key(key(KeyCode::Char('l')));
        prompt.handle_key(key(KeyCode::Char('/')));
        for c in "old/n-1".chars() {
            prompt.handle_key(key(KeyCode::Char(c)));
        }
        let screen = render_sized(&prompt, 80, 30);
        assert!(screen.contains("1 of 1 match · 49 in all"), "{screen}");
        assert!(
            screen.contains("Notes/n-10.md   →  Notes/old/n-10.md"),
            "{screen}"
        );
    }

    use crate::sync::plan::{BigChange, FolderDecision, PlannedConflict};

    fn file(path: &str, size: u64) -> PlannedFile {
        PlannedFile {
            path: path.into(),
            size,
            modtime_only: false,
        }
    }

    fn folder(path: &str, files: u64, bytes: u64) -> DeletedFolder {
        DeletedFolder {
            path: path.into(),
            files,
            bytes,
            measured: true,
            maybe_rename: false,
        }
    }

    /// A two-way plan after the design's mockups.
    pub fn plan_mockup() -> Plan {
        Plan {
            job: "documents".into(),
            destination_id: "d-7f3a".into(),
            destination: "work:Backups/Documents".into(),
            destination_type: "onedrive".into(),
            to_remote: Direction {
                new: vec![
                    file("Projects/2026/brief.docx", 1_200_000),
                    file("new.txt", 6),
                ],
                changed: vec![file("Projects/2026/budget.xlsx", 248_000)],
                bytes: 1_448_006,
            },
            to_local: Direction {
                new: vec![file("Shared/meeting-notes.md", 14_000)],
                changed: vec![],
                bytes: 14_000,
            },
            delete_remote: vec!["Drafts/old.txt".into(), "Drafts/older.txt".into()],
            delete_local: vec!["Shared/agenda.md".into()],
            conflicts: vec![PlannedConflict {
                path: "Notes/plan.md".into(),
                kept_as: Some("Notes/plan.conflict-2026-10-13-1.md".into()),
            }],
            folders: vec![FolderDecision {
                folder: folder("Old projects/2019", 1284, 3_400_000_000),
                answer: FolderAnswer::Keep,
                by: DecidedBy::Asked,
            }],
            big: vec![],
            lift: false,
            force: false,
            changes: 8,
            excludes: vec!["*.tmp".into()],
            copy_hidden: false,
            effective_excludes: vec![],
            holds: vec!["Archive/2015".into()],
        }
    }

    fn render_prompt(prompt: &PreviewPrompt, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 44)).unwrap();
        terminal.draw(|f| prompt.render(f)).unwrap();
        terminal.backend().to_string()
    }

    fn plan_prompt(plan: &Plan) -> PreviewPrompt {
        let (mut body, q, default) = question(&Question::Plan(plan), "onedrive");
        let (lines, full) = plan_screen(plan);
        body.splice(0..0, lines);
        PreviewPrompt::with_lines("Run documents".into(), body, q, default, "sync")
            .with_full_lists(full)
    }

    #[test]
    fn two_way_plan() {
        let plan = plan_mockup();
        snapshot("plan_plain", render_plan_text(&plan, false));
        snapshot("plan_prompt_80", render_prompt(&plan_prompt(&plan), 80));
        snapshot("plan_prompt_120", render_prompt(&plan_prompt(&plan), 120));
    }

    #[test]
    fn two_way_plan_with_mass_deletion() {
        let mut plan = plan_mockup();
        plan.destination_type = "local".into();
        plan.destination = "/Volumes/Backup/Documents".into();
        plan.big = vec![BigChange {
            kind: BigKind::Deletions,
            side: Where::Destination,
            count: 6,
            total: 10,
        }];
        let text = render_plan_text(&plan, false);
        assert!(text.contains("More than half would be deleted on the drive: 6 of 10"));
        let prompt = plan_prompt(&plan);
        assert_eq!(prompt.default, Answer::No);
        assert!(prompt.question.contains("[y/N]"));
        snapshot("plan_prompt_mass_deletion_80", render_prompt(&prompt, 80));
    }

    /// A deletion list far taller than the screen: the question stays in
    /// view under it, and scrolling stops at the end of the wrapped body.
    #[test]
    fn a_long_deletion_list_keeps_the_question_in_view() {
        let mut plan = plan_mockup();
        plan.delete_remote = (0..200)
            .map(|i| format!("Projects/archive/a long folder name that wraps on a narrow screen/file-{i:03}.docx"))
            .collect();
        plan.big = vec![BigChange {
            kind: BigKind::Deletions,
            side: Where::Destination,
            count: 200,
            total: 201,
        }];
        let mut prompt = plan_prompt(&plan);
        let shown = render_prompt(&prompt, 60);
        assert!(
            shown.contains("Go ahead, with those deletions? [y/N]"),
            "{shown}"
        );
        assert!(!shown.contains("file-199"), "{shown}");
        let max = prompt.max_scroll.get();
        assert!(max > 200, "wrapped rows counted: {max}");
        for _ in 0..1000 {
            prompt.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(prompt.scroll, max);
        let end = render_prompt(&prompt, 60);
        assert!(end.contains("file-199"), "{end}");
        assert!(
            end.contains("Go ahead, with those deletions? [y/N]"),
            "{end}"
        );
        // One key up moves at once, however far down was pressed.
        prompt.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(prompt.scroll, max - 1);
        let text = render_plan_text(&plan, false);
        assert!(text.contains("(99.5%)"), "{text}");
    }

    fn offer() -> ResyncOffer {
        ResyncOffer {
            job: "documents".into(),
            destination_id: "d-7f3a".into(),
            destination: "work:Backups/Documents".into(),
            destination_type: "onedrive".into(),
            reason: Some(ResyncReason::FirstRun),
            to_remote: Direction {
                new: vec![file("a.txt", 10)],
                changed: vec![file("Notes/plan.md", 20)],
                bytes: 30,
            },
            to_local: Direction {
                new: vec![file("only-there.txt", 5)],
                changed: vec![],
                bytes: 5,
            },
            kept: vec![PlannedConflict {
                path: "Notes/plan.md".into(),
                kept_as: Some("Notes/plan.conflict-2026-10-13-1.md".into()),
            }],
            ..ResyncOffer::default()
        }
    }

    #[test]
    fn first_run_offer() {
        let o = offer();
        let text = render_resync_text(&o, false);
        // The design's words, wrapped.
        let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(joined.contains(&FIRST_RUN.split_whitespace().collect::<Vec<_>>().join(" ")));
        snapshot("resync_plain", text);
        let (mut body, q, default) = question(&Question::Resync(&o), "onedrive");
        body.splice(0..0, resync_lines(&o));
        let prompt = PreviewPrompt::with_lines("Run documents".into(), body, q, default, "merge");
        snapshot("resync_prompt_80", render_prompt(&prompt, 80));
        let mut damaged = offer();
        damaged.reason = Some(ResyncReason::Damaged);
        damaged.detail = Some("bisync said: cannot find prior Path1 or Path2 listings".into());
        damaged.destination_missing = true;
        snapshot("resync_damaged_plain", render_resync_text(&damaged, false));
    }

    #[test]
    fn stale_lock_question() {
        let (lines, q, default) = question(
            &Question::StaleLock {
                pid: 4242,
                path: "~/.local/share/onefolder/bisync/documents/d-7f3a/x..y.lck",
            },
            "onedrive",
        );
        assert_eq!(default, Answer::Yes);
        snapshot("stale_lock_plain", lines_text(&lines, false) + &q);
    }

    fn render_folders(prompt: &FolderPrompt, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| prompt.render(f)).unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn folder_prompt_screens() {
        let one = FolderPrompt::new(
            "Run documents".into(),
            "onedrive",
            vec![folder("Old projects/2019", 1284, 3_400_000_000)],
        );
        snapshot("folder_prompt_80", render_folders(&one, 80, 20));
        snapshot("folder_prompt_120", render_folders(&one, 120, 20));
        let text = lines_text(&folder_lines("onedrive", &one.folders[0], 1, 1), false);
        assert!(
            text.contains("  On OneDrive: 1,284 files, 3.4 GB\n"),
            "{text}"
        );
        assert!(text.contains("Not on your Mac any more. What should happen on OneDrive?"));
        {
            // On Linux and WSL the words name "this computer".
            let _linux = crate::platform::testing::describe(crate::platform::Os::Linux);
            let text = lines_text(&folder_lines("onedrive", &one.folders[0], 1, 1), false);
            assert!(
                text.contains("Not on your computer any more. What should happen on OneDrive?"),
                "{text}"
            );
            let choices = choice_labels("onedrive");
            assert_eq!(choices[2], "Restore it to this computer from OneDrive");
            let plan = lines_text(&plan_lines(&plan_mockup()), false);
            for words in [
                "  To this computer              1 file        14 KB",
                "  Delete on this computer       1 file",
                "  Folders deleted on this computer",
                "  Will be deleted on this computer",
                "  New and changed, to this computer",
            ] {
                assert!(plan.contains(words), "{words}\n{plan}");
            }
            assert!(!plan.contains("Mac"), "{plan}");
        }
        let mut renamed = folder("Drafts", 3, 30);
        renamed.maybe_rename = true;
        renamed.measured = false;
        snapshot(
            "folder_prompt_plain_drive",
            lines_text(&folder_lines("local", &renamed, 2, 3), false),
        );
        let many: Vec<DeletedFolder> = (0..23)
            .map(|i| {
                folder(
                    &format!("Archive/{}", 2000 + i),
                    10 + i,
                    1_000_000 * (i + 1),
                )
            })
            .collect();
        let mut list = FolderPrompt::new("Run documents".into(), "onedrive", many);
        assert!(list.list_mode());
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        list.handle_key(key(KeyCode::Right));
        list.handle_key(key(KeyCode::Down));
        list.handle_key(key(KeyCode::Char('d')));
        snapshot("folder_prompt_list_80", render_folders(&list, 80, 40));
        snapshot("folder_prompt_list_120", render_folders(&list, 120, 40));
    }

    #[test]
    fn folder_prompt_keys() {
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let two = vec![folder("A", 1, 1), folder("B", 1, 1)];
        let mut p = FolderPrompt::new("t".into(), "local", two.clone());
        // Keep is highlighted first: Enter keeps; deleting needs a move.
        assert_eq!(p.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(p.handle_key(key(KeyCode::Up)), None);
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            Some(vec![FolderAnswer::Keep, FolderAnswer::Delete])
        );
        let mut p = FolderPrompt::new("t".into(), "local", vec![folder("A", 1, 1)]);
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            Some(vec![FolderAnswer::Keep])
        );
        let mut p = FolderPrompt::new("t".into(), "local", two);
        assert_eq!(
            p.handle_key(key(KeyCode::Esc)),
            Some(vec![FolderAnswer::CancelRun; 2])
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(p.handle_key(ctrl_c), Some(vec![FolderAnswer::CancelRun; 2]));
        // List mode: one answer for all, with an override.
        let many: Vec<DeletedFolder> = (0..21).map(|i| folder(&format!("F{i}"), 1, 1)).collect();
        let mut p = FolderPrompt::new("t".into(), "onedrive", many);
        p.handle_key(key(KeyCode::Right));
        p.handle_key(key(KeyCode::Right));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Char('d')));
        let answers = p.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(answers[0], FolderAnswer::Restore);
        assert_eq!(answers[1], FolderAnswer::Delete);
        assert_eq!(answers[20], FolderAnswer::Restore);
    }

    #[test]
    fn typed_answers() {
        assert_eq!(parse_folder_answer("d"), Some(FolderAnswer::Delete));
        assert_eq!(parse_folder_answer(" Keep \n"), Some(FolderAnswer::Keep));
        assert_eq!(parse_folder_answer(""), Some(FolderAnswer::Keep));
        assert_eq!(parse_folder_answer("r"), Some(FolderAnswer::Restore));
        assert_eq!(parse_folder_answer("x"), None);
        assert_eq!(
            parse_overrides("3k, 7r", 10),
            Some(vec![(2, FolderAnswer::Keep), (6, FolderAnswer::Restore)])
        );
        assert_eq!(parse_overrides("", 10), Some(vec![]));
        assert_eq!(parse_overrides("11d", 10), None);
        assert_eq!(parse_overrides("0d", 10), None);
        assert_eq!(parse_overrides("3x", 10), None);
        assert_eq!(parse_answer_or("", Answer::No), Answer::No);
        assert_eq!(parse_answer_or("y", Answer::No), Answer::Yes);
        assert_eq!(thousands(1_284), "1,284");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(12), "12");
        assert_eq!(wrap("aa bb cc", 7, "  "), ["  aa bb", "  cc"]);
    }

    // ------------------------------------------------- whole lists

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A preview whose two lists are both cut: 37 new and 12 changed.
    fn long_preview() -> Preview {
        let mut p = mockup();
        p.changed = (0..12)
            .map(|i| file(&format!("Notes/note-{i:02}.md"), 1_000 * (i + 1)))
            .collect();
        p.changed_bytes = p.changed.iter().map(|f| f.size).sum();
        p
    }

    fn render_sized(prompt: &PreviewPrompt, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| prompt.render(f)).unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn l_opens_the_whole_list_only_when_a_list_was_cut() {
        let mut prompt = PreviewPrompt::new("Run documents".into(), mockup());
        assert_eq!(prompt.full.len(), 1, "only the new files were cut");
        assert_eq!(prompt.full[0].heading(), "New files · 37 files · 412.6 MB");
        assert!(prompt
            .hints()
            .contains("skip this destination · l whole list · ↑↓ scroll"));
        assert_eq!(prompt.handle_key(key(KeyCode::Char('l'))), None);
        assert!(prompt.view.is_some());

        // Ten or fewer of each: nothing to open, no hint, no key.
        let mut short = mockup();
        short.new.truncate(10);
        let mut prompt = PreviewPrompt::new("Run documents".into(), short.clone());
        assert!(prompt.full.is_empty());
        assert!(!prompt.hints().contains("whole list"));
        assert_eq!(prompt.handle_key(key(KeyCode::Char('l'))), None);
        assert!(prompt.view.is_none());
        assert!(!lines_text(&preview_screen(&short).0, false).contains("more"));
    }

    #[test]
    fn the_more_line_names_the_key_on_screen_only() {
        let screen = lines_text(&preview_screen(&mockup()).0, false);
        assert!(
            screen.contains("    and 27 more · l whole list\n"),
            "{screen}"
        );
        // The plain output and the --dry-run printout are unchanged.
        let plain = render_text(&mockup(), false);
        assert!(plain.contains("    and 27 more\n"), "{plain}");
        assert!(!plain.contains(MORE_KEY));

        let mut plan = plan_mockup();
        plan.delete_local = (0..61).map(|i| format!("Old/{i:02}.txt")).collect();
        let screen = lines_text(&plan_screen(&plan).0, false);
        assert!(
            screen.contains("    and 51 more · l whole list\n"),
            "{screen}"
        );
        assert!(!render_plan_text(&plan, false).contains(MORE_KEY));
    }

    #[test]
    fn closing_the_whole_list_keeps_the_confirmation_as_it_was() {
        let mut prompt = PreviewPrompt::new("Run documents".into(), long_preview());
        render_sized(&prompt, 80, 30);
        for _ in 0..3 {
            prompt.handle_key(key(KeyCode::Down));
        }
        assert_eq!(prompt.scroll, 3);
        let before = render_sized(&prompt, 80, 30);
        for close in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('l')] {
            assert_eq!(prompt.handle_key(key(KeyCode::Char('l'))), None);
            assert!(prompt.view.is_some());
            render_sized(&prompt, 80, 30);
            // Moving in the view leaves the confirmation's scroll alone,
            // and y, n and Enter answer nothing while the view is open.
            for k in [
                KeyCode::Down,
                KeyCode::PageDown,
                KeyCode::Char('y'),
                KeyCode::Char('Y'),
                KeyCode::Char('n'),
                KeyCode::Enter,
            ] {
                assert_eq!(prompt.handle_key(key(k)), None, "{k:?}");
                assert!(prompt.view.is_some(), "{k:?}");
            }
            assert_eq!(prompt.handle_key(key(close)), None);
            assert!(prompt.view.is_none(), "{close:?}");
            assert_eq!(prompt.scroll, 3);
            assert_eq!(render_sized(&prompt, 80, 30), before);
        }
        // Ctrl-C still answers no from the view.
        prompt.handle_key(key(KeyCode::Char('l')));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(prompt.handle_key(ctrl_c), Some(Answer::No));
        // Back on the confirmation, the keys answer again.
        let mut prompt = PreviewPrompt::new("Run documents".into(), long_preview());
        prompt.handle_key(key(KeyCode::Char('l')));
        prompt.handle_key(key(KeyCode::Esc));
        assert_eq!(prompt.handle_key(key(KeyCode::Enter)), Some(Answer::Yes));
    }

    #[test]
    fn whole_list_scrolling_stops_at_both_ends() {
        let mut prompt = PreviewPrompt::new("Run documents".into(), long_preview());
        prompt.handle_key(key(KeyCode::Char('l')));
        // 24 rows tall: 17 rows of list; 37 + 12 files, 2 headings, 1 blank.
        let screen = render_sized(&prompt, 80, 24);
        assert!(screen.contains("1–16 of 49"), "{screen}");
        let view = |p: &PreviewPrompt| p.view.as_ref().unwrap().scroll;
        // A page is 16 rows: a pinned heading can take the 17th.
        let max = 52 - 16;
        prompt.handle_key(key(KeyCode::Char('G')));
        assert_eq!(view(&prompt), max);
        prompt.handle_key(key(KeyCode::Down));
        prompt.handle_key(key(KeyCode::PageDown));
        prompt.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(view(&prompt), max);
        let screen = render_sized(&prompt, 80, 24);
        assert!(screen.contains("Notes/note-11.md"), "{screen}");
        assert!(screen.contains("36–49 of 49"), "{screen}");
        prompt.handle_key(key(KeyCode::PageUp));
        assert_eq!(view(&prompt), max - 16);
        prompt.handle_key(key(KeyCode::Char('g')));
        assert_eq!(view(&prompt), 0);
        prompt.handle_key(key(KeyCode::Up));
        prompt.handle_key(key(KeyCode::PageUp));
        assert_eq!(view(&prompt), 0);
        prompt.handle_key(key(KeyCode::PageDown));
        assert_eq!(view(&prompt), 16);
        prompt.handle_key(key(KeyCode::Char('k')));
        prompt.handle_key(key(KeyCode::Char('j')));
        prompt.handle_key(key(KeyCode::Char('j')));
        assert_eq!(view(&prompt), 17);
    }

    #[test]
    fn slash_filters_the_whole_list() {
        let mut prompt = PreviewPrompt::new("Run documents".into(), long_preview());
        prompt.handle_key(key(KeyCode::Char('l')));
        prompt.handle_key(key(KeyCode::Char('/')));
        for c in "FILE-1".chars() {
            assert_eq!(prompt.handle_key(key(KeyCode::Char(c))), None);
        }
        let shown = |p: &PreviewPrompt| p.view.as_ref().unwrap().shown;
        // file-10 to file-19, whatever the case typed.
        assert_eq!(shown(&prompt), 10);
        let screen = render_sized(&prompt, 80, 30);
        assert!(
            screen.contains("1–10 of 10 matches · 49 in all · /FILE-1▏"),
            "{screen}"
        );
        assert!(
            screen.contains("type to filter · enter keep · esc clear"),
            "{screen}"
        );
        assert!(!screen.contains("Changed files"), "{screen}");
        // Letters that are keys elsewhere are text while typing.
        prompt.handle_key(key(KeyCode::Char('q')));
        assert!(prompt.view.is_some());
        assert_eq!(shown(&prompt), 0);
        let screen = render_sized(&prompt, 80, 30);
        assert!(screen.contains("no matches · 49 in all"), "{screen}");
        assert!(
            screen.contains("no path matches · esc clears the filter"),
            "{screen}"
        );
        prompt.handle_key(key(KeyCode::Backspace));
        prompt.handle_key(key(KeyCode::Backspace));
        assert_eq!(shown(&prompt), 36, "file-");
        prompt.handle_key(key(KeyCode::Char('3')));
        // Enter keeps the filter and goes back to scrolling.
        assert_eq!(prompt.handle_key(key(KeyCode::Enter)), None);
        let view = prompt.view.as_ref().unwrap();
        assert!(!view.filtering);
        assert_eq!(view.filter, "FILE-3");
        assert_eq!(shown(&prompt), 7, "file-30 to file-36");
        let screen = render_sized(&prompt, 80, 30);
        assert!(
            screen.contains("1–7 of 7 matches · 49 in all · /FILE-3 "),
            "{screen}"
        );
        assert!(screen.contains("esc clear · l back"), "{screen}");
        // Esc clears the filter first, then leaves the view.
        prompt.handle_key(key(KeyCode::Esc));
        assert!(prompt.view.is_some());
        assert_eq!(shown(&prompt), 49);
        prompt.handle_key(key(KeyCode::Esc));
        assert!(prompt.view.is_none());
        // Esc while typing clears too.
        prompt.handle_key(key(KeyCode::Char('l')));
        prompt.handle_key(key(KeyCode::Char('/')));
        prompt.handle_key(key(KeyCode::Char('x')));
        prompt.handle_key(key(KeyCode::Esc));
        let view = prompt.view.as_ref().unwrap();
        assert!(!view.filtering && view.filter.is_empty());
        assert_eq!(shown(&prompt), 49);
    }

    #[test]
    fn position_reads_with_separators() {
        assert_eq!(
            position_text(1, 38, 29_152, 29_152, false),
            "1–38 of 29,152"
        );
        assert_eq!(
            position_text(1_001, 1_038, 29_152, 29_152, false),
            "1,001–1,038 of 29,152"
        );
        assert_eq!(
            position_text(1, 12, 12, 29_152, true),
            "1–12 of 12 matches · 29,152 in all"
        );
        assert_eq!(
            position_text(1, 1, 1, 29_152, true),
            "1 of 1 match · 29,152 in all"
        );
        assert_eq!(
            position_text(0, 0, 0, 29_152, true),
            "no matches · 29,152 in all"
        );
    }

    #[test]
    fn hints_fit_80_columns() {
        let mut prompt = PreviewPrompt::new("Run documents".into(), long_preview());
        let mut hints = vec![prompt.hints()];
        prompt.default = Answer::No;
        hints.push(prompt.hints());
        prompt.handle_key(key(KeyCode::Char('l')));
        hints.push(prompt.view.as_ref().unwrap().hints());
        prompt.handle_key(key(KeyCode::Char('/')));
        hints.push(prompt.view.as_ref().unwrap().hints());
        prompt.handle_key(key(KeyCode::Char('a')));
        prompt.handle_key(key(KeyCode::Enter));
        hints.push(prompt.view.as_ref().unwrap().hints());
        for h in &hints {
            assert!(h.chars().count() <= 76, "{h}");
        }
        let mut short = mockup();
        short.new.truncate(10);
        let without = PreviewPrompt::new("Run documents".into(), short).hints();
        snapshot(
            "preview_prompt_hints",
            format!(
                "cut:      {}\nnot cut:  {without}\nview:     {}\ntyping:   {}\nfiltered: {}\n",
                hints[0], hints[2], hints[3], hints[4]
            ),
        );
    }

    #[test]
    fn whole_list_screens() {
        let mut prompt = PreviewPrompt::new("Run documents".into(), long_preview());
        prompt.handle_key(key(KeyCode::Char('l')));
        snapshot("full_list_80", render_sized(&prompt, 80, 60));
        snapshot("full_list_120", render_sized(&prompt, 120, 60));
        // A long path is shortened from the front, the size column stays.
        let mut p = long_preview();
        p.new[5].path = format!(
            "Projects/2026/{}report.docx",
            "a-long-folder-name/".repeat(5)
        );
        let mut prompt = PreviewPrompt::new("Run documents".into(), p);
        prompt.handle_key(key(KeyCode::Char('l')));
        let screen = render_sized(&prompt, 60, 30);
        assert!(screen.contains("…"), "{screen}");
        let sizes: Vec<usize> = screen
            .lines()
            .filter(|l| l.contains(".docx "))
            .map(|l| l[..l.find(" MB ").unwrap()].chars().count())
            .collect();
        assert!(
            sizes.len() > 5 && sizes.iter().all(|c| *c == sizes[0]),
            "{screen}"
        );
    }

    #[test]
    fn two_way_whole_lists() {
        let mut plan = plan_mockup();
        plan.delete_local = (0..61).rev().map(|i| format!("Old/{i:02}.txt")).collect();
        plan.to_remote.new = (0..15)
            .map(|i| file(&format!("Up/{i:02}.txt"), 1_000))
            .collect();
        // The plan's own byte count is what the heading shows.
        plan.to_remote.bytes = 263_000;
        let lists = plan_full_lists(&plan);
        let headings: Vec<String> = lists.iter().map(FullList::heading).collect();
        assert_eq!(
            headings,
            [
                "Will be deleted on this Mac · 61 files",
                "New and changed, to OneDrive · 16 files · 263 KB",
            ]
        );
        assert_eq!(lists[0].files[0].path, "Old/00.txt", "sorted");
        // Built as the run screen builds it.
        let (mut body, q, default) = question(&Question::Plan(&plan), "onedrive");
        body.splice(0..0, plan_screen(&plan).0);
        let mut prompt =
            PreviewPrompt::with_lines("Run documents".into(), body, q, default, "sync")
                .with_full_lists(lists);
        prompt.handle_key(key(KeyCode::Char('l')));
        snapshot("full_list_two_way_80", render_sized(&prompt, 80, 30));
        // Scrolled into the middle of a list, its heading stays on top, so
        // a deletion never reads as an upload (colour alone is lost under
        // NO_COLOR).
        for _ in 0..30 {
            prompt.handle_key(key(KeyCode::Down));
        }
        let screen = render_sized(&prompt, 80, 30);
        assert!(
            screen.contains("    Will be deleted on this Mac · 61 files"),
            "{screen}"
        );
        assert!(screen.contains("Old/29.txt"), "{screen}");
        assert!(!screen.contains("Old/28.txt"), "{screen}");
        snapshot("full_list_two_way_pinned_80", screen);
        render_sized(&prompt, 80, 20);
        prompt.handle_key(key(KeyCode::Char('G')));
        let screen = render_sized(&prompt, 80, 20);
        assert!(
            screen.contains("    New and changed, to OneDrive · 16 files · 263 KB"),
            "{screen}"
        );
        assert!(!screen.contains("Will be deleted"), "{screen}");
        assert!(screen.contains("Up/14.txt"), "{screen}");
        // Every row is seen when paging through, none hidden under a
        // pinned heading.
        prompt.handle_key(key(KeyCode::Char('g')));
        let mut seen = std::collections::HashSet::new();
        for _ in 0..20 {
            for l in render_sized(&prompt, 80, 30).lines() {
                if let Some(i) = l.find("Old/") {
                    seen.insert(l[i..i + 6].to_string());
                }
            }
            prompt.handle_key(key(KeyCode::PageDown));
        }
        assert_eq!(seen.len(), 61);
        // Questions without lists are as before.
        let (body, q, default) = question(&Question::StaleLock { pid: 1, path: "x" }, "onedrive");
        let mut prompt = PreviewPrompt::with_lines("Run".into(), body, q, default, "remove it");
        assert!(!prompt.hints().contains("whole list"));
        assert_eq!(prompt.handle_key(key(KeyCode::Char('l'))), None);
        assert!(prompt.view.is_none());
    }

    /// 100,000 paths: only the rows on screen are made, so drawing and
    /// moving stay well under a frame; filtering is one pass over the
    /// lower-case paths made once.
    #[test]
    fn a_hundred_thousand_paths_draw_quickly() {
        let mut p = mockup();
        p.new = (0..100_000)
            .map(|i| {
                file(
                    &format!("Photos/{:03}/IMG_{i:06}.HEIC", i / 1000),
                    2_400_000,
                )
            })
            .collect();
        p.new_bytes = 240_000_000_000;
        let mut prompt = PreviewPrompt::new("Run photos".into(), p);
        prompt.handle_key(key(KeyCode::Char('l')));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..50 {
            prompt.handle_key(key(KeyCode::PageDown));
            terminal.draw(|f| prompt.render(f)).unwrap();
        }
        let took = start.elapsed();
        // Debug build; a frame is 100 ms on the run screen.
        assert!(took.as_millis() < 1_000, "50 draws took {took:?}");
        render_sized(&prompt, 80, 24);
        prompt.handle_key(key(KeyCode::Char('G')));
        let screen = render_sized(&prompt, 80, 24);
        assert!(screen.contains("99,985–100,000 of 100,000"), "{screen}");
        assert!(screen.contains("IMG_099999.HEIC"), "{screen}");
        snapshot("full_list_100k_80", screen);
        prompt.handle_key(key(KeyCode::Char('g')));
        prompt.handle_key(key(KeyCode::Char('/')));
        let start = std::time::Instant::now();
        for c in "img_0420".chars() {
            prompt.handle_key(key(KeyCode::Char(c)));
        }
        assert!(start.elapsed().as_millis() < 5_000);
        assert_eq!(prompt.view.as_ref().unwrap().shown, 100);
    }

    /// Paths as macOS hands them back: wide characters, and accents
    /// decomposed (NFD). The size column stays put in the preview and in
    /// the whole-lists view, and the filter finds them typed composed.
    #[test]
    fn wide_and_decomposed_names() {
        let mut p = long_preview();
        p.new[1].path =
            "日本語のフォルダ/とても長い名前/もっと長いサブフォルダ/写真ファイル番号_1.jpg".into();
        p.new[2].path = "Cafe\u{301}/re\u{301}sume\u{301}.docx".into();
        p.new[3].path = "日本語/短い.jpg".into();
        let column = |screen: &str, unit: &str| -> Vec<usize> {
            screen
                .lines()
                .filter(|l| l.contains('/') && l.contains(unit) && !l.contains(" · "))
                .map(|l| {
                    let i = l.rfind(&format!(" {unit}")).unwrap();
                    crate::ui::width(&l[..i])
                })
                .collect()
        };
        // The ten-path preview, short enough that all of them show.
        let mut short = p.clone();
        short.new.truncate(6);
        // Under the preview's 50-column cap, as a longer path pushes its
        // own size out, by design.
        short.new[1].path = "日本語のフォルダ/写真_1.jpg".into();
        let text = render_text(&short, false);
        assert!(text.contains("日本語/短い.jpg"), "{text}");
        let cols = column(&text, "MB");
        assert!(
            cols.len() == 6 && cols.iter().all(|c| *c == cols[0]),
            "{text}"
        );
        // The whole-lists view at 80 columns: the long wide path is cut
        // from the front and keeps its size.
        let mut prompt = PreviewPrompt::new("Run documents".into(), p);
        prompt.handle_key(key(KeyCode::Char('l')));
        let top = render_sized(&prompt, 80, 30);
        prompt.handle_key(key(KeyCode::Char('G')));
        let end = render_sized(&prompt, 80, 30);
        assert!(crate::sync::nfc(&top).contains("Café/résumé.docx"), "{top}");
        assert!(end.contains("…"), "{end}");
        assert!(end.contains("写真ファイル番号_1.jpg"), "{end}");
        let cols = [column(&top, "MB"), column(&end, "MB")].concat();
        assert!(
            cols.len() > 20 && cols.iter().all(|c| *c == cols[0]),
            "{top}{end}"
        );
        prompt.handle_key(key(KeyCode::Char('g')));
        // Typed composed, found decomposed.
        prompt.handle_key(key(KeyCode::Char('/')));
        for c in "CAFÉ".chars() {
            prompt.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(prompt.view.as_ref().unwrap().shown, 1);
        prompt.handle_key(key(KeyCode::Esc));
        prompt.handle_key(key(KeyCode::Char('/')));
        for c in "re\u{301}sume\u{301}".chars() {
            prompt.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(prompt.view.as_ref().unwrap().shown, 1);
    }

    /// However short the terminal, the keys show and the position counts
    /// from the first file at or after the scroll offset.
    #[test]
    fn tiny_terminals() {
        let mut prompt = PreviewPrompt::new("Run documents".into(), long_preview());
        prompt.handle_key(key(KeyCode::Char('l')));
        for height in 1..=10u16 {
            let screen = render_sized(&prompt, 80, height);
            if height >= 3 {
                assert!(screen.contains("esc/l back"), "{height}: {screen}");
            }
            assert!(!screen.contains("0 of 49"), "{height}: {screen}");
        }
        let screen = render_sized(&prompt, 80, 8);
        assert!(screen.contains("1 of 49"), "{screen}");
        // Keys still move on a tiny screen and stop at the end.
        for _ in 0..100 {
            prompt.handle_key(key(KeyCode::PageDown));
        }
        let screen = render_sized(&prompt, 80, 8);
        assert!(screen.contains("of 49"), "{screen}");
    }
}
