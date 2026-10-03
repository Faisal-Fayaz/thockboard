//! System tray, window lifecycle and mute handling.
//!
//! The tray is the primary surface: this is a background utility and the window
//! is secondary. Closing the window hides it rather than exiting.

use crate::App;
use crate::audio::{Origin, Pack};
use crate::settings::Settings;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder};
use tauri::{AppHandle, Manager, Wry};

/// Menu items we need handles to, so their check state can be synced when mute,
/// sound, autostart or release change from somewhere other than the tray.
pub struct MenuState {
    pub mute: CheckMenuItem<Wry>,
    pub autostart: CheckMenuItem<Wry>,
    pub release: CheckMenuItem<Wry>,
    pub scroll: CheckMenuItem<Wry>,
    /// One per pack in the library, builtin and imported alike.
    pub packs: Vec<CheckMenuItem<Wry>>,
    /// Held for the process lifetime: a dropped tray icon disappears.
    pub _tray: tauri::tray::TrayIcon<Wry>,
}

pub type MenuRef = Arc<Mutex<MenuState>>;

/// Recovers from poisoning: a panic while holding the menu lock must not take
/// the tray down with it.
fn lock(m: &MenuRef) -> MutexGuard<'_, MenuState> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn build(app: &AppHandle, packs: &[Pack], saved: &Settings) -> tauri::Result<MenuRef> {
    let open = MenuItem::with_id(app, "open", "Open ThockBoard", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let mute = CheckMenuItem::with_id(
        app,
        "mute",
        "Mute  (Ctrl+Alt+M)",
        true,
        !saved.enabled,
        None::<&str>,
    )?;
    let release = CheckMenuItem::with_id(
        app,
        "release",
        "Key release sound",
        true,
        saved.release_sound,
        None::<&str>,
    )?;
    let scroll = CheckMenuItem::with_id(
        app,
        "scroll",
        "Scroll wheel tick",
        true,
        saved.scroll_sound,
        None::<&str>,
    )?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Launch at startup",
        true,
        is_autostart_enabled(app),
        None::<&str>,
    )?;

    // One check item per pack. Imported packs are labelled so their origin is
    // obvious in a list that also contains the built-ins.
    let sound = Submenu::with_id(app, "sounds", "Sound", true)?;
    let mut pack_rows: Vec<CheckMenuItem<Wry>> = Vec::with_capacity(packs.len());
    for (pi, p) in packs.iter().enumerate() {
        let label = match p.origin {
            Origin::Builtin => p.name.clone(),
            other => format!("{}  ({})", p.name, other.label()),
        };
        let it = CheckMenuItem::with_id(
            app,
            format!("pack:{pi}"),
            &label,
            true,
            pi == saved.pack,
            None::<&str>,
        )?;
        sound.append(&it)?;
        pack_rows.push(it);
    }

    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&open, &sound, &mute, &release, &scroll, &autostart, &sep, &quit],
    )?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

    let tray = TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("ThockBoard")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(|tray, event| {
            // Left click toggles visibility: the common "where did it go" action.
            if let tauri::tray::TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(Arc::new(Mutex::new(MenuState {
        mute,
        autostart,
        release,
        scroll,
        packs: pack_rows,
        _tray: tray,
    })))
}

fn is_autostart_enabled(app: &AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

pub fn toggle_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        match win.is_visible() {
            Ok(true) => {
                let _ = win.hide();
            }
            _ => {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }
    }
}

/// Applies mute everywhere: atomic, persisted settings, and the tray checkbox.
/// Single source of truth for the muted flag.
pub fn apply_mute(state: &App, muted: bool) {
    state.shared.enabled.store(!muted, Ordering::Relaxed);
    state.persist();

    // Clone the handle and drop the lock before calling: `set_checked` dispatches
    // to the main thread and blocks, which would deadlock holding the mutex.
    let item = lock(&state.menu).mute.clone();
    let _ = item.set_checked(muted);
}

pub fn is_muted(state: &App) -> bool {
    lock(&state.menu).mute.is_checked().unwrap_or(false)
}

/// Inverts mute. Shared by the tray item and the global hotkey so the two can
/// never disagree.
pub fn toggle_mute(app: &AppHandle) {
    let state = app.state::<App>();
    apply_mute(&state, !is_muted(&state));
}

/// Selects a pack, syncing the tray checkboxes.
pub fn apply_pack(state: &App, idx: usize) {
    let idx = idx.min(state.pack_count().saturating_sub(1));
    state.shared.pack.store(idx, Ordering::Relaxed);
    state.persist();

    let items = lock(&state.menu).packs.clone();
    for (i, it) in items.iter().enumerate() {
        let _ = it.set_checked(i == idx);
    }
}

pub fn apply_release(state: &App, on: bool) {
    state.shared.release_sound.store(on, Ordering::Relaxed);
    let item = lock(&state.menu).release.clone();
    let _ = item.set_checked(on);
}

pub fn apply_scroll(state: &App, on: bool) {
    state.shared.scroll_sound.store(on, Ordering::Relaxed);
    let item = lock(&state.menu).scroll.clone();
    let _ = item.set_checked(on);
}

fn set_autostart(app: &AppHandle, desired: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let res = if desired {
        app.autolaunch().enable()
    } else {
        app.autolaunch().disable()
    };
    match res {
        Ok(()) => {
            let state = app.state::<App>();
            let item = lock(&state.menu).autostart.clone();
            let _ = item.set_checked(desired);
        }
        Err(e) => eprintln!("[autostart] {e}"),
    }
}

fn on_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    let id = event.id().0.clone();
    let state = app.state::<App>();
    match id.as_str() {
        "open" => toggle_window(app),
        "quit" => app.exit(0),
        "mute" => toggle_mute(app),
        "release" => {
            let cur = state.shared.release_sound.load(Ordering::Relaxed);
            apply_release(&state, !cur);
            state.persist();
        }
        "scroll" => {
            let cur = state.shared.scroll_sound.load(Ordering::Relaxed);
            apply_scroll(&state, !cur);
            state.persist();
        }
        "autostart" => {
            let desired = !lock(&state.menu).autostart.is_checked().unwrap_or(false);
            set_autostart(app, desired);
        }
        other if other.starts_with("pack:") => {
            if let Ok(idx) = other[5..].parse::<usize>() {
                apply_pack(&state, idx);
            }
        }
        _ => {}
    }
}
