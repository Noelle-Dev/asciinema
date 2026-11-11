//! Compositing tracks over a recording: laying track screens on top of the
//! main screen, and drawing the result with as little output as possible.

use avt::Vt;

use super::edits::{Offset, Rect};
use super::style::TextStyle;

/// A screen cell: character, style, width (2 for the first half of a wide
/// character, 0 for its second half).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: TextStyle,
    pub width: u8,
}

const BLANK: Cell = Cell {
    ch: ' ',
    style: TextStyle {
        fg: None,
        bg: None,
        bold: false,
        faint: false,
        italic: false,
        underline: false,
        blink: false,
        inverse: false,
        strike: false,
    },
    width: 1,
};

/// A track, placed on the main screen.
pub struct Layer<'a> {
    pub content: Content<'a>,
    pub crop: Option<Rect>,
    pub offset: Offset,
}

/// What a track shows.
#[derive(Clone, Copy)]
pub enum Content<'a> {
    /// A screen: its whitespace is see-through, anything else covers.
    Screen(&'a Vt),
    /// A `cols`x`rows` area restyling what's under it.
    Style {
        cols: usize,
        rows: usize,
        style: TextStyle,
    },
}

/// `style` with a highlight's on top: its colours replace the cell's, and
/// what it turns on (bold, underline...) is turned on.
pub fn restyle(style: TextStyle, with: &TextStyle) -> TextStyle {
    TextStyle {
        fg: with.fg.or(style.fg),
        bg: with.bg.or(style.bg),
        bold: style.bold || with.bold,
        faint: style.faint || with.faint,
        italic: style.italic || with.italic,
        underline: style.underline || with.underline,
        blink: style.blink || with.blink,
        inverse: style.inverse != with.inverse,
        strike: style.strike || with.strike,
    }
}

/// The composited screen, and which of its cells a track covers.
pub struct Screen {
    pub cells: Vec<Vec<Cell>>,
    pub covered: Vec<Vec<bool>>,
}

fn cells_of(vt: &Vt) -> Vec<Vec<Cell>> {
    let (cols, _) = vt.size();

    vt.view()
        .map(|line| {
            let mut row: Vec<Cell> = line
                .cells()
                .iter()
                .map(|c| Cell {
                    ch: c.char(),
                    style: TextStyle::from_pen(c.pen()),
                    width: c.width(),
                })
                .collect();

            row.resize(cols, BLANK);
            row
        })
        .collect()
}

/// `main`'s screen with `layers` on top, in order (later ones on top). A
/// track's whitespace is see-through; anything else covers what's below.
/// Whatever a layer's offset puts outside the main screen is cut off.
pub fn compose(main: &Vt, layers: &[Layer]) -> Screen {
    let mut cells = cells_of(main);
    let (cols, rows) = main.size();
    let mut covered = vec![vec![false; cols]; rows];

    for layer in layers {
        let vt = match layer.content {
            Content::Screen(vt) => vt,

            Content::Style {
                cols: w,
                rows: h,
                style,
            } => {
                let crop = layer.crop.unwrap_or(Rect {
                    row: 0,
                    col: 0,
                    rows: h,
                    cols: w,
                });

                for sr in crop.row..(crop.row + crop.rows).min(h) {
                    for sc in crop.col..(crop.col + crop.cols).min(w) {
                        let r = layer.offset.row + (sr - crop.row) as isize;
                        let c = layer.offset.col + (sc - crop.col) as isize;

                        if (0..rows as isize).contains(&r) && (0..cols as isize).contains(&c) {
                            let cell = &mut cells[r as usize][c as usize];
                            cell.style = restyle(cell.style, &style);
                        }
                    }
                }

                continue;
            }
        };

        let src = cells_of(vt);
        let (src_cols, src_rows) = vt.size();

        let crop = layer.crop.unwrap_or(Rect {
            row: 0,
            col: 0,
            rows: src_rows,
            cols: src_cols,
        });

        let row_end = (crop.row + crop.rows).min(src_rows);
        let col_end = (crop.col + crop.cols).min(src_cols);

        for (sr, src_row) in src.iter().enumerate().take(row_end).skip(crop.row) {
            let r = layer.offset.row + (sr - crop.row) as isize;

            if r < 0 || r >= rows as isize {
                continue;
            }

            let r = r as usize;

            for (sc, &src_cell) in src_row.iter().enumerate().take(col_end).skip(crop.col) {
                let c = layer.offset.col + (sc - crop.col) as isize;

                if c < 0 || c >= cols as isize {
                    continue;
                }

                let c = c as usize;
                let mut cell = src_cell;

                match cell.width {
                    // the second half of a wide character goes with its first
                    0 => continue,

                    // only half of it is shown
                    2 if sc + 1 >= col_end || c + 1 >= cols => cell = Cell { ch: ' ', ..cell },

                    _ => {}
                }

                if cell.ch.is_whitespace() {
                    continue;
                }

                put(&mut cells[r], c, cell);
                covered[r][c] = true;

                if cell.width == 2 {
                    put(
                        &mut cells[r],
                        c + 1,
                        Cell {
                            ch: ' ',
                            style: cell.style,
                            width: 0,
                        },
                    );
                    covered[r][c + 1] = true;
                }
            }
        }
    }

    Screen { cells, covered }
}

/// Sets `row[c]`, blanking what's left of a wide character it splits.
fn put(row: &mut [Cell], c: usize, cell: Cell) {
    if row[c].width == 0 && c > 0 && cell.width != 0 {
        row[c - 1] = Cell {
            ch: ' ',
            width: 1,
            ..row[c - 1]
        };
    }

    if row[c].width == 2 && cell.width != 2 {
        if let Some(next) = row.get_mut(c + 1) {
            *next = Cell {
                ch: ' ',
                width: 1,
                ..*next
            };
        }
    }

    row[c] = cell;
}

/// What the output terminal is showing while tracks are drawn: lets each
/// change be drawn as just the cells that differ.
pub struct Painter {
    shown: Vec<Vec<Cell>>,
    pen: TextStyle,
    /// Where the terminal's cursor is, when known.
    at: Option<(usize, usize)>,
    visible: bool,
}

impl Painter {
    /// The terminal after `start()`'s output: a cleared `cols`x`rows` screen.
    pub fn new((cols, rows): (usize, usize)) -> Self {
        Self {
            shown: vec![vec![BLANK; cols]; rows],
            pen: TextStyle::default(),
            at: Some((0, 0)),
            visible: true,
        }
    }

    fn goto(&mut self, r: usize, c: usize, out: &mut String) {
        if self.at != Some((r, c)) {
            out.push_str(&format!("\x1b[{};{}H", r + 1, c + 1));
            self.at = Some((r, c));
        }
    }

    /// Erasing and scrolling fill with the current background: back to the
    /// default first.
    fn reset_pen(&mut self, out: &mut String) {
        if self.pen != TextStyle::default() {
            out.push_str("\x1b[0m");
            self.pen = TextStyle::default();
        }
    }

    /// When `cells` is what's shown moved up by some lines (as when output
    /// scrolls the screen), scrolls the terminal rather than redrawing
    /// every line.
    fn scroll(&mut self, cells: &[Vec<Cell>], out: &mut String) {
        let rows = self.shown.len();

        if cells.len() != rows || rows < 2 {
            return;
        }

        // how many non-blank lines would be in place after moving up by k
        // (k = 0: as it is); scroll when that's more lines than now
        let in_place = |k: usize| {
            (0..rows - k)
                .filter(|&r| {
                    self.shown[r + k] == cells[r] && self.shown[r + k].iter().any(|c| *c != BLANK)
                })
                .count()
        };

        let now = in_place(0);
        let Some((k, best)) = (1..rows)
            .map(|k| (k, in_place(k)))
            .max_by_key(|&(k, n)| (n, std::cmp::Reverse(k)))
        else {
            return;
        };

        if best <= now {
            return;
        }

        // from the bottom line, k new lines scroll it up (the scroll region
        // is the whole screen: the terminal was reset by `start`)
        self.reset_pen(out);
        out.push_str(&format!("\x1b[{rows};1H"));
        out.push_str(&"\n".repeat(k));
        self.at = Some((rows - 1, 0));

        self.shown.drain(..k);
        let cols = cells[0].len();
        self.shown.extend((0..k).map(|_| vec![BLANK; cols]));
    }

    /// Output putting the terminal in the state [`Painter::new`] expects:
    /// primary screen, default modes, cleared, cursor home and visible.
    pub fn start() -> &'static str {
        super::timeline::REDRAW_PREFIX
    }

    /// Output changing the terminal from what it shows to `screen`, with the
    /// cursor where `main`'s is (hidden when a track covers it).
    pub fn paint(&mut self, screen: &Screen, main: &Vt) -> String {
        let mut out = String::new();
        self.scroll(&screen.cells, &mut out);

        for (r, row) in screen.cells.iter().enumerate() {
            let cols = row.len();
            let mut c = 0;

            while c < cols {
                let cell = row[c];

                // the rest of the line is blank: erase it in one go
                if row[c..].iter().all(|x| *x == BLANK) {
                    if self.shown[r][c..].iter().any(|x| *x != BLANK) {
                        self.goto(r, c, &mut out);
                        self.reset_pen(&mut out);
                        out.push_str("\x1b[K");

                        for x in &mut self.shown[r][c..] {
                            *x = BLANK;
                        }
                    }

                    break;
                }

                let differs = cell != self.shown[r][c]
                    || (cell.width == 2 && row.get(c + 1) != self.shown[r].get(c + 1));

                if cell.width == 0 || !differs {
                    c += 1;
                    continue;
                }

                if self.at != Some((r, c)) {
                    out.push_str(&format!("\x1b[{};{}H", r + 1, c + 1));
                }

                if self.pen != cell.style {
                    out.push_str(&cell.style.sgr());
                    self.pen = cell.style;
                }

                out.push(cell.ch);
                self.shown[r][c] = cell;

                let w = (cell.width as usize).max(1);

                if w == 2 {
                    if let Some(next) = row.get(c + 1) {
                        self.shown[r][c + 1] = *next;
                    }
                }

                // at the end of a line the cursor stays put (pending wrap)
                self.at = (c + w < cols).then_some((r, c + w));
                c += w;
            }
        }

        let cursor = main.cursor();
        let (cols, rows) = main.size();
        let (row, col) = (cursor.row.min(rows - 1), cursor.col.min(cols - 1));

        if self.at != Some((row, col)) {
            out.push_str(&format!("\x1b[{};{}H", row + 1, col + 1));
            self.at = Some((row, col));
        }

        let visible = cursor.visible && !screen.covered[row][col];

        if visible != self.visible {
            out.push_str(if visible { "\x1b[?25h" } else { "\x1b[?25l" });
            self.visible = visible;
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::{compose, Content, Layer, Painter};
    use crate::edit::edits::{Offset, Rect};

    fn vt(cols: usize, rows: usize, s: &str) -> avt::Vt {
        let mut vt = avt::Vt::builder().size(cols, rows).build();
        vt.feed_str(s);
        vt
    }

    fn text(screen: &super::Screen) -> Vec<String> {
        screen
            .cells
            .iter()
            .map(|r| r.iter().filter(|c| c.width > 0).map(|c| c.ch).collect())
            .collect()
    }

    #[test]
    fn whitespace_is_see_through_and_offsets_crop_at_the_edge() {
        let main = vt(8, 2, "abcdefgh\r\nijklmnop");
        let track = vt(4, 2, "X Y\r\nZZZZ");
        let layers = [Layer {
            content: Content::Screen(&track),
            crop: None,
            offset: Offset { row: 1, col: 6 },
        }];

        let s = compose(&main, &layers);
        assert_eq!(text(&s), ["abcdefgh", "ijklmnXp"]);
        assert!(s.covered[1][6] && !s.covered[1][7]);
    }

    #[test]
    fn crops_and_negative_offsets() {
        let main = vt(4, 2, "....\r\n....");
        let track = vt(4, 3, "abcd\r\nefgh\r\nijkl");
        let layers = [Layer {
            content: Content::Screen(&track),
            crop: Some(Rect {
                row: 1,
                col: 1,
                rows: 2,
                cols: 2,
            }),
            offset: Offset { row: -1, col: 0 },
        }];

        assert_eq!(text(&compose(&main, &layers)), ["jk..", "...."]);
    }

    #[test]
    fn later_layers_are_on_top() {
        let main = vt(3, 1, "...");
        let a = vt(3, 1, "aaa");
        let b = vt(3, 1, " b ");
        let layer = |vt| Layer {
            content: Content::Screen(vt),
            crop: None,
            offset: Offset::default(),
        };

        assert_eq!(text(&compose(&main, &[layer(&a), layer(&b)])), ["aba"]);
    }

    #[test]
    fn painting_draws_what_changed_and_hides_a_covered_cursor() {
        let main = vt(6, 2, "hello");
        let track = vt(6, 2, "     X");
        let mut p = Painter::new((6, 2));

        let mut term = vt(6, 2, Painter::start());
        let screen = compose(&main, &[]);
        term.feed_str(&p.paint(&screen, &main));
        assert_eq!(
            super::super::timeline::screen_text(&term)[0].trim_end(),
            "hello"
        );
        assert!(term.cursor().visible);

        // the cursor (after "hello") is covered
        let layers = [Layer {
            content: Content::Screen(&track),
            crop: None,
            offset: Offset::default(),
        }];
        let out = p.paint(&compose(&main, &layers), &main);
        assert_eq!(out, "X\x1b[1;6H\x1b[?25l");
        term.feed_str(&out);
        assert_eq!(super::super::timeline::screen_text(&term)[0], "helloX");
        assert!(!term.cursor().visible);

        // nothing changed: nothing to draw
        assert_eq!(p.paint(&compose(&main, &layers), &main), "");
    }
}
