//! Text editor (`e`): change text on the frame on screen.
//!
//! The cursor moves over the preview. Editing a line works like a text
//! field: typing inserts, Backspace / Delete remove characters (the rest of
//! the line moves along), and Shift+←/→ or a mouse drag selects. Tab
//! switches to the side panel, which sets how long the change lasts. Enter
//! applies it as a text edit; Esc cancels.

use avt::Vt;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

use super::app::App;
use crate::edit::style::{StyleColor, TextStyle, COLOR_NAMES};
use crate::edit::timeline::{text_patch, Origin};
use crate::edit::Edit;

const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Yellow;
const KEY: Color = Color::Cyan;
const LABEL_W: usize = 13;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Colour,
    Background,
    Style,
    Apply,
}

const STYLE_CHOICES: [&str; 6] = [
    "Bold",
    "Italic",
    "Underline",
    "Strikethrough",
    "Inverse",
    "Clear the styling (back to the original)",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Screen,
    Panel,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PanelMode {
    List,
    Dropdown {
        row: Row,
        selected: usize,
    },
    /// Typing a #rrggbb colour in place (for the text, or its background).
    EditHex {
        background: bool,
        input: String,
    },
}

#[derive(Debug, Clone)]
pub struct TextEditor {
    /// Cursor on the recorded screen: (col, row).
    pub cursor: (usize, usize),
    /// Selection start column on the cursor's row, while selecting.
    pub anchor: Option<usize>,
    /// The line being edited, once an edit key is pressed on it.
    pub line: Option<LineBuf>,
    /// Other lines changed in this edit, kept while editing elsewhere.
    pub others: Vec<LineBuf>,
    pub focus: Focus,
    pub row: usize,
    pub panel: PanelMode,
    /// A mouse drag in progress: its row.
    drag_row: Option<usize>,
}

/// A screen line being edited, as a fixed-width row of characters, each
/// with its own style (so text keeps its colours as it moves).
#[derive(Debug, Clone, PartialEq)]
pub struct LineBuf {
    pub row: usize,
    pub original: Vec<char>,
    pub original_styles: Vec<TextStyle>,
    pub buf: Vec<char>,
    pub styles: Vec<TextStyle>,
}

impl LineBuf {
    fn of(vt: &Vt, row: usize, cols: usize) -> Self {
        let cells: Vec<(char, TextStyle)> = vt
            .line(row)
            .cells()
            .iter()
            .filter(|c| c.width() > 0)
            .map(|c| (c.char(), TextStyle::from_pen(c.pen())))
            .chain(std::iter::repeat((' ', TextStyle::default())))
            .take(cols)
            .collect();

        let original: Vec<char> = cells.iter().map(|c| c.0).collect();
        let original_styles: Vec<TextStyle> = cells.iter().map(|c| c.1).collect();

        Self {
            row,
            buf: original.clone(),
            styles: original_styles.clone(),
            original,
            original_styles,
        }
    }

    fn changed(&self) -> bool {
        self.buf != self.original || self.styles != self.original_styles
    }

    fn remove(&mut self, i: usize) {
        if i < self.buf.len() {
            self.buf.remove(i);
            self.styles.remove(i);
            self.buf.push(' ');
            self.styles.push(TextStyle::default());
        }
    }

    fn insert(&mut self, i: usize, ch: char) {
        let cols = self.buf.len();

        // a new character looks like the one before it (or the one it's on)
        let style = i
            .checked_sub(1)
            .and_then(|p| self.styles.get(p))
            .or_else(|| self.styles.get(i))
            .copied()
            .unwrap_or_default();

        self.buf.insert(i.min(cols), ch);
        self.styles.insert(i.min(cols), style);
        self.buf.truncate(cols);
        self.styles.truncate(cols);
    }
}

impl TextEditor {
    /// Puts the current line aside (keeping it if it's changed) and picks
    /// up row `row`'s, if it was changed earlier.
    fn go_to_row(&mut self, row: usize) {
        if self.line.as_ref().is_some_and(|l| l.row == row) {
            return;
        }

        if let Some(l) = self.line.take().filter(LineBuf::changed) {
            self.others.retain(|o| o.row != l.row);
            self.others.push(l);
        }

        if let Some(i) = self.others.iter().position(|o| o.row == row) {
            self.line = Some(self.others.remove(i));
        }
    }

    /// Every changed line, in row order.
    fn changed_lines(&self) -> Vec<&LineBuf> {
        let mut lines: Vec<&LineBuf> = self
            .others
            .iter()
            .chain(self.line.iter())
            .filter(|l| l.changed())
            .collect();

        lines.sort_by_key(|l| l.row);
        lines
    }

    pub fn rows(&self) -> Vec<Row> {
        vec![Row::Colour, Row::Background, Row::Style, Row::Apply]
    }

    /// Selected columns on the cursor's row (inclusive), if selecting.
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.anchor
            .map(|a| (a.min(self.cursor.0), a.max(self.cursor.0)))
    }

    /// The change so far, as the text edit it would be: (col, from, to,
    /// styles). `styles` is empty when the new text looks the way it would
    /// anyway.
    ///
    /// Only the part that changed is kept; when the length changes, the rest
    /// of the line moves along on playback.
    pub fn change(&self) -> Option<(usize, String, String, Vec<TextStyle>)> {
        self.change_of(self.line.as_ref()?)
    }

    /// The change made to `line` (see [`TextEditor::change`]).
    fn change_of(&self, line: &LineBuf) -> Option<(usize, String, String, Vec<TextStyle>)> {
        let line = Some(line).filter(|l| l.changed())?;

        let old: Vec<(char, TextStyle)> = line
            .original
            .iter()
            .copied()
            .zip(line.original_styles.iter().copied())
            .collect();
        let new: Vec<(char, TextStyle)> = line
            .buf
            .iter()
            .copied()
            .zip(line.styles.iter().copied())
            .collect();

        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();

        // Just the text that changed: playback moves the rest of the line
        // along when the length differs, as retyping it would.
        let (old, new) = (trim_blank(&old), trim_blank(&new));

        let prefix = prefix.min(old.len()).min(new.len());

        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();

        // The fixed-width rows line up again in the blanks after the text,
        // so a change that grows takes those blanks in: "ab   " -> "abcd ".
        // Without them, the length difference is the real one (and moves the
        // rest of the line, and the cursor, on playback).
        let old = trim_blank(&old[prefix..old.len() - suffix]);
        let mut new = trim_blank(&new[prefix..new.len() - suffix]);

        // (text added after the end of the line replaces nothing: an insert)
        if old.is_empty() && new.is_empty() {
            return None;
        }

        // what playback would draw without explicit styles: each new
        // character in the style of the text it replaces
        let default_style = |i: usize| {
            Some(
                old.get(i.min(old.len().saturating_sub(1)))
                    .map_or_else(TextStyle::default, |c| c.1),
            )
        };

        let plain = new
            .iter()
            .enumerate()
            .all(|(i, (_, st))| Some(*st) == default_style(i));

        // trailing spaces in their default look are just padding
        if plain {
            let end = new
                .iter()
                .rposition(|(c, _)| *c != ' ')
                .map_or(0, |i| i + 1);
            new = &new[..end];
        }

        let from: String = old.iter().map(|c| c.0).collect();
        let to: String = new.iter().map(|c| c.0).collect();
        let styles = if plain {
            Vec::new()
        } else {
            new.iter().map(|c| c.1).collect()
        };

        Some((prefix, from, to, styles))
    }
}

fn trim_blank(cells: &[(char, TextStyle)]) -> &[(char, TextStyle)] {
    let end = cells
        .iter()
        .rposition(|(c, st)| *c != ' ' || st.bg.is_some() || st.inverse)
        .map_or(0, |i| i + 1);

    &cells[..end]
}

fn trim_end(chars: &[char]) -> &[char] {
    let end = chars.iter().rposition(|c| *c != ' ').map_or(0, |i| i + 1);

    &chars[..end]
}

/// The text of `cols` cells of `row` from `col`.
fn cells_text(vt: &Vt, row: usize, col: usize, cols: usize) -> String {
    let (w, h) = vt.size();

    if row >= h {
        return String::new();
    }

    vt.line(row)
        .cells()
        .iter()
        .skip(col)
        .take(cols.min(w.saturating_sub(col)))
        .filter(|c| c.width() > 0)
        .map(|c| c.char())
        .collect()
}

// ---- opening & applying ---------------------------------------------------------

pub fn open(app: &mut App) {
    if let Some(Origin::Clip { .. }) = app.current_frame().and_then(|i| app.timeline.origin(i)) {
        return app.info(
            "That frame is part of an inserted clip; edit text on the recording's own frames",
        );
    }

    let vt = app.vt();
    let (cols, rows) = vt.size();
    let c = vt.cursor();
    let cursor = (
        c.col.min(cols.saturating_sub(1)),
        c.row.min(rows.saturating_sub(1)),
    );
    drop(vt);

    app.text = Some(TextEditor {
        cursor,
        anchor: None,
        line: None,
        others: Vec::new(),
        focus: Focus::Screen,
        row: 0,
        panel: PanelMode::List,
        drag_row: None,
    });

    app.info("Move to the text (arrows), then type, Backspace or Delete as in any text field");
}

fn apply(app: &mut App) {
    let Some(ed) = &app.text else { return };
    let at = app.insert_position();

    // one text edit per changed line, saved (and undone) together
    let edits: Vec<Edit> = ed
        .changed_lines()
        .into_iter()
        .filter_map(|line| {
            let (col, from, to, styles) = ed.change_of(line)?;

            Some(Edit::Text {
                at,
                row: line.row,
                col,
                from,
                to,
                styles: crate::edit::style::to_runs(&styles),
            })
        })
        .collect();

    if edits.is_empty() {
        return app.info("Nothing's changed yet: type, Backspace / Delete, or restyle some text");
    }

    let msg = match &edits[..] {
        [one] => format!("{} (saved)", crate::cmd::edit::describe(one)),
        many => format!("Changed {} lines (saved)", many.len()),
    };

    app.text = None;
    app.transact(msg, |list| {
        for e in edits {
            list.push(e)?;
        }

        Ok(())
    });
}

// ---- the preview ------------------------------------------------------------------

/// The frame with the edit being typed drawn in.
pub fn preview_screen(app: &App) -> Option<Vt> {
    let ed = app.text.as_ref()?;
    let lines = ed.changed_lines();

    if lines.is_empty() {
        return None;
    }

    let mut vt = app.timeline.vt_at_position(app.position);

    for line in lines {
        let text: String = line.buf.iter().collect();
        let patch = text_patch(&vt, line.row, 0, line.buf.len(), &text, &line.styles);
        vt.feed_str(&patch);
    }

    Some(vt)
}

/// What the editor highlights on the preview, in recorded screen cells.
pub struct Overlay {
    /// Selected cells: (row, first col, last col).
    pub selection: Option<(usize, usize, usize)>,
    /// The cursor: (col, row).
    pub cursor: (usize, usize),
}

pub fn overlay(app: &App) -> Option<Overlay> {
    let ed = app.text.as_ref()?;

    Some(Overlay {
        selection: ed.selection().map(|(a, b)| (ed.cursor.1, a, b)),
        cursor: ed.cursor,
    })
}

// ---- keys & mouse ----------------------------------------------------------------

pub fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(ed) = &app.text else { return };

    match ed.focus {
        Focus::Screen => screen_key(app, key),
        Focus::Panel => panel_key(app, key),
    }
}

fn screen_key(app: &mut App, key: KeyEvent) {
    let (cols, rows) = app.vt().size();
    let vt = app.timeline.vt_at_position(app.position);
    let Some(ed) = &mut app.text else { return };
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let col = ed.cursor.0;

    // the line under the cursor, as an editable buffer
    let start_line = |ed: &mut TextEditor| {
        let row = ed.cursor.1;

        ed.go_to_row(row);

        if ed.line.is_none() {
            ed.line = Some(LineBuf::of(&vt, row, cols));
        }
    };

    // removes the selection; true if there was one
    let cut_selection = |ed: &mut TextEditor| -> bool {
        let Some((a, b)) = ed.selection() else {
            return false;
        };

        if let Some(line) = &mut ed.line {
            for _ in a..=b.min(line.buf.len() - 1) {
                line.remove(a);
            }
        }

        ed.anchor = None;
        ed.cursor.0 = a;
        true
    };

    match key.code {
        KeyCode::Esc => {
            if ed.anchor.is_some() {
                ed.anchor = None;
            } else {
                app.text = None;
                app.info("Text edit cancelled");
            }
        }

        KeyCode::Tab => ed.focus = Focus::Panel,
        KeyCode::Enter => apply(app),

        // changes to the line being left are kept
        KeyCode::Up | KeyCode::Down => {
            ed.anchor = None;
            let row = match key.code {
                KeyCode::Up => ed.cursor.1.saturating_sub(1),
                _ => (ed.cursor.1 + 1).min(rows.saturating_sub(1)),
            };
            ed.cursor.1 = row;
            ed.go_to_row(row);
        }

        KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End => {
            if shift {
                ed.anchor.get_or_insert(col);
            } else {
                ed.anchor = None;
            }

            let end_of_text = ed
                .line
                .as_ref()
                .map(|l| trim_end(&l.buf).len())
                .unwrap_or_else(|| {
                    cells_text(&vt, ed.cursor.1, 0, cols)
                        .trim_end()
                        .chars()
                        .count()
                });

            ed.cursor.0 = match key.code {
                KeyCode::Left => col.saturating_sub(1),
                KeyCode::Right => (col + 1).min(cols.saturating_sub(1)),
                KeyCode::Home => 0,
                _ => end_of_text.min(cols.saturating_sub(1)),
            };
        }

        KeyCode::Backspace => {
            start_line(ed);

            if !cut_selection(ed) && col > 0 {
                if let Some(line) = &mut ed.line {
                    line.remove(col - 1);
                }

                ed.cursor.0 = col - 1;
            }
        }

        KeyCode::Delete => {
            start_line(ed);

            if !cut_selection(ed) {
                if let Some(line) = &mut ed.line {
                    line.remove(col);
                }
            }
        }

        KeyCode::Char(ch) if !ctrl => {
            start_line(ed);
            cut_selection(ed);

            let col = ed.cursor.0;

            if let Some(line) = &mut ed.line {
                // inserting pushes the rest of the line along (off the edge)
                line.insert(col, ch);
            }

            ed.cursor.0 = (col + 1).min(cols.saturating_sub(1));
        }

        _ => {}
    }
}

/// Choices in a row's dropdown.
fn choices(row: Row) -> Vec<String> {
    match row {
        Row::Colour | Row::Background => std::iter::once("The terminal's default".to_owned())
            .chain(COLOR_NAMES.iter().map(|n| n.replace('-', " ")))
            .chain(std::iter::once("Custom #rrggbb…".to_owned()))
            .collect(),

        Row::Style => STYLE_CHOICES.iter().map(|s| s.to_string()).collect(),
        Row::Apply => Vec::new(),
    }
}

/// Where the dropdown starts, given the current setting.
fn initial_choice(ed: &TextEditor, row: Row) -> usize {
    match row {
        Row::Colour | Row::Background => {
            let style = target_styles(ed).and_then(|s| s.first().copied());
            let c = style.and_then(|s| if row == Row::Colour { s.fg } else { s.bg });

            match c {
                None => 0,
                Some(StyleColor::Indexed(n)) if n < 16 => n as usize + 1,
                Some(_) => 17,
            }
        }
        _ => 0,
    }
}

/// The cells styling applies to: the selection, or the one under the cursor.
fn target_range(ed: &TextEditor) -> (usize, usize) {
    ed.selection().unwrap_or((ed.cursor.0, ed.cursor.0))
}

fn target_styles(ed: &TextEditor) -> Option<Vec<TextStyle>> {
    let (a, b) = target_range(ed);
    let line = ed.line.as_ref()?;

    Some(
        line.styles
            .get(a..=b.min(line.styles.len().saturating_sub(1)))?
            .to_vec(),
    )
}

/// Changes the style of the selected cells (starting the line if needed).
fn restyle(app: &mut App, change: impl Fn(&mut TextStyle, &TextStyle)) {
    let (cols, _) = app.vt().size();
    let vt = app.timeline.vt_at_position(app.position);
    let Some(ed) = &mut app.text else { return };
    let row = ed.cursor.1;

    ed.go_to_row(row);

    if ed.line.is_none() {
        ed.line = Some(LineBuf::of(&vt, row, cols));
    }

    let (a, b) = target_range(ed);

    if let Some(line) = &mut ed.line {
        for i in a..=b.min(cols.saturating_sub(1)) {
            let original = line.original_styles[i];
            change(&mut line.styles[i], &original);
        }
    }
}

fn panel_key(app: &mut App, key: KeyEvent) {
    let Some(ed) = &mut app.text else { return };
    let rows = ed.rows();

    match ed.panel.clone() {
        PanelMode::List => match key.code {
            KeyCode::Tab | KeyCode::Esc => ed.focus = Focus::Screen,
            KeyCode::Up | KeyCode::Char('k') => ed.row = ed.row.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => ed.row = (ed.row + 1).min(rows.len() - 1),

            KeyCode::Enter => match rows[ed.row.min(rows.len() - 1)] {
                Row::Apply => apply(app),

                row => {
                    let selected = initial_choice(ed, row);
                    ed.panel = PanelMode::Dropdown { row, selected };
                }
            },

            _ => {}
        },

        PanelMode::Dropdown { row, selected } => {
            let n = choices(row).len();

            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    ed.panel = PanelMode::Dropdown {
                        row,
                        selected: selected.saturating_sub(1),
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    ed.panel = PanelMode::Dropdown {
                        row,
                        selected: (selected + 1).min(n - 1),
                    }
                }
                KeyCode::Esc => ed.panel = PanelMode::List,
                KeyCode::Enter => choose(app, row, selected),
                _ => {}
            }
        }

        PanelMode::EditHex {
            background,
            mut input,
        } => match key.code {
            KeyCode::Esc => ed.panel = PanelMode::List,
            KeyCode::Backspace => {
                input.pop();
                ed.panel = PanelMode::EditHex { background, input };
            }
            KeyCode::Char(c) if c.is_ascii_hexdigit() || c == '#' => {
                if input.len() < 7 {
                    input.push(c);
                }
                ed.panel = PanelMode::EditHex { background, input };
            }

            KeyCode::Enter => {
                let hex = super::color::normalize_hex(&input);

                match StyleColor::parse(&hex) {
                    Ok(StyleColor::Rgb(r, g, b)) => {
                        ed.panel = PanelMode::List;
                        let c = Some(StyleColor::Rgb(r, g, b));
                        restyle(app, |s, _| if background { s.bg = c } else { s.fg = c });
                    }
                    _ => app.error("Not a colour - use #rrggbb or #rgb"),
                }
            }

            _ => {}
        },
    }
}

fn choose(app: &mut App, row: Row, choice: usize) {
    let Some(ed) = &mut app.text else { return };
    ed.panel = PanelMode::List;

    match row {
        Row::Colour | Row::Background => {
            let background = row == Row::Background;

            // last choice: type a colour
            if choice == 17 {
                ed.panel = PanelMode::EditHex {
                    background,
                    input: String::new(),
                };
                return;
            }

            let c = choice.checked_sub(1).map(|n| StyleColor::Indexed(n as u8));

            restyle(app, |s, _| if background { s.bg = c } else { s.fg = c });
        }

        Row::Style => {
            // a toggle: on for all of them, unless they all have it already
            let all_on = |pick: fn(&TextStyle) -> bool| {
                target_styles(ed).is_some_and(|v| !v.is_empty() && v.iter().all(pick))
            };

            let on = match choice {
                0 => !all_on(|s| s.bold),
                1 => !all_on(|s| s.italic),
                2 => !all_on(|s| s.underline),
                3 => !all_on(|s| s.strike),
                4 => !all_on(|s| s.inverse),
                _ => false,
            };

            restyle(app, move |s, original| match choice {
                0 => s.bold = on,
                1 => s.italic = on,
                2 => s.underline = on,
                3 => s.strike = on,
                4 => s.inverse = on,
                _ => *s = *original,
            });
        }

        Row::Apply => {}
    }
}

/// Mouse over the preview: click to move the cursor, drag along a line to
/// select. `cell` maps the mouse to a cell of the recorded screen.
pub fn handle_preview_mouse(app: &mut App, mouse: MouseEvent, cell: Option<(usize, usize)>) {
    let Some(ed) = &mut app.text else { return };

    match (mouse.kind, cell) {
        (MouseEventKind::Down(MouseButton::Left), Some((c, r))) => {
            ed.focus = Focus::Screen;
            ed.cursor = (c, r);
            ed.go_to_row(r);
            ed.anchor = None;
            ed.drag_row = Some(r);
        }

        (MouseEventKind::Drag(MouseButton::Left), Some((c, _))) => {
            if let Some(r) = ed.drag_row {
                ed.anchor.get_or_insert(ed.cursor.0);
                ed.cursor = (c, r); // a selection stays on one line
            }
        }

        (MouseEventKind::Up(MouseButton::Left), _) => ed.drag_row = None,
        _ => {}
    }
}

pub fn handle_panel_mouse(app: &mut App, mouse: MouseEvent, inner: Rect) {
    let Some(ed) = &mut app.text else { return };

    if let (PanelMode::List, MouseEventKind::Down(MouseButton::Left)) = (&ed.panel, mouse.kind) {
        let i = mouse.row.saturating_sub(inner.y) as usize;

        if i < ed.rows().len() {
            ed.focus = Focus::Panel;
            ed.row = i;
        }
    }
}

// ---- drawing ------------------------------------------------------------------------

fn label(s: &str) -> Span<'static> {
    Span::styled(format!("{s:<LABEL_W$}"), Style::default().fg(Color::Gray))
}

fn row_line(ed: &TextEditor, row: Row) -> Line<'static> {
    let arrow = || Span::styled(" ▾", Style::default().fg(DIM));

    match row {
        Row::Colour | Row::Background => {
            let styles = target_styles(ed).unwrap_or_default();
            let picks: Vec<Option<StyleColor>> = styles
                .iter()
                .map(|s| if row == Row::Colour { s.fg } else { s.bg })
                .collect();

            let value = match picks.first() {
                None => "-".to_owned(),
                Some(first) if picks.iter().all(|p| p == first) => match first {
                    None => "default".to_owned(),
                    Some(c) => c.to_string().replace('-', " "),
                },
                Some(_) => "mixed".to_owned(),
            };

            let name = if row == Row::Colour {
                "Colour"
            } else {
                "Background"
            };

            // typing a custom colour: the value box
            if let PanelMode::EditHex { background, input } = &ed.panel {
                if *background == (row == Row::Background) {
                    return Line::from(vec![
                        label(name),
                        Span::styled(
                            format!(" {input}▏      "),
                            Style::default()
                                .fg(Color::White)
                                .bg(Color::Indexed(238))
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]);
                }
            }

            Line::from(vec![label(name), Span::raw(value), arrow()])
        }

        Row::Style => {
            let styles = target_styles(ed).unwrap_or_default();
            let all = |pick: fn(&TextStyle) -> bool| !styles.is_empty() && styles.iter().all(pick);
            let mut on: Vec<&str> = Vec::new();

            for (yes, name) in [
                (all(|s| s.bold), "bold"),
                (all(|s| s.italic), "italic"),
                (all(|s| s.underline), "underline"),
                (all(|s| s.strike), "strikethrough"),
                (all(|s| s.inverse), "inverse"),
            ] {
                if yes {
                    on.push(name);
                }
            }

            let value = if on.is_empty() {
                "plain".to_owned()
            } else {
                on.join(", ")
            };
            Line::from(vec![label("Style"), Span::raw(value), arrow()])
        }

        Row::Apply => Line::from(Span::styled(
            "  ⏎ Apply the change",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
    }
}

fn row_help(row: Row) -> &'static str {
    match row {
        Row::Colour => "The colour of the selected text (or the character under the cursor): the terminal's default, one of the 16 palette colours, or a #rrggbb of your own.",
        Row::Background => "The background behind the selected text (or the character under the cursor).",
        Row::Style => "Turn bold, italic, underline, strikethrough or inverse on or off for the selected text, or put back how it originally looked.",
        Row::Apply => "Save the change as a text edit. It shows on the timeline as ✎; d on its frame removes it, u undoes.",
    }
}

pub fn draw(buf: &mut Buffer, area: Rect, app: &App) -> Rect {
    let Some(ed) = &app.text else {
        return Rect::default();
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(" edit text ");

    let inner = block.inner(area);
    Clear.render(area, buf);
    block.render(area, buf);

    let mut y = inner.y;
    let line_at = |buf: &mut Buffer, y: u16, line: Line<'_>| {
        if y < inner.bottom() {
            Paragraph::new(line).render(Rect::new(inner.x, y, inner.width, 1), buf);
        }
    };

    // what's changing
    let change = if let Some((_, from, to, styles)) = ed.change() {
        let restyled = if styles.is_empty() { "" } else { " (restyled)" };
        let others = ed.changed_lines().len().saturating_sub(1);
        let restyled = match others {
            0 => restyled.to_owned(),
            n => format!("{restyled} +{n} other line(s)"),
        };

        Line::from(vec![
            Span::styled(format!("\"{from}\""), Style::default().fg(DIM)),
            Span::raw(" → "),
            Span::styled(format!("\"{to}\""), Style::default().fg(ACCENT)),
            Span::styled(restyled, Style::default().fg(DIM)),
        ])
    } else {
        Line::from(Span::styled(
            "Type, Backspace or Delete to change the line",
            Style::default().fg(DIM),
        ))
    };

    line_at(buf, y, change);
    y += 2;

    let rows = ed.rows();

    for (i, row) in rows.iter().enumerate() {
        let mut line = row_line(ed, *row);

        if ed.focus == Focus::Panel && i == ed.row {
            line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
        }

        line_at(buf, y + i as u16, line);
    }

    let help = match ed.focus {
        Focus::Screen => "Arrows move the cursor. Typing inserts; Backspace and Delete remove characters, and the rest of the line moves along. Shift+←/→ or a drag selects (typing replaces it). Tab for the options, Enter applies, Esc cancels.".to_owned(),
        Focus::Panel => format!("{}  Tab returns to the text.", row_help(rows[ed.row.min(rows.len() - 1)])),
    };

    let hy = y + rows.len() as u16 + 1;

    if hy < inner.bottom() {
        Paragraph::new(Span::styled(help, Style::default().fg(DIM)))
            .wrap(Wrap { trim: true })
            .render(
                Rect::new(inner.x, hy, inner.width, inner.bottom() - hy),
                buf,
            );
    }

    // the dropdown, under its row (scrolling when it doesn't fit)
    if let PanelMode::Dropdown { row, selected } = &ed.panel {
        let choices = choices(*row);
        let theme = app
            .timeline
            .header
            .term_theme
            .clone()
            .unwrap_or_else(super::meta::default_theme);

        let ry = y + rows.iter().position(|r| r == row).unwrap_or(0) as u16 + 1;
        let x = inner.x + LABEL_W as u16 - 1;
        let w = inner.width.saturating_sub(LABEL_W as u16 - 1);
        let h = (choices.len() as u16 + 2).min(inner.bottom().saturating_sub(ry));
        let rect = Rect::new(x, ry, w, h);

        Clear.render(rect, buf);
        let b = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(KEY));
        let list = b.inner(rect);
        b.render(rect, buf);

        let shown = list.height as usize;
        let top = selected.saturating_sub(shown.saturating_sub(1));
        let colours = matches!(row, Row::Colour | Row::Background);

        for (n, c) in choices.iter().enumerate().skip(top).take(shown) {
            let mut spans = Vec::new();

            // palette colours get a swatch, in the recording's theme
            if colours && (1..=16).contains(&n) {
                let rgb = theme.palette[n - 1];
                spans.push(Span::styled(
                    " ██",
                    Style::default().fg(Color::Rgb(rgb.r, rgb.g, rgb.b)),
                ));
            } else if colours {
                spans.push(Span::raw("   "));
            }

            spans.push(Span::raw(format!(" {c}")));
            let mut line = Line::from(spans);

            if n == *selected {
                line = line.patch_style(Style::default().fg(KEY).add_modifier(Modifier::REVERSED));
            }

            Paragraph::new(line).render(
                Rect::new(list.x, list.y + (n - top) as u16, list.width, 1),
                buf,
            );
        }
    }

    inner
}

pub fn hints(ed: &TextEditor) -> &'static [(&'static str, &'static str)] {
    match (ed.focus, &ed.panel) {
        (Focus::Screen, _) => &[
            ("arrows", "move"),
            ("type", "insert"),
            ("⌫ / del", "remove"),
            ("⇧←→", "select"),
            ("tab", "options"),
            ("enter", "apply"),
            ("esc", "cancel"),
        ],
        (Focus::Panel, PanelMode::List) => &[
            ("↑↓", "select"),
            ("enter", "change"),
            ("tab", "back to the text"),
        ],
        (Focus::Panel, PanelMode::Dropdown { .. }) => {
            &[("↑↓", "choose"), ("enter", "select"), ("esc", "cancel")]
        }
        (Focus::Panel, PanelMode::EditHex { .. }) => {
            &[("#rrggbb", "colour"), ("enter", "apply"), ("esc", "cancel")]
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{handle_key, open, PanelMode};
    use crate::edit::timeline::Origin;
    use crate::edit::tui::app::tests::app;
    use crate::edit::tui::app::App;
    use crate::edit::Edit;

    fn key(a: &mut App, code: KeyCode) {
        handle_key(a, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn shift(a: &mut App, code: KeyCode) {
        handle_key(a, KeyEvent::new(code, KeyModifiers::SHIFT));
    }

    fn typed(a: &mut App, s: &str) {
        for c in s.chars() {
            key(a, KeyCode::Char(c));
        }
    }

    fn row(a: &App, r: usize) -> String {
        a.vt().text()[r].trim_end().to_owned()
    }

    /// The fixture after "$ ls\r\nfile\r\n": row 0 "$ ls", row 1 "file".
    fn at_file(a: &mut App) {
        a.seek_position(4);
        open(a);
        a.text.as_mut().unwrap().cursor = (0, 1);
    }

    /// The edit as (col, from, to).
    fn change(a: &App) -> (usize, String, String) {
        match &a.edits.edits[..] {
            [Edit::Text { col, from, to, .. }] => (*col, from.clone(), to.clone()),
            other => panic!("expected one text edit, got {other:?}"),
        }
    }

    #[test]
    fn select_and_type() {
        let (mut a, _d) = app();
        at_file(&mut a);

        // select "file" and replace it
        for _ in 0..3 {
            shift(&mut a, KeyCode::Right);
        }
        typed(&mut a, "data");
        assert_eq!(
            super::preview_screen(&a).unwrap().text()[1].trim_end(),
            "data",
            "the preview shows the edit as it's typed"
        );

        key(&mut a, KeyCode::Enter);
        assert!(a.text.is_none());
        assert_eq!(
            a.edits.edits,
            [Edit::Text {
                at: 4,
                row: 1,
                col: 0,
                from: "file".to_owned(),
                to: "data".to_owned(),
                styles: Vec::new(),
            }]
        );

        // the edit is drawn on this frame and stays
        assert_eq!(row(&a, 1), "data");
        a.step(1);
        assert_eq!(row(&a, 1), "data");

        // d on the edit's frame removes it
        let k = (0..a.len())
            .find(|&i| a.timeline.origin(i) == Some(Origin::TextEdit { edit: 0 }))
            .unwrap();
        a.seek_position(k + 1);
        a.delete();
        assert!(a.edits.edits.is_empty());
        a.undo();
        assert_eq!(a.edits.edits.len(), 1);
    }

    #[test]
    fn backspace_and_delete_remove_existing_text() {
        let (mut a, _d) = app();
        a.seek_position(4); // row 0: "$ ls"
        open(&mut a);
        a.text.as_mut().unwrap().cursor = (3, 0); // on "s"

        // Backspace removes the "l" before the cursor; the "s" moves left
        key(&mut a, KeyCode::Backspace);
        assert_eq!(
            super::preview_screen(&a).unwrap().text()[0].trim_end(),
            "$ s"
        );
        assert_eq!(a.text.as_ref().unwrap().cursor, (2, 0));

        // Delete removes the "s" under the cursor
        key(&mut a, KeyCode::Delete);
        assert_eq!(super::preview_screen(&a).unwrap().text()[0].trim_end(), "$");

        key(&mut a, KeyCode::Enter);
        assert_eq!(change(&a), (1, " ls".to_owned(), String::new()));
        assert_eq!(row(&a, 0), "$");
    }

    #[test]
    fn deleting_moves_the_rest_of_the_line() {
        let (mut a, _d) = app();
        a.seek_position(4);
        open(&mut a);
        a.text.as_mut().unwrap().cursor = (2, 0); // on "l" of "$ ls"

        key(&mut a, KeyCode::Delete);
        key(&mut a, KeyCode::Enter);

        // just the removed letter; playback moves the rest along
        assert_eq!(change(&a), (2, "l".to_owned(), String::new()));
        assert_eq!(row(&a, 0), "$ s");
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let (mut a, _d) = app();
        at_file(&mut a);
        a.text.as_mut().unwrap().cursor = (2, 1); // "fi|le"

        typed(&mut a, "XY");
        key(&mut a, KeyCode::Backspace); // takes back the Y
        key(&mut a, KeyCode::Enter);

        assert_eq!(row(&a, 1), "fiXle");
        assert_eq!(change(&a), (2, String::new(), "X".to_owned()));
    }

    #[test]
    fn longer_text_moves_the_recorded_cursor() {
        use crate::asciicast::{Event, Header};
        use crate::edit::{EditList, Timeline};
        use std::time::Duration;

        // a prompt, then a command typed after it
        let header = Header {
            term_cols: 20,
            term_rows: 2,
            ..Default::default()
        };
        let events = vec![
            Event::output(Duration::from_secs(1), "me$ ".to_owned()),
            Event::output(Duration::from_secs(2), "l".to_owned()),
            Event::output(Duration::from_secs(3), "s".to_owned()),
        ];
        let dir = tempfile::tempdir().unwrap();
        let tl = Timeline::new(header, events);
        let mut a = App::new(
            dir.path().join("x.cast"),
            dir.path().join("x.edits.json"),
            tl,
            EditList::new(3),
        );

        // after the prompt, change "me" to "me@laptop"
        a.seek_position(1);
        open(&mut a);
        a.text.as_mut().unwrap().cursor = (2, 0);
        typed(&mut a, "@laptop");
        key(&mut a, KeyCode::Enter);

        // the command typed afterwards follows the longer prompt
        let end = a.timeline.vt_at_position(a.len());
        assert_eq!(end.text()[0].trim_end(), "me@laptop$ ls");
        assert_eq!(end.cursor().col, 13);

        // and typing past the end of the text is an insert
        let (mut a, _d) = app();
        at_file(&mut a);
        key(&mut a, KeyCode::End);
        typed(&mut a, "s");
        key(&mut a, KeyCode::Enter);
        assert_eq!(change(&a), (4, String::new(), "s".to_owned()));
        assert_eq!(row(&a, 1), "files");
    }

    #[test]
    fn selection_then_backspace_removes_it() {
        let (mut a, _d) = app();
        at_file(&mut a);
        a.text.as_mut().unwrap().cursor = (1, 1); // "f|ile"
        shift(&mut a, KeyCode::Right); // "il"
        key(&mut a, KeyCode::Backspace);
        key(&mut a, KeyCode::Enter);

        assert_eq!(row(&a, 1), "fe");
    }

    #[test]
    fn several_lines_in_one_edit() {
        let (mut a, _d) = app();
        a.seek_position(4); // row 0 "$ ls", row 1 "file"
        open(&mut a);

        // restyle on row 1, then move up: the change is kept and the cursor
        // moves freely
        a.text.as_mut().unwrap().cursor = (0, 1);
        shift(&mut a, KeyCode::Right);
        key(&mut a, KeyCode::Tab);
        key(&mut a, KeyCode::Enter); // Colour
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down); // red
        key(&mut a, KeyCode::Enter);
        key(&mut a, KeyCode::Tab);

        key(&mut a, KeyCode::Up);
        assert_eq!(
            a.text.as_ref().unwrap().cursor.1,
            0,
            "moved to another line"
        );

        // change row 0 too; both show in the preview
        a.text.as_mut().unwrap().cursor = (2, 0);
        shift(&mut a, KeyCode::Right);
        typed(&mut a, "LS");
        let vt = super::preview_screen(&a).unwrap();
        assert_eq!(vt.text()[0].trim_end(), "$ LS");
        assert_eq!(
            vt.line(1).cells()[0].pen().foreground(),
            Some(avt::Color::Indexed(1))
        );

        // back on row 1, its change is still there
        key(&mut a, KeyCode::Down);
        assert!(a.text.as_ref().unwrap().line.as_ref().unwrap().styles[0]
            .fg
            .is_some());

        // Enter saves both, as one undoable step
        key(&mut a, KeyCode::Enter);
        assert_eq!(a.edits.edits.len(), 2);
        a.undo();
        assert!(a.edits.edits.is_empty());
    }

    #[test]
    fn esc_cancels() {
        let (mut a, _d) = app();
        at_file(&mut a);
        shift(&mut a, KeyCode::Right);
        key(&mut a, KeyCode::Esc); // clears the selection first
        assert!(a.text.is_some());
        key(&mut a, KeyCode::Esc);
        assert!(a.text.is_none());
        assert!(a.edits.edits.is_empty());
    }

    #[test]
    fn colouring_a_selection() {
        use crate::edit::style::StyleColor;

        let (mut a, _d) = app();
        at_file(&mut a);

        // select "fi", then Tab to the panel: Colour -> red
        shift(&mut a, KeyCode::Right);
        key(&mut a, KeyCode::Tab);
        key(&mut a, KeyCode::Enter); // Colour
        assert!(matches!(
            a.text.as_ref().unwrap().panel,
            PanelMode::Dropdown { selected: 0, .. }
        ));
        key(&mut a, KeyCode::Down); // black
        key(&mut a, KeyCode::Down); // red
        key(&mut a, KeyCode::Enter);

        // Style -> bold
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down); // Style
        key(&mut a, KeyCode::Enter);
        key(&mut a, KeyCode::Enter); // bold

        // the preview shows it, before applying
        let vt = super::preview_screen(&a).unwrap();
        let pen = *vt.line(1).cells()[0].pen();
        assert_eq!(pen.foreground(), Some(avt::Color::Indexed(1)));
        assert!(pen.is_bold());

        key(&mut a, KeyCode::Tab);
        key(&mut a, KeyCode::Enter);

        let [Edit::Text {
            from, to, styles, ..
        }] = &a.edits.edits[..]
        else {
            panic!("{:?}", a.edits.edits);
        };
        assert_eq!(
            (from.as_str(), to.as_str()),
            ("fi", "fi"),
            "same text, new look"
        );
        assert_eq!(styles.len(), 1);
        assert_eq!(styles[0].len, 2);
        assert_eq!(styles[0].style.fg, Some(StyleColor::Indexed(1)));
        assert!(styles[0].style.bold);

        // and it's in the recording
        let pen = *a.vt().line(1).cells()[1].pen();
        assert_eq!(pen.foreground(), Some(avt::Color::Indexed(1)));
        let plain = *a.vt().line(1).cells()[2].pen();
        assert_eq!(plain.foreground(), None, "only the selection");
    }

    #[test]
    fn moved_text_keeps_its_own_colours() {
        use crate::asciicast::{Event, Header};
        use crate::edit::{EditList, Timeline};
        use std::time::Duration;

        // "ab" plain, then "CD" in red
        let header = Header {
            term_cols: 10,
            term_rows: 2,
            ..Default::default()
        };
        let events = vec![Event::output(
            Duration::from_secs(1),
            "ab\x1b[31mCD\x1b[0m".to_owned(),
        )];
        let dir = tempfile::tempdir().unwrap();
        let tl = Timeline::new(header, events);
        let mut a = App::new(
            dir.path().join("x.cast"),
            dir.path().join("x.edits.json"),
            tl,
            EditList::new(1),
        );

        a.seek_position(1);
        open(&mut a);
        a.text.as_mut().unwrap().cursor = (1, 0); // on "b"
        key(&mut a, KeyCode::Delete); // "aCD": the red letters move left
        key(&mut a, KeyCode::Enter);

        let line = a.vt().line(0).cells().to_vec();
        assert_eq!(a.vt().text()[0].trim_end(), "aCD");
        assert_eq!(
            line[1].pen().foreground(),
            Some(avt::Color::Indexed(1)),
            "C is still red"
        );
        assert_eq!(line[2].pen().foreground(), Some(avt::Color::Indexed(1)));
        assert_eq!(line[3].pen().foreground(), None);
    }
}
