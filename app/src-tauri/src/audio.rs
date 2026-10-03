//! Audio engine: sound synthesis, pack playback and the real-time thread.
//!
//! # Rules for the audio callback
//!
//! It runs on the audio thread, so: no locks, no allocation on the steady-state
//! path, no filesystem, no logging. Everything it needs is either pre-rendered
//! behind an `Arc` or read from an atomic.
//!
//! # Synthesis
//!
//! Recipe sounds are built by modal synthesis: a bank of damped sinusoids at the
//! resonant frequencies a real switch, plate and case produce, plus a short
//! filtered-noise impact, a raised-cosine attack and a few early-reflection taps.
//! A plain sine plus filtered noise cannot produce the difference between a thock
//! and a clack, because those sounds *are* different resonant mode shapes.
//!
//! Imported sample packs are decoded, resampled, trimmed and normalised into the
//! same buffer structure, so playback has exactly one path regardless of origin.
//!
//! # Streaming
//!
//! Sounds are longer than one audio buffer, so playback keeps a cursor and spans
//! multiple callbacks. An earlier version truncated every sound to a single buffer
//! (5.8 ms), which silently threw away the decay that carries the character.

use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Per-strike pitch and level variation, in percent.
///
/// Without this, every identical keystroke sounds machine-gunned and obviously
/// synthetic. Baked into separate buffers at startup so it costs nothing at
/// playback time.
pub const VARIANTS: usize = 8;
const PITCH_JITTER: f32 = 0.028;
const GAIN_JITTER: f32 = 0.10;

/// Which key was struck, for sounds that are not uniform across the board.
///
/// Real keyboards are not uniform: the spacebar and enter are deeper, modifiers
/// are lighter. Four slots is a deliberate compromise against memory — backspace
/// and friends reuse the default, which is what most imported packs do anyway.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeySlot {
    Normal,
    Space,
    Enter,
    Modifier,
}

pub const SLOTS: usize = 4;

impl KeySlot {
    pub fn index(self) -> usize {
        match self {
            KeySlot::Normal => 0,
            KeySlot::Space => 1,
            KeySlot::Enter => 2,
            KeySlot::Modifier => 3,
        }
    }

    /// Classifies an rdev key name. Layout names only, never characters.
    pub fn classify(name: &str) -> Self {
        match name {
            "Space" => KeySlot::Space,
            "Enter" | "KeyEnter" | "NumpadEnter" => KeySlot::Enter,
            "ShiftLeft" | "ShiftRight" | "ControlLeft" | "ControlRight" | "Alt"
            | "AltGr" | "MetaLeft" | "MetaRight" | "CapsLock" | "Tab" | "Backspace" => {
                KeySlot::Modifier
            }
            _ => KeySlot::Normal,
        }
    }
}

/// A damped sinusoid: one resonant mode.
#[derive(Clone, Serialize, Deserialize)]
pub struct Mode {
    pub freq_hz: f32,
    pub amp: f32,
    pub decay_s: f32,
}

/// The short filtered-noise transient that gives a strike its edge.
#[derive(Clone, Serialize, Deserialize)]
pub struct Impact {
    pub amp: f32,
    pub decay_s: f32,
    pub lowpass_hz: f32,
}

/// Release feel relative to press: `[gain, pitch, decay]` multipliers.
#[derive(Clone, Serialize, Deserialize)]
pub struct Release {
    pub gain: f32,
    pub pitch: f32,
    pub decay: f32,
}

impl Default for Release {
    fn default() -> Self {
        Self {
            gain: 0.38,
            pitch: 1.25,
            decay: 0.5,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Voice {
    pub name: String,
    pub modes: Vec<Mode>,
    pub impact: Impact,
    /// Raised-cosine attack length. Rounds off the harshest edge while keeping
    /// the transient defined; a hard start reads as synthetic.
    #[serde(default)]
    pub attack_ms: f32,
    #[serde(default)]
    pub room: Vec<(f32, f32)>,
    #[serde(default)]
    pub release: Release,
    /// Target peak for this voice. `render` normalises to it, which is the only
    /// way a designer's loudness control can survive synthesis.
    #[serde(default = "default_peak")]
    pub peak: f32,
}

fn default_peak() -> f32 {
    0.92
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Family {
    pub id: String,
    pub name: String,
    pub voices: Vec<Voice>,
}

#[derive(Deserialize)]
struct SoundFile {
    #[serde(default)]
    families: Vec<Family>,
    /// Non-keyboard sounds, rendered once for the whole app rather than per pack.
    #[serde(default)]
    ui: Vec<Voice>,
}

/// A decoded sample bank: raw mono f32 at the source file's own sample rate,
/// indexed `[gesture][variant][slot]`.
pub struct RawBank {
    pub rate: u32,
    pub key: Vec<Vec<Vec<Vec<f32>>>>,
    pub click: Vec<Vec<Vec<f32>>>,
}

impl RawBank {
    pub fn empty() -> Self {
        Self {
            rate: 48_000,
            key: Vec::new(),
            click: Vec::new(),
        }
    }
}

/// One voice, pre-rendered for every variant, key slot and gesture.
///
/// `Arc<[f32]>` rather than `Vec<f32>` so playback can hold the buffer across
/// many callbacks without borrowing from the swappable pack set.
#[derive(Default)]
pub struct VoiceBank {
    /// `[gesture][variant][slot]`
    pub key: Vec<Vec<Vec<Arc<[f32]>>>>,
    /// `[gesture][variant]`
    pub click: Vec<Vec<Arc<[f32]>>>,
}

impl VoiceBank {
    pub fn get(&self, gesture: Gesture, variant: usize, slot: KeySlot) -> Option<Arc<[f32]>> {
        let g = if gesture == Gesture::Press { 0 } else { 1 };
        let variants = self.key.get(g)?;
        if variants.is_empty() {
            return None;
        }
        let variants = self.key.get(g)?;
        if variants.is_empty() {
            return None;
        }
        let per_slot = variants.get(variant % variants.len())?;
        let buf = per_slot.get(slot.index())?;
        Some(buf.clone())
    }

    pub fn get_click(&self, gesture: Gesture, variant: usize) -> Option<Arc<[f32]>> {
        let g = if gesture == Gesture::Press { 0 } else { 1 };
        let row = self.click.get(g)?;
        if row.is_empty() {
            return None;
        }
        Some(row.get(variant % row.len())?.clone())
    }
}

pub struct Pack {
    pub id: String,
    pub name: String,
    pub origin: Origin,
    pub voices: Vec<VoiceBank>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Builtin,
    Recipe,
    Mechvibes,
    Samples,
    /// Real recorded keystrokes, sliced from CC0 board recordings at build time.
    Recorded,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Builtin => "built in",
            Origin::Recipe => "recipe",
            Origin::Mechvibes => "mechvibes",
            Origin::Samples => "samples",
            Origin::Recorded => "recorded",
        }
    }
}

/// Non-keyboard sounds shared across every pack.
#[derive(Default)]
pub struct UiBank {
    pub scroll: Vec<Arc<[f32]>>,
}

/// The complete, swappable set of sounds.
pub struct PackSet {
    pub packs: Vec<Pack>,
    pub ui: UiBank,
}

// ---------------------------------------------------------------------------
// Synthesis
// ---------------------------------------------------------------------------

/// Small xorshift. Avoids a mutex-protected `RandomState` on the audio thread
/// and keeps the sequence reproducible from a seed.
struct Rng(u32);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    fn jitter(&mut self, spread: f32) -> f32 {
        let t = (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32;
        (t * 2.0 - 1.0) * spread
    }
}

fn slot_shift(slot: KeySlot) -> (f32, f32, f32) {
    match slot {
        KeySlot::Normal => (1.0, 1.0, 1.0),
        // Deeper and longer, like a 6.25u bottom-out.
        KeySlot::Space => (0.70, 1.60, 1.10),
        KeySlot::Enter => (0.78, 1.35, 1.15),
        // Lighter and shorter, like a short-plunger stab.
        KeySlot::Modifier => (1.14, 0.70, 0.70),
    }
}

fn apply_slot(voice: &Voice, slot: KeySlot) -> Voice {
    let (freq, decay, amp) = slot_shift(slot);
    if slot == KeySlot::Normal {
        return voice.clone();
    }
    let mut v = voice.clone();
    for m in &mut v.modes {
        m.freq_hz *= freq;
        m.decay_s *= decay;
        m.amp *= amp;
    }
    v.impact.amp *= if slot == KeySlot::Space || slot == KeySlot::Enter {
        1.05
    } else {
        0.8
    };
    v.impact.decay_s *= decay;
    v
}

/// Derives the release voice: quieter, brighter, shorter.
fn release_of(voice: &Voice) -> Voice {
    let r = &voice.release;
    let mut v = voice.clone();
    for m in &mut v.modes {
        m.freq_hz *= r.pitch;
        m.decay_s *= r.decay;
        m.amp *= r.gain;
    }
    v.impact.amp *= r.gain * 1.15;
    v.impact.decay_s *= r.decay * 0.7;
    v.impact.lowpass_hz *= 1.4;
    v.attack_ms *= 0.6;
    v
}

/// Renders one recipe voice to mono f32.
pub fn render(voice: &Voice, rate: u32, pitch: f32, gain: f32) -> Vec<f32> {
    let longest = voice
        .modes
        .iter()
        .map(|m| m.decay_s)
        .fold(0.0f32, f32::max)
        .max(voice.impact.decay_s)
        .max(0.005);
    let room_tail = voice.room.iter().map(|(d, _)| *d).fold(0.0f32, f32::max);
    let dur = longest * 4.0 + room_tail + 0.005;
    let n = ((rate as f32) * dur).ceil() as usize + 2;

    let mut buf = vec![0.0f32; n];

    for m in &voice.modes {
        let f = m.freq_hz * pitch;
        if f < 20.0 || f > rate as f32 * 0.45 {
            continue;
        }
        let step = 2.0 * std::f32::consts::PI * f / rate as f32;
        let decay_n = (rate as f32 * m.decay_s).max(1.0);
        let mut phase = 0.0f32;
        for i in 0..n {
            let env = (-(i as f32) / decay_n).exp();
            if env < 1e-4 {
                break;
            }
            phase += step;
            buf[i] += phase.sin() * env * m.amp;
        }
    }

    // Impact: one-pole lowpassed noise, a fraction of a mode's length.
    let impact_n = (rate as f32 * voice.impact.decay_s).max(1.0);
    let alpha = {
        let x = (-std::f32::consts::PI * voice.impact.lowpass_hz / rate as f32).exp();
        (1.0 - x).clamp(0.0, 1.0)
    };
    let mut rng = Rng(0x51F3_A7C1);
    let mut lp = 0.0f32;
    for i in 0..n {
        let env = (-(i as f32) / impact_n).exp();
        if env < 1e-4 {
            break;
        }
        let white = (rng.next_u32() as f32 / u32::MAX as f32) * 2.0 - 1.0;
        lp += alpha * (white - lp);
        buf[i] += lp * env * voice.impact.amp;
    }

    let attack_n = ((rate as f32) * voice.attack_ms * 0.001).round().max(0.0) as usize;
    if attack_n > 0 {
        let a = attack_n.min(n / 2);
        for i in 0..a {
            let t = i as f32 / a as f32;
            buf[i] *= 0.5 - 0.5 * (std::f32::consts::PI * t).cos();
        }
    }

    // Early reflections: a handful of decaying taps. Cheaper than a convolver
    // and enough to stop the sound feeling like it is inside a vacuum.
    let taps: Vec<(usize, f32)> = voice
        .room
        .iter()
        .map(|(d, g)| (((rate as f32) * d) as usize, *g))
        .filter(|(d, _)| *d > 0 && *d < n)
        .collect();
    for (delay, g) in taps {
        for i in 0..(n - delay) {
            buf[i + delay] += buf[i] * g;
        }
    }

    normalise(&mut buf, gain, voice.peak);
    buf
}

fn normalise(buf: &mut [f32], gain: f32, peak: f32) {
    let cur = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let target = peak.clamp(0.01, 1.0);
    let norm = if cur > 1e-6 { target / cur } else { 1.0 };
    for s in buf.iter_mut() {
        *s = (*s * norm * gain).clamp(-1.0, 1.0);
    }
    let n = buf.len();
    let fade = ((48_000 / 200).min(n / 2)).max(1);
    for i in 0..fade {
        buf[n - 1 - i] *= i as f32 / fade as f32;
    }
}

/// Pre-renders one recipe voice across every variant, slot and gesture.
pub fn bank_from_voice(voice: &Voice, rate: u32) -> VoiceBank {
    let slots = [
        KeySlot::Normal,
        KeySlot::Space,
        KeySlot::Enter,
        KeySlot::Modifier,
    ];
    let mut bank = VoiceBank {
        key: Vec::with_capacity(2),
        click: Vec::with_capacity(2),
    };

    for gesture in 0..2 {
        let mut per_variant: Vec<Vec<Arc<[f32]>>> = Vec::with_capacity(VARIANTS);
        let mut click_row: Vec<Arc<[f32]>> = Vec::with_capacity(VARIANTS);
        for v in 0..VARIANTS {
            let mut rng = Rng(
                0x1234_5678 ^ (gesture as u32).wrapping_mul(0x9E37_79B9)
                    ^ (v as u32).wrapping_mul(0x85EB_CA6B),
            );
            let pitch = 1.0 + rng.jitter(PITCH_JITTER);
            let gain = 1.0 + rng.jitter(GAIN_JITTER);
            let base: Voice = if gesture == 0 {
                voice.clone()
            } else {
                release_of(voice)
            };
            let per_slot: Vec<Arc<[f32]>> = slots
                .iter()
                .map(|s| {
                    let shaped = if gesture == 0 {
                        apply_slot(&base, *s)
                    } else {
                        base.clone()
                    };
                    Arc::from(render(&shaped, rate, pitch, gain).into_boxed_slice())
                })
                .collect();
            per_variant.push(per_slot);
            click_row.push(Arc::from(render(&base, rate, pitch, gain).into_boxed_slice()));
        }
        bank.key.push(per_variant);
        bank.click.push(click_row);
    }
    bank
}

/// Builds a pack from a decoded sample bank, resampling to the device rate and
/// giving each slot its character.
///
/// Slot character is folded into the resample length (a shorter buffer at the
/// same rate *is* a lower pitch), so playback stays a straight buffer read with
/// no per-strike cost.
pub fn bank_from_raw(raw: &RawBank, rate: u32) -> VoiceBank {
    let slots = [
        KeySlot::Normal,
        KeySlot::Space,
        KeySlot::Enter,
        KeySlot::Modifier,
    ];
    let mut bank = VoiceBank::default();
    if raw.key.is_empty() {
        return bank;
    }

    for gesture in 0..raw.key.len().min(2) {
        let mut per_variant: Vec<Vec<Arc<[f32]>>> = Vec::new();
        let mut click_row: Vec<Arc<[f32]>> = Vec::new();

        for v in 0..raw.key[gesture].len() {
            let mut per_slot: Vec<Arc<[f32]>> = Vec::new();
            for s in 0..raw.key[gesture][v].len() {
                let (pitch, _decay, amp) =
                    slot_shift(slots.get(s).copied().unwrap_or(KeySlot::Normal));
                let src = &raw.key[gesture][v][s];
                let out_len = ((src.len() as f32) * pitch).round().max(16.0) as usize;
                let mut b = resample_to(src, raw.rate, out_len);
                normalise(&mut b, amp, 0.92);
                per_slot.push(Arc::from(b.into_boxed_slice()));
            }
            per_variant.push(per_slot);

            if gesture < raw.click.len() {
                if let Some(c) = raw.click[gesture].get(v) {
                    let mut cb = resample_to(
                        c,
                        raw.rate,
                        ((c.len() as f32) * (rate as f32 / raw.rate as f32)) as usize,
                    );
                    normalise(&mut cb, 0.85, 0.92);
                    click_row.push(Arc::from(cb.into_boxed_slice()));
                }
            }
        }
        bank.key.push(per_variant);
        bank.click.push(click_row);
    }

    // Pad so lookups never panic on a thin import.
    while bank.key.len() < 2 {
        let row = bank.key.first().cloned().unwrap_or_default();
        bank.key.push(row);
    }
    while bank.click.len() < 2 {
        let row = bank.click.first().cloned().unwrap_or_default();
        bank.click.push(row);
    }
    bank
}

/// Resamples to an explicit output length at `from`'s rate. Used both for rate
/// conversion and for baking in pitch.
pub fn resample_to(input: &[f32], from: u32, out_len: usize) -> Vec<f32> {
    if input.is_empty() || from == 0 || out_len == 0 {
        return input.to_vec();
    }
    let mut src: Vec<f32> = input.to_vec();
    if out_len < src.len() {
        // Shrinking: anti-alias with a one-pole lowpass just under the new
        // Nyquist, otherwise the downsampled content aliases.
        let ratio = out_len as f32 / src.len() as f32;
        let cutoff = (from as f32 * 0.45 * ratio).max(200.0);
        let a = 1.0 - (-std::f32::consts::PI * cutoff / from as f32).exp();
        let mut lp = 0.0f32;
        for s in src.iter_mut() {
            lp += a * (*s - lp);
            *s = lp;
        }
    }

    let mut out = Vec::with_capacity(out_len);
    let last = src.len() - 1;
    for i in 0..out_len {
        let t = i as f64 * last as f64 / out_len.max(1) as f64;
        let i0 = t.floor() as usize;
        let f = (t - i0 as f64) as f32;
        let p0 = src[i0.min(last)];
        let p1 = src[(i0 + 1).min(last)];
        let p2 = src[(i0 + 2).min(last)];
        let p3 = src[(i0 + 3).min(last)];
        // Catmull-Rom
        let a0 = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
        let a1 = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
        let a2 = -0.5 * p0 + 0.5 * p2;
        out.push((((a0 * f + a1) * f + a2) * f + p1).clamp(-1.0, 1.0));
    }
    out
}

/// Renders the non-keyboard sounds once for the whole app.
///
/// Only scroll ticks survive: on Linux a trackpad tap and a mouse click are the
/// same button event, so there is no separate tap sound to make.
pub fn build_ui(voices: &[Voice], rate: u32) -> UiBank {
    let mut ui = UiBank::default();
    for v in voices {
        if !v.name.to_lowercase().contains("scroll") {
            continue;
        }
        if let Some(row) = bank_from_voice(v, rate).click.first() {
            ui.scroll = row.clone();
        }
    }
    ui
}

pub fn builtin_families() -> Vec<Family> {
    builtin_file().families
}

pub fn builtin_ui() -> Vec<Voice> {
    builtin_file().ui
}

fn builtin_file() -> SoundFile {
    serde_json::from_str::<SoundFile>(include_str!("../sounds.json"))
        .expect("builtin sounds.json is malformed")
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

/// Lock-free state the audio callback reads.
pub struct Shared {
    /// f32 bits, 0-100.
    pub volume: std::sync::atomic::AtomicU32,
    pub enabled: AtomicBool,
    pub pack: AtomicUsize,
    pub voice: AtomicUsize,
    pub release_sound: AtomicBool,
    pub scroll_sound: AtomicBool,
    /// Gain multiplier from the matching per-app rule: 1.0 normal, ~0.35 quiet,
    /// 0 silent. Kept as f32 bits so the audio thread reads it with one load.
    pub rule_scale: std::sync::atomic::AtomicU32,
}

impl Shared {
    fn gain(&self) -> f32 {
        let vol = f32::from_bits(self.volume.load(Ordering::Relaxed)) / 100.0;
        let rule = f32::from_bits(self.rule_scale.load(Ordering::Relaxed));
        (vol * rule).clamp(0.0, 1.0)
    }
}

pub fn store_gain(slot: &std::sync::atomic::AtomicU32, pct: f32) {
    slot.store(pct.clamp(0.0, 100.0).to_bits(), Ordering::Relaxed);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    Press,
    Release,
}

#[derive(Clone)]
pub enum Kind {
    Key(KeySlot),
    Click,
    Scroll,
    /// An arbitrary rendered buffer, used by the designer's preview. Carries its
    /// own audio so it needs no pack lookup.
    OneShot(Arc<[f32]>),
}

impl Kind {
    /// Higher wins when several events land in one audio buffer. A keypress is
    /// the sound the user is actually listening for.
    fn priority(&self) -> u8 {
        match self {
            Kind::Key(_) | Kind::OneShot(_) => 4,
            Kind::Click => 2,
            Kind::Scroll => 1,
        }
    }
}

pub struct Event {
    pub kind: Kind,
    pub gesture: Gesture,
}

/// Resolves the currently selected sound to an owned buffer.
///
/// Returns an owned `Arc<[f32]>` rather than a reference, because the pack set
/// is swappable and a borrow cannot outlive the guard that produced it.
fn resolve(
    set: &Arc<ArcSwap<PackSet>>,
    shared: &Shared,
    gesture: Gesture,
    variant: usize,
    slot: Option<KeySlot>,
) -> Option<Arc<[f32]>> {
    let s = set.load();
    let pack = s.packs.get(shared.pack.load(Ordering::Relaxed))?;
    let bank = pack.voices.get(shared.voice.load(Ordering::Relaxed))?;
    match slot {
        Some(k) => bank.get(gesture, variant, k),
        None => bank.get_click(gesture, variant),
    }
}

/// Opens the output stream and returns the negotiated sample rate.
///
/// The stream is intentionally leaked: it must stay alive for the life of the
/// process, and holding it in Tauri managed state would require `Sync`, which
/// cpal's ALSA `Stream` is not.
pub fn start(
    shared: Arc<Shared>,
    set: Arc<ArcSwap<PackSet>>,
    rx: crossbeam_channel::Receiver<Event>,
) -> Result<u32, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no default output device")?;
    let config = device
        .default_output_config()
        .map_err(|e| format!("no output config: {e}"))?;
    let rate = config.sample_rate().0;
    let channels = config.channels() as usize;

    let mut stream_config: cpal::StreamConfig = config.into();
    stream_config.buffer_size = cpal::BufferSize::Fixed(256);

    let shared_c = shared;
    let set_c = set;
    let mut rng = Rng(0x2545_F491);

    // Playback state, held across callbacks because sounds are longer than one
    // buffer. Holding an `Arc<[f32]>` keeps the data alive without borrowing from
    // the swappable pack set.
    let mut playing: Option<(Arc<[f32]>, usize, f32)> = None;

    let stream = device
        .build_output_stream(
            &stream_config,
            move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                for s in out.iter_mut() {
                    *s = 0.0;
                }
                let ch = channels.max(1);
                let frames = out.len() / ch;

                let gain = shared_c.gain();
                let enabled = shared_c.enabled.load(Ordering::Relaxed)
                    && gain > 0.0;

                // Drain the queue, keeping the highest-priority pending event.
                let mut pending: Option<Event> = None;
                while let Ok(ev) = rx.try_recv() {
                    if ev.gesture == Gesture::Release
                        && !shared_c.release_sound.load(Ordering::Relaxed)
                    {
                        continue;
                    }
                    match &pending {
                        None => pending = Some(ev),
                        Some(cur) => {
                            if ev.kind.priority() >= cur.kind.priority() {
                                pending = Some(ev);
                            }
                        }
                    }
                }
                if !enabled {
                    playing = None;
                    while rx.try_recv().is_ok() {}
                    return;
                }

                // Start a new sound if one is pending and nothing long is playing.
                if let Some(ev) = pending {
                    let variant = (rng.next_u32() as usize) % VARIANTS;

                    let buf = match ev.kind {
                        // A one-shot carries its own audio: the designer renders
                        // a shape that is not in any pack.
                        Kind::OneShot(ref b) => Some(b.clone()),
                        Kind::Key(slot) => {
                            resolve(&set_c, &shared_c, ev.gesture, variant, Some(slot))
                        }
                        Kind::Click => {
                            resolve(&set_c, &shared_c, ev.gesture, variant, None)
                        }
                        Kind::Scroll => {
                            if shared_c.scroll_sound.load(Ordering::Relaxed) {
                                set_c.load().ui.scroll.get(variant).cloned()
                            } else {
                                None
                            }
                        }
                    };

                    if let Some(b) = buf {
                        let amp = if matches!(ev.kind, Kind::Click) {
                            gain * 0.85
                        } else {
                            gain
                        };
                        playing = Some((b, 0, amp));
                    }
                }

                // Stream the active sound across as many buffers as it needs.
                if let Some((data, cursor, amp)) = playing.as_mut() {
                    let mut i = 0usize;
                    while i < frames && *cursor < data.len() {
                        let s = data[*cursor] * *amp;
                        for c in 0..ch {
                            out[i * ch + c] += s;
                        }
                        *cursor += 1;
                        i += 1;
                    }
                    if *cursor >= data.len() {
                        playing = None;
                    }
                }
            },
            |err| eprintln!("[audio] {err}"),
            None,
        )
        .map_err(|e| format!("could not open stream: {e}"))?;

    stream
        .play()
        .map_err(|e| format!("could not start stream: {e}"))?;
    std::mem::forget(stream);
    Ok(rate)
}
