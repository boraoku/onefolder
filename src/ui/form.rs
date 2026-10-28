//! The in-house form component: a vertical list of fields (text, path,
//! picker, toggle, list of strings, rows managed by the owner, buttons)
//! with inline validation and a footer of key hints. Single-line editing
//! uses `tui-input`.
//!
//! Keys, in the field list: Tab, Shift-Tab, arrows and j/k move; Enter
//! edits a text field, opens a picker, a list or the rows, or presses a
//! button; ←→ and h/l change a picker or toggle in place; Ctrl-S submits;
//! Esc cancels (asking for a second Esc when something changed).
//! Inside an edit, Enter confirms and Esc undoes. Inside a list or the
//! rows, as on the Jobs screen, ↑↓ move the cursor and j/k move the
//! highlighted item down or up (J/K too, as Shift+j/k arrive); while an
//! item's text is edited they are letters like any other.
//!
//! Folder fields (every path field, and a text field marked with
//! [`Field::with_folder`]) also complete: while editing, Tab completes the
//! last path segment to the first matching folder and further Tabs and
//! Shift-Tabs cycle through the matches ([`complete`]); `b` in the field
//! list, or Ctrl-B while editing, asks the owner to open the folder
//! browser ([`FormEvent::Browse`]). A OneDrive field's names come from
//! the owner: the form asks with [`FormEvent::List`] and is answered with
//! [`Form::set_listing`].

use std::collections::HashMap;
use std::path::Path;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use tui_input::backend::crossterm::to_input_request;
use tui_input::Input;

use crate::config;
use crate::ui::complete::{self, Cycle};
use crate::ui::theme::{self, Tone};

/// Checks a text value; `Err` holds the inline message.
pub type Validator = fn(&str) -> Result<(), String>;

/// Width of the label column, cursor included, unless a longer label
/// needs more (see [`Form::label_width`]).
const LABEL: usize = 16;

/// A piece of a row with its tone.
pub type Segment = (String, Tone);

/// What pressing a button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonAction {
    Submit,
    Cancel,
    /// Reported to the owner as [`FormEvent::Pressed`].
    Press,
}

/// What a folder field names: a folder on this Mac, or one on a OneDrive
/// account (relative to the drive root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Folder {
    Local,
    Remote,
}

/// The kinds of field.
#[derive(Debug, Clone)]
pub enum Kind {
    Text {
        input: Input,
        placeholder: String,
    },
    /// A local path: `~` is expanded for the existence hint shown beside it.
    Path {
        input: Input,
        placeholder: String,
        /// True when a missing folder is fine (it will be created).
        missing_ok: bool,
    },
    Picker {
        options: Vec<String>,
        selected: usize,
    },
    Toggle {
        on: bool,
    },
    /// A list of strings edited in place, such as exclude patterns.
    List {
        items: Vec<String>,
        placeholder: String,
        add_label: String,
    },
    /// Rows the owner manages (add, edit, remove, reorder come back as events).
    Rows {
        rows: Vec<Vec<Segment>>,
        add_label: String,
    },
    Button {
        label: String,
        action: ButtonAction,
    },
}

/// One field of a form.
#[derive(Debug, Clone)]
pub struct Field {
    pub key: &'static str,
    pub label: String,
    pub kind: Kind,
    pub help: Option<String>,
    pub error: Option<String>,
    pub visible: bool,
    validate: Option<Validator>,
    /// Set for a folder field: Tab completes and `b` browses.
    pub folder: Option<Folder>,
}

impl Field {
    fn new(key: &'static str, label: &str, kind: Kind) -> Field {
        Field {
            key,
            label: label.to_string(),
            kind,
            help: None,
            error: None,
            visible: true,
            validate: None,
            folder: None,
        }
    }

    pub fn text(key: &'static str, label: &str, value: &str) -> Field {
        Field::new(
            key,
            label,
            Kind::Text {
                input: Input::new(value.to_string()),
                placeholder: String::new(),
            },
        )
    }

    /// A local folder: a folder field ([`Folder::Local`]).
    pub fn path(key: &'static str, label: &str, value: &str, missing_ok: bool) -> Field {
        Field::new(
            key,
            label,
            Kind::Path {
                input: Input::new(value.to_string()),
                placeholder: String::new(),
                missing_ok,
            },
        )
        .with_folder(Folder::Local)
    }

    pub fn picker(key: &'static str, label: &str, options: Vec<String>, selected: usize) -> Field {
        let selected = selected.min(options.len().saturating_sub(1));
        Field::new(key, label, Kind::Picker { options, selected })
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the account form, milestone 7")
    )]
    pub fn toggle(key: &'static str, label: &str, on: bool) -> Field {
        Field::new(key, label, Kind::Toggle { on })
    }

    pub fn list(key: &'static str, label: &str, items: Vec<String>, add_label: &str) -> Field {
        Field::new(
            key,
            label,
            Kind::List {
                items,
                placeholder: "none".to_string(),
                add_label: add_label.to_string(),
            },
        )
    }

    pub fn rows(key: &'static str, label: &str, rows: Vec<Vec<Segment>>, add_label: &str) -> Field {
        Field::new(
            key,
            label,
            Kind::Rows {
                rows,
                add_label: add_label.to_string(),
            },
        )
    }

    pub fn button(key: &'static str, label: &str, action: ButtonAction) -> Field {
        Field::new(
            key,
            "",
            Kind::Button {
                label: label.to_string(),
                action,
            },
        )
    }

    pub fn with_help(mut self, help: &str) -> Field {
        self.help = Some(help.to_string());
        self
    }

    pub fn with_placeholder(mut self, text: &str) -> Field {
        match &mut self.kind {
            Kind::Text { placeholder, .. }
            | Kind::Path { placeholder, .. }
            | Kind::List { placeholder, .. } => *placeholder = text.to_string(),
            _ => {}
        }
        self
    }

    /// Makes a text or path field a folder field.
    pub fn with_folder(mut self, folder: Folder) -> Field {
        self.folder = Some(folder);
        self
    }

    pub fn with_validator(mut self, validate: Validator) -> Field {
        self.validate = Some(validate);
        self
    }

    fn input_mut(&mut self) -> Option<&mut Input> {
        match &mut self.kind {
            Kind::Text { input, .. } | Kind::Path { input, .. } => Some(input),
            _ => None,
        }
    }

    fn value(&self) -> Option<&str> {
        match &self.kind {
            Kind::Text { input, .. } | Kind::Path { input, .. } => Some(input.value()),
            _ => None,
        }
    }

    /// Runs the validator, storing and returning whether it passed.
    fn run_validator(&mut self) -> bool {
        if let (Some(validate), Some(value)) = (self.validate, self.value()) {
            self.error = validate(value).err();
        }
        self.error.is_none()
    }
}

/// What a key press meant to the owner of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormEvent {
    /// Nothing the owner needs to act on.
    None,
    /// Every field validator passed and the user asked to save.
    Submit,
    Cancel,
    /// A field's value changed (a picker moved, a text edit was confirmed).
    Changed(&'static str),
    /// An action on a rows field, for the owner to carry out.
    Row(&'static str, RowAction),
    /// A button with [`ButtonAction::Press`] was pressed.
    Pressed(&'static str),
    /// The folder field wants the folder browser (`b`, or Ctrl-B while
    /// editing, which keeps the text typed so far).
    Browse(&'static str),
    /// The OneDrive folder field needs the names of the folders in a
    /// folder: [`Form::take_list_request`] says which, and
    /// [`Form::set_listing`] answers.
    List(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    Add,
    Edit(usize),
    Remove(usize),
    MoveUp(usize),
    MoveDown(usize),
}

#[derive(Debug, Clone)]
enum State {
    Nav,
    EditText {
        backup: String,
    },
    Picking {
        backup: usize,
    },
    List {
        cursor: usize,
    },
    ListEdit {
        cursor: usize,
        input: Input,
        new: bool,
    },
    Rows {
        cursor: usize,
    },
}

/// A form: title, fields, focus and the current editing state.
#[derive(Debug, Clone)]
pub struct Form {
    pub title: String,
    pub subtitle: Option<String>,
    fields: Vec<Field>,
    focus: usize,
    state: State,
    /// A line under the fields: a note, a warning or an error.
    pub message: Option<(String, Tone)>,
    dirty: bool,
    confirm_discard: bool,
    /// The Tab completion going on in the field being edited.
    cycle: Option<Cycle>,
    /// A line under the field being edited, in place of its help:
    /// `no matching folder`, `listing…` or why it could not be listed.
    note: Option<(String, Tone)>,
    /// OneDrive folder names by folder (relative to the drive root), as
    /// the owner listed them, kept for the life of the form.
    listings: HashMap<String, Vec<String>>,
    /// A listing asked for and not in yet: the field, the folder, the text
    /// when Tab was pressed and whether it went forwards.
    waiting: Option<(usize, String, String, bool)>,
    /// The folder the owner should list next.
    request: Option<String>,
}

const DISCARD: &str = "Press esc again to discard your changes.";

impl Form {
    pub fn new(title: &str, fields: Vec<Field>) -> Form {
        let mut form = Form {
            title: title.to_string(),
            subtitle: None,
            fields,
            focus: 0,
            state: State::Nav,
            message: None,
            dirty: false,
            confirm_discard: false,
            cycle: None,
            note: None,
            listings: HashMap::new(),
            waiting: None,
            request: None,
        };
        if !form.fields.first().is_some_and(|f| f.visible) {
            form.move_focus(1);
        }
        form
    }

    fn index(&self, key: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.key == key)
    }

    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    fn field_mut(&mut self, key: &str) -> Option<&mut Field> {
        self.fields.iter_mut().find(|f| f.key == key)
    }

    /// The value of a text or path field, as typed.
    pub fn text(&self, key: &str) -> String {
        self.field(key)
            .and_then(Field::value)
            .unwrap_or_default()
            .to_string()
    }

    pub fn picked(&self, key: &str) -> usize {
        match self.field(key).map(|f| &f.kind) {
            Some(Kind::Picker { selected, .. }) => *selected,
            _ => 0,
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the account form, milestone 7")
    )]
    pub fn toggled(&self, key: &str) -> bool {
        matches!(
            self.field(key).map(|f| &f.kind),
            Some(Kind::Toggle { on: true })
        )
    }

    pub fn items(&self, key: &str) -> Vec<String> {
        match self.field(key).map(|f| &f.kind) {
            Some(Kind::List { items, .. }) => items.clone(),
            _ => Vec::new(),
        }
    }

    /// Replaces the rows of a rows field, keeping the row cursor in range.
    pub fn set_rows(&mut self, key: &str, new_rows: Vec<Vec<Segment>>) {
        let len = new_rows.len();
        if let Some(Kind::Rows { rows, .. }) = self.field_mut(key).map(|f| &mut f.kind) {
            *rows = new_rows;
        }
        if self.focused_key() == Some(key) {
            if let State::Rows { cursor } = &mut self.state {
                *cursor = (*cursor).min(len);
            }
        }
    }

    /// Puts the row cursor on `row` (when the rows field is open).
    pub fn set_row_cursor(&mut self, row: usize) {
        if let State::Rows { cursor } = &mut self.state {
            *cursor = row;
        }
    }

    pub fn set_error(&mut self, key: &str, error: Option<String>) {
        if let Some(f) = self.field_mut(key) {
            f.error = error;
        }
    }

    #[cfg(test)]
    pub fn error(&self, key: &str) -> Option<&str> {
        self.field(key).and_then(|f| f.error.as_deref())
    }

    pub fn clear_errors(&mut self) {
        for f in &mut self.fields {
            f.error = None;
        }
    }

    pub fn set_visible(&mut self, key: &str, visible: bool) {
        if let Some(f) = self.field_mut(key) {
            f.visible = visible;
        }
    }

    pub fn focus(&mut self, key: &str) {
        if let Some(i) = self.index(key) {
            self.focus = i;
            self.state = State::Nav;
            self.end_completion();
        }
    }

    /// Puts `value` in a text or path field (a folder chosen in the
    /// browser), leaves any edit and validates the field as an edit would.
    pub fn set_value(&mut self, key: &str, value: &str) -> FormEvent {
        let Some(i) = self.index(key) else {
            return FormEvent::None;
        };
        self.focus = i;
        self.state = State::Nav;
        self.end_completion();
        let f = &mut self.fields[i];
        let before = f.value().map(str::to_string);
        if let Some(input) = f.input_mut() {
            *input = Input::new(value.to_string()).with_cursor(value.chars().count());
        }
        f.run_validator();
        if before.as_deref() != Some(value) {
            self.dirty = true;
            FormEvent::Changed(f.key)
        } else {
            FormEvent::None
        }
    }

    /// The folder a [`FormEvent::List`] asked to have listed, relative to
    /// the drive root. Taken once.
    pub fn take_list_request(&mut self) -> Option<String> {
        self.request.take()
    }

    /// The names of the folders in `folder`, for a OneDrive folder field,
    /// or why they could not be listed. The names are kept for the rest of
    /// the form (an error is not, so the next Tab tries again). When Tab
    /// was waiting for them and the text is still as it was, the
    /// completion happens now.
    pub fn set_listing(&mut self, folder: &str, result: Result<Vec<String>, String>) {
        let waited = match &self.waiting {
            Some((i, f, text, forward)) if f == folder => Some((*i, text.clone(), *forward)),
            _ => None,
        };
        if waited.is_some() {
            self.waiting = None;
        }
        match result {
            Ok(names) => {
                self.listings.insert(folder.to_string(), names);
            }
            Err(e) => {
                if waited.is_some() {
                    self.note = Some((format!("could not list: {e}"), Tone::Error));
                }
                return;
            }
        }
        let Some((i, text, forward)) = waited else {
            return;
        };
        let still = i == self.focus
            && matches!(self.state, State::EditText { .. })
            && self.fields[i].value() == Some(text.as_str());
        if still {
            self.complete(forward);
        } else if self
            .note
            .as_ref()
            .is_some_and(|(n, _)| n == complete::LISTING)
        {
            self.note = None;
        }
    }

    /// Forgets the OneDrive names listed so far (the account changed).
    pub fn clear_listings(&mut self) {
        self.listings.clear();
        self.waiting = None;
        self.request = None;
    }

    fn end_completion(&mut self) {
        self.cycle = None;
        self.note = None;
    }

    /// Tab (`forward`) or Shift-Tab in the folder field being edited.
    fn complete(&mut self, forward: bool) -> FormEvent {
        let i = self.focus;
        let Some(folder) = self.fields.get(i).and_then(|f| f.folder) else {
            return FormEvent::None;
        };
        if let Some(cycle) = &mut self.cycle {
            cycle.step(forward);
            let text = cycle.text();
            self.put_text(&text);
            return FormEvent::None;
        }
        let value = self.fields[i].value().unwrap_or_default().to_string();
        let split = complete::split(&value, folder == Folder::Local);
        let names = match folder {
            Folder::Local => complete::local_parent(&split.base)
                .and_then(|dir| complete::local_folders(&dir).ok())
                .unwrap_or_default(),
            Folder::Remote => {
                let parent = complete::remote_parent(&split.base);
                match self.listings.get(&parent) {
                    Some(names) => names.clone(),
                    // Still listing this folder: Tab does nothing until the
                    // names come. A different folder replaces the wait (the
                    // older answer is then kept for later but not used now).
                    None if self
                        .waiting
                        .as_ref()
                        .is_some_and(|(_, f, _, _)| *f == parent) =>
                    {
                        return FormEvent::None
                    }
                    None => {
                        self.waiting = Some((i, parent.clone(), value, forward));
                        self.request = Some(parent);
                        self.note = Some((complete::LISTING.to_string(), Tone::Dim));
                        return FormEvent::List(self.fields[i].key);
                    }
                }
            }
        };
        match Cycle::start(&split, &names, !forward) {
            Some(cycle) => {
                let text = cycle.text();
                self.cycle = Some(cycle);
                self.note = None;
                self.put_text(&text);
            }
            None => self.note = Some((complete::NO_MATCH.to_string(), Tone::Dim)),
        }
        FormEvent::None
    }

    /// Replaces the text of the field being edited, the cursor at its end.
    fn put_text(&mut self, text: &str) {
        if let Some(input) = self.fields[self.focus].input_mut() {
            *input = Input::new(text.to_string()).with_cursor(text.chars().count());
            self.dirty = true;
        }
    }

    fn focused_folder(&self) -> bool {
        self.fields
            .get(self.focus)
            .is_some_and(|f| f.folder.is_some())
    }

    pub fn focused_key(&self) -> Option<&'static str> {
        self.fields.get(self.focus).map(|f| f.key)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    #[cfg(test)]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// True while a text, list item or picker is being edited.
    #[cfg(test)]
    pub fn is_editing(&self) -> bool {
        !matches!(self.state, State::Nav)
    }

    fn move_focus(&mut self, step: isize) {
        let n = self.fields.len() as isize;
        if n == 0 {
            return;
        }
        let mut i = self.focus as isize;
        for _ in 0..n {
            i += step;
            if i < 0 || i >= n {
                return; // stay at the edge
            }
            if self.fields[i as usize].visible {
                self.focus = i as usize;
                return;
            }
        }
    }

    fn focus_edge(&mut self, last: bool) {
        let found = if last {
            self.fields.iter().rposition(|f| f.visible)
        } else {
            self.fields.iter().position(|f| f.visible)
        };
        if let Some(i) = found {
            self.focus = i;
        }
    }

    /// Runs every visible field's validator. On failure focuses the first
    /// field with an error and says so.
    pub fn validate(&mut self) -> bool {
        let mut first = None;
        for (i, f) in self.fields.iter_mut().enumerate() {
            if f.visible && !f.run_validator() && first.is_none() {
                first = Some(i);
            }
        }
        if let Some(i) = first {
            self.focus = i;
            self.message = Some(("Fix the marked fields first.".into(), Tone::Error));
            return false;
        }
        true
    }

    fn submit(&mut self) -> FormEvent {
        if self.validate() {
            FormEvent::Submit
        } else {
            FormEvent::None
        }
    }

    /// Ends a text edit, keeping the value and validating it. Says
    /// `Changed` when the value differs from before the edit.
    fn commit_text(&mut self) -> FormEvent {
        self.end_completion();
        let backup = match std::mem::replace(&mut self.state, State::Nav) {
            State::EditText { backup } => Some(backup),
            _ => None,
        };
        let f = &mut self.fields[self.focus];
        f.run_validator();
        if backup.as_deref() != f.value() {
            FormEvent::Changed(f.key)
        } else {
            FormEvent::None
        }
    }

    /// Leaves any edit, putting back the value it started from.
    fn undo_edit(&mut self) {
        self.end_completion();
        match std::mem::replace(&mut self.state, State::Nav) {
            State::EditText { backup } => {
                if let Some(input) = self.fields[self.focus].input_mut() {
                    *input = Input::new(backup);
                }
            }
            State::Picking { backup } => {
                if let Kind::Picker { selected, .. } = &mut self.fields[self.focus].kind {
                    *selected = backup;
                }
            }
            State::Nav | State::List { .. } | State::ListEdit { .. } | State::Rows { .. } => {}
        }
    }

    /// Applies one key and says what it meant.
    pub fn handle_key(&mut self, key: KeyEvent) -> FormEvent {
        if key.kind == KeyEventKind::Release {
            return FormEvent::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            // Like Esc from the field list: undo any edit in progress, then
            // cancel, asking first when something changed.
            self.undo_edit();
            return self.key_nav(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        if ctrl && key.code == KeyCode::Char('s') {
            if let State::EditText { .. } = self.state {
                self.commit_text();
            }
            if let State::ListEdit { .. } = self.state {
                self.commit_list_item();
            }
            self.state = State::Nav;
            return self.submit();
        }
        if key.code != KeyCode::Esc && self.confirm_discard {
            self.confirm_discard = false;
            if self.message.as_ref().is_some_and(|(m, _)| m == DISCARD) {
                self.message = None;
            }
        }
        let completing = matches!(self.state, State::EditText { .. })
            && self.focused_folder()
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab);
        if !completing {
            // Any other key ends the cycle and edits from the text as it is.
            self.end_completion();
        }
        let event = match self.state.clone() {
            State::Nav => self.key_nav(key),
            State::EditText { backup } => self.key_edit(key, backup),
            State::Picking { backup } => self.key_picking(key, backup),
            State::List { cursor } => self.key_list(key, cursor),
            State::ListEdit { cursor, input, new } => self.key_list_edit(key, cursor, input, new),
            State::Rows { cursor } => self.key_rows(key, cursor),
        };
        if !matches!(self.state, State::EditText { .. }) {
            self.end_completion();
        }
        event
    }

    fn key_nav(&mut self, key: KeyEvent) -> FormEvent {
        let Some(field) = self.fields.get_mut(self.focus) else {
            return match key.code {
                KeyCode::Esc => FormEvent::Cancel,
                _ => FormEvent::None,
            };
        };
        let fkey = field.key;
        match key.code {
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => self.move_focus(1),
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => self.move_focus(-1),
            KeyCode::Home | KeyCode::Char('g') => self.focus_edge(false),
            KeyCode::End | KeyCode::Char('G') => self.focus_edge(true),
            KeyCode::Esc => {
                if self.dirty && !self.confirm_discard {
                    self.confirm_discard = true;
                    self.message = Some((DISCARD.into(), Tone::Warning));
                } else {
                    return FormEvent::Cancel;
                }
            }
            KeyCode::Enter => match &mut field.kind {
                Kind::Text { input, .. } | Kind::Path { input, .. } => {
                    let backup = input.value().to_string();
                    let end = input.value().chars().count();
                    *input = input.clone().with_cursor(end);
                    self.state = State::EditText { backup };
                }
                Kind::Picker { selected, .. } => {
                    self.state = State::Picking { backup: *selected };
                }
                Kind::Toggle { on } => {
                    *on = !*on;
                    self.dirty = true;
                    return FormEvent::Changed(fkey);
                }
                Kind::List { .. } => self.state = State::List { cursor: 0 },
                Kind::Rows { .. } => self.state = State::Rows { cursor: 0 },
                Kind::Button { action, .. } => {
                    return match action {
                        ButtonAction::Submit => self.submit(),
                        ButtonAction::Cancel => FormEvent::Cancel,
                        ButtonAction::Press => FormEvent::Pressed(fkey),
                    };
                }
            },
            KeyCode::Left | KeyCode::Char('h') => return self.step_choice(-1),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => return self.step_choice(1),
            KeyCode::Char('b') if field.folder.is_some() => return FormEvent::Browse(fkey),
            KeyCode::Char('a') => match &field.kind {
                Kind::Rows { .. } => return FormEvent::Row(fkey, RowAction::Add),
                Kind::List { items, .. } => {
                    let cursor = items.len();
                    self.state = State::ListEdit {
                        cursor,
                        input: Input::default(),
                        new: true,
                    };
                }
                _ => {}
            },
            _ => {}
        }
        FormEvent::None
    }

    /// Moves a picker or flips a toggle, in the field list or while picking.
    fn step_choice(&mut self, step: isize) -> FormEvent {
        let field = &mut self.fields[self.focus];
        match &mut field.kind {
            Kind::Picker { options, selected } if !options.is_empty() => {
                let n = options.len() as isize;
                let next = (*selected as isize + step).rem_euclid(n) as usize;
                if next != *selected {
                    *selected = next;
                    self.dirty = true;
                    return FormEvent::Changed(field.key);
                }
            }
            Kind::Toggle { on } => {
                *on = !*on;
                self.dirty = true;
                return FormEvent::Changed(field.key);
            }
            _ => {}
        }
        FormEvent::None
    }

    fn key_edit(&mut self, key: KeyEvent, backup: String) -> FormEvent {
        let folder = self.focused_folder();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Tab if folder => self.complete(true),
            KeyCode::BackTab if folder => self.complete(false),
            KeyCode::Char('b') if folder && ctrl => {
                self.commit_text();
                FormEvent::Browse(self.fields[self.focus].key)
            }
            KeyCode::Enter => self.commit_text(),
            KeyCode::Esc => {
                if let Some(input) = self.fields[self.focus].input_mut() {
                    *input = Input::new(backup);
                }
                self.state = State::Nav;
                FormEvent::None
            }
            KeyCode::Tab | KeyCode::Down => {
                let event = self.commit_text();
                self.move_focus(1);
                event
            }
            KeyCode::BackTab | KeyCode::Up => {
                let event = self.commit_text();
                self.move_focus(-1);
                event
            }
            _ => {
                if let Some(request) = to_input_request(&Event::Key(key)) {
                    if let Some(input) = self.fields[self.focus].input_mut() {
                        if input.handle(request).is_some_and(|c| c.value) {
                            self.dirty = true;
                        }
                    }
                }
                FormEvent::None
            }
        }
    }

    fn key_picking(&mut self, key: KeyEvent, backup: usize) -> FormEvent {
        match key.code {
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
                self.step_choice(-1)
            }
            KeyCode::Right
            | KeyCode::Down
            | KeyCode::Char('l')
            | KeyCode::Char('j')
            | KeyCode::Char(' ') => self.step_choice(1),
            KeyCode::Enter => {
                self.state = State::Nav;
                FormEvent::None
            }
            KeyCode::Tab => {
                self.state = State::Nav;
                self.move_focus(1);
                FormEvent::None
            }
            KeyCode::Esc => {
                self.state = State::Nav;
                let field = &mut self.fields[self.focus];
                if let Kind::Picker { selected, .. } = &mut field.kind {
                    if *selected != backup {
                        *selected = backup;
                        return FormEvent::Changed(field.key);
                    }
                }
                FormEvent::None
            }
            _ => FormEvent::None,
        }
    }

    fn list_items_mut(&mut self) -> Option<&mut Vec<String>> {
        match &mut self.fields[self.focus].kind {
            Kind::List { items, .. } => Some(items),
            _ => None,
        }
    }

    fn key_list(&mut self, key: KeyEvent, cursor: usize) -> FormEvent {
        let fkey = self.fields[self.focus].key;
        let Some(items) = self.list_items_mut() else {
            self.state = State::Nav;
            return FormEvent::None;
        };
        let len = items.len();
        match key.code {
            KeyCode::Char('k') | KeyCode::Char('K') => return self.move_list_item(cursor, -1),
            KeyCode::Char('j') | KeyCode::Char('J') => return self.move_list_item(cursor, 1),
            KeyCode::Up => {
                self.state = State::List {
                    cursor: cursor.saturating_sub(1),
                }
            }
            KeyCode::Down => {
                self.state = State::List {
                    cursor: (cursor + 1).min(len),
                }
            }
            KeyCode::Enter => {
                let value = items.get(cursor).cloned();
                let new = value.is_none();
                let value = value.unwrap_or_default();
                let end = value.chars().count();
                self.state = State::ListEdit {
                    cursor,
                    input: Input::new(value).with_cursor(end),
                    new,
                };
            }
            KeyCode::Char('a') => {
                self.state = State::ListEdit {
                    cursor: len,
                    input: Input::default(),
                    new: true,
                }
            }
            KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace
                if cursor < len =>
            {
                items.remove(cursor);
                self.dirty = true;
                self.state = State::List {
                    cursor: cursor.min(len - 1),
                };
                return FormEvent::Changed(fkey);
            }
            KeyCode::Esc => self.state = State::Nav,
            KeyCode::Tab => {
                self.state = State::Nav;
                self.move_focus(1);
            }
            KeyCode::BackTab => {
                self.state = State::Nav;
                self.move_focus(-1);
            }
            _ => {}
        }
        FormEvent::None
    }

    fn move_list_item(&mut self, cursor: usize, step: isize) -> FormEvent {
        let fkey = self.fields[self.focus].key;
        let Some(items) = self.list_items_mut() else {
            return FormEvent::None;
        };
        let target = cursor as isize + step;
        if cursor >= items.len() || target < 0 || target as usize >= items.len() {
            return FormEvent::None;
        }
        items.swap(cursor, target as usize);
        self.dirty = true;
        self.state = State::List {
            cursor: target as usize,
        };
        FormEvent::Changed(fkey)
    }

    /// Ends a list item edit, keeping the text. An emptied item is removed.
    fn commit_list_item(&mut self) -> FormEvent {
        let State::ListEdit { cursor, input, new } = self.state.clone() else {
            return FormEvent::None;
        };
        let fkey = self.fields[self.focus].key;
        let value = input.value().trim().to_string();
        let Some(items) = self.list_items_mut() else {
            self.state = State::Nav;
            return FormEvent::None;
        };
        let mut cursor = cursor.min(items.len());
        let changed = match (new, value.is_empty()) {
            (true, true) => false,
            (true, false) => {
                items.insert(cursor, value);
                cursor += 1; // ready to add the next one
                true
            }
            (false, true) => {
                items.remove(cursor);
                true
            }
            (false, false) => {
                let changed = items[cursor] != value;
                items[cursor] = value;
                changed
            }
        };
        self.state = State::List { cursor };
        if changed {
            self.dirty = true;
            FormEvent::Changed(fkey)
        } else {
            FormEvent::None
        }
    }

    fn key_list_edit(
        &mut self,
        key: KeyEvent,
        cursor: usize,
        mut input: Input,
        new: bool,
    ) -> FormEvent {
        match key.code {
            KeyCode::Enter => self.commit_list_item(),
            KeyCode::Esc => {
                self.state = State::List { cursor };
                FormEvent::None
            }
            _ => {
                if let Some(request) = to_input_request(&Event::Key(key)) {
                    input.handle(request);
                }
                self.state = State::ListEdit { cursor, input, new };
                FormEvent::None
            }
        }
    }

    fn key_rows(&mut self, key: KeyEvent, cursor: usize) -> FormEvent {
        let fkey = self.fields[self.focus].key;
        let len = match &self.fields[self.focus].kind {
            Kind::Rows { rows, .. } => rows.len(),
            _ => {
                self.state = State::Nav;
                return FormEvent::None;
            }
        };
        let row = |action| FormEvent::Row(fkey, action);
        match key.code {
            KeyCode::Char('k') | KeyCode::Char('K') if cursor > 0 && cursor < len => {
                row(RowAction::MoveUp(cursor))
            }
            KeyCode::Char('j') | KeyCode::Char('J') if cursor + 1 < len => {
                row(RowAction::MoveDown(cursor))
            }
            // At either end (or on the add row) j and k do nothing.
            KeyCode::Char('j' | 'k' | 'J' | 'K') => FormEvent::None,
            KeyCode::Up => {
                self.state = State::Rows {
                    cursor: cursor.saturating_sub(1),
                };
                FormEvent::None
            }
            KeyCode::Down => {
                self.state = State::Rows {
                    cursor: (cursor + 1).min(len),
                };
                FormEvent::None
            }
            KeyCode::Enter if cursor < len => row(RowAction::Edit(cursor)),
            KeyCode::Enter | KeyCode::Char('a') => row(RowAction::Add),
            KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace
                if cursor < len =>
            {
                row(RowAction::Remove(cursor))
            }
            KeyCode::Esc => {
                self.state = State::Nav;
                FormEvent::None
            }
            KeyCode::Tab => {
                self.state = State::Nav;
                self.move_focus(1);
                FormEvent::None
            }
            KeyCode::BackTab => {
                self.state = State::Nav;
                self.move_focus(-1);
                FormEvent::None
            }
            _ => FormEvent::None,
        }
    }

    /// The key hints for the current state.
    pub fn hints(&self) -> String {
        let d = theme::DOT;
        match self.state {
            State::Nav => {
                let picker = matches!(
                    self.fields.get(self.focus).map(|f| &f.kind),
                    Some(Kind::Picker { .. } | Kind::Toggle { .. })
                );
                if self.focused_folder() {
                    format!(
                        "tab {}{} move {d} enter edit {d} b browse {d} ctrl-s save {d} esc cancel",
                        theme::UP,
                        theme::DOWN
                    )
                } else if picker {
                    format!(
                        "tab {}{} move {d} {}{} choose {d} ctrl-s save {d} esc cancel",
                        theme::UP,
                        theme::DOWN,
                        '←',
                        '→'
                    )
                } else {
                    format!(
                        "tab {}{} move {d} enter edit {d} ctrl-s save {d} esc cancel",
                        theme::UP,
                        theme::DOWN
                    )
                }
            }
            State::EditText { .. } if self.focused_folder() => {
                format!("tab complete {d} ctrl-b browse {d} enter done {d} esc undo")
            }
            State::EditText { .. } => format!("enter done {d} esc undo {d} tab next"),
            State::Picking { .. } => format!("←→ choose {d} enter done {d} esc undo"),
            State::List { .. } => format!(
                "{}{} move {d} enter edit {d} a add {d} d delete {d} j/k reorder {d} esc done",
                theme::UP,
                theme::DOWN
            ),
            State::ListEdit { .. } => format!("enter done {d} esc undo"),
            State::Rows { .. } => format!(
                "{}{} move {d} enter edit {d} a add {d} d remove {d} j/k reorder {d} esc done",
                theme::UP,
                theme::DOWN
            ),
        }
    }

    /// The form's lines for a content area `width` wide, the line the focus
    /// is on, and the cursor position (column, line) while editing text.
    /// The label column: [`LABEL`] wide, or wider when a label needs it
    /// (the cursor and a space before it, two spaces after).
    fn label_width(&self) -> usize {
        self.fields
            .iter()
            .map(|f| f.label.chars().count() + 4)
            .max()
            .unwrap_or(0)
            .max(LABEL)
    }

    pub fn lines(&self, width: u16) -> (Vec<Line<'static>>, usize, Option<(u16, u16)>) {
        let label_w = self.label_width();
        let width = width as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut title = vec![Span::styled(self.title.clone(), theme::style(Tone::Bold))];
        if let Some(sub) = &self.subtitle {
            title.push(Span::raw("  "));
            title.push(Span::styled(sub.clone(), theme::style(Tone::Dim)));
        }
        lines.push(Line::from(title));
        lines.push(Line::default());

        let mut focus_line = 0;
        let mut cursor = None;
        let value_width = width.saturating_sub(label_w + 1).max(4);
        let pad = " ".repeat(label_w);

        for (i, f) in self.fields.iter().enumerate() {
            if !f.visible {
                continue;
            }
            let focused = i == self.focus;
            if focused {
                focus_line = lines.len();
            }
            let mut head: Vec<Span<'static>> = Vec::new();
            if focused {
                head.push(Span::styled(theme::CURSOR, theme::style(Tone::Accent)));
                head.push(Span::raw(" "));
            } else {
                head.push(Span::raw("  "));
            }

            if let Kind::Button { label, .. } = &f.kind {
                let tone = if focused { Tone::Accent } else { Tone::Plain };
                head.push(Span::styled(label.clone(), theme::style(tone)));
                lines.push(Line::from(head));
                continue;
            }

            let label = format!("{:<w$}", f.label, w = label_w - 2);
            let label_tone = if focused { Tone::Accent } else { Tone::Plain };
            head.push(Span::styled(label, theme::style(label_tone)));

            match &f.kind {
                Kind::Text { input, placeholder }
                | Kind::Path {
                    input, placeholder, ..
                } => {
                    let editing = focused && matches!(self.state, State::EditText { .. });
                    if editing {
                        let scroll = input.visual_scroll(value_width);
                        let shown: String = input.value().chars().skip(scroll).collect();
                        head.push(Span::styled(shown, theme::style(Tone::Plain)));
                        cursor = Some((
                            (label_w + input.visual_cursor().saturating_sub(scroll)) as u16,
                            lines.len() as u16,
                        ));
                    } else if input.value().is_empty() {
                        head.push(Span::styled(placeholder.clone(), theme::style(Tone::Dim)));
                    } else {
                        head.push(Span::raw(input.value().to_string()));
                    }
                    // No existence hint beside an error: the error says more.
                    if let (Kind::Path { missing_ok, .. }, None) = (&f.kind, &f.error) {
                        if let Some((hint, tone)) = path_hint(input.value(), *missing_ok) {
                            head.push(Span::raw("  "));
                            head.push(Span::styled(hint, theme::style(tone)));
                        }
                    }
                    lines.push(Line::from(head));
                }
                Kind::Picker { options, selected } => {
                    let picking = focused && matches!(self.state, State::Picking { .. });
                    for (n, option) in options.iter().enumerate() {
                        if n > 0 {
                            head.push(Span::raw("  "));
                        }
                        let (glyph, tone) = if n == *selected {
                            (
                                theme::SELECTED,
                                if picking { Tone::Accent } else { Tone::Bold },
                            )
                        } else {
                            (theme::UNSELECTED, Tone::Plain)
                        };
                        head.push(Span::styled(
                            format!("{glyph} {option}"),
                            theme::style(tone),
                        ));
                    }
                    lines.push(Line::from(head));
                }
                Kind::Toggle { on } => {
                    let (glyph, word) = if *on {
                        (theme::SELECTED, "on")
                    } else {
                        (theme::UNSELECTED, "off")
                    };
                    head.push(Span::raw(format!("{glyph} {word}")));
                    lines.push(Line::from(head));
                }
                Kind::List {
                    items,
                    placeholder,
                    add_label,
                } => {
                    let inner = match &self.state {
                        State::List { cursor } if focused => Some((*cursor, None)),
                        State::ListEdit { cursor, input, .. } if focused => {
                            Some((*cursor, Some(input)))
                        }
                        _ => None,
                    };
                    match inner {
                        None => {
                            if items.is_empty() {
                                head.push(Span::styled(
                                    placeholder.clone(),
                                    theme::style(Tone::Dim),
                                ));
                            } else {
                                head.push(Span::raw(items.join(", ")));
                            }
                            lines.push(Line::from(head));
                        }
                        Some((at, editing)) => {
                            lines.push(Line::from(head));
                            let mut rows: Vec<Option<&String>> = items.iter().map(Some).collect();
                            // A new item is edited in place before the add slot.
                            let new_at = match (&self.state, editing) {
                                (State::ListEdit { new: true, .. }, Some(_)) => Some(at),
                                _ => None,
                            };
                            if let Some(n) = new_at {
                                rows.insert(n.min(rows.len()), None);
                            }
                            for (n, item) in rows.iter().enumerate() {
                                let here = n == at;
                                let mut spans = vec![Span::raw(pad[..label_w - 2].to_string())];
                                spans.push(cursor_glyph(here));
                                match (here, editing) {
                                    (true, Some(input)) => {
                                        let scroll = input.visual_scroll(value_width);
                                        let shown: String =
                                            input.value().chars().skip(scroll).collect();
                                        spans.push(Span::raw(shown));
                                        cursor = Some((
                                            (label_w + input.visual_cursor().saturating_sub(scroll))
                                                as u16,
                                            lines.len() as u16,
                                        ));
                                    }
                                    _ => spans.push(Span::raw(
                                        item.map(|s| s.to_string()).unwrap_or_default(),
                                    )),
                                }
                                if here {
                                    focus_line = lines.len();
                                }
                                lines.push(Line::from(spans));
                            }
                            if new_at.is_none() {
                                let here = at >= items.len();
                                if here {
                                    focus_line = lines.len();
                                }
                                lines.push(Line::from(vec![
                                    Span::raw(pad[..label_w - 2].to_string()),
                                    cursor_glyph(here),
                                    Span::styled(
                                        format!("+ {add_label}"),
                                        theme::style(if here { Tone::Accent } else { Tone::Dim }),
                                    ),
                                ]));
                            }
                        }
                    }
                }
                Kind::Rows { rows, add_label } => {
                    let inner = match self.state {
                        State::Rows { cursor } if focused => Some(cursor),
                        _ => None,
                    };
                    if rows.is_empty() {
                        head.push(Span::styled("none yet", theme::style(Tone::Dim)));
                    }
                    lines.push(Line::from(head));
                    for (n, row) in rows.iter().enumerate() {
                        let here = inner == Some(n);
                        if here {
                            focus_line = lines.len();
                        }
                        let mut spans = vec![Span::raw("    "), cursor_glyph(here)];
                        spans.extend(
                            row.iter().map(|(text, tone)| {
                                Span::styled(text.clone(), theme::style(*tone))
                            }),
                        );
                        lines.push(Line::from(spans));
                    }
                    let here = inner == Some(rows.len());
                    if here {
                        focus_line = lines.len();
                    }
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        cursor_glyph(here),
                        Span::styled(
                            format!("+ {add_label}"),
                            theme::style(if here || focused {
                                Tone::Accent
                            } else {
                                Tone::Dim
                            }),
                        ),
                    ]));
                }
                Kind::Button { .. } => {}
            }

            let completion = match (&self.cycle, &self.note) {
                _ if !focused => None,
                (Some(cycle), _) => Some((cycle.hint(value_width), Tone::Dim)),
                (None, Some(note)) => Some(note.clone()),
                (None, None) => None,
            };
            if let Some((text, tone)) = completion {
                lines.push(Line::from(vec![
                    Span::raw(pad.clone()),
                    Span::styled(crate::ui::fit_text(&text, value_width), theme::style(tone)),
                ]));
            } else if let Some(error) = &f.error {
                lines.push(Line::from(vec![
                    Span::raw(pad.clone()),
                    Span::styled(
                        format!("{} {error}", theme::FAILED),
                        theme::style(Tone::Error),
                    ),
                ]));
            } else if let (true, Some(help)) = (focused, &f.help) {
                lines.push(Line::from(vec![
                    Span::raw(pad.clone()),
                    Span::styled(help.clone(), theme::style(Tone::Dim)),
                ]));
            }
        }

        if let Some((message, tone)) = &self.message {
            lines.push(Line::default());
            for text in message.lines() {
                lines.push(Line::from(Span::styled(
                    text.to_string(),
                    theme::style(*tone),
                )));
            }
        }
        (lines, focus_line, cursor)
    }

    /// Draws the form into `area`, with the key hints on its last line.
    pub fn render_in(&self, frame: &mut Frame, area: Rect) {
        let area = area.inner(Margin {
            horizontal: 2,
            vertical: 1,
        });
        if area.height < 3 || area.width < 10 {
            return;
        }
        let body_height = area.height - 2;
        let (lines, focus_line, cursor) = self.lines(area.width);
        let scroll = if focus_line + 2 > body_height as usize {
            (focus_line + 3).saturating_sub(body_height as usize) as u16
        } else {
            0
        };
        let body = Rect {
            height: body_height,
            ..area
        };
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
        let footer = Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                self.hints(),
                theme::style(Tone::Dim),
            ))),
            footer,
        );
        if let Some((x, y)) = cursor {
            if y >= scroll && y - scroll < body_height {
                frame.set_cursor_position((area.x + x.min(area.width - 1), area.y + y - scroll));
            }
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        self.render_in(frame, frame.area());
    }
}

fn cursor_glyph(here: bool) -> Span<'static> {
    if here {
        Span::styled(format!("{} ", theme::CURSOR), theme::style(Tone::Accent))
    } else {
        Span::raw("  ")
    }
}

/// The hint beside a path field: whether the folder is there.
pub fn path_hint(value: &str, missing_ok: bool) -> Option<(String, Tone)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let expanded = config::expand_tilde(value);
    let path = Path::new(&expanded);
    if !path.is_absolute() {
        return Some((format!("{} not a full path", theme::FAILED), Tone::Error));
    }
    Some(match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => (format!("{} folder exists", theme::DONE), Tone::Success),
        Ok(_) => (format!("{} not a folder", theme::FAILED), Tone::Error),
        Err(_) if missing_ok => (format!("{} will be created", theme::SKIPPED), Tone::Warning),
        Err(_) => (format!("{} does not exist", theme::FAILED), Tone::Error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(form: &mut Form, text: &str) {
        for c in text.chars() {
            form.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn not_empty(v: &str) -> Result<(), String> {
        if v.trim().is_empty() {
            Err("needs a value".into())
        } else {
            Ok(())
        }
    }

    fn sample() -> Form {
        let mut form = Form::new(
            "Add job",
            vec![
                Field::text("name", "Name", "documents").with_validator(not_empty),
                Field::path("source", "Source", "/", false)
                    .with_help("The folder to sync, such as ~/Documents"),
                Field::list(
                    "excludes",
                    "Excludes",
                    vec![".DS_Store".into(), "node_modules/**".into()],
                    "add a pattern",
                ),
                Field::rows(
                    "dests",
                    "Destinations",
                    vec![
                        vec![
                            (
                                format!("{} work:Backups/Documents", theme::ACTION),
                                Tone::Plain,
                            ),
                            ("   two-way".into(), Tone::Dim),
                        ],
                        vec![
                            (
                                format!("{} /nonexistent/Backup", theme::ACTION),
                                Tone::Plain,
                            ),
                            ("   one-way".into(), Tone::Dim),
                        ],
                    ],
                    "add a destination",
                ),
                Field::picker("mode", "Mode", vec!["one-way".into(), "two-way".into()], 0),
                Field::toggle("delta", "Delta", false),
                Field::button("save", "Save job", ButtonAction::Submit),
                Field::button("cancel", "Cancel", ButtonAction::Cancel),
            ],
        );
        form.subtitle = Some("new".into());
        form
    }

    #[test]
    fn tab_arrows_and_vim_keys_move() {
        let mut f = sample();
        assert_eq!(f.focused_key(), Some("name"));
        f.handle_key(key(KeyCode::Tab));
        assert_eq!(f.focused_key(), Some("source"));
        f.handle_key(key(KeyCode::Down));
        f.handle_key(key(KeyCode::Char('j')));
        assert_eq!(f.focused_key(), Some("dests"));
        f.handle_key(key(KeyCode::BackTab));
        assert_eq!(f.focused_key(), Some("excludes"));
        f.handle_key(key(KeyCode::Up));
        f.handle_key(key(KeyCode::Char('k')));
        f.handle_key(key(KeyCode::Char('k')));
        assert_eq!(f.focused_key(), Some("name"), "stops at the top");
        f.handle_key(key(KeyCode::Char('G')));
        assert_eq!(f.focused_key(), Some("cancel"));
        f.handle_key(key(KeyCode::Down));
        assert_eq!(f.focused_key(), Some("cancel"), "stops at the bottom");
    }

    #[test]
    fn hidden_fields_are_skipped() {
        let mut f = sample();
        f.set_visible("source", false);
        f.handle_key(key(KeyCode::Tab));
        assert_eq!(f.focused_key(), Some("excludes"));
    }

    #[test]
    fn enter_edits_text_and_esc_undoes() {
        let mut f = sample();
        f.handle_key(key(KeyCode::Enter));
        assert!(f.is_editing());
        typed(&mut f, "-x");
        // j and k are letters while editing, not moves.
        typed(&mut f, "jk");
        assert_eq!(f.text("name"), "documents-xjk");
        f.handle_key(key(KeyCode::Esc));
        assert!(!f.is_editing());
        assert_eq!(f.text("name"), "documents");

        f.handle_key(key(KeyCode::Enter));
        f.handle_key(key(KeyCode::Backspace));
        assert_eq!(
            f.handle_key(key(KeyCode::Enter)),
            FormEvent::Changed("name")
        );
        assert_eq!(f.text("name"), "document");
        assert!(f.is_dirty());
    }

    #[test]
    fn inline_validation_marks_the_field() {
        let mut f = sample();
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.error("name"), Some("needs a value"));
        // Submitting refuses and focuses the field.
        f.handle_key(key(KeyCode::Char('G')));
        f.handle_key(key(KeyCode::Up));
        assert_eq!(f.handle_key(key(KeyCode::Enter)), FormEvent::None);
        assert_eq!(f.focused_key(), Some("name"));
        assert!(f.message.is_some());
        f.handle_key(key(KeyCode::Enter));
        typed(&mut f, "ok");
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.error("name"), None);
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(f.handle_key(ctrl_s), FormEvent::Submit);
    }

    #[test]
    fn esc_asks_twice_when_dirty() {
        let mut f = sample();
        assert_eq!(f.handle_key(key(KeyCode::Esc)), FormEvent::Cancel);
        f.mark_dirty();
        assert_eq!(f.handle_key(key(KeyCode::Esc)), FormEvent::None);
        assert_eq!(f.message.as_ref().map(|m| m.0.as_str()), Some(DISCARD));
        assert_eq!(f.handle_key(key(KeyCode::Esc)), FormEvent::Cancel);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        // Ctrl-C is Esc: it asks first when something changed.
        let mut f = sample();
        assert_eq!(f.handle_key(ctrl_c), FormEvent::Cancel, "nothing changed");
        f.mark_dirty();
        assert_eq!(f.handle_key(ctrl_c), FormEvent::None);
        assert_eq!(f.message.as_ref().map(|m| m.0.as_str()), Some(DISCARD));
        assert_eq!(f.handle_key(ctrl_c), FormEvent::Cancel);
        // From inside an edit it undoes the edit, then asks.
        let mut f = sample();
        f.handle_key(key(KeyCode::Enter));
        typed(&mut f, "xyz");
        assert_eq!(f.handle_key(ctrl_c), FormEvent::None);
        assert!(!f.is_editing());
        assert_eq!(f.text("name"), "documents");
        assert_eq!(
            f.handle_key(ctrl_c),
            FormEvent::Cancel,
            "the second one cancels"
        );
    }

    #[test]
    fn press_buttons_report_to_the_owner() {
        let mut f = Form::new(
            "t",
            vec![Field::button("again", "Do it again", ButtonAction::Press)],
        );
        assert_eq!(
            f.handle_key(key(KeyCode::Enter)),
            FormEvent::Pressed("again")
        );
    }

    #[test]
    fn no_path_hint_beside_an_error() {
        let mut f = Form::new(
            "t",
            vec![Field::path("p", "Folder", "/nonexistent/x", true)],
        );
        let (lines, _, _) = f.lines(100);
        let text: String = lines.iter().map(|l| l.to_string()).collect();
        assert!(text.contains("will be created"), "{text}");
        f.set_error("p", Some("Drive not mounted".into()));
        let (lines, _, _) = f.lines(100);
        let text: String = lines.iter().map(|l| l.to_string()).collect();
        assert!(!text.contains("will be created"), "{text}");
        assert!(text.contains("✗ Drive not mounted"), "{text}");
    }

    #[test]
    fn pickers_and_toggles() {
        let mut f = sample();
        f.focus("mode");
        assert_eq!(
            f.handle_key(key(KeyCode::Right)),
            FormEvent::Changed("mode")
        );
        assert_eq!(f.picked("mode"), 1);
        f.handle_key(key(KeyCode::Char('l')));
        assert_eq!(f.picked("mode"), 0, "wraps around");
        // Enter opens it; Esc puts the old choice back.
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(key(KeyCode::Down));
        assert_eq!(f.picked("mode"), 1);
        assert_eq!(f.handle_key(key(KeyCode::Esc)), FormEvent::Changed("mode"));
        assert_eq!(f.picked("mode"), 0);
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(key(KeyCode::Char('j')));
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.picked("mode"), 1);

        f.focus("delta");
        assert_eq!(
            f.handle_key(key(KeyCode::Enter)),
            FormEvent::Changed("delta")
        );
        assert!(f.toggled("delta"));
        f.handle_key(key(KeyCode::Char(' ')));
        assert!(!f.toggled("delta"));
    }

    #[test]
    fn list_items_are_added_edited_and_removed() {
        let mut f = sample();
        f.focus("excludes");
        f.handle_key(key(KeyCode::Enter)); // into the list
        f.handle_key(key(KeyCode::Char('a')));
        typed(&mut f, "*.tmp");
        assert_eq!(
            f.handle_key(key(KeyCode::Enter)),
            FormEvent::Changed("excludes")
        );
        assert_eq!(
            f.items("excludes"),
            vec![".DS_Store", "node_modules/**", "*.tmp"]
        );
        // Edit the first: Up to the top, Enter, change it.
        for _ in 0..5 {
            f.handle_key(key(KeyCode::Up));
        }
        f.handle_key(key(KeyCode::Enter));
        typed(&mut f, "x");
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.items("excludes")[0], ".DS_Storex");
        // Reorder and delete.
        f.handle_key(key(KeyCode::Char('j')));
        assert_eq!(f.items("excludes")[1], ".DS_Storex");
        f.handle_key(key(KeyCode::Char('d')));
        assert_eq!(f.items("excludes"), vec!["node_modules/**", "*.tmp"]);
        // Esc leaves the list, and Esc in an item edit undoes.
        f.handle_key(key(KeyCode::Enter));
        typed(&mut f, "zzz");
        f.handle_key(key(KeyCode::Esc));
        assert_eq!(f.items("excludes"), vec!["node_modules/**", "*.tmp"]);
        f.handle_key(key(KeyCode::Esc));
        assert!(!f.is_editing());
        assert_eq!(f.focused_key(), Some("excludes"));
    }

    #[test]
    fn j_and_k_reorder_a_list_and_type_in_its_items() {
        let shift = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT);
        let mut f = sample();
        f.focus("excludes");
        f.handle_key(key(KeyCode::Enter)); // into the list, on the first item
        assert_eq!(f.items("excludes"), vec![".DS_Store", "node_modules/**"]);
        assert_eq!(
            f.handle_key(key(KeyCode::Char('j'))),
            FormEvent::Changed("excludes")
        );
        assert_eq!(f.items("excludes"), vec!["node_modules/**", ".DS_Store"]);
        // The cursor went with it: at the bottom j does nothing.
        assert_eq!(f.handle_key(key(KeyCode::Char('j'))), FormEvent::None);
        assert_eq!(f.handle_key(shift('J')), FormEvent::None);
        assert_eq!(
            f.handle_key(key(KeyCode::Char('k'))),
            FormEvent::Changed("excludes")
        );
        assert_eq!(f.items("excludes"), vec![".DS_Store", "node_modules/**"]);
        assert_eq!(f.handle_key(key(KeyCode::Char('k'))), FormEvent::None);
        // Shift+j arrives as J and does the same.
        assert_eq!(f.handle_key(shift('J')), FormEvent::Changed("excludes"));
        assert_eq!(f.handle_key(shift('K')), FormEvent::Changed("excludes"));
        assert_eq!(f.items("excludes"), vec![".DS_Store", "node_modules/**"]);
        // The arrows, Shift or not, only move the cursor.
        f.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        f.handle_key(key(KeyCode::Up));
        f.handle_key(key(KeyCode::Down));
        assert_eq!(f.items("excludes"), vec![".DS_Store", "node_modules/**"]);
        // On the second item: Enter edits it, and j, k, J, K are letters.
        f.handle_key(key(KeyCode::Enter));
        for c in ['j', 'k', 'J', 'K'] {
            f.handle_key(key(KeyCode::Char(c)));
        }
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(
            f.items("excludes"),
            vec![".DS_Store", "node_modules/**jkJK"]
        );
        // And in a new item.
        f.handle_key(key(KeyCode::Char('a')));
        typed(&mut f, "jk/KJ");
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.items("excludes")[2], "jk/KJ");
        // A text field outside the list keeps them too.
        f.handle_key(key(KeyCode::Esc));
        f.focus("name");
        f.handle_key(key(KeyCode::Enter));
        for c in ['j', 'k', 'J', 'K'] {
            f.handle_key(key(KeyCode::Char(c)));
        }
        f.handle_key(key(KeyCode::Enter));
        assert!(f.text("name").ends_with("jkJK"), "{}", f.text("name"));
    }

    #[test]
    fn rows_report_actions_to_the_owner() {
        let mut f = sample();
        f.focus("dests");
        assert_eq!(
            f.handle_key(key(KeyCode::Char('a'))),
            FormEvent::Row("dests", RowAction::Add)
        );
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(
            f.handle_key(key(KeyCode::Enter)),
            FormEvent::Row("dests", RowAction::Edit(0))
        );
        assert_eq!(
            f.handle_key(key(KeyCode::Char('j'))),
            FormEvent::Row("dests", RowAction::MoveDown(0))
        );
        assert_eq!(
            f.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT)),
            FormEvent::Row("dests", RowAction::MoveDown(0)),
            "J too"
        );
        assert_eq!(
            f.handle_key(key(KeyCode::Char('k'))),
            FormEvent::None,
            "already first"
        );
        f.handle_key(key(KeyCode::Down));
        assert_eq!(
            f.handle_key(key(KeyCode::Char('k'))),
            FormEvent::Row("dests", RowAction::MoveUp(1))
        );
        assert_eq!(
            f.handle_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT)),
            FormEvent::Row("dests", RowAction::MoveUp(1)),
            "K too"
        );
        assert_eq!(
            f.handle_key(key(KeyCode::Char('j'))),
            FormEvent::None,
            "last"
        );
        assert_eq!(
            f.handle_key(key(KeyCode::Char('x'))),
            FormEvent::Row("dests", RowAction::Remove(1))
        );
        f.handle_key(key(KeyCode::Down));
        assert_eq!(
            f.handle_key(key(KeyCode::Enter)),
            FormEvent::Row("dests", RowAction::Add)
        );
        f.handle_key(key(KeyCode::Esc));
        assert!(!f.is_editing());
    }

    #[test]
    fn buttons() {
        let mut f = sample();
        f.focus("cancel");
        assert_eq!(f.handle_key(key(KeyCode::Enter)), FormEvent::Cancel);
        f.focus("save");
        assert_eq!(f.handle_key(key(KeyCode::Enter)), FormEvent::Submit);
    }

    #[test]
    fn path_hints() {
        assert_eq!(path_hint("", false), None);
        assert_eq!(path_hint("/", false).unwrap().1, Tone::Success);
        assert_eq!(path_hint("/nonexistent/x", false).unwrap().1, Tone::Error);
        assert_eq!(path_hint("/nonexistent/x", true).unwrap().1, Tone::Warning);
        assert_eq!(path_hint("relative", true).unwrap().1, Tone::Error);
    }

    fn lines_text(form: &Form) -> String {
        let (lines, _, _) = form.lines(100);
        lines.iter().map(|l| l.to_string() + "\n").collect()
    }

    #[test]
    fn tab_completes_folders_in_a_path_field() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for d in ["Documents", "Downloads", "Desktop", ".dotfiles", "Music"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        std::fs::write(root.join("Drafts.txt"), "a file").unwrap();
        let at = |rel: &str| format!("{}/{rel}", root.display());
        let mut f = Form::new(
            "t",
            vec![
                Field::path("p", "Folder", &at("d"), false),
                Field::text("n", "Name", ""),
            ],
        );
        assert!(f.hints().contains("b browse"));
        f.handle_key(key(KeyCode::Enter));
        assert!(f.hints().starts_with("tab complete · ctrl-b browse"));
        assert_eq!(f.handle_key(key(KeyCode::Tab)), FormEvent::None);
        assert_eq!(f.text("p"), at("Desktop/"), "files are not offered");
        assert_eq!(f.focused_key(), Some("p"), "tab stays in the field");
        assert!(
            lines_text(&f).contains("1 of 3: [Desktop], Documents, Downloads"),
            "{}",
            lines_text(&f)
        );
        f.handle_key(key(KeyCode::Tab));
        assert_eq!(f.text("p"), at("Documents/"));
        f.handle_key(key(KeyCode::BackTab));
        f.handle_key(key(KeyCode::BackTab));
        assert_eq!(f.text("p"), at("Downloads/"), "wraps back");
        // Typing carries on from the completed text and ends the cycle.
        typed(&mut f, "x");
        assert_eq!(f.text("p"), at("Downloads/x"));
        assert!(!lines_text(&f).contains("of 3"));
        f.handle_key(key(KeyCode::Tab));
        assert!(lines_text(&f).contains("no matching folder"));
        // A trailing slash lists the children; a dot shows hidden ones.
        f.handle_key(key(KeyCode::Char('u')));
        f.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        typed(&mut f, &at("."));
        f.handle_key(key(KeyCode::Tab));
        assert_eq!(f.text("p"), at(".dotfiles/"));
        assert_eq!(f.handle_key(key(KeyCode::Enter)), FormEvent::Changed("p"));
        assert_eq!(f.focused_key(), Some("p"));
        assert_eq!(
            f.handle_key(key(KeyCode::Char('b'))),
            FormEvent::Browse("p")
        );
        // A text field that is not a folder field: tab moves on, b is nothing.
        f.focus("n");
        assert_eq!(f.handle_key(key(KeyCode::Char('b'))), FormEvent::None);
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(key(KeyCode::Tab));
        assert!(!f.is_editing());
    }

    #[test]
    fn an_empty_path_field_completes_from_home() {
        let mut f = Form::new("t", vec![Field::path("p", "Folder", "", false)]);
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(key(KeyCode::Tab));
        let text = f.text("p");
        assert!(
            text.starts_with("~/") || lines_text(&f).contains("no matching folder"),
            "{text}"
        );
    }

    #[test]
    fn a_tab_for_another_folder_replaces_the_listing_waited_for() {
        let mut f = Form::new(
            "t",
            vec![Field::text("r", "Folder", "Backups/Do").with_folder(Folder::Remote)],
        );
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.handle_key(key(KeyCode::Tab)), FormEvent::List("r"));
        assert_eq!(f.take_list_request(), Some("Backups".to_string()));
        // Another folder typed while that one lists: a new request.
        f.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        typed(&mut f, "Archive/");
        assert!(!lines_text(&f).contains("listing…"));
        assert_eq!(f.handle_key(key(KeyCode::Tab)), FormEvent::List("r"));
        assert_eq!(f.take_list_request(), Some("Archive".to_string()));
        assert!(lines_text(&f).contains("listing…"));
        // The older answer is not used for this Tab.
        f.set_listing("Backups", Ok(vec!["Documents".into()]));
        assert_eq!(f.text("r"), "Archive/");
        assert!(lines_text(&f).contains("listing…"));
        f.set_listing("Archive", Ok(vec!["2019".into()]));
        assert_eq!(f.text("r"), "Archive/2019/");
    }

    #[test]
    fn onedrive_folders_are_listed_by_the_owner() {
        let mut f = Form::new(
            "t",
            vec![Field::text("r", "Folder", "Backups/Do").with_folder(Folder::Remote)],
        );
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.handle_key(key(KeyCode::Tab)), FormEvent::List("r"));
        assert_eq!(f.take_list_request(), Some("Backups".to_string()));
        assert!(lines_text(&f).contains("listing…"));
        // Tab does nothing until the names come.
        assert_eq!(f.handle_key(key(KeyCode::Tab)), FormEvent::None);
        assert_eq!(f.take_list_request(), None);
        f.set_listing("Backups", Ok(vec!["Documents".into(), "Downloads".into()]));
        assert_eq!(
            f.text("r"),
            "Backups/Documents/",
            "the waiting Tab completes"
        );
        f.handle_key(key(KeyCode::Tab));
        assert_eq!(f.text("r"), "Backups/Downloads/");
        // Kept for the life of the form: no second request.
        for _ in 0.."ownloads/".len() {
            f.handle_key(key(KeyCode::Backspace));
        }
        assert_eq!(f.text("r"), "Backups/D");
        assert_eq!(f.handle_key(key(KeyCode::Tab)), FormEvent::None);
        assert_eq!(f.text("r"), "Backups/Documents/");
        // An error is shown and not kept.
        assert_eq!(f.handle_key(key(KeyCode::Tab)), FormEvent::None);
        typed(&mut f, "x/");
        assert_eq!(f.handle_key(key(KeyCode::Tab)), FormEvent::List("r"));
        f.set_listing(
            "Backups/Downloads/x",
            Err("rclone lsjson failed: token expired".into()),
        );
        assert_eq!(f.take_list_request(), Some("Backups/Downloads/x".into()));
        f.set_listing(
            "Backups/Downloads/x",
            Err("rclone lsjson failed: token expired".into()),
        );
        assert!(lines_text(&f).contains("could not list: rclone lsjson failed: token expired"));
        // A listing that arrives after the text changed fills the cache only.
        f.clear_listings();
        f.handle_key(key(KeyCode::Tab));
        typed(&mut f, "y");
        f.set_listing("Backups/Downloads/x", Ok(vec!["z".into()]));
        assert_eq!(f.text("r"), "Backups/Downloads/x/y");
        // Ctrl-B keeps the text and asks for the browser.
        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(f.handle_key(ctrl_b), FormEvent::Browse("r"));
        assert!(!f.is_editing());
        assert_eq!(f.text("r"), "Backups/Downloads/x/y");
        assert_eq!(f.set_value("r", "Archive"), FormEvent::Changed("r"));
        assert_eq!(f.text("r"), "Archive");
    }

    fn render(form: &Form, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| form.render(frame)).unwrap();
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
        let mut f = sample();
        f.handle_key(key(KeyCode::Tab));
        snapshot("form_80", render(&f, 80));
    }

    #[test]
    fn renders_at_120_columns_while_editing_lists() {
        let mut f = sample();
        f.focus("excludes");
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(key(KeyCode::Down));
        f.set_error(
            "name",
            Some("job name \"documents\" is used more than once".into()),
        );
        snapshot("form_120", render(&f, 120));
    }

    #[test]
    fn renders_rows_and_messages_at_80_columns() {
        let mut f = sample();
        f.focus("dests");
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(key(KeyCode::Down));
        f.message = Some((
            "Recorded volume \"Backup\" at /Volumes/Backup.".into(),
            Tone::Success,
        ));
        snapshot("form_rows_80", render(&f, 80));
    }
}
