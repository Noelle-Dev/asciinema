//! Metadata side panel: header fields, color picker and cursor picker.
//!
//! Changes made in a picker are previewed live (the header is modified
//! without saving) and committed as a normal edit on Enter, or reverted on Esc.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use rgb::RGB8;

use super::app::{App, PromptKind};
use super::color::{ColorPicker, Slider, Slot, HEX_ROW, PALETTE_NAMES};
use crate::asciicast::{CursorShape, Header};
use crate::edit::edits::{to_hex, ThemeColors};
use crate::edit::themes::{self, PRESETS};
use crate::edit::Edit;
use crate::tty::TtyTheme;

pub const PANEL_WIDTH: u16 = 48;

const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Yellow;
const KEY: Color = Color::Cyan;

/// Width of the label column in the color picker.
const LABEL_W: u16 = 13;

/// Width of the label column in the list (fits "15 bright magenta").
const LIST_LABEL_W: u16 = 18;

pub const CURSOR_OPTIONS: [(Option<CursorShape>, &str); 7] = [
    (None, "Terminal default"),
    (Some(CursorShape::BlinkingBlock), "Block, blinking"),
    (Some(CursorShape::Block), "Block, steady"),
    (Some(CursorShape::BlinkingUnderline), "Underline, blinking"),
    (Some(CursorShape::Underline), "Underline, steady"),
    (Some(CursorShape::BlinkingBar), "Bar, blinking"),
    (Some(CursorShape::Bar), "Bar, steady"),
];

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Heading(&'static str),
    Title,
    Command,
    IdleLimit,
    Cursor,
    Theme,
    Color(Slot),
    Info(String, String),
}

impl Row {
    fn selectable(&self) -> bool {
        !matches!(self, Row::Heading(_))
    }
}

#[derive(Debug, Clone)]
pub enum Mode {
    List,
    Color(ColorPicker),
    Cursor(usize),
    Preset(usize),
}

#[derive(Debug, Clone)]
pub struct Panel {
    pub selected: usize,
    pub scroll: Cell<usize>,
    pub mode: Mode,
    /// Opened straight into a picker (C): close the panel when it's done.
    close_when_done: bool,
}

/// Used when a recording has no theme and a color gets edited: xterm's defaults.
pub fn default_theme() -> TtyTheme {
    PRESETS
        .iter()
        .find(|p| p.name == "xterm")
        .expect("xterm preset exists")
        .theme()
}

fn theme_color(theme: &TtyTheme, slot: Slot) -> RGB8 {
    match slot {
        Slot::Foreground => theme.fg,
        Slot::Background => theme.bg,
        Slot::Palette(i) => theme.palette.get(i).copied().unwrap_or_default(),
    }
}

fn with_color(mut theme: TtyTheme, slot: Slot, c: RGB8) -> TtyTheme {
    match slot {
        Slot::Foreground => theme.fg = c,
        Slot::Background => theme.bg = c,
        Slot::Palette(i) => {
            if let Some(p) = theme.palette.get_mut(i) {
                *p = c;
            }
        }
    }

    theme
}

pub fn rows(app: &App) -> Vec<Row> {
    let h = &app.timeline.header;
    let mut rows = vec![
        Row::Heading("Recording"),
        Row::Title,
        Row::Command,
        Row::IdleLimit,
        Row::Heading("Terminal"),
        Row::Cursor,
        Row::Theme,
        Row::Color(Slot::Foreground),
        Row::Color(Slot::Background),
    ];

    rows.extend((0..PALETTE_NAMES.len()).map(|i| Row::Color(Slot::Palette(i))));
    rows.push(Row::Heading("Recorded (read-only)"));

    let format = match app.timeline.source_version {
        Some(v) if v == 3 => "asciicast v3".to_owned(),
        Some(v) => format!("asciicast v{v} (renders as v3)"),
        None => "asciicast v3".to_owned(),
    };

    rows.push(Row::Info("Format".to_owned(), format));
    rows.push(Row::Info(
        "Size".to_owned(),
        format!("{}×{}", h.term_cols, h.term_rows),
    ));
    rows.push(Row::Info(
        "Terminal".to_owned(),
        h.term_type.clone().unwrap_or_else(|| "unknown".to_owned()),
    ));
    rows.push(Row::Info(
        "Recorded at".to_owned(),
        h.timestamp.map_or_else(|| "unknown".to_owned(), format_utc),
    ));

    let mut env: Vec<_> = h.env.iter().flatten().collect();
    env.sort();

    for (k, v) in env {
        rows.push(Row::Info(format!("${k}"), v.clone()));
    }

    rows
}

/// Formats a Unix timestamp as `YYYY-MM-DD HH:MM:SS UTC`.
pub fn format_utc(ts: u64) -> String {
    // Howard Hinnant's civil_from_days
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

// ---- opening & live preview ---------------------------------------------

pub fn open(app: &mut App) {
    let rows = rows(app);
    let first = rows.iter().position(Row::selectable).unwrap_or(0);

    app.meta = Some(Panel {
        selected: first,
        scroll: Cell::new(0),
        mode: Mode::List,
        close_when_done: false,
    });
}

pub fn open_cursor_picker(app: &mut App) {
    let rows = rows(app);
    let selected = rows.iter().position(|r| *r == Row::Cursor).unwrap_or(0);
    let current = CURSOR_OPTIONS
        .iter()
        .position(|(s, _)| *s == app.timeline.header.term_cursor)
        .unwrap_or(0);

    app.meta = Some(Panel {
        selected,
        scroll: Cell::new(0),
        mode: Mode::Cursor(current),
        close_when_done: true,
    });
}

fn current_theme(app: &App) -> TtyTheme {
    app.timeline
        .header
        .term_theme
        .clone()
        .unwrap_or_else(default_theme)
}

fn preview_color(app: &mut App, slot: Slot, c: RGB8) {
    let theme = with_color(current_theme(app), slot, c);
    app.timeline.header.term_theme = Some(theme);
}

fn preview_cursor(app: &mut App, shape: Option<CursorShape>) {
    app.timeline.header.term_cursor = shape;
}

/// Drops any uncommitted preview, back to what the edit list says.
fn revert_preview(app: &mut App) {
    app.timeline.apply_edits(&app.edits);
}

/// Leaves a picker: back to the list, or closes the panel if opened directly.
fn picker_done(app: &mut App) {
    if let Some(panel) = &mut app.meta {
        if panel.close_when_done {
            app.meta = None;
        } else {
            panel.mode = Mode::List;
        }
    }
}

fn commit_theme(app: &mut App, theme: Option<TtyTheme>) {
    let theme = theme.as_ref().map(ThemeColors::from_tty);
    app.commit(Edit::SetTheme { theme });
}

// ---- list actions ---------------------------------------------------------

fn edit_row(app: &mut App, row: &Row) {
    match row {
        Row::Title => app.open_prompt(PromptKind::Title),
        Row::Command => app.open_prompt(PromptKind::Command),
        Row::IdleLimit => app.open_prompt(PromptKind::IdleLimit),

        Row::Cursor => {
            let current = CURSOR_OPTIONS
                .iter()
                .position(|(s, _)| *s == app.timeline.header.term_cursor)
                .unwrap_or(0);

            if let Some(p) = &mut app.meta {
                p.mode = Mode::Cursor(current);
            }
        }

        Row::Color(slot) => {
            let initial = theme_color(&current_theme(app), *slot);
            let recorded = app
                .timeline
                .original_header()
                .term_theme
                .as_ref()
                .map(|t| theme_color(t, *slot));

            if let Some(p) = &mut app.meta {
                p.mode = Mode::Color(ColorPicker::new(*slot, initial, recorded));
            }
        }

        Row::Theme => {
            let current = app
                .timeline
                .header
                .term_theme
                .as_ref()
                .and_then(themes::matching)
                .and_then(|name| PRESETS.iter().position(|p| p.name == name))
                .unwrap_or(0);

            if let Some(p) = &mut app.meta {
                p.mode = Mode::Preset(current);
            }

            preview_preset(app, current);
        }

        Row::Info(..) => app.info("That's recorded information and can't be edited"),
        Row::Heading(_) => {}
    }
}

/// `d`: clear a field, or for a color, reset it to the recorded one.
fn clear_row(app: &mut App, row: &Row) {
    match row {
        Row::Title => app.commit(Edit::SetTitle { title: None }),
        Row::Command => app.commit(Edit::SetCommand { command: None }),
        Row::IdleLimit => app.commit(Edit::SetIdleTimeLimit { secs: None }),
        Row::Cursor => app.commit(Edit::SetCursor { shape: None }),

        Row::Color(slot) => match app.timeline.original_header().term_theme.clone() {
            Some(recorded) => {
                let theme = with_color(current_theme(app), *slot, theme_color(&recorded, *slot));
                commit_theme(app, Some(theme));
            }

            // nothing recorded to go back to: drop the custom theme entirely
            None => commit_theme(app, None),
        },

        // back to whatever theme was recorded (or none)
        Row::Theme => {
            let recorded = app.timeline.original_header().term_theme.clone();
            commit_theme(app, recorded);
        }

        Row::Info(..) | Row::Heading(_) => {}
    }
}

fn move_selection(app: &mut App, delta: isize) {
    let rows = rows(app);
    let Some(panel) = &mut app.meta else { return };
    let mut i = panel.selected;

    for _ in 0..delta.unsigned_abs() {
        let next = if delta < 0 {
            (0..i).rev().find(|&j| rows[j].selectable())
        } else {
            (i + 1..rows.len()).find(|&j| rows[j].selectable())
        };

        match next {
            Some(j) => i = j,
            None => break,
        }
    }

    panel.selected = i;
}

// ---- keys -------------------------------------------------------------------

pub fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(panel) = &app.meta else { return };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match panel.mode.clone() {
        Mode::List => {
            let rows = rows(app);
            let row = rows.get(panel.selected).cloned();

            match key.code {
                KeyCode::Esc | KeyCode::Char('m') => app.meta = None,
                KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
                KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
                KeyCode::PageUp => move_selection(app, -10),
                KeyCode::PageDown => move_selection(app, 10),
                KeyCode::Home => move_selection(app, -1000),
                KeyCode::End => move_selection(app, 1000),
                KeyCode::Enter => {
                    if let Some(r) = row {
                        edit_row(app, &r)
                    }
                }
                KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                    if let Some(r) = row {
                        clear_row(app, &r)
                    }
                }
                KeyCode::Char('u') => app.undo(),
                KeyCode::Char('r') if ctrl => app.redo(),
                _ => {}
            }
        }

        Mode::Preset(i) => match key.code {
            KeyCode::Up | KeyCode::Char('k') => set_preset_choice(app, i.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => set_preset_choice(app, i + 1),

            KeyCode::Enter => {
                revert_preview(app);
                commit_theme(app, Some(PRESETS[i].theme()));
                picker_done(app);
            }

            KeyCode::Esc => {
                revert_preview(app);
                picker_done(app);
            }

            _ => {}
        },

        Mode::Cursor(i) => match key.code {
            KeyCode::Up | KeyCode::Char('k') => set_cursor_choice(app, i.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => set_cursor_choice(app, i + 1),

            KeyCode::Enter => {
                app.commit(Edit::SetCursor {
                    shape: CURSOR_OPTIONS[i].0,
                });
                picker_done(app);
            }

            KeyCode::Esc => {
                revert_preview(app);
                picker_done(app);
            }

            _ => {}
        },

        Mode::Color(mut picker) => {
            if let Some(input) = &mut picker.hex_input {
                match key.code {
                    KeyCode::Char(c) if c.is_ascii_hexdigit() || c == '#' => {
                        if input.len() < 7 {
                            input.push(c)
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Enter => {
                        if !picker.apply_hex() {
                            app.error("Not a hex color - use #rrggbb or #rgb");
                        }
                    }
                    KeyCode::Esc => picker.hex_input = None,
                    _ => {}
                }
            } else {
                let step = if shift { 10.0 } else { 1.0 };
                let slider = Slider::ALL.get(picker.focus).copied();

                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        picker.focus = picker.focus.saturating_sub(1)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        picker.focus = (picker.focus + 1).min(HEX_ROW)
                    }

                    KeyCode::Left | KeyCode::Char('h') => {
                        if let Some(s) = slider {
                            picker.adjust(s, -step)
                        }
                    }

                    KeyCode::Right | KeyCode::Char('l') => {
                        if let Some(s) = slider {
                            picker.adjust(s, step)
                        }
                    }

                    KeyCode::Char('#') => {
                        picker.focus = HEX_ROW;
                        picker.hex_input = Some(String::new());
                    }

                    KeyCode::Enter if picker.focus == HEX_ROW => {
                        picker.hex_input = Some(picker.hex());
                    }

                    KeyCode::Enter => {
                        let theme = with_color(current_theme(app), picker.slot, picker.rgb);
                        // the live preview already changed the header; reset
                        // it so the committed edit is what gets applied
                        revert_preview(app);
                        commit_theme(app, Some(theme));
                        picker_done(app);
                        return;
                    }

                    KeyCode::Esc => {
                        revert_preview(app);
                        picker_done(app);
                        return;
                    }

                    _ => {}
                }
            }

            let (slot, rgb) = (picker.slot, picker.rgb);

            if let Some(p) = &mut app.meta {
                p.mode = Mode::Color(picker);
            }

            preview_color(app, slot, rgb);
        }
    }
}

fn preview_preset(app: &mut App, i: usize) {
    app.timeline.header.term_theme = Some(PRESETS[i].theme());
}

fn set_preset_choice(app: &mut App, i: usize) {
    let i = i.min(PRESETS.len() - 1);

    if let Some(p) = &mut app.meta {
        p.mode = Mode::Preset(i);
    }

    preview_preset(app, i);
}

fn set_cursor_choice(app: &mut App, i: usize) {
    let i = i.min(CURSOR_OPTIONS.len() - 1);

    if let Some(p) = &mut app.meta {
        p.mode = Mode::Cursor(i);
    }

    preview_cursor(app, CURSOR_OPTIONS[i].0);
}

// ---- layout (shared by drawing and mouse hit-testing) ---------------------

/// Visible list rows: `(row index, y)`.
fn list_layout(panel: &Panel, inner: Rect, count: usize) -> Vec<(usize, u16)> {
    let h = inner.height as usize;

    if h == 0 {
        return Vec::new();
    }

    let mut scroll = panel.scroll.get().min(count.saturating_sub(h));

    if panel.selected < scroll {
        scroll = panel.selected;
    } else if panel.selected >= scroll + h {
        scroll = panel.selected + 1 - h;
    }

    // keep the heading above the first selectable row visible
    if scroll == 1 && panel.selected == 1 {
        scroll = 0;
    }

    panel.scroll.set(scroll);

    (scroll..count.min(scroll + h))
        .enumerate()
        .map(|(n, i)| (i, inner.y + n as u16))
        .collect()
}

/// First row of the color picker's sliders, relative to the panel's inner area.
const SLIDERS_TOP: u16 = 3;

/// Slider bar geometry: `(x, width)`.
fn slider_bar(inner: Rect) -> (u16, u16) {
    let x = inner.x + LABEL_W;
    let w = inner.width.saturating_sub(LABEL_W + 6).max(1);

    (x, w)
}

const CURSOR_TOP: u16 = 2;
const PRESET_TOP: u16 = 2;

// ---- mouse ------------------------------------------------------------------

pub fn handle_mouse(app: &mut App, mouse: MouseEvent, inner: Rect) {
    let Some(panel) = &app.meta else { return };
    let (x, y) = (mouse.column, mouse.row);

    match (panel.mode.clone(), mouse.kind) {
        (Mode::List, MouseEventKind::Down(MouseButton::Left)) => {
            let rows = rows(app);
            let layout = list_layout(panel, inner, rows.len());

            if let Some(&(i, _)) = layout.iter().find(|(_, ry)| *ry == y) {
                if rows[i].selectable() {
                    let already = panel.selected == i;

                    if let Some(p) = &mut app.meta {
                        p.selected = i;
                    }

                    if already {
                        edit_row(app, &rows[i]);
                    }
                }
            }
        }

        (Mode::List, MouseEventKind::ScrollUp) => move_selection(app, -1),
        (Mode::List, MouseEventKind::ScrollDown) => move_selection(app, 1),

        (
            Mode::Color(mut picker),
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left),
        ) => {
            let row = y.checked_sub(inner.y + SLIDERS_TOP).map(usize::from);
            let (bx, bw) = slider_bar(inner);

            if let Some(s) = row.and_then(|r| Slider::ALL.get(r).copied()) {
                if x >= bx && x < bx + bw {
                    let t = (x - bx) as f64 / (bw.max(2) - 1) as f64;
                    picker.focus = row.unwrap_or(0);
                    picker.set(s, t * s.max());

                    let (slot, rgb) = (picker.slot, picker.rgb);

                    if let Some(p) = &mut app.meta {
                        p.mode = Mode::Color(picker);
                    }

                    preview_color(app, slot, rgb);
                }
            }
        }

        (Mode::Preset(current), MouseEventKind::Down(MouseButton::Left)) => {
            if let Some(i) = y
                .checked_sub(inner.y + PRESET_TOP)
                .map(usize::from)
                .filter(|&i| i < PRESETS.len())
            {
                if i == current {
                    revert_preview(app);
                    commit_theme(app, Some(PRESETS[i].theme()));
                    picker_done(app);
                } else {
                    set_preset_choice(app, i);
                }
            }
        }

        (Mode::Preset(i), MouseEventKind::ScrollUp) => set_preset_choice(app, i.saturating_sub(1)),
        (Mode::Preset(i), MouseEventKind::ScrollDown) => set_preset_choice(app, i + 1),

        (Mode::Cursor(current), MouseEventKind::Down(MouseButton::Left)) => {
            if let Some(i) = y
                .checked_sub(inner.y + CURSOR_TOP)
                .map(usize::from)
                .filter(|&i| i < CURSOR_OPTIONS.len())
            {
                if i == current {
                    app.commit(Edit::SetCursor {
                        shape: CURSOR_OPTIONS[i].0,
                    });
                    picker_done(app);
                } else {
                    set_cursor_choice(app, i);
                }
            }
        }

        _ => {}
    }
}

// ---- drawing ----------------------------------------------------------------

fn rgb_color(c: RGB8) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

fn swatch(c: RGB8) -> Span<'static> {
    Span::styled("███", Style::default().fg(rgb_color(c)))
}

fn edited_mark(edited: bool) -> Span<'static> {
    if edited {
        Span::styled(" *", Style::default().fg(ACCENT))
    } else {
        Span::raw("")
    }
}

fn text_or_none(v: Option<String>) -> Span<'static> {
    match v {
        Some(v) => Span::raw(v),
        None => Span::styled("(none)", Style::default().fg(DIM)),
    }
}

fn label(s: &str) -> Span<'static> {
    Span::styled(
        format!("{s:<w$}", w = LIST_LABEL_W as usize),
        Style::default().fg(Color::Gray),
    )
}

fn row_line(row: &Row, h: &Header, original: &Header) -> Line<'static> {
    match row {
        Row::Heading(t) => Line::from(Span::styled(
            t.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),

        Row::Title => Line::from(vec![
            label("Title"),
            text_or_none(h.title.clone()),
            edited_mark(h.title != original.title),
        ]),

        Row::Command => Line::from(vec![
            label("Command"),
            text_or_none(h.command.clone()),
            edited_mark(h.command != original.command),
        ]),

        Row::IdleLimit => Line::from(vec![
            label("Idle limit"),
            text_or_none(h.idle_time_limit.map(|s| format!("{s}s"))),
            edited_mark(h.idle_time_limit != original.idle_time_limit),
        ]),

        Row::Cursor => {
            let name = CURSOR_OPTIONS
                .iter()
                .find(|(s, _)| *s == h.term_cursor)
                .map_or("?", |(_, n)| n);

            Line::from(vec![
                label("Cursor"),
                Span::raw(name.to_owned()),
                edited_mark(h.term_cursor != original.term_cursor),
            ])
        }

        Row::Theme => {
            let value = match &h.term_theme {
                Some(t) => Span::raw(themes::matching(t).unwrap_or("custom").to_owned()),
                None => Span::styled("(terminal's own)", Style::default().fg(DIM)),
            };

            let edited = match (&h.term_theme, &original.term_theme) {
                (Some(a), Some(b)) => a.fg != b.fg || a.bg != b.bg || a.palette != b.palette,
                (None, None) => false,
                _ => true,
            };

            Line::from(vec![label("Theme"), value, edited_mark(edited)])
        }

        Row::Color(slot) => {
            let name = match slot {
                Slot::Foreground => "Foreground".to_owned(),
                Slot::Background => "Background".to_owned(),
                Slot::Palette(i) => format!("{i:>2} {}", PALETTE_NAMES[*i]),
            };

            let now = h.term_theme.as_ref().map(|t| theme_color(t, *slot));
            let was = original.term_theme.as_ref().map(|t| theme_color(t, *slot));

            let mut spans = vec![label(&name)];

            match now {
                Some(c) => {
                    spans.push(swatch(c));
                    spans.push(Span::raw(format!(" {}", to_hex(c))));
                }

                None => spans.push(Span::styled("(terminal's own)", Style::default().fg(DIM))),
            }

            spans.push(edited_mark(now != was));

            Line::from(spans)
        }

        Row::Info(k, v) => Line::from(vec![
            Span::styled(
                format!("{k:<w$}", w = LIST_LABEL_W as usize),
                Style::default().fg(DIM),
            ),
            Span::styled(v.clone(), Style::default().fg(DIM)),
        ]),
    }
}

/// Draws the panel; returns its inner area for mouse handling.
pub fn draw(buf: &mut Buffer, area: Rect, app: &App) -> Rect {
    let Some(panel) = &app.meta else {
        return Rect::default();
    };

    let title = match &panel.mode {
        Mode::List => " metadata ".to_owned(),
        Mode::Color(p) => format!(" color: {} ", p.slot.label().trim()),
        Mode::Cursor(_) => " cursor shape ".to_owned(),
        Mode::Preset(_) => " theme presets ".to_owned(),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(title);

    let inner = block.inner(area);
    ratatui::widgets::Widget::render(ratatui::widgets::Clear, area, buf);
    ratatui::widgets::Widget::render(block, area, buf);

    match &panel.mode {
        Mode::List => draw_list(buf, inner, app, panel),
        Mode::Color(p) => draw_color(buf, inner, p),
        Mode::Cursor(i) => draw_cursor(buf, inner, app, *i),
        Mode::Preset(i) => draw_presets(buf, inner, app, *i),
    }

    inner
}

fn render_line(buf: &mut Buffer, x: u16, y: u16, width: u16, line: Line<'_>) {
    ratatui::widgets::Widget::render(Paragraph::new(line), Rect::new(x, y, width, 1), buf);
}

fn draw_list(buf: &mut Buffer, inner: Rect, app: &App, panel: &Panel) {
    let rows = rows(app);
    let h = &app.timeline.header;
    let original = app.timeline.original_header();

    for (i, y) in list_layout(panel, inner, rows.len()) {
        let mut line = row_line(&rows[i], h, original);

        if i == panel.selected {
            line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
            line.spans
                .insert(0, Span::styled("▶", Style::default().fg(KEY)));
        } else {
            line.spans.insert(0, Span::raw(" "));
        }

        render_line(buf, inner.x, y, inner.width, line);
    }
}

fn draw_color(buf: &mut Buffer, inner: Rect, p: &ColorPicker) {
    let w = inner.width;

    // big swatch of the new color next to the one it replaces
    render_line(
        buf,
        inner.x,
        inner.y,
        w,
        Line::from(vec![
            Span::styled("██████", Style::default().fg(rgb_color(p.rgb))),
            Span::styled("███", Style::default().fg(rgb_color(p.initial))),
            Span::raw(format!(" {}  ", p.hex())),
            Span::styled(
                format!("was {}", to_hex(p.initial)),
                Style::default().fg(DIM),
            ),
        ]),
    );

    if let Some(rec) = p.recorded {
        render_line(
            buf,
            inner.x,
            inner.y + 1,
            w,
            Line::from(vec![
                Span::styled("recorded ", Style::default().fg(DIM)),
                swatch(rec),
                Span::styled(format!(" {}", to_hex(rec)), Style::default().fg(DIM)),
            ]),
        );
    }

    let (bx, bw) = slider_bar(inner);

    for (row, s) in Slider::ALL.iter().enumerate() {
        let y = inner.y + SLIDERS_TOP + row as u16;

        if y >= inner.bottom() {
            break;
        }

        let focused = p.focus == row;
        let label_style = if focused {
            Style::default().fg(KEY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        let marker = if focused { "▶" } else { " " };
        render_line(
            buf,
            inner.x,
            y,
            LABEL_W,
            Line::from(Span::styled(format!("{marker}{}", s.label()), label_style)),
        );

        // gradient: each cell shows the color that value would give
        let value = p.value(*s);
        let knob = ((value / s.max()) * (bw.max(2) - 1) as f64).round() as u16;

        for i in 0..bw {
            let t = i as f64 / (bw.max(2) - 1) as f64;
            let c = rgb_color(p.color_at(*s, t));

            if let Some(cell) = buf.cell_mut((bx + i, y)) {
                if i == knob {
                    // knob: contrasting marker over the gradient
                    cell.set_char('┃').set_style(
                        Style::default()
                            .fg(Color::White)
                            .bg(c)
                            .add_modifier(Modifier::BOLD),
                    );
                } else {
                    cell.set_char(' ').set_style(Style::default().bg(c));
                }
            }
        }

        render_line(
            buf,
            bx + bw + 1,
            y,
            5,
            Line::from(format!("{:>3}{}", value.round() as i64, s.unit())),
        );
    }

    let y = inner.y + SLIDERS_TOP + HEX_ROW as u16;

    if y < inner.bottom() {
        let focused = p.focus == HEX_ROW;
        let marker = if focused { "▶" } else { " " };
        let style = if focused {
            Style::default().fg(KEY).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        let value = match &p.hex_input {
            Some(input) => Span::styled(format!("{input}▏"), Style::default().fg(ACCENT)),
            None => Span::raw(p.hex()),
        };

        render_line(
            buf,
            inner.x,
            y,
            w,
            Line::from(vec![
                Span::styled(
                    format!("{marker}{:<w$}", "Hex", w = LABEL_W as usize - 1),
                    style,
                ),
                value,
            ]),
        );
    }
}

fn draw_presets(buf: &mut Buffer, inner: Rect, app: &App, selected: usize) {
    render_line(
        buf,
        inner.x,
        inner.y,
        inner.width,
        Line::from(Span::styled(
            "Previewed as you move; tweak colours after",
            Style::default().fg(DIM),
        )),
    );

    let recorded = app
        .timeline
        .original_header()
        .term_theme
        .as_ref()
        .and_then(themes::matching);

    for (i, preset) in PRESETS.iter().enumerate() {
        let y = inner.y + PRESET_TOP + i as u16;

        if y >= inner.bottom() {
            break;
        }

        let theme = preset.theme();
        let bg = rgb_color(theme.bg);

        // the 16 palette colours on the theme's background, then a text sample
        let mut spans = vec![Span::raw(if i == selected { "▶ " } else { "  " })];

        for &c in &theme.palette {
            spans.push(Span::styled("▆", Style::default().fg(rgb_color(c)).bg(bg)));
        }

        spans.push(Span::styled(
            " Aa ",
            Style::default().fg(rgb_color(theme.fg)).bg(bg),
        ));

        let mut name = Span::raw(format!(" {}", preset.name));

        if i == selected {
            name = name.style(Style::default().fg(KEY).add_modifier(Modifier::BOLD));
        }

        spans.push(name);

        if recorded == Some(preset.name) {
            spans.push(Span::styled(" (recorded)", Style::default().fg(DIM)));
        }

        render_line(buf, inner.x, y, inner.width, Line::from(spans));
    }
}

fn draw_cursor(buf: &mut Buffer, inner: Rect, app: &App, selected: usize) {
    render_line(
        buf,
        inner.x,
        inner.y,
        inner.width,
        Line::from(Span::styled(
            "Shown in the preview as you move",
            Style::default().fg(DIM),
        )),
    );

    let recorded = app.timeline.original_header().term_cursor;

    for (i, (shape, name)) in CURSOR_OPTIONS.iter().enumerate() {
        let y = inner.y + CURSOR_TOP + i as u16;

        if y >= inner.bottom() {
            break;
        }

        let blinking = matches!(
            shape,
            Some(
                CursorShape::BlinkingBlock
                    | CursorShape::BlinkingUnderline
                    | CursorShape::BlinkingBar
            )
        );

        let mut sample_style = Style::default().fg(Color::White);

        if blinking {
            sample_style = sample_style.add_modifier(Modifier::SLOW_BLINK);
        }

        let sample = match shape {
            None => Span::styled(" ", sample_style),
            Some(CursorShape::Block | CursorShape::BlinkingBlock) => {
                Span::styled("█", sample_style)
            }
            Some(CursorShape::Underline | CursorShape::BlinkingUnderline) => {
                Span::styled("▁", sample_style)
            }
            Some(_) => Span::styled("▏", sample_style),
        };

        let mut line = Line::from(vec![
            Span::raw(if i == selected { "▶ " } else { "  " }),
            sample,
            Span::raw(format!("  {name}")),
            Span::styled(
                if *shape == recorded {
                    "  (recorded)"
                } else {
                    ""
                },
                Style::default().fg(DIM),
            ),
        ]);

        if i == selected {
            line = line.patch_style(Style::default().fg(KEY).add_modifier(Modifier::BOLD));
        }

        render_line(buf, inner.x, y, inner.width, line);
    }
}

/// Key hints for the bottom line while the panel is open.
pub fn hints(panel: &Panel) -> &'static [(&'static str, &'static str)] {
    match &panel.mode {
        Mode::List => &[
            ("↑↓", "select"),
            ("enter", "edit"),
            ("d", "clear / reset colour"),
            ("u", "undo"),
            ("esc", "close"),
        ],

        Mode::Color(p) if p.hex_input.is_some() => &[
            ("0-9a-f", "type hex"),
            ("enter", "use it"),
            ("esc", "stop typing"),
        ],

        Mode::Color(_) => &[
            ("↑↓", "slider"),
            ("←→", "adjust (⇧ ×10)"),
            ("#", "type hex"),
            ("enter", "apply"),
            ("esc", "cancel"),
        ],

        Mode::Cursor(_) | Mode::Preset(_) => {
            &[("↑↓", "choose"), ("enter", "apply"), ("esc", "cancel")]
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rgb::RGB8;

    use super::{format_utc, handle_key, open, open_cursor_picker, rows, Mode, Row};
    use crate::asciicast::CursorShape;
    use crate::edit::tui::app::tests::app;
    use crate::edit::tui::color::Slot;
    use crate::edit::Edit;

    fn key(app: &mut crate::edit::tui::app::App, code: KeyCode) {
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn select(app: &mut crate::edit::tui::app::App, row: Row) {
        let i = rows(app).iter().position(|r| *r == row).unwrap();
        app.meta.as_mut().unwrap().selected = i;
    }

    #[test]
    fn timestamps() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(1793869492), "2026-11-05 09:04:52 UTC");
        assert_eq!(format_utc(951782400), "2000-02-29 00:00:00 UTC");
    }

    #[test]
    fn cursor_picker_previews_then_commits() {
        let (mut a, _d) = app();
        open_cursor_picker(&mut a);

        key(&mut a, KeyCode::Down); // Block, blinking
        assert_eq!(
            a.timeline.header.term_cursor,
            Some(CursorShape::BlinkingBlock)
        );
        assert!(a.edits.edits.is_empty(), "preview only");

        key(&mut a, KeyCode::Down); // Block, steady
        key(&mut a, KeyCode::Esc);
        assert_eq!(a.timeline.header.term_cursor, None, "esc reverts");
        assert!(a.meta.is_none(), "opened with C: closes when done");

        open_cursor_picker(&mut a);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Enter);
        assert_eq!(
            a.edits.edits,
            [Edit::SetCursor {
                shape: Some(CursorShape::BlinkingBlock)
            }]
        );
    }

    #[test]
    fn color_picker_previews_commits_and_resets() {
        let (mut a, _d) = app(); // no recorded theme
        open(&mut a);
        select(&mut a, Row::Color(Slot::Palette(1)));
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.meta.as_ref().unwrap().mode, Mode::Color(_)));

        // type a hex color
        key(&mut a, KeyCode::Char('#'));
        for c in "00ff00".chars() {
            key(&mut a, KeyCode::Char(c));
        }
        key(&mut a, KeyCode::Enter);

        let red = a.timeline.header.term_theme.as_ref().unwrap().palette[1];
        assert_eq!(red, RGB8::new(0, 255, 0), "previewed live");
        assert!(a.edits.edits.is_empty());

        key(&mut a, KeyCode::Up); // to Blue slider
        key(&mut a, KeyCode::Right); // blue 0 -> 1
        key(&mut a, KeyCode::Enter); // apply

        let theme = a.timeline.header.term_theme.as_ref().unwrap();
        assert_eq!(theme.palette[1], RGB8::new(0, 255, 1));
        assert_eq!(
            theme.palette[0],
            RGB8::new(0, 0, 0),
            "other colors from the default theme"
        );
        assert_eq!(a.edits.edits.len(), 1);
        assert!(matches!(a.meta.as_ref().unwrap().mode, Mode::List));

        // d on a color with no recorded theme removes the custom theme
        key(&mut a, KeyCode::Char('d'));
        assert!(a.timeline.header.term_theme.is_none());

        a.undo();
        assert!(a.timeline.header.term_theme.is_some());
    }

    #[test]
    fn color_picker_escape_reverts() {
        let (mut a, _d) = app();
        open(&mut a);
        select(&mut a, Row::Color(Slot::Background));
        key(&mut a, KeyCode::Enter);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down); // lightness
        for _ in 0..5 {
            key(&mut a, KeyCode::Right);
        }
        assert!(a.timeline.header.term_theme.is_some());

        key(&mut a, KeyCode::Esc);
        assert!(a.timeline.header.term_theme.is_none());
        assert!(a.edits.edits.is_empty());
    }

    #[test]
    fn theme_presets() {
        use crate::edit::themes::{matching, PRESETS};

        let (mut a, _d) = app(); // no recorded theme
        open(&mut a);
        select(&mut a, Row::Theme);
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.meta.as_ref().unwrap().mode, Mode::Preset(0)));
        assert!(
            a.timeline.header.term_theme.is_some(),
            "first preset previewed"
        );

        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down);
        let previewed = a.timeline.header.term_theme.clone().unwrap();
        assert_eq!(matching(&previewed), Some(PRESETS[2].name));
        assert!(a.edits.edits.is_empty());

        key(&mut a, KeyCode::Esc);
        assert!(a.timeline.header.term_theme.is_none(), "esc reverts");

        key(&mut a, KeyCode::Enter);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Enter);
        let applied = a.timeline.header.term_theme.clone().unwrap();
        assert_eq!(matching(&applied), Some(PRESETS[1].name));
        assert_eq!(a.edits.edits.len(), 1);

        // tweaking one colour afterwards makes it "custom" but keeps the rest
        select(&mut a, Row::Color(Slot::Palette(1)));
        key(&mut a, KeyCode::Enter);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Right);
        key(&mut a, KeyCode::Enter);
        let tweaked = a.timeline.header.term_theme.clone().unwrap();
        assert_eq!(matching(&tweaked), None);
        assert_eq!(tweaked.bg, applied.bg);
        assert_eq!(a.edits.edits.len(), 1, "theme edits coalesce");

        // d on the Theme row goes back to the recorded theme (none here)
        select(&mut a, Row::Theme);
        key(&mut a, KeyCode::Char('d'));
        assert!(a.timeline.header.term_theme.is_none());
    }

    #[test]
    fn text_fields_use_prompts() {
        let (mut a, _d) = app();
        open(&mut a);
        select(&mut a, Row::Title);
        key(&mut a, KeyCode::Enter);
        a.prompt.as_mut().unwrap().input = "My demo".to_owned();
        a.submit_prompt();
        assert_eq!(a.timeline.header.title.as_deref(), Some("My demo"));

        select(&mut a, Row::IdleLimit);
        key(&mut a, KeyCode::Enter);
        a.prompt.as_mut().unwrap().input = "1.5".to_owned();
        a.submit_prompt();
        assert_eq!(a.timeline.header.idle_time_limit, Some(1.5));

        key(&mut a, KeyCode::Char('d'));
        assert_eq!(a.timeline.header.idle_time_limit, None);
    }

    #[test]
    fn navigation_skips_headings() {
        let (mut a, _d) = app();
        open(&mut a);
        assert_eq!(rows(&a)[a.meta.as_ref().unwrap().selected], Row::Title);

        key(&mut a, KeyCode::Up);
        assert_eq!(rows(&a)[a.meta.as_ref().unwrap().selected], Row::Title);

        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down); // past the "Terminal" heading
        assert_eq!(rows(&a)[a.meta.as_ref().unwrap().selected], Row::Cursor);
    }
}
