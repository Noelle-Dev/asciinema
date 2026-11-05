//! Interactive terminal editor (`asciinema edit <file>`).

mod app;
mod color;
mod fxpanel;
mod meta;
mod ui;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::{Position, Rect};

use self::app::{App, Drag, PromptKind, TimelineMode, EVENT_VIEW_DRAG_STEP};
use self::ui::Areas;
use super::{EditList, Timeline};

pub fn run(path: PathBuf, edits_path: PathBuf, timeline: Timeline, edits: EditList) -> Result<()> {
    let mut app = App::new(path, edits_path, timeline, edits);

    // ratatui::init installs a panic hook that restores the terminal, but it
    // doesn't know about mouse capture. Without turning that off too, a panic
    // leaves the shell flooded with mouse escape sequences.
    let mut terminal = ratatui::init();
    let ratatui_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui_hook(info);
    }));

    execute!(io::stdout(), EnableMouseCapture)?;

    let result = event_loop(&mut terminal, &mut app);

    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();

    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut areas = Areas::default();

    while !app.quit {
        terminal.draw(|frame| areas = ui::draw(frame, app))?;

        let timeout = if app.is_animating() {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(250)
        };

        if event::poll(timeout)? {
            // Drain everything pending before redrawing, so drags stay smooth.
            loop {
                handle_event(app, event::read()?, areas);

                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }

        app.tick();
    }

    Ok(())
}

fn handle_event(app: &mut App, event: Event, areas: Areas) {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => handle_key(app, key),
        Event::Mouse(mouse) => handle_mouse(app, mouse, areas),
        _ => {}
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if ctrl && key.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }

    if let Some(prompt) = &mut app.prompt {
        if prompt.kind == PromptKind::InsertMenu {
            match key.code {
                KeyCode::Char(c) => app.choose_insert(c),
                _ => app.prompt = None,
            }

            return;
        }

        match key.code {
            KeyCode::Enter => app.submit_prompt(),
            KeyCode::Esc => app.prompt = None,
            KeyCode::Backspace => {
                prompt.input.pop();
            }
            KeyCode::Char(c) => prompt.input.push(c),
            _ => {}
        }

        return;
    }

    if app.show_help {
        app.show_help = false;
        return;
    }

    if app.meta.is_some() {
        meta::handle_key(app, key);
        return;
    }

    if app.fx.is_some() {
        fxpanel::handle_key(app, key);
        return;
    }

    match key.code {
        KeyCode::Char('q') => app.quit = true,

        KeyCode::Esc => {
            if app.drag.is_some() {
                app.cancel_drag();
            } else if app.fullscreen {
                app.fullscreen = false;
            } else {
                app.selection_anchor = None;
            }
        }

        // navigation
        KeyCode::Left if shift => app.prev_pause(),
        KeyCode::Right if shift => app.next_pause(),
        KeyCode::Left => app.step(-1),
        KeyCode::Right => app.step(1),
        KeyCode::Home => app.seek_position(0),
        KeyCode::End => app.seek_position(app.len()),
        KeyCode::Char('[') => app.prev_marker(),
        KeyCode::Char(']') => app.next_marker(),
        KeyCode::Char(' ') | KeyCode::Char('p') => app.toggle_play(),
        KeyCode::Char('g') => app.open_prompt(PromptKind::Goto),

        // timing
        KeyCode::Char('+') => app.nudge(0.1),
        KeyCode::Char('-') => app.nudge(-0.1),
        KeyCode::Char('>') => app.nudge(1.0),
        KeyCode::Char('<') => app.nudge(-1.0),
        KeyCode::Char('=') => app.open_prompt(PromptKind::SetDelay),
        KeyCode::Char('v') => app.toggle_selection(),
        KeyCode::Char('c') => app.open_prompt(PromptKind::Cap),
        KeyCode::Char('s') => app.open_prompt(PromptKind::Scale),
        KeyCode::Char('u') => app.undo(),
        KeyCode::Char('d') | KeyCode::Delete => app.delete(),
        KeyCode::Char('D') => app.open_prompt(PromptKind::Restore),
        KeyCode::Char('i') | KeyCode::Insert => app.open_prompt(PromptKind::InsertMenu),
        KeyCode::Char('r') if ctrl => app.redo(),

        // view
        KeyCode::Char('t') => app.toggle_mode(),
        KeyCode::Char('C') => meta::open_cursor_picker(app),
        KeyCode::Char('m') => meta::open(app),
        KeyCode::Char('x') => fxpanel::open(app),
        KeyCode::Char('(') => app.zoom_by(2.0),
        KeyCode::Char(')') => app.zoom_by(0.5),
        KeyCode::Char('f') => app.fullscreen = !app.fullscreen,
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('h') => app.pan_by(-1, 0),
        KeyCode::Char('l') => app.pan_by(1, 0),
        KeyCode::Char('k') => app.pan_by(0, -1),
        KeyCode::Char('j') => app.pan_by(0, 1),
        KeyCode::Char('H') => app.pan_by(-10, 0),
        KeyCode::Char('L') => app.pan_by(10, 0),
        KeyCode::Char('K') => app.pan_by(0, -10),
        KeyCode::Char('J') => app.pan_by(0, 10),
        KeyCode::Char('0') => app.pan = None,

        // output
        KeyCode::Char('r') => app.open_prompt(PromptKind::RenderTo),
        KeyCode::Char('R') => app.open_prompt(PromptKind::ConfirmRenderInPlace),

        _ => {}
    }
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    area.contains(Position { x, y })
}

fn handle_mouse(app: &mut App, mouse: MouseEvent, areas: Areas) {
    if app.prompt.is_some() || app.show_help {
        return;
    }

    let (mx, my) = (mouse.column, mouse.row);

    if app.meta.is_some() && contains(areas.meta, mx, my) {
        meta::handle_mouse(app, mouse, areas.meta);
        return;
    }

    if app.fx.is_some() {
        if contains(areas.meta, mx, my) {
            fxpanel::handle_panel_mouse(app, mouse, areas.meta);
            return;
        }

        // picking a region: map the mouse to a cell of the recorded screen
        let p = areas.preview;
        let (ox, oy) = app.viewport_offset(p.width as usize, p.height as usize);
        let (cols, rows) = app.vt().size();
        let cell = contains(p, mx, my).then(|| {
            (
                (ox + (mx - p.x) as usize).min(cols.saturating_sub(1)),
                (oy + (my - p.y) as usize).min(rows.saturating_sub(1)),
            )
        });

        if matches!(
            app.fx.as_ref().map(|f| &f.mode),
            Some(fxpanel::Mode::PickRegion { .. })
        ) {
            fxpanel::handle_preview_mouse(app, mouse, cell);
            return;
        }
    }
    let tl = areas.timeline;
    let in_timeline = contains(tl, mx, my);
    let x = mx.saturating_sub(tl.x).min(tl.width.saturating_sub(1));
    let ctrl = mouse.modifiers.contains(KeyModifiers::CONTROL);
    let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) if contains(areas.play_button, mx, my) => {
            app.toggle_play();
        }

        MouseEventKind::Down(MouseButton::Left) if in_timeline => {
            let on_tick_row = my == tl.y + ui::TICK_ROW;

            let hit = on_tick_row
                .then(|| match app.mode {
                    TimelineMode::Time => {
                        let window = app.time_window();
                        ui::event_in_col(app, x, window, tl.width).map(|e| {
                            let secs_per_col =
                                window.1.as_secs_f64() / (tl.width.max(2) - 1) as f64;
                            (e, secs_per_col, Some(window))
                        })
                    }

                    TimelineMode::Event => {
                        let e = app.event_window(tl.width as usize) + x as usize;
                        (e < app.len()).then_some((e, EVENT_VIEW_DRAG_STEP, None))
                    }
                })
                .flatten();

            match hit {
                Some((event, secs_per_col, window)) => {
                    app.start_event_drag(event, x, secs_per_col, window)
                }

                None => {
                    app.drag = Some(Drag::Scrub);
                    scrub(app, x, tl.width);
                }
            }
        }

        MouseEventKind::Drag(MouseButton::Left) => match app.drag {
            Some(Drag::Scrub) => scrub(app, x, tl.width),
            Some(Drag::Event { .. }) => app.update_event_drag(x),
            None => {}
        },

        MouseEventKind::Up(MouseButton::Left) => match app.drag {
            Some(Drag::Event { .. }) => app.finish_drag(),
            _ => app.drag = None,
        },

        MouseEventKind::ScrollUp if in_timeline && ctrl => app.zoom_by(1.25),
        MouseEventKind::ScrollDown if in_timeline && ctrl => app.zoom_by(0.8),
        MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft if in_timeline => app.step(-1),
        MouseEventKind::ScrollDown | MouseEventKind::ScrollRight if in_timeline => app.step(1),

        MouseEventKind::ScrollUp if contains(areas.preview, mx, my) => {
            if shift {
                app.pan_by(-4, 0)
            } else {
                app.pan_by(0, -2)
            }
        }

        MouseEventKind::ScrollDown if contains(areas.preview, mx, my) => {
            if shift {
                app.pan_by(4, 0)
            } else {
                app.pan_by(0, 2)
            }
        }

        _ => {}
    }
}

fn scrub(app: &mut App, x: u16, width: u16) {
    match app.mode {
        TimelineMode::Time => {
            let window = app.time_window();
            app.seek_time(ui::time_at_col(x as f64, window, width));
        }

        TimelineMode::Event => {
            let start = app.event_window(width as usize);
            app.seek_position(start + x as usize);
        }
    }
}
