//! Non-destructive editing of asciicast recordings.
//!
//! The original recording is never modified. Edits are stored in a sidecar
//! "edit list" file (by default `<recording>.edits.json`), and rendered into a
//! new recording on demand. This keeps edits undoable and replayable, and lets
//! the interactive editor and the `--render` path share one implementation.

pub mod edits;
pub mod render;
pub mod retime;
pub mod themes;
pub mod timeline;
pub mod tui;

pub use edits::{Edit, EditList};
pub use timeline::Timeline;
