//! The look: glyphs, colours, rules and borders, after Mole.
//!
//! Colour is used sparingly: one accent for the cursor and bars, green for
//! success, yellow for skipped items and warnings, red for errors.
//! Everything else uses the terminal's default foreground.

use std::io::IsTerminal;

use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;

/// Done.
pub const DONE: &str = "✓";
/// Skipped, with a reason.
pub const SKIPPED: &str = "◎";
/// An action taken, or a destination.
pub const ACTION: &str = "→";
/// The current item.
pub const CURSOR: &str = "➤";
/// A selected choice.
pub const SELECTED: &str = "●";
/// An unselected choice.
pub const UNSELECTED: &str = "○";
/// Failed.
pub const FAILED: &str = "✗";
/// The status line in the main menu.
pub const GEAR: &str = "⚙";
/// Upload, in the run screen.
pub const UP: &str = "↑";
/// Download, in the run screen.
pub const DOWN: &str = "↓";
/// Separator between short facts on one line.
pub const DOT: &str = "·";
/// Filled part of a bar.
pub const BAR_FULL: char = '█';
/// Empty part of a bar.
pub const BAR_EMPTY: char = '░';

/// Width of the rules that frame the summary block.
pub const RULE_WIDTH: usize = 70;

/// The narrowest terminal the output is fitted to.
pub const MIN_WIDTH: usize = 40;

/// The rule for a terminal `width` columns wide: [`RULE_WIDTH`], or the
/// terminal's width when it is narrower (never under [`MIN_WIDTH`]).
/// `None` (output to a file or a pipe) gives the full width.
pub fn rule_for(width: Option<usize>) -> String {
    let w = width.map_or(RULE_WIDTH, |w| w.clamp(MIN_WIDTH, RULE_WIDTH));
    "=".repeat(w)
}

/// Borders for framed panes.
pub const BORDERS: border::Set = border::ROUNDED;

/// The meaning of a piece of text, mapped to a colour per output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Terminal default.
    Plain,
    /// Titles.
    Bold,
    /// Cursor and bars.
    Accent,
    Success,
    Warning,
    Error,
    /// Hints and secondary facts.
    Dim,
}

pub const ACCENT: Color = Color::Cyan;
pub const SUCCESS: Color = Color::Green;
pub const WARNING: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;

/// The ratatui style for a tone.
pub fn style(tone: Tone) -> Style {
    match tone {
        Tone::Plain => Style::default(),
        Tone::Bold => Style::default().add_modifier(Modifier::BOLD),
        Tone::Accent => Style::default().fg(ACCENT),
        Tone::Success => Style::default().fg(SUCCESS),
        Tone::Warning => Style::default().fg(WARNING),
        Tone::Error => Style::default().fg(ERROR),
        Tone::Dim => Style::default().add_modifier(Modifier::DIM),
    }
}

/// Wraps `text` in ANSI colour codes for plain terminal output. With
/// `colour` false the text is returned unchanged.
pub fn paint(text: &str, tone: Tone, colour: bool) -> String {
    let code = match tone {
        Tone::Plain => return text.to_string(),
        Tone::Bold => "1",
        Tone::Accent => "36",
        Tone::Success => "32",
        Tone::Warning => "33",
        Tone::Error => "31",
        Tone::Dim => "2",
    };
    if colour && !text.is_empty() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Reads text painted by [`paint`] back into toned segments: the inverse
/// of [`paint`], for the interactive session, which shows printed output
/// on a screen. `tone` is the tone in force before the text and is left at
/// the one in force after it, so a painted run can span lines. Other
/// escape sequences are dropped.
pub fn unpaint(text: &str, tone: &mut Tone) -> Vec<super::Segment> {
    let mut out: Vec<super::Segment> = Vec::new();
    let mut run = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            if c != '\r' {
                run.push(c);
            }
            continue;
        }
        // An escape: CSI parameters up to the final letter.
        let mut params = String::new();
        let mut last = None;
        if chars.peek() == Some(&'[') {
            chars.next();
            for p in chars.by_ref() {
                if p.is_ascii_alphabetic() || p == '~' {
                    last = Some(p);
                    break;
                }
                params.push(p);
            }
        }
        if last != Some('m') {
            continue;
        }
        let next = match params.as_str() {
            "1" => Tone::Bold,
            "36" => Tone::Accent,
            "32" => Tone::Success,
            "33" => Tone::Warning,
            "31" => Tone::Error,
            "2" => Tone::Dim,
            _ => Tone::Plain,
        };
        if !run.is_empty() {
            out.push((std::mem::take(&mut run), *tone));
        }
        *tone = next;
    }
    if !run.is_empty() {
        out.push((run, *tone));
    }
    out
}

/// True when stdout is a terminal and `NO_COLOR` is not set.
pub fn colour_on_stdout() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// A progress bar of `width` cells for a fraction between 0 and 1.
pub fn bar(fraction: f64, width: usize) -> String {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let full = (fraction * width as f64).round() as usize;
    let mut s = String::with_capacity(width * 3);
    s.extend(std::iter::repeat_n(BAR_FULL, full));
    s.extend(std::iter::repeat_n(BAR_EMPTY, width - full));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_match_design() {
        let all = [DONE, SKIPPED, ACTION, CURSOR, SELECTED, UNSELECTED, FAILED];
        assert_eq!(all.concat(), "✓◎→➤●○✗");
        assert_eq!(BORDERS.top_left, "╭");
    }

    #[test]
    fn bars() {
        assert_eq!(bar(0.0, 4), "░░░░");
        assert_eq!(bar(0.5, 4), "██░░");
        assert_eq!(bar(1.5, 4), "████");
        assert_eq!(bar(f64::NAN, 2), "░░");
    }

    #[test]
    fn paint_without_colour_is_plain() {
        assert_eq!(paint("ok", Tone::Success, false), "ok");
        assert_eq!(paint("ok", Tone::Success, true), "\x1b[32mok\x1b[0m");
    }

    #[test]
    fn unpaint_reads_paint_back() {
        let tones = [
            Tone::Plain,
            Tone::Bold,
            Tone::Accent,
            Tone::Success,
            Tone::Warning,
            Tone::Error,
            Tone::Dim,
        ];
        for t in tones {
            let mut tone = Tone::Plain;
            let painted = format!("a {}", paint("word", t, true));
            let mut want = vec![("a ".to_string(), Tone::Plain)];
            if t == Tone::Plain {
                want[0].0.push_str("word");
            } else {
                want.push(("word".to_string(), t));
            }
            assert_eq!(unpaint(&painted, &mut tone), want, "{t:?}");
            assert_eq!(tone, Tone::Plain);
        }
        // A run left open carries on; other escapes are dropped.
        let mut tone = Tone::Plain;
        assert_eq!(
            unpaint("\x1b[31mred\x1b[?25l\r", &mut tone),
            vec![("red".to_string(), Tone::Error)]
        );
        assert_eq!(tone, Tone::Error);
        assert_eq!(
            unpaint("still", &mut tone),
            vec![("still".to_string(), Tone::Error)]
        );
    }

    #[test]
    fn rule_is_seventy_wide() {
        assert_eq!(rule_for(None).chars().count(), 70);
        assert_eq!(rule_for(Some(120)).chars().count(), 70);
        assert_eq!(rule_for(Some(60)).chars().count(), 60);
        assert_eq!(rule_for(Some(40)).chars().count(), 40);
        assert_eq!(rule_for(Some(20)).chars().count(), 40, "never under 40");
    }
}
