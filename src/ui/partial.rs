//! A partial run: some of one job's destinations, chosen with `p` where
//! Enter runs the whole job (the Jobs list, `Run which job?`), or named
//! with `run <job> --dest a --dest b`.
//!
//! - [`ChooseDestinations`] is the checklist `p` opens, modelled on the
//!   batch's [`super::batch::ChooseJobs`]; its ticks are remembered per job
//!   in the state file (`partial_unticked`).
//! - [`RunPicker`] is `Run which job?` as an inline list that tells Enter
//!   from `p`.
//! - [`title`] heads the run screen and the plain output, and the closing
//!   summary lists the destinations left out as [`NOT_CHOSEN`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::accounts::{Inline, Picked, Picker};
use super::batch::{cut, plural, seg, tick_key, tick_keys};
use super::theme::{self, Tone};
use super::Segment;
use crate::model::Mode;

/// What follows `Run` on the run screen of a partial run: `records ·
/// partial, 2 of 3 destinations`.
pub fn title(job: &str, chosen: usize, of: usize) -> String {
    format!(
        "{job} {} partial, {chosen} of {of} destinations",
        theme::DOT
    )
}

/// The outcome of a destination left out of a partial run, in its
/// closing summary (never in the exit code or `done`).
pub const NOT_CHOSEN: &str = "not-chosen";

/// The reason after `not chosen` in the closing summary: when the
/// destination last ran for real, as the Jobs list says it (`last run 3d
/// ago`), or `never run`.
pub fn not_chosen_reason(finished: Option<i64>, now: i64) -> String {
    match finished {
        Some(t) => format!("last run {}", super::jobs::ago(t, now)),
        None => "never run".to_string(),
    }
}

// --------------------------------------------------------- choosing

/// One destination on the checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestChoice {
    pub id: String,
    /// As shown: `work:Backups/Documents` or a local path.
    pub target: String,
    pub mode: Mode,
    /// What the Jobs list says about it: `✓ 2h ago`, `◎ not mounted`.
    pub note: Segment,
}

/// The checklist of a job's destinations: tick the ones to run, then
/// Enter. Space ticks and unticks, `a` ticks all, `n` none; Enter starts
/// once one is ticked; Esc goes back.
#[derive(Debug, Clone)]
pub struct ChooseDestinations {
    pub job: String,
    /// Already shortened with `~`.
    pub source: String,
    pub destinations: Vec<DestChoice>,
    pub ticked: Vec<bool>,
    pub cursor: usize,
    pub width: Option<usize>,
}

impl ChooseDestinations {
    /// Every destination ticked but the ones unticked last time.
    pub fn new(
        job: &str,
        source: &str,
        destinations: Vec<DestChoice>,
        unticked: &[String],
    ) -> Self {
        let ticked = destinations
            .iter()
            .map(|d| !unticked.contains(&d.id))
            .collect();
        ChooseDestinations {
            job: job.to_string(),
            source: source.to_string(),
            destinations,
            ticked,
            cursor: 0,
            width: None,
        }
    }

    /// The ids ticked, in config order.
    pub fn chosen(&self) -> Vec<String> {
        self.destinations
            .iter()
            .zip(&self.ticked)
            .filter(|(_, t)| **t)
            .map(|(d, _)| d.id.clone())
            .collect()
    }

    /// `2 of 3 destinations will run, one after another`.
    pub fn counts(&self) -> String {
        let n = self.ticked.iter().filter(|t| **t).count();
        let of = self.destinations.len();
        match n {
            0 => format!("No destination ticked {} space ticks one", theme::DOT),
            _ if n == of => format!(
                "All {} will run, as enter on the job does",
                plural(of, "destination")
            ),
            1 => format!("1 of {of} destinations will run"),
            _ => format!("{n} of {of} destinations will run, one after another"),
        }
    }

    /// Applies one key: the ids ticked once started, `Some(None)` to go
    /// back (Esc, q, Ctrl-C).
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Option<Vec<String>>> {
        if tick_key(&key, &mut self.ticked, &mut self.cursor) {
            return None;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(None),
            KeyCode::Esc | KeyCode::Char('q') => Some(None),
            KeyCode::Enter if self.ticked.iter().any(|t| *t) => Some(Some(self.chosen())),
            _ => None,
        }
    }

    /// The screen's lines: the title, a row per destination, the counts
    /// and the keys, each fitted to the width.
    pub fn segments(&self) -> Vec<Vec<Segment>> {
        let max = self.width.map(|w| w.saturating_sub(1));
        let note_w = self
            .destinations
            .iter()
            .map(|d| super::width(&d.note.0))
            .max()
            .unwrap_or(0);
        // The destination column gives way first, then the note is cut.
        let target_max = self
            .width
            .map(|w| w.saturating_sub(8 + 3 + 10 + note_w + 1).max(18));
        let shown: Vec<String> = self
            .destinations
            .iter()
            .map(|d| match target_max {
                Some(m) => super::shorten_path(&d.target, m),
                None => d.target.clone(),
            })
            .collect();
        let target_w = shown.iter().map(|s| super::width(s)).max().unwrap_or(0) + 3;
        let mut lines = vec![
            cut(
                vec![
                    seg("  ", Tone::Plain),
                    seg(format!("Choose destinations for {}", self.job), Tone::Bold),
                    seg(format!("   {}", self.source), Tone::Plain),
                ],
                max,
            ),
            Vec::new(),
        ];
        for (i, dest) in self.destinations.iter().enumerate() {
            let box_ = if self.ticked[i] { "[x]" } else { "[ ]" };
            let target = super::pad(&shown[i], target_w);
            let mut line = if i == self.cursor {
                vec![
                    seg("  ", Tone::Plain),
                    seg(theme::CURSOR, Tone::Accent),
                    seg(format!(" {box_} "), Tone::Accent),
                    seg(target, Tone::Accent),
                ]
            } else {
                vec![seg(format!("    {box_} {target}"), Tone::Plain)]
            };
            line.push(seg(format!("{:<10}", dest.mode.as_str()), Tone::Plain));
            line.push(dest.note.clone());
            lines.push(cut(line, max));
        }
        lines.push(Vec::new());
        lines.push(cut(
            vec![seg(format!("  {}", self.counts()), Tone::Plain)],
            max,
        ));
        lines.push(Vec::new());
        lines.push(cut(tick_keys(), max));
        lines
    }
}

impl Inline for ChooseDestinations {
    type Out = Vec<String>;
    fn set_width(&mut self, width: Option<usize>) {
        self.width = width;
    }
    fn segments(&self) -> Vec<Vec<Segment>> {
        ChooseDestinations::segments(self)
    }
    fn key(&mut self, key: KeyEvent) -> Option<Option<Vec<String>>> {
        self.handle_key(key)
    }
    /// `  Choose destinations for records: boramektup:Records, work:Archive`.
    fn answer_line(&self, out: &Vec<String>, colour: bool) -> String {
        let shown: Vec<&str> = self
            .destinations
            .iter()
            .filter(|d| out.contains(&d.id))
            .map(|d| d.target.as_str())
            .collect();
        format!(
            "  {} {}\n",
            theme::paint(
                &format!("Choose destinations for {}:", self.job),
                Tone::Bold,
                colour
            ),
            shown.join(", ")
        )
    }
}

// ----------------------------------------------------------- picker

/// `Run which job?` as an inline list that tells Enter ([`Picked::Chosen`])
/// from `p` on a job with two destinations or more ([`Picked::Partial`]).
pub struct RunPicker(pub Picker);

impl Inline for RunPicker {
    type Out = Picked;
    fn set_width(&mut self, width: Option<usize>) {
        self.0.width = width;
    }
    fn segments(&self) -> Vec<Vec<Segment>> {
        self.0.segments()
    }
    fn key(&mut self, key: KeyEvent) -> Option<Option<Picked>> {
        self.0.handle_key(key).map(|p| match p {
            Picked::Cancelled => None,
            chosen => Some(chosen),
        })
    }
    /// The picker's answer line for Enter; none for `p`, which leads to the
    /// checklist and leaves its own.
    fn answer_line(&self, out: &Picked, colour: bool) -> String {
        match out {
            Picked::Chosen(i) => self.0.answer_line(*i, colour),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &str, value: String) {
        insta::with_settings!({
            snapshot_path => concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"),
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!(name, value);
        });
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(lines: &[Vec<Segment>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.iter()
                    .map(|(t, _)| t.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The mockup's three destinations of `records`.
    fn choices() -> Vec<DestChoice> {
        let dest = |id: &str, target: &str, mode, note: &str, tone| DestChoice {
            id: id.into(),
            target: target.into(),
            mode,
            note: (note.into(), tone),
        };
        vec![
            dest(
                "d-0001",
                "boramektup:Records",
                Mode::OneWay,
                "✓ 2h ago",
                Tone::Success,
            ),
            dest(
                "d-0002",
                "/Volumes/T7 Shield/Records",
                Mode::OneWay,
                "◎ not mounted",
                Tone::Warning,
            ),
            dest(
                "d-0003",
                "work:Archive/Records",
                Mode::TwoWay,
                "✗ needs resync 3d ago",
                Tone::Error,
            ),
        ]
    }

    #[test]
    fn choosing_destinations_ticks_counts_and_starts() {
        // Ticked as last time: the T7 was left out; an id no longer in the
        // job is ignored, and a destination not known then is ticked.
        let mut c = ChooseDestinations::new(
            "records",
            "~/Records",
            choices(),
            &["d-0002".into(), "d-gone".into()],
        );
        assert_eq!(c.ticked, [true, false, true]);
        assert_eq!(
            c.counts(),
            "2 of 3 destinations will run, one after another"
        );
        c.handle_key(key(KeyCode::Down));
        c.width = Some(80);
        snapshot("partial_choose_80", text(&c.segments()));
        c.width = Some(50);
        let narrow = c.segments();
        for line in &narrow {
            let w: usize = line.iter().map(|(t, _)| crate::ui::width(t)).sum();
            assert!(w < 50, "{}", text(std::slice::from_ref(line)));
        }
        snapshot("partial_choose_50", text(&narrow));
        // Space ticks the row under the cursor: all ticked is a whole run.
        assert_eq!(c.handle_key(key(KeyCode::Char(' '))), None);
        assert_eq!(c.ticked, [true, true, true]);
        assert_eq!(
            c.counts(),
            "All 3 destinations will run, as enter on the job does"
        );
        c.handle_key(key(KeyCode::Char(' ')));
        // n none: Enter does nothing until one is ticked.
        c.handle_key(key(KeyCode::Char('n')));
        assert_eq!(c.ticked, [false, false, false]);
        assert_eq!(c.counts(), "No destination ticked · space ticks one");
        assert_eq!(c.handle_key(key(KeyCode::Enter)), None);
        c.handle_key(key(KeyCode::Char('j')));
        c.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(c.counts(), "1 of 3 destinations will run");
        assert_eq!(
            c.handle_key(key(KeyCode::Enter)),
            Some(Some(vec!["d-0003".into()]))
        );
        c.handle_key(key(KeyCode::Char('k')));
        c.handle_key(key(KeyCode::Up));
        c.handle_key(key(KeyCode::Up));
        assert_eq!(c.cursor, 0, "no wrap");
        c.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            c.handle_key(key(KeyCode::Enter)),
            Some(Some(vec!["d-0001".into(), "d-0003".into()])),
            "config order"
        );
        // a all.
        c.handle_key(key(KeyCode::Char('a')));
        assert_eq!(c.chosen().len(), 3);
        assert_eq!(
            Inline::answer_line(&c, &vec!["d-0001".into(), "d-0003".into()], false),
            "  Choose destinations for records: boramektup:Records, work:Archive/Records\n"
        );
        // Esc goes back; so do q and Ctrl-C, as on the batch's selection.
        assert_eq!(c.handle_key(key(KeyCode::Esc)), Some(None));
        assert_eq!(c.handle_key(key(KeyCode::Char('q'))), Some(None));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(c.handle_key(ctrl_c), Some(None));
        // All ticked on first use.
        let fresh = ChooseDestinations::new("records", "~/Records", choices(), &[]);
        assert!(fresh.ticked.iter().all(|t| *t));
    }

    #[test]
    fn the_title_and_the_not_chosen_reason() {
        assert_eq!(
            title("records", 2, 3),
            "records · partial, 2 of 3 destinations"
        );
        let now = 1_791_852_062;
        assert_eq!(
            not_chosen_reason(Some(now - 3 * 86_400), now),
            "last run 3d ago"
        );
        assert_eq!(
            not_chosen_reason(Some(now - 2 * 3600), now),
            "last run 2h ago"
        );
        assert_eq!(not_chosen_reason(None, now), "never run");
    }

    fn run_picker() -> Picker {
        let mut picker = Picker::new(
            "Run which job?",
            [
                ("documents", "~/Documents"),
                ("records", "~/Records"),
                ("photos", "~/Pictures"),
            ]
            .iter()
            .map(|(n, s)| (n.to_string(), s.to_string()))
            .collect(),
        );
        picker.extra = Some(super::super::batch::SEVERAL.into());
        picker.esc = "back";
        picker.partial = vec![true, true, false];
        picker
    }

    #[test]
    fn p_on_the_run_picker() {
        let mut p = RunPicker(run_picker());
        // On a job with two destinations or more: some of them.
        assert_eq!(
            p.key(key(KeyCode::Char('p'))),
            Some(Some(Picked::Partial(0)))
        );
        assert_eq!(p.answer_line(&Picked::Partial(0), false), "");
        assert_eq!(p.key(key(KeyCode::Enter)), Some(Some(Picked::Chosen(0))));
        assert_eq!(
            p.answer_line(&Picked::Chosen(0), false),
            "  Run which job?: documents\n"
        );
        snapshot("run_picker_partial", p.0.lines(false).join("\n"));
        // On a job with one: as Enter, and no hint.
        p.key(key(KeyCode::Down));
        p.key(key(KeyCode::Down));
        assert_eq!(
            p.key(key(KeyCode::Char('p'))),
            Some(Some(Picked::Chosen(2)))
        );
        let keys = p.0.lines(false).last().cloned().unwrap();
        assert_eq!(keys, "  ↑↓ j/k move · enter select · esc back");
        // On the several-jobs row: nothing.
        p.key(key(KeyCode::Down));
        assert_eq!(p.0.cursor, 3);
        assert_eq!(p.key(key(KeyCode::Char('p'))), None);
        assert_eq!(p.key(key(KeyCode::Esc)), Some(None));
        // Narrow, every line still fits.
        let mut picker = run_picker();
        picker.width = Some(50);
        for line in picker.lines(false) {
            assert!(crate::ui::width(&line) < 50, "{line}");
        }
        assert_eq!(
            picker.lines(false).last().unwrap(),
            "  ↑↓ j/k move · enter select · esc back",
            "p partial gives way first"
        );
        snapshot("run_picker_partial_50", picker.lines(false).join("\n"));
        // Any other picker ignores p.
        let mut plain = run_picker();
        plain.partial.clear();
        assert_eq!(plain.handle_key(key(KeyCode::Char('p'))), None);
    }
}
