use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::asciicast::{self, CursorShape, EventData};
use crate::cli;
use crate::edit::timeline::Origin;
use crate::edit::{fx, render, tui, Clip, Edit, EditList, Timeline};
use crate::status;

impl cli::Edit {
    pub fn run(self) -> Result<()> {
        if self.file.starts_with("http://") || self.file.starts_with("https://") || self.file == "-"
        {
            bail!("edit requires a local recording file (download it first)");
        }

        if self.fx_list || self.fx_options.is_some() {
            return self.print_fx_help();
        }

        let path = PathBuf::from(&self.file);
        let mut timeline = Timeline::load(asciicast::open_from_path(&path)?)?;
        let edits_path = self
            .edits
            .clone()
            .unwrap_or_else(|| EditList::sidecar_path(&path));
        if let Some(dir) = path.parent() {
            timeline.set_clip_base(dir);
        }

        let mut edits = EditList::load_or_new(&edits_path, timeline.original_len())?;
        let mut changed = self.update_edits(&mut edits)?;

        if let Some(arg) = &self.edit_text {
            let edit = self.text_edit(arg, &timeline)?;
            let description = describe(&edit);
            edits.push(edit)?;
            status::info!("Added {description}");
            changed = true;
        }

        let effect = match (&self.insert_effect, &self.replace_effect) {
            (Some(_), Some(_)) => bail!("use --insert-effect or --replace-effect, not both"),
            (Some(a), None) => Some((a, false)),
            (None, Some(a)) => Some((a, true)),
            (None, None) => None,
        };

        if let Some((arg, replace)) = effect {
            let edit = self.capture_effect(arg, replace, &timeline, &path)?;
            let description = describe(&edit);
            edits.push(edit)?;
            status::info!("Added {description}");
            changed = true;
        }

        // build before saving, so an edit that can't be applied isn't kept
        timeline.apply_edits(&edits)?;

        if changed {
            edits.save(&edits_path)?;
            status::info!(
                "Saved {} edit(s) to {}",
                edits.edits.len(),
                edits_path.display()
            );
        }

        if self.list {
            print_events(&timeline)?;
        }

        if let Some(secs) = self.frame {
            print_frame(&timeline, secs)?;
        }

        if let Some(output) = &self.render {
            self.render(&timeline, &path, output)?;
        }

        if self.render_in_place {
            warn_idle_time_limit(&timeline);
            let backup = render::render_in_place(&timeline, &path, &edits_path)?;
            status::info!(
                "Rendered edits into {} (previous version kept as {})",
                path.display(),
                backup.display()
            );
        }

        let did_something = changed
            || self.list
            || self.frame.is_some()
            || self.render.is_some()
            || self.render_in_place;

        if !did_something {
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                tui::run(path, edits_path, timeline, edits)?;
            } else {
                print_summary(&timeline, &edits, &edits_path);
            }
        }

        Ok(())
    }

    /// Applies --undo and any new edits. Returns whether the edit list changed.
    fn update_edits(&self, edits: &mut EditList) -> Result<bool> {
        let n = edits.event_count;
        let mut changed = false;

        if self.undo {
            match edits.undo() {
                Some(edit) => {
                    status::info!("Undid {}", describe(&edit));
                    changed = true;
                }

                None => status::warning!("Nothing to undo"),
            }
        }

        for range in &self.restore {
            let (from, to) = parse_range(range, n)?;

            if edits.restore(from, to) {
                status::info!("Restored events {from}..{to}");
                changed = true;
            } else {
                status::warning!("Nothing is cut in {from}..{to}");
            }
        }

        let new_edits = self
            .set_delay
            .iter()
            .map(|s| parse_set_delay(s, n))
            .chain(self.scale.iter().map(|s| {
                parse_range_op(s, n).map(|(from, to, factor)| Edit::Scale { from, to, factor })
            }))
            .chain(self.cap_idle.iter().map(|s| {
                parse_range_op(s, n).map(|(from, to, max)| Edit::CapIdle { from, to, max })
            }))
            .chain(self.cursor.as_deref().map(parse_cursor))
            .chain(
                self.delete
                    .iter()
                    .map(|s| parse_range(s, n).map(|(from, to)| Edit::Delete { from, to })),
            )
            .chain(self.insert_hold.iter().map(|s| {
                parse_at_secs(s, n).map(|(at, secs)| Edit::Insert {
                    at,
                    clip: Clip::Hold { secs },
                })
            }))
            .chain(self.insert_blank.iter().map(|s| {
                parse_at_secs(s, n).map(|(at, secs)| Edit::Insert {
                    at,
                    clip: Clip::Blank { secs },
                })
            }))
            .chain(self.insert_text.iter().map(|s| parse_insert_text(s, n)))
            .chain(self.insert_cast.iter().map(|s| parse_insert_cast(s, n)))
            .chain(self.duplicate.iter().map(|s| {
                parse_range(s, n).map(|(from, to)| Edit::Insert {
                    at: to + 1,
                    clip: Clip::Recording {
                        path: None,
                        from,
                        to: Some(to),
                    },
                })
            }));

        for edit in new_edits {
            let edit = edit?;
            let description = describe(&edit);

            if edits.push(edit)? {
                status::info!("Replaced an earlier edit of the same kind: {description}");
            } else {
                status::info!("Added {description}");
            }
            changed = true;
        }

        Ok(changed)
    }

    fn print_fx_help(&self) -> Result<()> {
        let tool = fx::find_tool()?;

        match &self.fx_options {
            Some(effect) => {
                let options = fx::effect_options(&tool, effect)?;

                if options.is_empty() {
                    bail!(
                        "{} doesn't know an effect called '{effect}' (see --fx-list)",
                        tool.name()
                    );
                }

                for o in options {
                    let value = if o.takes_value { " <value>" } else { "" };

                    if !print_line(&format!("{}{value}\n    {}\n", o.flag, o.description)) {
                        break;
                    }
                }
            }

            None => {
                for e in fx::list_effects(&tool)? {
                    if !print_line(&format!("{:<16} {}", e.name, e.description)) {
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Parses `AT[@ROW,COL]:FROM=>TO`, finding FROM on the screen at AT.
    fn text_edit(&self, arg: &str, timeline: &Timeline) -> Result<Edit> {
        let err = || anyhow!("invalid --edit-text '{arg}', expected AT[@ROW,COL]:FROM=>TO");
        let (place, change) = arg.split_once(':').ok_or_else(err)?;
        let (from, to) = change.split_once("=>").ok_or_else(err)?;

        let (at, spot) = match place.split_once('@') {
            Some((at, spot)) => {
                let (r, c) = spot.split_once(',').ok_or_else(err)?;
                let num = |v: &str| v.trim().parse::<usize>().map_err(|_| err());
                (at, Some((num(r)?, num(c)?)))
            }
            None => (place, None),
        };

        let at = parse_at(at, timeline.original_len())?;
        let screen = timeline.original_vt_at(at);
        let lines = crate::edit::timeline::screen_text(&screen);
        let from_chars: Vec<char> = from.chars().collect();

        let found = lines.iter().enumerate().find_map(|(r, line)| {
            let chars: Vec<char> = line.chars().collect();
            (0..chars.len())
                .find(|&c| chars[c..].starts_with(&from_chars))
                .map(|c| (r, c))
        });

        let (row, col) = match spot {
            Some((r, c)) => {
                let here: String = lines
                    .get(r)
                    .map(|l| l.chars().skip(c).take(from_chars.len()).collect())
                    .unwrap_or_default();

                if here != from {
                    status::warning!("The screen at {r},{c} shows \"{here}\", not \"{from}\"; changing it anyway");
                }

                (r, c)
            }

            None => match found {
                Some(spot) => spot,
                None => bail!(
                    "\"{from}\" isn't on the screen at {at}; give its position with AT@ROW,COL"
                ),
            },
        };

        // the look of the new text: FROM's own style, with any changes asked for
        let cell_style = |r: usize, c: usize| {
            let pen = screen
                .view()
                .nth(r)
                .and_then(|l| l.cells().get(c).map(|cell| *cell.pen()))
                .unwrap_or_default();

            crate::edit::style::TextStyle::from_pen(&pen)
        };

        let restyled = self.text_fg.is_some()
            || self.text_bg.is_some()
            || self.text_bold
            || self.text_italic
            || self.text_underline;

        let mut new_style = cell_style(row, col);

        if let Some(c) = &self.text_fg {
            new_style.fg = Some(crate::edit::style::StyleColor::parse(c)?);
        }

        if let Some(c) = &self.text_bg {
            new_style.bg = Some(crate::edit::style::StyleColor::parse(c)?);
        }

        new_style.bold |= self.text_bold;
        new_style.italic |= self.text_italic;
        new_style.underline |= self.text_underline;

        // just the changed text: playback moves the rest of the line along
        let styles = if restyled {
            crate::edit::style::to_runs(&vec![new_style; to.chars().count()])
        } else {
            Vec::new()
        };

        let (from, to) = (from.to_owned(), to.to_owned());

        Ok(Edit::Text {
            at,
            row,
            col,
            from,
            to,
            styles,
        })
    }

    fn capture_effect(
        &self,
        arg: &str,
        replace: bool,
        timeline: &Timeline,
        path: &Path,
    ) -> Result<Edit> {
        let (place, effect) = arg.split_once('=').ok_or_else(|| {
            anyhow!("invalid effect '{arg}', expected AT=EFFECT (or FROM..TO=EFFECT to replace)")
        })?;

        let n = timeline.original_len();

        // where it goes, and the screen it animates
        let (at, replaced) = if replace {
            let (from, to) = parse_range(place, n)?;
            (from, Some(to))
        } else {
            (parse_at(place, n)?, None)
        };

        let source = match (&self.fx_text, &self.fx_region, self.fx_screen) {
            (Some(text), None, false) => fx::FxSource::Text {
                text: text.replace("\\n", "\n"),
                anchor: fx::Anchor::from_flag(&self.fx_anchor).unwrap_or_default(),
                area: match &self.fx_area {
                    Some(a) => match parse_region(a)? {
                        fx::FxSource::Region {
                            col,
                            row,
                            cols,
                            rows,
                        } => Some(fx::Area {
                            col,
                            row,
                            cols,
                            rows,
                        }),
                        _ => None,
                    },
                    None => None,
                },
                over: self.fx_over,
            },
            (None, Some(region), false) => parse_region(region)?,
            (None, None, true) => fx::FxSource::Screen,
            _ => bail!("give exactly one of --fx-text, --fx-region or --fx-screen"),
        };

        let spec = fx::FxSpec {
            effect: effect.trim().to_owned(),
            source,
            args: self.fx_arg.clone(),
            speed: self.fx_speed,
            duration: self.fx_duration,
            seed: self.fx_seed.unwrap_or_else(|| rand::random::<u32>() as u64),
            keep_colors: self.fx_keep_colors,
        };

        let tool = fx::find_tool()?;
        status::info!("Capturing {} with {}...", spec.effect, tool.name());
        let screen = timeline.original_vt_at(replaced.map_or(at, |to| to + 1));
        let cache = fx::capture_to_cache(&tool, &spec, &screen, path)?;

        let clip = Clip::Effect {
            fx: spec,
            hold: self.fx_hold,
            cache,
        };

        Ok(match replaced {
            Some(to) => Edit::Replace { from: at, to, clip },
            None => Edit::Insert { at, clip },
        })
    }

    fn render(&self, timeline: &Timeline, input: &Path, output: &str) -> Result<()> {
        warn_idle_time_limit(timeline);

        if output == "-" {
            let mut stdout = io::stdout().lock();
            render::write(timeline, &mut stdout)?;

            return Ok(stdout.flush()?);
        }

        render::render_to_path(timeline, input, Path::new(output), self.overwrite)?;
        status::info!("Rendered edited recording to {output}");

        Ok(())
    }
}

fn warn_idle_time_limit(timeline: &Timeline) {
    if timeline.header.idle_time_limit.is_some() {
        status::warning!(
            "The recording's idle_time_limit header is kept; players will still cap pauses at it"
        );
    }
}

fn print_summary(timeline: &Timeline, edits: &EditList, edits_path: &Path) {
    println!(
        "{} events, duration {}",
        timeline.len(),
        fmt_secs(timeline.duration())
    );

    if edits.edits.is_empty() {
        println!("No edits yet ({})", edits_path.display());
    } else {
        println!("{} edit(s) in {}:", edits.edits.len(), edits_path.display());

        for (i, edit) in edits.edits.iter().enumerate() {
            println!("  {}. {}", i + 1, describe(edit));
        }
    }

    println!("\nSee `asciinema edit --help` for available operations.");
}

fn print_events(timeline: &Timeline) -> Result<()> {
    let mut out = io::stdout().lock();

    writeln!(
        out,
        "{:>7}  {:>10}  {:>9}  {:<6}  DATA",
        "EVENT", "TIME", "DELAY", "TYPE"
    )?;

    for i in 0..timeline.len() {
        let (time, data) = timeline.event(i).expect("index in range");
        let delay = timeline.delay(i).expect("index in range");
        let edited = if timeline.is_retimed(i) { "*" } else { " " };

        // edits refer to original event numbers; synthetic items are labelled
        let (label, note) = match timeline.origin(i) {
            Some(Origin::Event(e)) => (e.to_string(), String::new()),
            Some(Origin::Cut { from, to }) => (
                "✂".to_owned(),
                format!("[jump cut over events {from}..{to}] "),
            ),
            Some(Origin::Clip { insert }) => (format!("+clip{insert}"), String::new()),
            Some(Origin::TextEdit { edit }) => (format!("✎text{edit}"), String::new()),
            None => (String::new(), String::new()),
        };

        let (kind, preview) = match data {
            EventData::Output(s) => ("output", format!("{note}{}", preview(s))),
            EventData::Input(s) => ("input", preview(s)),
            EventData::Resize(c, r) => ("resize", format!("{c}x{r}")),
            EventData::Marker(s) => ("marker", preview(s)),
            EventData::Exit(s) => ("exit", s.to_string()),
            EventData::Other(c, s) => ("other", format!("{c} {}", preview(s))),
        };

        let line = writeln!(
            out,
            "{label:>7}  {:>10}  {:>8}{edited}  {kind:<6}  {preview}",
            fmt_secs(time),
            fmt_secs(delay)
        );

        // Stop quietly when piped into `head` etc.
        if matches!(&line, Err(e) if e.kind() == io::ErrorKind::BrokenPipe) {
            return Ok(());
        }

        line?;
    }

    Ok(())
}

/// Prints a line; false once stdout is closed (e.g. piped into `head`).
fn print_line(line: &str) -> bool {
    writeln!(io::stdout().lock(), "{line}").is_ok()
}

fn print_frame(timeline: &Timeline, secs: f64) -> Result<()> {
    if !secs.is_finite() || secs < 0.0 {
        bail!("--frame must be a non-negative number of seconds");
    }

    let t = Duration::from_secs_f64(secs);
    let position = timeline.position_at(t);
    let vt = timeline.vt_at_position(position);
    let (cols, rows) = vt.size();

    // Metadata goes to stderr so stdout is just the screen text, safe to pipe or diff.
    status::info!(
        "Frame at {} (after event {}), {cols}x{rows}",
        fmt_secs(t),
        position
            .checked_sub(1)
            .map_or("-".to_owned(), |p| p.to_string())
    );

    print!("{}", frame_text(&crate::edit::timeline::screen_text(&vt)));

    Ok(())
}

/// Screen lines with trailing whitespace and trailing blank rows removed, so
/// wide recordings don't wrap in a narrower terminal.
fn frame_text(lines: &[String]) -> String {
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    let used = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |i| i + 1);

    lines[..used].iter().fold(String::new(), |mut out, l| {
        out.push_str(l);
        out.push('\n');

        out
    })
}

fn preview(s: &str) -> String {
    const MAX: usize = 60;
    let escaped: String = s.escape_debug().collect();

    if escaped.chars().count() > MAX {
        let mut p: String = escaped.chars().take(MAX - 1).collect();
        p.push('…');

        p
    } else {
        escaped
    }
}

fn fmt_secs(d: Duration) -> String {
    format!("{:.3}s", d.as_secs_f64())
}

pub(crate) fn describe(edit: &Edit) -> String {
    match *edit {
        Edit::SetDelay { event, secs } => format!("set delay of event {event} to {secs}s"),
        Edit::Scale { from, to, factor } => {
            format!("scale delays of events {from}..{to} by {factor}")
        }
        Edit::CapIdle { from, to, max } => format!("cap delays of events {from}..{to} at {max}s"),
        Edit::SetCursor { shape: Some(s) } => format!("set cursor shape to {}", s.name()),
        Edit::SetCursor { shape: None } => "use the terminal's default cursor shape".to_owned(),
        Edit::SetTitle { title: Some(ref t) } => format!("set title to \"{t}\""),
        Edit::SetTitle { title: None } => "remove the title".to_owned(),
        Edit::SetCommand {
            command: Some(ref c),
        } => format!("set command to \"{c}\""),
        Edit::SetCommand { command: None } => "remove the command".to_owned(),
        Edit::SetIdleTimeLimit { secs: Some(s) } => format!("set idle time limit to {s}s"),
        Edit::SetIdleTimeLimit { secs: None } => "remove the idle time limit".to_owned(),
        Edit::SetTheme { theme: Some(_) } => "change the color theme".to_owned(),
        Edit::SetTheme { theme: None } => "remove the color theme".to_owned(),
        Edit::Delete { from, to } => format!("cut events {from}..{to}"),
        Edit::Insert { at, ref clip } => format!("insert {} at {at}", clip.describe()),
        Edit::Replace { from, to, ref clip } => {
            format!("replace events {from}..{to} with {}", clip.describe())
        }
        Edit::Text {
            at,
            row,
            col,
            ref from,
            ref to,
            ref styles,
        } => {
            let restyled = if styles.is_empty() { "" } else { ", restyled" };

            format!("change \"{from}\" to \"{to}\" at row {row}, col {col} at {at}{restyled}")
        }
    }
}

/// Parses `FROM..TO` (either end may be omitted) or a single index `N`.
fn parse_range(s: &str, event_count: usize) -> Result<(usize, usize)> {
    let (from, to, _) = parse_range_op(&format!("{s}=0"), event_count)?;

    Ok((from, to))
}

/// Parses an insert position: an event index, or the event count for the end.
fn parse_at(s: &str, event_count: usize) -> Result<usize> {
    let at = s
        .trim()
        .parse::<usize>()
        .map_err(|_| anyhow!("invalid insert position '{s}'"))?;

    if at > event_count {
        bail!("insert position {at} out of range (0..={event_count})");
    }

    Ok(at)
}

/// Parses `AT=SECS`.
fn parse_at_secs(s: &str, event_count: usize) -> Result<(usize, f64)> {
    let (at, secs) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid argument '{s}', expected AT=SECS"))?;

    let secs = secs
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid number of seconds '{secs}' in '{s}'"))?;

    Ok((parse_at(at, event_count)?, secs))
}

/// Parses `AT=SECS:TEXT`, where `\n` in TEXT starts a new line.
fn parse_insert_text(s: &str, event_count: usize) -> Result<Edit> {
    let (head, text) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid --insert-text '{s}', expected AT=SECS:TEXT"))?;

    let (at, secs) = parse_at_secs(head, event_count)?;
    let text = text.replace("\\n", "\n");

    Ok(Edit::Insert {
        at,
        clip: Clip::Text { text, secs },
    })
}

/// Parses `AT=PATH` or `AT=PATH@FROM..TO`.
fn parse_insert_cast(s: &str, event_count: usize) -> Result<Edit> {
    let (at, rest) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid --insert-cast '{s}', expected AT=PATH[@FROM..TO]"))?;

    let at = parse_at(at, event_count)?;

    let (path, from, to) = match rest.rsplit_once('@') {
        Some((path, range)) => {
            let (a, b) = range
                .split_once("..")
                .ok_or_else(|| anyhow!("invalid range '{range}' in '{s}', expected FROM..TO"))?;

            let num = |v: &str| {
                v.trim()
                    .parse::<usize>()
                    .map_err(|_| anyhow!("invalid event index '{v}' in '{s}'"))
            };
            let from = if a.trim().is_empty() { 0 } else { num(a)? };
            let to = if b.trim().is_empty() {
                None
            } else {
                Some(num(b)?)
            };

            (path, from, to)
        }

        None => (rest, 0, None),
    };

    Ok(Edit::Insert {
        at,
        clip: Clip::Recording {
            path: Some(path.to_owned()),
            from,
            to,
        },
    })
}

/// Parses `COL,ROW,COLSxROWS`.
pub(crate) fn parse_region(s: &str) -> Result<fx::FxSource> {
    let err = || anyhow!("invalid region '{s}', expected COL,ROW,COLSxROWS (e.g. 0,2,40x3)");
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();

    let [col, row, size] = parts[..] else {
        return Err(err());
    };

    let (cols, rows) = size.split_once('x').ok_or_else(err)?;
    let num = |v: &str| v.trim().parse::<usize>().map_err(|_| err());

    Ok(fx::FxSource::Region {
        col: num(col)?,
        row: num(row)?,
        cols: num(cols)?,
        rows: num(rows)?,
    })
}

/// Parses a cursor shape name, or `default` to remove it.
fn parse_cursor(name: &str) -> Result<Edit> {
    let shape = match name {
        "default" => None,
        name => Some(
            CursorShape::from_name(name).ok_or_else(|| anyhow!("unknown cursor shape '{name}'"))?,
        ),
    };

    Ok(Edit::SetCursor { shape })
}

/// Parses `EVENT=SECS`.
fn parse_set_delay(s: &str, event_count: usize) -> Result<Edit> {
    let (event, secs) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid --set-delay '{s}', expected EVENT=SECS"))?;

    let event = parse_index(event, event_count)?;
    let secs = secs
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid number of seconds '{secs}' in '{s}'"))?;

    Ok(Edit::SetDelay { event, secs })
}

/// Parses `FROM..TO=VALUE`. Either end of the range may be omitted (`..TO`,
/// `FROM..`, `..`), and a single index `N` means `N..N`.
fn parse_range_op(s: &str, event_count: usize) -> Result<(usize, usize, f64)> {
    let (range, value) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid argument '{s}', expected FROM..TO=VALUE"))?;

    let (from, to) = match range.split_once("..") {
        Some((from, to)) => {
            let from = if from.trim().is_empty() {
                0
            } else {
                parse_index(from, event_count)?
            };

            let to = if to.trim().is_empty() {
                event_count.saturating_sub(1)
            } else {
                parse_index(to, event_count)?
            };

            (from, to)
        }

        None => {
            let i = parse_index(range, event_count)?;
            (i, i)
        }
    };

    let value = value
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid number '{value}' in '{s}'"))?;

    Ok((from, to, value))
}

/// Parses an event index. Negative indexes count from the end (-1 is the last event).
fn parse_index(s: &str, event_count: usize) -> Result<usize> {
    let s = s.trim();
    let i: i64 = s
        .parse()
        .map_err(|_| anyhow!("invalid event index '{s}'"))?;

    if i < 0 {
        let back = i.unsigned_abs() as usize;

        event_count
            .checked_sub(back)
            .ok_or_else(|| anyhow!("event index {i} out of range ({event_count} events)"))
    } else {
        Ok(i as usize)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{frame_text, parse_range_op, parse_set_delay};
    use crate::asciicast::{self, EventData};
    use crate::cli;
    use crate::edit::{Edit, Timeline};

    #[test]
    fn parse_args() {
        assert_eq!(
            parse_set_delay("4=0.25", 10).unwrap(),
            Edit::SetDelay {
                event: 4,
                secs: 0.25
            }
        );

        assert_eq!(
            parse_set_delay("-1=2", 10).unwrap(),
            Edit::SetDelay {
                event: 9,
                secs: 2.0
            }
        );

        assert!(parse_set_delay("4", 10).is_err());
        assert!(parse_set_delay("x=1", 10).is_err());
        assert!(parse_set_delay("-11=1", 10).is_err());

        assert_eq!(parse_range_op("2..5=0.5", 10).unwrap(), (2, 5, 0.5));
        assert_eq!(parse_range_op("..5=1", 10).unwrap(), (0, 5, 1.0));
        assert_eq!(parse_range_op("3..=1", 10).unwrap(), (3, 9, 1.0));
        assert_eq!(parse_range_op("..=1", 10).unwrap(), (0, 9, 1.0));
        assert_eq!(parse_range_op("7=2", 10).unwrap(), (7, 7, 2.0));
        assert!(parse_range_op("1..2", 10).is_err());
    }

    #[test]
    fn frame_text_trims_padding_and_blank_rows() {
        let lines = vec![
            "$ echo hi    ".to_owned(),
            "".to_owned(),
            "hi       ".to_owned(),
            "     ".to_owned(),
            "".to_owned(),
        ];

        assert_eq!(frame_text(&lines), "$ echo hi\n\nhi\n");
        assert_eq!(frame_text(&["  ".to_owned()]), "");
    }

    fn edit_cmd(file: &str) -> cli::Edit {
        cli::Edit {
            file: file.to_owned(),
            edits: None,
            list: false,
            frame: None,
            set_delay: vec![],
            scale: vec![],
            cap_idle: vec![],
            undo: false,
            render: None,
            render_in_place: false,
            cursor: None,
            delete: vec![],
            restore: vec![],
            insert_hold: vec![],
            insert_blank: vec![],
            insert_text: vec![],
            insert_cast: vec![],
            duplicate: vec![],
            insert_effect: None,
            edit_text: None,
            text_fg: None,
            text_bg: None,
            text_bold: false,
            text_italic: false,
            text_underline: false,
            replace_effect: None,
            fx_duration: None,
            fx_anchor: "c".to_owned(),
            fx_area: None,
            fx_over: false,
            fx_text: None,
            fx_region: None,
            fx_screen: false,
            fx_speed: 1.0,
            fx_seed: None,
            fx_hold: 1.0,
            fx_keep_colors: false,
            fx_arg: vec![],
            fx_list: false,
            fx_options: None,
            overwrite: false,
        }
    }

    #[test]
    fn cut_and_insert_end_to_end() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("demo.cast");
        let output = dir.path().join("out.cast");
        fs::copy("tests/casts/full-v2.cast", &input).unwrap();
        fs::copy("tests/casts/minimal-v3.cast", dir.path().join("other.cast")).unwrap();
        let input_s = input.to_string_lossy().into_owned();

        let mut cmd = edit_cmd(&input_s);
        cmd.delete = vec!["1..1".to_owned()];
        cmd.insert_text = vec!["0=1.5:Title\\nline two".to_owned()];
        cmd.insert_cast = vec!["5=other.cast".to_owned()];
        cmd.render = Some(output.to_string_lossy().into_owned());
        cmd.run().unwrap();

        let events: Vec<_> = asciicast::open_from_path(&output)
            .unwrap()
            .events
            .map(|e| e.unwrap())
            .collect();

        let outputs: String = events
            .iter()
            .filter_map(|e| match &e.data {
                EventData::Output(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();

        assert!(outputs.contains("Title") && outputs.contains("line two"));
        assert!(outputs.contains("hello"), "other.cast's output is pasted");

        // a bad edit is rejected and the edit list on disk is unchanged
        let saved = fs::read_to_string(dir.path().join("demo.cast.edits.json")).unwrap();
        let mut cmd = edit_cmd(&input_s);
        cmd.insert_cast = vec!["0=missing.cast".to_owned()];
        assert!(cmd.run().is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("demo.cast.edits.json")).unwrap(),
            saved
        );

        // restoring the cut
        let mut cmd = edit_cmd(&input_s);
        cmd.restore = vec!["1..1".to_owned()];
        cmd.run().unwrap();
        let saved = fs::read_to_string(dir.path().join("demo.cast.edits.json")).unwrap();
        assert!(!saved.contains("delete"));
    }

    #[test]
    fn edit_text_end_to_end() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("demo.cast");
        let output = dir.path().join("out.cast");
        fs::write(
            &input,
            concat!(
                "{\"version\":3,\"term\":{\"cols\":20,\"rows\":4}}\n",
                "[0.1, \"o\", \"user$ ls\\r\\n\"]\n",
                "[0.1, \"o\", \"file\\r\\n\"]\n",
                "[0.1, \"o\", \"user$ \"]\n",
            ),
        )
        .unwrap();
        let input_s = input.to_string_lossy().into_owned();

        // once: found on the screen at that point
        let mut cmd = edit_cmd(&input_s);
        cmd.edit_text = Some("2:file=>data".to_owned());
        cmd.run().unwrap();

        // the prompt after it, which gets longer: the cursor moves along
        let mut cmd = edit_cmd(&input_s);
        cmd.edit_text = Some("3@2,0:user=>me".to_owned());
        cmd.render = Some(output.to_string_lossy().into_owned());
        cmd.run().unwrap();

        let tl = Timeline::load(asciicast::open_from_path(&output).unwrap()).unwrap();
        let end: Vec<String> = tl
            .vt_at_position(tl.len())
            .text()
            .iter()
            .map(|l| l.trim_end().to_owned())
            .collect();
        assert_eq!(end[..3], ["user$ ls", "data", "me$"]);
        assert_eq!(tl.vt_at_position(tl.len()).cursor().col, 4);

        // text that isn't there is an error
        let mut cmd = edit_cmd(&input_s);
        cmd.edit_text = Some("1:nope=>x".to_owned());
        assert!(cmd.run().is_err());
    }

    #[test]
    fn edit_text_styles_from_the_command_line() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("demo.cast");
        fs::copy("tests/casts/full-v2.cast", &input).unwrap();
        let input_s = input.to_string_lossy().into_owned();

        let mut cmd = edit_cmd(&input_s);
        cmd.edit_text = Some("2:ółć=>OK".to_owned());
        cmd.text_fg = Some("bright-green".to_owned());
        cmd.text_bold = true;
        cmd.run().unwrap();

        let saved = fs::read_to_string(dir.path().join("demo.cast.edits.json")).unwrap();
        assert!(
            saved.contains("bright-green") && saved.contains("\"bold\": true"),
            "{saved}"
        );

        // a bad colour is an error
        let mut cmd = edit_cmd(&input_s);
        cmd.edit_text = Some("2:ż=>Z".to_owned());
        cmd.text_fg = Some("reddish".to_owned());
        assert!(cmd.run().is_err());
    }

    #[test]
    fn edit_and_render_end_to_end() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("demo.cast");
        let output = dir.path().join("out.cast");
        fs::copy("tests/casts/full-v2.cast", &input).unwrap();
        let input_s = input.to_string_lossy().into_owned();

        let original: Vec<_> = asciicast::open_from_path(&input)
            .unwrap()
            .events
            .map(|e| e.unwrap().time.as_secs_f64())
            .collect();

        // add edits (persisted to the sidecar)
        let mut cmd = edit_cmd(&input_s);
        cmd.set_delay = vec!["1=0.5".to_owned()];
        cmd.cap_idle = vec!["..=1".to_owned()];
        cmd.run().unwrap();

        assert!(dir.path().join("demo.cast.edits.json").exists());

        // render in a separate invocation, picking the edits up from the sidecar
        let mut cmd = edit_cmd(&input_s);
        cmd.render = Some(output.to_string_lossy().into_owned());
        cmd.run().unwrap();

        let rendered = asciicast::open_from_path(&output).unwrap();
        assert_eq!(rendered.version, asciicast::Version::Three);

        let times: Vec<_> = rendered
            .events
            .map(|e| e.unwrap().time.as_secs_f64())
            .collect();

        assert_eq!(times.len(), original.len());
        // asciicast v3 stores intervals with millisecond precision
        assert!((times[1] - (original[0] + 0.5)).abs() < 2e-3);

        for w in times.windows(2) {
            assert!(w[1] - w[0] <= 1.0 + 2e-3, "delay not capped: {times:?}");
        }

        // the source recording is untouched
        assert_eq!(
            fs::read("tests/casts/full-v2.cast").unwrap(),
            fs::read(&input).unwrap()
        );

        // refuses to clobber without --overwrite, and never renders over the source
        let mut cmd = edit_cmd(&input_s);
        cmd.render = Some(output.to_string_lossy().into_owned());
        assert!(cmd.run().is_err());

        let mut cmd = edit_cmd(&input_s);
        cmd.render = Some(input_s.clone());
        cmd.overwrite = true;
        assert!(cmd.run().is_err());

        // undo removes the last edit
        let mut cmd = edit_cmd(&input_s);
        cmd.undo = true;
        cmd.run().unwrap();

        let json = fs::read_to_string(dir.path().join("demo.cast.edits.json")).unwrap();
        assert!(json.contains("set_delay") && !json.contains("cap_idle"));
    }
}
