use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::{self, Duration, Instant};

use crate::asciicast::{self, CursorShape, Event, EventData};
use crate::config::Key;
use crate::tty::{DevTty, RawTty, TtyTheme};
use rgb::RGB8;

pub struct KeyBindings {
    pub quit: Key,
    pub pause: Key,
    pub step: Key,
    pub next_marker: Key,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            quit: Some(vec![0x03]),
            pause: Some(vec![b' ']),
            step: Some(vec![b'.']),
            next_marker: Some(vec![b']']),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn play(
    recording: asciicast::Asciicast<'static>,
    speed: f64,
    idle_time_limit_override: Option<f64>,
    pause_on_markers: bool,
    keys: &KeyBindings,
    auto_resize: bool,
    apply_theme: bool,
    clear: bool,
) -> Result<bool> {
    let mut tty = DevTty::open().await?;
    let header = &recording.header;

    let settings = TerminalSettings {
        cursor: header.term_cursor,
        theme: header.term_theme.clone().filter(|_| apply_theme),
        clear,
    };

    let restore = apply_terminal_settings(&tty, settings).await?;

    let result = play_on(
        &mut tty,
        recording,
        speed,
        idle_time_limit_override,
        pause_on_markers,
        keys,
        auto_resize,
    )
    .await;

    // Restore however playback ended: finished, quit or error.
    let _ = tty.write_all(restore.as_bytes()).await;

    result
}

struct TerminalSettings {
    cursor: Option<CursorShape>,
    theme: Option<TtyTheme>,
    /// Clear the screen before playing (the result stays on screen afterwards).
    clear: bool,
}

/// Switches the terminal to the recording's cursor shape and colors.
/// Returns the escape sequences that undo it.
async fn apply_terminal_settings(tty: &DevTty, settings: TerminalSettings) -> Result<String> {
    let TerminalSettings {
        cursor,
        theme,
        clear,
    } = settings;

    let mut apply = String::new();
    let mut restore = String::new();

    if clear {
        apply.push_str(crate::tty::CLEAR_SCREEN);
    }

    if let Some(shape) = cursor {
        apply.push_str(shape.escape_sequence());
        restore.push_str(CursorShape::RESET_SEQUENCE);
    }

    if let Some(theme) = theme {
        // Ask for the current colors first so exactly those can be put back.
        // Terminals that don't answer get a reset to their configured colors.
        let (_, current) = crate::tty::inspect(tty).await;

        apply.push_str(&theme_sequence(&theme));
        restore.push_str(&match current {
            Some(current) => theme_sequence(&current),
            None => THEME_RESET_SEQUENCE.to_owned(),
        });
    }

    tty.write_all(apply.as_bytes()).await?;

    Ok(restore)
}

/// Resets palette (OSC 104), foreground (OSC 110) and background (OSC 111).
const THEME_RESET_SEQUENCE: &str = "\x1b]104\x07\x1b]110\x07\x1b]111\x07";

/// Sets foreground (OSC 10), background (OSC 11) and palette (OSC 4).
fn theme_sequence(theme: &TtyTheme) -> String {
    let color = |c: RGB8| format!("rgb:{:02x}/{:02x}/{:02x}", c.r, c.g, c.b);
    let mut seq = format!(
        "\x1b]10;{}\x07\x1b]11;{}\x07",
        color(theme.fg),
        color(theme.bg)
    );

    for (i, &c) in theme.palette.iter().enumerate().take(16) {
        seq.push_str(&format!("\x1b]4;{i};{}\x07", color(c)));
    }

    seq
}

async fn play_on(
    tty: &mut DevTty,
    recording: asciicast::Asciicast<'static>,
    speed: f64,
    idle_time_limit_override: Option<f64>,
    pause_on_markers: bool,
    keys: &KeyBindings,
    auto_resize: bool,
) -> Result<bool> {
    let initial_cols = recording.header.term_cols;
    let initial_rows = recording.header.term_rows;
    let mut events = emit_session_events(recording, speed, idle_time_limit_override)?;
    let mut epoch = Instant::now();
    let mut pause_elapsed_time: Option<u64> = None;
    let mut next_event = events.recv().await.transpose()?;
    let mut input = [0u8; 1024];

    if auto_resize {
        tty.resize((initial_cols as usize, initial_rows as usize).into())
            .await?;
    }

    while let Some(Event { time, data }) = &next_event {
        if let Some(pet) = pause_elapsed_time {
            let n = tty.read(&mut input).await?;
            let key = &input[..n];

            if keys.quit.as_ref().is_some_and(|k| k == key) {
                tty.write_all("\r\n".as_bytes()).await?;
                return Ok(false);
            }

            if keys.pause.as_ref().is_some_and(|k| k == key) {
                epoch = Instant::now() - Duration::from_micros(pet);
                pause_elapsed_time = None;
            } else if keys.step.as_ref().is_some_and(|k| k == key) {
                pause_elapsed_time = Some(time.as_micros() as u64);

                match data {
                    EventData::Output(data) => {
                        tty.write_all(data.as_bytes()).await?;
                    }

                    EventData::Resize(cols, rows) if auto_resize => {
                        tty.resize((*cols as usize, *rows as usize).into()).await?;
                    }

                    _ => {}
                }

                next_event = events.recv().await.transpose()?;
            } else if keys.next_marker.as_ref().is_some_and(|k| k == key) {
                while let Some(Event { time, data }) = next_event {
                    next_event = events.recv().await.transpose()?;

                    match data {
                        EventData::Output(data) => {
                            tty.write_all(data.as_bytes()).await?;
                        }

                        EventData::Marker(_) => {
                            pause_elapsed_time = Some(time.as_micros() as u64);
                            break;
                        }

                        EventData::Resize(cols, rows) if auto_resize => {
                            tty.resize((cols as usize, rows as usize).into()).await?;
                        }

                        _ => {}
                    }
                }
            }
        } else {
            while let Some(Event { time, data }) = &next_event {
                let delay = time.as_micros() as i64 - epoch.elapsed().as_micros() as i64;

                if delay > 0 {
                    if let Ok(result) =
                        time::timeout(Duration::from_micros(delay as u64), tty.read(&mut input))
                            .await
                    {
                        let n = result?;
                        let key = &input[..n];

                        if keys.quit.as_ref().is_some_and(|k| k == key) {
                            tty.write_all("\r\n".as_bytes()).await?;
                            return Ok(false);
                        }

                        if keys.pause.as_ref().is_some_and(|k| k == key) {
                            pause_elapsed_time = Some(epoch.elapsed().as_micros() as u64);
                            break;
                        }

                        continue;
                    }
                }

                match data {
                    EventData::Output(data) => {
                        tty.write_all(data.as_bytes()).await?;
                    }

                    EventData::Resize(cols, rows) if auto_resize => {
                        tty.resize((*cols as usize, *rows as usize).into()).await?;
                    }

                    EventData::Marker(_) if pause_on_markers => {
                        pause_elapsed_time = Some(time.as_micros() as u64);
                        next_event = events.recv().await.transpose()?;
                        break;
                    }

                    _ => (),
                }

                next_event = events.recv().await.transpose()?;
            }
        }
    }

    Ok(true)
}

fn emit_session_events(
    recording: asciicast::Asciicast<'static>,
    speed: f64,
    idle_time_limit_override: Option<f64>,
) -> Result<mpsc::Receiver<Result<Event>>> {
    let idle_time_limit = idle_time_limit_override
        .or(recording.header.idle_time_limit)
        .unwrap_or(f64::MAX);

    let events = asciicast::limit_idle_time(recording.events, idle_time_limit);
    let events = asciicast::accelerate(events, speed);
    let (tx, rx) = mpsc::channel::<Result<Event>>(1024);

    tokio::task::spawn_blocking(move || {
        for event in events {
            if tx.blocking_send(event).is_err() {
                break;
            }
        }
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use rgb::RGB8;

    use super::theme_sequence;
    use crate::tty::TtyTheme;

    #[test]
    fn theme_sequence_sets_fg_bg_and_palette() {
        let theme = TtyTheme {
            fg: RGB8::new(0xcc, 0xcc, 0xcc),
            bg: RGB8::new(0x0c, 0x0c, 0x0c),
            palette: (0..16).map(|i| RGB8::new(i, 0, 0xff)).collect(),
        };

        let seq = theme_sequence(&theme);

        assert!(seq.starts_with("\x1b]10;rgb:cc/cc/cc\x07\x1b]11;rgb:0c/0c/0c\x07"));
        assert!(seq.contains("\x1b]4;0;rgb:00/00/ff\x07"));
        assert!(seq.ends_with("\x1b]4;15;rgb:0f/00/ff\x07"));
        assert_eq!(seq.matches("\x1b]4;").count(), 16);
    }
}
