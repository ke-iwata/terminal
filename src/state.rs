//! Persisted UI state -- things the app remembers about itself between
//! runs (currently just the window frame). Deliberately a separate file
//! from `~/.terminal.config.toml`: config is the user's to edit, state is
//! the app's to overwrite, and mixing them means every clean exit
//! rewrites (and reformats) a hand-edited file.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowFrame {
    /// Outer position of the window's top-left corner, in physical pixels.
    pub x: i32,
    pub y: i32,
    /// Inner (content) size, in physical pixels.
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub window: Option<WindowFrame>,
    /// Whether the file-tree sidebar was open. Someone using it instead
    /// of Finder shouldn't have to reopen it every launch.
    pub file_tree_visible: bool,
    /// The sidebar's dragged width in pixels; zero means the default.
    pub file_tree_width: f32,
}

fn state_path() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|home| PathBuf::from(home).join(".terminal.state.toml"))
}

/// Load persisted state, falling back to defaults on any problem -- a
/// missing or corrupt state file must never break startup.
pub fn load() -> State {
    let Some(path) = state_path() else {
        return State::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

/// Best-effort write; a read-only home directory just means the next
/// launch uses defaults.
pub fn save(state: &State) {
    let Some(path) = state_path() else {
        return;
    };
    if let Ok(serialized) = toml::to_string(state) {
        let _ = std::fs::write(path, serialized);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_frame_round_trips_through_toml() {
        let state = State {
            window: Some(WindowFrame { x: -12, y: 40, width: 1280, height: 800 }),
            file_tree_visible: true,
            file_tree_width: 320.0,
        };
        let serialized = toml::to_string(&state).unwrap();
        let parsed: State = toml::from_str(&serialized).unwrap();
        let frame = parsed.window.unwrap();
        assert_eq!((frame.x, frame.y, frame.width, frame.height), (-12, 40, 1280, 800));
        assert!(parsed.file_tree_visible);
        assert_eq!(parsed.file_tree_width, 320.0);
    }

    #[test]
    fn missing_or_garbage_state_parses_to_default() {
        let parsed: State = toml::from_str("").unwrap();
        assert!(parsed.window.is_none());
        assert!(!parsed.file_tree_visible, "the sidebar starts hidden");
        assert_eq!(parsed.file_tree_width, 0.0, "zero means the default width");
        assert!(toml::from_str::<State>("not toml at all [").is_err());
    }
}
