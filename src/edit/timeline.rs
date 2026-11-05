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

use super::edits::Clip;
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
const REDRAW_PREFIX: &str = "\x1b[?1049l\x1b[!p\x1b[H\x1b[2J";

/// Where a playlist item came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// An original event.
    Event(usize),
    /// The jump at a cut of original events `from..=to`.
    Cut { from: usize, to: usize },
    /// Part of the `insert`-th inserted clip (counting insert edits in order).
    Clip { insert: usize },
}

enum ItemData {
    Original(usize),
    Owned(EventData),
}

enum Delay {
    /// The (edited) delay of an original event.
    Of(usize),
    Fixed(Duration),
}

struct Item {
    data: ItemData,
    delay: Delay,
    origin: Origin,
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

        let items = (0..events.len())
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
            original_keyframes,
            original_times,
            events,
            items,
            structure: Vec::new(),
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
        match &self.items.get(i)?.delay {
            Delay::Of(e) => Some(*e),
            Delay::Fixed(_) => None,
        }
    }

    /// Current (edited) delay before playlist item `i`.
    pub fn delay(&self, i: usize) -> Option<Duration> {
        delay_in(&self.times, i)
    }

    /// Delay before playlist item `i` in the unedited recording (for clips,
    /// their own delay).
    pub fn original_delay(&self, i: usize) -> Option<Duration> {
        match self.delay_owner(i) {
            Some(e) => delay_in(&self.original_times, e),
            None => self.delay(i),
        }
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
            .filter(|e| matches!(e, Edit::Delete { .. } | Edit::Insert { .. }))
            .cloned()
            .collect();

        if structure != self.structure {
            let items = self.compile(&structure)?;
            let size = (
                self.original_header.term_cols as usize,
                self.original_header.term_rows as usize,
            );

            let events = &self.events;
            self.keyframes =
                Keyframes::build(size, items.iter().map(|item| item_data(events, item)));
            self.event_pos = vec![None; self.events.len()];

            for (i, item) in items.iter().enumerate() {
                if let Origin::Event(e) = item.origin {
                    self.event_pos[e] = Some(i);
                }
            }

            self.items = items;
            self.structure = structure;
            self.structure_version += 1;
        }

        let retimed = retime::apply(&self.original_times, &edits.edits);
        self.delays = retime::delays(&retimed);

        let mut t = Duration::ZERO;

        self.times = self
            .items
            .iter()
            .map(|item| {
                t += match item.delay {
                    Delay::Of(e) => self.delays[e],
                    Delay::Fixed(d) => d,
                };

                t
            })
            .collect();

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

    /// Playlist events with their edited times.
    pub fn edited_events(&self) -> impl Iterator<Item = Event> + '_ {
        (0..self.items.len()).map(|i| Event {
            time: self.times[i],
            data: self.data(i).clone(),
        })
    }

    // ---- building the playlist ----------------------------------------------

    fn compile(&self, structure: &[Edit]) -> Result<Vec<Item>> {
        let n = self.events.len();
        let mut deleted = vec![false; n];
        let mut inserts: Vec<Vec<(usize, &Clip)>> = vec![Vec::new(); n + 1];
        let mut insert_no = 0;

        for edit in structure {
            match edit {
                Edit::Delete { from, to } => {
                    for d in deleted.iter_mut().take((*to).min(n - 1) + 1).skip(*from) {
                        *d = true;
                    }
                }

                Edit::Insert { at, clip } => {
                    inserts[(*at).min(n)].push((insert_no, clip));
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

        for p in 0..=n {
            for &(insert, clip) in &inserts[p] {
                self.push_clip(&mut b, Origin::Clip { insert }, clip, showing)?;
            }

            if p == n {
                break;
            }

            if deleted[p] {
                cut_from.get_or_insert(p);
                continue;
            }

            if let Some(from) = cut_from.take() {
                // jump cut: show the screen as it was after the cut, then go on
                let origin = Origin::Cut { from, to: p - 1 };
                b.redraw(
                    self.original_keyframes.vt_at(p, |i| &self.events[i].data),
                    Delay::Of(p),
                    origin,
                );
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

    fn push_clip(
        &self,
        b: &mut Builder,
        origin: Origin,
        clip: &Clip,
        showing: usize,
    ) -> Result<()> {
        let empty = || ItemData::Owned(EventData::Output(String::new()));
        let secs = |s: f64| Duration::from_secs_f64(s);
        let fixed0 = || Delay::Fixed(Duration::ZERO);

        match clip {
            // the screen doesn't change, so there's nothing to restore
            Clip::Hold { secs: s } => {
                b.push(empty(), Delay::Fixed(secs(*s)), origin);
                return Ok(());
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

                let before = self
                    .original_keyframes
                    .vt_at(showing, |i| &self.events[i].data);
                b.redraw(before, Delay::Fixed(tail), origin);

                return Ok(());
            }
        }

        // put back the screen as it was before the clip
        let before = self
            .original_keyframes
            .vt_at(showing, |i| &self.events[i].data);
        b.redraw(before, fixed0(), origin);

        Ok(())
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

        (vt.text(), c.col, c.row, vt.size())
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
        let original = Timeline::new(header.clone(), events.clone());
        let mut tl = Timeline::new(header, events);

        apply(
            &mut tl,
            vec![
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
            ],
        );

        let mut linear = new_vt(40, 10);
        let mut applied = 0;

        for p in 0..n {
            let Some(pos) = tl.position_of_event(p) else {
                continue;
            };

            assert_eq!(
                screen(&tl.vt_at_position(pos)),
                screen(&original.vt_at_position(p)),
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
}
