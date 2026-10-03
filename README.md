# ThockBoard

Mechanical keyboard sounds for any keyboard, system-wide.

Type anywhere and hear your board. ThockBoard plays a chosen keyboard's sound for
every keystroke and mouse click, using native audio so it keeps up with fast typing.

**Status: early.** Linux is the working platform. Windows and macOS compile but are
untested — see [Platform support](#platform-support). Contributions targeting those
platforms are the most useful thing you can do here.

## What it does

- **Recorded boards.** 30 real keystrokes sliced from CC0 recordings of three
  boards: HHKB Topre, Leopold FC660M, WhiteFox Hako Violet. Ten distinct strikes
  per board, de-duplicated by spectral fingerprint so they genuinely differ.
- **A synthesizer.** 31 voices across 8 packs, tunable with a perceptual designer.
- **Your own sounds.** Import MechVibes configs, loose sample folders, recipe JSON,
  or a zip.
- **Per-app rules.** Mute or swap packs automatically depending on which window has
  focus (Linux only).
- **Tray, autostart, and a mute hotkey** (`Ctrl+Alt+M`).

### Why recorded, not synthesized

The synthesizer was built first and sounded wrong, measurably so:

|                | Synthesized | Real switches |
| -------------- | ----------- | ------------- |
| Fundamental    | ~118Hz      | ~844–2425Hz   |
| Duration       | ~150ms      | ~37–40ms      |

A modal oscillator cannot reproduce a switch's sharp broadband attack and board-specific
mode ratios. So the built-in library is recordings, and synthesis remains where it is
honest: as a tool for custom sounds.

## Install

Grab a release artifact: https://github.com/Faisal-Fayaz/thockboard/releases

```bash
# Linux, Debian/Ubuntu
sudo apt install ./ThockBoard_*_amd64.deb

# Linux, anything else
chmod +x ThockBoard_*_amd64.AppImage && ./ThockBoard_*_amd64.AppImage

# Windows: run the .msi
# macOS:   open the .dmg and drag to Applications
```

**These artifacts are unsigned.** macOS Gatekeeper and Windows SmartScreen will warn on
first launch. On macOS use System Settings -> Privacy & Security -> Open Anyway. On
Windows choose More info -> Run anyway.

On macOS, global keyboard hooks additionally require Accessibility permission in
System Settings -> Privacy & Security -> Accessibility. Without it the app runs but
hooks do nothing.

The icon is still the stock Tauri logo.

## Build from source

You need Rust (stable), Node 18+, and `pnpm`.

```bash
cd app
pnpm install
pnpm tauri dev      # development
pnpm tauri build    # produces installable bundles for your platform
```

`pnpm tauri build` also runs `tsc`, so the frontend is type-checked before bundling.

### A trap worth knowing

`generate_context!` embeds the built frontend at compile time, so **`dist/` must exist
before `cargo` runs**. `pnpm tauri build` handles this for you. If you run `cargo build`
directly, do this first:

```bash
pnpm build                                          # creates dist/
cd src-tauri && cargo build --release --features custom-protocol
./target/release/thockboard
```

Without `custom-protocol`, the binary is a dev build: it will show
"Could not connect to localhost" because it is looking for a Vite dev server that
isn't running.

### Linux system dependencies

Debian/Ubuntu, needed for Tauri and for global input hooks:

```bash
sudo apt install libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
                 libayatana-appindicator3-dev librsvg2-dev \
                 libasound2-dev build-essential
```

To produce installers rather than just run it, also need `fakeroot` and `dpkg-dev`:

```bash
sudo apt install fakeroot dpkg-dev
```

## Platform support

| | Linux | Windows | macOS |
| --- | --- | --- | --- |
| Global hooks (X11) | working | compiles, untested | compiles, untested |
| Audio playback | working | compiles, untested | compiles, untested |
| Tray and autostart | working | compiles, untested | compiles, untested |
| Per-app rules | working | stub | stub |

Three platform-specific things you will hit:

- **Linux is X11 only.** Wayland is not supported: there is no reliable global
  keyboard hook, and the per-app rules use X11 to find the focused window.
- **macOS requires Accessibility permission** for global keyboard events, granted in
  System Settings → Privacy & Security → Accessibility. Without it hooks silently do
  nothing. There is no in-app prompt for this yet.
- **Key release events never arrive on Linux X11.** The mute hotkey therefore uses
  sticky modifier tracking plus a 500ms debounce rather than key-up detection.

## How the recorded sounds were made

`tools/slicer` is a build-time-only tool. It reads long CC0 recordings of someone
typing, finds individual strikes by onset detection in the back-to-back tail where
keys do not overlap, and emits short normalised slices with spectral de-duplication
and edge fades.

```bash
tools/slicer/target/release/slicer reference app/src-tauri/assets/slices
tools/slicer/target/release/slicer --selftest   # 15 DSP checks
```

The output is committed at `app/src-tauri/assets/slices/` and embedded in the binary,
so **building the app never requires the source recordings.** To re-slice, see
[tools/fetch-reference.md](tools/fetch-reference.md).

## Layout

```
app/            Tauri app: Rust backend, TypeScript frontend
  src-tauri/
    src/audio.rs   native playback and synthesis
    src/packs.rs   built-in, recorded, and custom pack loading
    src/rules.rs   per-app rules (X11-gated)
  src/main.ts      frontend
tools/slicer/   build-time CC0 slicer (never shipped)
spike/          latency measurement harness
NOTES.md        architecture, measurements, and decisions
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Platform work is especially welcome.

## License

[MIT](LICENSE). Bundled recordings are CC0; see [tools/fetch-reference.md](tools/fetch-reference.md) for sources.