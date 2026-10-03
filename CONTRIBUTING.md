# Contributing

Early project, small surface area, no roadmap beyond "make it good on Windows and
macOS". Platform work is the most useful contribution.

## Getting set up

```bash
git clone https://github.com/Faisal-Fayaz/thockboard
cd thockboard/app
pnpm install
pnpm tauri dev
```

Linux needs system packages first (Tauri plus the X11 input hooks):

```bash
sudo apt install libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
                 libayatana-appindicator3-dev librsvg2-dev \
                 libasound2-dev build-essential
```

## Before you build with cargo

`pnpm build` must run before `cargo`, because `generate_context!` embeds the built
frontend at compile time and fails without `dist/`.

```bash
pnpm build
cd src-tauri
cargo build --release --features custom-protocol
./target/release/thockboard
```

The `custom-protocol` feature is what embeds the frontend. Without it you get a dev
binary that reports "Could not connect to localhost", which reads like a networking
bug but is not. `pnpm tauri dev` and `pnpm tauri build` both handle this for you.

## Platform notes

Read these before implementing anything that touches input or window focus.

**Linux is X11 only.** Wayland is unsupported. There is no dependable global keyboard
hook on Wayland, and `rules.rs` uses X11 (`XGetInputFocus`) to identify the focused
window. If you add per-app support elsewhere, follow the existing pattern: a
`#[cfg]`-gated implementation plus a fallback that returns `None`.

**macOS needs Accessibility permission.** Global keyboard events require the user to
grant it in System Settings → Privacy & Security → Accessibility. Without it, hooks
fail silently. There is currently no in-app prompt, which is a real gap and a good
first task.

**Key release never fires on Linux X11.** `rdev`'s X11 backend does not deliver it.
That is why the mute hotkey uses sticky modifier tracking plus a 500ms debounce
instead of key-up detection. On Windows and macOS `KeyRelease` *is* delivered, so
code that assumes it will silently misbehave on Linux and work elsewhere. Keep that
asymmetry in mind rather than "fixing" it.

**Global dependency declarations.** Linux-only crates belong under
`[target.'cfg(target_os = "linux")'.dependencies]`, not `[dependencies]`. `x11` is
declared this way; declaring it unconditionally breaks Windows and macOS builds.

## Tests

```bash
cd app/src-tauri && cargo test --release      # pack decoding and slice integrity
tools/slicer/target/release/slicer --selftest # 15 DSP checks
```

The self-test checks measurement code against signals with known answers. If you
touch filtering, onset detection, fingerprints or statistics, run it. Two of its
assertions encode deliberate design decisions that look like bugs:

- Sharpness is the fraction of energy above a fixed 2kHz split, so a 2.5kHz tone
  correctly scores 1.0.
- The fingerprint is amplitude-invariant, so scaling a signal must not change it. The
  property that matters is that *different spectra* produce different fingerprints.

## Changing the built-in sounds

The built-in boards are recordings, not synthesis. See
[tools/fetch-reference.md](tools/fetch-reference.md) for how to obtain the CC0 sources
and re-slice them.

Two rules the slicer enforces, both of which exist because ignoring them produced
audibly wrong output:

- **No hard slice edges.** Every slice fades to exactly zero at both ends. A
  non-zero boundary sample is a discontinuity, and it clicks on every keypress.
- **Only clean single strikes.** Slices are rejected on crest factor and energy
  front-loading, because a naive search also returns two impacts in one slice. The
  counts of rejected candidates are printed for this reason.

## Style

Match the surrounding code. No comments explaining *what* the code does; comment the
non-obvious *why*, especially anything about audio timing, X11, or platform quirks.
`NOTES.md` documents the reasoning behind most decisions, including the measurements
behind latency claims.