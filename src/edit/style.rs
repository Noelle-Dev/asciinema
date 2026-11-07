//! Text styles (colours and attributes) for text edits.

use std::fmt::Write as _;

use anyhow::{bail, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A colour: one of the 256 terminal palette colours, or 24-bit RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleColor {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub const COLOR_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright-black",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
    "bright-white",
];

impl StyleColor {
    /// Parses a palette index (`0`-`255`), a colour name (`red`,
    /// `bright-blue`, ...) or `#rrggbb`.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim().to_lowercase();

        if let Some(c) = crate::edit::edits::parse_hex(&s) {
            return Ok(StyleColor::Rgb(c.r, c.g, c.b));
        }

        if let Ok(n) = s.parse::<u8>() {
            return Ok(StyleColor::Indexed(n));
        }

        match COLOR_NAMES.iter().position(|n| *n == s) {
            Some(i) => Ok(StyleColor::Indexed(i as u8)),
            None => {
                bail!("unknown colour '{s}': use a name like red or bright-blue, 0-255, or #rrggbb")
            }
        }
    }

    fn sgr(self, base: u8) -> String {
        match self {
            StyleColor::Indexed(n) if n < 8 => format!(";{}", base + n),
            StyleColor::Indexed(n) if n < 16 => format!(";{}", base + 60 + n - 8),
            StyleColor::Indexed(n) => format!(";{};5;{n}", base + 8),
            StyleColor::Rgb(r, g, b) => format!(";{};2;{r};{g};{b}", base + 8),
        }
    }

    fn from_avt(c: avt::Color) -> Self {
        match c {
            avt::Color::Indexed(n) => StyleColor::Indexed(n),
            avt::Color::RGB(c) => StyleColor::Rgb(c.r, c.g, c.b),
        }
    }
}

impl std::fmt::Display for StyleColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StyleColor::Indexed(n) => match COLOR_NAMES.get(*n as usize) {
                Some(name) => write!(f, "{name}"),
                None => write!(f, "{n}"),
            },
            StyleColor::Rgb(r, g, b) => write!(f, "#{r:02x}{g:02x}{b:02x}"),
        }
    }
}

impl Serialize for StyleColor {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for StyleColor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        StyleColor::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// How a character looks. `None` colours are the terminal's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TextStyle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<StyleColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<StyleColor>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub faint: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub blink: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub inverse: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strike: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl TextStyle {
    pub fn from_pen(pen: &avt::Pen) -> Self {
        Self {
            fg: pen.foreground().map(StyleColor::from_avt),
            bg: pen.background().map(StyleColor::from_avt),
            bold: pen.is_bold(),
            faint: pen.is_faint(),
            italic: pen.is_italic(),
            underline: pen.is_underline(),
            blink: pen.is_blink(),
            inverse: pen.is_inverse(),
            strike: pen.is_strikethrough(),
        }
    }

    /// An SGR sequence fully setting this style (starting from a reset).
    pub fn sgr(&self) -> String {
        let mut s = String::from("\x1b[0");

        for (on, code) in [
            (self.bold, "1"),
            (self.faint, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.blink, "5"),
            (self.inverse, "7"),
            (self.strike, "9"),
        ] {
            if on {
                let _ = write!(s, ";{code}");
            }
        }

        if let Some(c) = self.fg {
            s.push_str(&c.sgr(30));
        }

        if let Some(c) = self.bg {
            s.push_str(&c.sgr(40));
        }

        s.push('m');

        s
    }
}

/// Styles for a run of `len` consecutive characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleRun {
    pub len: usize,
    #[serde(flatten)]
    pub style: TextStyle,
}

/// Per-character styles as runs.
pub fn to_runs(styles: &[TextStyle]) -> Vec<StyleRun> {
    let mut runs: Vec<StyleRun> = Vec::new();

    for s in styles {
        match runs.last_mut() {
            Some(r) if r.style == *s => r.len += 1,
            _ => runs.push(StyleRun { len: 1, style: *s }),
        }
    }

    runs
}

/// Runs back to per-character styles.
pub fn from_runs(runs: &[StyleRun]) -> Vec<TextStyle> {
    runs.iter()
        .flat_map(|r| std::iter::repeat_n(r.style, r.len))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{from_runs, to_runs, StyleColor, TextStyle};

    #[test]
    fn colours_parse_and_print() {
        assert_eq!(StyleColor::parse("red").unwrap(), StyleColor::Indexed(1));
        assert_eq!(
            StyleColor::parse("Bright-Blue").unwrap(),
            StyleColor::Indexed(12)
        );
        assert_eq!(StyleColor::parse("208").unwrap(), StyleColor::Indexed(208));
        assert_eq!(
            StyleColor::parse("#FF8000").unwrap(),
            StyleColor::Rgb(255, 128, 0)
        );
        assert!(StyleColor::parse("redish").is_err());

        assert_eq!(StyleColor::Indexed(9).to_string(), "bright-red");
        assert_eq!(StyleColor::Indexed(200).to_string(), "200");
        assert_eq!(StyleColor::Rgb(1, 2, 255).to_string(), "#0102ff");
    }

    #[test]
    fn sgr() {
        let s = TextStyle {
            fg: Some(StyleColor::Indexed(1)),
            bg: Some(StyleColor::Rgb(0, 0, 255)),
            bold: true,
            underline: true,
            ..Default::default()
        };

        assert_eq!(s.sgr(), "\x1b[0;1;4;31;48;2;0;0;255m");
        assert_eq!(TextStyle::default().sgr(), "\x1b[0m");
        assert_eq!(
            TextStyle {
                fg: Some(StyleColor::Indexed(12)),
                ..Default::default()
            }
            .sgr(),
            "\x1b[0;94m"
        );
    }

    #[test]
    fn runs_round_trip_and_json() {
        let red = TextStyle {
            fg: Some(StyleColor::Indexed(1)),
            ..Default::default()
        };
        let styles = [red, red, TextStyle::default(), red];

        let runs = to_runs(&styles);
        assert_eq!(runs.len(), 3);
        assert_eq!(from_runs(&runs), styles);

        assert_eq!(
            serde_json::to_value(&runs).unwrap(),
            serde_json::json!([{"len": 2, "fg": "red"}, {"len": 1}, {"len": 1, "fg": "red"}])
        );

        let back: Vec<super::StyleRun> =
            serde_json::from_str(r##"[{"len":3,"fg":"#ff0000","bold":true}]"##).unwrap();
        assert_eq!(back[0].style.fg, Some(StyleColor::Rgb(255, 0, 0)));
        assert!(back[0].style.bold);
    }
}
