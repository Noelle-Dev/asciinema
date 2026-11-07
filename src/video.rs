//! Rendering a recording to an MP4 video.
//!
//! The recording is replayed through a virtual terminal; each video frame is
//! the screen at that moment, drawn with a monospace font, in the recording's
//! colour theme and cursor shape. Frames are piped to ffmpeg as raw RGB and
//! encoded as H.264, which plays everywhere (phones included).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use avt::{Color, Vt};

use crate::asciicast::{Asciicast, CursorShape, EventData, Header};

/// How long the last screen stays up at the end of the video.
const TAIL: Duration = Duration::from_secs(1);

pub struct Options {
    /// Font size in pixels.
    pub font_size: f32,
    /// A monospace font file; found with fontconfig when not given.
    pub font: Option<PathBuf>,
    pub fps: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            font_size: 24.0,
            font: None,
            fps: 30,
        }
    }
}

type Rgb = [u8; 3];

/// The asciinema player's default theme, for recordings without their own.
const DEFAULT_FG: Rgb = [0xcc, 0xcc, 0xcc];
const DEFAULT_BG: Rgb = [0x12, 0x13, 0x14];
const DEFAULT_PALETTE: [Rgb; 16] = [
    [0x00, 0x00, 0x00],
    [0xdd, 0x3c, 0x69],
    [0x4e, 0xbf, 0x22],
    [0xdd, 0xaf, 0x3c],
    [0x26, 0xb0, 0xd7],
    [0xb9, 0x54, 0xe1],
    [0x54, 0xe1, 0xb9],
    [0xd9, 0xd9, 0xd9],
    [0x4d, 0x4d, 0x4d],
    [0xdd, 0x3c, 0x69],
    [0x4e, 0xbf, 0x22],
    [0xdd, 0xaf, 0x3c],
    [0x26, 0xb0, 0xd7],
    [0xb9, 0x54, 0xe1],
    [0x54, 0xe1, 0xb9],
    [0xff, 0xff, 0xff],
];

struct Theme {
    fg: Rgb,
    bg: Rgb,
    palette: [Rgb; 16],
}

impl Theme {
    fn of(header: &Header) -> Self {
        match &header.term_theme {
            Some(t) if t.palette.len() >= 16 => {
                let mut palette = [[0; 3]; 16];

                for (p, c) in palette.iter_mut().zip(&t.palette) {
                    *p = [c.r, c.g, c.b];
                }

                Self {
                    fg: [t.fg.r, t.fg.g, t.fg.b],
                    bg: [t.bg.r, t.bg.g, t.bg.b],
                    palette,
                }
            }

            _ => Self {
                fg: DEFAULT_FG,
                bg: DEFAULT_BG,
                palette: DEFAULT_PALETTE,
            },
        }
    }

    fn color(&self, c: Color) -> Rgb {
        match c {
            Color::RGB(c) => [c.r, c.g, c.b],
            Color::Indexed(i) if i < 16 => self.palette[i as usize],

            // the 6x6x6 cube
            Color::Indexed(i) if i < 232 => {
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };

                [level(i / 36), level((i / 6) % 6), level(i % 6)]
            }

            // the grayscale ramp
            Color::Indexed(i) => {
                let v = 8 + (i - 232) * 10;
                [v, v, v]
            }
        }
    }
}

struct Fonts {
    regular: fontdue::Font,
    bold: Option<fontdue::Font>,
    size: f32,
    glyphs: HashMap<(char, bool), (fontdue::Metrics, Vec<u8>)>,
}

impl Fonts {
    fn load(options: &Options) -> Result<Self> {
        let load = |path: &Path| -> Result<fontdue::Font> {
            let data = std::fs::read(path)
                .with_context(|| format!("can't read font {}", path.display()))?;

            fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
                .map_err(|e| anyhow!("can't load font {}: {e}", path.display()))
        };

        let (regular, bold) = match &options.font {
            Some(path) => (load(path)?, None),

            None => {
                let regular = find_font("monospace").context(
                    "can't find a monospace font; give one with --font (a .ttf or .otf file)",
                )?;
                let bold = find_font("monospace:bold").filter(|b| *b != regular);

                (load(&regular)?, bold.and_then(|b| load(&b).ok()))
            }
        };

        Ok(Self {
            regular,
            bold,
            size: options.font_size,
            glyphs: HashMap::new(),
        })
    }

    /// Cell size and baseline (from the cell's top), in pixels.
    fn cell(&self) -> (usize, usize, usize) {
        let width = self.regular.metrics('M', self.size).advance_width.ceil() as usize;
        let line = self
            .regular
            .horizontal_line_metrics(self.size)
            .expect("a horizontal font");
        let height = (line.ascent - line.descent + line.line_gap).ceil() as usize;
        let baseline = (line.ascent + line.line_gap / 2.0).round() as usize;

        (width.max(1), height.max(1), baseline)
    }

    fn glyph(&mut self, ch: char, bold: bool) -> &(fontdue::Metrics, Vec<u8>) {
        let (regular, bold_font, size) = (&self.regular, &self.bold, self.size);

        self.glyphs.entry((ch, bold)).or_insert_with(|| {
            let font = match bold_font {
                Some(b) if bold && b.has_glyph(ch) => b,
                _ => regular,
            };

            font.rasterize(ch, size)
        })
    }
}

/// A font file for a fontconfig pattern.
fn find_font(pattern: &str) -> Option<PathBuf> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .filter(|p| p.is_file());

    out.or_else(|| {
        [
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/System/Library/Fonts/Menlo.ttc",
            "/Library/Fonts/Courier New.ttf",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    })
}

/// Draws terminal screens as RGB images.
struct Painter {
    fonts: Fonts,
    theme: Theme,
    cursor: CursorShape,
    cols: usize,
    rows: usize,
    cell: (usize, usize, usize),
    pad: usize,
    /// Image size: even, as H.264 needs.
    width: usize,
    height: usize,
}

impl Painter {
    fn new(fonts: Fonts, header: &Header) -> Self {
        let cell = fonts.cell();
        let (cols, rows) = (header.term_cols as usize, header.term_rows as usize);
        let pad = cell.0;
        let even = |v: usize| v + v % 2;

        Self {
            width: even(cols * cell.0 + 2 * pad),
            height: even(rows * cell.1 + 2 * pad),
            fonts,
            theme: Theme::of(header),
            cursor: header.term_cursor.unwrap_or(CursorShape::Block),
            cols,
            rows,
            cell,
            pad,
        }
    }

    fn fill(&self, img: &mut [u8], x: usize, y: usize, w: usize, h: usize, c: Rgb) {
        for py in y..(y + h).min(self.height) {
            let row = py * self.width * 3;

            for px in x..(x + w).min(self.width) {
                img[row + px * 3..row + px * 3 + 3].copy_from_slice(&c);
            }
        }
    }

    /// The screen of `vt` (the part that fits: a resize can make it larger).
    fn draw(&mut self, vt: &Vt, img: &mut [u8]) {
        let (cw, ch, baseline) = self.cell;
        let bg = self.theme.bg;
        self.fill(img, 0, 0, self.width, self.height, bg);

        let cursor = vt.cursor();

        for (r, line) in vt.view().enumerate().take(self.rows) {
            for (c, cell) in line.cells().iter().enumerate().take(self.cols) {
                if cell.width() == 0 {
                    continue;
                }

                let pen = cell.pen();
                let mut fg = pen
                    .foreground()
                    .map_or(self.theme.fg, |c| self.theme.color(c));
                let mut cbg = pen.background().map(|c| self.theme.color(c));

                if pen.is_inverse() {
                    let f = fg;
                    fg = cbg.unwrap_or(self.theme.bg);
                    cbg = Some(f);
                }

                if pen.is_faint() {
                    fg = mix(fg, cbg.unwrap_or(self.theme.bg), 0.5);
                }

                let is_cursor = cursor.visible && cursor.row == r && cursor.col == c;
                let block = is_cursor
                    && matches!(self.cursor, CursorShape::Block | CursorShape::BlinkingBlock);

                if block {
                    // a block cursor: the cell drawn inverted
                    let f = fg;
                    fg = cbg.unwrap_or(self.theme.bg);
                    cbg = Some(f);
                }

                let x = self.pad + c * cw;
                let y = self.pad + r * ch;
                let w = cw * cell.width() as usize;

                if let Some(b) = cbg {
                    self.fill(img, x, y, w, ch, b);
                }

                let chr = cell.char();

                if chr != ' ' {
                    self.glyph(img, chr, pen.is_bold(), x, y + baseline, fg);
                }

                if pen.is_underline() {
                    self.fill(img, x, y + baseline + 2, w, 1.max(ch / 16), fg);
                }

                if pen.is_strikethrough() {
                    self.fill(img, x, y + baseline - ch / 4, w, 1.max(ch / 16), fg);
                }

                if is_cursor && !block {
                    let thick = 2.max(ch / 12);
                    let c = self.theme.fg;

                    match self.cursor {
                        CursorShape::Underline | CursorShape::BlinkingUnderline => {
                            self.fill(img, x, y + ch - thick, w, thick, c)
                        }
                        _ => self.fill(img, x, y, thick, ch, c),
                    }
                }
            }
        }
    }

    /// Draws `chr` with its baseline at `baseline`, blended over what's there.
    fn glyph(&mut self, img: &mut [u8], chr: char, bold: bool, x: usize, baseline: usize, fg: Rgb) {
        let (width, height) = (self.width, self.height);
        let (m, bitmap) = self.fonts.glyph(chr, bold);
        let (m, bitmap) = (*m, bitmap.clone());

        // without a bold font, bold is drawn twice, a pixel apart
        let passes: &[i32] = if bold && self.fonts.bold.is_none() {
            &[0, 1]
        } else {
            &[0]
        };

        let top = baseline as i32 - m.height as i32 - m.ymin;

        for &dx in passes {
            for gy in 0..m.height {
                let py = top + gy as i32;

                if py < 0 || py >= height as i32 {
                    continue;
                }

                for gx in 0..m.width {
                    let px = x as i32 + m.xmin + gx as i32 + dx;

                    if px < 0 || px >= width as i32 {
                        continue;
                    }

                    let a = bitmap[gy * m.width + gx];

                    if a == 0 {
                        continue;
                    }

                    let i = (py as usize * width + px as usize) * 3;
                    let under = [img[i], img[i + 1], img[i + 2]];
                    img[i..i + 3].copy_from_slice(&mix(fg, under, a as f32 / 255.0));
                }
            }
        }
    }
}

/// `a` over `b`, with `a`'s weight `t` (0..=1).
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |a: u8, b: u8| (a as f32 * t + b as f32 * (1.0 - t)).round() as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

/// The ffmpeg to use: `ASCIINEMA_FFMPEG`, or `ffmpeg` on the PATH.
fn ffmpeg() -> String {
    std::env::var("ASCIINEMA_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_owned())
}

/// Renders `cast` to an MP4 file at `output` (overwriting it).
pub fn render(cast: Asciicast<'_>, output: &Path, options: &Options) -> Result<()> {
    if options.fps == 0 || options.fps > 120 {
        bail!("frame rate must be 1 to 120, got {}", options.fps);
    }

    if !(options.font_size.is_finite() && (4.0..=200.0).contains(&options.font_size)) {
        bail!(
            "font size must be 4 to 200 pixels, got {}",
            options.font_size
        );
    }

    let header = cast.header;
    let events = cast.events.collect::<Result<Vec<_>>>()?;
    let mut painter = Painter::new(Fonts::load(options)?, &header);

    // event times, with long pauses shortened as players do
    let limit = header.idle_time_limit.map(Duration::from_secs_f64);
    let mut prev = Duration::ZERO;
    let mut t = Duration::ZERO;

    let timed: Vec<(Duration, &EventData)> = events
        .iter()
        .map(|e| {
            let delay = e.time.saturating_sub(prev);
            prev = e.time;
            t += limit.map_or(delay, |l| delay.min(l));
            (t, &e.data)
        })
        .collect();

    let duration = t + TAIL;
    let frames = (duration.as_secs_f64() * options.fps as f64).ceil() as u64;

    let mut child = Command::new(ffmpeg())
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24"])
        .args(["-s", &format!("{}x{}", painter.width, painter.height)])
        .args(["-r", &options.fps.to_string(), "-i", "-"])
        .args(["-c:v", "libx264", "-preset", "medium", "-crf", "18"])
        .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow!(
                "making a video needs ffmpeg, which isn't installed (e.g. sudo apt install ffmpeg)"
            ),
            _ => anyhow!("can't run ffmpeg: {e}"),
        })?;

    let mut stdin = child.stdin.take().expect("piped");
    let mut vt = Vt::builder()
        .size(header.term_cols as usize, header.term_rows as usize)
        .scrollback_limit(0)
        .build();

    let mut img = vec![0; painter.width * painter.height * 3];
    painter.draw(&vt, &mut img);
    let mut next = 0;

    let written: Result<()> = (|| {
        for f in 0..frames {
            let at = Duration::from_secs_f64(f as f64 / options.fps as f64);
            let mut changed = false;

            while next < timed.len() && timed[next].0 <= at {
                match timed[next].1 {
                    EventData::Output(s) => {
                        vt.feed_str(s);
                        changed = true;
                    }
                    EventData::Resize(c, r) => {
                        vt.resize(*c as usize, *r as usize);
                        changed = true;
                    }
                    _ => {}
                }

                next += 1;
            }

            if changed {
                painter.draw(&vt, &mut img);
            }

            stdin.write_all(&img)?;
        }

        Ok(())
    })();

    drop(stdin);
    let out = child.wait_with_output()?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("ffmpeg failed: {}", err.trim());
    }

    written.context("can't send frames to ffmpeg")
}

#[cfg(test)]
mod tests {
    use super::{find_font, Fonts, Options, Painter};
    use crate::asciicast::Header;

    fn painter(cursor: Option<crate::asciicast::CursorShape>) -> Option<Painter> {
        // needs a monospace font on the machine
        let font = find_font("monospace")?;
        let options = Options {
            font: Some(font),
            ..Default::default()
        };
        let header = Header {
            term_cols: 10,
            term_rows: 2,
            term_cursor: cursor,
            ..Default::default()
        };

        Some(Painter::new(Fonts::load(&options).unwrap(), &header))
    }

    #[test]
    fn draws_text_colours_and_the_cursor() {
        let Some(mut p) = painter(None) else { return };
        let (cw, ch, _) = p.cell;
        assert_eq!(p.width % 2, 0);
        assert_eq!(p.height % 2, 0);
        assert!(p.width >= 10 * cw && p.height >= 2 * ch);

        let mut vt = avt::Vt::builder().size(10, 2).build();
        vt.feed_str("\x1b[41mX\x1b[0m ab");
        let mut img = vec![0; p.width * p.height * 3];
        p.draw(&vt, &mut img);

        let px = |x: usize, y: usize| {
            let i = (y * p.width + x) * 3;
            [img[i], img[i + 1], img[i + 2]]
        };

        // the background, a red cell, and the block cursor (after "ab")
        assert_eq!(px(1, 1), super::DEFAULT_BG);
        assert_eq!(px(p.pad + 1, p.pad + 1), super::DEFAULT_PALETTE[1]);
        assert_eq!(px(p.pad + 4 * cw + 1, p.pad + 1), super::DEFAULT_FG);

        // text draws something in the cell of "a"
        let a = (0..cw)
            .flat_map(|x| (0..ch).map(move |y| (x, y)))
            .any(|(x, y)| px(p.pad + 2 * cw + x, p.pad + y) != super::DEFAULT_BG);
        assert!(a);
    }

    #[test]
    fn missing_ffmpeg_is_explained() {
        if find_font("monospace").is_none() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let cast = crate::asciicast::open_from_path("tests/casts/minimal-v3.cast").unwrap();

        // SAFETY: tests that read this variable run in this test only
        unsafe { std::env::set_var("ASCIINEMA_FFMPEG", "/nonexistent/ffmpeg") };
        let err = super::render(cast, &dir.path().join("x.mp4"), &Options::default())
            .unwrap_err()
            .to_string();
        unsafe { std::env::remove_var("ASCIINEMA_FFMPEG") };

        assert!(err.contains("needs ffmpeg"), "{err}");
    }
}
