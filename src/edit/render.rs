//! Writing an edited timeline out as a new recording.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::Timeline;
use crate::asciicast::{self, Asciicast, Version};
use crate::encoder::{AsciicastV3Encoder, EncoderExt};
use crate::output_writer;

/// Encodes the timeline (with edited times) as asciicast v3.
pub fn write<W: Write + ?Sized>(timeline: &Timeline, writer: &mut W) -> Result<()> {
    let cast = Asciicast {
        version: Version::Three,
        header: timeline.header.clone(),
        events: Box::new(timeline.edited_events().map(Ok)),
    };

    AsciicastV3Encoder::new(false).encode_to_writer(cast, writer)
}

/// Renders to `output`, refusing to overwrite `source` or (unless `overwrite`)
/// any existing non-empty file. A `.zst` suffix enables compression.
pub fn render_to_path(
    timeline: &Timeline,
    source: &Path,
    output: &Path,
    overwrite: bool,
) -> Result<()> {
    if output.exists() {
        if same_file(source, output) {
            bail!("refusing to render over the source recording; use render in place instead");
        }

        if !overwrite && fs::metadata(output)?.len() > 0 {
            bail!(
                "{} exists, use --overwrite to overwrite it",
                output.display()
            );
        }
    }

    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(output)
        .with_context(|| format!("can't open {}", output.display()))?;

    let compress = output.to_string_lossy().to_lowercase().ends_with(".zst");
    let mut writer = output_writer::new(file, compress)?;
    write(timeline, writer.as_mut())?;
    writer.finish()?;

    Ok(())
}

/// Path of the backup kept by [`render_in_place`]: `demo.cast` -> `demo.cast.bak`.
pub fn backup_path(source: &Path) -> PathBuf {
    let mut name = source.as_os_str().to_owned();
    name.push(".bak");

    PathBuf::from(name)
}

/// Replaces `source` with the edited recording and removes the sidecar edit list.
///
/// The previous contents of `source` are kept as `<source>.bak` (replacing any
/// older backup). The new file is written to a temp file in the same directory
/// and atomically renamed over `source`, so a failure part-way never leaves a
/// truncated recording behind. Compression of the source is preserved.
///
/// Returns the backup path.
pub fn render_in_place(timeline: &Timeline, source: &Path, sidecar: &Path) -> Result<PathBuf> {
    let dir = source
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    let compress = asciicast::is_zstd(source)?;
    let permissions = fs::metadata(source)?.permissions();

    let tmp = tempfile::Builder::new()
        .prefix(".asciinema-render-")
        .tempfile_in(dir)
        .with_context(|| format!("can't create a temp file in {}", dir.display()))?;

    let mut writer = output_writer::new(tmp.as_file().try_clone()?, compress)?;
    write(timeline, writer.as_mut())?;
    writer.finish()?;
    tmp.as_file().sync_all()?;
    fs::set_permissions(tmp.path(), permissions)?;

    let backup = backup_path(source);
    fs::copy(source, &backup)
        .with_context(|| format!("can't write backup {}", backup.display()))?;

    tmp.persist(source)
        .with_context(|| format!("can't replace {}", source.display()))?;

    if sidecar.exists() {
        fs::remove_file(sidecar)
            .with_context(|| format!("can't remove edit list {}", sidecar.display()))?;
    }

    // captured effects are now part of the recording
    let effects = super::fx::cache_dir(source);

    if effects.is_dir() {
        fs::remove_dir_all(&effects)
            .with_context(|| format!("can't remove effects cache {}", effects.display()))?;
    }

    Ok(backup)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::{backup_path, render_in_place, render_to_path};
    use crate::asciicast;
    use crate::edit::{Edit, EditList, Timeline};

    fn load(path: &std::path::Path) -> Timeline {
        Timeline::load(asciicast::open_from_path(path).unwrap()).unwrap()
    }

    fn times(path: &std::path::Path) -> Vec<u128> {
        asciicast::open_from_path(path)
            .unwrap()
            .events
            .map(|e| e.unwrap().time.as_millis())
            .collect()
    }

    #[test]
    fn in_place_replaces_source_keeps_backup_and_removes_sidecar() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("demo.cast");
        let sidecar = EditList::sidecar_path(&src);
        fs::copy("tests/casts/full-v2.cast", &src).unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();

        let mut tl = load(&src);
        let mut edits = EditList::new(tl.len());
        edits
            .push(Edit::CapIdle {
                from: 0,
                to: 4,
                max: 1.0,
            })
            .unwrap();
        edits.save(&sidecar).unwrap();
        tl.apply_edits(&edits).unwrap();

        let backup = render_in_place(&tl, &src, &sidecar).unwrap();

        assert_eq!(backup, backup_path(&src));
        assert_eq!(
            fs::read(&backup).unwrap(),
            fs::read("tests/casts/full-v2.cast").unwrap()
        );
        assert!(!sidecar.exists());
        assert_eq!(times(&src), [0, 1000, 2000, 3000, 4000]);
        assert_eq!(
            fs::metadata(&src).unwrap().permissions().mode() & 0o777,
            0o644
        );

        // no temp files left behind
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".asciinema-render-")
            })
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn in_place_preserves_zstd() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("demo.cast.zst");
        let plain = load(std::path::Path::new("tests/casts/minimal-v3.cast"));
        render_to_path(&plain, std::path::Path::new("x"), &src, false).unwrap();
        assert!(asciicast::is_zstd(&src).unwrap());

        let tl = load(&src);
        render_in_place(&tl, &src, &EditList::sidecar_path(&src)).unwrap();

        assert!(asciicast::is_zstd(&src).unwrap());
        assert_eq!(times(&src).len(), 1);
    }

    #[test]
    fn render_refuses_source_and_existing_files() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("demo.cast");
        let out = dir.path().join("out.cast");
        fs::copy("tests/casts/full-v2.cast", &src).unwrap();
        let tl = load(&src);

        assert!(render_to_path(&tl, &src, &src, true).is_err());
        render_to_path(&tl, &src, &out, false).unwrap();
        assert!(render_to_path(&tl, &src, &out, false).is_err());
        render_to_path(&tl, &src, &out, true).unwrap();
    }
}
