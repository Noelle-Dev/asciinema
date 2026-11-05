use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::asciicast::CursorShape;

const FORMAT_VERSION: u32 = 1;

/// A single edit operation.
///
/// Events are addressed by their index in the *original* recording (counting
/// every event: output, input, resize, marker, exit). Indexes stay stable no
/// matter how the timing is changed, so edits can be freely stacked and undone.
///
/// "Delay of event N" means the time between event N-1 and event N (for event
/// 0, the time from the start of the recording).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Edit {
    /// Set the delay before a single event to an exact value.
    SetDelay { event: usize, secs: f64 },

    /// Multiply delays of events `from..=to` by `factor` (<1 speeds up, >1 slows down).
    Scale { from: usize, to: usize, factor: f64 },

    /// Cap delays of events `from..=to` at `max` seconds.
    CapIdle { from: usize, to: usize, max: f64 },

    /// Set the cursor shape stored in the recording's header (`None` removes
    /// it, so playback uses the terminal's default).
    SetCursor { shape: Option<CursorShape> },
}

/// An ordered list of edits for one recording. Edits are applied in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditList {
    pub version: u32,

    /// Number of events in the source recording, used to detect that the edit
    /// list is being applied to a different (or modified) recording.
    pub event_count: usize,

    #[serde(default)]
    pub edits: Vec<Edit>,
}

impl EditList {
    pub fn new(event_count: usize) -> Self {
        Self {
            version: FORMAT_VERSION,
            event_count,
            edits: Vec::new(),
        }
    }

    /// Default sidecar location: `demo.cast` -> `demo.cast.edits.json`.
    pub fn sidecar_path(recording: &Path) -> PathBuf {
        let mut name = recording.as_os_str().to_owned();
        name.push(".edits.json");

        PathBuf::from(name)
    }

    /// Loads the edit list at `path`, or returns an empty one if it doesn't exist.
    pub fn load_or_new(path: &Path, event_count: usize) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new(event_count));
        }

        let json = fs::read_to_string(path)
            .with_context(|| format!("can't read edit list {}", path.display()))?;

        let list: EditList = serde_json::from_str(&json)
            .with_context(|| format!("invalid edit list {}", path.display()))?;

        if list.version != FORMAT_VERSION {
            bail!(
                "unsupported edit list version {} in {}",
                list.version,
                path.display()
            );
        }

        if list.event_count != event_count {
            bail!(
                "edit list {} was made for a recording with {} events, but this one has {}",
                path.display(),
                list.event_count,
                event_count
            );
        }

        Ok(list)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;

        // Write via a temp file + rename so a crash never leaves a truncated edit list.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json + "\n")
            .with_context(|| format!("can't write edit list {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("can't write edit list {}", path.display()))?;

        Ok(())
    }

    /// Validates `edit` against this recording, then appends it.
    ///
    /// A `SetDelay` sets an absolute value, so it makes any earlier `SetDelay`
    /// of the same event redundant; those are removed rather than piling up.
    /// Returns whether an earlier edit was replaced.
    pub fn push(&mut self, edit: Edit) -> Result<bool> {
        edit.validate(self.event_count)?;
        let len = self.edits.len();

        // Edits that set an absolute value make earlier ones of the same kind redundant.
        match edit {
            Edit::SetDelay { event, .. } => self
                .edits
                .retain(|e| !matches!(*e, Edit::SetDelay { event: other, .. } if other == event)),

            Edit::SetCursor { .. } => self.edits.retain(|e| !matches!(e, Edit::SetCursor { .. })),

            _ => {}
        }

        let replaced = self.edits.len() != len;
        self.edits.push(edit);

        Ok(replaced)
    }

    pub fn undo(&mut self) -> Option<Edit> {
        self.edits.pop()
    }
}

impl Edit {
    pub fn validate(&self, event_count: usize) -> Result<()> {
        let check_index = |i: usize| -> Result<()> {
            if i >= event_count {
                bail!(
                    "event index {i} out of range (recording has {event_count} events, 0..={})",
                    event_count.saturating_sub(1)
                );
            }

            Ok(())
        };

        let check_range = |from: usize, to: usize| -> Result<()> {
            check_index(from)?;
            check_index(to)?;

            if from > to {
                bail!("invalid event range {from}..{to}: start is after end");
            }

            Ok(())
        };

        let check_secs = |name: &str, v: f64| -> Result<()> {
            if !v.is_finite() || v < 0.0 {
                bail!("{name} must be a non-negative number of seconds, got {v}");
            }

            Ok(())
        };

        match *self {
            Edit::SetDelay { event, secs } => {
                check_index(event)?;
                check_secs("delay", secs)
            }

            Edit::Scale { from, to, factor } => {
                check_range(from, to)?;

                if !factor.is_finite() || factor <= 0.0 {
                    bail!("scale factor must be a positive number, got {factor}");
                }

                Ok(())
            }

            Edit::CapIdle { from, to, max } => {
                check_range(from, to)?;
                check_secs("idle cap", max)
            }

            Edit::SetCursor { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;

    use super::{Edit, EditList};

    #[test]
    fn sidecar_path() {
        assert_eq!(
            EditList::sidecar_path(Path::new("dir/demo.cast")),
            Path::new("dir/demo.cast.edits.json")
        );
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("demo.cast.edits.json");
        let mut list = EditList::new(10);

        list.push(Edit::SetDelay {
            event: 3,
            secs: 0.5,
        })
        .unwrap();
        list.push(Edit::Scale {
            from: 0,
            to: 9,
            factor: 0.5,
        })
        .unwrap();
        list.save(&path).unwrap();

        assert_eq!(EditList::load_or_new(&path, 10).unwrap(), list);
    }

    #[test]
    fn json_format() {
        let mut list = EditList::new(5);
        list.push(Edit::CapIdle {
            from: 1,
            to: 4,
            max: 2.0,
        })
        .unwrap();

        let json: serde_json::Value = serde_json::to_value(&list).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "version": 1,
                "event_count": 5,
                "edits": [{"op": "cap_idle", "from": 1, "to": 4, "max": 2.0}]
            })
        );
    }

    #[test]
    fn missing_file_gives_empty_list() {
        let dir = tempdir().unwrap();
        let list = EditList::load_or_new(&dir.path().join("nope.json"), 7).unwrap();

        assert_eq!(list, EditList::new(7));
    }

    #[test]
    fn rejects_mismatched_recording() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.json");
        EditList::new(10).save(&path).unwrap();

        assert!(EditList::load_or_new(&path, 11).is_err());
    }

    #[test]
    fn validation() {
        let mut list = EditList::new(5);

        assert!(list
            .push(Edit::SetDelay {
                event: 5,
                secs: 1.0
            })
            .is_err());
        assert!(list
            .push(Edit::SetDelay {
                event: 0,
                secs: -1.0
            })
            .is_err());
        assert!(list
            .push(Edit::SetDelay {
                event: 0,
                secs: f64::NAN
            })
            .is_err());
        assert!(list
            .push(Edit::Scale {
                from: 3,
                to: 1,
                factor: 2.0
            })
            .is_err());
        assert!(list
            .push(Edit::Scale {
                from: 0,
                to: 1,
                factor: 0.0
            })
            .is_err());
        assert!(list
            .push(Edit::CapIdle {
                from: 0,
                to: 4,
                max: 0.0
            })
            .is_ok());
        assert_eq!(list.edits.len(), 1);
    }

    #[test]
    fn set_delay_replaces_earlier_set_delay_of_same_event() {
        let mut list = EditList::new(10);

        assert!(!list
            .push(Edit::SetDelay {
                event: 3,
                secs: 1.0
            })
            .unwrap());
        assert!(!list
            .push(Edit::CapIdle {
                from: 0,
                to: 9,
                max: 2.0
            })
            .unwrap());
        assert!(!list
            .push(Edit::SetDelay {
                event: 4,
                secs: 1.0
            })
            .unwrap());
        assert!(list
            .push(Edit::SetDelay {
                event: 3,
                secs: 5.0
            })
            .unwrap());

        assert_eq!(
            list.edits,
            [
                Edit::CapIdle {
                    from: 0,
                    to: 9,
                    max: 2.0
                },
                Edit::SetDelay {
                    event: 4,
                    secs: 1.0
                },
                Edit::SetDelay {
                    event: 3,
                    secs: 5.0
                },
            ]
        );
    }

    #[test]
    fn set_cursor_json_and_coalescing() {
        use crate::asciicast::CursorShape;

        let mut list = EditList::new(3);
        list.push(Edit::SetCursor {
            shape: Some(CursorShape::Bar),
        })
        .unwrap();
        list.push(Edit::SetDelay {
            event: 1,
            secs: 1.0,
        })
        .unwrap();
        assert!(list
            .push(Edit::SetCursor {
                shape: Some(CursorShape::Block)
            })
            .unwrap());
        list.push(Edit::SetCursor { shape: None }).unwrap();

        assert_eq!(list.edits.len(), 2);

        let json = serde_json::to_value(&list.edits).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"op": "set_delay", "event": 1, "secs": 1.0},
                {"op": "set_cursor", "shape": null},
            ])
        );

        let parsed: Edit =
            serde_json::from_str(r#"{"op":"set_cursor","shape":"blinking-bar"}"#).unwrap();
        assert_eq!(
            parsed,
            Edit::SetCursor {
                shape: Some(CursorShape::BlinkingBar)
            }
        );
    }

    #[test]
    fn undo() {
        let mut list = EditList::new(5);
        list.push(Edit::SetDelay {
            event: 1,
            secs: 1.0,
        })
        .unwrap();

        assert_eq!(
            list.undo(),
            Some(Edit::SetDelay {
                event: 1,
                secs: 1.0
            })
        );
        assert_eq!(list.undo(), None);
    }
}
