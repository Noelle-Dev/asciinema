//! Color picker state: HSL and RGB sliders plus hex entry, kept in sync.

use rgb::RGB8;

use crate::edit::edits::{parse_hex, to_hex};

/// Which theme color is being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Foreground,
    Background,
    Palette(usize),
}

pub const PALETTE_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright black",
    "bright red",
    "bright green",
    "bright yellow",
    "bright blue",
    "bright magenta",
    "bright cyan",
    "bright white",
];

impl Slot {
    pub fn label(self) -> String {
        match self {
            Slot::Foreground => "Foreground".to_owned(),
            Slot::Background => "Background".to_owned(),
            Slot::Palette(i) => format!("{i:>2} {}", PALETTE_NAMES.get(i).unwrap_or(&"")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slider {
    Hue,
    Saturation,
    Lightness,
    Red,
    Green,
    Blue,
}

impl Slider {
    pub const ALL: [Slider; 6] = [
        Slider::Hue,
        Slider::Saturation,
        Slider::Lightness,
        Slider::Red,
        Slider::Green,
        Slider::Blue,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Slider::Hue => "Hue",
            Slider::Saturation => "Saturation",
            Slider::Lightness => "Lightness",
            Slider::Red => "Red",
            Slider::Green => "Green",
            Slider::Blue => "Blue",
        }
    }

    pub fn max(self) -> f64 {
        match self {
            Slider::Hue => 360.0,
            Slider::Saturation | Slider::Lightness => 100.0,
            Slider::Red | Slider::Green | Slider::Blue => 255.0,
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Slider::Hue => "°",
            Slider::Saturation | Slider::Lightness => "%",
            _ => "",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColorPicker {
    pub slot: Slot,
    /// Color when the picker was opened (Esc returns to it).
    pub initial: RGB8,
    /// Color in the original recording, for reference.
    pub recorded: Option<RGB8>,
    pub rgb: RGB8,
    /// HSL kept separately so hue/saturation survive passing through grays/black.
    pub hsl: (f64, f64, f64),
    /// Focused row: 0..6 are sliders, 6 is the hex field.
    pub focus: usize,
    /// Hex being typed, when the hex field is being edited.
    pub hex_input: Option<String>,
}

pub const HEX_ROW: usize = Slider::ALL.len();

impl ColorPicker {
    pub fn new(slot: Slot, initial: RGB8, recorded: Option<RGB8>) -> Self {
        Self {
            slot,
            initial,
            recorded,
            rgb: initial,
            hsl: rgb_to_hsl(initial),
            focus: 0,
            hex_input: None,
        }
    }

    pub fn value(&self, s: Slider) -> f64 {
        let (h, sat, l) = self.hsl;

        match s {
            Slider::Hue => h,
            Slider::Saturation => sat * 100.0,
            Slider::Lightness => l * 100.0,
            Slider::Red => self.rgb.r as f64,
            Slider::Green => self.rgb.g as f64,
            Slider::Blue => self.rgb.b as f64,
        }
    }

    pub fn set(&mut self, s: Slider, v: f64) {
        let v = v.clamp(0.0, s.max());
        let (h, sat, l) = self.hsl;

        match s {
            Slider::Hue => self.set_hsl((v % 360.0, sat, l)),
            Slider::Saturation => self.set_hsl((h, v / 100.0, l)),
            Slider::Lightness => self.set_hsl((h, sat, v / 100.0)),
            Slider::Red => self.set_rgb(RGB8 {
                r: v.round() as u8,
                ..self.rgb
            }),
            Slider::Green => self.set_rgb(RGB8 {
                g: v.round() as u8,
                ..self.rgb
            }),
            Slider::Blue => self.set_rgb(RGB8 {
                b: v.round() as u8,
                ..self.rgb
            }),
        }
    }

    pub fn adjust(&mut self, s: Slider, delta: f64) {
        let mut v = self.value(s) + delta;

        // hue wraps around; everything else clamps
        if s == Slider::Hue {
            v = v.rem_euclid(360.0);
        }

        self.set(s, v);
    }

    fn set_hsl(&mut self, hsl: (f64, f64, f64)) {
        self.hsl = hsl;
        self.rgb = hsl_to_rgb(hsl);
    }

    pub fn set_rgb(&mut self, rgb: RGB8) {
        let (h, s, l) = rgb_to_hsl(rgb);
        let (old_h, old_s, _) = self.hsl;

        // In grays hue is undefined (and saturation in black/white): keep the old ones.
        let s_keep = if l <= 0.0 || l >= 1.0 { old_s } else { s };
        let h_keep = if s <= 0.0 || l <= 0.0 || l >= 1.0 {
            old_h
        } else {
            h
        };

        self.rgb = rgb;
        self.hsl = (h_keep, s_keep, l);
    }

    /// Commits typed hex if it's valid; returns whether it was.
    pub fn apply_hex(&mut self) -> bool {
        match self
            .hex_input
            .take()
            .as_deref()
            .map(normalize_hex)
            .and_then(|h| parse_hex(&h))
        {
            Some(rgb) => {
                self.set_rgb(rgb);
                true
            }

            None => false,
        }
    }

    pub fn hex(&self) -> String {
        to_hex(self.rgb)
    }

    /// Color the slider would produce at fraction `t` (0..=1), for drawing gradients.
    pub fn color_at(&self, s: Slider, t: f64) -> RGB8 {
        let mut probe = self.clone();
        probe.set(s, t * s.max());

        probe.rgb
    }
}

/// Accepts `rrggbb`, `#rrggbb`, `rgb` and `#rgb`.
pub fn normalize_hex(s: &str) -> String {
    let s = s.trim().trim_start_matches('#');

    if s.len() == 3 {
        let doubled: String = s.chars().flat_map(|c| [c, c]).collect();
        format!("#{doubled}")
    } else {
        format!("#{s}")
    }
}

pub fn rgb_to_hsl(c: RGB8) -> (f64, f64, f64) {
    let (r, g, b) = (c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;

    if d == 0.0 {
        return (0.0, 0.0, l);
    }

    let s = d / (1.0 - (2.0 * l - 1.0).abs());

    let h = if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };

    (h, s.clamp(0.0, 1.0), l)
}

pub fn hsl_to_rgb((h, s, l): (f64, f64, f64)) -> RGB8 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());

    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    let m = l - c / 2.0;
    let to_u8 = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;

    RGB8::new(to_u8(r), to_u8(g), to_u8(b))
}

#[cfg(test)]
mod tests {
    use rgb::RGB8;

    use super::{hsl_to_rgb, normalize_hex, rgb_to_hsl, ColorPicker, Slider, Slot};

    #[test]
    fn hsl_round_trips() {
        for c in [
            RGB8::new(0, 0, 0),
            RGB8::new(255, 255, 255),
            RGB8::new(0xc5, 0x0f, 0x1f),
            RGB8::new(0x13, 0xa1, 0x0e),
            RGB8::new(0x3b, 0x78, 0xff),
            RGB8::new(12, 12, 12),
            RGB8::new(1, 2, 3),
        ] {
            assert_eq!(hsl_to_rgb(rgb_to_hsl(c)), c, "{c:?}");
        }

        assert_eq!(hsl_to_rgb((0.0, 1.0, 0.5)), RGB8::new(255, 0, 0));
        assert_eq!(hsl_to_rgb((120.0, 1.0, 0.5)), RGB8::new(0, 255, 0));
        assert_eq!(hsl_to_rgb((240.0, 1.0, 0.5)), RGB8::new(0, 0, 255));
    }

    #[test]
    fn sliders_stay_in_sync() {
        let mut p = ColorPicker::new(Slot::Background, RGB8::new(255, 0, 0), None);

        p.set(Slider::Hue, 120.0);
        assert_eq!(p.rgb, RGB8::new(0, 255, 0));

        p.set(Slider::Blue, 255.0);
        assert_eq!(p.rgb, RGB8::new(0, 255, 255));
        assert!((p.value(Slider::Hue) - 180.0).abs() < 0.5);

        p.adjust(Slider::Hue, 200.0); // wraps 180 + 200 -> 20
        assert!((p.value(Slider::Hue) - 20.0).abs() < 0.5);

        p.adjust(Slider::Red, 1000.0);
        assert_eq!(p.rgb.r, 255);
    }

    #[test]
    fn hue_survives_desaturation() {
        let mut p = ColorPicker::new(Slot::Foreground, RGB8::new(0, 0, 255), None);

        p.set(Slider::Saturation, 0.0); // gray: hue undefined
        p.set(Slider::Saturation, 100.0);
        assert_eq!(p.rgb, RGB8::new(0, 0, 255), "hue should be remembered");

        p.set(Slider::Lightness, 0.0); // black
        p.set(Slider::Lightness, 50.0);
        assert_eq!(p.rgb, RGB8::new(0, 0, 255));
    }

    #[test]
    fn hex_entry() {
        let mut p = ColorPicker::new(Slot::Palette(1), RGB8::new(0, 0, 0), None);

        p.hex_input = Some("C50F1F".to_owned());
        assert!(p.apply_hex());
        assert_eq!(p.hex(), "#c50f1f");

        p.hex_input = Some("#abc".to_owned());
        assert!(p.apply_hex());
        assert_eq!(p.hex(), "#aabbcc");

        p.hex_input = Some("nope".to_owned());
        assert!(!p.apply_hex());
        assert_eq!(p.hex(), "#aabbcc", "invalid hex leaves the color unchanged");

        assert_eq!(normalize_hex(" 123456 "), "#123456");
    }
}
