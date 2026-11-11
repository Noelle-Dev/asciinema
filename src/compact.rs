//! Compacting a recording: the same screens, in fewer bytes.
//!
//! The recording is replayed through a virtual terminal, and after each event
//! only what changed on screen is written: the cells that differ (each colour
//! set only when it changes), a scroll when the screen moved up, an erase for
//! a line's blank end, and the cursor. Events that change nothing visible are
//! dropped, their time going to the next one.
//!
//! The output starts by resetting the terminal (primary screen, default
//! modes, cleared) and from then on positions everything itself, so modes the
//! recording set (scroll regions, the alternate screen...) don't carry over.
//! Cursor shape changes and the bell, which aren't part of the screen, are
//! passed through.

use avt::Vt;

use crate::asciicast::{Event, EventData};
use crate::edit::composite::{compose, Painter};

/// The compacted events of a `cols`x`rows` recording. With `max_fps`, the
/// screen is drawn at most that many times a second: of the events within
/// each 1/`max_fps` of a second, only the screen after the last one is drawn
/// (at its time). Screens shown for less than that are skipped.
pub fn compact((cols, rows): (usize, usize), events: &[Event], max_fps: Option<u32>) -> Vec<Event> {
    let mut vt = new_vt(cols, rows);
    let mut painter = Painter::new((cols, rows));
    let mut started = false;
    let mut kept = String::new(); // passed-through bits waiting to be drawn
    let mut out = Vec::new();

    let slot = |e: &Event| max_fps.map(|f| (e.time.as_secs_f64() * f as f64).floor() as u64);

    for (i, event) in events.iter().enumerate() {
        let time = event.time;

        match &event.data {
            EventData::Output(s) => {
                vt.feed_str(s);
                kept.push_str(&passthrough(s));

                // more output in the same frame: draw after that instead
                let next = events.get(i + 1);
                let same_frame = max_fps.is_some()
                    && next.is_some_and(|n| {
                        matches!(n.data, EventData::Output(_)) && slot(n) == slot(event)
                    });

                if same_frame {
                    continue;
                }

                let mut s = std::mem::take(&mut kept);

                if !started {
                    s.insert_str(0, Painter::start());
                    started = true;
                }

                s.push_str(&painter.paint(&compose(&vt, &[]), &vt));

                // nothing visible happened: dropped (the next event keeps its
                // own time, so the pause carries over)
                if !s.is_empty() {
                    out.push(Event {
                        time,
                        data: EventData::Output(s),
                    });
                }
            }

            EventData::Resize(c, r) => {
                // the event itself, then a fresh start at the new size with
                // the next output
                vt.resize(*c as usize, *r as usize);
                painter = Painter::new((*c as usize, *r as usize));
                started = false;

                out.push(event.clone());
            }

            // input, markers, exit: kept
            _ => out.push(event.clone()),
        }
    }

    out
}

/// The parts of `s` that aren't screen contents but matter on playback:
/// cursor shape changes (DECSCUSR) and the bell.
fn passthrough(s: &str) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;

    while i < b.len() {
        match b[i] {
            0x07 => {
                // a bell, unless it ends an OSC sequence (a title, say)
                if !in_osc(b, i) {
                    out.push('\x07');
                }

                i += 1;
            }

            0x1b if b.get(i + 1) == Some(&b'[') => {
                // CSI: parameters, intermediates, final byte
                let start = i;
                let mut j = i + 2;

                while j < b.len() && (0x20..=0x3f).contains(&b[j]) {
                    j += 1;
                }

                if j < b.len() && b[j] == b'q' && b[j - 1] == b' ' {
                    out.push_str(&s[start..=j]);
                }

                i = j + 1;
            }

            _ => i += 1,
        }
    }

    out
}

/// Whether the byte at `i` is inside an OSC sequence (`ESC ]` ... BEL).
fn in_osc(b: &[u8], i: usize) -> bool {
    let Some(start) = b[..i].windows(2).rposition(|w| w == b"\x1b]") else {
        return false;
    };

    // not if something ended it in between
    !b[start + 2..i].iter().any(|&c| c == 0x07 || c == 0x1b)
}

fn new_vt(cols: usize, rows: usize) -> Vt {
    Vt::builder().size(cols, rows).scrollback_limit(0).build()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use avt::Vt;

    use super::compact;
    use crate::asciicast::{Event, EventData};

    fn screen(vt: &Vt) -> (Vec<String>, usize, usize, bool) {
        let c = vt.cursor();
        let lines = vt.view().map(|l| l.text()).collect();

        // where it's drawn: just after the last column (waiting to wrap)
        // shows in the last column
        (lines, c.row, c.col.min(vt.size().0 - 1), c.visible)
    }

    fn pens(vt: &Vt) -> Vec<Vec<avt::Pen>> {
        vt.view()
            .map(|l| l.cells().iter().map(|c| *c.pen()).collect())
            .collect()
    }

    /// The compacted recording shows the same screen as the original after
    /// every event (dropped ones: by the time the next one has played).
    pub fn check(size: (usize, usize), events: &[Event], fps: Option<u32>) {
        let compacted = compact(size, events, fps);
        let slot = |e: &Event| fps.map(|f| (e.time.as_secs_f64() * f as f64).floor() as u64);

        let mut a = Vt::builder().size(size.0, size.1).build();
        let mut b = Vt::builder().size(size.0, size.1).build();
        let mut next = 0;

        for (i, e) in events.iter().enumerate() {
            feed(&mut a, &e.data);

            // play the compacted events up to this time
            while next < compacted.len() && compacted[next].time <= e.time {
                feed(&mut b, &compacted[next].data);
                next += 1;
            }

            // only compare where no later original event shares this time
            // (or, with a frame rate, this frame)
            if events
                .get(i + 1)
                .is_some_and(|n| n.time == e.time || (fps.is_some() && slot(n) == slot(e)))
            {
                continue;
            }

            assert_eq!(screen(&a), screen(&b), "after event {i}");
            assert_eq!(pens(&a), pens(&b), "colours after event {i}");
        }
    }

    fn feed(vt: &mut Vt, data: &EventData) {
        match data {
            EventData::Output(s) => {
                vt.feed_str(s);
            }
            EventData::Resize(c, r) => {
                vt.resize(*c as usize, *r as usize);
            }
            _ => {}
        }
    }

    fn out(ms: u64, s: &str) -> Event {
        Event::output(Duration::from_millis(ms), s.to_owned())
    }

    #[test]
    fn same_screens_scrolls_and_drops_invisible_events() {
        let mut events = vec![out(0, "\x1b[?2004h\x1b]0;title\x07$ ")];

        // shell output scrolling a 20x5 screen
        for i in 0..12 {
            events.push(out(
                100 + i * 10,
                &format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 8),
            ));
        }

        // a full-screen program: alternate screen, scroll region, colours
        events.push(out(
            300,
            "\x1b[?1049h\x1b[H\x1b[2J\x1b[2;4r\x1b[2;1Hin\x1b[44m region\x1b[0m",
        ));
        events.push(out(310, "\x1b[4;1H\nscrolled\x1b[r"));
        events.push(out(320, "\x1b[?25l"));
        events.push(out(330, "\x1b[?1049l"));
        events.push(out(340, "\x1b[?2004l")); // nothing visible
        events.push(out(350, "wide: 日本"));

        super::tests::check((20, 5), &events, None);
        super::tests::check((20, 5), &events, Some(60));

        let compacted = compact((20, 5), &events, None);

        // the invisible event is gone
        assert!(!compacted
            .iter()
            .any(|e| e.time == Duration::from_millis(340)));

        // scrolling is a scroll, not a redraw of every line
        let scrolled = compacted
            .iter()
            .find(|e| e.time == Duration::from_millis(200))
            .unwrap();
        let EventData::Output(s) = &scrolled.data else {
            panic!()
        };
        assert!(s.len() < 40, "{s:?}");
    }

    #[test]
    fn cursor_shape_and_bell_pass_through() {
        let compacted = compact((10, 2), &[out(0, "\x1b]0;t\x07a\x07\x1b[5 q")], None);

        let EventData::Output(s) = &compacted[0].data else {
            panic!()
        };
        assert!(s.contains("\x07"), "{s:?}");
        assert!(s.contains("\x1b[5 q"), "{s:?}");
        assert_eq!(
            s.matches('\x07').count(),
            1,
            "the title's terminator isn't a bell"
        );
    }

    /// Compacts the recording at $COMPACT_CHECK and checks it plays the same.
    #[test]
    #[ignore]
    fn check_a_recording() {
        let path = std::env::var("COMPACT_CHECK").expect("COMPACT_CHECK=path");
        let cast = crate::asciicast::open_from_path(&path).unwrap();
        let size = (
            cast.header.term_cols as usize,
            cast.header.term_rows as usize,
        );
        let events: Vec<Event> = cast.events.map(|e| e.unwrap()).collect();

        check(size, &events, None);
        check(size, &events, Some(60));
    }

    #[test]
    fn a_frame_rate_keeps_the_last_screen_of_each_frame() {
        // 100 frames in a second, counting up
        let events: Vec<Event> = (0..100).map(|i| out(i * 10, &format!("\r{i:3}"))).collect();
        let compacted = compact((10, 1), &events, Some(30));

        assert!(compacted.len() <= 31, "{}", compacted.len());
        super::tests::check((10, 1), &events, Some(30));

        // the last one is there, at its own time
        let last = compacted.last().unwrap();
        assert_eq!(last.time, Duration::from_millis(990));
    }
}
