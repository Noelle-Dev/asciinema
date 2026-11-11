//! In-memory timeline: the recording assembled with all edits applied, with
//! fast random access to terminal state (scrubbing).
//!
//! The timeline is a *playlist* built from the original events plus the
//! structural edits (cuts and inserted clips). One invariant keeps it
//! correct: at any point corresponding to original position `p` (outside a
//! clip), the screen looks exactly like the original recording's screen at
//! `p`. Cuts are jump cuts that end in a redraw of the screen as it was after
//! the cut, and clips end in a redraw of the screen as it was before them, so
//! everything around an edit plays exactly as recorded.
//!
//! Reconstructing the screen at an arbitrary point requires replaying events
//! through a virtual terminal. To keep seeking cheap, terminal state is
//! snapshotted ("keyframes") at regular intervals in one linear pass; a seek
//! restores the nearest preceding keyframe and replays only what follows.
//!
//! Keyframes are keyed by playlist position, not time, so retiming never
//! invalidates them; only structural edits rebuild them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use avt::Vt;

use super::composite::{self, Painter};
use super::edits::{Clip, Track, TrackSource};
use super::{retime, Edit, EditList};
use crate::asciicast::{self, Asciicast, Event, EventData, Header, Version};

/// Take a keyframe after at least this many bytes of output since the last one...
const KEYFRAME_BYTES: usize = 16 * 1024;

/// ...or after this many events, whichever comes first.
const KEYFRAME_EVENTS: usize = 1000;

/// How long the last frame of a clip that runs to its source's end stays up.
const CLIP_TAIL: Duration = Duration::from_secs(1);

/// Puts a terminal in the state avt's screen dump expects: primary screen,
/// default modes (soft reset, which leaves colors and cursor shape alone),
/// cleared, cursor home.
pub(crate) const REDRAW_PREFIX: &str = "\x1b[?1049l\x1b[!p\x1b[H\x1b[2J";

/// Where a playlist item came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// An original event.
    Event(usize),
    /// The jump at a cut of original events `from..=to`.
    Cut { from: usize, to: usize },
    /// Part of the `insert`-th inserted clip (counting insert edits in order).
    Clip { insert: usize },
    /// Drawing of the `edit`-th text edit (counting text edits in order).
    TextEdit { edit: usize },
    /// Drawing of the `track`-th track (counting track edits in order).
    Track { track: usize },
}

#[derive(Clone)]
enum ItemData {
    Original(usize),
    Owned(EventData),
}

#[derive(Clone)]
enum Delay {
    /// The (edited) delay of an original event.
    Of(usize),
    Fixed(Duration),
}

#[derive(Clone)]
struct Item {
    data: ItemData,
    delay: Delay,
    origin: Origin,
}

/// The original recording with text edits drawn into it: original events,
/// plus the patches drawing each edit. Cuts and clips work on top of this,
/// so their redraws show the edits that are active at that point.
struct Base {
    items: Vec<ItemData>,
    /// Index in `items` of original position `p` (before event `p`, after
    /// the patches at `p`), for `p` in `0..=n`.
    pos: Vec<usize>,
    keyframes: Keyframes,
    /// Patches drawn at original position `p` (before event `p`): (edit, output).
    pre: Vec<Vec<(usize, String)>>,
}

impl Base {
    fn data<'a>(&'a self, events: &'a [Event], i: usize) -> &'a EventData {
        match &self.items[i] {
            ItemData::Original(e) => &events[*e].data,
            ItemData::Owned(d) => d,
        }
    }
}

/// Another recording used as a clip source.
struct Source {
    events: Vec<Event>,
    keyframes: Keyframes,
}

pub struct Timeline {
    pub header: Header,
    /// Format version of the file this was loaded from, if loaded from one.
    pub source_version: Option<Version>,
    /// Bumped whenever the playlist's structure changes (positions shift).
    pub structure_version: u64,

    /// Header as recorded, before metadata edits.
    original_header: Header,
    events: Vec<Event>,
    original_times: Vec<Duration>,
    original_keyframes: Keyframes,
    /// Edited delay before each original event.
    delays: Vec<Duration>,

    items: Vec<Item>,
    times: Vec<Duration>,
    keyframes: Keyframes,
    /// Playlist position of each original event (None when cut).
    event_pos: Vec<Option<usize>>,
    /// The structural edits the playlist was built from.
    structure: Vec<Edit>,

    /// The recording with its text edits, when there are any.
    base: Option<Base>,

    /// The playlist before tracks are drawn over it, and its keyframes.
    main: Vec<Item>,
    main_keyframes: Keyframes,
    /// The track edits the playlist was composited with: (at, track).
    tracks: Vec<(usize, Track)>,

    /// Directory that relative clip paths are resolved against.
    clip_base: PathBuf,
    sources: Mutex<HashMap<PathBuf, Arc<Source>>>,
}

impl Timeline {
    pub fn load(cast: Asciicast<'_>) -> Result<Self> {
        let events = cast.events.collect::<Result<Vec<_>>>()?;
        let mut timeline = Self::new(cast.header, events);
        timeline.source_version = Some(cast.version);

        Ok(timeline)
    }

    pub fn new(header: Header, events: Vec<Event>) -> Self {
        let original_times: Vec<Duration> = events.iter().map(|e| e.time).collect();
        let size = (header.term_cols as usize, header.term_rows as usize);
        let original_keyframes = Keyframes::build(size, events.iter().map(|e| &e.data));

        let items: Vec<Item> = (0..events.len())
            .map(|i| Item {
                data: ItemData::Original(i),
                delay: Delay::Of(i),
                origin: Origin::Event(i),
            })
            .collect();

        Self {
            original_header: header.clone(),
            source_version: None,
            structure_version: 0,
            header,
            delays: retime::delays(&original_times),
            times: original_times.clone(),
            event_pos: (0..events.len()).map(Some).collect(),
            keyframes: original_keyframes.clone(),
            main_keyframes: original_keyframes.clone(),
            original_keyframes,
            original_times,
            events,
            main: items.clone(),
            tracks: Vec::new(),
            items,
            structure: Vec::new(),
            base: None,
            clip_base: PathBuf::from("."),
            sources: Mutex::new(HashMap::new()),
        }
    }

    /// Sets the directory relative clip paths are resolved against.
    pub fn set_clip_base(&mut self, dir: &Path) {
        self.clip_base = dir.to_owned();
    }

    /// Number of items in the playlist.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number of events in the original recording.
    pub fn original_len(&self) -> usize {
        self.events.len()
    }

    fn data(&self, i: usize) -> &EventData {
        item_data(&self.events, &self.items[i])
    }

    /// Playlist item `i`, with its time as currently edited.
    pub fn event(&self, i: usize) -> Option<(Duration, &EventData)> {
        (i < self.items.len()).then(|| (self.times[i], self.data(i)))
    }

    pub fn origin(&self, i: usize) -> Option<Origin> {
        self.items.get(i).map(|item| item.origin)
    }

    /// Playlist position of original event `e`, unless it's cut.
    pub fn position_of_event(&self, e: usize) -> Option<usize> {
        self.event_pos.get(e).copied().flatten()
    }

    /// The original event whose (editable) delay comes before playlist item `i`.
    pub fn delay_owner(&self, i: usize) -> Option<usize> {
        let item = self.items.get(i)?;

        match (&item.delay, item.origin) {
            (Delay::Of(e), _) => Some(*e),
            // an event right after a cut's jump: its pause is the jump's
            (Delay::Fixed(_), Origin::Event(e)) => Some(e),
            (Delay::Fixed(_), _) => None,
        }
    }

    /// Edited pause before original event `e`.
    pub fn event_delay(&self, e: usize) -> Option<Duration> {
        self.delays.get(e).copied()
    }

    /// Pause before original event `e` in the unedited recording.
    pub fn original_event_delay(&self, e: usize) -> Option<Duration> {
        delay_in(&self.original_times, e)
    }

    /// Current (edited) delay before playlist item `i`.
    pub fn delay(&self, i: usize) -> Option<Duration> {
        delay_in(&self.times, i)
    }

    /// Whether the delay before playlist item `i` was retimed.
    pub fn is_retimed(&self, i: usize) -> bool {
        self.delay_owner(i)
            .is_some_and(|e| Some(self.delays[e]) != delay_in(&self.original_times, e))
    }

    /// Total duration as currently edited.
    pub fn duration(&self) -> Duration {
        self.times.last().copied().unwrap_or_default()
    }

    /// Total duration of the original, unedited recording.
    pub fn original_duration(&self) -> Duration {
        self.original_times.last().copied().unwrap_or_default()
    }

    /// Header as recorded, before metadata edits.
    pub fn original_header(&self) -> &Header {
        &self.original_header
    }

    /// Rebuilds the timeline from the original recording plus `edits`. On
    /// error (e.g. a clip's source recording can't be read) nothing changes.
    pub fn apply_edits(&mut self, edits: &EditList) -> Result<()> {
        let structure: Vec<Edit> = edits
            .edits
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Edit::Delete { .. }
                        | Edit::Insert { .. }
                        | Edit::Replace { .. }
                        | Edit::Text { .. }
                )
            })
            .cloned()
            .collect();

        let tracks: Vec<(usize, Track)> = edits
            .edits
            .iter()
            .filter_map(|e| match e {
                Edit::Track { at, track } => Some((*at, track.clone())),
                _ => None,
            })
            .collect();

        let size = (
            self.original_header.term_cols as usize,
            self.original_header.term_rows as usize,
        );

        // the playlist without tracks: rebuilt when the structure changes
        let rebuilt = if structure != self.structure {
            let base = self.build_base(&structure);
            let items = self.compile(&structure, base.as_ref())?;
            let events = &self.events;
            let keyframes =
                Keyframes::build(size, items.iter().map(|item| item_data(events, item)));

            Some((items, keyframes, base))
        } else {
            None
        };

        let (main, main_keyframes) = match &rebuilt {
            Some((items, keyframes, _)) => (items, keyframes),
            None => (&self.main, &self.main_keyframes),
        };

        let retimed = retime::apply(&self.original_times, &edits.edits);
        let delays = retime::delays(&retimed);
        let mut t = Duration::ZERO;

        let main_times: Vec<Duration> = main
            .iter()
            .map(|item| {
                t += match item.delay {
                    Delay::Of(e) => delays[e],
                    Delay::Fixed(d) => d,
                };

                t
            })
            .collect();

        // tracks drawn over it: redone whenever anything changes, as
        // retiming the recording moves the tracks' events among its own
        let composited = if tracks.is_empty() {
            None
        } else {
            let (items, times) = self.composite(main, main_keyframes, &main_times, &tracks)?;
            let events = &self.events;
            let keyframes =
                Keyframes::build(size, items.iter().map(|item| item_data(events, item)));

            Some((items, times, keyframes))
        };

        // nothing failed: commit
        let changed = rebuilt.is_some() || composited.is_some() || !self.tracks.is_empty();

        if let Some((items, keyframes, base)) = rebuilt {
            self.main = items;
            self.main_keyframes = keyframes;
            self.base = base;
            self.structure = structure;
        }

        self.delays = delays;

        match composited {
            Some((items, times, keyframes)) => {
                self.items = items;
                self.times = times;
                self.keyframes = keyframes;
            }

            None => {
                if changed {
                    self.items = self.main.clone();
                    self.keyframes = self.main_keyframes.clone();
                }

                self.times = main_times;
            }
        }

        if changed {
            self.event_pos = vec![None; self.events.len()];

            for (i, item) in self.items.iter().enumerate() {
                if let Origin::Event(e) = item.origin {
                    self.event_pos[e].get_or_insert(i);
                }
            }

            self.structure_version += 1;
        }

        self.tracks = tracks;

        self.header = super::edits::apply_header(&self.original_header, &edits.edits);

        Ok(())
    }

    /// Number of playlist items that have happened by time `t` (i.e. the
    /// index of the next one to be played at `t`).
    pub fn position_at(&self, t: Duration) -> usize {
        self.times.partition_point(|&et| et <= t)
    }

    /// Terminal state after the first `next` playlist items have been applied.
    pub fn vt_at_position(&self, next: usize) -> Vt {
        self.keyframes
            .vt_at(next.min(self.items.len()), |i| self.data(i))
    }

    /// Terminal state at time `t` (as currently edited).
    #[allow(dead_code)]
    pub fn vt_at(&self, t: Duration) -> Vt {
        self.vt_at_position(self.position_at(t))
    }

    /// Applies items `from..to` to `vt`, which must hold the state at
    /// position `from`. Lets callers step forward without a full seek.
    pub fn advance(&self, vt: &mut Vt, from: usize, to: usize) {
        let to = to.min(self.items.len());

        for i in from.min(to)..to {
            feed(vt, self.data(i));
        }
    }

    /// Screen of the recording after `p` events, with the text edits active
    /// there (but no cuts or clips).
    pub fn original_vt_at(&self, p: usize) -> Vt {
        base_vt(
            &self.events,
            &self.original_keyframes,
            self.base.as_ref(),
            p,
        )
    }

    /// Playlist events with their edited times.
    /// Whether edits add drawing of their own (cuts, clips, effects, text
    /// edits). Those draw at fixed screen positions, so they only line up
    /// when playback starts on a cleared screen.
    pub fn draws_edits(&self) -> bool {
        !self.structure.is_empty() || !self.tracks.is_empty()
    }

    pub fn edited_events(&self) -> impl Iterator<Item = Event> + '_ {
        (0..self.items.len()).map(|i| Event {
            time: self.times[i],
            data: self.data(i).clone(),
        })
    }

    // ---- building the playlist ----------------------------------------------

    fn compile(&self, structure: &[Edit], base: Option<&Base>) -> Result<Vec<Item>> {
        let screen_at = |p: usize| base_vt(&self.events, &self.original_keyframes, base, p);
        let n = self.events.len();
        let mut deleted = vec![false; n];
        let mut replaced = vec![false; n];
        // clips by position: (clip number, clip, last replaced event if a replace)
        let mut inserts: Vec<Vec<(usize, &Clip, Option<usize>)>> = vec![Vec::new(); n + 1];
        let mut insert_no = 0;

        for edit in structure {
            match edit {
                Edit::Delete { from, to } => {
                    for d in deleted.iter_mut().take((*to).min(n - 1) + 1).skip(*from) {
                        *d = true;
                    }
                }

                Edit::Insert { at, clip } => {
                    inserts[(*at).min(n)].push((insert_no, clip, None));
                    insert_no += 1;
                }

                Edit::Replace { from, to, clip } => {
                    let to = (*to).min(n - 1);

                    for r in replaced.iter_mut().take(to + 1).skip(*from) {
                        *r = true;
                    }

                    inserts[*from].push((insert_no, clip, Some(to)));
                    insert_no += 1;
                }

                _ => {}
            }
        }

        let mut b = Builder {
            items: Vec::new(),
            size: (
                self.original_header.term_cols as usize,
                self.original_header.term_rows as usize,
            ),
        };

        // Original position whose screen the playlist currently shows.
        let mut showing = 0;
        let mut cut_from = None;

        // text edit patches; not inside cuts or replaced events, whose
        // closing redraw shows the edited screen anyway
        let patch = |b: &mut Builder, patches: &[(usize, String)]| {
            for (edit, out) in patches {
                b.push(
                    output(out.clone()),
                    Delay::Fixed(Duration::ZERO),
                    Origin::TextEdit { edit: *edit },
                );
            }
        };

        for p in 0..=n {
            let skipped = p < n && (deleted[p] || replaced[p]);

            if let Some(base) = base {
                if !skipped && cut_from.is_none() {
                    patch(&mut b, &base.pre[p]);
                }
            }

            for &(insert, clip, replaces) in &inserts[p] {
                // a replace ends on the screen as it was after the replaced events
                let restore_to = replaces.map_or(showing, |to| to + 1);
                self.push_clip(
                    &mut b,
                    Origin::Clip { insert },
                    clip,
                    showing,
                    restore_to,
                    &screen_at,
                )?;
                showing = restore_to;
            }

            if p == n {
                break;
            }

            // replaced events are skipped; their clip already moved the screen on
            if replaced[p] && !deleted[p] {
                continue;
            }

            if deleted[p] {
                cut_from.get_or_insert(p);
                continue;
            }

            if let Some(from) = cut_from.take() {
                // jump cut: show the screen as it was after the cut, then go on
                let origin = Origin::Cut { from, to: p - 1 };
                b.redraw(screen_at(p), Delay::Of(p), origin);
                b.push(
                    ItemData::Original(p),
                    Delay::Fixed(Duration::ZERO),
                    Origin::Event(p),
                );
            } else {
                b.push(ItemData::Original(p), Delay::Of(p), Origin::Event(p));
            }

            if let EventData::Resize(cols, rows) = self.events[p].data {
                b.size = (cols as usize, rows as usize);
            }

            showing = p + 1;
        }

        // a cut running to the end still gets an item, so it shows in the editor
        if let Some(from) = cut_from {
            b.push(
                ItemData::Owned(EventData::Output(String::new())),
                Delay::Fixed(Duration::ZERO),
                Origin::Cut { from, to: n - 1 },
            );
        }

        Ok(b.items)
    }

    /// Draws `tracks` over the playlist `main` (played at `times`): returns
    /// the new playlist and its times. Outside tracks the playlist plays as
    /// before; while any is showing, the screen is drawn as the composite
    /// of the main screen and the tracks, as just the cells that change.
    fn composite(
        &self,
        main: &[Item],
        main_keyframes: &Keyframes,
        times: &[Duration],
        tracks: &[(usize, Track)],
    ) -> Result<(Vec<Item>, Vec<Duration>)> {
        let n = self.events.len();

        // where the main playlist reaches original position `p`
        let main_pos = |p: usize| {
            main.iter()
                .position(|item| matches!(item.origin, Origin::Event(e) if e >= p))
                .unwrap_or(main.len())
        };

        let time_at = |pos: usize| pos.checked_sub(1).map_or(Duration::ZERO, |i| times[i]);

        /// A track, resolved: its events (with times on the main timeline)
        /// and its screen when it starts.
        struct Placed<'a> {
            track: &'a Track,
            pos: usize,
            start: Duration,
            end: Duration,
            events: Vec<(Duration, EventData)>,
            vt: Vt,
            /// For a highlight: the area and style it restyles with.
            restyle: Option<(usize, usize, super::style::TextStyle)>,
            /// Where its top-left corner goes.
            offset: super::edits::Offset,
        }

        let mut placed = Vec::with_capacity(tracks.len());
        let screen = (
            self.original_header.term_cols as usize,
            self.original_header.term_rows as usize,
        );

        for (at, track) in tracks {
            let pos = main_pos(*at);
            let start = time_at(pos);
            let until = track.until.map(|u| time_at(main_pos(u.min(n))));

            // tracks the editor draws: one screen, for a while
            if let Some(secs) = track.source.secs() {
                let (vt, restyle, offset) = generated(&track.source, screen);
                let end = start + Duration::from_secs_f64(secs);

                placed.push(Placed {
                    track,
                    pos,
                    start,
                    end: until.map_or(end, |u| end.max(u)),
                    events: Vec::new(),
                    vt,
                    restyle,
                    offset: super::edits::Offset {
                        row: offset.row + track.offset.row,
                        col: offset.col + track.offset.col,
                    },
                });

                continue;
            }

            let TrackSource::Recording { path, from, to } = &track.source else {
                unreachable!("only recordings have no set length");
            };

            let source = match path {
                Some(p) => Some(self.source(p)?),
                None => None,
            };

            let (events, keyframes) = match &source {
                Some(s) => (&s.events, &s.keyframes),
                None => (&self.events, &self.original_keyframes),
            };

            let last = to.unwrap_or(events.len().saturating_sub(1));

            if events.is_empty() || *from > last || last >= events.len() {
                bail!(
                    "track range {from}..{last} is out of range for a recording with {} events",
                    events.len()
                );
            }

            let base = from
                .checked_sub(1)
                .map_or(Duration::ZERO, |i| events[i].time);
            let rel = |t: Duration| start + t.saturating_sub(base);

            let mut end = match events.get(last + 1) {
                Some(next) => rel(next.time),
                None => rel(events[last].time) + CLIP_TAIL,
            };

            if let Some(u) = until {
                end = end.max(u);
            }

            let drawing = events[*from..=last]
                .iter()
                .filter(|e| matches!(e.data, EventData::Output(_) | EventData::Resize(..)))
                .map(|e| (rel(e.time), e.data.clone()))
                .collect();

            placed.push(Placed {
                track,
                pos,
                start,
                end,
                events: drawing,
                vt: keyframes.vt_at(*from, |i| &events[i].data),
                restyle: None,
                offset: track.offset,
            });
        }

        // everything that happens, in order: (time, main position it's
        // before on a tie, kind order, step)
        #[derive(Clone, Copy)]
        enum Step {
            Main(usize),
            Start(usize),
            Event(usize, usize),
            End(usize),
        }

        type Key = (Duration, usize, u8, usize, usize);
        let mut steps: Vec<(Key, Step)> = Vec::new();

        for (i, &t) in times.iter().enumerate() {
            steps.push(((t, i, 1, 0, 0), Step::Main(i)));
        }

        for (k, p) in placed.iter().enumerate() {
            steps.push(((p.start, p.pos, 0, k, 0), Step::Start(k)));

            for (j, (t, _)) in p.events.iter().enumerate() {
                steps.push(((*t, p.pos, 0, k, j + 1), Step::Event(k, j)));
            }

            steps.push(((p.end.max(p.start), p.pos, 0, k, usize::MAX), Step::End(k)));
        }

        // times only go forward: a track's events never come before its start
        steps.sort_by_key(|(key, _)| *key);

        let mut items: Vec<Item> = Vec::with_capacity(main.len() + steps.len());
        let mut out_times: Vec<Duration> = Vec::with_capacity(items.capacity());
        let mut active = vec![false; placed.len()];
        // the main screen, while tracks are showing
        let mut screen: Option<(Vt, Painter)> = None;
        let mut applied = 0; // main items applied so far
        let mut prev = Duration::ZERO;

        let mut emit =
            |items: &mut Vec<Item>, t: Duration, data: ItemData, delay: Option<Delay>, origin| {
                let d = t.saturating_sub(prev);
                prev = prev.max(t);
                items.push(Item {
                    data,
                    delay: delay.unwrap_or(Delay::Fixed(d)),
                    origin,
                });
                out_times.push(t.max(out_times.last().copied().unwrap_or_default()));
            };

        let repaint = |placed: &[Placed], active: &[bool], main: &Vt, painter: &mut Painter| {
            let layers: Vec<composite::Layer> = placed
                .iter()
                .zip(active)
                .filter(|(_, &on)| on)
                .map(|(p, _)| composite::Layer {
                    content: match p.restyle {
                        Some((cols, rows, style)) => {
                            composite::Content::Style { cols, rows, style }
                        }
                        None => composite::Content::Screen(&p.vt),
                    },
                    crop: p.track.crop,
                    offset: p.offset,
                })
                .collect();

            painter.paint(&composite::compose(main, &layers), main)
        };

        for (key, step) in steps {
            let t = key.0;

            match step {
                Step::Main(i) => {
                    let item = &main[i];
                    applied = i + 1;

                    let Some((vt, painter)) = &mut screen else {
                        emit(
                            &mut items,
                            t,
                            item.data.clone(),
                            Some(item.delay.clone()),
                            item.origin,
                        );
                        continue;
                    };

                    let data = item_data(&self.events, item);

                    match data {
                        EventData::Output(_) => {
                            feed(vt, data);
                            let s = repaint(&placed, &active, vt, painter);
                            emit(
                                &mut items,
                                t,
                                output(s),
                                Some(item.delay.clone()),
                                item.origin,
                            );
                        }

                        // resized: start over at the new size
                        EventData::Resize(..) => {
                            feed(vt, data);
                            emit(
                                &mut items,
                                t,
                                item.data.clone(),
                                Some(item.delay.clone()),
                                item.origin,
                            );

                            *painter = Painter::new(vt.size());
                            let s = format!(
                                "{}{}",
                                Painter::start(),
                                repaint(&placed, &active, vt, painter)
                            );
                            emit(&mut items, t, output(s), None, item.origin);
                        }

                        _ => emit(
                            &mut items,
                            t,
                            item.data.clone(),
                            Some(item.delay.clone()),
                            item.origin,
                        ),
                    }
                }

                Step::Start(k) => {
                    active[k] = true;
                    let origin = Origin::Track { track: k };

                    // the first track: from here on the screen is drawn as
                    // the composite, starting on a cleared screen
                    let entering = screen.is_none();

                    let (vt, painter) = screen.get_or_insert_with(|| {
                        let vt =
                            main_keyframes.vt_at(applied, |i| item_data(&self.events, &main[i]));
                        let painter = Painter::new(vt.size());
                        (vt, painter)
                    });

                    let mut s = repaint(&placed, &active, vt, painter);

                    if entering {
                        s = format!("{}{s}", Painter::start());
                    }

                    emit(&mut items, t, output(s), None, origin);
                }

                Step::Event(k, j) => {
                    let p = &mut placed[k];
                    feed(&mut p.vt, &p.events[j].1);

                    if let Some((vt, painter)) = &mut screen {
                        let s = repaint(&placed, &active, vt, painter);
                        emit(&mut items, t, output(s), None, Origin::Track { track: k });
                    }
                }

                Step::End(k) => {
                    active[k] = false;
                    let origin = Origin::Track { track: k };

                    if active.iter().any(|&a| a) {
                        if let Some((vt, painter)) = &mut screen {
                            let s = repaint(&placed, &active, vt, painter);
                            emit(&mut items, t, output(s), None, origin);
                        }
                    } else if let Some((vt, _)) = screen.take() {
                        // the last track's gone: back to the recording as it is
                        let s = format!("{REDRAW_PREFIX}{}", vt.dump());
                        emit(&mut items, t, output(s), None, origin);
                    }
                }
            }
        }

        Ok((items, out_times))
    }

    fn push_clip(
        &self,
        b: &mut Builder,
        origin: Origin,
        clip: &Clip,
        showing: usize,
        restore_to: usize,
        screen_at: &dyn Fn(usize) -> Vt,
    ) -> Result<()> {
        let empty = || ItemData::Owned(EventData::Output(String::new()));
        let secs = |s: f64| Duration::from_secs_f64(s);
        let fixed0 = || Delay::Fixed(Duration::ZERO);

        match clip {
            // the screen doesn't change, so (unless replacing) there's nothing to restore
            Clip::Hold { secs: s } => {
                b.push(empty(), Delay::Fixed(secs(*s)), origin);

                if restore_to == showing {
                    return Ok(());
                }
            }

            Clip::Blank { secs: s } => {
                b.push(output(REDRAW_PREFIX.to_owned()), fixed0(), origin);
                b.push(empty(), Delay::Fixed(secs(*s)), origin);
            }

            Clip::Text { text, secs: s } => {
                let card = format!("{REDRAW_PREFIX}{}", centered(text, b.size));
                b.push(output(card), fixed0(), origin);
                b.push(empty(), Delay::Fixed(secs(*s)), origin);
            }

            Clip::Effect { hold, cache, .. } => {
                // the captured frames draw over the current screen
                let source = self
                    .source(cache)
                    .context("the effect's captured frames are missing; recreate the effect")?;
                let mut prev = Duration::ZERO;

                for event in &source.events {
                    if let EventData::Output(_) = event.data {
                        let delay = event.time.saturating_sub(prev);
                        prev = event.time;
                        b.push(
                            ItemData::Owned(event.data.clone()),
                            Delay::Fixed(delay),
                            origin,
                        );
                    }
                }

                let before = screen_at(restore_to);
                b.redraw(before, Delay::Fixed(secs(*hold)), origin);

                return Ok(());
            }

            Clip::Recording { path, from, to } => {
                let source = match path {
                    Some(p) => Some(self.source(p)?),
                    None => None,
                };

                let (events, keyframes) = match &source {
                    Some(s) => (&s.events, &s.keyframes),
                    None => (&self.events, &self.original_keyframes),
                };

                let last = match to {
                    Some(to) => *to,
                    None => events.len().saturating_sub(1),
                };

                if events.is_empty() || *from > last || last >= events.len() {
                    bail!(
                        "clip range {from}..{last} is out of range for a recording with {} events",
                        events.len()
                    );
                }

                // start from the source's screen at that point, then play its output
                let start = keyframes.vt_at(*from, |i| &events[i].data);
                b.push(
                    output(format!("{REDRAW_PREFIX}{}", start.dump())),
                    fixed0(),
                    origin,
                );

                // keep the source's own timing: the pause before the first
                // event, and how long its last frame stayed up
                let mut prev = from
                    .checked_sub(1)
                    .map_or(Duration::ZERO, |i| events[i].time);
                let mut pending = Duration::ZERO;

                for (i, event) in events.iter().enumerate().take(last + 1).skip(*from) {
                    pending += event.time.saturating_sub(prev);
                    prev = prev.max(event.time);

                    // only output: a pasted resize, exit or marker would misbehave
                    if let EventData::Output(_) = event.data {
                        let data = match path {
                            None => ItemData::Original(i),
                            Some(_) => ItemData::Owned(event.data.clone()),
                        };

                        b.push(data, Delay::Fixed(std::mem::take(&mut pending)), origin);
                    }
                }

                let tail = match events.get(last + 1) {
                    Some(next) => pending + next.time.saturating_sub(prev),
                    None => pending + CLIP_TAIL,
                };

                let before = screen_at(restore_to);
                b.redraw(before, Delay::Fixed(tail), origin);

                return Ok(());
            }
        }

        // put back the screen as it was before the clip (or, replacing, after
        // the replaced events)
        let before = screen_at(restore_to);
        b.redraw(before, fixed0(), origin);

        Ok(())
    }

    /// Builds the recording with its text edits drawn in (see [`Base`]),
    /// or `None` when there are none.
    fn build_base(&self, structure: &[Edit]) -> Option<Base> {
        let edits: Vec<TextEdit> = structure
            .iter()
            .filter_map(|e| match e {
                Edit::Text {
                    at,
                    row,
                    col,
                    from,
                    to,
                    styles,
                } => Some(TextEdit {
                    at: *at,
                    row: *row,
                    col: *col,
                    from: from.chars().collect(),
                    to: to.clone(),
                    styles: super::style::from_runs(styles),
                }),
                _ => None,
            })
            .collect();

        if edits.is_empty() {
            return None;
        }

        let n = self.events.len();
        let size = (
            self.original_header.term_cols as usize,
            self.original_header.term_rows as usize,
        );

        let mut vt = new_vt(size.0, size.1);
        let mut items = Vec::with_capacity(n + edits.len());
        let mut pos = Vec::with_capacity(n + 1);
        let mut pre = vec![Vec::new(); n + 1];

        for (p, patches) in pre.iter_mut().enumerate() {
            // as if retyped here: the old text deleted and the new typed in,
            // the rest of the line moving along, and the cursor too. What the
            // recording does next plays as recorded, over the result.
            for (k, e) in edits.iter().enumerate().filter(|(_, e)| e.at == p) {
                let s = retyped(&vt, e);
                vt.feed_str(&s);
                items.push(output(s.clone()));
                patches.push((k, s));
            }

            pos.push(items.len());

            if p == n {
                break;
            }

            items.push(ItemData::Original(p));
            feed(&mut vt, &self.events[p].data);
        }

        let events = &self.events;
        let keyframes = Keyframes::build(
            size,
            items.iter().map(|d| match d {
                ItemData::Original(e) => &events[*e].data,
                ItemData::Owned(d) => d,
            }),
        );

        Some(Base {
            items,
            pos,
            keyframes,
            pre,
        })
    }

    fn source(&self, path: &str) -> Result<Arc<Source>> {
        let full = self.clip_base.join(path);

        if let Some(s) = self.sources.lock().expect("not poisoned").get(&full) {
            return Ok(s.clone());
        }

        let cast = asciicast::open_from_path(&full)
            .with_context(|| format!("can't load clip recording {}", full.display()))?;

        let size = (
            cast.header.term_cols as usize,
            cast.header.term_rows as usize,
        );
        let events = cast.events.collect::<Result<Vec<_>>>()?;
        let keyframes = Keyframes::build(size, events.iter().map(|e| &e.data));
        let source = Arc::new(Source { events, keyframes });

        self.sources
            .lock()
            .expect("not poisoned")
            .insert(full, source.clone());

        Ok(source)
    }
}

/// The screen a track the editor draws shows (a caption or a box), or for a
/// highlight, the area and style it restyles with; and where it goes on a
/// `screen`-sized terminal (before the track's own offset).
#[allow(clippy::type_complexity)]
fn generated(
    source: &TrackSource,
    (cols, rows): (usize, usize),
) -> (
    Vt,
    Option<(usize, usize, super::style::TextStyle)>,
    super::edits::Offset,
) {
    use super::edits::Offset;
    use super::fx::Anchor;
    use super::style::TextStyle;

    match source {
        TrackSource::Caption {
            text,
            anchor,
            style,
            ..
        } => {
            // with a background, the box is padded and all of it covers (a
            // track's spaces are see-through, so its blanks are drawn as
            // blocks in the background colour)
            let boxed = style.bg.is_some();
            let pad = usize::from(boxed);
            let lines: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
            let w = lines.iter().map(Vec::len).max().unwrap_or(0) + 2 * pad;
            let h = lines.len().max(1);

            let mut vt = new_vt(w.max(1), h);
            let blank = TextStyle {
                fg: style.bg,
                ..*style
            };

            for (r, line) in lines.iter().enumerate() {
                let mut out = format!("\x1b[{};1H", r + 1);
                let padded = std::iter::repeat_n(' ', pad)
                    .chain(line.iter().copied())
                    .chain(std::iter::repeat(' '))
                    .take(w);

                for ch in padded {
                    if ch == ' ' && boxed {
                        out.push_str(&blank.sgr());
                        out.push('█');
                    } else {
                        out.push_str(&style.sgr());
                        out.push(ch);
                    }
                }

                vt.feed_str(&out);
            }

            let col = match anchor {
                Anchor::Nw | Anchor::W | Anchor::Sw => 0,
                Anchor::N | Anchor::C | Anchor::S => cols.saturating_sub(w) / 2,
                Anchor::Ne | Anchor::E | Anchor::Se => cols.saturating_sub(w),
            };

            let row = match anchor {
                Anchor::Nw | Anchor::N | Anchor::Ne => 0,
                Anchor::W | Anchor::C | Anchor::E => rows.saturating_sub(h) / 2,
                Anchor::Sw | Anchor::S | Anchor::Se => rows.saturating_sub(h),
            };

            (
                vt,
                None,
                Offset {
                    row: row as isize,
                    col: col as isize,
                },
            )
        }

        TrackSource::Box {
            cols: w,
            rows: h,
            fill,
            color,
            ..
        } => {
            let mut vt = new_vt(*w, *h);
            let style = TextStyle {
                fg: *color,
                ..Default::default()
            };

            for r in 0..*h {
                let row: String = std::iter::repeat_n(*fill, *w).collect();
                vt.feed_str(&format!("\x1b[{};1H{}{row}", r + 1, style.sgr()));
            }

            (vt, None, Offset::default())
        }

        TrackSource::Highlight {
            cols: w,
            rows: h,
            style,
            ..
        } => (new_vt(1, 1), Some((*w, *h, *style)), Offset::default()),

        TrackSource::Recording { .. } => unreachable!("recordings aren't generated"),
    }
}

/// Screen after `p` original events: of the recording with its text edits
/// (`base`) if there are any, else of the original.
fn base_vt(events: &[Event], original: &Keyframes, base: Option<&Base>, p: usize) -> Vt {
    match base {
        Some(b) => {
            let i = b.pos[p.min(b.pos.len() - 1)];
            b.keyframes.vt_at(i, |j| b.data(events, j))
        }

        None => original.vt_at(p.min(events.len()), |i| &events[i].data),
    }
}

/// A text edit, ready for matching.
struct TextEdit {
    at: usize,
    row: usize,
    col: usize,
    from: Vec<char>,
    to: String,
    /// Per-character styles for `to` (empty: the cells' own).
    styles: Vec<super::style::TextStyle>,
}

/// Drawing instructions changing `width` cells at `row`, `col` to `to`
/// (padded with spaces; a longer `to` overwrites the cells after). Each
/// character is drawn in `styles[i]`, or where that's missing, its cell's
/// existing style. The cursor, pen and modes are left as they were.
pub fn text_patch(
    vt: &Vt,
    row: usize,
    col: usize,
    width: usize,
    to: &str,
    styles: &[super::style::TextStyle],
) -> String {
    let (cols, rows) = vt.size();

    if row >= rows || col >= cols {
        return String::new();
    }

    let line = vt.line(row);
    let cells = line.cells();
    let to: Vec<char> = to.chars().collect();
    let n = width.max(to.len()).min(cols - col);

    // save cursor and pen; origin mode off so the position is absolute
    let mut out = format!("\x1b7\x1b[?6l\x1b[{};{}H", row + 1, col + 1);
    let cell = |i: usize| {
        let style = styles.get(i).copied().unwrap_or_else(|| {
            let cell_pen = cells.get(col + i).map(|c| *c.pen()).unwrap_or_default();
            super::style::TextStyle::from_pen(&cell_pen)
        });

        (to.get(i).copied().unwrap_or(' '), style)
    };

    // A patch that runs to the end of the line ends in erase-to-end-of-line
    // rather than trailing blanks: a narrower terminal would wrap those onto
    // the next line, and it's much less output.
    let blank = |i: usize| {
        let (ch, st) = cell(i);
        ch == ' ' && st == super::style::TextStyle::default()
    };

    let to_eol = col + n == cols;
    let end = if to_eol {
        (0..n).rposition(|i| !blank(i)).map_or(0, |i| i + 1)
    } else {
        n
    };

    let mut pen: Option<super::style::TextStyle> = None;

    for i in 0..end {
        let (ch, style) = cell(i);

        if pen != Some(style) {
            out.push_str(&style.sgr());
            pen = Some(style);
        }

        out.push(ch);
    }

    if to_eol && end < n {
        out.push_str("\x1b[0m\x1b[K");
    }

    // restores the cursor, pen and origin mode
    out.push_str("\x1b8");

    out
}

/// A one-off edit on the screen `vt`, as if it were retyped: `e.to` replaces
/// the `e.from`-wide text at its position, the rest of the line moving along
/// (keeping its own characters and styles) when the lengths differ, and the
/// cursor moving with it when it's after the change on that line.
fn retyped(vt: &Vt, e: &TextEdit) -> String {
    use super::style::TextStyle;

    let (cols, rows) = vt.size();

    if e.row >= rows || e.col >= cols {
        return String::new();
    }

    let cells: Vec<(char, TextStyle)> = vt
        .line(e.row)
        .cells()
        .iter()
        .filter(|c| c.width() > 0)
        .map(|c| (c.char(), TextStyle::from_pen(c.pen())))
        .collect();

    let mut line = replace_in(&cells, &[e.col], e);
    line.resize(cols, (' ', TextStyle::default()));
    line.truncate(cols);

    let (text, styles): (String, Vec<TextStyle>) = line[e.col..].iter().copied().unzip();
    let mut out = text_patch(vt, e.row, e.col, cols - e.col, &text, &styles);

    let c = vt.cursor();
    let (from, to) = (e.from.len(), e.to.chars().count());

    if c.row == e.row && c.col >= e.col + from && from != to {
        let col = (c.col + to).saturating_sub(from).min(cols - 1);
        out.push_str(&format!("\x1b[{};{}H", c.row + 1, col + 1));
    }

    out
}

/// `cells` with `e.from` at each of `starts` replaced by `e.to` (with an
/// empty `e.from`, `e.to` is inserted there). New text takes the edit's
/// styles, or those of the cells it replaces (inserted text: of the cell
/// before it); the rest of the line keeps its own.
fn replace_in(
    cells: &[(char, super::style::TextStyle)],
    starts: &[usize],
    e: &TextEdit,
) -> Vec<(char, super::style::TextStyle)> {
    let to: Vec<char> = e.to.chars().collect();
    let width = e.from.len();
    let mut starts: Vec<usize> = starts.to_vec();
    starts.sort_unstable();
    starts.dedup();
    let mut starts = starts.into_iter().peekable();

    let mut out = Vec::with_capacity(cells.len() + to.len());
    let mut i = 0;

    // `i` only ever moves forward and each start is used once, so this ends
    // even when nothing is replaced (an insert)
    while i <= cells.len() {
        if starts.peek() == Some(&i) {
            starts.next();

            let replaced = &cells[i..(i + width).min(cells.len())];
            let before = i.checked_sub(1).and_then(|p| cells.get(p)).map(|c| c.1);

            for (k, ch) in to.iter().enumerate() {
                let style = e.styles.get(k).copied().unwrap_or_else(|| {
                    replaced
                        .get(k.min(replaced.len().saturating_sub(1)))
                        .map(|c| c.1)
                        .or(before)
                        .unwrap_or_default()
                });

                out.push((*ch, style));
            }

            if width > 0 {
                i += width;
                continue;
            }
        }

        let Some(cell) = cells.get(i) else { break };
        out.push(*cell);
        i += 1;
    }

    out
}

fn item_data<'a>(events: &'a [Event], item: &'a Item) -> &'a EventData {
    match &item.data {
        ItemData::Original(e) => &events[*e].data,
        ItemData::Owned(d) => d,
    }
}

fn output(s: String) -> ItemData {
    ItemData::Owned(EventData::Output(s))
}

/// Accumulates playlist items, tracking the terminal size they play at.
struct Builder {
    items: Vec<Item>,
    size: (usize, usize),
}

impl Builder {
    fn push(&mut self, data: ItemData, delay: Delay, origin: Origin) {
        self.items.push(Item {
            data,
            delay,
            origin,
        });
    }

    /// Redraws the screen to `vt`'s state (resizing first if needed). The
    /// first item takes `delay`, the rest follow immediately.
    fn redraw(&mut self, vt: Vt, delay: Delay, origin: Origin) {
        let mut delay = Some(delay);
        let mut next_delay = || delay.take().unwrap_or(Delay::Fixed(Duration::ZERO));

        if vt.size() != self.size {
            let (cols, rows) = vt.size();
            let d = next_delay();
            self.push(
                ItemData::Owned(EventData::Resize(cols as u16, rows as u16)),
                d,
                origin,
            );
            self.size = (cols, rows);
        }

        let d = next_delay();
        self.push(output(format!("{REDRAW_PREFIX}{}", vt.dump())), d, origin);
    }
}

/// `text` centered on a `cols`x`rows` screen, as cursor moves plus text.
fn centered(text: &str, (cols, rows): (usize, usize)) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let top = rows.saturating_sub(lines.len()) / 2;
    let mut out = String::new();

    for (i, line) in lines.iter().enumerate().take(rows) {
        let line: String = line.chars().take(cols).collect();
        let left = cols.saturating_sub(line.chars().count()) / 2;
        out.push_str(&format!("\x1b[{};{}H{line}", top + i + 1, left + 1));
    }

    out
}

/// The visible screen's lines. (avt's `text()` is always the main screen,
/// even while a program like vim is using the alternate one.)
pub fn screen_text(vt: &Vt) -> Vec<String> {
    vt.view().map(|l| l.text()).collect()
}

fn delay_in(times: &[Duration], i: usize) -> Option<Duration> {
    let t = *times.get(i)?;
    let prev = i.checked_sub(1).map_or(Duration::ZERO, |p| times[p]);

    Some(t.saturating_sub(prev))
}

fn new_vt(cols: usize, rows: usize) -> Vt {
    // Keyframes only capture the visible screen (avt's dump doesn't include
    // scrollback), so there's no point keeping scrollback around.
    Vt::builder().size(cols, rows).scrollback_limit(0).build()
}

fn feed(vt: &mut Vt, data: &EventData) {
    match data {
        EventData::Output(s) => {
            vt.feed_str(s);
        }

        EventData::Resize(cols, rows) => {
            vt.resize(*cols as usize, *rows as usize);
        }

        _ => {}
    }
}

/// Terminal state after applying `events[..next]`.
#[derive(Clone)]
struct Keyframe {
    next: usize,
    cols: usize,
    rows: usize,
    dump: String,
}

#[derive(Clone)]
struct Keyframes(Vec<Keyframe>);

impl Keyframes {
    fn build<'a>(
        (cols, rows): (usize, usize),
        events: impl Iterator<Item = &'a EventData>,
    ) -> Self {
        let mut vt = new_vt(cols, rows);

        let mut keyframes = vec![Keyframe {
            next: 0,
            cols,
            rows,
            dump: vt.dump(),
        }];

        let mut bytes = 0;
        let mut count = 0;

        for (i, data) in events.enumerate() {
            feed(&mut vt, data);
            count += 1;

            if let EventData::Output(s) = data {
                bytes += s.len();
            }

            if bytes >= KEYFRAME_BYTES || count >= KEYFRAME_EVENTS {
                let (cols, rows) = vt.size();

                keyframes.push(Keyframe {
                    next: i + 1,
                    cols,
                    rows,
                    dump: vt.dump(),
                });

                bytes = 0;
                count = 0;
            }
        }

        Self(keyframes)
    }

    /// State after `next` events, where `data(i)` gives event `i`.
    fn vt_at<'a>(&self, next: usize, data: impl Fn(usize) -> &'a EventData) -> Vt {
        // keyframes[0].next == 0, so there is always a match
        let k = self.0.partition_point(|k| k.next <= next) - 1;
        let kf = &self.0[k];

        let mut vt = new_vt(kf.cols, kf.rows);
        vt.feed_str(&kf.dump);

        for i in kf.next..next {
            feed(&mut vt, data(i));
        }

        vt
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use avt::Vt;

    use super::{feed, new_vt, Origin, Timeline};
    use crate::asciicast::{self, Event, EventData, Header};
    use crate::edit::{Clip, Edit, EditList};

    fn screen(vt: &Vt) -> (Vec<String>, usize, usize, (usize, usize)) {
        let c = vt.cursor();

        (super::screen_text(vt), c.col, c.row, vt.size())
    }

    /// A recording long enough to produce many keyframes, with colors,
    /// cursor movement, scrolling, a resize and the alternate screen.
    fn synthetic() -> (Header, Vec<Event>) {
        let header = Header {
            term_cols: 40,
            term_rows: 10,
            ..Default::default()
        };

        let mut events = Vec::new();
        let mut t = 0;

        for i in 0..5000 {
            t += 10_000;
            let time = Duration::from_micros(t);

            let data = match i % 7 {
                0 => format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 8),
                1 => format!("\x1b[{};{}Hx", i % 10 + 1, i % 40 + 1),
                2 => "a long line that wraps around the forty column terminal ".repeat(2),
                3 if i % 700 == 3 => "\x1b[?1049h\x1b[2Jalt screen".to_owned(),
                3 if i % 700 == 353 => "\x1b[?1049l".to_owned(),
                _ => format!("{i} "),
            };

            events.push(Event::output(time, data));

            if i == 2500 {
                events.push(Event::resize(time, (60, 15)));
            }
        }

        (header, events)
    }

    fn simple(outputs: &[(u64, &str)]) -> Timeline {
        let events = outputs
            .iter()
            .map(|&(s, o)| Event::output(Duration::from_secs(s), o.to_owned()))
            .collect();

        Timeline::new(
            Header {
                term_cols: 20,
                term_rows: 5,
                ..Default::default()
            },
            events,
        )
    }

    fn apply(tl: &mut Timeline, edits: Vec<Edit>) -> EditList {
        let mut list = EditList::new(tl.original_len());

        for e in edits {
            list.push(e).unwrap();
        }

        tl.apply_edits(&list).unwrap();

        list
    }

    fn row(vt: &Vt, r: usize) -> String {
        vt.text()[r].trim_end().to_owned()
    }

    #[test]
    fn seek_matches_linear_replay() {
        let (header, events) = synthetic();
        let (cols, rows) = (header.term_cols as usize, header.term_rows as usize);
        let mut linear = new_vt(cols, rows);
        let tl = Timeline::new(header, events.clone());

        assert!(tl.keyframes.0.len() > 5, "test should exercise keyframes");

        // Positions just before, at, and after keyframes, plus some arbitrary ones.
        let mut checkpoints: Vec<usize> = tl
            .keyframes
            .0
            .iter()
            .flat_map(|k| [k.next.saturating_sub(1), k.next, k.next + 1])
            .chain([0, 1, 777, 2501, 2502, events.len()])
            .filter(|&p| p <= events.len())
            .collect();

        checkpoints.sort();
        checkpoints.dedup();

        let mut applied = 0;

        for p in checkpoints {
            for e in &events[applied..p] {
                feed(&mut linear, &e.data);
            }

            applied = p;

            assert_eq!(
                screen(&tl.vt_at_position(p)),
                screen(&linear),
                "mismatch at position {p}"
            );
        }
    }

    /// The core invariant: around cuts and clips, the screen is exactly the
    /// original's - checked across a resize and alternate screen switches.
    #[test]
    fn edits_preserve_the_screen_around_them() {
        let (header, events) = synthetic();
        let n = events.len();

        // text edits change the screen from where they are; cuts and clips mustn't
        // disturb that, so the reference is the recording with just them
        let text_edits = vec![
            Edit::Text {
                at: 1000,
                row: 0,
                col: 0,
                from: "xxxx".to_owned(),
                to: "EDIT".to_owned(),
                styles: Vec::new(),
            },
            Edit::Text {
                at: 15, // inside a cut
                row: 3,
                col: 2,
                from: "ab".to_owned(),
                to: "AB".to_owned(),
                styles: Vec::new(),
            },
        ];

        let mut original = Timeline::new(header.clone(), events.clone());
        apply(&mut original, text_edits.clone());
        let mut tl = Timeline::new(header, events);

        apply(
            &mut tl,
            text_edits
                .into_iter()
                .chain(vec![
                    Edit::Delete { from: 10, to: 20 },
                    Edit::Delete {
                        from: 2490,
                        to: 2520,
                    }, // spans the resize
                    Edit::Delete { from: 690, to: 720 }, // spans an alt screen switch
                    Edit::Delete {
                        from: n - 5,
                        to: n - 1,
                    }, // to the end
                    Edit::Insert {
                        at: 0,
                        clip: Clip::Text {
                            text: "Title\nsub".to_owned(),
                            secs: 1.0,
                        },
                    },
                    Edit::Insert {
                        at: 100,
                        clip: Clip::Blank { secs: 1.0 },
                    },
                    Edit::Insert {
                        at: 100,
                        clip: Clip::Hold { secs: 2.0 },
                    },
                    Edit::Insert {
                        at: 400,
                        clip: Clip::Recording {
                            path: None,
                            from: 3000,
                            to: Some(3100),
                        },
                    },
                    Edit::Insert {
                        at: 15,
                        clip: Clip::Hold { secs: 1.0 },
                    }, // inside a cut
                    Edit::Insert {
                        at: 2600,
                        clip: Clip::Recording {
                            path: None,
                            from: 5,
                            to: Some(30),
                        },
                    }, // other size
                    Edit::Insert {
                        at: n,
                        clip: Clip::Text {
                            text: "The end".to_owned(),
                            secs: 1.0,
                        },
                    },
                    Edit::Replace {
                        from: 800,
                        to: 900,
                        clip: Clip::Blank { secs: 1.0 },
                    },
                    // spans an alt screen switch (at event 703)
                    Edit::Replace {
                        from: 699,
                        to: 702,
                        clip: Clip::Hold { secs: 1.0 },
                    },
                ])
                .collect(),
        );

        let mut linear = new_vt(40, 10);
        let mut applied = 0;

        for p in 0..n {
            let Some(pos) = tl.position_of_event(p) else {
                continue;
            };

            let reference = original.position_of_event(p).unwrap();

            assert_eq!(
                screen(&tl.vt_at_position(pos)),
                screen(&original.vt_at_position(reference)),
                "screen before original event {p} (playlist {pos}) differs"
            );

            // the keyframed playlist agrees with straight playback of it
            if p % 97 == 0 {
                tl.advance(&mut linear, applied, pos);
                applied = pos;
                assert_eq!(
                    screen(&linear),
                    screen(&tl.vt_at_position(pos)),
                    "playlist {pos}"
                );
            }
        }

        // cut events aren't in the playlist, everything else is
        assert!(tl.position_of_event(15).is_none());
        assert!(tl.position_of_event(2500).is_none());
        assert!(tl.position_of_event(21).is_some());
    }

    fn text(at: usize, row: usize, col: usize, from: &str, to: &str) -> Edit {
        Edit::Text {
            at,
            row,
            col,
            from: from.to_owned(),
            to: to.to_owned(),
            styles: Vec::new(),
        }
    }

    fn before(tl: &Timeline, e: usize) -> Vt {
        tl.vt_at_position(tl.position_of_event(e).unwrap_or(tl.len()))
    }

    #[test]
    fn once_persists_scrolls_and_gets_drawn_over() {
        let mut tl = simple(&[
            (1, "top\r\nhello world\r\n"),
            (2, "one\r\n"),
            (3, "two\r\n"),
            (4, "three\r\n"), // the screen scrolls by one here
            (5, "\x1b[1;1Hhelp!"),
        ]);

        apply(&mut tl, vec![text(1, 1, 0, "hello", "HOWDY")]);

        // drawn at the edit point, as its own item, at the same time
        let p = tl.position_of_event(1).unwrap();
        assert_eq!(tl.origin(p - 1), Some(Origin::TextEdit { edit: 0 }));
        assert_eq!(row(&before(&tl, 1), 1), "HOWDY world");

        // it stays while nothing draws over it ...
        assert_eq!(row(&before(&tl, 3), 1), "HOWDY world");

        // ... scrolls with the screen ...
        assert_eq!(row(&before(&tl, 4), 0), "HOWDY world");

        // ... and goes when the recording draws over those cells
        assert_eq!(row(&tl.vt_at_position(tl.len()), 0), "help! world");
        assert_eq!(tl.duration(), Duration::from_secs(5), "edits take no time");
    }

    #[test]
    fn shifting_styles_and_cursor() {
        let mut tl = simple(&[(1, "\x1b[31mhello\x1b[0m world"), (2, "!")]);
        apply(&mut tl, vec![text(1, 0, 0, "hello", "hi")]);

        let vt = tl.vt_at_position(tl.len());
        // shorter: the rest of the line - and the cursor - move along
        assert_eq!(row(&vt, 0), "hi world!");
        assert_eq!(
            vt.line(0).cells()[1].pen().foreground(),
            Some(avt::Color::Indexed(1)),
            "the new text takes the style of what it replaced"
        );
        assert_eq!(
            vt.line(0).cells()[3].pen().foreground(),
            None,
            "the rest keeps its own"
        );

        // longer: pushes the rest along
        let mut tl = simple(&[(1, "hello world"), (2, "!")]);
        apply(&mut tl, vec![text(1, 0, 0, "hello", "greetings")]);
        assert_eq!(row(&tl.vt_at_position(tl.len()), 0), "greetings world!");
    }

    /// An edit is as if the text had been deleted and retyped in the
    /// recording session: at that point the screen is what retyping would
    /// leave (cursor included), and the rest plays on as recorded.
    #[test]
    fn once_is_as_if_retyped() {
        let prompt = "$ ";
        let typed: Vec<String> = "Hello world".chars().map(String::from).collect();

        // typed "Hello world", then (later) a new prompt
        let mut events = vec![(1, prompt.to_owned())];
        events.extend(typed.iter().map(|c| (2, c.clone())));
        events.push((3, "\r\n$ exit".to_owned()));
        let outputs: Vec<(u64, &str)> = events.iter().map(|(t, s)| (*t, s.as_str())).collect();

        // the same session, where it was corrected by hand to "Hey"
        let mut by_hand = events.clone();
        by_hand.splice(
            12..12,
            std::iter::repeat_n((2, "\x08\x1b[K".to_owned()), 10)
                .chain([(2, "e".to_owned()), (2, "y".to_owned())]),
        );
        let by_hand: Vec<(u64, &str)> = by_hand.iter().map(|(t, s)| (*t, s.as_str())).collect();
        let by_hand = simple(&by_hand);

        let mut tl = simple(&outputs);
        apply(&mut tl, vec![text(12, 0, 2, "Hello world", "Hey")]);

        // right after the edit: as if corrected by hand
        assert_eq!(
            screen(&before(&tl, 12)),
            screen(&before(&by_hand, 24)),
            "at the edit"
        );

        // and from there on, the same
        assert_eq!(
            screen(&tl.vt_at_position(tl.len())),
            screen(&by_hand.vt_at_position(by_hand.len()))
        );
        assert_eq!(row(&tl.vt_at_position(tl.len()), 0), "$ Hey");
    }

    /// What the recording does after an edit plays as recorded: a program
    /// that places its output itself (as vim does) still puts it where it
    /// did in the recording, over a lengthened edit.
    #[test]
    fn once_plays_the_rest_as_recorded() {
        // vim-like: the word, then each key typed after it placed absolutely
        let mut tl = simple(&[
            (1, "\x1b[2;1Hmy cookout"),
            (2, "\x1b[2;11H "),
            (3, "\x1b[2;12H:"),
            (4, "\x1b[2;13HD"),
        ]);

        let edit = Edit::Text {
            at: 1,
            row: 1,
            col: 3,
            from: "cookout".to_owned(),
            to: "HOUSEWARMING".to_owned(),
            styles: Vec::new(),
        };
        apply(&mut tl, vec![edit]);

        // right after the edit the cursor is after the new word
        let at = before(&tl, 1);
        assert_eq!(row(&at, 1), "my HOUSEWARMING");
        assert_eq!((at.cursor().row, at.cursor().col), (1, 15));

        let end = tl.vt_at_position(tl.len());
        assert_eq!(row(&end, 1), "my HOUSEWA :DNG");
    }

    /// Regression: an edit that replaces nothing (an insert) used to loop
    /// forever, eating memory, while rebuilding the line.
    #[test]
    fn inserting_replaces_nothing_and_ends() {
        let insert = |col: usize, to: &str| Edit::Text {
            at: 1,
            row: 0,
            col,
            from: String::new(),
            to: to.to_owned(),
            styles: Vec::new(),
        };

        let mut tl = simple(&[(1, "abcd")]);
        apply(&mut tl, vec![insert(2, "XY")]);
        assert_eq!(row(&tl.vt_at_position(tl.len()), 0), "abXYcd");

        // at the end of the text, and at the last column
        let mut tl = simple(&[(1, "abcd")]);
        apply(&mut tl, vec![insert(4, "!")]);
        assert_eq!(row(&tl.vt_at_position(tl.len()), 0), "abcd!");

        let mut tl = simple(&[(1, "abcd")]);
        apply(&mut tl, vec![insert(19, "z")]);
        let end = tl.vt_at_position(tl.len());
        assert_eq!(
            end.text()[0].chars().count(),
            20,
            "the line stays the screen's width"
        );
        assert!(end.text()[0].ends_with('z'));
    }

    #[test]
    fn once_moves_the_cursor_with_the_text() {
        let mut tl = simple(&[(1, "noelle@Home$ "), (2, "e"), (3, "c"), (4, "h"), (5, "o")]);
        let edit = Edit::Text {
            at: 1,
            row: 0,
            col: 0,
            from: "noelle@Home".to_owned(),
            to: "me".to_owned(),
            styles: Vec::new(),
        };
        apply(&mut tl, vec![edit]);

        let end = tl.vt_at_position(tl.len());
        assert_eq!(
            row(&end, 0),
            "me$ echo",
            "what's typed next follows the change"
        );
        assert_eq!(end.cursor().col, 8);
    }

    #[test]
    fn cuts_and_clips_keep_text_edits() {
        let mut tl = simple(&[(1, "hello world"), (2, "\r\nmore"), (3, "\r\nend")]);

        apply(
            &mut tl,
            vec![
                text(1, 0, 0, "hello", "HOWDY"),
                Edit::Delete { from: 1, to: 1 },
                Edit::Insert {
                    at: 3,
                    clip: Clip::Text {
                        text: "card".to_owned(),
                        secs: 1.0,
                    },
                },
            ],
        );

        // the jump over the cut shows the edited screen ...
        assert_eq!(row(&before(&tl, 2), 0), "HOWDY world");

        // ... and so does the restore after the card
        let end = tl.vt_at_position(tl.len());
        assert_eq!(row(&end, 0), "HOWDY world");
        assert_eq!(row(&end, 1), "more");
    }

    #[test]
    fn jump_cut_timing_and_screen() {
        let mut tl = simple(&[(1, "a"), (2, "b"), (4, "c"), (5, "d")]);
        apply(&mut tl, vec![Edit::Delete { from: 1, to: 2 }]);

        // a, [jump to the screen after "c"] + d
        assert_eq!(tl.len(), 3);
        assert_eq!(tl.origin(1), Some(Origin::Cut { from: 1, to: 2 }));
        assert_eq!(
            row(&tl.vt_at_position(2), 0),
            "abc",
            "jump shows what the cut part drew"
        );
        assert_eq!(row(&tl.vt_at_position(3), 0), "abcd");

        // the cut part's pauses are gone; the pause before "d" remains
        let times: Vec<u64> = tl.edited_events().map(|e| e.time.as_secs()).collect();
        assert_eq!(times, [1, 2, 2]);
        assert_eq!(tl.duration(), Duration::from_secs(2));

        // and it's the pause the redraw item reports as editable
        assert_eq!(tl.delay_owner(1), Some(3));
    }

    #[test]
    fn replace_plays_the_clip_instead_and_ends_after_the_replaced_events() {
        let mut tl = simple(&[(1, "a"), (2, "b"), (4, "c"), (5, "d")]);
        apply(
            &mut tl,
            vec![Edit::Replace {
                from: 1,
                to: 2,
                clip: Clip::Text {
                    text: "X".to_owned(),
                    secs: 1.0,
                },
            }],
        );

        assert_eq!(tl.position_of_event(1), None);
        assert_eq!(tl.position_of_event(2), None);

        // the card is up straight after "a" ...
        assert_eq!(row(&tl.vt_at_position(2), 2), "         X");

        // ... and the moment it ends, the screen is as after "c" - no flash
        // of the screen from before the replaced events
        let d = tl.position_of_event(3).unwrap();
        assert_eq!(row(&tl.vt_at_position(d), 0), "abc");
        for i in 2..d {
            assert_ne!(
                row(&tl.vt_at_position(i), 0),
                "a",
                "the old screen shows at {i}"
            );
        }
        assert_eq!(row(&tl.vt_at_position(d + 1), 0), "abcd");

        // a, card (1s), then d after its own 1s pause; b and c's time is gone
        assert_eq!(tl.duration(), Duration::from_secs(3));

        // a replace with a hold freezes, then moves on to after the events
        let mut tl = simple(&[(1, "a"), (2, "b"), (3, "c")]);
        apply(
            &mut tl,
            vec![Edit::Replace {
                from: 1,
                to: 1,
                clip: Clip::Hold { secs: 2.0 },
            }],
        );

        let c = tl.position_of_event(2).unwrap();
        assert_eq!(row(&tl.vt_at_position(c), 0), "ab");
        assert_eq!(
            row(&tl.vt_at_position(2), 0),
            "a",
            "frozen on the screen before b"
        );
    }

    #[test]
    fn clips() {
        let mut tl = simple(&[(1, "a"), (2, "b")]);
        apply(
            &mut tl,
            vec![
                Edit::Insert {
                    at: 1,
                    clip: Clip::Text {
                        text: "Hi".to_owned(),
                        secs: 3.0,
                    },
                },
                Edit::Insert {
                    at: 2,
                    clip: Clip::Hold { secs: 2.0 },
                },
            ],
        );

        // a, card, (3s), restore, b, hold
        assert_eq!(
            row(&tl.vt_at_position(2), 2),
            "         Hi",
            "centered on the 20x5 screen"
        );
        let restored = (0..tl.len())
            .find(|&i| i > 2 && tl.origin(i) == Some(Origin::Event(1)))
            .unwrap();
        assert_eq!(
            row(&tl.vt_at_position(restored), 0),
            "a",
            "screen restored after the card"
        );
        assert_eq!(row(&tl.vt_at_position(restored), 2), "");
        assert_eq!(tl.duration(), Duration::from_secs(1 + 3 + 1 + 2));
        assert_eq!(tl.delay_owner(1), None, "clip items have fixed delays");
    }

    #[test]
    fn recording_clip_from_another_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy("tests/casts/full-v2.cast", dir.path().join("other.cast")).unwrap();

        let mut tl = simple(&[(1, "a")]);
        tl.set_clip_base(dir.path());
        apply(
            &mut tl,
            vec![Edit::Insert {
                at: 1,
                clip: Clip::Recording {
                    path: Some("other.cast".to_owned()),
                    from: 0,
                    to: Some(1),
                },
            }],
        );

        // other.cast: "ż" at 0.000001s, "ółć" at 1s, its next event at 2.3s.
        // Its timing is kept: "a" at 1s, then ż, ółć, and the last frame stays
        // up until where the source's next event was.
        assert_eq!(tl.duration(), Duration::from_micros(1_000_000 + 2_300_000));

        let during = tl.vt_at_position(tl.len() - 1);
        assert!(row(&during, 0).starts_with("żółć"), "{:?}", row(&during, 0));
        assert_eq!(
            row(&tl.vt_at_position(tl.len()), 0),
            "a",
            "restored afterwards"
        );

        // missing file: an error, and the timeline is left as it was
        let before = tl.len();
        let mut bad = EditList::new(1);
        bad.push(Edit::Insert {
            at: 0,
            clip: Clip::Recording {
                path: Some("nope.cast".to_owned()),
                from: 0,
                to: None,
            },
        })
        .unwrap();
        assert!(tl.apply_edits(&bad).is_err());
        assert_eq!(tl.len(), before);
    }

    #[test]
    fn retiming_skips_rebuilding_the_playlist() {
        let mut tl = simple(&[(1, "a"), (2, "b"), (3, "c")]);
        apply(&mut tl, vec![Edit::Delete { from: 1, to: 1 }]);
        let v = tl.structure_version;

        apply(
            &mut tl,
            vec![
                Edit::Delete { from: 1, to: 1 },
                Edit::SetDelay {
                    event: 2,
                    secs: 5.0,
                },
            ],
        );
        assert_eq!(tl.structure_version, v);
        assert_eq!(tl.duration(), Duration::from_secs(6));
    }

    #[test]
    fn seek_on_real_recording() {
        let cast = asciicast::open_from_path("tests/casts/full-v2.cast").unwrap();
        let tl = Timeline::load(cast).unwrap();

        assert_eq!(tl.vt_at(Duration::ZERO).text()[0].trim_end(), "");
        assert!(tl.vt_at(tl.duration()).text()[0].starts_with("żółć"));
        // the recording resizes to 80x40 at 5.6s
        assert_eq!(tl.vt_at(tl.duration()).size(), (80, 40));
    }

    #[test]
    fn position_at_and_retime() {
        let mut tl = simple(&[(1, "a"), (2, "b"), (10, "c")]);

        assert_eq!(tl.position_at(Duration::from_millis(500)), 0);
        assert_eq!(tl.position_at(Duration::from_secs(1)), 1);
        assert_eq!(tl.position_at(Duration::from_secs(5)), 2);
        assert_eq!(tl.vt_at(Duration::from_secs(5)).text()[0].trim_end(), "ab");

        apply(
            &mut tl,
            vec![Edit::SetDelay {
                event: 2,
                secs: 1.0,
            }],
        );

        assert_eq!(tl.duration(), Duration::from_secs(3));
        assert_eq!(tl.delay(2), Some(Duration::from_secs(1)));
        assert_eq!(tl.vt_at(Duration::from_secs(5)).text()[0].trim_end(), "abc");

        let times: Vec<_> = tl.edited_events().map(|e| e.time.as_secs()).collect();
        assert_eq!(times, [1, 2, 3]);
    }

    #[test]
    fn empty_recording() {
        let tl = Timeline::new(Header::default(), Vec::new());

        assert!(tl.is_empty());
        assert_eq!(tl.duration(), Duration::ZERO);
        assert_eq!(tl.vt_at(Duration::from_secs(1)).size(), (80, 24));
    }

    #[test]
    fn pasted_non_output_events_are_skipped() {
        let mut tl = Timeline::new(
            Header {
                term_cols: 20,
                term_rows: 5,
                ..Default::default()
            },
            vec![
                Event::output(Duration::from_secs(1), "a".to_owned()),
                Event::marker(Duration::from_secs(2), "m".to_owned()),
                Event::output(Duration::from_secs(3), "b".to_owned()),
            ],
        );

        apply(
            &mut tl,
            vec![Edit::Insert {
                at: 3,
                clip: Clip::Recording {
                    path: None,
                    from: 0,
                    to: None,
                },
            }],
        );

        let pasted: Vec<_> = (3..tl.len())
            .filter_map(|i| tl.event(i))
            .filter(|(_, d)| matches!(d, EventData::Marker(_)))
            .collect();

        assert!(pasted.is_empty(), "markers aren't pasted");
    }

    fn track(at: usize, from: usize, to: usize, row: isize, col: isize) -> Edit {
        Edit::Track {
            at,
            track: crate::edit::Track {
                source: crate::edit::TrackSource::Recording {
                    path: None,
                    from,
                    to: Some(to),
                },
                offset: crate::edit::edits::Offset { row, col },
                crop: None,
                until: None,
            },
        }
    }

    fn at_secs(tl: &Timeline, secs: f64) -> Vt {
        tl.vt_at(Duration::from_secs_f64(secs))
    }

    #[test]
    fn tracks_play_on_top_and_then_disappear() {
        let outputs = [
            (1, "aaaa\r\n"),
            (2, "bb bb\r\n"),
            (3, "cccc\r\n"),
            (10, "dddd"),
        ];
        let original = simple(&outputs);
        let mut tl = simple(&outputs);

        // events 0..=1 of this recording, from after event 0 (1s), at row 3, col 10
        apply(&mut tl, vec![track(1, 0, 1, 3, 10)]);

        // its first event plays 1s in (at 2s), its second 1s later
        let vt = at_secs(&tl, 2.5);
        assert_eq!(row(&vt, 1), "bb bb");
        assert_eq!(row(&vt, 3), "          aaaa");

        let vt = at_secs(&tl, 3.5);
        assert_eq!(row(&vt, 2), "cccc");
        assert_eq!(row(&vt, 3), "          aaaa");
        assert_eq!(row(&vt, 4), "          bb bb");

        // the cursor is where the recording's is
        assert_eq!((vt.cursor().row, vt.cursor().col), (3, 0));
        assert!(vt.cursor().visible);

        // it ends when its next event would have come (3s in, at 4s)
        for secs in [4.0, 5.0, 10.0, 11.0] {
            assert_eq!(
                screen(&at_secs(&tl, secs)),
                screen(&at_secs(&original, secs)),
                "at {secs}s"
            );
        }

        // straight playback agrees with seeking
        let mut linear = new_vt(20, 5);
        tl.advance(&mut linear, 0, tl.len());
        assert_eq!(screen(&linear), screen(&tl.vt_at_position(tl.len())));
        assert_eq!(tl.duration(), Duration::from_secs(10));
        assert!(tl.draws_edits());
    }

    #[test]
    fn tracks_are_see_through_layered_and_cut_off_at_the_edge() {
        let outputs = [(1, "1234567890123456789"), (2, "\r\nX Y"), (3, "\r\nZZZ")];
        let mut tl = simple(&outputs);

        // from after event 1 (2s): a slice starts on the screen before it,
        // so the tracks' first rows are the digits, moved off screen.
        // "X Y" over the first row from column 18: "X" at 18, "Y" off
        // screen; then "ZZZ" (a later track, so on top) from column 16
        apply(
            &mut tl,
            vec![track(2, 1, 1, -1, 18), track(2, 2, 2, -2, 16)],
        );

        let vt = at_secs(&tl, 3.5);
        assert_eq!(row(&vt, 0), "1234567890123456ZZZ");
        // the tracks' blank rows show the recording
        assert_eq!(row(&vt, 1), "X Y");
        assert_eq!(row(&vt, 2), "ZZZ");
    }

    #[test]
    fn a_covered_cursor_is_hidden() {
        let outputs = [(1, "ab"), (2, "XYZ"), (9, "!")];
        let mut tl = simple(&outputs);

        // event 1 ("XYZ", after "ab") from column 1: covers the cursor
        // (after "abXYZ")
        apply(&mut tl, vec![track(1, 1, 1, 0, 1)]);

        let vt = at_secs(&tl, 3.5);
        assert_eq!(row(&vt, 0), "aabXYZ");
        assert_eq!((vt.cursor().row, vt.cursor().col), (0, 5));
        assert!(!vt.cursor().visible);
    }

    #[test]
    fn retiming_moves_the_recording_under_its_tracks() {
        let outputs = [(1, "a"), (2, "b"), (3, "c"), (4, "d")];
        let mut tl = simple(&outputs);

        // a track of event 3 ("d", after "abc") at row 2, from after event
        // 0 (at 1s); "d" comes 1s after "c", at 2s, and it's gone at 3s
        let mut edits = vec![track(1, 3, 3, 2, 0)];
        apply(&mut tl, edits.clone());
        assert_eq!(row(&at_secs(&tl, 1.5), 2), "abc");
        assert_eq!(row(&at_secs(&tl, 2.5), 2), "abcd");
        assert_eq!(row(&at_secs(&tl, 2.5), 0), "ab");

        // slowing the recording down leaves the track's timing alone
        edits.push(Edit::SetDelay {
            event: 1,
            secs: 5.0,
        });
        apply(&mut tl, edits);
        assert_eq!(row(&at_secs(&tl, 2.5), 2), "abcd");
        assert_eq!(row(&at_secs(&tl, 2.5), 0), "a");
        assert_eq!(row(&at_secs(&tl, 6.5), 0), "ab");
        assert_eq!(row(&at_secs(&tl, 6.5), 2), "");
        assert_eq!(
            tl.position_of_event(1).map(|p| tl.origin(p)),
            Some(Some(Origin::Event(1)))
        );
    }

    fn generated(at: usize, source: crate::edit::TrackSource, row: isize, col: isize) -> Edit {
        Edit::Track {
            at,
            track: crate::edit::Track {
                source,
                offset: crate::edit::edits::Offset { row, col },
                crop: None,
                until: None,
            },
        }
    }

    #[test]
    fn captions_sit_at_the_bottom_and_cover_with_a_background() {
        use crate::edit::style::{StyleColor, TextStyle};
        use crate::edit::TrackSource;

        let outputs = [
            (1, "line one\r\nline two\r\nline three\r\nfour\r\nfive"),
            (9, "!"),
        ];
        let mut tl = simple(&outputs);

        // "hi there" on red: padded, bottom centre of 20x5, for 2s from 1s
        let caption = TrackSource::Caption {
            text: "hi there".to_owned(),
            secs: 2.0,
            anchor: crate::edit::fx::Anchor::S,
            style: TextStyle {
                bg: Some(StyleColor::Indexed(1)),
                ..Default::default()
            },
        };
        apply(&mut tl, vec![generated(1, caption, 0, 0)]);

        let vt = at_secs(&tl, 2.0);
        // 10 wide (8 + padding), centred: columns 5..15; its blanks are blocks
        assert_eq!(row(&vt, 4), "five █hi█there█");
        assert_eq!(
            vt.line(4).cells()[6].pen().background(),
            Some(avt::Color::Indexed(1))
        );
        assert_eq!(row(&vt, 3), "four", "one line tall");

        // gone after 2s
        assert_eq!(row(&at_secs(&tl, 3.5), 4), "five");
    }

    #[test]
    fn boxes_hide_and_highlights_restyle() {
        use crate::edit::style::{StyleColor, TextStyle};
        use crate::edit::TrackSource;

        let mut tl = simple(&[(1, "user: me@ex.com"), (9, "!")]);

        let box_ = TrackSource::Box {
            cols: 9,
            rows: 1,
            secs: 5.0,
            fill: '█',
            color: None,
        };
        let highlight = TrackSource::Highlight {
            cols: 4,
            rows: 1,
            secs: 5.0,
            style: TextStyle {
                fg: Some(StyleColor::Indexed(2)),
                bold: true,
                ..Default::default()
            },
        };
        apply(
            &mut tl,
            vec![generated(1, box_, 0, 6), generated(1, highlight, 0, 0)],
        );

        let vt = at_secs(&tl, 3.0);
        assert_eq!(row(&vt, 0), "user: █████████");

        // the highlight keeps the text and restyles it
        let pen = *vt.line(0).cells()[1].pen();
        assert_eq!(pen.foreground(), Some(avt::Color::Indexed(2)));
        assert!(pen.is_bold());
        assert_eq!(vt.line(0).cells()[4].pen().foreground(), None);

        // the cursor (at the end of the line, past the box) still shows
        assert!(vt.cursor().visible);

        // and after 5s, all back
        let vt = at_secs(&tl, 7.0);
        assert_eq!(row(&vt, 0), "user: me@ex.com");
        assert_eq!(vt.line(0).cells()[1].pen().foreground(), None);
    }

    #[test]
    fn generated_tracks_last_until_a_position_if_later() {
        use crate::edit::TrackSource;

        let mut tl = simple(&[(1, "a"), (2, "b"), (10, "c"), (11, "d")]);
        let box_ = TrackSource::Box {
            cols: 2,
            rows: 1,
            secs: 1.0,
            fill: '#',
            color: None,
        };
        let Edit::Track { at, mut track } = generated(1, box_, 2, 0) else {
            unreachable!()
        };
        track.until = Some(3); // until "c", at 10s
        apply(&mut tl, vec![Edit::Track { at, track }]);

        assert_eq!(row(&at_secs(&tl, 9.0), 2), "##");
        assert_eq!(row(&at_secs(&tl, 10.5), 2), "");
    }
}
