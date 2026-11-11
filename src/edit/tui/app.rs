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
use crate::edit::timeline::Origin;
use crate::edit::{render, Clip, Edit, EditList, Timeline};

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
    /// Single-key menu: what to insert.
    InsertMenu,
    InsertHold,
    InsertBlank,
    InsertText,
    /// Second step of a text card: how long (the text is in `stash`).
    InsertTextSecs,
    InsertRecording,
    /// Second step of inserting a recording: which events (path in `stash`).
    InsertRecordingRange,
    Restore,
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    pub input: String,
    /// Input from an earlier step of a multi-step prompt.
    pub stash: String,
}

#[derive(Debug, Clone)]
pub enum Drag {
    /// Dragging on empty timeline: scrubbing.
    Scrub,
    /// Dragging an event's tick to change the delay before it.
    Event {
        /// Playlist item being dragged.
        item: usize,
        /// Original event whose delay changes.
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
    /// Effect side panel, when open.
    pub fx: Option<super::fxpanel::FxPanel>,
    /// Text editor, when open.
    pub text: Option<super::textedit::TextEditor>,
    pub prompt: Option<Prompt>,
    pub drag: Option<Drag>,
    message: Option<(String, bool, Instant)>,
    pub quit: bool,

    /// (structure version, position, screen) - positions shift when the
    /// playlist's structure changes, so the version is part of the key.
    vt_cache: RefCell<(u64, usize, Vt)>,
}

impl App {
    pub fn new(path: PathBuf, edits_path: PathBuf, timeline: Timeline, edits: EditList) -> Self {
        let vt = timeline.vt_at_position(0);
        let timeline_version = timeline.structure_version;

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
            fx: None,
            text: None,
            prompt: None,
            drag: None,
            message: None,
            quit: false,
            vt_cache: RefCell::new((timeline_version, 0, vt)),
        }
    }

    // ---- screen state -------------------------------------------------

    /// Terminal state at the playhead. Stepping forward is incremental;
    /// anything else restores from the nearest keyframe.
    ///
    /// The cache is only borrowed mutably when it's out of date, so callers
    /// may hold the returned `Ref` while calling other `&self` methods.
    pub fn vt(&self) -> Ref<'_, Vt> {
        let version = self.timeline.structure_version;
        let stale = {
            let (v, pos, _) = &*self.vt_cache.borrow();
            *v != version || *pos != self.position
        };

        if stale {
            let mut cache = self.vt_cache.borrow_mut();
            let (v, pos, vt) = &mut *cache;

            if *v == version && *pos < self.position && self.position - *pos <= MAX_INCREMENTAL {
                self.timeline.advance(vt, *pos, self.position);
            } else {
                *vt = self.timeline.vt_at_position(self.position);
            }

            *v = version;
            *pos = self.position;
        }

        Ref::map(self.vt_cache.borrow(), |(_, _, vt)| vt)
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

    /// Delay before playlist item `i`, in seconds.
    fn delay_secs(&self, i: usize) -> f64 {
        self.timeline.delay(i).unwrap_or_default().as_secs_f64()
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

    /// Whether the screen is animating (playback or an effect preview), so
    /// the UI should redraw often.
    pub fn is_animating(&self) -> bool {
        self.is_playing()
            || self
                .fx
                .as_ref()
                .is_some_and(|f| f.preview.is_some() || f.busy())
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
        super::fxpanel::tick(self);

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

    /// The selected frames, as the playlist items that drew them (inclusive):
    /// from the frame showing when `v` was pressed to the one showing now.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        let last = self.len().checked_sub(1)?;
        let frame = |p: usize| p.saturating_sub(1).min(last);
        let (a, b) = (frame(anchor), frame(self.position));

        Some((a.min(b), a.max(b)))
    }

    /// The item that drew the frame on screen (`None` before the first one).
    pub fn current_frame(&self) -> Option<usize> {
        self.position.checked_sub(1).filter(|&i| i < self.len())
    }

    /// The selected frames, or else the one on screen.
    pub fn frames(&self) -> Option<(usize, usize)> {
        self.selection()
            .or_else(|| self.current_frame().map(|i| (i, i)))
    }

    /// Original events covered by playlist items `lo..=hi` (cut ranges included).
    pub fn events_in(&self, lo: usize, hi: usize) -> Option<(usize, usize)> {
        let mut range: Option<(usize, usize)> = None;

        for i in lo..=hi {
            let (a, b) = match self.timeline.origin(i) {
                Some(Origin::Event(e)) => (e, e),
                Some(Origin::Cut { from, to }) => (from, to),
                _ => continue,
            };

            range = Some(range.map_or((a, b), |(x, y)| (x.min(a), y.max(b))));
        }

        range
    }

    /// Original events in the selection, or all of them if nothing is selected.
    fn selected_events_or_all(&self) -> Option<(usize, usize)> {
        match self.selection() {
            Some((lo, hi)) => self.events_in(lo, hi),
            None => self
                .timeline
                .original_len()
                .checked_sub(1)
                .map(|last| (0, last)),
        }
    }

    // ---- editing ------------------------------------------------------

    /// Original event whose (editable) preceding pause the playhead is in.
    /// `None` at the end, or inside an inserted clip.
    pub fn target_event(&self) -> Option<usize> {
        // text edits are drawn in no time: the pause is the next item's
        let mut i = self.position;

        while let Some(Origin::TextEdit { .. }) = self.timeline.origin(i) {
            i += 1;
        }

        self.timeline.delay_owner(i)
    }

    fn no_pause_here(&mut self) {
        if self.position >= self.len() {
            self.info("At the end - no pause to edit");
        } else {
            self.info("Inside an inserted clip - its length is part of the clip");
        }
    }

    /// Validates, records for undo, saves and applies `edit`.
    pub fn commit(&mut self, edit: Edit) {
        let description = crate::cmd::edit::describe(&edit);
        self.transact(format!("{description} (saved)"), |edits| {
            edits.push(edit).map(|_| ())
        });
    }

    /// Changes the edit list as one undoable step. If the change is invalid,
    /// or the result can't be built (e.g. a clip's file is missing), it's
    /// rolled back and the error shown.
    pub fn transact(&mut self, msg: String, change: impl FnOnce(&mut EditList) -> Result<()>) {
        let before = self.edits.edits.clone();

        if let Err(e) = change(&mut self.edits) {
            self.edits.edits = before;
            self.error(format!("{e:#}"));
            return;
        }

        if let Err(e) = self.rebuild() {
            self.edits.edits = before;
            let _ = self.rebuild();
            self.error(format!("{e:#}"));
            return;
        }

        self.undo_stack.push(before);
        self.redo_stack.clear();
        self.saved(msg);
    }

    /// Re-applies the edit list, keeping the playhead on the same frame.
    fn rebuild(&mut self) -> Result<()> {
        // anchor on the original event the playhead is before
        let anchor = (self.position..self.len()).find_map(|i| match self.timeline.origin(i) {
            Some(Origin::Event(e)) => Some(e),
            _ => None,
        });

        let version = self.timeline.structure_version;
        self.timeline.apply_edits(&self.edits)?;

        if self.timeline.structure_version != version {
            self.position = match anchor {
                // still there: the same frame, before it
                Some(e) if self.timeline.position_of_event(e).is_some() => {
                    self.timeline.position_of_event(e).unwrap_or(0)
                }

                // now cut: at the cut, before its jump
                // now cut: at the cut, before its jump; replaced: the next event
                Some(e) => (0..self.len())
                    .find(|&i| {
                        matches!(self.timeline.origin(i), Some(Origin::Cut { from, to }) if (from..=to).contains(&e))
                    })
                    .or_else(|| {
                        (e..self.timeline.original_len())
                            .find_map(|e| self.timeline.position_of_event(e))
                    })
                    .unwrap_or(self.len()),

                None => self.len(),
            };
        }

        self.position = self.position.min(self.len());
        self.time = self.time_of_position(self.position);

        Ok(())
    }

    fn saved(&mut self, msg: String) {
        match self.edits.save(&self.edits_path) {
            Ok(()) => self.info(msg),
            Err(e) => self.error(format!("couldn't save edits: {e:#}")),
        }
    }

    fn swap_history(&mut self, undo: bool) {
        let (from, to) = if undo {
            (&mut self.undo_stack, &mut self.redo_stack)
        } else {
            (&mut self.redo_stack, &mut self.undo_stack)
        };

        let Some(edits) = from.pop() else {
            self.info(if undo {
                "Nothing to undo"
            } else {
                "Nothing to redo"
            });
            return;
        };

        let current = std::mem::replace(&mut self.edits.edits, edits);
        to.push(current);

        if let Err(e) = self.rebuild() {
            self.error(format!("{e:#}"));
            return;
        }

        self.saved(if undo { "Undone" } else { "Redone" }.to_owned());
    }

    pub fn undo(&mut self) {
        self.swap_history(true);
    }

    pub fn redo(&mut self) {
        self.swap_history(false);
    }

    /// The editable pause the playhead is in, in seconds.
    fn target_delay_secs(&self) -> Option<f64> {
        self.target_event()
            .and_then(|e| self.timeline.event_delay(e))
            .map(|d| d.as_secs_f64())
    }

    pub fn nudge(&mut self, delta: f64) {
        match self.target_event() {
            Some(event) => {
                let now = self.target_delay_secs().unwrap_or_default();
                let secs = round_ms((now + delta).max(0.0));
                self.commit(Edit::SetDelay { event, secs });
            }

            None => self.no_pause_here(),
        }
    }

    pub fn set_delay(&mut self, secs: f64) {
        match self.target_event() {
            Some(event) => self.commit(Edit::SetDelay { event, secs }),
            None => self.no_pause_here(),
        }
    }

    pub fn cap(&mut self, max: f64) {
        if let Some((from, to)) = self.selected_events_or_all() {
            self.commit(Edit::CapIdle { from, to, max });
        }
    }

    pub fn scale(&mut self, factor: f64) {
        if let Some((from, to)) = self.selected_events_or_all() {
            self.commit(Edit::Scale { from, to, factor });
        }
    }

    // ---- cutting & inserting ------------------------------------------------

    /// Original position a clip inserted at the playhead goes to.
    pub fn insert_position(&self) -> usize {
        match self.timeline.origin(self.position) {
            Some(Origin::Event(e)) => e,
            Some(Origin::Cut { to, .. }) => to + 1,
            Some(Origin::Clip { insert }) => self.insert_edit(insert).map_or(0, |(_, at)| at),
            Some(Origin::TextEdit { edit }) => self.text_edit(edit).map_or(0, |(_, at)| at),
            // a track's drawing between events: before the next one
            Some(Origin::Track { .. }) => (self.position..self.len())
                .find_map(|i| match self.timeline.origin(i) {
                    Some(Origin::Event(e)) => Some(e),
                    _ => None,
                })
                .unwrap_or(self.timeline.original_len()),
            None => self.timeline.original_len(),
        }
    }

    /// The `k`-th insert edit: (index in the edit list, position).
    fn insert_edit(&self, k: usize) -> Option<(usize, usize)> {
        self.edits
            .edits
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e {
                Edit::Insert { at, .. } => Some((i, *at)),
                Edit::Replace { from, .. } => Some((i, *from)),
                _ => None,
            })
            .nth(k)
    }

    /// The `k`-th text edit: (index in the edit list, position).
    fn text_edit(&self, k: usize) -> Option<(usize, usize)> {
        self.edits
            .edits
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e {
                Edit::Text { at, .. } => Some((i, *at)),
                _ => None,
            })
            .nth(k)
    }

    /// Cuts the selected frames, or the frame on screen (the events behind
    /// them, and any inserted clips among them).
    pub fn delete(&mut self) {
        let Some((lo, hi)) = self.frames() else {
            self.info("Nothing on screen yet - step forward to a frame to cut it");
            return;
        };

        let mut events: Option<(usize, usize)> = None;
        let mut clips = Vec::new();
        let mut texts = Vec::new();

        for i in lo..=hi {
            match self.timeline.origin(i) {
                Some(Origin::Event(e)) => {
                    events = Some(events.map_or((e, e), |(a, b)| (a.min(e), b.max(e))));
                }

                Some(Origin::Clip { insert }) if !clips.contains(&insert) => clips.push(insert),
                Some(Origin::TextEdit { edit }) if !texts.contains(&edit) => texts.push(edit),
                _ => {}
            }
        }

        // text edits are removed when they're what's selected, not when
        // their drawing happens to fall inside a range of events being cut
        if events.is_some() {
            texts.clear();
        }

        if events.is_none() && clips.is_empty() && texts.is_empty() {
            self.info("Already cut - D restores it");
            return;
        }

        let remove: Vec<usize> = clips
            .iter()
            .filter_map(|&k| self.insert_edit(k).map(|(i, _)| i))
            .chain(
                texts
                    .iter()
                    .filter_map(|&k| self.text_edit(k).map(|(i, _)| i)),
            )
            .collect();

        let msg = match (events, clips.len(), texts.len()) {
            (Some((a, b)), 0, _) => format!("Cut events {a}..{b} (saved; D restores)"),
            (Some((a, b)), n, _) => {
                format!("Cut events {a}..{b} and {n} inserted clip(s) (saved)")
            }
            (None, 0, n) => format!("Removed {n} text edit(s) (saved)"),
            (None, n, 0) => format!("Removed {n} inserted clip(s) (saved)"),
            (None, n, m) => format!("Removed {n} inserted clip(s) and {m} text edit(s) (saved)"),
        };

        self.selection_anchor = None;

        self.transact(msg, |list| {
            let mut i = 0;
            list.edits.retain(|_| {
                let keep = !remove.contains(&i);
                i += 1;
                keep
            });

            if let Some((from, to)) = events {
                list.push(Edit::Delete { from, to })?;
            }

            Ok(())
        });
    }

    /// The cut nearest the playhead (or in the selection), as an event range.
    pub fn cut_to_restore(&self) -> Option<(usize, usize)> {
        if let Some((lo, hi)) = self.selection() {
            let (a, b) = self.events_in(lo, hi)?;
            let cut: Vec<usize> = (a..=b).filter(|&e| self.edits.is_deleted(e)).collect();

            return Some((*cut.first()?, *cut.last()?));
        }

        [self.position, self.position.wrapping_sub(1)]
            .iter()
            .find_map(|&i| match self.timeline.origin(i) {
                Some(Origin::Cut { from, to }) => Some((from, to)),
                _ => None,
            })
    }

    pub fn restore(&mut self, from: usize, to: usize) {
        self.selection_anchor = None;

        self.transact(format!("Restored events {from}..{to} (saved)"), |list| {
            if list.restore(from, to) {
                Ok(())
            } else {
                anyhow::bail!("nothing is cut in {from}..{to}")
            }
        });
    }

    /// Number of clips (inserts and replaces) so far; the next one's number.
    fn clip_count(&self) -> usize {
        self.edits
            .edits
            .iter()
            .filter(|e| matches!(e, Edit::Insert { .. } | Edit::Replace { .. }))
            .count()
    }

    /// Puts the playhead on clip `k`'s first frame, so it's on screen (and
    /// `d` would remove it).
    fn seek_to_clip(&mut self, k: usize) {
        if let Some(start) =
            (0..self.len()).find(|&i| self.timeline.origin(i) == Some(Origin::Clip { insert: k }))
        {
            self.seek_position(start + 1);
        }
    }

    /// Inserts `clip` at the playhead, leaving the playhead at its start.
    pub fn insert(&mut self, clip: Clip) {
        let at = self.insert_position();
        let msg = format!("Inserted {} (saved)", clip.describe());
        let k = self.clip_count();

        self.transact(msg, |list| list.push(Edit::Insert { at, clip }).map(|_| ()));
        self.seek_to_clip(k);
    }

    /// Plays `clip` instead of original events `from..=to`, leaving the
    /// playhead at its start.
    pub fn replace(&mut self, from: usize, to: usize, clip: Clip) {
        let msg = format!(
            "Replaced events {from}..{to} with {} (saved)",
            clip.describe()
        );
        let k = self.clip_count();
        self.selection_anchor = None;

        self.transact(msg, |list| {
            list.push(Edit::Replace { from, to, clip }).map(|_| ())
        });
        self.seek_to_clip(k);
    }

    /// Copies the selected events to right after the selection.
    pub fn duplicate_selection(&mut self) {
        let Some((from, to)) = self.selection().and_then(|(lo, hi)| self.events_in(lo, hi)) else {
            self.info("Select a range first (v), then duplicate it");
            return;
        };

        self.selection_anchor = None;
        self.seek_position(
            self.timeline
                .position_of_event(to)
                .map_or(self.len(), |p| p + 1),
        );
        self.insert(Clip::Recording {
            path: None,
            from,
            to: Some(to),
        });
    }

    // ---- dragging -----------------------------------------------------

    /// Starts dragging playlist item `item` to change the pause before it.
    pub fn start_event_drag(
        &mut self,
        item: usize,
        x: u16,
        secs_per_col: f64,
        frozen_window: Option<(Duration, Duration)>,
    ) {
        self.stop();

        let Some(event) = self.timeline.delay_owner(item) else {
            self.seek_position(item);
            self.info("That's part of an inserted clip - its timing is fixed");
            return;
        };

        let original_delay = self.delay_secs(item);

        self.drag = Some(Drag::Event {
            item,
            event,
            start_x: x,
            original_delay,
            secs_per_col,
            new_delay: original_delay,
            frozen_window,
        });

        self.seek_position(item);
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

        // timing only, so this can't fail to build
        if preview.push(Edit::SetDelay { event, secs }).is_ok()
            && self.timeline.apply_edits(&preview).is_ok()
        {
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
                let _ = self.rebuild();
            }
        }
    }

    pub fn cancel_drag(&mut self) {
        if matches!(self.drag.take(), Some(Drag::Event { .. })) {
            let _ = self.rebuild();
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
                .target_delay_secs()
                .map(|d| format!("{d:.3}"))
                .unwrap_or_default(),

            PromptKind::InsertHold | PromptKind::InsertBlank => "2".to_owned(),

            PromptKind::Restore => match self.cut_to_restore() {
                Some((a, b)) => format!("{a}..{b}"),
                None => {
                    self.info("No cut here - move the playhead to a ✂ or select across one");
                    return;
                }
            },

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

        self.prompt = Some(Prompt {
            kind,
            input,
            stash: String::new(),
        });
    }

    pub fn submit_prompt(&mut self) {
        let Some(Prompt { kind, input, stash }) = self.prompt.take() else {
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

            // the menus act on a single key press (choose_insert, choose_text)
            PromptKind::InsertMenu => {}

            PromptKind::InsertHold | PromptKind::InsertBlank => match number() {
                Some(secs) if kind == PromptKind::InsertHold => self.insert(Clip::Hold { secs }),
                Some(secs) => self.insert(Clip::Blank { secs }),
                None => self.error(format!("not a number: '{input}'")),
            },

            PromptKind::InsertText => {
                if input.is_empty() {
                    return;
                }

                // literal \n starts a new line, since the prompt is one line
                self.prompt = Some(Prompt {
                    kind: PromptKind::InsertTextSecs,
                    input: "3".to_owned(),
                    stash: input.replace("\\n", "\n"),
                });
            }

            PromptKind::InsertTextSecs => match number() {
                Some(secs) => self.insert(Clip::Text { text: stash, secs }),
                None => self.error(format!("not a number: '{input}'")),
            },

            PromptKind::InsertRecording => {
                if input.is_empty() {
                    return;
                }

                self.prompt = Some(Prompt {
                    kind: PromptKind::InsertRecordingRange,
                    input: String::new(),
                    stash: input.to_owned(),
                });
            }

            PromptKind::InsertRecordingRange => match parse_range(input) {
                Some((from, to)) => self.insert(Clip::Recording {
                    path: Some(stash),
                    from,
                    to,
                }),
                None => self.error(format!(
                    "not a range: '{input}' (e.g. 10..200, 10.., or empty for all)"
                )),
            },

            PromptKind::Restore => match parse_range(input) {
                Some((from, Some(to))) => self.restore(from, to),
                _ => self.error(format!("not a range: '{input}' (e.g. 10..20)")),
            },
        }
    }

    /// Handles a key press in the insert menu.
    pub fn choose_insert(&mut self, key: char) {
        self.prompt = None;

        match key {
            'h' => self.open_prompt(PromptKind::InsertHold),
            'b' => self.open_prompt(PromptKind::InsertBlank),
            't' => self.open_prompt(PromptKind::InsertText),
            'r' => self.open_prompt(PromptKind::InsertRecording),
            'd' => self.duplicate_selection(),
            _ => {}
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

            PromptKind::InsertMenu => {
                "Insert: h hold · b blank screen · t text card · r another recording · d duplicate selection (Esc cancels) "
                    .to_owned()
            }

            PromptKind::InsertHold => "Hold the screen for (secs): ".to_owned(),
            PromptKind::InsertBlank => "Blank screen for (secs): ".to_owned(),
            PromptKind::InsertText => "Text card (\\n for a new line): ".to_owned(),
            PromptKind::InsertTextSecs => "Show it for (secs): ".to_owned(),
            PromptKind::InsertRecording => "Recording to insert (path): ".to_owned(),
            PromptKind::InsertRecordingRange => {
                "Its events, FROM..TO (empty for all, FROM.. to the end): ".to_owned()
            }
            PromptKind::Restore => "Restore cut events (FROM..TO, part of a cut is fine): ".to_owned(),

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
        let mut timeline = Timeline::load(asciicast::open_from_path(&self.path)?)?;

        if let Some(dir) = self.path.parent() {
            timeline.set_clip_base(dir);
        }

        self.edits = EditList::new(timeline.original_len());
        self.timeline = timeline;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.selection_anchor = None;
        *self.vt_cache.borrow_mut() = (
            self.timeline.structure_version,
            0,
            self.timeline.vt_at_position(0),
        );
        self.seek_position(self.position);

        self.info(format!(
            "Rendered into {} (previous version: {})",
            self.path.display(),
            backup.display()
        ));

        Ok(())
    }
}

/// Parses `FROM..TO` / `FROM..` / `N` / empty (everything) into an event range.
fn parse_range(s: &str) -> Option<(usize, Option<usize>)> {
    let s = s.trim();

    if s.is_empty() {
        return Some((0, None));
    }

    match s.split_once("..") {
        Some((a, b)) => {
            let from = if a.trim().is_empty() {
                0
            } else {
                a.trim().parse().ok()?
            };
            let to = if b.trim().is_empty() {
                None
            } else {
                Some(b.trim().parse().ok()?)
            };

            Some((from, to))
        }

        None => s.parse().ok().map(|n| (n, Some(n))),
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
    use crate::edit::{Clip, Edit, EditList, Timeline};

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

        let mut tl = Timeline::new(header, events);
        tl.set_clip_base(dir.path());
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

        // the frames drawn by events 1 and 2
        app.seek_position(2);
        app.toggle_selection();
        app.seek_position(3);
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

    fn row(app: &App, r: usize) -> String {
        app.vt().text()[r].trim_end().to_owned()
    }

    #[test]
    fn d_cuts_the_frame_on_screen() {
        let (mut app, _d) = app();

        app.seek_position(0);
        app.delete();
        assert!(app.edits.edits.is_empty(), "nothing on screen yet");

        app.seek_position(2); // showing "$ l", drawn by event 1
        app.delete();
        assert_eq!(app.edits.edits, [Edit::Delete { from: 1, to: 1 }]);
    }

    #[test]
    fn cut_selection_then_restore_part_of_it() {
        use crate::edit::timeline::Origin;

        let (mut app, _d) = app();
        app.seek_position(2); // the frame drawn by event 1 ("l") ...
        app.toggle_selection();
        app.seek_position(3); // ... to the one drawn by event 2 ("s")
        app.delete();

        assert_eq!(app.edits.edits, [Edit::Delete { from: 1, to: 2 }]);
        assert_eq!(app.timeline.position_of_event(1), None);
        assert_eq!(app.selection_anchor, None);

        // the playhead is on the cut's jump: the screen looks as it did
        // (the jump shows what the cut part drew)
        let cut = app.current_frame().and_then(|i| app.timeline.origin(i));
        assert_eq!(cut, Some(Origin::Cut { from: 1, to: 2 }));
        assert_eq!(row(&app, 0), "$ ls");

        // D prefills the whole cut; restoring just event 2 splits it
        app.seek_position(1);
        app.open_prompt(PromptKind::Restore);
        assert_eq!(app.prompt.as_ref().unwrap().input, "1..2");
        app.prompt.as_mut().unwrap().input = "2..2".to_owned();
        app.submit_prompt();
        assert_eq!(app.edits.edits, [Edit::Delete { from: 1, to: 1 }]);

        app.undo();
        assert_eq!(app.edits.edits, [Edit::Delete { from: 1, to: 2 }]);
        app.undo();
        assert!(app.edits.edits.is_empty());
        assert_eq!(app.len(), 6);
    }

    #[test]
    fn inserting_clips() {
        use crate::edit::timeline::Origin;

        let (mut app, _d) = app();
        app.seek_position(3); // after "$ ls", before "\r\nfile"

        // menu -> text -> secs
        app.open_prompt(PromptKind::InsertMenu);
        app.choose_insert('t');
        app.prompt.as_mut().unwrap().input = "Hello\\nthere".to_owned();
        app.submit_prompt();
        assert_eq!(
            app.prompt.as_ref().unwrap().kind,
            PromptKind::InsertTextSecs
        );
        app.submit_prompt(); // default 3s

        assert_eq!(
            app.edits.edits,
            [Edit::Insert {
                at: 3,
                clip: Clip::Text {
                    text: "Hello\nthere".to_owned(),
                    secs: 3.0
                }
            }]
        );

        // the playhead is at the start of the clip, and stepping in shows the card
        assert_eq!(
            app.timeline.origin(app.position),
            Some(Origin::Clip { insert: 0 })
        );
        app.step(1);
        // two lines centered on the 20x5 screen: rows 1 and 2
        assert_eq!(row(&app, 1), "       Hello");
        assert_eq!(row(&app, 2), "       there");
        assert_eq!(app.target_event(), None, "clip timing isn't editable");
        assert_eq!(app.duration(), Duration::from_secs(23));

        // d inside the clip removes it
        app.delete();
        assert!(app.edits.edits.is_empty());
        assert_eq!(app.duration(), Duration::from_secs(20));

        // a missing recording is rejected and nothing is kept
        app.seek_position(0);
        app.open_prompt(PromptKind::InsertRecording);
        app.prompt.as_mut().unwrap().input = "missing.cast".to_owned();
        app.submit_prompt();
        app.submit_prompt(); // range: all
        assert!(app.message().unwrap().1, "should be an error");
        assert!(app.edits.edits.is_empty());
        assert_eq!(app.len(), 6);
    }

    #[test]
    fn duplicate_selection() {
        let (mut app, _d) = app();
        app.seek_position(2);
        app.toggle_selection();
        app.seek_position(3);
        app.duplicate_selection();

        assert_eq!(
            app.edits.edits,
            [Edit::Insert {
                at: 3,
                clip: Clip::Recording {
                    path: None,
                    from: 1,
                    to: Some(2)
                }
            }]
        );
    }

    #[test]
    fn retiming_still_targets_original_events_after_a_cut() {
        let (mut app, _d) = app();
        app.seek_position(1);
        app.toggle_selection();
        app.seek_position(2);
        app.delete(); // cut events 0..1

        // the playhead is at the cut's jump, whose pause is event 2's
        assert_eq!(app.target_event(), Some(2));
        app.set_delay(0.5);
        assert!(app.edits.edits.contains(&Edit::SetDelay {
            event: 2,
            secs: 0.5
        }));
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
