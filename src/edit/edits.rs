use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::asciicast::{CursorShape, Header};
use crate::tty::TtyTheme;

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

    /// Set (or with `None`, remove) the recording's title.
    SetTitle { title: Option<String> },

    /// Set (or remove) the recorded command shown in the metadata.
    SetCommand { command: Option<String> },

    /// Set (or remove) the idle time limit players apply on playback.
    SetIdleTimeLimit { secs: Option<f64> },

    /// Replace (or remove) the terminal color theme.
    SetTheme { theme: Option<ThemeColors> },

    /// Cut events `from..=to`. Playback jumps straight to the screen as it
    /// looked after the cut (a jump cut), so everything after stays intact.
    Delete { from: usize, to: usize },

    /// Insert a clip at position `at`: after `at` events, before event `at`.
    /// When the clip ends, the screen is redrawn as it was before it.
    Insert { at: usize, clip: Clip },

    /// Play a clip instead of events `from..=to`. When it ends, the screen is
    /// as it looked after those events.
    Replace { from: usize, to: usize, clip: Clip },

    /// Change text on screen, from position `at` (after `at` events): the
    /// text `from` at `row`, `col` becomes `to`, in the original cells' style.
    /// A shorter `to` is padded with spaces; a longer one overwrites the
    /// cells after `from`.
    Text {
        at: usize,
        row: usize,
        col: usize,
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "TextMode::is_default")]
        mode: TextMode,
        /// How `to` looks, as runs covering its characters. Empty: each
        /// character takes the style of the cell it's drawn in.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        styles: Vec<super::style::StyleRun>,
    },
}

/// How long a text edit lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TextMode {
    /// Drawn once; it stays (and scrolls with the screen) until the
    /// recording draws over those cells.
    #[default]
    UntilDrawnOver,

    /// Wherever `from` appears on screen, from `at` until event `until`
    /// (the end when omitted) - surviving repaints and scrolling.
    Everywhere {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<usize>,
        #[serde(default, skip_serializing_if = "Occurrences::is_every")]
        occurrences: Occurrences,
    },
}

impl TextMode {
    fn is_default(&self) -> bool {
        *self == TextMode::UntilDrawnOver
    }
}

/// Which occurrences of the text an "everywhere" edit changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Occurrences {
    /// Every occurrence on screen.
    #[default]
    Every,
    /// Only the one nearest where the edit was made.
    Nearest,
}

impl Occurrences {
    fn is_every(&self) -> bool {
        *self == Occurrences::Every
    }
}

/// Something inserted into the timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Clip {
    /// Freeze the current screen.
    Hold { secs: f64 },

    /// A cleared screen.
    Blank { secs: f64 },

    /// Text centered on a cleared screen.
    Text { text: String, secs: f64 },

    /// A text effect captured from ttfx/tte, cached in `cache` (relative to
    /// the recording's directory). It plays over the screen at the insert
    /// point, its last frame holds for `hold` seconds, then the screen is
    /// restored.
    Effect {
        fx: super::fx::FxSpec,
        hold: f64,
        cache: String,
    },

    /// Events `from..=to` (`to` omitted: to the end) of another recording,
    /// or of this one when `path` is omitted.
    Recording {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        from: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<usize>,
    },
}

impl Clip {
    pub fn describe(&self) -> String {
        match self {
            Clip::Hold { secs } => format!("hold for {secs}s"),
            Clip::Blank { secs } => format!("blank screen for {secs}s"),
            Clip::Text { text, secs } => {
                let first = text.lines().next().unwrap_or("");
                format!("text \"{first}\" for {secs}s")
            }
            Clip::Effect { fx, .. } => format!("{} effect on {}", fx.effect, fx.source.describe()),

            Clip::Recording { path, from, to } => {
                let range = match to {
                    Some(to) => format!("{from}..{to}"),
                    None => format!("{from}.."),
                };

                match path {
                    Some(p) => format!("events {range} of {p}"),
                    None => format!("copy of events {range}"),
                }
            }
        }
    }
}

/// A color theme as stored in the edit list, with `#rrggbb` colors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeColors {
    pub fg: String,
    pub bg: String,
    pub palette: Vec<String>,
}

impl ThemeColors {
    pub fn from_tty(theme: &TtyTheme) -> Self {
        Self {
            fg: to_hex(theme.fg),
            bg: to_hex(theme.bg),
            palette: theme.palette.iter().map(|&c| to_hex(c)).collect(),
        }
    }

    pub fn to_tty(&self) -> Result<TtyTheme> {
        let color = |s: &str| parse_hex(s).with_context(|| format!("invalid color '{s}'"));

        if self.palette.len() != 16 {
            bail!(
                "a theme palette needs 16 colors, got {}",
                self.palette.len()
            );
        }

        Ok(TtyTheme {
            fg: color(&self.fg)?,
            bg: color(&self.bg)?,
            palette: self
                .palette
                .iter()
                .map(|c| color(c))
                .collect::<Result<_>>()?,
        })
    }
}

/// Parses `#rrggbb`.
pub fn parse_hex(s: &str) -> Option<rgb::RGB8> {
    let hex = s.strip_prefix('#')?;

    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();

    Some(rgb::RGB8::new(byte(0)?, byte(2)?, byte(4)?))
}

pub fn to_hex(c: rgb::RGB8) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// Applies the header edits in `edits` (the last of each kind wins) to `original`.
pub fn apply_header(original: &Header, edits: &[Edit]) -> Header {
    let mut header = original.clone();

    for edit in edits {
        match edit {
            Edit::SetCursor { shape } => header.term_cursor = *shape,
            Edit::SetTitle { title } => header.title = title.clone(),
            Edit::SetCommand { command } => header.command = command.clone(),
            Edit::SetIdleTimeLimit { secs } => header.idle_time_limit = *secs,

            Edit::SetTheme { theme } => {
                // validated on push, so a bad theme can only come from a
                // hand-edited file; ignore it rather than failing
                header.term_theme = theme.as_ref().and_then(|t| t.to_tty().ok());
            }

            Edit::SetDelay { .. }
            | Edit::Scale { .. }
            | Edit::CapIdle { .. }
            | Edit::Delete { .. }
            | Edit::Insert { .. }
            | Edit::Replace { .. }
            | Edit::Text { .. } => {}
        }
    }

    header
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

            Edit::Scale { .. }
            | Edit::CapIdle { .. }
            | Edit::Delete { .. }
            | Edit::Insert { .. }
            | Edit::Replace { .. }
            | Edit::Text { .. } => {}

            // header edits: one of each kind
            _ => {
                let kind = std::mem::discriminant(&edit);
                self.edits.retain(|e| std::mem::discriminant(e) != kind);
            }
        }

        let replaced = self.edits.len() != len;
        self.edits.push(edit);

        Ok(replaced)
    }

    pub fn undo(&mut self) -> Option<Edit> {
        self.edits.pop()
    }

    /// Un-cuts events `from..=to`: shrinks or splits the delete edits covering
    /// them. Returns whether anything was restored.
    pub fn restore(&mut self, from: usize, to: usize) -> bool {
        let mut changed = false;
        let mut out = Vec::with_capacity(self.edits.len());

        for edit in self.edits.drain(..) {
            match edit {
                Edit::Delete { from: a, to: b } if a <= to && from <= b => {
                    changed = true;

                    if a < from {
                        out.push(Edit::Delete {
                            from: a,
                            to: from - 1,
                        });
                    }

                    if b > to {
                        out.push(Edit::Delete {
                            from: to + 1,
                            to: b,
                        });
                    }
                }

                other => out.push(other),
            }
        }

        self.edits = out;

        changed
    }

    /// Whether original event `i` is cut.
    pub fn is_deleted(&self, i: usize) -> bool {
        self.edits
            .iter()
            .any(|e| matches!(*e, Edit::Delete { from, to } if (from..=to).contains(&i)))
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

            Edit::SetCursor { .. } | Edit::SetTitle { .. } | Edit::SetCommand { .. } => Ok(()),

            Edit::SetIdleTimeLimit { secs } => match secs {
                Some(v) if !v.is_finite() || v <= 0.0 => {
                    bail!("idle time limit must be a positive number of seconds, got {v}")
                }
                _ => Ok(()),
            },

            Edit::SetTheme { ref theme } => {
                if let Some(t) = theme {
                    t.to_tty()?;
                }

                Ok(())
            }

            Edit::Delete { from, to } => check_range(from, to),

            Edit::Insert { at, ref clip } => {
                if at > event_count {
                    bail!("insert position {at} out of range (0..={event_count})");
                }

                validate_clip(clip, event_count)
            }

            Edit::Replace { from, to, ref clip } => {
                check_range(from, to)?;
                validate_clip(clip, event_count)
            }

            Edit::Text {
                at,
                ref from,
                ref to,
                mode,
                ref styles,
                ..
            } => {
                if at > event_count {
                    bail!("text edit position {at} out of range (0..={event_count})");
                }

                let styled: usize = styles.iter().map(|r| r.len).sum();

                if !styles.is_empty() && styled != to.chars().count() {
                    bail!(
                        "a text edit's styles cover {styled} characters, but its text has {}",
                        to.chars().count()
                    );
                }

                // (an "until drawn over" edit may insert, replacing nothing)
                if from.is_empty() && (to.is_empty() || matches!(mode, TextMode::Everywhere { .. }))
                {
                    bail!("a text edit needs the text it changes");
                }

                if from.contains(['\n', '\r']) || to.contains(['\n', '\r']) {
                    bail!("a text edit changes text on one line");
                }

                if matches!(mode, TextMode::Everywhere { .. }) && from.trim().is_empty() {
                    bail!("to change text wherever it appears, it can't be just spaces");
                }

                // the new text would be found and changed again, growing each time
                // (the same text, restyled, is fine: it doesn't grow)
                if matches!(mode, TextMode::Everywhere { .. })
                    && to != from
                    && to.contains(from.as_str())
                {
                    bail!(
                        "to change text wherever it appears, the new text can't contain the old (\"{to}\" contains \"{from}\")"
                    );
                }

                if let TextMode::Everywhere { until: Some(u), .. } = mode {
                    if u < at || u > event_count {
                        bail!("text edit end {u} must be between {at} and {event_count}");
                    }
                }

                Ok(())
            }
        }
    }
}

fn validate_clip(clip: &Clip, event_count: usize) -> Result<()> {
    let check_index = |i: usize| -> Result<()> {
        if i >= event_count {
            bail!(
                "event index {i} out of range (recording has {event_count} events, 0..={})",
                event_count.saturating_sub(1)
            );
        }

        Ok(())
    };

    match clip {
        Clip::Hold { secs } | Clip::Blank { secs } | Clip::Text { secs, .. } => {
            if !secs.is_finite() || *secs <= 0.0 {
                bail!("clip duration must be a positive number of seconds, got {secs}");
            }
        }

        Clip::Recording { .. } => {}

        Clip::Effect { fx, hold, cache } => {
            fx.validate()?;

            if !hold.is_finite() || *hold < 0.0 {
                bail!("effect hold must be zero or more seconds, got {hold}");
            }

            if cache.is_empty() {
                bail!("effect has no captured frames");
            }
        }
    }

    match clip {
        Clip::Text { text, .. } if text.trim().is_empty() => {
            bail!("a text clip needs some text")
        }

        Clip::Recording {
            from, to: Some(to), ..
        } if from > to => {
            bail!("invalid event range {from}..{to}: start is after end")
        }

        Clip::Recording {
            path: None,
            from,
            to,
            ..
        } => {
            check_index(*from)?;

            if let Some(to) = to {
                check_index(*to)?;
            }

            Ok(())
        }

        _ => Ok(()),
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
    fn header_edits() {
        use crate::asciicast::Header;

        use super::{apply_header, ThemeColors};

        let original = Header {
            title: Some("old".to_owned()),
            idle_time_limit: Some(2.0),
            ..Default::default()
        };

        let theme = ThemeColors {
            fg: "#ffffff".to_owned(),
            bg: "#000000".to_owned(),
            palette: vec!["#112233".to_owned(); 16],
        };

        let mut list = EditList::new(3);
        list.push(Edit::SetTitle {
            title: Some("first".to_owned()),
        })
        .unwrap();
        list.push(Edit::SetDelay {
            event: 1,
            secs: 1.0,
        })
        .unwrap();
        list.push(Edit::SetTitle {
            title: Some("second".to_owned()),
        })
        .unwrap();
        list.push(Edit::SetIdleTimeLimit { secs: None }).unwrap();
        list.push(Edit::SetTheme { theme: Some(theme) }).unwrap();

        // one entry per header field
        assert_eq!(list.edits.len(), 4);

        let h = apply_header(&original, &list.edits);
        assert_eq!(h.title.as_deref(), Some("second"));
        assert_eq!(h.idle_time_limit, None);
        assert_eq!(
            h.term_theme.unwrap().palette[3],
            rgb::RGB8::new(0x11, 0x22, 0x33)
        );

        assert!(list
            .push(Edit::SetIdleTimeLimit { secs: Some(0.0) })
            .is_err());

        let bad = ThemeColors {
            fg: "red".to_owned(),
            bg: "#000000".to_owned(),
            palette: vec!["#000000".to_owned(); 16],
        };
        assert!(list.push(Edit::SetTheme { theme: Some(bad) }).is_err());

        let short = ThemeColors {
            fg: "#000000".to_owned(),
            bg: "#000000".to_owned(),
            palette: vec!["#000000".to_owned(); 8],
        };
        assert!(list.push(Edit::SetTheme { theme: Some(short) }).is_err());
    }

    #[test]
    fn hex_colors() {
        use super::{parse_hex, to_hex};

        assert_eq!(parse_hex("#0c0C0c"), Some(rgb::RGB8::new(12, 12, 12)));
        assert_eq!(parse_hex("0c0c0c"), None);
        assert_eq!(parse_hex("#0c0c0"), None);
        assert_eq!(parse_hex("#0c0c0g"), None);
        assert_eq!(to_hex(rgb::RGB8::new(255, 0, 16)), "#ff0010");
    }

    #[test]
    fn delete_and_restore() {
        let mut list = EditList::new(20);
        list.push(Edit::Delete { from: 5, to: 10 }).unwrap();
        list.push(Edit::SetDelay {
            event: 7,
            secs: 1.0,
        })
        .unwrap();
        assert!(list.is_deleted(5) && list.is_deleted(10) && !list.is_deleted(11));

        // restoring the middle splits the cut
        assert!(list.restore(7, 8));
        assert_eq!(
            list.edits,
            [
                Edit::Delete { from: 5, to: 6 },
                Edit::Delete { from: 9, to: 10 },
                Edit::SetDelay {
                    event: 7,
                    secs: 1.0
                },
            ]
        );

        // restoring a range covering both removes them
        assert!(list.restore(0, 19));
        assert_eq!(
            list.edits,
            [Edit::SetDelay {
                event: 7,
                secs: 1.0
            }]
        );
        assert!(!list.restore(0, 19));

        assert!(list.push(Edit::Delete { from: 3, to: 2 }).is_err());
        assert!(list.push(Edit::Delete { from: 0, to: 20 }).is_err());
    }

    #[test]
    fn insert_validation_and_json() {
        use super::Clip;

        let mut list = EditList::new(10);
        let ok = [
            Clip::Hold { secs: 1.0 },
            Clip::Blank { secs: 0.5 },
            Clip::Text {
                text: "Hi".to_owned(),
                secs: 2.0,
            },
            Clip::Recording {
                path: None,
                from: 2,
                to: Some(4),
            },
            Clip::Recording {
                path: Some("other.cast".to_owned()),
                from: 0,
                to: None,
            },
        ];

        for clip in ok {
            list.push(Edit::Insert { at: 10, clip }).unwrap();
        }

        assert_eq!(list.edits.len(), 5, "inserts never coalesce");

        let bad = [
            (11, Clip::Hold { secs: 1.0 }),
            (0, Clip::Hold { secs: 0.0 }),
            (
                0,
                Clip::Text {
                    text: "  ".to_owned(),
                    secs: 1.0,
                },
            ),
            (
                0,
                Clip::Recording {
                    path: None,
                    from: 4,
                    to: Some(2),
                },
            ),
            (
                0,
                Clip::Recording {
                    path: None,
                    from: 0,
                    to: Some(10),
                },
            ),
        ];

        for (at, clip) in bad {
            assert!(
                list.push(Edit::Insert {
                    at,
                    clip: clip.clone()
                })
                .is_err(),
                "{clip:?}"
            );
        }

        let json = serde_json::to_value(&list.edits[3]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"op": "insert", "at": 10, "clip": {"kind": "recording", "from": 2, "to": 4}})
        );
    }

    #[test]
    fn text_edits() {
        use super::{Occurrences, TextMode};

        let t = |at: usize, from: &str, to: &str, mode: TextMode| Edit::Text {
            at,
            row: 1,
            col: 4,
            from: from.to_owned(),
            to: to.to_owned(),
            mode,
            styles: Vec::new(),
        };

        let mut list = EditList::new(10);
        let edit = t(3, "hunter2", "*******", TextMode::UntilDrawnOver);

        list.push(edit.clone()).unwrap();
        list.push(edit.clone()).unwrap();
        assert_eq!(list.edits.len(), 2, "text edits never coalesce");

        assert_eq!(
            serde_json::to_value(&edit).unwrap(),
            serde_json::json!({"op": "text", "at": 3, "row": 1, "col": 4, "from": "hunter2", "to": "*******"})
        );

        let everywhere = t(
            3,
            "hunter2",
            "*******",
            TextMode::Everywhere {
                until: Some(8),
                occurrences: Occurrences::Nearest,
            },
        );
        let json = serde_json::to_value(&everywhere).unwrap();
        assert_eq!(
            json["mode"],
            serde_json::json!({"kind": "everywhere", "until": 8, "occurrences": "nearest"})
        );
        assert_eq!(serde_json::from_value::<Edit>(json).unwrap(), everywhere);

        let bad = |e: Edit| EditList::new(10).push(e).is_err();
        assert!(bad(t(11, "a", "b", TextMode::UntilDrawnOver)));
        // an insert (replacing nothing) is fine until drawn over, not everywhere
        assert!(!bad(t(3, "", "b", TextMode::UntilDrawnOver)));
        assert!(bad(t(3, "", "", TextMode::UntilDrawnOver)));
        assert!(bad(t(
            3,
            "",
            "b",
            TextMode::Everywhere {
                until: None,
                occurrences: Occurrences::Every,
            }
        )));
        assert!(bad(t(3, "a", "b\nc", TextMode::UntilDrawnOver)));
        assert!(bad(t(
            3,
            "a",
            "b",
            TextMode::Everywhere {
                until: Some(2),
                occurrences: Occurrences::Every,
            }
        )));
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
