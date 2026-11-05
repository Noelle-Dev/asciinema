//! Editor state and operations, independent of the terminal.
//!
//! Everything here is plain state manipulation so it can be unit-tested
//! without a terminal; `ui` draws it and `mod` maps input onto it.

use std::cell::{Cell, Ref, RefCell};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use avt::Vt;

use crate::asciicast::{self, EventData};
use crate::edit::{render, Edit, EditList, Timeline};

/// Max events to replay incrementally before falling back to a keyframe seek.
const MAX_INCREMENTAL: usize = 2000;

/// Pauses at least this long are stops for "jump to next/previous pause".
pub const PAUSE_THRESHOLD: Duration = Duration::from_millis(500);

/// Delay change per column when dragging an event in event view.
pub const EVENT_VIEW_DRAG_STEP: f64 = 0.1;

const MESSAGE_TTL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineMode {
    /// Width represents time; a tick per event.
    Time,
    /// One column per event; bar height represents the pause before it.
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    SetDelay,
    Cap,
    Scale,
    Goto,
    RenderTo,
    ConfirmRenderInPlace,
    Title,
    Command,
    IdleLimit,
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    pub input: String,
}

#[derive(Debug, Clone)]
pub enum Drag {
    /// Dragging on empty timeline: scrubbing.
    Scrub,
    /// Dragging an event's tick to change the delay before it.
    Event {
        event: usize,
        start_x: u16,
        original_delay: f64,
        /// Seconds of delay change per column moved.
        secs_per_col: f64,
        new_delay: f64,
        /// Time window frozen at drag start, so the ticks don't rescale
        /// under the mouse as the recording gets longer or shorter.
        frozen_window: Option<(Duration, Duration)>,
    },
}

struct Playback {
    started: Instant,
    from: Duration,
}

pub struct App {
    pub path: PathBuf,
    pub edits_path: PathBuf,
    pub timeline: Timeline,
    pub edits: EditList,
    undo_stack: Vec<Vec<Edit>>,
    redo_stack: Vec<Vec<Edit>>,

    /// Number of events applied to the screen. The playhead sits in the
    /// pause *before* event `position`, which is what delay edits target.
    pub position: usize,
    pub time: Duration,
    playback: Option<Playback>,

    pub mode: TimelineMode,
    /// Time view zoom: the visible span is `duration / zoom`.
    pub zoom: f64,
    /// Time view: start of the visible window.
    pub time_view_start: Cell<Duration>,
    /// Event view: index of the first visible event.
    pub event_view_start: Cell<usize>,

    pub fullscreen: bool,
    pub show_help: bool,
    /// Manual pan offset (col, row) into the recorded screen; `None` auto-scrolls
    /// to keep the recorded terminal's cursor in view.
    pub pan: Option<(usize, usize)>,
    /// Offset chosen while auto-scrolling; sticky to avoid jitter.
    pub auto_scroll_offset: Cell<(usize, usize)>,

    pub selection_anchor: Option<usize>,
    /// Metadata side panel, when open.
    pub meta: Option<super::meta::Panel>,
    pub prompt: Option<Prompt>,
    pub drag: Option<Drag>,
    message: Option<(String, bool, Instant)>,
    pub quit: bool,

    vt_cache: RefCell<(usize, Vt)>,
}

impl App {
    pub fn new(path: PathBuf, edits_path: PathBuf, timeline: Timeline, edits: EditList) -> Self {
        let vt = timeline.vt_at_position(0);

        Self {
            path,
            edits_path,
            timeline,
            edits,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            position: 0,
            time: Duration::ZERO,
            playback: None,
            mode: TimelineMode::Time,
            zoom: 1.0,
            time_view_start: Cell::new(Duration::ZERO),
            event_view_start: Cell::new(0),
            fullscreen: false,
            show_help: false,
            pan: None,
            auto_scroll_offset: Cell::new((0, 0)),
            selection_anchor: None,
            meta: None,
            prompt: None,
            drag: None,
            message: None,
            quit: false,
            vt_cache: RefCell::new((0, vt)),
        }
    }

    // ---- screen state -------------------------------------------------

    /// Terminal state at the playhead. Stepping forward is incremental;
    /// anything else restores from the nearest keyframe.
    ///
    /// The cache is only borrowed mutably when it's out of date, so callers
    /// may hold the returned `Ref` while calling other `&self` methods.
    pub fn vt(&self) -> Ref<'_, Vt> {
        if self.vt_cache.borrow().0 != self.position {
            let mut cache = self.vt_cache.borrow_mut();
            let (pos, vt) = &mut *cache;

            if *pos < self.position && self.position - *pos <= MAX_INCREMENTAL {
                self.timeline.advance(vt, *pos, self.position);
            } else {
                *vt = self.timeline.vt_at_position(self.position);
            }

            *pos = self.position;
        }

        Ref::map(self.vt_cache.borrow(), |(_, vt)| vt)
    }

    // ---- messages -----------------------------------------------------

    pub fn info(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), false, Instant::now()));
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), true, Instant::now()));
    }

    /// Current status message and whether it's an error.
    pub fn message(&self) -> Option<(&str, bool)> {
        self.message
            .as_ref()
            .filter(|(_, _, at)| at.elapsed() < MESSAGE_TTL)
            .map(|(m, err, _)| (m.as_str(), *err))
    }

    // ---- navigation ---------------------------------------------------

    pub fn len(&self) -> usize {
        self.timeline.len()
    }

    pub fn duration(&self) -> Duration {
        self.timeline.duration()
    }

    /// Time of the last applied event (where stepping puts the playhead).
    fn time_of_position(&self, position: usize) -> Duration {
        position
            .checked_sub(1)
            .and_then(|i| self.timeline.event(i))
            .map_or(Duration::ZERO, |(t, _)| t)
    }

    pub fn seek_position(&mut self, position: usize) {
        self.stop();
        self.position = position.min(self.len());
        self.time = self.time_of_position(self.position);
    }

    pub fn seek_time(&mut self, t: Duration) {
        self.stop();
        self.set_time(t);
    }

    fn set_time(&mut self, t: Duration) {
        self.time = t.min(self.duration());
        self.position = self.timeline.position_at(self.time);
    }

    pub fn step(&mut self, delta: isize) {
        let p = self.position.saturating_add_signed(delta);
        self.seek_position(p);
    }

    fn delay_secs(&self, event: usize) -> f64 {
        self.timeline.delay(event).unwrap_or_default().as_secs_f64()
    }

    /// Moves into the next pause of at least [`PAUSE_THRESHOLD`], ready to edit it.
    pub fn next_pause(&mut self) {
        let found = (self.position + 1..self.len())
            .find(|&i| self.timeline.delay(i).is_some_and(|d| d >= PAUSE_THRESHOLD));

        match found {
            Some(i) => self.seek_position(i),
            None => self.seek_position(self.len()),
        }
    }

    pub fn prev_pause(&mut self) {
        let found = (0..self.position)
            .rev()
            .find(|&i| self.timeline.delay(i).is_some_and(|d| d >= PAUSE_THRESHOLD));

        self.seek_position(found.unwrap_or(0));
    }

    fn is_marker(&self, i: usize) -> bool {
        matches!(self.timeline.event(i), Some((_, EventData::Marker(_))))
    }

    pub fn next_marker(&mut self) {
        match (self.position..self.len()).find(|&i| self.is_marker(i)) {
            Some(m) => self.seek_position(m + 1),
            None => self.info("No more markers"),
        }
    }

    pub fn prev_marker(&mut self) {
        // the marker we're sitting right after doesn't count
        match (0..self.position.saturating_sub(1))
            .rev()
            .find(|&i| self.is_marker(i))
        {
            Some(m) => self.seek_position(m + 1),
            None => self.info("No earlier markers"),
        }
    }

    // ---- playback -----------------------------------------------------

    pub fn is_playing(&self) -> bool {
        self.playback.is_some()
    }

    pub fn toggle_play(&mut self) {
        if self.playback.is_some() {
            self.stop();
        } else {
            if self.time >= self.duration() {
                self.time = Duration::ZERO;
                self.position = 0;
            }

            self.playback = Some(Playback {
                started: Instant::now(),
                from: self.time,
            });
        }
    }

    fn stop(&mut self) {
        self.playback = None;
    }

    /// Advances playback; call regularly.
    pub fn tick(&mut self) {
        if let Some(pb) = &self.playback {
            let t = pb.from + pb.started.elapsed();
            self.set_time(t);

            if t >= self.duration() {
                self.stop();
            }
        }
    }

    // ---- selection ----------------------------------------------------

    pub fn toggle_selection(&mut self) {
        self.selection_anchor = match self.selection_anchor {
            Some(_) => None,
            None => Some(self.position),
        };
    }

    /// Selected events (inclusive), whose delays range edits apply to.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        let last = self.len().checked_sub(1)?;
        let (lo, hi) = (anchor.min(self.position), anchor.max(self.position));

        Some((lo.min(last), hi.min(last)))
    }

    /// Selection, or the whole recording if nothing is selected.
    fn selection_or_all(&self) -> Option<(usize, usize)> {
        self.selection()
            .or_else(|| self.len().checked_sub(1).map(|last| (0, last)))
    }

    // ---- editing ------------------------------------------------------

    /// Event whose preceding pause the playhead is in.
    pub fn target_event(&self) -> Option<usize> {
        (self.position < self.len()).then_some(self.position)
    }

    /// Validates, records for undo, saves and applies `edit`.
    pub fn commit(&mut self, edit: Edit) {
        let before = self.edits.edits.clone();

        match self.edits.push(edit.clone()) {
            Ok(_) => {
                self.undo_stack.push(before);
                self.redo_stack.clear();
                self.after_change(format!("{} (saved)", crate::cmd::edit::describe(&edit)));
            }

            Err(e) => self.error(e.to_string()),
        }
    }

    fn after_change(&mut self, msg: String) {
        self.timeline.apply_edits(&self.edits);
        // Keep showing the same frame; only its time moved.
        self.time = self.time_of_position(self.position);

        match self.edits.save(&self.edits_path) {
            Ok(()) => self.info(msg),
            Err(e) => self.error(format!("couldn't save edits: {e:#}")),
        }
    }

    pub fn undo(&mut self) {
        match self.undo_stack.pop() {
            Some(prev) => {
                let current = std::mem::replace(&mut self.edits.edits, prev);
                self.redo_stack.push(current);
                self.after_change("Undone".to_owned());
            }

            None => self.info("Nothing to undo"),
        }
    }

    pub fn redo(&mut self) {
        match self.redo_stack.pop() {
            Some(next) => {
                let current = std::mem::replace(&mut self.edits.edits, next);
                self.undo_stack.push(current);
                self.after_change("Redone".to_owned());
            }

            None => self.info("Nothing to redo"),
        }
    }

    pub fn nudge(&mut self, delta: f64) {
        match self.target_event() {
            Some(event) => {
                let secs = round_ms((self.delay_secs(event) + delta).max(0.0));
                self.commit(Edit::SetDelay { event, secs });
            }

            None => self.info("At the end - no pause to edit"),
        }
    }

    pub fn set_delay(&mut self, secs: f64) {
        match self.target_event() {
            Some(event) => self.commit(Edit::SetDelay { event, secs }),
            None => self.info("At the end - no pause to edit"),
        }
    }

    pub fn cap(&mut self, max: f64) {
        if let Some((from, to)) = self.selection_or_all() {
            self.commit(Edit::CapIdle { from, to, max });
        }
    }

    pub fn scale(&mut self, factor: f64) {
        if let Some((from, to)) = self.selection_or_all() {
            self.commit(Edit::Scale { from, to, factor });
        }
    }

    // ---- dragging -----------------------------------------------------

    pub fn start_event_drag(
        &mut self,
        event: usize,
        x: u16,
        secs_per_col: f64,
        frozen_window: Option<(Duration, Duration)>,
    ) {
        self.stop();
        let original_delay = self.delay_secs(event);

        self.drag = Some(Drag::Event {
            event,
            start_x: x,
            original_delay,
            secs_per_col,
            new_delay: original_delay,
            frozen_window,
        });

        self.seek_position(event);
    }

    /// Updates a drag in progress, previewing the new timing without saving.
    pub fn update_event_drag(&mut self, x: u16) {
        let Some(Drag::Event {
            event,
            start_x,
            original_delay,
            secs_per_col,
            new_delay,
            ..
        }) = &mut self.drag
        else {
            return;
        };

        let cols = x as f64 - *start_x as f64;
        *new_delay = round_ms((*original_delay + cols * *secs_per_col).max(0.0));

        let mut preview = self.edits.clone();
        let event = *event;
        let secs = *new_delay;

        if preview.push(Edit::SetDelay { event, secs }).is_ok() {
            self.timeline.apply_edits(&preview);
            self.time = self.time_of_position(self.position);
        }
    }

    pub fn finish_drag(&mut self) {
        if let Some(Drag::Event {
            event,
            original_delay,
            new_delay,
            ..
        }) = self.drag.take()
        {
            if (new_delay - original_delay).abs() >= 0.0005 {
                self.commit(Edit::SetDelay {
                    event,
                    secs: new_delay,
                });
            } else {
                self.timeline.apply_edits(&self.edits);
            }
        }
    }

    pub fn cancel_drag(&mut self) {
        if matches!(self.drag.take(), Some(Drag::Event { .. })) {
            self.timeline.apply_edits(&self.edits);
            self.time = self.time_of_position(self.position);
        }
    }

    // ---- view ---------------------------------------------------------

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            TimelineMode::Time => TimelineMode::Event,
            TimelineMode::Event => TimelineMode::Time,
        };
    }

    pub fn zoom_by(&mut self, factor: f64) {
        self.zoom = (self.zoom * factor).clamp(1.0, 10_000.0);
    }

    /// Visible time window (start, span) for the time view, `width` columns wide.
    pub fn time_window(&self) -> (Duration, Duration) {
        if let Some(Drag::Event {
            frozen_window: Some(w),
            ..
        }) = &self.drag
        {
            return *w;
        }

        let duration = self.duration().max(Duration::from_millis(1));
        let span = duration.div_f64(self.zoom);
        let max_start = duration.saturating_sub(span);
        let mut start = self.time_view_start.get().min(max_start);

        if self.time < start || self.time > start + span {
            start = self.time.saturating_sub(span / 2).min(max_start);
        }

        self.time_view_start.set(start);

        (start, span)
    }

    /// First visible event in event view, keeping the playhead in view.
    pub fn event_window(&self, width: usize) -> usize {
        let width = width.max(1);
        let mut start = self.event_view_start.get();
        let target = self.position.min(self.len().saturating_sub(1));

        if target < start || target >= start + width {
            start = target.saturating_sub(width / 2);
        }

        self.event_view_start.set(start);

        start
    }

    pub fn pan_by(&mut self, dx: isize, dy: isize) {
        let (x, y) = self.pan.unwrap_or_else(|| self.auto_scroll_offset.get());
        self.pan = Some((x.saturating_add_signed(dx), y.saturating_add_signed(dy)));
    }

    /// Viewport offset into the recorded screen for a `w`x`h` preview area.
    pub fn viewport_offset(&self, w: usize, h: usize) -> (usize, usize) {
        let vt = self.vt();
        let (cols, rows) = vt.size();
        let max = (cols.saturating_sub(w), rows.saturating_sub(h));

        let (x, y) = match self.pan {
            Some(p) => p,

            None => {
                let c = vt.cursor();
                let (mut x, mut y) = self.auto_scroll_offset.get();

                if c.col < x {
                    x = c.col;
                } else if c.col >= x + w {
                    x = c.col + 1 - w;
                }

                if c.row < y {
                    y = c.row;
                } else if c.row >= y + h {
                    y = c.row + 1 - h;
                }

                let offset = (x.min(max.0), y.min(max.1));
                self.auto_scroll_offset.set(offset);

                offset
            }
        };

        (x.min(max.0), y.min(max.1))
    }

    // ---- prompts ------------------------------------------------------

    pub fn open_prompt(&mut self, kind: PromptKind) {
        let input = match kind {
            PromptKind::SetDelay => self
                .target_event()
                .map(|e| format!("{:.3}", self.delay_secs(e)))
                .unwrap_or_default(),

            PromptKind::Title => self.timeline.header.title.clone().unwrap_or_default(),
            PromptKind::Command => self.timeline.header.command.clone().unwrap_or_default(),
            PromptKind::IdleLimit => self
                .timeline
                .header
                .idle_time_limit
                .map(|s| s.to_string())
                .unwrap_or_default(),

            _ => String::new(),
        };

        self.prompt = Some(Prompt { kind, input });
    }

    pub fn submit_prompt(&mut self) {
        let Some(Prompt { kind, input }) = self.prompt.take() else {
            return;
        };

        let input = input.trim();

        let number = || -> Option<f64> { input.parse::<f64>().ok().filter(|v| v.is_finite()) };

        match kind {
            PromptKind::SetDelay => match number() {
                Some(v) => self.set_delay(v),
                None => self.error(format!("not a number: '{input}'")),
            },

            PromptKind::Cap => match number() {
                Some(v) => self.cap(v),
                None => self.error(format!("not a number: '{input}'")),
            },

            PromptKind::Scale => match number() {
                Some(v) => self.scale(v),
                None => self.error(format!("not a number: '{input}'")),
            },

            PromptKind::Goto => match number() {
                Some(v) if v >= 0.0 => self.seek_time(Duration::from_secs_f64(v)),
                _ => self.error(format!("not a time: '{input}'")),
            },

            PromptKind::Title => self.commit(Edit::SetTitle {
                title: (!input.is_empty()).then(|| input.to_owned()),
            }),

            PromptKind::Command => self.commit(Edit::SetCommand {
                command: (!input.is_empty()).then(|| input.to_owned()),
            }),

            PromptKind::IdleLimit => match input {
                "" | "none" => self.commit(Edit::SetIdleTimeLimit { secs: None }),

                _ => match number() {
                    Some(v) => self.commit(Edit::SetIdleTimeLimit { secs: Some(v) }),
                    None => self.error(format!("not a number: '{input}'")),
                },
            },

            PromptKind::RenderTo => {
                if input.is_empty() {
                    return;
                }

                let out = PathBuf::from(input);

                match render::render_to_path(&self.timeline, &self.path, &out, false) {
                    Ok(()) => self.info(format!("Rendered to {}", out.display())),
                    Err(e) => self.error(format!("{e:#}")),
                }
            }

            PromptKind::ConfirmRenderInPlace => {
                if input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes") {
                    self.render_in_place();
                } else {
                    self.info("Render in place cancelled");
                }
            }
        }
    }

    pub fn prompt_label(&self, kind: PromptKind) -> String {
        let scope = match self.selection() {
            Some((a, b)) => format!("events {a}..{b}"),
            None => "all events".to_owned(),
        };

        match kind {
            PromptKind::SetDelay => match self.target_event() {
                Some(e) => format!("Delay before event {e} (secs): "),
                None => "Delay (secs): ".to_owned(),
            },

            PromptKind::Cap => format!("Cap pauses in {scope} at (secs): "),
            PromptKind::Scale => format!("Scale pauses in {scope} by: "),
            PromptKind::Goto => "Go to time (secs): ".to_owned(),
            PromptKind::RenderTo => "Render to file: ".to_owned(),
            PromptKind::Title => "Title (empty to remove): ".to_owned(),
            PromptKind::Command => "Command (empty to remove): ".to_owned(),
            PromptKind::IdleLimit => "Idle time limit in secs (empty to remove): ".to_owned(),

            PromptKind::ConfirmRenderInPlace => format!(
                "Overwrite {} with {} edit(s)? Previous version kept as .bak [y/N]: ",
                self.path.display(),
                self.edits.edits.len()
            ),
        }
    }

    fn render_in_place(&mut self) {
        if let Err(e) = self.try_render_in_place() {
            self.error(format!("render in place failed: {e:#}"));
        }
    }

    fn try_render_in_place(&mut self) -> Result<()> {
        let backup = render::render_in_place(&self.timeline, &self.path, &self.edits_path)?;

        // Reload: the edited recording is now the original.
        let timeline = Timeline::load(asciicast::open_from_path(&self.path)?)?;
        self.edits = EditList::new(timeline.len());
        self.timeline = timeline;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.selection_anchor = None;
        *self.vt_cache.borrow_mut() = (0, self.timeline.vt_at_position(0));
        self.seek_position(self.position);

        self.info(format!(
            "Rendered into {} (previous version: {})",
            self.path.display(),
            backup.display()
        ));

        Ok(())
    }
}

fn round_ms(secs: f64) -> f64 {
    (secs * 1000.0).round() / 1000.0
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;
    use std::time::Duration;

    use tempfile::{tempdir, TempDir};

    use super::{App, PromptKind};
    use crate::asciicast::{Event, Header};
    use crate::edit::{Edit, EditList, Timeline};

    /// Events at 1, 2, 10, 11, 11.1 (marker), 20 seconds.
    pub(crate) fn app() -> (App, TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("demo.cast");
        let edits_path = EditList::sidecar_path(&path);

        let s = |v: f64| Duration::from_secs_f64(v);

        let events = vec![
            Event::output(s(1.0), "$ ".to_owned()),
            Event::output(s(2.0), "l".to_owned()),
            Event::output(s(10.0), "s".to_owned()),
            Event::output(s(11.0), "\r\nfile\r\n".to_owned()),
            Event::marker(s(11.1), "m".to_owned()),
            Event::output(s(20.0), "$ ".to_owned()),
        ];

        let header = Header {
            term_cols: 20,
            term_rows: 5,
            ..Default::default()
        };

        let tl = Timeline::new(header, events);
        let edits = EditList::new(tl.len());

        (App::new(path, edits_path, tl, edits), dir)
    }

    fn secs(app: &App, event: usize) -> f64 {
        app.timeline.delay(event).unwrap().as_secs_f64()
    }

    #[test]
    fn stepping_and_screen() {
        let (mut app, _d) = app();

        app.step(3);
        assert_eq!(app.position, 3);
        assert_eq!(app.time, Duration::from_secs(10));
        assert_eq!(app.vt().text()[0].trim_end(), "$ ls");

        app.step(-10);
        assert_eq!(app.position, 0);
        assert_eq!(app.vt().text()[0].trim_end(), "");

        app.step(100);
        assert_eq!(app.position, 6);
        assert_eq!(app.target_event(), None);
    }

    #[test]
    fn pause_jumps_land_in_the_pause() {
        let (mut app, _d) = app();

        app.next_pause();
        assert_eq!(app.position, 1); // pause before event 1 (1s)
        app.next_pause();
        assert_eq!(app.position, 2); // 8s pause before event 2
        assert_eq!(app.target_event(), Some(2));
        app.next_pause();
        assert_eq!(app.position, 3); // 1s pause before event 3
        app.next_pause();
        assert_eq!(app.position, 5); // skips the 0.1s pause before the marker
        app.prev_pause();
        assert_eq!(app.position, 3);
    }

    #[test]
    fn markers() {
        let (mut app, _d) = app();

        app.next_marker();
        assert_eq!(app.position, 5);
        app.next_marker();
        assert_eq!(app.position, 5);
        app.prev_marker();
        assert_eq!(app.position, 5);
    }

    #[test]
    fn nudge_set_and_undo_redo() {
        let (mut app, _d) = app();
        app.seek_position(2);

        app.nudge(-5.0);
        assert!((secs(&app, 2) - 3.0).abs() < 1e-9);
        app.nudge(-100.0);
        assert_eq!(secs(&app, 2), 0.0);
        // repeated nudges of one event coalesce into one entry
        assert_eq!(app.edits.edits.len(), 1);

        app.undo();
        assert!((secs(&app, 2) - 3.0).abs() < 1e-9);
        app.undo();
        assert!((secs(&app, 2) - 8.0).abs() < 1e-9);
        app.redo();
        assert!((secs(&app, 2) - 3.0).abs() < 1e-9);

        // edits are autosaved
        let saved = EditList::load_or_new(&app.edits_path, app.len()).unwrap();
        assert_eq!(saved, app.edits);

        // the playhead stays on the same frame after a retime
        assert_eq!(app.position, 2);
        assert_eq!(app.time, Duration::from_secs(2));
    }

    #[test]
    fn cap_selection_or_everything() {
        let (mut app, _d) = app();

        app.seek_position(1);
        app.toggle_selection();
        app.seek_position(2);
        app.cap(1.0);
        assert_eq!(
            app.edits.edits,
            [Edit::CapIdle {
                from: 1,
                to: 2,
                max: 1.0
            }]
        );
        assert!((secs(&app, 5) - 8.9).abs() < 1e-6);

        app.toggle_selection();
        app.cap(0.5);
        assert_eq!(
            app.edits.edits[1],
            Edit::CapIdle {
                from: 0,
                to: 5,
                max: 0.5
            }
        );
    }

    #[test]
    fn prompts() {
        let (mut app, _d) = app();
        app.seek_position(2);

        app.open_prompt(PromptKind::SetDelay);
        assert_eq!(app.prompt.as_ref().unwrap().input, "8.000");
        app.prompt.as_mut().unwrap().input = "1.5".to_owned();
        app.submit_prompt();
        assert!((secs(&app, 2) - 1.5).abs() < 1e-9);

        app.open_prompt(PromptKind::Scale);
        app.prompt.as_mut().unwrap().input = "nope".to_owned();
        app.submit_prompt();
        assert!(app.message().unwrap().1, "should be an error");

        app.open_prompt(PromptKind::Goto);
        // edited times are now 1, 2, 3.5, 4.5, 4.6, 13.5
        app.prompt.as_mut().unwrap().input = "4.0".to_owned();
        app.submit_prompt();
        assert_eq!(app.position, 3);
    }

    #[test]
    fn drag_previews_then_commits_once() {
        let (mut app, _d) = app();

        app.start_event_drag(2, 10, 0.5, None);
        app.update_event_drag(4); // -6 cols * 0.5 = -3s
        assert!((secs(&app, 2) - 5.0).abs() < 1e-9);
        assert!(app.edits.edits.is_empty(), "not committed while dragging");

        app.update_event_drag(0); // -5s
        app.finish_drag();
        assert_eq!(
            app.edits.edits,
            [Edit::SetDelay {
                event: 2,
                secs: 3.0
            }]
        );

        app.start_event_drag(2, 10, 0.5, None);
        app.update_event_drag(30);
        app.cancel_drag();
        assert!((secs(&app, 2) - 3.0).abs() < 1e-9);
        assert_eq!(app.edits.edits.len(), 1);
    }

    #[test]
    fn render_in_place_reloads() {
        let (mut app, dir) = app();
        // the in-memory fixture needs to exist on disk for render in place
        crate::edit::render::render_to_path(&app.timeline, Path::new("x"), &app.path, false)
            .unwrap();

        app.seek_position(2);
        app.set_delay(1.0);
        app.open_prompt(PromptKind::ConfirmRenderInPlace);
        app.prompt.as_mut().unwrap().input = "y".to_owned();
        app.submit_prompt();

        assert!(app.edits.edits.is_empty());
        assert!(!app.edits_path.exists());
        assert!(dir.path().join("demo.cast.bak").exists());
        assert!((secs(&app, 2) - 1.0).abs() < 1e-3);
        assert_eq!(app.duration(), Duration::from_secs(13));
    }

    #[test]
    fn auto_scroll_viewport() {
        let (mut app, _d) = app();
        app.step(4); // cursor on row 2 after "file\r\n"

        assert_eq!(app.viewport_offset(10, 2), (0, 1));
        app.pan_by(3, 0);
        assert_eq!(app.viewport_offset(10, 2), (3, 1));
        app.pan_by(100, 100);
        assert_eq!(app.viewport_offset(10, 2), (10, 3)); // clamped
    }
}
