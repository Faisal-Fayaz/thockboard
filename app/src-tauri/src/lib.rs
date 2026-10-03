mod audio;
mod packs;
mod slices_generated;
mod rules;
mod settings;
mod tray;

use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait};
use audio::{Gesture, KeySlot, Kind, Pack, PackSet, Shared, Voice};
use serde::Serialize;
use settings::Settings;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

const REPEAT_FILTER: Duration = Duration::from_millis(35);
const BTN_REPEAT_FILTER: Duration = Duration::from_millis(20);
const SCROLL_FILTER: Duration = Duration::from_millis(40);

/// Time-based re-arm for the hotkey, standing in for the KeyRelease we never get
/// on X11. Comfortably longer than the OS auto-repeat interval.
const HOTKEY_DEBOUNCE: Duration = Duration::from_millis(500);

/// Mirror of a strike, for the UI activity feed. Never on the audio path.
#[derive(Clone, Serialize)]
struct Strike {
    kind: &'static str,
    key: String,
    gesture: &'static str,
}

struct App {
    shared: Arc<Shared>,
    set: Arc<ArcSwap<PackSet>>,
    config_dir: PathBuf,
    sample_rate: u32,
    strikes: AtomicU64,
    menu: tray::MenuRef,
    tx: crossbeam_channel::Sender<audio::Event>,
    rules: std::sync::RwLock<rules::RuleSet>,
}

impl App {
    fn persist(&self) {
        settings::save(
            &self.config_dir,
            &Settings {
                pack: self.shared.pack.load(Ordering::Relaxed),
                voice: self.shared.voice.load(Ordering::Relaxed),
                volume: f32::from_bits(self.shared.volume.load(Ordering::Relaxed)),
                enabled: self.shared.enabled.load(Ordering::Relaxed),
                release_sound: self.shared.release_sound.load(Ordering::Relaxed),
                scroll_sound: self.shared.scroll_sound.load(Ordering::Relaxed),
            },
        );
    }

    fn pack_count(&self) -> usize {
        self.set.load().packs.len()
    }
}

/// Modifier names exactly as rdev spells them. Guessing these silently
/// misclassifies every modifier.
const MODIFIERS: &[&str] = &[
    "ShiftLeft",
    "ShiftRight",
    "ControlLeft",
    "ControlRight",
    "Alt",
    "AltGr",
    "MetaLeft",
    "MetaRight",
];

/// Sticky modifier state for the mute hotkey.
///
/// Deliberately sticky: rdev's X11 backend delivers no `KeyRelease` at all
/// (measured: 75 events, zero releases), so tracking press/release pairs can
/// never re-arm. Windows and macOS do deliver releases, and clearing on release
/// there is still correct.
#[derive(Default)]
struct Mods {
    ctrl: bool,
    alt: bool,
}

impl Mods {
    fn press(&mut self, key: &str) {
        match key {
            "ControlLeft" | "ControlRight" => self.ctrl = true,
            "Alt" | "AltGr" => self.alt = true,
            _ => {}
        }
    }

    fn clear(&mut self) {
        self.ctrl = false;
        self.alt = false;
    }

    /// Ctrl+Alt, excluding Shift and Meta so the chord cannot collide with a
    /// desktop shortcut.
    fn is_chord(&self) -> bool {
        self.ctrl && self.alt
    }
}

/// Normalises an rdev key name: `KeyA` -> `A`, `ShiftLeft` -> `ShiftLeft`.
///
/// rdev reports US-layout names regardless of the user's actual layout. Only
/// used for classification and UI labels, never to produce a character.
fn normalise(name: &str) -> String {
    name.strip_prefix("Key")
        .map(|s| s.to_string())
        .unwrap_or_else(|| name.to_string())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PackInfo {
    id: String,
    name: String,
    origin: &'static str,
    voices: Vec<String>,
    /// One peak envelope per voice for the UI waveform thumbnails.
    peaks: Vec<Vec<f32>>,
}

/// One thumbnail worth of peaks, sampled from the rendered buffers rather than
/// the recipe, so what you see is what you hear.
fn peaks_for(pack: &Pack, voice: usize, buckets: usize) -> Vec<f32> {
    let Some(bank) = pack.voices.get(voice) else {
        return vec![0.0; buckets];
    };
    let Some(buf) = bank.get(Gesture::Press, 0, KeySlot::Normal) else {
        return vec![0.0; buckets];
    };
    let per = (buf.len() / buckets).max(1);
    (0..buckets)
        .map(|b| {
            let start = b * per;
            let end = (start + per).min(buf.len());
            if start >= end {
                return 0.0;
            }
            buf[start..end].iter().fold(0.0f32, |m, s| m.max(s.abs()))
        })
        .map(|v| (v * 1.08).min(1.0))
        .collect()
}

#[tauri::command]
fn list_packs(state: State<App>) -> Vec<PackInfo> {
    let set = state.set.load();
    set.packs
        .iter()
        .map(|p| PackInfo {
            id: p.id.clone(),
            name: p.name.clone(),
            origin: p.origin.label(),
            voices: (0..p.voices.len()).map(|i| format!("Sound {}", i + 1)).collect(),
            peaks: (0..p.voices.len())
                .map(|v| peaks_for(p, v, 48))
                .collect(),
        })
        .collect()
}

#[tauri::command]
fn set_pack(state: State<App>, index: usize) {
    tray::apply_pack(&state, index);
    // Voices are per pack, so a pack change resets the voice index.
    state.shared.voice.store(0, Ordering::Relaxed);
    state.persist();
}

fn set_voice_inner(state: &App, index: usize) {
    let pack = state.shared.pack.load(Ordering::Relaxed);
    let len = state
        .set
        .load()
        .packs
        .get(pack)
        .map(|p| p.voices.len())
        .unwrap_or(1);
    state
        .shared
        .voice
        .store(index.min(len.saturating_sub(1)), Ordering::Relaxed);
    state.persist();
}

#[tauri::command]
fn set_voice(state: State<App>, index: usize) {
    set_voice_inner(&state, index);
}

fn send(state: &App, kind: Kind, gesture: Gesture) {
    let _ = state.tx.try_send(audio::Event { kind, gesture });
}

#[tauri::command]
fn preview(state: State<App>, index: Option<usize>) {
    if let Some(i) = index {
        set_voice_inner(&state, i);
    }
    send(&state, Kind::Key(KeySlot::Normal), Gesture::Press);
    send(&state, Kind::Key(KeySlot::Normal), Gesture::Release);
}

/// Applies the four perceptual axes to a base voice, renders it, plays it, and
/// returns peaks for the UI so the preview shows exactly what is heard.
#[tauri::command]
fn designer_preview(
    state: State<App>,
    pack: usize,
    voice: usize,
    pitch: f32,
    attack: f32,
    resonance: f32,
    loudness: f32,
) -> serde_json::Value {
    let Some(base) = builtin_base(pack, voice) else {
        return serde_json::json!({ "ok": false, "error": "no such base sound" });
    };
    let shaped = packs::morph(&base, pitch, attack, resonance, loudness);
    let buf = audio::render(&shaped, state.sample_rate, 1.0, 1.0);
    let peaks = peaks_of(&buf, 48);

    // Preview without disturbing the saved selection: the buffer carries itself.
    let _ = state.tx.try_send(audio::Event {
        kind: audio::Kind::OneShot(Arc::from(buf.into_boxed_slice())),
        gesture: Gesture::Press,
    });

    serde_json::json!({ "ok": true, "peaks": peaks })
}

/// Samples a buffer into `buckets` normalised peaks for the UI.
fn peaks_of(buf: &[f32], buckets: usize) -> Vec<f32> {
    let per = (buf.len() / buckets).max(1);
    (0..buckets)
        .map(|b| {
            let s = b * per;
            let e = (s + per).min(buf.len());
            if s >= e {
                0.0
            } else {
                (buf[s..e].iter().fold(0.0f32, |m, v| m.max(v.abs())) * 1.08).min(1.0)
            }
        })
        .collect()
}

/// The recipe a built-in voice came from, for use as a designer base.
fn builtin_base(pack: usize, voice: usize) -> Option<Voice> {
    audio::builtin_families()
        .get(pack)?
        .voices
        .get(voice)
        .cloned()
}

/// Saves the shaped voice as a new recipe pack and reloads the library.
#[tauri::command]
fn designer_save(
    state: State<App>,
    name: String,
    pack: usize,
    voice: usize,
    pitch: f32,
    attack: f32,
    resonance: f32,
    loudness: f32,
) -> serde_json::Value {
    let Some(base) = builtin_base(pack, voice) else {
        return serde_json::json!({ "ok": false, "error": "no such base sound" });
    };
    let mut shaped = packs::morph(&base, pitch, attack, resonance, loudness);
    shaped.name = name.clone();
    let id = format!("custom-{}", name.to_lowercase().replace(' ', "-"));

    match packs::save_recipe(&state.config_dir, &id, &name, &[shaped]) {
        Ok(path) => {
            println!("[designer] saved {}", path.display());
            reload(&state);
            serde_json::json!({ "ok": true })
        }
        Err(e) => {
            eprintln!("[designer] save failed: {e}");
            serde_json::json!({ "ok": false, "error": e.to_string() })
        }
    }
}

/// Rebuilds the whole pack set from disk and swaps it in atomically.
fn reload(state: &App) {
    let fresh = Arc::new(packs::load_all(&state.config_dir, state.sample_rate));
    let count = fresh.packs.len();
    state.set.store(fresh);
    println!("[packs] reloaded, {count} packs available");
}

#[tauri::command]
fn reload_packs(state: State<App>) -> usize {
    reload(&state);
    state.pack_count()
}

#[tauri::command]
fn open_packs_folder(state: State<App>) {
    let dir = state.config_dir.join("packs");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&dir).spawn();
}

#[tauri::command]
fn set_volume(state: State<App>, pct: f32) {
    audio::store_gain(&state.shared.volume, pct);
    state.persist();
}

#[tauri::command]
fn set_enabled(state: State<App>, on: bool) {
    tray::apply_mute(&state, !on);
}

#[tauri::command]
fn set_release_sound(state: State<App>, on: bool) {
    tray::apply_release(&state, on);
    state.persist();
}

#[tauri::command]
fn set_scroll_sound(state: State<App>, on: bool) {
    tray::apply_scroll(&state, on);
    state.persist();
}

#[tauri::command]
fn list_rules(state: State<App>) -> serde_json::Value {
    state
        .rules
        .read()
        .ok()
        .and_then(|g| serde_json::to_value(&*g).ok())
        .unwrap_or(serde_json::Value::Null)
}

#[tauri::command]
fn save_rules(state: State<App>, rules: serde_json::Value) {
    if let Ok(r) = serde_json::from_value::<rules::RuleSet>(rules) {
        if let Ok(mut guard) = state.rules.write() {
            *guard = r;
            rules::persist(&state.config_dir, &guard);
        }
    }
}

#[tauri::command]
fn status(state: State<App>) -> serde_json::Value {
    serde_json::json!({
        "sampleRate": state.sample_rate,
        "volume": f32::from_bits(state.shared.volume.load(Ordering::Relaxed)),
        "enabled": state.shared.enabled.load(Ordering::Relaxed),
        "pack": state.shared.pack.load(Ordering::Relaxed),
        "voice": state.shared.voice.load(Ordering::Relaxed),
        "releaseSound": state.shared.release_sound.load(Ordering::Relaxed),
        "scrollSound": state.shared.scroll_sound.load(Ordering::Relaxed),
        "strikes": state.strikes.load(Ordering::Relaxed),
        "packs": state.pack_count(),
    })
}

/// Fires synthetic strikes through XTest so the hook and audio path can be
/// verified without a person at the keyboard.
#[tauri::command]
fn simulate_burst(count: usize) {
    std::thread::spawn(move || {
        let keys = [
            rdev::Key::KeyH,
            rdev::Key::KeyI,
            rdev::Key::Space,
            rdev::Key::ShiftLeft,
        ];
        for i in 0..count.clamp(1, 32) {
            let key = keys[i % keys.len()];
            let _ = rdev::simulate(&rdev::EventType::KeyPress(key));
            std::thread::sleep(Duration::from_millis(90));
            let _ = rdev::simulate(&rdev::EventType::KeyRelease(key));
            std::thread::sleep(Duration::from_millis(40));
        }
    });
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

/// The hook. Runs on the OS input thread, so it does the minimum possible:
/// classify, hand off, return.
fn spawn_hook(app: AppHandle, tx: crossbeam_channel::Sender<audio::Event>) {
    std::thread::spawn(move || {
        // Read once. `env::var_os` allocates, and this callback runs on the OS
        // input thread where an allocation per keystroke would be felt.
        let debug = std::env::var_os("THOCKBOARD_DEBUG").is_some();
        let mut last_key: Option<Instant> = None;
        let mut last_btn: Option<Instant> = None;
        let mut last_scroll: Option<Instant> = None;
        let mut mods = Mods::default();
        let mut last_hotkey: Option<Instant> = None;

        // Surface the result: a silently dead hook looks like a quiet keyboard.
        if let Err(e) = rdev::listen(move |event| {
            let raw = match &event.event_type {
                rdev::EventType::KeyPress(k) => format!("{k:?}"),
                rdev::EventType::KeyRelease(k) => format!("{k:?}"),
                rdev::EventType::ButtonPress(b) => {
                    let now = Instant::now();
                    if let Some(prev) = last_btn {
                        if now.duration_since(prev) < BTN_REPEAT_FILTER {
                            return;
                        }
                    }
                    last_btn = Some(now);
                    let _ = tx.try_send(audio::Event {
                        kind: Kind::Click,
                        gesture: Gesture::Press,
                    });
                    let _ = app.emit(
                        "strike",
                        Strike {
                            kind: "click",
                            key: format!("{b:?}"),
                            gesture: "press",
                        },
                    );
                    return;
                }
                rdev::EventType::Wheel { delta_y, .. } => {
                    // Trackpads send continuous high-resolution deltas; only a
                    // change of direction counts as a deliberate tick.
                    let now = Instant::now();
                    if let Some(prev) = last_scroll {
                        if now.duration_since(prev) < SCROLL_FILTER {
                            return;
                        }
                    }
                    last_scroll = Some(now);
                    let _ = tx.try_send(audio::Event {
                        kind: Kind::Scroll,
                        gesture: if *delta_y > 0 {
                            Gesture::Press
                        } else {
                            Gesture::Release
                        },
                    });
                    return;
                }
                _ => return,
            };
            let key = normalise(&raw);
            let down = matches!(event.event_type, rdev::EventType::KeyPress(_));
            let gesture = if down {
                Gesture::Press
            } else {
                Gesture::Release
            };

            if debug {
                eprintln!("[ev] down={down} key={key} chord={}", mods.is_chord());
            }

            if !down {
                mods.clear();
            } else {
                mods.press(&key);
            }

            if down && !MODIFIERS.contains(&key.as_str()) {
                if key == "M" && mods.is_chord() {
                    let now = Instant::now();
                    let armed = last_hotkey
                        .map_or(true, |t| now.duration_since(t) > HOTKEY_DEBOUNCE);
                    if armed {
                        last_hotkey = Some(now);
                        let handle = app.clone();
                        // Off the input thread: menu updates dispatch to the main
                        // thread and would stall typing if done here.
                        std::thread::spawn(move || tray::toggle_mute(&handle));
                    }
                    mods.clear();
                    return;
                }
                mods.clear();
            }

            if down {
                let now = Instant::now();
                if let Some(prev) = last_key {
                    if now.duration_since(prev) < REPEAT_FILTER {
                        return;
                    }
                }
                last_key = Some(now);
            }

            let _ = tx.try_send(audio::Event {
                kind: Kind::Key(KeySlot::classify(&key)),
                gesture,
            });

            if down {
                if let Some(state) = app.try_state::<App>() {
                    let n = state.strikes.fetch_add(1, Ordering::Relaxed) + 1;
                    if n <= 20 || n % 50 == 0 {
                        println!(
                            "[strike] {key} (total {n}, pack {}, voice {})",
                            state.shared.pack.load(Ordering::Relaxed),
                            state.shared.voice.load(Ordering::Relaxed),
                        );
                    }
                }
            }
            let _ = app.emit(
                "strike",
                Strike {
                    kind: "key",
                    key,
                    gesture: if down { "press" } else { "release" },
                },
            );
        }) {
            eprintln!("[hook] listener stopped: {e:?}");
            println!("[hook] FAILED - input will not register");
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be first: a second instance would double every sound.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::toggle_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            let handle = app.handle().clone();
            let config_dir = app
                .path()
                .app_config_dir()
                .map_err(|e| format!("no config dir: {e}"))?;
            let saved = settings::load(&config_dir);

            let shared = Arc::new(Shared {
                volume: Default::default(),
                enabled: std::sync::atomic::AtomicBool::new(saved.enabled),
                pack: std::sync::atomic::AtomicUsize::new(saved.pack),
                voice: std::sync::atomic::AtomicUsize::new(saved.voice),
                release_sound: std::sync::atomic::AtomicBool::new(saved.release_sound),
                scroll_sound: std::sync::atomic::AtomicBool::new(saved.scroll_sound),
                rule_scale: Default::default(),
            });
            audio::store_gain(&shared.volume, saved.volume);
            shared.rule_scale.store(1.0f32.to_bits(), Ordering::Relaxed);

            // Probe the device rate first: every buffer is rendered at it.
            let rate = {
                let host = cpal::default_host();
                host.default_output_device()
                    .and_then(|d| d.default_output_config().ok())
                    .map(|c| c.sample_rate().0)
                    .unwrap_or(44_100)
            };

            let set = Arc::new(ArcSwap::from_pointee(packs::load_all(&config_dir, rate)));
            println!("[packs] {} loaded", set.load().packs.len());

            let (tx, rx) = crossbeam_channel::bounded::<audio::Event>(256);
            let sample_rate = audio::start(shared.clone(), set.clone(), rx)
                .map_err(std::io::Error::other)?;
            println!("[audio] running at {sample_rate} Hz");

            let menu = tray::build(&handle, &set.load().packs, &saved)?;
            let rules = std::sync::RwLock::new(rules::load(&config_dir));

            app.manage(App {
                shared,
                set,
                config_dir,
                sample_rate,
                strikes: AtomicU64::new(0),
                menu,
                tx: tx.clone(),
                rules,
            });
            spawn_hook(handle.clone(), tx);

            // Watch the focused window so per-app rules can take effect.
            rules::spawn_watcher(handle.clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Background utility: closing the window hides it. Quit lives in the
            // tray, which is the discoverable way to actually exit.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_packs,
            set_pack,
            set_voice,
            preview,
            designer_preview,
            designer_save,
            reload_packs,
            open_packs_folder,
            set_volume,
            set_enabled,
            set_release_sound,
            set_scroll_sound,
            list_rules,
            save_rules,
            status,
            simulate_burst,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
