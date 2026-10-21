//! Preferences remembered between runs, in `state.json` under the data directory.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// Whether files are shown in full by default, rather than only their changes.
    #[serde(default)]
    show_full_files: bool,
}

fn path() -> std::path::PathBuf {
    crate::comments::data_dir().join("state.json")
}

fn load() -> State {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn show_full_files() -> bool {
    load().show_full_files
}

pub fn set_show_full_files(value: bool) {
    let mut state = load();
    if state.show_full_files == value {
        return;
    }
    state.show_full_files = value;
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(&state) {
        let _ = std::fs::write(path, json);
    }
}
