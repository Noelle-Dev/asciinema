//! In-memory timeline with fast random access to terminal state (scrubbing).
//!
//! Reconstructing the screen at an arbitrary point requires replaying every
//! output event up to that point through a virtual terminal. To keep seeking
//! cheap on long recordings, the timeline snapshots terminal state ("keyframes")
//! at regular intervals during one linear pass. A seek then restores the
//! nearest preceding keyframe and replays only the events after it.
//!
//! Keyframes are keyed by event *index*, not time, so retiming never
//! invalidates them - only the index<->time mapping changes.

use std::time::Duration;

use anyhow::Result;
use avt::Vt;

use super::{retime, EditList};
use crate::asciicast::{Asciicast, Event, EventData, Header, Version};

/// Take a keyframe after at least this many bytes of output since the last one...
const KEYFRAME_BYTES: usize = 16 * 1024;

/// ...or after this many events, whichever comes first.
const KEYFRAME_EVENTS: usize = 1000;

pub struct Timeline {
    pub header: Header,
    /// Format version of the file this was loaded from, if loaded from one.
    pub source_version: Option<Version>,
    events: Vec<Event>,
    original_times: Vec<Duration>,
    /// Header as recorded, before metadata edits.
    original_header: Header,
    times: Vec<Duration>,
    keyframes: Vec<Keyframe>,
}

/// Terminal state after applying `events[..next]`.
struct Keyframe {
    next: usize,
    cols: usize,
    rows: usize,
    dump: String,
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
        let keyframes = build_keyframes(&header, &events);

        Self {
            original_header: header.clone(),
            source_version: None,
            header,
            times: original_times.clone(),
            original_times,
            events,
            keyframes,
        }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[allow(dead_code)] // used by the upcoming TUI editor
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Event `i`, with its time as currently edited.
    pub fn event(&self, i: usize) -> Option<(Duration, &EventData)> {
        self.events.get(i).map(|e| (self.times[i], &e.data))
    }

    /// Current (edited) delay before event `i`.
    pub fn delay(&self, i: usize) -> Option<Duration> {
        delay_in(&self.times, i)
    }

    /// Delay before event `i` in the original, unedited recording.
    pub fn original_delay(&self, i: usize) -> Option<Duration> {
        delay_in(&self.original_times, i)
    }

    /// Whether the delay before event `i` differs from the original recording.
    pub fn is_retimed(&self, i: usize) -> bool {
        delay_in(&self.times, i) != delay_in(&self.original_times, i)
    }

    /// Total duration as currently edited.
    pub fn duration(&self) -> Duration {
        self.times.last().copied().unwrap_or_default()
    }

    /// Total duration of the original, unedited recording.
    pub fn original_duration(&self) -> Duration {
        self.original_times.last().copied().unwrap_or_default()
    }

    /// Recomputes event times from the original recording plus `edits`.
    pub fn apply_edits(&mut self, edits: &EditList) {
        self.times = retime::apply(&self.original_times, &edits.edits);

        self.header = super::edits::apply_header(&self.original_header, &edits.edits);
    }

    /// Header as recorded, before metadata edits.
    pub fn original_header(&self) -> &Header {
        &self.original_header
    }

    /// Number of events that have happened by time `t` (i.e. the index of the
    /// next event to be played at `t`).
    pub fn position_at(&self, t: Duration) -> usize {
        self.times.partition_point(|&et| et <= t)
    }

    /// Terminal state after the first `next` events have been applied.
    pub fn vt_at_position(&self, next: usize) -> Vt {
        let next = next.min(self.events.len());

        // keyframes[0].next == 0, so there is always a match
        let k = self.keyframes.partition_point(|k| k.next <= next) - 1;
        let kf = &self.keyframes[k];

        let mut vt = new_vt(kf.cols, kf.rows);
        vt.feed_str(&kf.dump);

        for event in &self.events[kf.next..next] {
            feed(&mut vt, &event.data);
        }

        vt
    }

    /// Terminal state at time `t` (as currently edited).
    #[allow(dead_code)] // used by the upcoming TUI editor
    pub fn vt_at(&self, t: Duration) -> Vt {
        self.vt_at_position(self.position_at(t))
    }

    /// Applies `events[from..to]` to `vt`, which must hold the state at
    /// position `from`. Lets callers step forward without a full seek.
    pub fn advance(&self, vt: &mut Vt, from: usize, to: usize) {
        let to = to.min(self.events.len());

        for event in &self.events[from.min(to)..to] {
            feed(vt, &event.data);
        }
    }

    /// Events with their edited times.
    pub fn edited_events(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.iter().zip(&self.times).map(|(e, &time)| Event {
            time,
            data: e.data.clone(),
        })
    }
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

fn build_keyframes(header: &Header, events: &[Event]) -> Vec<Keyframe> {
    let (cols, rows) = (header.term_cols as usize, header.term_rows as usize);
    let mut vt = new_vt(cols, rows);

    let mut keyframes = vec![Keyframe {
        next: 0,
        cols,
        rows,
        dump: vt.dump(),
    }];

    let mut bytes = 0;
    let mut count = 0;

    for (i, event) in events.iter().enumerate() {
        feed(&mut vt, &event.data);
        count += 1;

        if let EventData::Output(s) = &event.data {
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

    keyframes
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use avt::Vt;

    use super::{feed, new_vt, Timeline};
    use crate::asciicast::{self, Event, Header};
    use crate::edit::{Edit, EditList};

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

    #[test]
    fn seek_matches_linear_replay() {
        let (header, events) = synthetic();
        let (cols, rows) = (header.term_cols as usize, header.term_rows as usize);
        let mut linear = new_vt(cols, rows);
        let tl = Timeline::new(header, events.clone());

        assert!(tl.keyframes.len() > 5, "test should exercise keyframes");

        // Positions just before, at, and after keyframes, plus some arbitrary ones.
        let mut checkpoints: Vec<usize> = tl
            .keyframes
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
        let header = Header::default();

        let events = vec![
            Event::output(Duration::from_secs(1), "a".to_owned()),
            Event::output(Duration::from_secs(2), "b".to_owned()),
            Event::output(Duration::from_secs(10), "c".to_owned()),
        ];

        let mut tl = Timeline::new(header, events);

        assert_eq!(tl.position_at(Duration::from_millis(500)), 0);
        assert_eq!(tl.position_at(Duration::from_secs(1)), 1);
        assert_eq!(tl.position_at(Duration::from_secs(5)), 2);
        assert_eq!(tl.vt_at(Duration::from_secs(5)).text()[0].trim_end(), "ab");

        let mut edits = EditList::new(3);
        edits
            .push(Edit::SetDelay {
                event: 2,
                secs: 1.0,
            })
            .unwrap();
        tl.apply_edits(&edits);

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
}
