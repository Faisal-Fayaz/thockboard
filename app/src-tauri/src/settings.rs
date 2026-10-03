//! Settings persisted to the app config directory.
//!
//! Deliberately a plain JSON file rather than a plugin: it is a handful of
//! fields, and a dependency for that would cost more than it saves.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Settings {
    pub pack: usize,
    pub voice: usize,
    pub volume: f32,
    pub enabled: bool,
    pub release_sound: bool,
    pub scroll_sound: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            pack: 0,
            voice: 0,
            volume: 70.0,
            enabled: true,
            release_sound: false,
            scroll_sound: true,
        }
    }
}

fn path(dir: &PathBuf) -> PathBuf {
    dir.join("settings.json")
}

/// Reads settings, falling back to defaults. A corrupt file is treated as
/// absent rather than fatal: losing preferences must not stop the app starting.
pub fn load(dir: &PathBuf) -> Settings {
    let p = path(dir);
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Settings::default();
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[settings] ignoring unreadable {}: {e}", p.display());
            Settings::default()
        }
    }
}

pub fn save(dir: &PathBuf, s: &Settings) {
    let p = path(dir);
    if let Some(parent) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("[settings] cannot create {}: {e}", parent.display());
            return;
        }
    }
    if let Err(e) = std::fs::write(&p, serde_json::to_string_pretty(s).unwrap_or_default()) {
        eprintln!("[settings] cannot write {}: {e}", p.display());
    }
}
