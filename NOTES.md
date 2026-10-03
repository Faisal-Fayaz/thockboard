# ThockBoard — Spike findings

## Environment (measured, 2026-10-02)

| | |
|---|---|
| OS | Ubuntu 24.04.5 LTS, kernel 7.0.0-34 |
| Session | **X11** (GNOME) — global hooks viable, Wayland blocker does not apply |
| Hardware | ASUS laptop: built-in keyboard + Mouse + Touchpad (all three test cases present) |
| Audio server | PipeWire 1.0.5 (PulseAudio protocol), WirePlumber |
| Sink | `alsa_output.pci-0000_05_00.6.analog-stereo`, Realtek ALC294, card 2 |
| Toolchain | rustc 1.98.1, cargo 1.98.1, gcc, pkg-config |

## Gate 0 result — PASS (subjective), FAIL (numeric target)

| Component | Measured | Source |
|---|---|---|
| X11 hook → callback | ~0.7ms | spike instrumentation |
| Hook → audio thread | ~0.00ms | spike instrumentation |
| PipeWire stream buffer | 1.45ms | `node.latency = 64/44100` |
| ALSA driver period | 21.33ms | `period_size: 1024` @ 48000 Hz |
| **Total** | **~23ms** | |
| Target | ≤15ms | missed |

**Subjective: "it feels good."** The ≤15ms target was wrong for this workload.

### Why 23ms is acceptable here

The sound is *causally triggered* by the keypress — there is no reference event to stay in sync with. This is the forgiving case, unlike video audio sync where 23ms would be plainly wrong.

### The decision this buys us

We do **not** need a system-wide `clock.force-quantum` change. That would have meant asking every user to edit PipeWire config and restart their audio stack — a serious adoption barrier. Not needed.

Caveat on scope: this was measured on a laptop membrane keyboard with headphones. The harder case is a desktop with loud speakers and clicky mechanical switches, where the real click is loud and close to the ears. Untested.

## Two mistakes worth remembering

1. **cpal's ALSA backend never populates `OutputStreamTimestamp`**, so `info.timestamp().playback - .callback` reads 0.00ms and is meaningless. Device latency must come from PipeWire (`pactl`) or `/proc/asound/*/pcm*p/sub0/hw_params`.
2. **cpal on Linux goes through ALSA → pipewire-alsa plugin**, not around PipeWire. Verified: our stream appears in `pactl list sink-inputs`. No device conflict. Streams open at 44100Hz while the sink runs 48000Hz, so resampling happens.

## Build notes

Required system packages for Stage A (headless Rust, no GUI):
```
libasound2-dev libxi-dev libxtst-dev
```
- `libxi-dev` + `libxtst-dev` are needed because the x11 crate's `build.rs` probes `pkg-config` for **enabled features**, and rdev enables `xlib`+`xrecord`+`xinput`. `xrecord = ["xtst"]`, so both `xi` and `xtst` are required. The crate `dlopen`s at runtime, but still probes at build time.
- `libxext-dev` comes in as a dependency of `libxtst-dev`.

## Doubling — resolved

**User test: no audible doubling.** The laptop's own key click was not heard as a second sound ~23ms after our thock; the 23ms offset is perceptually absorbed rather than heard as a flam.

Good outcome: no "recommend headphones to avoid doubling" caveat is needed in the landing page. Still untested on a desktop with loud speakers and clicky mechanical switches, where the real click is loud and close to the ears.

## Stage B — webview vs native audio: NATIVE WINS

The question Stage B existed to answer: can Web Audio in the webview meet the budget, or do we need Rust-owned audio?

### Measured

| | Native (cpal) | Webview (WebKitGTK) |
|---|---|---|
| PipeWire stream buffer (`node.latency`) | 1.45ms (`64/44100`) | **40.0ms** (`1764/44100`) |
| Web Audio `baseLatency` | n/a | 2.90ms |
| Web Audio `outputLatency` | n/a | **not implemented by WebKitGTK** |
| ALSA driver period | 21.33ms | 21.33ms |
| **Total** | **~23ms** (measured "feels good") | **~61ms** |
| Rust → webview IPC hop | n/a | **~0.00ms** (avg -0.09, max 0.56) |

**Verdict: webview audio rejected. Native Rust audio + webview for UI only.**

### The trap that nearly cost us the product

Web Audio reports `baseLatency = 2.90ms`, which looks like the webview path is nearly free. It is not. `baseLatency` measures only the JS audio graph; it excludes the buffer GStreamer negotiates with PulseAudio. That buffer is **1764 frames = 40ms**, and it is invisible from JS.

`PULSE_LATENCY_MSEC=15` had **no effect** — still 1764 frames.

**Never trust `baseLatency` alone.** Cross-check with `pactl list sink-inputs | grep node.latency`.

### Why native audio is now viable

The IPC hop from Rust to the webview measured **~0.00ms**. So routing "play sound" JS → Rust → cpal costs nothing measurable, while keeping the proven 23ms audio path.

Resulting architecture:
```
X11 hook (Rust) --> queue --> Rust audio thread (cpal) --> device
                |
                +--> emit to webview  (UI feedback only, never audio)
```
The webview owns pack selection, settings and onboarding. Audio is entirely Rust. Pack synthesis happens offline at build time into pre-rendered buffers, per the original v1 plan.

### Also learned

- `outputLatency` is unimplemented in WebKitGTK, so the app can never self-report true hardware latency. Any in-app latency figure must be read from the system (`pactl` / `/proc/asound`), or omitted.
- rdev 0.5.3 `EventType` variants are **tuple** variants (`KeyPress(Key)`, `ButtonPress(Button)`), not struct variants.
- rdev event timestamps: Rust `SystemTime` vs JS `performance.timeOrigin` show ~0.5ms skew. Fine for measuring, not usable for sub-ms product logic.
- Tauri commands must never block. An early test harness slept inside a command and stalled webview event delivery, producing a fake 4372ms "latency". Move blocking work to a spawned thread.

## Native audio ported into Tauri — DONE

`app/src-tauri/src/audio.rs` + `lib.rs`. Webview is UI only.

```
X11 hook (Rust) --try_send--> bounded(256) crossbeam queue --> cpal thread --> device
   |
   +--> app.emit("strike")  (UI activity feed only, never audio)
```

- Stream verified at `node.latency = 64/44100` (**1.45ms**) inside Tauri, identical to the spike.
- 5 builtin packs in `app/src-tauri/packs.json`, pre-rendered at startup into
  `[variant][key_class]` banks. 6 variants per pack bake in ±3% pitch and ±12% gain,
  which is what stops repeat strikes sounding machine-gunned. Zero runtime cost.
- Per-strike variation is **baked**, not resampled at play time — pitch variation
  would need a resampler.
- Key classes: `Normal`, `Space` (×0.72 freq, ×1.55 decay), `Modifier`
  (×1.12 freq, ×0.72 decay).
- Audio callback is lock-free: pre-rendered buffers behind `Arc`, atomics for
  volume/enabled/pack. A burst keeps only the **most recent** strike, so a held key
  cannot queue and fire seconds later.
- `cpal::Stream` is leaked via `mem::forget` on purpose — it must live for the
  process lifetime and is not `Sync`, so it cannot go in Tauri managed state.

### Verified end to end

12 synthetic presses (`spike --simulate`) → all captured, correctly classified:
`H`/`I` normal, `Space` space, `ShiftLeft` modifier, plus a mouse `click Left`.
No errors.

### rdev 0.5.3 naming traps (cost two bugs)

- `EventType` variants are **tuple** variants: `KeyPress(Key)`, `ButtonPress(Button)`.
- Modifier keys are `ShiftLeft`, `ControlLeft`, `Alt`, `MetaLeft` — **not**
  `LeftShift`/`LeftControl`. Guessing here silently misclassified every modifier.
- `rdev::simulate` takes `&EventType`.
- `try_send` exists on `crossbeam_channel::Sender`, **not** on `std::sync::mpsc::Sender`.

### Own bugs worth remembering

- A `n % 16 == 1` log throttle made 13 strikes look like 1, and briefly looked like
  the hook was dead. Log the first N in full, then throttle.
- Discarding `rdev::listen`'s `Result` makes a dead hook look exactly like a quiet
  keyboard. Always surface it.
- Click strikes were being routed to the *key* voice; `StrikeKind::Key(..)` vs
  `StrikeKind::Click` now makes the choice explicit.

## Milestone: working on Linux with real typing

User confirmed the app "works good" with physical keystrokes and headphones. That
closes the one measurement synthetic events could not stand in for.

### Binary size: **8.05MB**

Validates Tauri decisively — Electron's floor was ~150MB, and for a $15
impulse-download utility the download size is the conversion factor.

### Settings persistence

`src-tauri/src/settings.rs`, plain JSON at `~/.config/dev.thockboard.app/settings.json`.
No plugin: four fields do not justify a dependency.

Verified all three paths:
- fresh start -> defaults (pack 0, vol 70, enabled)
- restart -> restores saved values (pack 2, vol 41.5, enabled false)
- corrupt file -> logs, falls back to defaults, **app still starts**

Settings are restored *before* the audio stream opens, so the first strike already
uses the right pack.

App identity set to `dev.thockboard.app` / "ThockBoard" (was the scaffold default,
which also affected the config dir and the Linux desktop entry).

## Tray, autostart, hotkey

- **Tray** (`src-tauri/src/tray.rs`): open window, sound pack submenu (5 packs),
  Mute, Launch at startup, Quit. Left click on the icon toggles window visibility.
- **Close hides, doesn't exit** — `WindowEvent::CloseRequested` is prevented and the
  window hidden. Quit lives in the tray, which is the discoverable way out.
- **Single instance** (`tauri-plugin-single-instance`): verified, a 2nd launch hands
  off and exits, and the audio stream count stays at 1.
- **Ctrl+Alt+M** mute hotkey. Detected from the hook we already have, so no extra
  global-shortcut registration or dependency.
- `tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None)` with the tray
  checkbox reflecting the OS-reported state.

### rdev's X11 backend delivers NO KeyRelease

Measured: 75 events captured across injected bursts, **zero** releases.

`src/linux/listen.rs` sets `device_events.first = KeyPress`, `last = MotionNotify`,
so the range *looks* like it covers `KeyRelease` (3) — but none arrive.

Consequences:
- The hotkey **cannot** re-arm on KeyRelease. Replaced with **sticky modifiers**
  (set on modifier press, consumed on the chord or on any other key) plus a
  **500ms time debounce**.
- Consequence for Windows/macOS: they *do* deliver releases, and clearing on
  release there is still correct, so the same code works on all three platforms.

### XTEST cannot inject modifier chords into an XRecord listener

`spike --simulate-chord` (Ctrl+Alt+M) sends ControlLeft/Alt/KeyM via `rdev::simulate`
from a separate process. The app sees **only** the `M` presses — the modifiers never
arrive, so `chord=true` never happens and the hotkey cannot be tested automatically.
Regular keys (`--simulate`) inject fine, including `ShiftLeft`.

So the hotkey's *toggle path* is proven (settings.json flips), but its *chord
detection* needed a real keypress — **confirmed working by the user.** Ctrl+Alt+M
toggles mute, and repeating it toggles back, which is what the sticky-modifier +
debounce design exists to make possible.

### Traps hit

- `pkill -f 'target/release/app'` **matches its own shell** and hangs the tool.
  Use `pgrep -af '[t]arget/release/app'` (bracket trick) or kill by PID.
- `rg -c 'PipeWire ALSA \[app\]'` counts **two lines per stream**
  (`application.name` and `module-stream-restore.id`). It looked like duplicate
  audio streams; it was not. Count `Sink Input` blocks with awk.
- `std::env::var_os` **allocates**. Calling it per keystroke on the OS input
  thread is a latency bug. Hoisted out of the hook callback.

## Phase 1-4 — library, custom packs, designer, rules

### Sound engine rewrite — modal synthesis

Replaced sine + filtered-noise synthesis with **modal synthesis**: a bank of damped
sinusoids at the resonant frequencies a real switch/plate/case produces, plus a short
lowpassed noise impact, a raised-cosine attack, and early-reflection taps.

Rationale: a thock and a clack *are* different modal shapes. No amount of tuning a
sine plus a noise burst gets you there.

- `sounds.json`: **5 families, 19 sounds** — Thock (4), Clack (4), Typewriter (4),
  Toppy (3), Studio (4). Replaces the old flat `packs.json` (5 monolithic packs).
- **Press and release voices.** Release is derived as quieter, brighter and shorter.
  Typing only sounds convincing when the whole gesture does.
- **8 variants per sound** bake in ±2.8% pitch / ±10% gain.
- **912 buffers** pre-rendered at startup (19 sounds × 2 gestures × 8 variants ×
  3 key classes). All sounds are pre-rendered, not just the selected one.
- **Audition in-app**: family tabs + clickable voice list that plays a preview, so
  sounds can be judged without typing. This is what Clacky's website does.

### Bug I introduced and caught

The first version of the redesign packed a whole family into one bank indexed by
*variant*, cycling `voices[v % voices.len()]`. Result: the audio thread played a
**random voice from the family** and the selection was ignored entirely. Banks are now
keyed by selected voice (`Bank { voices: Vec<VoiceBank> }`).

### Memory: 191MB RSS

Mostly WebKitGTK, not our buffers. This is the real cost of the Tauri/webview choice:
**small binary, heavy runtime**. For a menu-bar app that runs all day that is a real
number to watch. Clacky is a small native app. Worth revisiting if memory ever
becomes a complaint.

## Packaging (Linux)

| Artifact | Size | Notes |
|---|---|---|
| `thockboard` binary | 8.05MB | stripped; what users actually run |
| `.deb` | **2.6MB** | Installed-Size 6548 KB. Correct deps declared |
| `.AppImage` | **80.4MiB** | Self-contained; bundles webkit + glibc |

**The 31x size gap is the distribution story for Linux.** Most users are better
served by `apt install` of a 3.2MiB deb than an 80MiB AppImage. AppImage is only for
people who cannot install packages.

AppImage build needs to download AppRun + linuxdeploy from GitHub releases; that
download timed out once and succeeded on retry, so **retry rather than assuming the
network is blocked**.

### Scaffold defaults that reached the artifact

The first deb shipped as `/usr/bin/app` with description "A Tauri App" and Tauri
logo icons. Fixed:

- crate renamed `app` -> `thockboard`, lib `app_lib` -> `thockboard_lib`
  (`src/main.rs` calls `thockboard_lib::run()`)
- description -> "Mechanical-keyboard sounds for any keyboard or mouse."
- result: `/usr/bin/thockboard`, `ThockBoard.desktop` with `Exec=thockboard`,
  icons `thockboard.png`

Identifier has changed twice: `dev.thockboard.app` -> `dev.thockboard.client` ->
`dev.faisal.thockboard`. The first change was because Tauri warns that an identifier
ending in `.app` conflicts with the macOS bundle extension.

The identifier names the config directory, so renaming it orphans user data:
`~/.config/dev.thockboard.client/` became `~/.config/dev.faisal.thockboard/`. The local
copy was migrated by hand. Any Tauri autostart entry is keyed by identifier too, so it
is orphaned by a rename and has to be re-created. Both are the reason to settle the
identifier *before* the first release rather than after.

**Icons are still the stock Tauri logo.** Nothing in this build made a real icon.

## What is still missing for a real release

1. ~~Tray icon, autostart, global hotkey.~~ **Done**, hotkey verified with real keys.
2. **Packaging.** Done on CI: deb 3.2MiB, AppImage 80.4MiB, msi/nsis, dmg/app.
   **Unsigned.** DMG and MSI build fine on hosted runners — no Mac or Windows cert is
   needed to *build* them; a cert only controls whether users get an unverified-writer
   warning. macOS Gatekeeper and Windows SmartScreen will still prompt on first run.
3. **Licensing.** Lemon Squeezy or Paddle for the free-starter + $15 unlock model.
4. **A real app icon.** Still the stock Tauri logo. Zero effort, high visibility.
5. **macOS Accessibility permission UX.** `CGEventTap` requires it, and this is the
   single biggest adoption risk: users bounce if the permission walkthrough is poor.
   Untested - no Mac access yet.
6. **Windows and macOS ports.** The latency findings are Linux/PipeWire specific;
   CoreAudio and WASAPI have their own behaviour. User has a Windows box, a friend
   has a Mac.
7. **Sound quality pass.** Whether the 5 packs sound meaningfully different, or like
   one click through five filters, is still unjudged. This is the actual product.

## Open questions

- **Sound quality.** Ask specifically whether packs are distinguishable, or just
  filter variations. This gates everything else.
- **Harder hardware case:** desktop + loud speakers + clicky mechanical switches.
  The masking problem is worst there and is untested.


## Phases 1-4 complete

### Phase 1 — library: 19 -> 31 sounds, 8 packs

Reorganised around the community's own vocabulary (Thock, Creamy, Clack, Poppy,
Marble, Typewriter, Muted, Fun) rather than invented names. Creamy and Muted are
new packs: Creamy is the most-searched profile after thock/clack, and Muted is the
largest untapped market ("I want sounds but I'm in a meeting"). `Clicky` added
separately from Clack, because a deliberate click mechanism (MX Blue) is a
different physical event from a bright bottom-out.

### Bug found while planning phase 2: every sound was truncated to 5.8ms

The callback did `src.len().min(frames)` where `frames` was one 256-frame buffer.
Voices are ~50ms, so only the impact and the start of the decay had ever been
played. Sounds now stream across as many buffers as they need, holding an
`Arc<[f32]>` per strike so playback cannot borrow from the swappable pack set.

This also made long imported samples possible, which one buffer never was.

### Phase 2 — pack engine

`ArcSwap<PackSet>` so packs are swappable at runtime: the audio callback does one
atomic load per buffer and a reload never touches the audio thread.

Three sources, all ending up in the same `VoiceBank`:
1. **Built-in recipes** — `sounds.json`, 31 sounds
2. **Recipe packs** — `packs/<name>/pack.json`, same schema. A whole pack is a few
   KB of text: no assets, no licensing, shareable and diffable
3. **Mechvibes v1/v2 + sample folders** — decoded with symphonia, resampled to
   the device rate, normalised at import, then the originals are dropped so
   sample packs cost the same memory as recipes

**Four key slots** (`Normal`, `Space`, `Enter`, `Modifier`) rather than per-key
mapping: a deliberate compromise against memory. Backspace and friends reuse the
default, which is what most community packs do anyway.

Per-slot character is folded into the resample length (a shorter buffer at the
same rate *is* a lower pitch), so playback stays a straight buffer read.

Zip imports reject path traversal, cap file count and unpacked size.

**Resampler:** cubic Hermite with a one-pole anti-alias pre-filter, rather than
rubato. These are 30-80ms one-shots that are never streamed, so windowed-sinc
would buy inaudible quality for latency-compensation complexity. Dependency
dropped.

### Bug: `symphonia` with `default-features = false` silently broke every .wav

WAV is only a container; the codec inside is PCM, which lives behind its own
`pcm` feature. Every WAV in every pack failed with `unsupported codec` and the
loader reported only "skipping unrecognised pack". Found by making the loader
print why it skipped something. Features now: pcm, mp3, flac, vorbis, aac, adpcm.

### Phase 3 — UI and the perceptual designer

Three tabs. Library (pack chips, search, waveform thumbnails, hover-to-preview
with a 220ms delay, keyboard-navigable rows), Create (the 4-axis designer),
Rules.

Peaks are computed in Rust from the *rendered* buffers, so a thumbnail shows what
you hear rather than what the recipe says.

**The designer's axes map 1:1 onto synthesis** because a recipe voice is a
physical model, not a recording:
- pitch -> mode frequencies (0.62x to 2.2x)
- attack -> impact decay 0.4ms to 7.5ms, plus attack shaping 0.1ms to 3ms
- resonance -> mode decay times x0.3 to x2.6, and early-reflection gain
- loudness -> render target peak

Saved sounds are written as recipe packs and appear in the library immediately.

Preview and select are separate sibling controls: nesting a button inside a
clickable row is an invalid, unusable accessibility tree.

**Bug found and fixed:** the loudness slider did nothing. `render` normalises to a
fixed peak, which erased any amplitude scaling. `Voice` now carries a `peak`
target that `render` honours.

### Phase 4 — per-app rules

Different sound per app, quieter, or silent. First match wins so the list reads
top to bottom.

The focused window is polled at 4Hz off the input thread — asking the window
manager per keystroke would add latency to typing, the one thing that must never
happen. The rule result is published to the audio path through a single atomic
(`Shared::rule_scale`), so the callback does one extra load and nothing more.

X11 via EWMH `_NET_ACTIVE_WINDOW` + `WM_CLASS`, using the `x11` crate that rdev
already pulls in rather than hand-rolled FFI. Windows and macOS untested.

### Dropped: trackpad tap sound

Clacky has one. On Linux a trackpad tap and a mouse click are the same button
event, so there is no separate sound to make and a toggle would be a lie.
Removed rather than shipped as a no-op.

### Current state

| | |
|---|---|
| Built-in sounds | 31 synthesized across 8 packs, plus 30 CC0 recorded strikes across 3 real boards |
| Custom pack formats | recipe JSON, Mechvibes v1/v2, loose audio folders, zip |
| Stream latency | `64/44100` = 1.45ms (unchanged through the whole refactor) |
| Binary | 8.05MB |
| .deb | 3.2MiB |
| AppImage | 80.4MiB |
| RSS | 213MB (WebKitGTK, not our buffers) |
| Test fixtures | `test-mv` and `test-recipe` left in the packs folder on purpose |

---

## Real recorded sounds (current work)

The synthesised library is wrong. Measured against real switches the gap is not
subtle:

| | Old synthesis | Real switches |
|---|---|---|
| Fundamental | ~118Hz | ~600-4000Hz |
| Duration | ~150ms | ~20-70ms |
| Attack | softened | sharp, 1-2ms |
| Modes | few, wrong ratios | many, board-specific |

No amount of tuning makes a modal oscillator sound like a switch, so the core
library has to be recordings. Synthesis stays for custom sounds and the
designer, where it is a tool rather than a claim about real hardware.

Sources are CC0 Freesound clips of people typing 50-word passages. Drop them in
`reference/` and rebuild. Individual strikes come from the tail, where keys are
hit back-to-back instead of overlapped.

### The slicer

`tools/slicer` is build-time only, never shipped. It cuts, normalises,
de-duplicates and measures each strike:

    tools/slicer/target/release/slicer reference app/src-tauri/assets/slices

Output is embedded via generated `src/slices_generated.rs` and loaded as
`Origin::Recorded` packs, ordered ahead of the synthesised ones. Ten 45ms mono
slices is ~40KB of binary, which is why recording is affordable here.

- onset detection on a 2ms envelope hop, strongest first, with a minimum gap so
  one keypress is not counted repeatedly
- 6ms pre-roll, 45ms slice, 70Hz high-pass, normalised to 0.89
- 24-band fingerprint de-duplication, so repeated strikes do not all sound alike
- reports peak frequency, centroid, decay duration and HF sharpness per board,
  which is the calibration data the synth voices are tuned against

### Verification

Statistics are only worth having if the FFT is right, so the tool is checked
against a synthetic pure 1000Hz tone:

    peak 991Hz, centroid 998Hz, sharpness 0.00

Two runtime tests in `packs.rs` guard the build-time-to-playback path: embedded
slices decode into four usable key slots at the device rate without clipping, and
every slice starts and ends at exactly zero.

### Bugs found while building this

- FFT twiddle index was `2*k*step` instead of `k*step`, overrunning the table
  and panicking. Found by running the tool, not by reading it.
- The onset minimum gap was computed in samples but compared against envelope
  frame indices, so it was ~30x too large and rejected nearly every strike.
- Duration was measured by scanning forward from sample 0. Once slices faded in
  from silence that reported 0ms for every sample. It now measures decay from
  the envelope peak.
- The FFT analysed the first 1024 samples of the slice, which includes the
  pre-roll. That shifted where the transient landed under the Hann taper and
  moved centroid from 998Hz to 1269Hz on the reference tone. Statistics now
  cover the event only.
- Statistics were computed after the fade, so the ramp's own step-like onset
  inflated centroid and sharpness. Measured before fading.
- Generated `include_bytes!` paths were one directory too high, and were
  hard-coded relative to the tool's working directory rather than derived from
  the output path.

## Measured results (real CC0 recordings)

| Board | Peak | Centroid | Decay | Sharpness | Slices | Rejected |
|---|---|---|---|---|---|---|
| HHKB Topre | 1688Hz | 4275Hz | 37ms | 0.59 | 10 | 8 |
| Leopold FC660M | 2425Hz | 4556Hz | 40ms | 0.69 | 10 | 4 |
| WhiteFox Hako Violet | 844Hz | 3888Hz | 40ms | 0.59 | 10 | 0 |

Compare the old synthesis: 118Hz fundamental, 150ms. The recorded boards sit
844-2425Hz with 37-40ms decay, which is the range switches actually occupy. This
table is now the calibration target for the designer.

Source recordings are good: 39dB peak-to-noise, noise floor at -73dBFS. Kept
slices have crest factor 6-13 and energy front-loaded 6-40x, so they are genuine
single impacts rather than noise that normalisation happened to amplify.

The two stu556 files are bass and treble EQ variants of the same recording, not
two boards, so they merge into one FC660M pack and contribute 20 voices.

### What I could not settle without ears

I can measure that a slice is impulsive and correctly band-limited; I cannot hear
whether it reads as a satisfying thock. Crest factor, centroid and sharpness are
proxy metrics, and energy-summed band percentages proved actively misleading:
65% of the energy above 3.2kHz looked like hiss but is just what a bright switch
sounds like, because a broadband sum favours wide content over one strong
resonance. Peak frequency is the number to trust. Final judgement is the user's.

### Bugs found in this phase

- The manifest was invalid JSON: the hand-rolled writer emitted a trailing comma.
  Replaced with `serde_json`, which is the right call for a build-time tool.
- The slicer did not recurse, so Freesound packs downloaded as a directory were
  skipped entirely.
- `include_bytes!` paths were one directory too high, and hard-coded relative to
  the tool's working directory instead of derived from the output path.
- Peak frequency read 73-138Hz despite a 70Hz high-pass. Residual rumble dominated
  the loudest bin. High-pass raised to 150Hz and made two-pole, which moved peaks
  to 844-2425Hz.
- No DC removal, so clip offset up to 0.028 survived into playback and analysis.
- No quality gate, so overlapping strikes and room tone were sliced as single
  keystrokes. Now rejected on crest factor and front-loading, with counts reported.

### Self-test

    tools/slicer/target/release/slicer --selftest

Fifteen checks against known signals: tone peak/centroid/sharpness at 1k and 2.5k,
crest and front-loading separating noise from impacts, fades reaching zero, and
fingerprint behaviour. Two of these initially failed and both were wrong
expectations rather than code bugs: sharpness is a fraction above a fixed 2kHz
split, so a 2.5kHz tone correctly scores 1.0; and the fingerprint is deliberately
amplitude-invariant, so scaling must not change it. The property that matters is
separating different spectra, which it does.

The pipeline gate also rejects the self-test tone, correctly: a smooth 2.5kHz
sine with 13ms decay has a crest factor of 2.6, so it is not a keystroke. That is
why the primitives are tested directly instead of through the pipeline.
