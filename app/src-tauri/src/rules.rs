//! Per-application sound rules.
//!
//! Lets a user pick a different sound per app, or mute entirely inside a game
//! or a video call. Only two competitors have this and both are poor, so it is
//! cheap differentiation.
//!
//! The focused window is polled on a timer rather than sampled per keystroke:
//! asking the window manager for the active window on the input thread would add
//! latency to typing, which is the one thing that must never happen.
//!
//! Linux/X11 only for now, via the EWMH `_NET_ACTIVE_WINDOW` property. Windows
//! and macOS need their own platform calls and are untested.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Case-insensitive substring matched against the focused window's class.
    pub pattern: String,
    pub action: Action,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Use the default pack.
    Default,
    /// Sound normally, but quieter.
    Quiet,
    /// No sound at all.
    Mute,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct RuleSet {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// First matching rule wins, so the list reads top to bottom.
    pub fn match_app(&self, app: &str) -> Option<Action> {
        let app = app.to_lowercase();
        self.rules
            .iter()
            .find(|r| !r.pattern.trim().is_empty() && app.contains(&r.pattern.to_lowercase()))
            .map(|r| r.action)
    }
}

fn path(config_dir: &Path) -> PathBuf {
    config_dir.join("rules.json")
}

pub fn load(config_dir: &Path) -> RuleSet {
    let p = path(config_dir);
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str::<RuleSet>(&t).ok())
        .unwrap_or_default()
}

pub fn persist(config_dir: &Path, rules: &RuleSet) {
    let p = path(config_dir);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(rules) {
        let _ = std::fs::write(&p, json);
    }
}

/// Which application owns the focused window.
///
/// One-shot: opens a short-lived X connection. Cheap enough at the ~4 Hz the
/// watcher runs at, and avoids holding a display handle across threads.
#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub fn focused_app() -> Option<String> {
    use std::ffi::CString;
    use x11::xlib;
    use x11::xlib::XA_WINDOW;

    // SAFETY: every pointer below is either null-checked or a value Xlib filled
    // in for us; `Display` is closed on all paths out.
    unsafe {
        let dpy = xlib::XOpenDisplay(std::ptr::null());
        if dpy.is_null() {
            return None;
        }

        let atom_name = CString::new("_NET_ACTIVE_WINDOW").ok();
        let prop = atom_name
            .as_ref()
            .map(|n| xlib::XInternAtom(dpy, n.as_ptr(), 0))
            .unwrap_or(0);
        if prop == 0 {
            xlib::XCloseDisplay(dpy);
            return None;
        }

        let root = xlib::XDefaultRootWindow(dpy);
        let mut typ: std::os::raw::c_ulong = 0;
        let mut fmt: std::os::raw::c_int = 0;
        let mut nitems: std::os::raw::c_ulong = 0;
        let mut remain: std::os::raw::c_ulong = 0;
        let mut data: *mut std::os::raw::c_uchar = std::ptr::null_mut();

        let rc = xlib::XGetWindowProperty(
            dpy,
            root,
            prop,
            0,
            1,
            0,
            XA_WINDOW,
            &mut typ,
            &mut fmt,
            &mut nitems,
            &mut remain,
            &mut data,
        );

        let mut out = None;
        if rc == 0 && !data.is_null() && nitems > 0 {
            // The property is a single WINDOW id, which Xlib returns as bytes.
            let window = std::os::raw::c_ulong::from_ne_bytes([
                *data, *data.add(1), *data.add(2), *data.add(3),
                *data.add(4), *data.add(5), *data.add(6), *data.add(7),
            ]);
            let mut hint: xlib::XClassHint = std::mem::zeroed();
            let got = xlib::XGetClassHint(dpy, window, &mut hint);
            if got != 0 && !hint.res_class.is_null() {
                out = Some(
                    std::ffi::CStr::from_ptr(hint.res_class)
                        .to_string_lossy()
                        .to_string(),
                );
                if !hint.res_name.is_null() {
                    xlib::XFree(hint.res_name as *mut std::os::raw::c_void);
                }
                xlib::XFree(hint.res_class as *mut std::os::raw::c_void);
            }
        }
        if !data.is_null() {
            xlib::XFree(data as *mut std::os::raw::c_void);
        }
        xlib::XCloseDisplay(dpy);
        out
    }
}

#[cfg(not(all(target_os = "linux", not(target_os = "android"))))]
pub fn focused_app() -> Option<String> {
    None
}

/// Polls the focused window, applies any matching rule, and emits `app-focus`
/// when it changes.
///
/// This runs off the input thread deliberately: the rule result is published to
/// the audio path through an atomic, so nothing here can add latency to typing.
pub fn spawn_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last: Option<String> = None;
        loop {
            std::thread::sleep(Duration::from_millis(250));

            let current = focused_app();
            if current != last {
                last = current.clone();
                apply(&app, current.as_deref());
                let _ = app.emit("app-focus", current);
            }
        }
    });
}

/// Publishes the gain a matching rule implies for the focused app.
fn apply(app: &AppHandle, focused: Option<&str>) {
    // The managed state is bound to a local so the read guard cannot outlive it.
    let state = app.try_state::<crate::App>();
    let action = focused
        .and_then(|f| {
            state
                .as_ref()
                .and_then(|s| s.rules.read().ok())
                .and_then(|g| g.match_app(f))
        })
        .map(|a| match a {
            Action::Default => 1.0,
            Action::Quiet => 0.35,
            Action::Mute => 0.0,
        })
        .unwrap_or(1.0);

    if let Some(st) = state {
        st.shared.rule_scale.store(scale_bits(action), Ordering::Relaxed);
    }
}

fn scale_bits(v: f32) -> u32 {
    v.to_bits()
}
