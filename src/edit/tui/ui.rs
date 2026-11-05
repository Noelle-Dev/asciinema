//! Drawing the editor.

use std::time::Duration;

use avt::Vt;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::app::{App, Drag, TimelineMode};
use crate::asciicast::{CursorShape, EventData};
use crate::edit::timeline::Origin;
use crate::tty::TtyTheme;

/// Rows inside the timeline box: ruler, bars, ticks, playhead/markers.
pub const TIMELINE_ROWS: u16 = 4;
pub const TICK_ROW: u16 = 2;

const ACCENT: Color = Color::Yellow;
const PLAYHEAD: Color = Color::Cyan;
const MARKER: Color = Color::Magenta;
const SELECTION_BG: Color = Color::Indexed(24);
const CLIP: Color = Color::Magenta;
const CUT: Color = Color::Red;
const DIM: Color = Color::DarkGray;
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Screen regions from the last draw, used for mouse hit-testing.
#[derive(Debug, Default, Clone, Copy)]
pub struct Areas {
    pub preview: Rect,
    /// Inside of the timeline box (empty in full screen).
    pub timeline: Rect,
    /// Clickable play/pause button in the status bar (empty in full screen).
    pub play_button: Rect,
    /// Inside of the metadata panel, when open.
    pub meta: Rect,
}

pub fn draw(frame: &mut Frame, app: &App) -> Areas {
    let area = frame.area();

    if app.fullscreen {
        draw_preview(frame.buffer_mut(), area, app);

        return Areas {
            preview: area,
            ..Default::default()
        };
    }

    let [preview_box, timeline_box, status, help] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(TIMELINE_ROWS + 2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // The metadata panel sits beside the preview so changes are visible live;
    // on narrow terminals it takes the preview's place.
    let (preview_box, meta_box) = if app.meta.is_some() {
        if preview_box.width >= super::meta::PANEL_WIDTH + 24 {
            let [p, m] = Layout::horizontal([
                Constraint::Min(20),
                Constraint::Length(super::meta::PANEL_WIDTH),
            ])
            .areas(preview_box);

            (p, Some(m))
        } else {
            (Rect::default(), Some(preview_box))
        }
    } else {
        (preview_box, None)
    };

    let preview_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .title(preview_title(app, preview_box))
        .title_bottom(Line::from(mode_title(app)).right_aligned());

    let preview = preview_block.inner(preview_box);
    frame.render_widget(preview_block, preview_box);
    draw_preview(frame.buffer_mut(), preview, app);

    let timeline_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .title(" timeline ");

    let timeline = timeline_block.inner(timeline_box);
    frame.render_widget(timeline_block, timeline_box);

    match app.mode {
        TimelineMode::Time => draw_time_view(frame.buffer_mut(), timeline, app),
        TimelineMode::Event => draw_event_view(frame.buffer_mut(), timeline, app),
    }

    frame.render_widget(Paragraph::new(status_line(app)), status);
    frame.render_widget(Paragraph::new(help_line(app)), help);

    if let Some(prompt) = &app.prompt {
        let label = app.prompt_label(prompt.kind);
        let x = help.x + (label.chars().count() + prompt.input.chars().count()) as u16;
        frame.set_cursor_position((x.min(help.right().saturating_sub(1)), help.y));
    }

    if app.show_help {
        draw_help(frame, area);
    }

    let play_button = Rect {
        width: (play_button_label(app).chars().count() as u16).min(status.width),
        ..status
    };

    let meta = meta_box.map_or(Rect::default(), |m| {
        super::meta::draw(frame.buffer_mut(), m, app)
    });

    Areas {
        preview,
        timeline,
        play_button,
        meta,
    }
}

// ---- preview ------------------------------------------------------------

fn preview_title(app: &App, area: Rect) -> String {
    // Don't hold the screen borrow: viewport_offset below needs it too.
    let (cols, rows) = app.vt().size();
    let name = app.path.file_name().map_or_else(
        || app.path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );

    let inner = (
        area.width.saturating_sub(2) as usize,
        area.height.saturating_sub(2) as usize,
    );
    let cropped = cols > inner.0 || rows > inner.1;

    let view = match (cropped, app.pan) {
        (false, _) => String::new(),
        (true, None) => " · cropped, auto-scrolling (hjkl to pan, f full screen)".to_owned(),
        (true, Some(_)) => {
            let (x, y) = app.viewport_offset(inner.0, inner.1);
            format!(" · panned to col {x}, row {y} (0: auto-scroll)")
        }
    };

    let cursor = app
        .timeline
        .header
        .term_cursor
        .map_or("default", CursorShape::name);

    format!(" {name} · {cols}×{rows} · cursor: {cursor} (C){view} ")
}

fn mode_title(app: &App) -> Span<'static> {
    let text = match app.mode {
        TimelineMode::Time => " time view (t: event view) ",
        TimelineMode::Event => " event view (t: time view) ",
    };

    Span::styled(text, Style::default().fg(DIM))
}

fn draw_preview(buf: &mut Buffer, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    let (w, h) = (area.width as usize, area.height as usize);
    let offset = app.viewport_offset(w, h);
    let vt = app.vt();
    draw_screen(
        buf,
        area,
        &vt,
        offset,
        app.timeline.header.term_theme.as_ref(),
        app.timeline.header.term_cursor,
    );
}

/// Paints the recorded terminal's cells into `area`, starting at `offset`.
pub fn draw_screen(
    buf: &mut Buffer,
    area: Rect,
    vt: &Vt,
    (ox, oy): (usize, usize),
    theme: Option<&TtyTheme>,
    cursor_shape: Option<CursorShape>,
) {
    let default_fg = theme.map_or(Color::Reset, |t| rgb(t.fg));
    let default_bg = theme.map_or(Color::Reset, |t| rgb(t.bg));
    let base = Style::default().fg(default_fg).bg(default_bg);
    let (cols, rows) = vt.size();

    buf.set_style(area, base);

    // Areas outside the recorded screen are shown dimmed.
    for y in 0..area.height {
        for x in 0..area.width {
            let (col, row) = (ox + x as usize, oy + y as usize);

            if col >= cols || row >= rows {
                if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                    cell.set_char('·')
                        .set_style(Style::default().fg(DIM).bg(Color::Reset));
                }
            }
        }
    }

    let cursor = vt.cursor();

    for (y, line) in vt.view().skip(oy).take(area.height as usize).enumerate() {
        let cells = line.cells();

        for x in 0..area.width as usize {
            let Some(c) = cells.get(ox + x) else { break };

            if c.width() == 0 {
                continue; // tail of a wide character, drawn by its head
            }

            let ch = if c.width() == 2 && x + 1 >= area.width as usize {
                ' ' // wide char cut off by the right edge
            } else {
                c.char()
            };

            let mut style = pen_style(c.pen(), theme, default_fg, default_bg);

            let mut ch = ch;

            if cursor.visible && cursor.col == ox + x && cursor.row == oy + y {
                (ch, style) = cursor_cell(ch, style, cursor_shape);
            }

            let pos = (area.x + x as u16, area.y + y as u16);

            if let Some(cell) = buf.cell_mut(pos) {
                cell.set_char(ch).set_style(style);
            }
        }
    }
}

/// How the recorded cursor is drawn in the preview, approximating its shape.
/// A terminal cell can't show a thin bar over a character, so a bar is drawn
/// as `▏` on blank cells (the usual case: the end of a line being typed) and
/// as a block otherwise.
fn cursor_cell(ch: char, style: Style, shape: Option<CursorShape>) -> (char, Style) {
    use CursorShape::*;

    let blink = matches!(shape, Some(BlinkingBlock | BlinkingUnderline | BlinkingBar));
    let style = if blink {
        style.add_modifier(Modifier::SLOW_BLINK)
    } else {
        style
    };

    match shape {
        Some(Underline | BlinkingUnderline) => (ch, style.add_modifier(Modifier::UNDERLINED)),
        Some(Bar | BlinkingBar) if ch == ' ' => ('▏', style),
        _ => (ch, style.add_modifier(Modifier::REVERSED)),
    }
}

fn pen_style(pen: &avt::Pen, theme: Option<&TtyTheme>, dfg: Color, dbg: Color) -> Style {
    let conv = |c: avt::Color| match c {
        avt::Color::Indexed(n) => match theme {
            Some(t) if (n as usize) < t.palette.len() && n < 16 => rgb(t.palette[n as usize]),
            _ => Color::Indexed(n),
        },

        avt::Color::RGB(c) => rgb(c),
    };

    let mut fg = pen.foreground().map_or(dfg, conv);
    let mut bg = pen.background().map_or(dbg, conv);

    if pen.is_inverse() {
        std::mem::swap(&mut fg, &mut bg);

        // Reset can't be swapped meaningfully without a theme; use reverse video.
        if fg == Color::Reset || bg == Color::Reset {
            std::mem::swap(&mut fg, &mut bg);

            return modifiers(pen, Style::default().fg(fg).bg(bg)).add_modifier(Modifier::REVERSED);
        }
    }

    modifiers(pen, Style::default().fg(fg).bg(bg))
}

fn modifiers(pen: &avt::Pen, mut style: Style) -> Style {
    let flags = [
        (pen.is_bold(), Modifier::BOLD),
        (pen.is_faint(), Modifier::DIM),
        (pen.is_italic(), Modifier::ITALIC),
        (pen.is_underline(), Modifier::UNDERLINED),
        (pen.is_strikethrough(), Modifier::CROSSED_OUT),
        (pen.is_blink(), Modifier::SLOW_BLINK),
    ];

    for (on, m) in flags {
        if on {
            style = style.add_modifier(m);
        }
    }

    style
}

fn rgb(c: rgb::RGB8) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

// ---- timeline: shared -----------------------------------------------------

fn put(buf: &mut Buffer, area: Rect, x: u16, row: u16, ch: char, style: Style) {
    if x < area.width && row < area.height {
        if let Some(cell) = buf.cell_mut((area.x + x, area.y + row)) {
            cell.set_char(ch).set_style(style);
        }
    }
}

fn put_str(buf: &mut Buffer, area: Rect, x: u16, row: u16, s: &str, style: Style) {
    for (i, ch) in s.chars().enumerate() {
        put(buf, area, x + i as u16, row, ch, style);
    }
}

fn in_selection(app: &App, event: usize) -> bool {
    app.selection()
        .is_some_and(|(a, b)| (a..=b).contains(&event))
}

/// Playlist item being dragged.
fn dragged_event(app: &App) -> Option<usize> {
    match app.drag {
        Some(Drag::Event { item, .. }) => Some(item),
        _ => None,
    }
}

fn is_clip(app: &App, i: usize) -> bool {
    matches!(app.timeline.origin(i), Some(Origin::Clip { .. }))
}

fn is_cut(app: &App, i: usize) -> bool {
    matches!(app.timeline.origin(i), Some(Origin::Cut { .. }))
}

// ---- timeline: time view --------------------------------------------------

/// Column for time `t` in a window, or None if outside.
pub fn col_of(t: Duration, (start, span): (Duration, Duration), width: u16) -> Option<u16> {
    if t < start || t > start + span || width == 0 {
        return None;
    }

    let frac = (t - start).as_secs_f64() / span.as_secs_f64().max(1e-9);

    Some(((frac * (width - 1) as f64).round() as u16).min(width - 1))
}

pub fn time_at_col(x: f64, (start, span): (Duration, Duration), width: u16) -> Duration {
    let frac = (x / (width.max(2) - 1) as f64).clamp(0.0, 1.0);

    start + span.mul_f64(frac)
}

/// First event drawn in column `x` of the time view.
pub fn event_in_col(app: &App, x: u16, window: (Duration, Duration), width: u16) -> Option<usize> {
    let lo = time_at_col(x as f64 - 0.5, window, width);
    let hi = time_at_col(x as f64 + 0.5, window, width);
    let first = app
        .timeline
        .position_at(lo.saturating_sub(Duration::from_nanos(1)));

    (first..app.len())
        .take_while(|&i| app.timeline.event(i).is_some_and(|(t, _)| t <= hi))
        .find(|&i| {
            app.timeline
                .event(i)
                .and_then(|(t, _)| col_of(t, window, width))
                == Some(x)
        })
}

fn draw_time_view(buf: &mut Buffer, area: Rect, app: &App) {
    if area.width < 2 || area.height < TIMELINE_ROWS {
        return;
    }

    let w = area.width;
    let window = app.time_window();
    let (start, span) = window;

    // ruler
    let step = nice_step(span.as_secs_f64() / (w as f64 / 12.0).max(1.0));
    let mut label_t = (start.as_secs_f64() / step).ceil() * step;

    while label_t <= (start + span).as_secs_f64() {
        if let Some(x) = col_of(Duration::from_secs_f64(label_t), window, w) {
            put(buf, area, x, 0, '╷', Style::default().fg(DIM));
            put_str(
                buf,
                area,
                x + 1,
                0,
                &fmt_label(label_t, step),
                Style::default().fg(DIM),
            );
        }

        label_t += step;
    }

    // ticks: count events per column
    let mut counts = vec![0u32; w as usize];
    let mut edited = vec![false; w as usize];
    let mut selected = vec![false; w as usize];
    let mut markers = vec![false; w as usize];
    let mut clip = vec![false; w as usize];
    let mut cut = vec![false; w as usize];
    let first = app
        .timeline
        .position_at(start.saturating_sub(Duration::from_nanos(1)));
    let dragged = dragged_event(app);
    let mut dragged_col = None;

    for i in first..app.len() {
        let Some((t, data)) = app.timeline.event(i) else {
            break;
        };

        if t > start + span {
            break;
        }

        // a clip covers the time from the item before it to itself
        if is_clip(app, i) {
            let prev = i
                .checked_sub(1)
                .and_then(|p| app.timeline.event(p))
                .map_or(Duration::ZERO, |(pt, _)| pt);
            let a = col_of(prev.max(start), window, w).unwrap_or(0) as usize;

            if let Some(b) = col_of(t, window, w) {
                for c in clip.iter_mut().take(b as usize + 1).skip(a) {
                    *c = true;
                }
            }
        }

        let Some(x) = col_of(t, window, w) else {
            continue;
        };
        let x = x as usize;
        cut[x] |= is_cut(app, i);
        counts[x] += 1;
        edited[x] |= app.timeline.is_retimed(i);
        selected[x] |= in_selection(app, i);
        markers[x] |= matches!(data, EventData::Marker(_));

        if dragged == Some(i) {
            dragged_col = Some(x);
        }
    }

    for x in 0..w as usize {
        let mut style = Style::default().fg(Color::Gray);

        if selected[x] {
            style = style.bg(SELECTION_BG);
            put(buf, area, x as u16, 1, ' ', style);
        }

        if clip[x] {
            put(buf, area, x as u16, 1, '▒', style.fg(CLIP));
        }

        if cut[x] {
            put(
                buf,
                area,
                x as u16,
                1,
                '✂',
                style.fg(CUT).add_modifier(Modifier::BOLD),
            );
        }

        if counts[x] > 1 {
            style = style.fg(Color::White).add_modifier(Modifier::BOLD);
        }

        if edited[x] {
            style = style.fg(ACCENT);
        }

        if dragged_col == Some(x) {
            style = style.fg(Color::Black).bg(ACCENT);
        }

        let ch = if counts[x] > 0 { '|' } else { ' ' };
        put(buf, area, x as u16, TICK_ROW, ch, style);

        if markers[x] {
            put(buf, area, x as u16, 3, '◆', Style::default().fg(MARKER));
        }
    }

    if let Some(x) = col_of(app.time, window, w) {
        put(
            buf,
            area,
            x,
            3,
            '▲',
            Style::default().fg(PLAYHEAD).add_modifier(Modifier::BOLD),
        );
    }
}

fn nice_step(raw: f64) -> f64 {
    let raw = raw.max(0.001);
    let mag = 10f64.powf(raw.log10().floor());

    [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * mag)
        .find(|&s| s >= raw)
        .unwrap_or(10.0 * mag)
}

fn fmt_label(t: f64, step: f64) -> String {
    if t >= 60.0 && step >= 1.0 {
        let t = t.round() as u64;
        format!("{}:{:02}", t / 60, t % 60)
    } else if step >= 1.0 {
        format!("{t:.0}s")
    } else if step >= 0.1 {
        format!("{t:.1}s")
    } else {
        format!("{t:.2}s")
    }
}

// ---- timeline: event view -------------------------------------------------

fn bar(delay: Duration) -> char {
    let d = delay.as_secs_f64();

    if d < 0.005 {
        return ' ';
    }

    // log scale: 10ms -> lowest bar, 10s and above -> full bar
    let level = ((d.log10() + 2.0) / 3.0 * 8.0).ceil().clamp(1.0, 8.0) as usize;

    BARS[level - 1]
}

fn event_glyph(data: &EventData) -> char {
    match data {
        EventData::Output(_) => '|',
        EventData::Input(_) => 'i',
        EventData::Resize(..) => '↔',
        EventData::Marker(_) => '◆',
        EventData::Exit(_) => '■',
        EventData::Other(..) => '?',
    }
}

fn draw_event_view(buf: &mut Buffer, area: Rect, app: &App) {
    if area.width < 2 || area.height < TIMELINE_ROWS {
        return;
    }

    let w = area.width as usize;
    let start = app.event_window(w);
    let dragged = dragged_event(app);

    for x in 0..w {
        let i = start + x;
        let Some((_, data)) = app.timeline.event(i) else {
            break;
        };

        // label original event numbers, which is what edits and the status refer to
        if let Some(Origin::Event(e)) = app.timeline.origin(i) {
            if e % 10 == 0 && x + 1 < w {
                put(buf, area, x as u16, 0, '╷', Style::default().fg(DIM));
                put_str(
                    buf,
                    area,
                    x as u16 + 1,
                    0,
                    &e.to_string(),
                    Style::default().fg(DIM),
                );
            }
        }

        let mut style = Style::default().fg(Color::Gray);

        if in_selection(app, i) {
            style = style.bg(SELECTION_BG);
        }

        if app.timeline.is_retimed(i) {
            style = style.fg(ACCENT);
        }

        if dragged == Some(i) {
            style = style.fg(Color::Black).bg(ACCENT);
        }

        let delay = app.timeline.delay(i).unwrap_or_default();
        put(buf, area, x as u16, 1, bar(delay), style);

        let (glyph, glyph_style) = match (app.timeline.origin(i), data) {
            (Some(Origin::Cut { .. }), _) => ('✂', style.fg(CUT).add_modifier(Modifier::BOLD)),
            (Some(Origin::Clip { .. }), _) => ('▒', style.fg(CLIP)),
            (_, EventData::Marker(_)) => (event_glyph(data), style.fg(MARKER)),
            _ => (event_glyph(data), style),
        };

        put(buf, area, x as u16, TICK_ROW, glyph, glyph_style);
    }

    if let Some(x) = app.position.checked_sub(start).filter(|&x| x < w) {
        put(
            buf,
            area,
            x as u16,
            3,
            '▲',
            Style::default().fg(PLAYHEAD).add_modifier(Modifier::BOLD),
        );
    }
}

// ---- status & help --------------------------------------------------------

fn secs(d: Duration) -> String {
    format!("{:.3}s", d.as_secs_f64())
}

/// The button shows what clicking it will do.
fn play_button_label(app: &App) -> &'static str {
    if app.is_playing() {
        " ⏸ pause "
    } else {
        " ▶ play "
    }
}

fn status_line(app: &App) -> Line<'static> {
    let sep = || Span::styled(" │ ", Style::default().fg(DIM));
    let mut spans = Vec::new();

    spans.push(Span::styled(
        play_button_label(app),
        Style::default().fg(Color::Black).bg(PLAYHEAD),
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::raw(format!(
        "{} / {}",
        secs(app.time),
        secs(app.duration())
    )));

    let original = app.timeline.original_duration();
    if original != app.duration() {
        spans.push(Span::styled(
            format!(" (was {})", secs(original)),
            Style::default().fg(DIM),
        ));
    }

    spans.push(sep());
    spans.push(Span::raw(format!("pos {}/{}", app.position, app.len())));

    if let Some(Drag::Event {
        event,
        original_delay,
        new_delay,
        ..
    }) = &app.drag
    {
        spans.push(sep());
        spans.push(Span::styled(
            format!("dragging #{event}: {original_delay:.3}s → {new_delay:.3}s"),
            Style::default().fg(ACCENT),
        ));
    } else if let Some(Origin::Clip { insert }) = app.timeline.origin(app.position) {
        spans.push(sep());
        spans.push(Span::styled(
            format!("in inserted clip #{insert} (d removes it)"),
            Style::default().fg(CLIP),
        ));
    } else if let Some(e) = app.target_event() {
        let p = app.position;
        let now = app.timeline.delay(p).unwrap_or_default();
        let was = app.timeline.original_delay(p).unwrap_or_default();
        spans.push(sep());

        if let Some(Origin::Cut { from, to }) = app.timeline.origin(p) {
            spans.push(Span::styled(
                format!("✂ events {from}..{to} cut (D restores) "),
                Style::default().fg(CUT),
            ));
        }

        if app.timeline.is_retimed(p) {
            spans.push(Span::styled(
                format!("pause before #{e}: {} → {} *", secs(was), secs(now)),
                Style::default().fg(ACCENT),
            ));
        } else {
            spans.push(Span::raw(format!("pause before #{e}: {}", secs(now))));
        }
    }

    if let Some((a, b)) = app.selection() {
        spans.push(sep());
        spans.push(Span::styled(
            format!("selected {a}..{b}"),
            Style::default().bg(SELECTION_BG),
        ));
    }

    Line::from(spans)
}

fn help_line(app: &App) -> Line<'static> {
    if let Some(prompt) = &app.prompt {
        return Line::from(vec![
            Span::styled(app.prompt_label(prompt.kind), Style::default().fg(ACCENT)),
            Span::raw(prompt.input.clone()),
        ]);
    }

    // Messages temporarily replace the key hints, which are always in `?`.
    let message = app.message();

    if let (Some(panel), None) = (&app.meta, message) {
        let mut spans = Vec::new();

        for (k, what) in super::meta::hints(panel) {
            spans.push(Span::styled(*k, Style::default().fg(PLAYHEAD)));
            spans.push(Span::styled(format!(" {what}  "), Style::default().fg(DIM)));
        }

        return Line::from(spans);
    }

    if let Some((msg, is_error)) = message {
        let color = if is_error { Color::Red } else { Color::Green };

        return Line::from(Span::styled(msg.to_owned(), Style::default().fg(color)));
    }

    let keys = [
        ("←→", "step"),
        ("⇧←→", "pause"),
        ("space/p", "play"),
        ("+-<>", "delay"),
        ("=", "set"),
        ("v", "select"),
        ("c", "cap"),
        ("s", "scale"),
        ("u", "undo"),
        ("d", "cut"),
        ("i", "insert"),
        ("t", "view"),
        ("f", "full"),
        ("m", "metadata"),
        ("?", "help"),
        ("q", "quit"),
    ];

    let mut spans = Vec::new();

    for (k, what) in keys {
        spans.push(Span::styled(k, Style::default().fg(PLAYHEAD)));
        spans.push(Span::styled(format!(" {what}  "), Style::default().fg(DIM)));
    }

    Line::from(spans)
}

/// Help popup width, including borders.
const HELP_WIDTH: u16 = 76;
const HELP_KEY_WIDTH: usize = 22;

pub const HELP: &[(&str, &str)] = &[
    ("Navigate", ""),
    ("← / →", "step one event back / forward"),
    ("⇧← / ⇧→", "jump to previous / next pause (≥ 0.5s)"),
    ("[ / ]", "previous / next marker"),
    ("Home / End", "start / end"),
    ("g", "go to time"),
    (
        "space / p",
        "play / pause (with edited timing); or click ▶ play",
    ),
    ("Timing", ""),
    ("+ / -", "pause before next event ±0.1s"),
    ("> / <", "pause before next event ±1s"),
    ("=", "set pause to an exact value"),
    ("v", "start / clear selection (Esc also clears)"),
    ("c / s", "cap / scale pauses in selection (or everything)"),
    ("u / ctrl-r", "undo / redo"),
    ("drag a |", "change the pause before that event"),
    ("drag elsewhere", "scrub"),
    ("Cut & insert", ""),
    ("d / Delete", "cut selection (or the event/clip here)"),
    ("D", "restore a cut (✂), or part of it"),
    ("i", "insert: hold, blank, text, recording, copy"),
    ("View", ""),
    ("t", "switch time view / event view"),
    ("m", "metadata: title, idle limit, colors, cursor…"),
    ("C", "choose the cursor shape for playback"),
    ("( / ) or ctrl-wheel", "zoom timeline in / out (time view)"),
    ("h j k l / H J K L", "pan the preview (by 1 / by 10)"),
    ("0", "auto-scroll to the recorded cursor (undoes hjkl)"),
    ("f", "full screen preview (f / Esc to leave)"),
    ("Output", ""),
    ("r", "render to a new file"),
    ("R", "render in place (keeps .bak, deletes edit list)"),
    ("q", "quit (edits are saved automatically)"),
];

fn draw_help(frame: &mut Frame, area: Rect) {
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                Line::from(Span::styled(
                    *k,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(vec![
                    Span::styled(
                        format!("  {k:<HELP_KEY_WIDTH$}"),
                        Style::default().fg(PLAYHEAD),
                    ),
                    Span::raw(*v),
                ])
            }
        })
        .collect();

    let w = HELP_WIDTH.min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" keys (any key to close) "),
        ),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::{bar, col_of, draw, event_in_col, nice_step, time_at_col};
    use crate::edit::tui::app::tests::app;
    use crate::edit::tui::app::TimelineMode;

    #[test]
    fn help_lines_fit_in_the_popup() {
        let inner = (super::HELP_WIDTH - 2) as usize;

        for (key, text) in super::HELP {
            let len = 2 + super::HELP_KEY_WIDTH.max(key.chars().count()) + text.chars().count();
            assert!(
                len <= inner,
                "help line for '{key}' is {len} > {inner} columns"
            );
        }
    }

    #[test]
    fn cursor_shapes_in_preview() {
        use ratatui::style::{Modifier, Style};

        use super::cursor_cell;
        use crate::asciicast::CursorShape::*;

        let s = Style::default();
        let has = |(_, st): (char, Style), m: Modifier| st.add_modifier.contains(m);

        assert!(has(cursor_cell('a', s, None), Modifier::REVERSED));
        assert!(has(cursor_cell('a', s, Some(Block)), Modifier::REVERSED));
        assert!(!has(cursor_cell('a', s, Some(Block)), Modifier::SLOW_BLINK));
        assert!(has(
            cursor_cell('a', s, Some(BlinkingBlock)),
            Modifier::SLOW_BLINK
        ));
        assert!(has(
            cursor_cell('a', s, Some(Underline)),
            Modifier::UNDERLINED
        ));
        assert_eq!(cursor_cell(' ', s, Some(Bar)).0, '▏');
        assert!(has(cursor_cell('a', s, Some(Bar)), Modifier::REVERSED));
    }

    #[test]
    fn column_mapping_round_trips() {
        let window = (Duration::ZERO, Duration::from_secs(10));

        assert_eq!(col_of(Duration::ZERO, window, 11), Some(0));
        assert_eq!(col_of(Duration::from_secs(5), window, 11), Some(5));
        assert_eq!(col_of(Duration::from_secs(10), window, 11), Some(10));
        assert_eq!(col_of(Duration::from_secs(11), window, 11), None);
        assert_eq!(time_at_col(5.0, window, 11), Duration::from_secs(5));
    }

    #[test]
    fn hit_testing_finds_first_event_in_column() {
        let (a, _d) = app();
        // 0..20s over 21 columns: 1 col per second; events at 1, 2, 10, 11, 11.1, 20
        let window = (Duration::ZERO, Duration::from_secs(20));

        assert_eq!(event_in_col(&a, 1, window, 21), Some(0));
        assert_eq!(event_in_col(&a, 11, window, 21), Some(3));
        assert_eq!(event_in_col(&a, 5, window, 21), None);
        assert_eq!(event_in_col(&a, 20, window, 21), Some(5));
    }

    #[test]
    fn bars_and_steps() {
        assert_eq!(bar(Duration::ZERO), ' ');
        assert_eq!(bar(Duration::from_millis(10)), '▁');
        assert_eq!(bar(Duration::from_secs(10)), '█');
        assert_eq!(bar(Duration::from_secs(1000)), '█');
        assert_eq!(nice_step(0.7), 1.0);
        assert_eq!(nice_step(3.0), 5.0);
        assert_eq!(nice_step(0.03), 0.05);
    }

    #[test]
    fn draws_without_panicking_at_various_sizes() {
        let (mut a, _d) = app();
        a.step(4);

        for (w, h) in [(80, 24), (30, 10), (5, 3), (1, 1), (200, 60)] {
            for mode in [TimelineMode::Time, TimelineMode::Event] {
                for fullscreen in [false, true] {
                    a.mode = mode;
                    a.fullscreen = fullscreen;
                    a.show_help = w > 60;
                    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
                    t.draw(|f| {
                        draw(f, &a);
                    })
                    .unwrap();
                }
            }
        }
    }

    #[test]
    fn panning_a_cropped_preview_draws() {
        let (mut a, _d) = app();
        a.step(4);

        // 20x5 recording in a 12-column preview: cropped, so the title
        // reports the pan offset (regression: this used to panic).
        let mut t = Terminal::new(TestBackend::new(14, 14)).unwrap();

        for (dx, dy) in [(1, 0), (0, 1), (-1, 0), (0, -1), (10, 10), (-10, -10)] {
            a.pan_by(dx, dy);
            t.draw(|f| {
                draw(f, &a);
            })
            .unwrap();
        }

        // stepping while panned, so the cached screen must update
        a.step(1);
        t.draw(|f| {
            draw(f, &a);
        })
        .unwrap();
    }

    #[test]
    fn preview_shows_screen_and_timeline_ticks() {
        let (mut a, _d) = app();
        a.step(4);
        a.fullscreen = false;
        a.show_help = false;

        let mut t = Terminal::new(TestBackend::new(60, 16)).unwrap();
        t.draw(|f| {
            draw(f, &a);
        })
        .unwrap();

        let text: String = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(text.contains("$ ls"));
        assert!(text.contains("file"));
        assert!(text.contains('|'));
        assert!(text.contains('▲'));
        assert!(text.contains("pos 4/6"));
        assert!(text.contains("▶ play"));
        assert!(
            text.contains("←→ step"),
            "key hints shown when there is no message"
        );
    }
}
