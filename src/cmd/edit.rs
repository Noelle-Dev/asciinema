use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::asciicast::{self, CursorShape, EventData};
use crate::cli;
use crate::edit::{render, tui, Edit, EditList, Timeline};
use crate::status;

impl cli::Edit {
    pub fn run(self) -> Result<()> {
        if self.file.starts_with("http://") || self.file.starts_with("https://") || self.file == "-"
        {
            bail!("edit requires a local recording file (download it first)");
        }

        let path = PathBuf::from(&self.file);
        let mut timeline = Timeline::load(asciicast::open_from_path(&path)?)?;
        let edits_path = self
            .edits
            .clone()
            .unwrap_or_else(|| EditList::sidecar_path(&path));
        let mut edits = EditList::load_or_new(&edits_path, timeline.len())?;
        let changed = self.update_edits(&mut edits)?;

        if changed {
            edits.save(&edits_path)?;
            status::info!(
                "Saved {} edit(s) to {}",
                edits.edits.len(),
                edits_path.display()
            );
        }

        timeline.apply_edits(&edits);

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
            .chain(self.cursor.as_deref().map(parse_cursor));

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

        let (kind, preview) = match data {
            EventData::Output(s) => ("output", preview(s)),
            EventData::Input(s) => ("input", preview(s)),
            EventData::Resize(c, r) => ("resize", format!("{c}x{r}")),
            EventData::Marker(s) => ("marker", preview(s)),
            EventData::Exit(s) => ("exit", s.to_string()),
            EventData::Other(c, s) => ("other", format!("{c} {}", preview(s))),
        };

        let line = writeln!(
            out,
            "{i:>7}  {:>10}  {:>8}{edited}  {kind:<6}  {preview}",
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

    print!("{}", frame_text(&vt.text()));

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
    }
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
    use crate::asciicast;
    use crate::cli;
    use crate::edit::Edit;

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
            overwrite: false,
        }
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
