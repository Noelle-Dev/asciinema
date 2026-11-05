//! Effect side panel: choose an effect and what it animates, preview it live
//! in the preview pane, then insert it at the playhead (or have it replace
//! the current frame).

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use avt::Vt;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

use super::app::App;
use crate::edit::fx::{self, EffectInfo, FxSource, FxSpec, OptionInfo, Tool};
use crate::edit::Clip;

const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Yellow;
const KEY: Color = Color::Cyan;
const LABEL_W: usize = 11;

/// Pause between preview loops.
const LOOP_GAP: Duration = Duration::from_millis(600);

/// How long settings must be unchanged before the preview is re-captured.
const CAPTURE_DELAY: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Effect,
    Animate,
    Placement,
    Speed,
    Length,
    Seed,
    Hold,
    Colors,
    Options,
    Apply,
}

const ROWS: [Row; 10] = [
    Row::Effect,
    Row::Animate,
    Row::Placement,
    Row::Speed,
    Row::Length,
    Row::Seed,
    Row::Hold,
    Row::Colors,
    Row::Options,
    Row::Apply,
];

pub enum Mode {
    List,
    /// Choosing an effect; remembers the one to go back to on Esc.
    Effects {
        selected: usize,
        previous: String,
    },
    /// All of the effect's options.
    Options {
        options: Vec<OptionInfo>,
        selected: usize,
    },
    /// A dropdown of choices under `row`.
    Dropdown {
        row: Row,
        choices: Vec<String>,
        selected: usize,
    },
    /// Dragging a region on the preview.
    PickRegion,
    /// Editing a value in place; `back` is the options list to return to.
    Edit {
        field: Field,
        input: String,
        back: Option<(Vec<OptionInfo>, usize)>,
    },
}

/// The captured effect, looping in the preview.
pub struct Preview {
    frames: fx::Frames,
    started: Instant,
    /// (frame index applied, screen) - advanced incrementally between draws.
    screen: RefCell<Option<(usize, Vt)>>,
}

pub struct FxPanel {
    pub tool: Tool,
    pub effects: Vec<EffectInfo>,
    pub spec: FxSpec,
    pub hold: f64,
    /// Play instead of the current frame (or the selection) rather than after it.
    pub replace: bool,
    /// The last typed text, offered again when switching back to typed text.
    pub last_text: String,
    pub selected: usize,
    pub mode: Mode,
    pub preview: Option<Preview>,
    /// How long the effect runs at 1× (from the last capture).
    pub natural: Option<Duration>,
    /// Region being dragged on the preview, in screen cells: (anchor, corner).
    pub drag: Option<((usize, usize), (usize, usize))>,
    /// When to start capturing the preview (settings changed; waits briefly
    /// so holding an arrow key doesn't start a capture per press).
    pub capture_due: Option<Instant>,
    /// A capture running in the background, and the settings it's for.
    pub job: Option<(FxSpec, Receiver<Result<fx::Captured>>)>,
}

impl FxPanel {
    /// Whether a preview is being prepared.
    pub fn busy(&self) -> bool {
        self.capture_due.is_some() || self.job.is_some()
    }
}

// ---- where the effect goes ------------------------------------------------------

/// Playlist positions for the effect: (screen it animates, screen it plays over).
/// Inserting, both are the frame at the playhead. Replacing, it animates the
/// screen *after* the replaced frames, over the screen from before them.
fn positions(app: &App, replace: bool) -> (usize, usize) {
    match replace.then(|| app.frames()).flatten() {
        Some((lo, hi)) => (hi + 1, lo),
        None => (app.position, app.position),
    }
}

// ---- opening & capturing ------------------------------------------------------

pub fn open(app: &mut App) {
    let tool = match fx::find_tool() {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("{e:#}");
            app.error(
                msg.lines()
                    .next()
                    .unwrap_or("no effects tool found")
                    .to_owned()
                    + " (see `asciinema edit --help`)",
            );
            return;
        }
    };

    let effects = match fx::list_effects(&tool) {
        Ok(e) if !e.is_empty() => e,
        Ok(_) => return app.error(format!("{} listed no effects", tool.name())),
        Err(e) => return app.error(format!("{e:#}")),
    };

    let effect = effects
        .iter()
        .find(|e| e.name == "decrypt")
        .unwrap_or(&effects[0])
        .name
        .clone();

    // start from the whole screen when there's something on it
    let screen = app.timeline.vt_at_position(app.position);
    let has_text = screen.text().iter().any(|l| !l.trim().is_empty());

    let source = if has_text {
        FxSource::Screen
    } else {
        FxSource::Text {
            text: "Hello".to_owned(),
        }
    };

    app.fx = Some(FxPanel {
        tool,
        effects,
        spec: FxSpec {
            effect,
            source,
            args: Vec::new(),
            speed: 1.0,
            duration: None,
            seed: rand::random::<u32>() as u64,
            keep_colors: false,
        },
        hold: 1.0,
        replace: false,
        last_text: "Hello".to_owned(),
        selected: 0,
        mode: Mode::List,
        preview: None,
        natural: None,
        drag: None,
        capture_due: None,
        job: None,
    });

    refresh(app);
}

/// Re-captures the preview with the current settings, shortly (see `tick`).
pub fn refresh(app: &mut App) {
    if let Some(p) = &mut app.fx {
        p.capture_due = Some(Instant::now() + CAPTURE_DELAY);
    }
}

/// Starts due captures in the background and picks up finished ones; call
/// regularly. Results for settings that have since changed are dropped.
pub fn tick(app: &mut App) {
    let replace = app.fx.as_ref().is_some_and(|p| p.replace);
    let (animated, _) = positions(app, replace);

    // (message, is_error) to show once the panel is no longer borrowed
    let message = {
        let Some(panel) = &mut app.fx else { return };

        if panel.capture_due.is_some_and(|due| Instant::now() >= due) {
            panel.capture_due = None;

            let (tool, spec) = (panel.tool.clone(), panel.spec.clone());
            let screen = app.timeline.vt_at_position(animated);
            let (size, dump) = (screen.size(), screen.dump());
            let (tx, rx) = mpsc::channel();

            // a large screen can take a second or two; keep the editor responsive
            std::thread::spawn(move || {
                let mut screen = Vt::builder()
                    .size(size.0, size.1)
                    .scrollback_limit(0)
                    .build();
                screen.feed_str(&dump);
                let _ = tx.send(fx::capture(&tool, &spec, &screen));
            });

            panel.job = Some((panel.spec.clone(), rx));
            (format!("Capturing {}…", panel.spec.effect), false)
        } else {
            let Some((spec, rx)) = &panel.job else { return };

            let result = match rx.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!("the capture stopped unexpectedly"))
                }
            };

            let current = *spec == panel.spec;
            panel.job = None;

            if !current {
                return; // settings changed meanwhile; a new capture is due
            }

            match result {
                Ok(captured) => {
                    let seconds: f64 = captured.frames.iter().map(|(d, _)| d.as_secs_f64()).sum();
                    let name = panel.spec.effect.clone();
                    panel.natural = Some(captured.natural);

                    panel.preview = Some(Preview {
                        frames: captured.frames,
                        started: Instant::now(),
                        screen: RefCell::new(None),
                    });

                    (format!("Previewing {name} ({seconds:.1}s)"), false)
                }

                Err(e) => {
                    panel.preview = None;
                    (format!("{e:#}"), true)
                }
            }
        }
    };

    match message {
        (m, false) => app.info(m),
        (m, true) => app.error(m),
    }
}

fn apply(app: &mut App) {
    let Some(panel) = &app.fx else { return };
    let (tool, spec, hold, replace) = (
        panel.tool.clone(),
        panel.spec.clone(),
        panel.hold,
        panel.replace,
    );

    // what's replaced: the events behind the frames
    let replaced = if replace {
        match app.frames().and_then(|(lo, hi)| app.events_in(lo, hi)) {
            Some(r) => Some(r),
            None => return app.error("There's no recorded frame here to replace"),
        }
    } else {
        None
    };

    let (animated, _) = positions(app, replace);
    let screen = app.timeline.vt_at_position(animated);

    match fx::capture_to_cache(&tool, &spec, &screen, &app.path) {
        Ok(cache) => {
            app.fx = None;
            let clip = Clip::Effect {
                fx: spec,
                hold,
                cache,
            };

            match replaced {
                Some((from, to)) => app.replace(from, to, clip),
                None => app.insert(clip),
            }
        }

        Err(e) => app.error(format!("{e:#}")),
    }
}

/// Screen to show in the preview while an effect preview is playing.
pub fn preview_screen(app: &App) -> Option<std::cell::Ref<'_, Vt>> {
    let panel = app.fx.as_ref()?;

    // picking a region: show the real screen, still, to pick from
    if matches!(panel.mode, Mode::PickRegion) {
        return None;
    }
    let preview = panel.preview.as_ref()?;
    let total: Duration = preview.frames.iter().map(|(d, _)| *d).sum::<Duration>()
        + Duration::from_secs_f64(panel.hold)
        + LOOP_GAP;

    // loop: which frame should be showing now?
    let t = Duration::from_secs_f64(
        preview.started.elapsed().as_secs_f64() % total.as_secs_f64().max(0.001),
    );
    let mut acc = Duration::ZERO;
    let mut due = 0;

    for (d, _) in &preview.frames {
        acc += *d;

        if acc > t {
            break;
        }

        due += 1;
    }

    {
        let mut cache = preview.screen.borrow_mut();

        let rebuild = match &*cache {
            Some((applied, _)) => *applied > due,
            None => true,
        };

        if rebuild {
            let (_, background) = positions(app, panel.replace);
            *cache = Some((0, app.timeline.vt_at_position(background)));
        }

        if let Some((applied, vt)) = &mut *cache {
            for (_, out) in &preview.frames[*applied..due] {
                vt.feed_str(out);
            }

            *applied = due;
        }
    }

    Some(std::cell::Ref::map(preview.screen.borrow(), |c| {
        &c.as_ref().expect("just filled").1
    }))
}

fn restart_preview(panel: &mut FxPanel) {
    if let Some(p) = &mut panel.preview {
        p.started = Instant::now();
        *p.screen.borrow_mut() = None;
    }
}

// ---- prompts ------------------------------------------------------------------

/// A value edited in place in the panel.
#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    Text,
    Speed,
    Length,
    Seed,
    Hold,
    Region,
    /// An effect option, by flag.
    Option(String),
}

impl Field {
    /// The list row it's edited in (options are edited in the options list).
    fn row(&self) -> Option<Row> {
        match self {
            Field::Text | Field::Region => Some(Row::Animate),
            Field::Speed => Some(Row::Speed),
            Field::Length => Some(Row::Length),
            Field::Seed => Some(Row::Seed),
            Field::Hold => Some(Row::Hold),
            Field::Option(_) => None,
        }
    }

    fn help(&self) -> String {
        match self {
            Field::Text => "Type the text to animate (\\n for a new line).".to_owned(),
            Field::Speed => "Type a speed: 2 is twice as fast, 0.5 half as fast.".to_owned(),
            Field::Length => "Type how many seconds it should run for, or leave it empty to go by the speed.".to_owned(),
            Field::Seed => "Type a whole number. The same seed gives the same animation.".to_owned(),
            Field::Hold => "Type how many seconds the last frame stays up.".to_owned(),
            Field::Region => "Type the region as COL,ROW,COLSxROWS, counting from 0 - e.g. 0,2,40x3.".to_owned(),
            Field::Option(flag) => format!("Type a value for {flag} (space separated for a list), or leave it empty for the default."),
        }
    }
}

fn current_value(p: &FxPanel, field: &Field) -> String {
    match field {
        Field::Text => match &p.spec.source {
            FxSource::Text { text } => text.clone(),
            _ => p.last_text.clone(),
        }
        .replace('\n', "\\n"),
        Field::Speed => p.spec.speed.to_string(),
        Field::Length => p.spec.duration.map(|d| d.to_string()).unwrap_or_default(),
        Field::Seed => p.spec.seed.to_string(),
        Field::Hold => p.hold.to_string(),
        Field::Region => match &p.spec.source {
            FxSource::Region {
                col,
                row,
                cols,
                rows,
            } => format!("{col},{row},{cols}x{rows}"),
            _ => String::new(),
        },
        Field::Option(flag) => option_value(&p.spec.args, flag).unwrap_or_default(),
    }
}

/// Starts editing `field` in place, prefilled with its current value.
fn start_edit(app: &mut App, field: Field) {
    let Some(p) = &mut app.fx else { return };
    let input = current_value(p, &field);

    // editing an option: go back to the options list afterwards
    let back = match std::mem::replace(&mut p.mode, Mode::List) {
        Mode::Options { options, selected } => Some((options, selected)),
        _ => None,
    };

    p.mode = Mode::Edit { field, input, back };
}

/// Leaves the editor, back to where it was opened from.
fn end_edit(p: &mut FxPanel) {
    if let Mode::Edit {
        back: Some((options, selected)),
        ..
    } = std::mem::replace(&mut p.mode, Mode::List)
    {
        p.mode = Mode::Options { options, selected };
    }
}

/// Applies an edited value. On a bad value the editor stays open.
fn submit_edit(app: &mut App) {
    let Some(p) = &mut app.fx else { return };
    let Mode::Edit { field, input, .. } = &p.mode else {
        return;
    };

    let (field, input) = (field.clone(), input.trim().to_owned());
    let number = input.parse::<f64>().ok().filter(|v| v.is_finite());

    let error = match &field {
        Field::Text if input.is_empty() => Some("type some text to animate".to_owned()),

        Field::Text => {
            let text = input.replace("\\n", "\n");
            p.last_text = text.clone();
            p.spec.source = FxSource::Text { text };
            p.preview = None;
            None
        }

        Field::Speed => match number {
            Some(v) if v > 0.0 => {
                p.spec.speed = v;
                p.spec.duration = None; // the speed is what was asked for now
                None
            }
            _ => Some(format!("not a positive number: '{input}'")),
        },

        Field::Length => match (input.is_empty(), number) {
            (true, _) => {
                p.spec.duration = None;
                None
            }
            (false, Some(v)) if v > 0.0 => {
                p.spec.duration = Some(v);
                None
            }
            _ => Some(format!("not a positive number of seconds: '{input}'")),
        },

        Field::Seed => match input.parse() {
            Ok(v) => {
                p.spec.seed = v;
                None
            }
            Err(_) => Some(format!("not a whole number: '{input}'")),
        },

        Field::Hold => match number {
            Some(v) if v >= 0.0 => {
                p.hold = v;
                None
            }
            _ => Some(format!("not a number of seconds: '{input}'")),
        },

        Field::Region => match crate::cmd::edit::parse_region(&input) {
            Ok(r) => {
                p.spec.source = r;
                p.preview = None;
                None
            }
            Err(e) => Some(format!("{e:#}")),
        },

        Field::Option(flag) => {
            set_option(
                &mut p.spec.args,
                flag,
                (!input.is_empty()).then_some(input.as_str()),
            );
            None
        }
    };

    if let Some(e) = error {
        return app.error(e);
    }

    end_edit(p);

    if field == Field::Hold {
        restart_preview(p); // the frames don't change
    } else {
        refresh(app);
    }
}

fn handle_edit_key(app: &mut App, key: KeyEvent) {
    let Some(p) = &mut app.fx else { return };
    let Mode::Edit { input, .. } = &mut p.mode else {
        return;
    };

    let ctrl = key
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Enter => submit_edit(app),
        KeyCode::Esc => end_edit(p),
        KeyCode::Backspace => {
            input.pop();
        }
        KeyCode::Char('u') if ctrl => input.clear(),
        KeyCode::Char(c) => input.push(c),
        _ => {}
    }
}

/// The value box shown in place of a row's value while it's edited.
fn input_box(input: &str, width: usize) -> Span<'static> {
    let text = format!(" {input}▏");
    let pad = width.saturating_sub(text.chars().count());

    Span::styled(
        format!("{text}{}", " ".repeat(pad)),
        Style::default()
            .fg(Color::White)
            .bg(Color::Indexed(238))
            .add_modifier(Modifier::BOLD),
    )
}

// ---- dropdowns ------------------------------------------------------------------------

fn dropdown_choices(app: &App, row: Row) -> Option<(Vec<String>, usize)> {
    let p = app.fx.as_ref()?;

    match row {
        Row::Animate => Some((
            vec![
                "Typed text…".to_owned(),
                "A region of the screen… (drag it)".to_owned(),
                "The whole screen".to_owned(),
            ],
            match p.spec.source {
                FxSource::Text { .. } => 0,
                FxSource::Region { .. } => 1,
                FxSource::Screen => 2,
            },
        )),

        Row::Placement => {
            let target = match app.selection() {
                Some(_) => "Replace the selected frames",
                None => "Replace this frame",
            };

            Some((
                vec!["Insert after this frame".to_owned(), target.to_owned()],
                usize::from(p.replace),
            ))
        }

        Row::Colors => Some((
            vec![
                "The effect's colours".to_owned(),
                "The screen's own colours".to_owned(),
            ],
            usize::from(p.spec.keep_colors),
        )),

        _ => None,
    }
}

fn choose(app: &mut App, row: Row, choice: usize) {
    let Some(p) = &mut app.fx else { return };
    p.mode = Mode::List;

    // leaving typed text: remember it, and show the real screen straight away
    // rather than the old animation until the new capture is in
    if row == Row::Animate {
        if let FxSource::Text { text } = &p.spec.source {
            p.last_text = text.clone();
        }

        if choice != 0 {
            p.preview = None;
        }
    }

    match (row, choice) {
        (Row::Animate, 0) => return start_edit(app, Field::Text),

        (Row::Animate, 1) => {
            p.mode = Mode::PickRegion;
            p.drag = None;
            return app
                .info("Drag over the preview to choose the region (c to type it, Esc to cancel)");
        }

        (Row::Animate, _) => p.spec.source = FxSource::Screen,

        // what's animated, and what it plays over, depend on this
        (Row::Placement, c) => p.replace = c == 1,

        (Row::Colors, c) => p.spec.keep_colors = c == 1,
        _ => return,
    }

    refresh(app);
}

// ---- options (as CLI-style args) ------------------------------------------------------

/// The effect's args as (flag, values) pairs.
fn parse_args(args: &[String]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();

    for a in args {
        if a.starts_with("--") {
            out.push((a.clone(), Vec::new()));
        } else if let Some(last) = out.last_mut() {
            last.1.push(a.clone());
        }
    }

    out
}

/// Sets (or with `None`, removes) an option. Values are space separated, like
/// on the command line; a switch is set with an empty value list.
fn set_option(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    let mut opts = parse_args(args);
    opts.retain(|(f, _)| f != flag);

    if let Some(v) = value {
        opts.push((
            flag.to_owned(),
            v.split_whitespace().map(str::to_owned).collect(),
        ));
    }

    *args = opts
        .into_iter()
        .flat_map(|(f, v)| std::iter::once(f).chain(v))
        .collect();
}

fn option_value(args: &[String], flag: &str) -> Option<String> {
    parse_args(args)
        .into_iter()
        .find(|(f, _)| f == flag)
        .map(|(_, v)| v.join(" "))
}

// ---- keys & mouse -------------------------------------------------------------------

pub fn handle_key(app: &mut App, key: KeyEvent) {
    let Some(panel) = &mut app.fx else { return };

    if matches!(panel.mode, Mode::Edit { .. }) {
        return handle_edit_key(app, key);
    }

    match &mut panel.mode {
        Mode::List => match key.code {
            KeyCode::Esc | KeyCode::Char('x') => app.fx = None,
            KeyCode::Up | KeyCode::Char('k') => panel.selected = panel.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                panel.selected = (panel.selected + 1).min(ROWS.len() - 1)
            }

            KeyCode::Char('r') => {
                panel.spec.seed = rand::random::<u32>() as u64;
                refresh(app);
            }

            KeyCode::Char(' ') | KeyCode::Char('p') => restart_preview(panel),
            KeyCode::Char('I') => apply(app),

            KeyCode::Enter => {
                let row = ROWS[panel.selected];
                activate(app, row);
            }

            _ => {}
        },

        Mode::Dropdown {
            row,
            choices,
            selected,
        } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1).min(choices.len() - 1)
            }
            KeyCode::Esc => panel.mode = Mode::List,

            KeyCode::Enter => {
                let (row, choice) = (*row, *selected);
                choose(app, row, choice);
            }

            _ => {}
        },

        Mode::Effects { selected, previous } => {
            let n = panel.effects.len();

            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    panel.spec.effect = panel.effects[*selected].name.clone();
                    refresh(app);
                }

                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(n - 1);
                    panel.spec.effect = panel.effects[*selected].name.clone();
                    refresh(app);
                }

                KeyCode::Enter => {
                    // options belong to one effect
                    if *previous != panel.spec.effect {
                        panel.spec.args.clear();
                    }

                    panel.mode = Mode::List;
                }

                KeyCode::Esc => {
                    panel.spec.effect = previous.clone();
                    panel.mode = Mode::List;
                    refresh(app);
                }

                _ => {}
            }
        }

        Mode::Options { options, selected } => match key.code {
            KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                *selected = (*selected + 1).min(options.len().saturating_sub(1))
            }
            KeyCode::Esc => panel.mode = Mode::List,

            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                if let Some(o) = options.get(*selected) {
                    let flag = o.flag.clone();
                    set_option(&mut panel.spec.args, &flag, None);
                    refresh(app);
                }
            }

            KeyCode::Enter => {
                let Some(o) = options.get(*selected).cloned() else {
                    return;
                };

                if o.takes_value {
                    start_edit(app, Field::Option(o.flag));
                } else {
                    // a switch: toggle it
                    let on = option_value(&panel.spec.args, &o.flag).is_some();
                    set_option(&mut panel.spec.args, &o.flag, (!on).then_some(""));
                    refresh(app);
                }
            }

            _ => {}
        },

        Mode::PickRegion => match key.code {
            KeyCode::Esc => {
                panel.mode = Mode::List;
                panel.drag = None;
            }
            KeyCode::Char('c') => start_edit(app, Field::Region),
            _ => {}
        },

        Mode::Edit { .. } => {}
    }
}

fn activate(app: &mut App, row: Row) {
    if let Some((choices, selected)) = dropdown_choices(app, row) {
        if let Some(p) = &mut app.fx {
            p.mode = Mode::Dropdown {
                row,
                choices,
                selected,
            };
        }

        return;
    }

    let Some(panel) = &mut app.fx else { return };

    match row {
        Row::Effect => {
            let selected = panel
                .effects
                .iter()
                .position(|e| e.name == panel.spec.effect)
                .unwrap_or(0);

            panel.mode = Mode::Effects {
                selected,
                previous: panel.spec.effect.clone(),
            };
        }

        Row::Speed => start_edit(app, Field::Speed),
        Row::Length => start_edit(app, Field::Length),
        Row::Seed => start_edit(app, Field::Seed),
        Row::Hold => start_edit(app, Field::Hold),

        Row::Options => match fx::effect_options(&panel.tool, &panel.spec.effect) {
            Ok(options) => {
                panel.mode = Mode::Options {
                    options,
                    selected: 0,
                }
            }
            Err(e) => app.error(format!("{e:#}")),
        },

        Row::Apply => apply(app),

        // dropdowns, handled above
        Row::Animate | Row::Placement | Row::Colors => {}
    }
}

/// Mouse over the preview while picking a region. `cell` maps a mouse
/// position to a screen cell.
pub fn handle_preview_mouse(app: &mut App, mouse: MouseEvent, cell: Option<(usize, usize)>) {
    let Some(panel) = &mut app.fx else { return };

    if !matches!(panel.mode, Mode::PickRegion) {
        return;
    }

    match (mouse.kind, cell) {
        (MouseEventKind::Down(MouseButton::Left), Some(c)) => panel.drag = Some((c, c)),

        (MouseEventKind::Drag(MouseButton::Left), Some(c)) => {
            if let Some((a, _)) = panel.drag {
                panel.drag = Some((a, c));
            }
        }

        (MouseEventKind::Up(MouseButton::Left), _) => {
            if let Some(((ax, ay), (bx, by))) = panel.drag.take() {
                panel.spec.source = FxSource::Region {
                    col: ax.min(bx),
                    row: ay.min(by),
                    cols: ax.abs_diff(bx) + 1,
                    rows: ay.abs_diff(by) + 1,
                };

                panel.mode = Mode::List;
                panel.preview = None;
                refresh(app);
            }
        }

        _ => {}
    }
}

/// The region to highlight on the preview, as (col, row, cols, rows).
pub fn highlighted_region(app: &App) -> Option<(usize, usize, usize, usize)> {
    let panel = app.fx.as_ref()?;

    if !matches!(panel.mode, Mode::PickRegion) {
        return None;
    }

    match (panel.drag, &panel.spec.source) {
        (Some(((ax, ay), (bx, by))), _) => Some((
            ax.min(bx),
            ay.min(by),
            ax.abs_diff(bx) + 1,
            ay.abs_diff(by) + 1,
        )),
        (
            None,
            FxSource::Region {
                col,
                row,
                cols,
                rows,
            },
        ) => Some((*col, *row, *cols, *rows)),
        _ => None,
    }
}

pub fn handle_panel_mouse(app: &mut App, mouse: MouseEvent, inner: Rect) {
    let Some(panel) = &mut app.fx else { return };

    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return;
    }

    let i = mouse.row.saturating_sub(inner.y) as usize;

    match &panel.mode {
        Mode::List if i < ROWS.len() => {
            if panel.selected == i {
                activate(app, ROWS[i]);
            } else {
                panel.selected = i;
            }
        }

        // a click on a choice picks it; anywhere else closes the dropdown
        Mode::Dropdown { row, choices, .. } => {
            let top = dropdown_top(inner, *row);
            let n = choices.len();
            let row = *row;

            match (mouse.row as usize).checked_sub(top as usize + 2) {
                Some(c) if c < n => choose(app, row, c),
                _ => panel.mode = Mode::List,
            }
        }

        _ => {}
    }
}

// ---- drawing ------------------------------------------------------------------------

fn label(s: &str) -> Span<'static> {
    Span::styled(format!("{s:<LABEL_W$}"), Style::default().fg(Color::Gray))
}

fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

fn row_line(app: &App, panel: &FxPanel, row: Row) -> Line<'static> {
    let s = &panel.spec;
    let dim = |t: String| Span::styled(t, Style::default().fg(DIM));
    let arrow = || dim(" ▾".to_owned());

    match row {
        Row::Effect => Line::from(vec![label("Effect"), Span::raw(s.effect.clone()), arrow()]),
        Row::Animate => Line::from(vec![
            label("Animate"),
            Span::raw(s.source.describe()),
            arrow(),
        ]),

        Row::Placement => {
            let what = match (panel.replace, app.selection()) {
                (false, _) => "insert after this frame",
                (true, Some(_)) => "replace selected frames",
                (true, None) => "replace this frame",
            };

            Line::from(vec![label("Placement"), Span::raw(what), arrow()])
        }

        Row::Speed => {
            let shown = match (s.duration, panel.natural) {
                (Some(d), Some(n)) => format!("{:.2}× (from the length)", n.as_secs_f64() / d),
                (Some(_), None) => "from the length".to_owned(),
                (None, _) => format!("{}×", s.speed),
            };

            Line::from(vec![label("Speed"), Span::raw(shown)])
        }

        Row::Length => {
            let shown = match (s.duration, panel.natural) {
                (Some(d), _) => Span::raw(format!("{d}s")),
                (None, Some(n)) => dim(format!("{} (from the speed)", secs(n.div_f64(s.speed)))),
                (None, None) => dim("from the speed".to_owned()),
            };

            Line::from(vec![label("Length"), shown])
        }

        Row::Seed => Line::from(vec![
            label("Seed"),
            Span::raw(s.seed.to_string()),
            dim("  r re-rolls".to_owned()),
        ]),

        Row::Hold => Line::from(vec![label("Hold"), Span::raw(format!("{}s", panel.hold))]),

        Row::Colors => Line::from(vec![
            label("Colours"),
            Span::raw(if s.keep_colors {
                "the screen's own"
            } else {
                "the effect's"
            }),
            arrow(),
        ]),

        Row::Options => {
            let n = parse_args(&s.args).len();
            let what = if n == 0 {
                "defaults".to_owned()
            } else {
                format!("{n} changed")
            };

            Line::from(vec![
                label("Options"),
                Span::raw(what),
                dim(" ⏎ see all".to_owned()),
            ])
        }

        Row::Apply => {
            let what = match (panel.replace, app.selection()) {
                (false, _) => "  ⏎ Insert after this frame",
                (true, Some(_)) => "  ⏎ Replace the selected frames",
                (true, None) => "  ⏎ Replace this frame",
            };

            Line::from(Span::styled(
                what,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
        }
    }
}

fn row_label(row: Row) -> &'static str {
    match row {
        Row::Effect => "Effect",
        Row::Animate => "Animate",
        Row::Placement => "Placement",
        Row::Speed => "Speed",
        Row::Length => "Length",
        Row::Seed => "Seed",
        Row::Hold => "Hold",
        Row::Colors => "Colours",
        Row::Options => "Options",
        Row::Apply => "",
    }
}

/// What the selected row does, shown at the bottom of the panel.
fn row_help(panel: &FxPanel, row: Row) -> String {
    match row {
        Row::Effect => {
            let desc = panel
                .effects
                .iter()
                .find(|e| e.name == panel.spec.effect)
                .map_or("", |e| e.description.as_str());

            format!("{desc}. Enter to browse the effects; each previews as you move.")
        }
        Row::Animate => "What the effect draws: typed text on a cleared screen, or text already on the screen - a region of it, or all of it - animated in place.".to_owned(),
        Row::Placement => "Insert plays the effect after the frame you're on. Replace plays it instead of that frame (or the selected frames): they're cut, and the effect ends on the screen as it looked after them.".to_owned(),
        Row::Speed => "How fast the animation runs: 2 is twice as fast, 0.5 half as fast. Setting a speed clears the length.".to_owned(),
        Row::Length => "Fit the animation to this many seconds; the speed is worked out to match. Leave empty to go by the speed.".to_owned(),
        Row::Seed => "Effects use randomness; the same seed gives the same animation every time. r picks a new one.".to_owned(),
        Row::Hold => "How long the effect's last frame stays on screen before the recording carries on.".to_owned(),
        Row::Colors => "Use the effect's own colour scheme, or keep the colours already on screen (for a region or the whole screen).".to_owned(),
        Row::Options => "Every option this effect has, with what it does and its default. Change any of them.".to_owned(),
        Row::Apply => "Capture the effect and put it in the recording. It shows on the timeline as a clip; d removes it, u undoes.".to_owned(),
    }
}

/// Screen row of `row`'s line in the list.
fn dropdown_top(inner: Rect, row: Row) -> u16 {
    inner.y + ROWS.iter().position(|r| *r == row).unwrap_or(0) as u16
}

pub fn draw(buf: &mut Buffer, area: Rect, app: &App) -> Rect {
    let Some(panel) = &app.fx else {
        return Rect::default();
    };

    let title = match &panel.mode {
        Mode::List | Mode::Dropdown { .. } => format!(" effect ({}) ", panel.tool.name()),
        Mode::Effects { .. } => " choose an effect ".to_owned(),
        Mode::Options { .. } => format!(" {} options ", panel.spec.effect),
        Mode::PickRegion => " drag a region on the preview ".to_owned(),
        Mode::Edit { back: Some(_), .. } => format!(" {} options ", panel.spec.effect),
        Mode::Edit { .. } => format!(" effect ({}) ", panel.tool.name()),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(title);

    let inner = block.inner(area);
    Clear.render(area, buf);
    block.render(area, buf);

    let line_at = |buf: &mut Buffer, y: u16, line: Line<'_>| {
        if y < inner.bottom() {
            Paragraph::new(line).render(Rect::new(inner.x, y, inner.width, 1), buf);
        }
    };

    let help_at = |buf: &mut Buffer, y: u16, text: String| {
        if y < inner.bottom() {
            Paragraph::new(Span::styled(text, Style::default().fg(DIM)))
                .wrap(Wrap { trim: true })
                .render(Rect::new(inner.x, y, inner.width, inner.bottom() - y), buf);
        }
    };

    let editing = match &panel.mode {
        Mode::Edit { field, input, .. } => Some((field, input.as_str())),
        _ => None,
    };

    // the options list, also while one of its values is being edited
    let options_view = match &panel.mode {
        Mode::Options { options, selected } => Some((options, *selected)),
        Mode::Edit {
            back: Some((options, selected)),
            ..
        } => Some((options, *selected)),
        _ => None,
    };

    if let Some((options, selected)) = options_view {
        // the selected option's description is shown below the list
        let desc_h = 6u16;
        let h = inner.height.saturating_sub(desc_h) as usize;
        let top = selected.saturating_sub(h.saturating_sub(1));

        for (n, o) in options.iter().enumerate().skip(top).take(h) {
            let value = option_value(&panel.spec.args, &o.flag);
            let editing_this = n == selected && editing.is_some();
            let shown = match (&value, o.takes_value) {
                (Some(v), true) => v.clone(),
                (Some(_), false) => "on".to_owned(),
                (None, _) => "default".to_owned(),
            };

            let name = format!(" {}  ", o.flag.trim_start_matches("--"));
            let width = (inner.width as usize).saturating_sub(name.chars().count());

            let mut line = match editing {
                Some((_, input)) if editing_this => {
                    Line::from(vec![Span::raw(name), input_box(input, width)])
                }
                _ => Line::from(vec![
                    Span::raw(name),
                    Span::styled(
                        shown,
                        Style::default().fg(if value.is_some() { ACCENT } else { DIM }),
                    ),
                ]),
            };

            if n == selected && !editing_this {
                line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
            }

            line_at(buf, inner.y + (n - top) as u16, line);
        }

        if let Some(o) = options.get(selected) {
            let kind = if editing.is_some() {
                "  Enter applies, Esc cancels."
            } else if o.takes_value {
                ""
            } else {
                " (a switch: Enter turns it on/off)"
            };
            help_at(
                buf,
                inner.bottom().saturating_sub(desc_h),
                format!("{}{kind}", o.description),
            );
        }
    } else if let Mode::Effects { selected, .. } = &panel.mode {
        let h = inner.height.saturating_sub(4) as usize;
        let top = selected.saturating_sub(h.saturating_sub(1));

        for (n, e) in panel.effects.iter().enumerate().skip(top).take(h) {
            let mut line = Line::from(format!(" {}", e.name));

            if n == *selected {
                line = line.patch_style(Style::default().fg(KEY).add_modifier(Modifier::REVERSED));
            }

            line_at(buf, inner.y + (n - top) as u16, line);
        }

        if let Some(e) = panel.effects.get(*selected) {
            help_at(buf, inner.bottom().saturating_sub(3), e.description.clone());
        }
    } else {
        for (i, row) in ROWS.iter().enumerate() {
            let mut line = match editing {
                Some((field, input)) if field.row() == Some(*row) => Line::from(vec![
                    label(row_label(*row)),
                    input_box(input, (inner.width as usize).saturating_sub(LABEL_W)),
                ]),
                _ => row_line(app, panel, *row),
            };

            if i == panel.selected && !matches!(panel.mode, Mode::PickRegion) {
                line = line.patch_style(Style::default().add_modifier(Modifier::REVERSED));
            }

            line_at(buf, inner.y + i as u16, line);
        }

        let help = match editing {
            Some((field, _)) => format!("{}  Enter applies, Esc cancels.", field.help()),
            None => row_help(panel, ROWS[panel.selected]),
        };

        help_at(buf, inner.y + ROWS.len() as u16 + 1, help);

        // the dropdown opens just under its row, over the rows below
        if let Mode::Dropdown {
            row,
            choices,
            selected,
        } = &panel.mode
        {
            let y = dropdown_top(inner, *row) + 1;
            let x = inner.x + LABEL_W as u16 - 1;
            let w = inner.width.saturating_sub(LABEL_W as u16 - 1);
            let h = (choices.len() as u16 + 2).min(inner.bottom().saturating_sub(y));
            let rect = Rect::new(x, y, w, h);

            Clear.render(rect, buf);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(KEY));
            let list = block.inner(rect);
            block.render(rect, buf);

            for (n, c) in choices.iter().enumerate().take(list.height as usize) {
                let mut line = Line::from(format!(" {c}"));

                if n == *selected {
                    line =
                        line.patch_style(Style::default().fg(KEY).add_modifier(Modifier::REVERSED));
                }

                Paragraph::new(line)
                    .render(Rect::new(list.x, list.y + n as u16, list.width, 1), buf);
            }
        }
    }

    inner
}

pub fn hints(panel: &FxPanel) -> &'static [(&'static str, &'static str)] {
    match panel.mode {
        Mode::List => &[
            ("↑↓", "select"),
            ("enter", "change"),
            ("r", "re-roll"),
            ("space", "replay"),
            ("I", "apply"),
            ("esc", "close"),
        ],
        Mode::Dropdown { .. } => &[("↑↓", "choose"), ("enter", "select"), ("esc", "cancel")],
        Mode::Effects { .. } => &[
            ("↑↓", "preview effects"),
            ("enter", "choose"),
            ("esc", "cancel"),
        ],
        Mode::Options { .. } => &[
            ("↑↓", "select"),
            ("enter", "set / toggle"),
            ("d", "back to default"),
            ("esc", "done"),
        ],
        Mode::PickRegion => &[
            ("drag", "select region"),
            ("c", "type it"),
            ("esc", "cancel"),
        ],
        Mode::Edit { .. } => &[("type", "a value"), ("enter", "apply"), ("esc", "cancel")],
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{
        activate, apply, handle_key, option_value, parse_args, preview_screen, refresh, set_option,
        Field, FxPanel, Mode, Row,
    };
    use crate::edit::fx::{FxSource, FxSpec, Tool, ToolKind};
    use crate::edit::timeline::Origin;
    use crate::edit::tui::app::tests::app;
    use crate::edit::tui::app::App;
    use crate::edit::{Clip, Edit};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// A stand-in for ttfx: lists one effect, and "animates" by drawing
    /// x, then h, then hi in the canvas's first row.
    fn fake_tool(dir: &std::path::Path) -> Tool {
        let path = dir.join("ttfx");
        let script = r#"#!/bin/sh
case "$*" in *--help*) printf 'Commands:\n  decrypt   Fake decrypt\n  help  Print\n\n'; exit 0;; esac
printf '\033[?25l     \n     \n\0337'
for f in x h hi; do printf '\0338\0337\033[2A%s' "$f"; done
printf '\033[?25h'
"#;
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        Tool {
            path,
            kind: ToolKind::Ttfx,
        }
    }

    fn panel(tool: Tool) -> FxPanel {
        FxPanel {
            tool,
            effects: Vec::new(),
            spec: FxSpec {
                effect: "decrypt".to_owned(),
                source: FxSource::Text {
                    text: "hi".to_owned(),
                },
                args: Vec::new(),
                speed: 1.0,
                duration: None,
                seed: 1,
                keep_colors: false,
            },
            hold: 0.5,
            replace: false,
            last_text: "hi".to_owned(),
            selected: 0,
            mode: Mode::List,
            preview: None,
            natural: None,
            drag: None,
            capture_due: None,
            job: None,
        }
    }

    fn wait_for_preview(a: &mut App) {
        for _ in 0..300 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            a.tick();

            if a.fx.as_ref().unwrap().preview.is_some() {
                return;
            }
        }

        panic!("no preview captured");
    }

    fn key(a: &mut App, code: KeyCode) {
        handle_key(a, KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn preview_then_insert_an_effect() {
        let (mut a, dir) = app();
        let tool = fake_tool(dir.path());
        a.seek_position(3); // screen: "$ ls"

        a.fx = Some(panel(tool));

        refresh(&mut a);
        assert!(a.is_animating(), "redraws while the preview is prepared");

        // the capture runs in the background; tick until it's in
        wait_for_preview(&mut a);

        // 3 frames: 2 intervals at 1x
        let natural = a.fx.as_ref().unwrap().natural.unwrap();
        assert!((natural.as_secs_f64() - 2.0 / 60.0).abs() < 1e-6);
        assert!(a.edits.edits.is_empty(), "previewing doesn't edit");

        apply(&mut a);
        assert!(a.fx.is_none());

        let [Edit::Insert {
            at: 3,
            clip: Clip::Effect { cache, hold, .. },
        }] = &a.edits.edits[..]
        else {
            panic!("expected one effect insert, got {:?}", a.edits.edits);
        };

        assert_eq!(*hold, 0.5);
        assert!(
            dir.path().join(cache).exists(),
            "frames cached next to the recording"
        );

        // 3 frames at 60fps, then the hold; the playhead is on its first frame
        assert_eq!(
            a.current_frame().and_then(|i| a.timeline.origin(i)),
            Some(Origin::Clip { insert: 0 })
        );
        let clip_items = (0..a.len())
            .filter(|&i| a.timeline.origin(i) == Some(Origin::Clip { insert: 0 }))
            .count();
        assert_eq!(clip_items, 4, "3 frames + the restore");
        // asciicast stores milliseconds
        let expected = 20.0 + 2.0 / 60.0 + 0.5;
        assert!(
            (a.duration().as_secs_f64() - expected).abs() < 0.002,
            "{:?}",
            a.duration()
        );

        // the last frame shows the text on a cleared screen; then it's restored
        a.step(2);
        assert_eq!(a.vt().text()[0].trim_end(), "hi");
        a.step(1);
        assert_eq!(a.vt().text()[0].trim_end(), "$ ls");
    }

    #[test]
    fn options_round_trip_as_args() {
        let mut args = Vec::new();

        set_option(&mut args, "--typing-speed", Some("4"));
        set_option(&mut args, "--final-gradient-stops", Some("ff0000 00ff00"));
        set_option(&mut args, "--a-switch", Some(""));
        assert_eq!(
            args,
            [
                "--typing-speed",
                "4",
                "--final-gradient-stops",
                "ff0000",
                "00ff00",
                "--a-switch"
            ]
        );

        assert_eq!(
            option_value(&args, "--final-gradient-stops").as_deref(),
            Some("ff0000 00ff00")
        );
        assert_eq!(option_value(&args, "--a-switch").as_deref(), Some(""));
        assert_eq!(option_value(&args, "--nope"), None);

        set_option(&mut args, "--typing-speed", Some("2"));
        set_option(&mut args, "--a-switch", None);
        assert_eq!(parse_args(&args).len(), 2);
        assert_eq!(option_value(&args, "--typing-speed").as_deref(), Some("2"));
    }

    #[test]
    fn dropdowns_choose_without_the_bottom_line() {
        let (mut a, dir) = app();
        a.seek_position(3);
        a.fx = Some(panel(fake_tool(dir.path())));

        activate(&mut a, Row::Colors);
        assert!(matches!(
            a.fx.as_ref().unwrap().mode,
            Mode::Dropdown { selected: 0, .. }
        ));
        assert!(a.prompt.is_none(), "no bottom-line prompt");

        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Enter);
        assert!(a.fx.as_ref().unwrap().spec.keep_colors);
        assert!(matches!(a.fx.as_ref().unwrap().mode, Mode::List));

        activate(&mut a, Row::Animate);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Enter);
        assert_eq!(a.fx.as_ref().unwrap().spec.source, FxSource::Screen);

        // esc leaves things as they were
        activate(&mut a, Row::Placement);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Esc);
        assert!(!a.fx.as_ref().unwrap().replace);
    }

    #[test]
    fn target_length_sets_the_pace() {
        let (mut a, dir) = app();
        a.seek_position(3);
        a.fx = Some(panel(fake_tool(dir.path())));
        a.fx.as_mut().unwrap().spec.duration = Some(1.0);

        refresh(&mut a);
        wait_for_preview(&mut a);
        apply(&mut a);

        // 3 frames fitted to 1s, then the 0.5s hold
        let expected = 20.0 + 1.0 + 0.5;
        assert!(
            (a.duration().as_secs_f64() - expected).abs() < 0.002,
            "{:?}",
            a.duration()
        );
    }

    #[test]
    fn replace_the_current_frame() {
        let (mut a, dir) = app();
        a.seek_position(3); // showing "$ ls": the frame drawn by event 2 ("s")
        a.fx = Some(panel(fake_tool(dir.path())));
        a.fx.as_mut().unwrap().replace = true;

        apply(&mut a);

        assert!(matches!(
            &a.edits.edits[..],
            [Edit::Replace {
                from: 2,
                to: 2,
                clip: Clip::Effect { .. }
            }]
        ));

        // event 2 is gone; after the effect the screen is as it was after it
        assert_eq!(a.timeline.position_of_event(2), None);
        let next = a.timeline.position_of_event(3).unwrap();
        assert_eq!(a.timeline.vt_at_position(next).text()[0].trim_end(), "$ ls");

        // d on the effect restores the frame
        a.delete();
        assert!(a.edits.edits.is_empty());
        assert!(a.timeline.position_of_event(2).is_some());
    }

    fn typed(a: &mut App, text: &str) {
        for c in text.chars() {
            key(a, KeyCode::Char(c));
        }
    }

    #[test]
    fn values_are_edited_in_place() {
        let (mut a, dir) = app();
        a.seek_position(3);
        a.fx = Some(panel(fake_tool(dir.path())));

        activate(&mut a, Row::Length);
        assert!(matches!(
            &a.fx.as_ref().unwrap().mode,
            Mode::Edit {
                field: Field::Length,
                ..
            }
        ));
        assert!(a.prompt.is_none(), "no bottom-line prompt");

        // a bad value keeps the editor open
        typed(&mut a, "abc");
        key(&mut a, KeyCode::Enter);
        assert!(matches!(a.fx.as_ref().unwrap().mode, Mode::Edit { .. }));
        assert!(a.message().unwrap().1);

        for _ in 0..3 {
            key(&mut a, KeyCode::Backspace);
        }
        typed(&mut a, "2.5");
        key(&mut a, KeyCode::Enter);
        assert_eq!(a.fx.as_ref().unwrap().spec.duration, Some(2.5));
        assert!(matches!(a.fx.as_ref().unwrap().mode, Mode::List));

        // Esc leaves the value alone
        activate(&mut a, Row::Seed);
        typed(&mut a, "999");
        key(&mut a, KeyCode::Esc);
        assert_eq!(a.fx.as_ref().unwrap().spec.seed, 1);

        // an option value goes back to the options list
        a.fx.as_mut().unwrap().mode = Mode::Options {
            options: vec![crate::edit::fx::OptionInfo {
                flag: "--typing-speed".to_owned(),
                takes_value: true,
                description: String::new(),
            }],
            selected: 0,
        };
        key(&mut a, KeyCode::Enter);
        typed(&mut a, "4");
        key(&mut a, KeyCode::Enter);
        assert_eq!(a.fx.as_ref().unwrap().spec.args, ["--typing-speed", "4"]);
        assert!(matches!(a.fx.as_ref().unwrap().mode, Mode::Options { .. }));
    }

    #[test]
    fn leaving_typed_text_shows_the_screen_and_remembers_the_text() {
        let (mut a, dir) = app();
        a.seek_position(3);
        a.fx = Some(panel(fake_tool(dir.path())));
        refresh(&mut a);
        wait_for_preview(&mut a);

        // typed "hi" -> the whole screen: the typed animation is gone at once
        activate(&mut a, Row::Animate);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Enter);
        assert!(a.fx.as_ref().unwrap().preview.is_none());
        assert!(
            preview_screen(&a).is_none(),
            "the real screen shows meanwhile"
        );

        // back to typed text (the dropdown opens on the current choice): the
        // old text is offered
        activate(&mut a, Row::Animate);
        key(&mut a, KeyCode::Up);
        key(&mut a, KeyCode::Up);
        key(&mut a, KeyCode::Enter);
        let Mode::Edit {
            field: Field::Text,
            input,
            ..
        } = &a.fx.as_ref().unwrap().mode
        else {
            panic!("expected the text editor");
        };
        assert_eq!(input, "hi");
    }

    #[test]
    fn no_animation_while_picking_a_region() {
        let (mut a, dir) = app();
        a.seek_position(3);
        a.fx = Some(panel(fake_tool(dir.path())));
        refresh(&mut a);
        wait_for_preview(&mut a);
        assert!(preview_screen(&a).is_some());

        activate(&mut a, Row::Animate);
        key(&mut a, KeyCode::Down);
        key(&mut a, KeyCode::Enter); // a region
        assert!(matches!(a.fx.as_ref().unwrap().mode, Mode::PickRegion));
        assert!(preview_screen(&a).is_none(), "the real screen, still");
    }
}
