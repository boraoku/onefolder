//! Everything the user sees. The only layer that depends on ratatui or
//! crossterm.

pub mod accounts;
pub mod browse;
pub mod complete;
pub mod form;
pub mod jobform;
pub mod jobs;
pub mod menu;
pub mod plain;
pub mod progress;
pub mod prompt;
pub mod session;
pub mod status;
pub mod summary;
pub mod theme;

use std::io::IsTerminal;

pub use session::screen;

/// The terminal's width in columns when stdout is a terminal. `None` when
/// the output goes to a file or a pipe: lists are then printed in full.
/// Inside the interactive session, the width of the output screen, which
/// keeps two columns of margin.
pub fn terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let margin = if session::is_open() { 2 } else { 0 };
    crossterm::terminal::size()
        .ok()
        .map(|(w, _)| usize::from(w).saturating_sub(margin))
        .filter(|w| *w > 0)
}

/// Columns `text` takes on a terminal: wide characters (CJK, most emoji)
/// take two, combining marks none.
pub fn width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// `text` padded with spaces to `columns` display columns (never cut).
pub fn pad(text: &str, columns: usize) -> String {
    let w = width(text);
    format!("{text}{}", " ".repeat(columns.saturating_sub(w)))
}

fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// The longest start of `text` that fits in `columns`.
fn front(text: &str, columns: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = char_width(c);
        if used + w > columns {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// The longest end of `text` that fits in `columns`.
fn back(text: &str, columns: usize) -> String {
    let mut out: Vec<char> = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let w = char_width(c);
        if used + w > columns {
            break;
        }
        used += w;
        out.push(c);
    }
    out.into_iter().rev().collect()
}

/// Shortens a path to at most `max` display columns by cutting out its
/// middle and keeping its last two components whole when they fit:
/// `/Volumes/Backu…/Documents/2026`. A OneDrive target keeps its
/// `account:` prefix (`work:Backu…/Documents/2026`). A path that fits is
/// returned as is.
pub fn shorten_path(path: &str, max: usize) -> String {
    if width(path) <= max {
        return path.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    // `account:path`: keep the account, shorten the rest.
    if !path.starts_with('/') {
        if let Some((account, rest)) = path.split_once(':') {
            let prefix = format!("{account}:");
            if !account.contains('/') && width(&prefix) + 8 <= max {
                return format!("{prefix}{}", shorten_path(rest, max - width(&prefix)));
            }
        }
    }
    let parts: Vec<&str> = path.split('/').collect();
    let tail = if parts.len() > 2 {
        parts[parts.len() - 2..].join("/")
    } else {
        String::new()
    };
    // Room for a few columns of the start, the ellipsis and the slash.
    if !tail.is_empty() && width(&tail) + 2 + 4 <= max {
        let head = front(path, max - width(&tail) - 2);
        return format!("{head}…/{tail}");
    }
    format!("…{}", back(path, max - 1))
}

/// Fits text into `max` display columns: a path is shortened in the
/// middle ([`shorten_path`]), anything else cut at the end with `…`.
pub fn fit_text(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    if text.starts_with('/') || text.starts_with('~') {
        return shorten_path(text, max);
    }
    if max == 0 {
        return String::new();
    }
    // At the last ` · ` part, else the last word's end, when that keeps a
    // third of the room, so no word is cut.
    let head = front(text, max - 1);
    let whole = !text[head.len()..].starts_with(|c: char| c != ' ');
    let enough = |at: usize| width(&head[..at]) * 3 >= max;
    let head = match (head.rfind(" · "), head.rfind(' ')) {
        _ if whole => head,
        (Some(at), _) if enough(at) => head[..at].to_string(),
        (_, Some(at)) if enough(at) => head[..at].to_string(),
        _ => head,
    };
    let head = head.trim_end().trim_end_matches(['·', ',']).trim_end();
    format!("{head}…")
}

/// Greedy word wrap to `columns` display columns; a word longer than a
/// line stays whole on its own line.
pub fn wrap(text: &str, columns: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    // A number and its unit (`184.3 MB`) stay on one line.
    let mut words: Vec<String> = Vec::new();
    for word in text.split(' ') {
        let unit = matches!(word, "B" | "KB" | "MB" | "GB" | "TB" | "PB");
        match words.last_mut() {
            Some(last) if unit && last.ends_with(|c: char| c.is_ascii_digit()) => {
                last.push(' ');
                last.push_str(word);
            }
            _ => words.push(word.to_string()),
        }
    }
    for word in &words {
        let word = word.as_str();
        if !line.is_empty() && width(&line) + 1 + width(word) > columns {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// A run of text with one tone, as the line builders produce them.
pub type Segment = (String, theme::Tone);

/// Fits one line of segments into `columns` display columns (never under
/// [`theme::MIN_WIDTH`]): a line that is too long is wrapped at spaces,
/// and every following piece starts `indent` columns in. A word longer
/// than a whole line is cut. A line that fits is returned as it is.
pub fn fit_line(line: Vec<Segment>, columns: usize, indent: usize) -> Vec<Vec<Segment>> {
    let columns = columns.max(theme::MIN_WIDTH);
    let total: usize = line.iter().map(|(t, _)| width(t)).sum();
    if total <= columns {
        return vec![line];
    }
    let indent = indent.min(columns / 2);
    // Words with the spaces before them, each in its tone.
    let mut words: Vec<(String, String, theme::Tone)> = Vec::new();
    let mut lead = String::new();
    for (text, tone) in &line {
        let mut word = String::new();
        for c in text.chars() {
            if c == ' ' {
                if !word.is_empty() {
                    push_word(
                        &mut words,
                        std::mem::take(&mut lead),
                        std::mem::take(&mut word),
                        *tone,
                    );
                }
                lead.push(' ');
            } else {
                word.push(c);
            }
        }
        if !word.is_empty() {
            push_word(&mut words, std::mem::take(&mut lead), word, *tone);
        }
    }
    let mut out: Vec<Vec<Segment>> = Vec::new();
    let mut current: Vec<Segment> = Vec::new();
    let mut used = 0;
    let mut has_word = false;
    for (spaces, word, tone) in words {
        let mut word = word;
        let gap = if has_word || current.is_empty() {
            width(&spaces)
        } else {
            0
        };
        if has_word && used + gap + width(&word) > columns {
            out.push(std::mem::take(&mut current));
            current.push((" ".repeat(indent), theme::Tone::Plain));
            used = indent;
        } else if gap > 0 {
            current.push((spaces, theme::Tone::Plain));
            used += gap;
        }
        // A word wider than the room left on a fresh line is cut.
        while used + width(&word) > columns {
            let room = columns.saturating_sub(used).max(1);
            let head = front(&word, room);
            if head.is_empty() {
                break;
            }
            word = word[head.len()..].to_string();
            current.push((head, tone));
            out.push(std::mem::take(&mut current));
            current.push((" ".repeat(indent), theme::Tone::Plain));
            used = indent;
        }
        used += width(&word);
        current.push((word, tone));
        has_word = true;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Adds a word for [`fit_line`]; a unit after a number (`184.3 MB`) joins
/// the number, so the two are never split.
fn push_word(
    words: &mut Vec<(String, String, theme::Tone)>,
    lead: String,
    word: String,
    tone: theme::Tone,
) {
    let unit = matches!(word.as_str(), "B" | "KB" | "MB" | "GB" | "TB" | "PB");
    if let Some(last) = words.last_mut() {
        if unit && lead == " " && last.2 == tone && last.1.ends_with(|c: char| c.is_ascii_digit()) {
            last.1.push(' ');
            last.1.push_str(&word);
            return;
        }
    }
    words.push((lead, word, tone));
}

/// [`fit_line`] for every line, with each continuation indented four
/// columns past the line's own indent. `None` leaves the lines alone.
pub fn fit_lines(lines: Vec<Vec<Segment>>, columns: Option<usize>) -> Vec<Vec<Segment>> {
    let Some(columns) = columns else {
        return lines;
    };
    lines
        .into_iter()
        .flat_map(|line| {
            let text: String = line.iter().map(|(t, _)| t.as_str()).collect();
            let own = width(&text) - width(text.trim_start());
            fit_line(line, columns, own + 4)
        })
        .collect()
}

/// Lines of segments as text, painted when `colour` is set, each without
/// trailing spaces and ending with a newline.
pub fn paint_lines(lines: &[Vec<Segment>], colour: bool) -> String {
    let mut out = String::new();
    for line in lines {
        let text: String = line
            .iter()
            .map(|(t, tone)| theme::paint(t, *tone, colour))
            .collect();
        if colour {
            out.push_str(&text);
        } else {
            out.push_str(text.trim_end());
        }
        out.push('\n');
    }
    out
}

/// `"Old projects/2019"`: a folder name in quotes, so leading and trailing
/// spaces, which can belong to a name, are visible.
pub fn quoted(name: &str) -> String {
    format!("\"{name}\"")
}

/// Folder names quoted and joined with commas.
pub fn quoted_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| quoted(n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Prints a command's text output on stdout; inside the interactive
/// session it goes on the session's page instead, to be shown on a screen
/// ([`session::show_page`]).
pub fn out(text: &str) -> anyhow::Result<()> {
    if session::is_open() {
        session::add(text);
        Ok(())
    } else {
        crate::jsonout::write_stdout(text.as_bytes())
    }
}

/// [`out`] for stderr: messages and refusals. Inside the session they go
/// on the same page, in the order they were printed.
pub fn err(text: &str) {
    if session::is_open() {
        session::add(text);
    } else {
        eprint!("{text}");
    }
}

/// After a command's output, before a screen takes the terminal back:
/// inside the session, the output on a screen to read
/// ([`session::show_page`]); otherwise `prompt` and a key press on the
/// normal screen ([`wait_for_key`]).
pub fn pause(prompt: &str) -> anyhow::Result<()> {
    if session::is_open() {
        session::show_page()
    } else {
        wait_for_key(prompt)
    }
}

/// Waits for one key press on the terminal, after printing `prompt`.
/// Used after a command's output, before a screen takes the terminal
/// back. Returns at once when stdin is not a terminal.
pub fn wait_for_key(prompt: &str) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyEventKind};
    use std::io::Write;
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }
    let mut out = std::io::stdout();
    write!(out, "{prompt}")?;
    out.flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let _raw = RawMode;
    loop {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                if is_ctrl_c(&key) {
                    drop(_raw);
                    quit_on_ctrl_c();
                }
                break;
            }
        }
    }
    drop(_raw);
    writeln!(out)?;
    Ok(())
}

/// True for Ctrl-C read as a key.
pub fn is_ctrl_c(key: &crossterm::event::KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Char('c')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
}

/// Puts the terminal back, once, whoever took it: ends the interactive
/// session (which prints its transcript) when one is open, otherwise
/// leaves the raw mode and alternate screen of the screen that is up. The
/// exit hook a signal runs, and Ctrl-C on a screen.
pub fn restore_terminal() {
    session::end();
    session::leave();
}

/// Ctrl-C on a screen other than the run screen: puts the terminal back
/// and quits OneFolder with exit code 130, as a shell expects after Ctrl-C.
/// Inside the session the transcript of its runs is printed first.
pub fn quit_on_ctrl_c() -> ! {
    let session = session::is_open();
    restore_terminal();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
    if !session {
        println!();
    }
    crate::rclone::exec::exit_now(130)
}

/// Leaves raw mode when dropped.
struct RawMode;

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::shorten_path;

    #[test]
    fn long_lines_are_fitted_at_40_60_80_and_120() {
        use super::{fit_lines, width, Segment};
        use crate::ui::theme::Tone;
        let line: Vec<Segment> = vec![
            ("  ✗ ".into(), Tone::Error),
            (
                "personal    personal   token expired · run: onefolder accounts reconnect \
                 personal and then a much longer explanation that goes on and on"
                    .into(),
                Tone::Dim,
            ),
        ];
        let long_word: Vec<Segment> = vec![(
            format!("  {}", "/Volumes/バックアップ/".repeat(12)),
            Tone::Plain,
        )];
        for columns in [40, 60, 80, 120] {
            for l in [line.clone(), long_word.clone()] {
                let fitted = fit_lines(vec![l.clone()], Some(columns));
                for piece in &fitted {
                    let text: String = piece.iter().map(|(t, _)| t.as_str()).collect();
                    assert!(width(&text) <= columns, "{columns}: {text}");
                }
                let all: String = fitted
                    .iter()
                    .flat_map(|p| p.iter().map(|(t, _)| t.trim()))
                    .collect();
                let before: String = l.iter().map(|(t, _)| t.replace(' ', "")).collect();
                assert_eq!(all.replace(' ', ""), before, "nothing is lost");
            }
        }
        // Continuations are indented past the line's own indent.
        let fitted = fit_lines(vec![line.clone()], Some(40));
        let second: String = fitted[1].iter().map(|(t, _)| t.as_str()).collect();
        assert!(second.starts_with("      ") && !second.starts_with("       "));
        // Narrower than 40 is fitted to 40.
        assert_eq!(fit_lines(vec![line.clone()], Some(10)), fitted);
        assert_eq!(fit_lines(vec![line.clone()], None), vec![line]);
    }

    #[test]
    fn text_is_cut_at_a_word_with_an_ellipsis() {
        use super::fit_text;
        assert_eq!(fit_text("short", 10), "short");
        assert_eq!(
            fit_text("3 jobs · 4 destinations · 1 drive not mounted", 30),
            "3 jobs · 4 destinations…"
        );
        assert_eq!(fit_text("abcdefghijkl", 6), "abcde…");
        let path = fit_text("/Volumes/Backup/a/b/c/d", 14);
        assert!(
            super::width(&path) <= 14 && path.ends_with("/c/d"),
            "{path}"
        );
    }

    #[test]
    fn folder_names_are_quoted() {
        assert_eq!(super::quoted(" Old "), "\" Old \"");
        assert_eq!(
            super::quoted_list(&["a".into(), "b c ".into()]),
            "\"a\", \"b c \""
        );
    }

    #[test]
    fn paths_are_shortened_in_the_middle() {
        assert_eq!(
            shorten_path("/Volumes/Backup/Docs", 40),
            "/Volumes/Backup/Docs"
        );
        let long = "/Volumes/Backup/Very/Long/Path/Documents/2026";
        let short = shorten_path(long, 30);
        assert_eq!(short, "/Volumes/Backu…/Documents/2026");
        assert_eq!(short.chars().count(), 30);
        // The last two components do not fit: keep the end.
        let tight = shorten_path(long, 12);
        assert_eq!(tight, "…uments/2026");
        assert_eq!(tight.chars().count(), 12);
        // A OneDrive target.
        assert!(
            shorten_path("work:Backups/Clients/Acme/Contracts/Signed", 28)
                .ends_with("…/Contracts/Signed")
        );
        assert_eq!(shorten_path("abcdef", 1), "…");
        assert_eq!(shorten_path("abcdef", 3), "…ef");
    }

    #[test]
    fn wide_characters_count_twice() {
        use super::{pad, width};
        assert_eq!(width("書類"), 4);
        assert_eq!(pad("書類", 6), "書類  ");
        let path = "/Volumes/バックアップ/書類/二〇二六年/契約書";
        let short = shorten_path(path, 30);
        assert!(width(&short) <= 30, "{short}");
        assert!(short.ends_with("…/二〇二六年/契約書"), "{short}");
        let tight = shorten_path(path, 9);
        assert!(width(&tight) <= 9, "{tight}");
        assert!(tight.starts_with('…'), "{tight}");
    }
}
