//! Applying timing edits.
//!
//! Timing is manipulated as a vector of *delays* (the gap before each event)
//! rather than absolute timestamps. Editing one delay then naturally shifts
//! everything after it, and absolute times are recovered as a prefix sum.

use std::time::Duration;

use super::Edit;

/// Converts absolute event times into per-event delays.
///
/// Out-of-order timestamps (which shouldn't occur, but malformed files exist)
/// are treated as a zero delay rather than panicking.
pub fn delays(times: &[Duration]) -> Vec<Duration> {
    let mut prev = Duration::ZERO;

    times
        .iter()
        .map(|&t| {
            let d = t.saturating_sub(prev);
            prev = prev.max(t);

            d
        })
        .collect()
}

/// Converts per-event delays back into absolute event times.
pub fn times(delays: &[Duration]) -> Vec<Duration> {
    let mut acc = Duration::ZERO;

    delays
        .iter()
        .map(|&d| {
            acc += d;

            acc
        })
        .collect()
}

/// Applies `edits` in order to the given original event times, returning new times.
///
/// Edits are expected to be validated already (see [`Edit::validate`]); indexes
/// out of range are ignored here rather than panicking.
pub fn apply(original_times: &[Duration], edits: &[Edit]) -> Vec<Duration> {
    let mut delays = delays(original_times);
    let last = delays.len().saturating_sub(1);

    for edit in edits {
        match *edit {
            Edit::SetDelay { event, secs } => {
                if let Some(d) = delays.get_mut(event) {
                    *d = Duration::from_secs_f64(secs);
                }
            }

            Edit::Scale { from, to, factor } => {
                for d in delays.iter_mut().take(to.min(last) + 1).skip(from) {
                    *d = d.mul_f64(factor);
                }
            }

            Edit::CapIdle { from, to, max } => {
                let max = Duration::from_secs_f64(max);

                for d in delays.iter_mut().take(to.min(last) + 1).skip(from) {
                    *d = (*d).min(max);
                }
            }

            // header edits don't affect timing
            Edit::SetCursor { .. }
            | Edit::SetTitle { .. }
            | Edit::SetCommand { .. }
            | Edit::SetIdleTimeLimit { .. }
            | Edit::SetTheme { .. } => {}
        }
    }

    times(&delays)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{apply, delays, times};
    use crate::edit::Edit;

    fn secs(v: &[f64]) -> Vec<Duration> {
        v.iter().map(|&s| Duration::from_secs_f64(s)).collect()
    }

    fn assert_times(actual: Vec<Duration>, expected: &[f64]) {
        let actual: Vec<f64> = actual.iter().map(|d| d.as_secs_f64()).collect();

        assert_eq!(actual.len(), expected.len());

        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 1e-6, "{actual:?} != {expected:?}");
        }
    }

    #[test]
    fn delays_and_times_round_trip() {
        let t = secs(&[0.5, 1.0, 3.0, 3.0, 4.25]);

        assert_eq!(delays(&t), secs(&[0.5, 0.5, 2.0, 0.0, 1.25]));
        assert_eq!(times(&delays(&t)), t);
    }

    #[test]
    fn out_of_order_times_dont_panic() {
        assert_eq!(delays(&secs(&[2.0, 1.0, 3.0])), secs(&[2.0, 0.0, 1.0]));
    }

    #[test]
    fn no_edits_is_identity() {
        let t = secs(&[0.1, 0.2, 5.0]);

        assert_eq!(apply(&t, &[]), t);
    }

    #[test]
    fn set_delay_shifts_following_events() {
        let t = secs(&[1.0, 2.0, 10.0, 11.0]);
        let edits = [Edit::SetDelay {
            event: 2,
            secs: 0.5,
        }];

        assert_times(apply(&t, &edits), &[1.0, 2.0, 2.5, 3.5]);
    }

    #[test]
    fn set_delay_can_elongate() {
        let t = secs(&[1.0, 2.0]);
        let edits = [Edit::SetDelay {
            event: 1,
            secs: 4.0,
        }];

        assert_times(apply(&t, &edits), &[1.0, 5.0]);
    }

    #[test]
    fn scale_range() {
        let t = secs(&[1.0, 2.0, 4.0, 8.0]);
        let edits = [Edit::Scale {
            from: 1,
            to: 2,
            factor: 0.5,
        }];

        // delays 1,1,2,4 -> 1,0.5,1,4
        assert_times(apply(&t, &edits), &[1.0, 1.5, 2.5, 6.5]);
    }

    #[test]
    fn cap_idle_range() {
        let t = secs(&[5.0, 6.0, 16.0, 26.0]);
        let edits = [Edit::CapIdle {
            from: 0,
            to: 2,
            max: 2.0,
        }];

        // delays 5,1,10,10 -> 2,1,2,10
        assert_times(apply(&t, &edits), &[2.0, 3.0, 5.0, 15.0]);
    }

    #[test]
    fn edits_apply_in_order() {
        let t = secs(&[1.0, 11.0]);

        let a = [
            Edit::CapIdle {
                from: 1,
                to: 1,
                max: 2.0,
            },
            Edit::Scale {
                from: 1,
                to: 1,
                factor: 3.0,
            },
        ];

        let b = [
            Edit::Scale {
                from: 1,
                to: 1,
                factor: 3.0,
            },
            Edit::CapIdle {
                from: 1,
                to: 1,
                max: 2.0,
            },
        ];

        assert_times(apply(&t, &a), &[1.0, 7.0]);
        assert_times(apply(&t, &b), &[1.0, 3.0]);
    }
}
