# Reference recordings

The built-in boards were sliced from CC0 recordings downloaded from Freesound. The
derived slices are committed at `app/src-tauri/assets/slices/` and embedded in the
binary, so **you do not need anything in `reference/` to build or run the app.**

You only need these files if you want to change how the built-in boards are sliced
(for example, to tune slice length, the high-pass corner, or the de-duplication
threshold).

## Getting the files

`reference/` is gitignored because it is ~69MB of third-party audio.

**Freesound requires a login to download.** There is no anonymous download, so this
cannot be scripted. Download each item in a browser while logged in, then move it
into `reference/`.

| File | Destination name | Source | License |
| --- | --- | --- | --- |
| `keyboard-typing-1-hhkb-topre` | `546032__grcekh__keyboard-typing-1-hhkb-topre.mp3` | <https://freesound.org/people/grcekh/sounds/546032/> | CC0 |
| `keyboard-typing-4-whitefox-mechanical` | `546045__grcekh__keyboard-typing-4-whitefox-mechanical.mp3` | <https://freesound.org/people/grcekh/sounds/546045/> | CC0 |
| "Mechanical Keyboard Typing" pack | `25510__stu556__mechanical-keyboard-typing/` | <https://freesound.org/people/stu556/packs/25510/> | CC0 |

## Filenames matter

Board detection in `tools/slicer` matches on substrings of the filename, not on any
metadata inside the audio:

| Substring in path | Resulting pack |
| --- | --- |
| `hhkb`, `topre` | HHKB Topre |
| `whitefox`, `hako` | WhiteFox Hako Violet |
| `fc660`, `stu556` | Leopold FC660M |
| `mechanical keyboard typing` | Mechanical |

Keep the filenames above, or edit `BOARD_ALIASES` in `tools/slicer/src/main.rs`.
Wrong names do not fail loudly: they silently produce a pack called "Mechanical".

The stu556 pack contains a bass and a treble EQ variant of the *same* recording. They
are not two boards, and they correctly merge into one FC660M pack.

## Then re-slice

```bash
tools/slicer/target/release/slicer reference app/src-tauri/assets/slices
cd app/src-tauri && cargo build --release --features custom-protocol
```

Read the calibration table the slicer prints. It reports peak frequency, spectral
centroid, decay duration and HF sharpness per board; these numbers are the target the
designer and any future synthesizer should be tuned against.

Verify your change did not break the measurements:

```bash
tools/slicer/target/release/slicer --selftest
cd app/src-tauri && cargo test --release
```