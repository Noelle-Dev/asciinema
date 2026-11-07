//! Text effects, captured from ttfx or TerminalTextEffects (TTE).
//!
//! Both tools share one command-line interface, so either works. An effect is
//! captured once: the tool runs with its output piped and frame pacing off,
//! its frames are split apart and played through a virtual terminal the size
//! of the effect's canvas, and each frame becomes the cells that changed,
//! positioned where the effect belongs on the recording's screen. The result
//! is cached as a small asciicast file, so rendering never needs the tool.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use avt::Vt;
use serde::{Deserialize, Serialize};

use crate::asciicast::{Event, Header, V3Encoder};

/// Captured effect output: (delay, output) per frame.
pub type Frames = Vec<(Duration, String)>;

/// Frame rate the tools run their animations at; speed scales it.
pub const BASE_FPS: f64 = 60.0;

/// Each frame of piped tool output starts with restore-cursor, save-cursor.
const FRAME_START: &str = "\x1b8\x1b7";

/// Set this to the path of `ttfx` or `tte` to choose the tool explicitly.
pub const TOOL_ENV: &str = "ASCIINEMA_FX_TOOL";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    pub path: PathBuf,
    pub kind: ToolKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Ttfx,
    Tte,
}

impl Tool {
    pub fn name(&self) -> &'static str {
        match self.kind {
            ToolKind::Ttfx => "ttfx",
            ToolKind::Tte => "tte",
        }
    }
}

pub const INSTALL_HELP: &str = "Text effects need ttfx or tte (TerminalTextEffects) installed. \
Install one of:\n  \
ttfx: cargo install --git https://github.com/omacom-io/ttfx\n  \
tte:  pipx install terminaltexteffects\n\
or point ASCIINEMA_FX_TOOL at either program.";

/// Finds the effects tool: `$ASCIINEMA_FX_TOOL`, else `ttfx`, else `tte` on PATH.
pub fn find_tool() -> Result<Tool> {
    let kind_of = |path: &Path| {
        let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase());

        match name.as_deref() {
            Some(n) if n.starts_with("tte") => ToolKind::Tte,
            _ => ToolKind::Ttfx,
        }
    };

    if let Some(p) = std::env::var_os(TOOL_ENV).filter(|p| !p.is_empty()) {
        let path = which::which(&p).with_context(|| {
            format!(
                "{TOOL_ENV} is set to {}, which wasn't found",
                p.to_string_lossy()
            )
        })?;

        let kind = kind_of(&path);

        return Ok(Tool { path, kind });
    }

    for (name, kind) in [("ttfx", ToolKind::Ttfx), ("tte", ToolKind::Tte)] {
        if let Ok(path) = which::which(name) {
            return Ok(Tool { path, kind });
        }
    }

    bail!("{INSTALL_HELP}")
}

// ---- what to animate -------------------------------------------------------

/// The text an effect animates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FxSource {
    /// Typed text, on a cleared screen or over the recording.
    Text {
        text: String,
        /// Where the text sits in its area.
        #[serde(default, skip_serializing_if = "Anchor::is_center")]
        anchor: Anchor,
        /// The area the text is placed in; the whole screen when omitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        area: Option<Area>,
        /// Draw over the recording instead of on a cleared screen.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        over: bool,
    },
    /// A rectangle of the screen at the insert point; animated in place.
    Region {
        col: usize,
        row: usize,
        cols: usize,
        rows: usize,
    },
    /// The whole screen at the insert point; animated in place.
    Screen,
}

/// A rectangle of the screen, in cells (0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Area {
    pub col: usize,
    pub row: usize,
    pub cols: usize,
    pub rows: usize,
}

/// Where text sits within its area (the tools' `--anchor-text`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Anchor {
    Nw,
    N,
    Ne,
    W,
    #[default]
    C,
    E,
    Sw,
    S,
    Se,
}

impl Anchor {
    /// In reading order: top row, middle row, bottom row.
    pub const ALL: [Anchor; 9] = [
        Anchor::Nw,
        Anchor::N,
        Anchor::Ne,
        Anchor::W,
        Anchor::C,
        Anchor::E,
        Anchor::Sw,
        Anchor::S,
        Anchor::Se,
    ];

    pub fn flag(self) -> &'static str {
        match self {
            Anchor::Nw => "nw",
            Anchor::N => "n",
            Anchor::Ne => "ne",
            Anchor::W => "w",
            Anchor::C => "c",
            Anchor::E => "e",
            Anchor::Sw => "sw",
            Anchor::S => "s",
            Anchor::Se => "se",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Anchor::Nw => "top left",
            Anchor::N => "top",
            Anchor::Ne => "top right",
            Anchor::W => "left",
            Anchor::C => "centre",
            Anchor::E => "right",
            Anchor::Sw => "bottom left",
            Anchor::S => "bottom",
            Anchor::Se => "bottom right",
        }
    }

    pub fn from_flag(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.flag() == s)
    }

    fn is_center(&self) -> bool {
        *self == Anchor::C
    }
}

impl FxSource {
    /// Typed text, centered on a cleared screen.
    pub fn text(text: impl Into<String>) -> Self {
        FxSource::Text {
            text: text.into(),
            anchor: Anchor::C,
            area: None,
            over: false,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            FxSource::Text {
                text,
                anchor,
                area,
                over,
            } => {
                let mut s = format!("\"{}\"", text.lines().next().unwrap_or(""));

                if *anchor != Anchor::C || area.is_some() {
                    s.push_str(&format!(" {}", anchor.label()));
                }

                if area.is_some() {
                    s.push_str(" of a box");
                }

                if *over {
                    s.push_str(" over the recording");
                }

                s
            }
            FxSource::Region {
                col,
                row,
                cols,
                rows,
            } => format!("region {cols}×{rows} at {col},{row}"),
            FxSource::Screen => "whole screen".to_owned(),
        }
    }
}

/// Everything that determines an effect's frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxSpec {
    pub effect: String,
    pub source: FxSource,
    /// Extra effect options, passed to the tool after the effect name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    pub speed: f64,
    /// Fit the animation to this many seconds instead (overrides `speed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    pub seed: u64,
    /// Use the screen's own colors (for region / whole screen sources).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_colors: bool,
}

impl FxSpec {
    pub fn validate(&self) -> Result<()> {
        if self.effect.trim().is_empty() {
            bail!("no effect chosen");
        }

        if !self.speed.is_finite() || self.speed <= 0.0 {
            bail!("effect speed must be a positive number, got {}", self.speed);
        }

        if let Some(d) = self.duration {
            if !d.is_finite() || d <= 0.0 {
                bail!("effect length must be a positive number of seconds, got {d}");
            }
        }

        match &self.source {
            FxSource::Text { text, .. } if text.trim().is_empty() => {
                bail!("an effect needs some text")
            }
            FxSource::Text { area: Some(a), .. } if a.cols == 0 || a.rows == 0 => {
                bail!("the text's box is empty")
            }
            FxSource::Region { cols, rows, .. } if *cols == 0 || *rows == 0 => {
                bail!("the effect region is empty")
            }
            _ => Ok(()),
        }
    }
}

/// Where and how big the effect canvas is on a `screen_cols`x`screen_rows` screen.
struct Canvas {
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
    /// Anchor for the text inside the canvas.
    anchor_text: &'static str,
    /// Clear the screen first (typed text replaces the screen).
    clear: bool,
    /// The screen's cells under the canvas, when drawing over it: the tool's
    /// blank cells let these show through.
    background: Option<Vec<Vec<avt::Cell>>>,
}

/// Works out the effect's input text and canvas from the screen it plays over.
fn layout(spec: &FxSpec, screen: &Vt) -> Result<(String, Canvas)> {
    let (cols, rows) = screen.size();

    match &spec.source {
        FxSource::Text {
            text,
            anchor,
            area,
            over,
        } => {
            let a = area.unwrap_or(Area {
                col: 0,
                row: 0,
                cols,
                rows,
            });

            if a.col + a.cols > cols || a.row + a.rows > rows {
                bail!(
                    "the text's box {}×{} at {},{} doesn't fit the {cols}×{rows} screen",
                    a.cols,
                    a.rows,
                    a.col,
                    a.row
                );
            }

            let background = over.then(|| {
                screen
                    .view()
                    .skip(a.row)
                    .take(a.rows)
                    .map(|l| l.cells().iter().skip(a.col).take(a.cols).copied().collect())
                    .collect()
            });

            Ok((
                text.clone(),
                Canvas {
                    col: a.col,
                    row: a.row,
                    cols: a.cols,
                    rows: a.rows,
                    anchor_text: anchor.flag(),
                    clear: !over,
                    background,
                },
            ))
        }

        FxSource::Screen => Ok((
            screen_text(screen, 0, 0, cols, rows, spec.keep_colors),
            Canvas {
                col: 0,
                row: 0,
                cols,
                rows,
                anchor_text: "nw",
                clear: false,
                background: None,
            },
        )),

        FxSource::Region {
            col,
            row,
            cols: w,
            rows: h,
        } => {
            if col + w > cols || row + h > rows {
                bail!(
                    "the effect region {w}×{h} at {col},{row} doesn't fit the {cols}×{rows} screen"
                );
            }

            Ok((
                screen_text(screen, *col, *row, *w, *h, spec.keep_colors),
                Canvas {
                    col: *col,
                    row: *row,
                    cols: *w,
                    rows: *h,
                    anchor_text: "nw",
                    clear: false,
                    background: None,
                },
            ))
        }
    }
}

/// Text of a screen rectangle, trailing spaces trimmed, optionally with its
/// colors as SGR sequences.
fn screen_text(vt: &Vt, col: usize, row: usize, cols: usize, rows: usize, colors: bool) -> String {
    let mut lines: Vec<String> = Vec::new();

    for line in vt.view().skip(row).take(rows) {
        let cells = line.cells();
        let mut out = String::new();
        let mut pen: Option<avt::Pen> = None;

        for c in cells.iter().skip(col).take(cols) {
            if c.width() == 0 {
                continue;
            }

            if colors && pen.as_ref() != Some(c.pen()) {
                out.push_str(&sgr(c.pen()));
                pen = Some(*c.pen());
            }

            out.push(c.char());
        }

        if colors {
            out.push_str("\x1b[0m");
        }

        lines.push(trim_end_keeping_sgr(&out));
    }

    while lines.last().is_some_and(|l| strip_sgr(l).trim().is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

fn strip_sgr(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }

    out
}

/// Trims trailing spaces, keeping a final reset sequence if there was one.
fn trim_end_keeping_sgr(s: &str) -> String {
    match s.strip_suffix("\x1b[0m") {
        Some(body) => format!("{}\x1b[0m", body.trim_end()),
        None => s.trim_end().to_owned(),
    }
}

// ---- running the tool ------------------------------------------------------

fn tool_args(spec: &FxSpec, canvas: &Canvas, input: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "--input-file",
        &input.to_string_lossy(),
        "--frame-rate",
        "0",
        "--canvas-width",
        &canvas.cols.to_string(),
        "--canvas-height",
        &canvas.rows.to_string(),
        "--anchor-canvas",
        "nw",
        "--anchor-text",
        canvas.anchor_text,
        "--ignore-terminal-dimensions",
        "--no-eol",
        "--seed",
        &spec.seed.to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    if spec.keep_colors {
        args.extend(["--existing-color-handling".to_owned(), "dynamic".to_owned()]);
    }

    args.push(spec.effect.clone());
    args.extend(spec.args.iter().cloned());

    args
}

fn run_tool(tool: &Tool, args: &[String]) -> Result<String> {
    let output = Command::new(&tool.path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("can't run {}", tool.path.display()))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let err = err.trim();
        bail!(
            "{} failed: {}",
            tool.name(),
            if err.is_empty() {
                "no error message"
            } else {
                err
            }
        );
    }

    Ok(String::from_utf8(output.stdout)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// A captured effect: its frames, and how long it runs at normal (1×) speed.
pub struct Captured {
    pub frames: Frames,
    pub natural: Duration,
}

/// Captures the effect as output events on the recording's screen. `screen`
/// is the screen it plays over.
pub fn capture(tool: &Tool, spec: &FxSpec, screen: &Vt) -> Result<Captured> {
    spec.validate()?;

    let (text, canvas) = layout(spec, screen)?;

    if strip_sgr(&text).trim().is_empty() {
        bail!("there's no text there to animate");
    }

    let mut input = tempfile::NamedTempFile::new()?;
    input.write_all(text.as_bytes())?;
    input.flush()?;

    let raw = run_tool(tool, &tool_args(spec, &canvas, input.path()))?;

    // frames are evenly spaced, so a target length fixes the frame time exactly
    let frames = raw.matches(FRAME_START).count();
    let intervals = frames.saturating_sub(1).max(1) as f64;
    let natural = Duration::from_secs_f64(intervals / BASE_FPS);

    let frame_time = match spec.duration {
        Some(d) => Duration::from_secs_f64(d / intervals),
        None => Duration::from_secs_f64(1.0 / (BASE_FPS * spec.speed)),
    };

    Ok(Captured {
        frames: composite(&raw, &canvas, frame_time),
        natural,
    })
}

/// Turns the tool's frames into output events drawing the changed cells at
/// the canvas position. The first frame paints the whole canvas.
fn composite(raw: &str, canvas: &Canvas, frame_time: Duration) -> Frames {
    // The tool writes as if to a terminal, first printing blank rows for its
    // canvas; one spare row keeps that from scrolling the canvas. Piped, it
    // uses bare \n for new lines, so newline mode (LNM) makes \n return to
    // the line start too - cheaper than rewriting the (large) output.
    let mut vt = Vt::builder()
        .size(canvas.cols, canvas.rows + 1)
        .scrollback_limit(0)
        .build();

    vt.feed_str("\x1b[20h");

    let mut chunks = raw.split(FRAME_START);
    let setup = chunks.next().unwrap_or("");
    vt.feed_str(setup);

    let mut events = Vec::new();
    let mut prev: Option<Vec<Vec<avt::Cell>>> = None;
    let mut pending = Duration::ZERO;

    for chunk in chunks {
        vt.feed_str(FRAME_START);
        vt.feed_str(chunk);

        let mut grid = canvas_cells(&vt, canvas.rows);

        // over the recording: blank cells show what's underneath
        if let Some(bg) = &canvas.background {
            for (line, under) in grid.iter_mut().zip(bg) {
                for (cell, below) in line.iter_mut().zip(under) {
                    if is_blank(cell) {
                        *cell = *below;
                    }
                }
            }
        }

        let mut out = cell_diff(prev.as_deref(), &grid, canvas.col, canvas.row);

        if prev.is_none() {
            // hide the cursor for the effect; typed text replaces the screen
            let clear = if canvas.clear { "\x1b[H\x1b[2J" } else { "" };
            out = format!("\x1b[?25l{clear}{out}");
        }

        if !out.is_empty() {
            events.push((pending, out));
            pending = Duration::ZERO;
        }

        pending += frame_time;
        prev = Some(grid);
    }

    events
}

/// A cell the tool left empty: a space with no colors of its own.
fn is_blank(cell: &avt::Cell) -> bool {
    let pen = cell.pen();

    cell.char() == ' ' && pen.background().is_none() && !pen.is_inverse()
}

/// The canvas rows of the virtual terminal (the spare row is dropped).
fn canvas_cells(vt: &Vt, rows: usize) -> Vec<Vec<avt::Cell>> {
    vt.view().take(rows).map(|l| l.cells().to_vec()).collect()
}

/// Escape sequences drawing the cells of `next` that differ from `prev`
/// (all of them when there's no `prev`), offset by (`col`, `row`).
fn cell_diff(
    prev: Option<&[Vec<avt::Cell>]>,
    next: &[Vec<avt::Cell>],
    col: usize,
    row: usize,
) -> String {
    let mut out = String::new();
    let mut pen: Option<avt::Pen> = None;

    for (y, line) in next.iter().enumerate() {
        // where the terminal cursor will be after the last write, if on this row
        let mut cursor: Option<usize> = None;

        for (x, cell) in line.iter().enumerate() {
            if cell.width() == 0 {
                continue; // tail of a wide character
            }

            let same = prev
                .and_then(|p| p.get(y))
                .and_then(|l| l.get(x))
                .is_some_and(|c| c == cell);

            if same {
                continue;
            }

            if cursor != Some(x) {
                let _ = write!(out, "\x1b[{};{}H", row + y + 1, col + x + 1);
            }

            if pen.as_ref() != Some(cell.pen()) {
                out.push_str(&sgr(cell.pen()));
                pen = Some(*cell.pen());
            }

            out.push(cell.char());
            cursor = Some(x + cell.width().max(1) as usize);
        }
    }

    if pen.is_some() {
        out.push_str("\x1b[0m");
    }

    out
}

/// An SGR sequence fully setting `pen` (starting from a reset).
pub(crate) fn sgr(pen: &avt::Pen) -> String {
    let mut s = String::from("\x1b[0");

    for (on, code) in [
        (pen.is_bold(), "1"),
        (pen.is_faint(), "2"),
        (pen.is_italic(), "3"),
        (pen.is_underline(), "4"),
        (pen.is_blink(), "5"),
        (pen.is_inverse(), "7"),
        (pen.is_strikethrough(), "9"),
    ] {
        if on {
            s.push(';');
            s.push_str(code);
        }
    }

    let color = |c: avt::Color, base: u8| -> String {
        match c {
            avt::Color::Indexed(n) if n < 8 => format!(";{}", base + n),
            avt::Color::Indexed(n) if n < 16 => format!(";{}", base + 60 + n - 8),
            avt::Color::Indexed(n) => format!(";{};5;{n}", base + 8),
            avt::Color::RGB(c) => format!(";{};2;{};{};{}", base + 8, c.r, c.g, c.b),
        }
    };

    if let Some(c) = pen.foreground() {
        s.push_str(&color(c, 30));
    }

    if let Some(c) = pen.background() {
        s.push_str(&color(c, 40));
    }

    s.push('m');

    s
}

// ---- cache -----------------------------------------------------------------

/// Directory effect captures are cached in: `demo.cast` -> `demo.cast.effects/`.
pub fn cache_dir(recording: &Path) -> PathBuf {
    let mut name = recording.as_os_str().to_owned();
    name.push(".effects");

    PathBuf::from(name)
}

/// Captures `spec` playing over `screen` and caches it next to `recording`.
/// Returns the cache file path, relative to the recording's directory.
/// An identical capture already in the cache is reused.
pub fn capture_to_cache(
    tool: &Tool,
    spec: &FxSpec,
    screen: &Vt,
    recording: &Path,
) -> Result<String> {
    let dir = cache_dir(recording);

    // name by everything that affects the frames
    let key = format!(
        "{}\n{}\n{:?}",
        serde_json::to_string(spec)?,
        screen.dump(),
        screen.size()
    );
    let name = format!(
        "{}-{:016x}.cast",
        spec.effect,
        crate::hash::fnv1a_128(key) as u64
    );
    let path = dir.join(&name);

    if !path.exists() {
        let events = capture(tool, spec, screen)?.frames;
        std::fs::create_dir_all(&dir).with_context(|| format!("can't create {}", dir.display()))?;
        write_cast(&path, screen.size(), &events)?;
    }

    let rel = Path::new(dir.file_name().expect("cache dir has a name")).join(name);

    Ok(rel.to_string_lossy().into_owned())
}

fn write_cast(
    path: &Path,
    (cols, rows): (usize, usize),
    events: &[(Duration, String)],
) -> Result<()> {
    let header = Header {
        term_cols: cols as u16,
        term_rows: rows as u16,
        ..Default::default()
    };

    let mut enc = V3Encoder::new();
    let mut data = enc.header(&header);
    let mut t = Duration::ZERO;

    for (delay, out) in events {
        t += *delay;

        // asciicast v3 stores millisecond intervals; rounding the absolute
        // times (not each interval) keeps the total from drifting
        let ms = Duration::from_millis((t.as_secs_f64() * 1000.0).round() as u64);
        data.extend(enc.event(&Event::output(ms, out.clone())));
    }

    std::fs::write(path, data).with_context(|| format!("can't write {}", path.display()))
}

// ---- effect list and options (from the tool's help) -------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct EffectInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OptionInfo {
    /// e.g. `--typing-speed`
    pub flag: String,
    /// Whether it takes values (otherwise it's a switch).
    pub takes_value: bool,
    pub description: String,
}

pub fn list_effects(tool: &Tool) -> Result<Vec<EffectInfo>> {
    let help = run_help(tool, &[])?;

    Ok(match tool.kind {
        ToolKind::Ttfx => parse_ttfx_effects(&help),
        ToolKind::Tte => parse_tte_effects(&help),
    })
}

pub fn effect_options(tool: &Tool, effect: &str) -> Result<Vec<OptionInfo>> {
    Ok(parse_options(&run_help(tool, &[effect])?))
}

fn run_help(tool: &Tool, args: &[&str]) -> Result<String> {
    let output = Command::new(&tool.path)
        .args(args)
        .arg("--help")
        .env("COLUMNS", "200")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("can't run {}", tool.path.display()))?;

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// ttfx (clap): a `Commands:` section of `  name   description` lines.
fn parse_ttfx_effects(help: &str) -> Vec<EffectInfo> {
    help.lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let l = l.trim();
            let (name, desc) = l.split_once(char::is_whitespace)?;

            (name != "help").then(|| EffectInfo {
                name: name.to_owned(),
                description: desc.trim().to_owned(),
            })
        })
        .collect()
}

/// tte (argparse): after `Available Effects`, `    name   description` lines,
/// with wrapped descriptions continued on more deeply indented lines.
fn parse_tte_effects(help: &str) -> Vec<EffectInfo> {
    let mut effects: Vec<EffectInfo> = Vec::new();

    for line in help
        .lines()
        .skip_while(|l| !l.contains("Available Effects"))
        .skip(1)
    {
        if line.trim().is_empty() {
            break;
        }

        let indent = line.len() - line.trim_start().len();
        let text = line.trim();

        if indent <= 6 {
            if let Some((name, desc)) = text.split_once(char::is_whitespace) {
                effects.push(EffectInfo {
                    name: name.to_owned(),
                    description: desc.trim().to_owned(),
                });
            }
        } else if let Some(last) = effects.last_mut() {
            last.description.push(' ');
            last.description.push_str(text);
        }
    }

    effects
}

/// Options from an effect's help, in either tool's format: an option line
/// starts with `--flag`, followed (on the same line or the next ones) by its
/// description.
fn parse_options(help: &str) -> Vec<OptionInfo> {
    let mut options: Vec<OptionInfo> = Vec::new();

    for line in help.lines() {
        let text = line.trim_start();

        if text.starts_with("--") || text.starts_with("-h, --help") {
            let flag_end = text.find(|c: char| c.is_whitespace()).unwrap_or(text.len());
            let flag = text[..flag_end].trim_end_matches(',').to_owned();
            let rest = text[flag_end..].trim();

            // value hint: <VALUE> (clap) or (type) / {a,b} (argparse)
            let takes_value =
                rest.starts_with('<') || rest.starts_with('(') || rest.starts_with('{');

            // argparse puts a short description after two spaces on the same line
            let description = rest
                .split_once("  ")
                .map(|(_, d)| d.trim().to_owned())
                .filter(|_| !rest.starts_with('<'))
                .unwrap_or_default();

            options.push(OptionInfo {
                flag,
                takes_value,
                description,
            });
        } else if let Some(last) = options.last_mut() {
            let text = text.trim();

            if text.is_empty() || line.starts_with(|c: char| !c.is_whitespace()) {
                continue;
            }

            if !last.description.is_empty() {
                last.description.push(' ');
            }

            last.description.push_str(text);
        }
    }

    options.retain(|o| o.flag != "--help" && o.flag != "-h");

    options
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use avt::Vt;

    use super::*;

    /// One frame at 60fps.
    const FT60: Duration = Duration::from_nanos(16_666_667);

    fn vt_with(cols: usize, rows: usize, text: &str) -> Vt {
        let mut vt = Vt::builder().size(cols, rows).scrollback_limit(0).build();
        vt.feed_str(text);

        vt
    }

    fn screen_after(base: &Vt, events: &[(Duration, String)]) -> Vec<String> {
        let (c, r) = base.size();
        let mut vt = vt_with(c, r, &base.dump());

        for (_, e) in events {
            vt.feed_str(e);
        }

        vt.text().iter().map(|l| l.trim_end().to_owned()).collect()
    }

    /// Frames the way the tools write them when piped: a blank row per canvas
    /// row (then a saved cursor on the row below), and each frame restoring
    /// that cursor, moving up and redrawing every row.
    fn fake_tool_output(cols: usize, frames: &[&[&str]]) -> String {
        let rows = frames[0].len();
        let mut out = String::from("\x1b[?25l");

        for _ in 0..rows {
            out.push_str(&format!("{}\n", " ".repeat(cols)));
        }

        out.push_str("\x1b7");

        for frame in frames {
            out.push_str(&format!("{FRAME_START}\x1b[{rows}A"));
            let lines: Vec<String> = frame.iter().map(|l| format!("{l:<cols$}")).collect();
            out.push_str(&lines.join("\n"));
        }

        out.push_str("\x1b[?25h");
        out
    }

    fn canvas(col: usize, row: usize, cols: usize, rows: usize) -> Canvas {
        Canvas {
            col,
            row,
            cols,
            rows,
            anchor_text: "nw",
            clear: false,
            background: None,
        }
    }

    #[test]
    fn composites_frames_into_place_and_diffs() {
        let raw = fake_tool_output(6, &[&["xx", "  yy"], &["ab", "  yy"], &["ab", "  cd"]]);
        let events = composite(&raw, &canvas(3, 1, 6, 2), FT60);

        // one event per changed frame, paced at 60fps
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, Duration::ZERO);
        assert!((events[1].0.as_secs_f64() - 1.0 / 60.0).abs() < 1e-9);

        let base = vt_with(12, 4, "background\r\nline two\r\nthree\r\nfour");
        let screen = screen_after(&base, &events);

        // the canvas lands at col 3, row 1; everything else is untouched
        assert_eq!(screen[0], "background");
        assert_eq!(screen[1], "linab");
        assert_eq!(screen[2].trim_end(), "thr  cd");
        assert_eq!(screen[3], "four");

        // later frames only redraw what changed
        assert!(!events[2].1.contains('a'), "{:?}", events[2].1);
    }

    #[test]
    fn unchanged_frames_add_to_the_next_delay() {
        let raw = fake_tool_output(4, &[&["a"], &["a"], &["a"], &["b"]]);
        let events = composite(&raw, &canvas(0, 0, 4, 1), FT60 / 2);

        assert_eq!(events.len(), 2);
        // 3 frames at 120fps
        assert!((events[1].0.as_secs_f64() - 3.0 / 120.0).abs() < 1e-6);
    }

    #[test]
    fn typed_text_clears_the_screen_first() {
        let raw = fake_tool_output(4, &[&["hi"]]);
        let mut c = canvas(0, 0, 4, 1);
        c.clear = true;

        let events = composite(&raw, &c, FT60);
        assert!(events[0].1.starts_with("\x1b[?25l\x1b[H\x1b[2J"));
    }

    #[test]
    fn over_the_recording_blank_cells_show_through() {
        let raw = fake_tool_output(6, &[&["  hi  "], &["      "]]);
        let base = vt_with(10, 2, "abcdefghij\r\nsecond");
        let spec = FxSpec {
            effect: "x".to_owned(),
            source: FxSource::Text {
                text: "hi".to_owned(),
                anchor: Anchor::C,
                area: Some(Area {
                    col: 2,
                    row: 0,
                    cols: 6,
                    rows: 1,
                }),
                over: true,
            },
            args: vec![],
            speed: 1.0,
            duration: None,
            seed: 1,
            keep_colors: false,
        };

        let (_, c) = layout(&spec, &base).unwrap();
        assert!(!c.clear, "over the recording: no clearing");
        assert_eq!((c.col, c.row, c.cols, c.rows), (2, 0, 6, 1));

        let events = composite(&raw, &c, FT60);

        // first frame: the text, with the recording around it
        let first = screen_after(&base, &events[..1]);
        assert_eq!(first[0], "abcdhighij");
        assert_eq!(first[1], "second");

        // the text going away reveals the recording again
        let after = screen_after(&base, &events);
        assert_eq!(after[0], "abcdefghij");
    }

    #[test]
    fn typed_text_position_and_box() {
        let base = vt_with(20, 6, "");
        let mut spec = FxSpec {
            effect: "x".to_owned(),
            source: FxSource::text("hi"),
            args: vec![],
            speed: 1.0,
            duration: None,
            seed: 1,
            keep_colors: false,
        };

        let (_, c) = layout(&spec, &base).unwrap();
        assert_eq!((c.cols, c.rows, c.anchor_text, c.clear), (20, 6, "c", true));

        spec.source = FxSource::Text {
            text: "hi".to_owned(),
            anchor: Anchor::Se,
            area: Some(Area {
                col: 4,
                row: 1,
                cols: 10,
                rows: 3,
            }),
            over: false,
        };
        let (_, c) = layout(&spec, &base).unwrap();
        assert_eq!(
            (c.col, c.row, c.cols, c.rows, c.anchor_text),
            (4, 1, 10, 3, "se")
        );
        assert!(c.background.is_none());

        // a box off the screen is rejected
        spec.source = FxSource::Text {
            text: "hi".to_owned(),
            anchor: Anchor::C,
            area: Some(Area {
                col: 15,
                row: 0,
                cols: 10,
                rows: 1,
            }),
            over: false,
        };
        assert!(layout(&spec, &base).is_err());

        // older edit files (just the text) still load, centered and clearing
        let old: FxSource = serde_json::from_str(r#"{"kind":"text","text":"hi"}"#).unwrap();
        assert_eq!(old, FxSource::text("hi"));
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"{"kind":"text","text":"hi"}"#
        );
    }

    #[test]
    fn colors_survive_compositing() {
        let raw = fake_tool_output(4, &[&["\x1b[38;2;10;20;30mok\x1b[0m"]]);
        let events = composite(&raw, &canvas(0, 0, 4, 1), FT60);

        assert!(
            events[0].1.contains("\x1b[0;38;2;10;20;30mo"),
            "{:?}",
            events[0].1
        );
    }

    #[test]
    fn region_text_and_layout() {
        let vt = vt_with(20, 4, "\x1b[31mred\x1b[0m and plain\r\n  indented\r\n\r\n");
        let region = FxSpec {
            effect: "decrypt".to_owned(),
            source: FxSource::Region {
                col: 0,
                row: 0,
                cols: 12,
                rows: 3,
            },
            args: vec![],
            speed: 1.0,
            duration: None,
            seed: 1,
            keep_colors: false,
        };

        let (text, c) = layout(&region, &vt).unwrap();
        assert_eq!(text, "red and plai\n  indented");
        assert_eq!(
            (c.col, c.row, c.cols, c.rows, c.anchor_text),
            (0, 0, 12, 3, "nw")
        );

        let colored = FxSpec {
            keep_colors: true,
            ..region.clone()
        };
        let (text, _) = layout(&colored, &vt).unwrap();
        assert!(text.starts_with("\x1b[0;31mred\x1b[0m and"), "{text:?}");

        let too_big = FxSpec {
            source: FxSource::Region {
                col: 15,
                row: 0,
                cols: 10,
                rows: 1,
            },
            ..region
        };
        assert!(layout(&too_big, &vt).is_err());
    }

    #[test]
    fn tool_arguments() {
        let spec = FxSpec {
            effect: "rain".to_owned(),
            source: FxSource::Screen,
            args: vec!["--rain-time".to_owned(), "5".to_owned()],
            speed: 1.0,
            duration: None,
            seed: 42,
            keep_colors: true,
        };

        let args = tool_args(&spec, &canvas(0, 0, 80, 24), Path::new("/tmp/in"));
        let joined = args.join(" ");

        assert!(joined.contains("--frame-rate 0 --canvas-width 80 --canvas-height 24"));
        assert!(joined.contains(
            "--anchor-canvas nw --anchor-text nw --ignore-terminal-dimensions --no-eol --seed 42"
        ));
        assert!(joined.contains("--existing-color-handling dynamic"));
        assert!(joined.ends_with("rain --rain-time 5"));
    }

    #[test]
    fn parses_both_tools_help() {
        let ttfx = "Usage: ttfx [OPTIONS] [COMMAND]\n\nCommands:\n  beams            Create beams\n  decrypt          Display a movie style decryption effect\n  help             Print this message\n\nOptions:\n";
        let effects = parse_ttfx_effects(ttfx);
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[1].name, "decrypt");

        let tte = "Effect:\n  {beams,decrypt}\n                        Available Effects\n    beams               Create beams which travel over the canvas illuminating the\n                        characters behind them.\n    decrypt             Display a movie style decryption effect.\n\nEx: ls\n";
        let effects = parse_tte_effects(tte);
        assert_eq!(effects.len(), 2);
        assert!(effects[0]
            .description
            .ends_with("the characters behind them."));

        let ttfx_opts = "Options:\n      --typing-speed <TYPING_SPEED>\n          Number of characters typed per keystroke [default: 2]\n      --flag-only\n          A switch\n  -h, --help\n          Print help\n";
        let opts = parse_options(ttfx_opts);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].flag, "--typing-speed");
        assert!(opts[0].takes_value);
        assert!(opts[0].description.contains("[default: 2]"));
        assert!(!opts[1].takes_value);

        let tte_opts = "options:\n  -h, --help            show this help message and exit\n  --typing-speed (int > 0)\n                        Number of characters typed per keystroke. (default: 2)\n  --final-gradient-direction (diagonal, horizontal, vertical, radial)\n                        Direction of the final gradient.\n";
        let opts = parse_options(tte_opts);
        assert_eq!(opts.len(), 2);
        assert!(opts[0].takes_value && opts[1].takes_value);
        assert!(opts[1].description.starts_with("Direction"));
    }

    /// Runs against a real ttfx/tte when one is available.
    #[test]
    fn real_tool_reproduces_the_region_text() {
        let Ok(tool) = find_tool() else {
            eprintln!("skipping: no ttfx/tte found");
            return;
        };

        let vt = vt_with(30, 6, "$ make test\r\nbuild ok\r\n42 tests passed\r\n$ ");
        let spec = FxSpec {
            effect: "decrypt".to_owned(),
            source: FxSource::Region {
                col: 0,
                row: 1,
                cols: 20,
                rows: 2,
            },
            args: vec![],
            speed: 1.0,
            duration: None,
            seed: 7,
            keep_colors: false,
        };

        let captured = capture(&tool, &spec, &vt).unwrap();
        let events = captured.frames;
        let screen = screen_after(&vt, &events);

        // decrypt ends on the real text, in place; the rest is untouched
        assert_eq!(screen[0], "$ make test");
        assert_eq!(screen[1], "build ok");
        assert_eq!(screen[2], "42 tests passed");
        assert_eq!(screen[3], "$");
        assert!(events.len() > 10, "an animation, not a single frame");

        // a target length is hit (to within the last frame's rounding)
        let fitted = FxSpec {
            duration: Some(2.0),
            ..spec
        };
        let total: f64 = capture(&tool, &fitted, &vt)
            .unwrap()
            .frames
            .iter()
            .map(|(d, _)| d.as_secs_f64())
            .sum();
        assert!((total - 2.0).abs() < 0.05, "{total}");
        assert!(captured.natural.as_secs_f64() > 1.0);
    }
}
