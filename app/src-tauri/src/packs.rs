//! Loading sound packs from built-ins and the user's packs directory.
//!
//! Three sources, all ending up in the same [`audio::VoiceBank`] structure so
//! playback has exactly one path:
//!
//! 1. **Built-in recipes** - `sounds.json`, synthesised from modal parameters.
//! 2. **Recipe packs** - `packs/<name>/pack.json`, same schema as a built-in
//!    family. A whole pack is a few KB of text: no audio assets, no licensing.
//! 3. **Mechvibes / sample packs** - `packs/<name>/config.json` plus audio
//!    files. Decoded, resampled to the device rate and normalised at import so
//!    the original files never stay resident.
//!
//! Layout:
//! ```text
//! <config dir>/packs/
//!   my-thock/pack.json            recipe pack
//!   cherry-mx-blue/
//!     config.json                 mechvibes v1/v2
//!     sounds/*.ogg
//!   downloaded.zip                 extracted on scan, then left alone
//! ```

use crate::audio::{self, Family, KeySlot, Origin, Pack, PackSet, Voice};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Guard rails for untrusted packs. A sound file is a few hundred KB; a whole
/// pack should never be gigabytes.
const MAX_AUDIO_BYTES: u64 = 32 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILES: usize = 512;

/// Loads everything available: built-ins first, then the user's packs.
///
/// A broken user pack is logged and skipped. One bad file must never stop the
/// app from making sound.
pub fn load_all(config_dir: &Path, rate: u32) -> PackSet {
    let mut packs = builtin_packs(rate);
    let ui = audio::build_ui(&audio::builtin_ui(), rate);

    let user_dir = config_dir.join("packs");
    if let Err(e) = std::fs::create_dir_all(&user_dir) {
        eprintln!("[packs] cannot create {}: {e}", user_dir.display());
        return PackSet { packs, ui };
    }

    extract_archives(&user_dir);

    let mut entries: Vec<PathBuf> = match std::fs::read_dir(&user_dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(e) => {
            eprintln!("[packs] cannot read {}: {e}", user_dir.display());
            return PackSet { packs, ui };
        }
    };
    // Stable ordering so the selected index means the same thing next launch.
    entries.sort();

    for path in entries {
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let loaded = load_recipe(&path, rate)
            .or_else(|| load_mechvibes(&path, rate))
            .or_else(|| load_samples(&path, rate));
        match loaded {
            Some(p) => {
                println!("[packs] loaded {} ({} sounds)", p.name, p.voices.len());
                packs.push(p);
            }
            None => eprintln!("[packs] skipping unrecognised pack: {name}"),
        }
    }

    PackSet { packs, ui }
}

fn builtin_packs(rate: u32) -> Vec<Pack> {
    let mut packs: Vec<Pack> = audio::builtin_families()
        .into_iter()
        .map(|f: Family| Pack {
            id: f.id,
            name: f.name,
            origin: Origin::Builtin,
            voices: f
                .voices
                .iter()
                .map(|v| audio::bank_from_voice(v, rate))
                .collect(),
        })
        .collect();
    // Real recorded boards come first: they are the reference point every other
    // sound should be judged against.
    let mut recorded = builtin_recorded_packs(rate);
    recorded.append(&mut packs);
    recorded
}

/// Recorded keystrokes embedded at build time by `tools/slicer`, grouped into one
/// pack per source board.
fn builtin_recorded_packs(rate: u32) -> Vec<Pack> {
    use crate::slices_generated::{BOARDS, SLICES};

    let mut groups: Vec<(String, Vec<&'static [u8]>)> = Vec::new();
    for s in SLICES {
        let board = s.path.split('/').next().unwrap_or("recorded").to_string();
        match groups.iter_mut().find(|(n, _)| *n == board) {
            Some((_, v)) => v.push(s.wav),
            None => groups.push((board, vec![s.wav])),
        }
    }

    let mut out = Vec::with_capacity(groups.len());
    for (board, files) in groups {
        let mut voices = Vec::with_capacity(files.len());
        for bytes in files {
            let Some((audio, src_rate)) = decode_bytes(bytes) else {
                eprintln!("[packs] cannot decode embedded slice in {board}");
                continue;
            };
            let mut raw = audio::RawBank::empty();
            raw.rate = src_rate;
            raw.key = vec![
                vec![vec![audio.clone()]],
                vec![vec![audio.clone()]],
            ];
            raw.click = vec![vec![audio.clone()], vec![audio]];
            voices.push(audio::bank_from_raw(&raw, rate));
        }
        if voices.is_empty() {
            continue;
        }
        let name = BOARDS
            .iter()
            .find(|(s, _)| *s == board)
            .map(|(_, n)| n.to_string())
            .unwrap_or_else(|| board.clone());
        println!("[packs] loaded {name}: {} CC0 recorded strikes", voices.len());
        out.push(Pack {
            id: format!("rec-{}", slug(&name)),
            name,
            origin: Origin::Recorded,
            voices,
        });
    }
    out
}


/// A recipe pack is a family under a different filename, so the built-in schema
/// is reused exactly.
fn load_recipe(dir: &Path, rate: u32) -> Option<Pack> {
    let file = dir.join("pack.json");
    let text = std::fs::read_to_string(&file).ok()?;
    let family: Family = serde_json::from_str(&text)
        .map_err(|e| eprintln!("[packs] bad recipe {}: {e}", file.display()))
        .ok()?;
    if family.voices.is_empty() {
        return None;
    }
    Some(Pack {
        id: family.id.clone(),
        name: family.name,
        origin: Origin::Recipe,
        voices: family
            .voices
            .iter()
            .map(|v| audio::bank_from_voice(v, rate))
            .collect(),
    })
}

#[derive(Deserialize)]
struct MechvibesConfig {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    key_define_type: Option<String>,
    #[serde(default)]
    sound: Option<String>,
    #[serde(default)]
    soundup: Option<String>,
    #[serde(default)]
    defines: HashMap<String, serde_json::Value>,
}

/// Mechvibes `config.json`, versions 1 and 2.
///
/// v1 uses `key_define_type: "single"` with one file for everything. v2 adds
/// `soundup` and per-key `-up` entries, and both versions allow `{0-4}` brace
/// ranges meaning "pick one of these at random".
fn load_mechvibes(dir: &Path, rate: u32) -> Option<Pack> {
    let cfg_file = dir.join("config.json");
    let text = std::fs::read_to_string(&cfg_file).ok()?;
    let cfg: MechvibesConfig = serde_json::from_str(&text)
        .map_err(|e| eprintln!("[packs] bad mechvibes config {}: {e}", cfg_file.display()))
        .ok()?;

    let single = cfg.key_define_type.as_deref() == Some("single");
    let mut cache: HashMap<String, Option<(Vec<f32>, u32)>> = HashMap::new();

    // Mechvibes lists filenames; everything downstream wants decoded buffers.
    let decode_list = |files: &[String], cache: &mut HashMap<String, Option<(Vec<f32>, u32)>>| {
        files
            .iter()
            .filter_map(|rel| decode_cached(dir, rel, cache))
            .collect::<Vec<Vec<f32>>>()
    };

    // Defaults: anything not explicitly mapped falls back to `sound`.
    let down_files = expand_variants(cfg.sound.as_deref());
    let up_files = expand_variants(cfg.soundup.as_deref());
    let down_default: Vec<Vec<f32>> = decode_list(&down_files, &mut cache);
    let up_default: Vec<Vec<f32>> = if up_files.is_empty() {
        down_default.clone()
    } else {
        decode_list(&up_files, &mut cache)
    };

    let mut slot_down: Vec<Vec<Vec<f32>>> = vec![Vec::new(); audio::SLOTS];
    let mut slot_up: Vec<Vec<Vec<f32>>> = vec![Vec::new(); audio::SLOTS];
    for s in 0..audio::SLOTS {
        slot_down[s] = down_default.clone();
        slot_up[s] = up_default.clone();
    }
    let mut click: Vec<Vec<f32>> = down_default.clone();

    for (key, value) in &cfg.defines {
        let (code_str, is_up) = match key.strip_suffix("-up") {
            Some(k) => (k, true),
            None => (key.as_str(), false),
        };
        let Ok(code) = code_str.parse::<u32>() else {
            continue;
        };
        let files: Vec<String> = match value {
            serde_json::Value::Null => continue,
            serde_json::Value::String(s) => expand_variants(Some(s)),
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str())
                .flat_map(|s| expand_variants(Some(s)))
                .collect(),
            _ => continue,
        };
        if files.is_empty() {
            continue;
        }
        let decoded = decode_list(&files, &mut cache);
        if decoded.is_empty() {
            continue;
        }

        // v1 `single`: the pack is one sound for the whole board.
        if single {
            for s in 0..audio::SLOTS {
                slot_down[s] = decoded.clone();
                slot_up[s] = decoded.clone();
            }
            click = decoded;
            continue;
        }

        let Some(slot) = evdev_slot(code) else {
            continue;
        };
        let idx = slot.index();
        if is_up {
            slot_up[idx] = decoded;
        } else {
            slot_down[idx] = decoded;
        }
    }

    if slot_down.iter().all(|v| v.is_empty()) {
        eprintln!(
            "[packs] {}: no audio decoded (looked for {})",
            dir.display(),
            down_files.join(", ")
        );
        return None;
    }
    if click.is_empty() {
        click = slot_down[KeySlot::Normal.index()].clone();
    }

    // A Mechvibes pack is one sound: its per-key variation lives *inside* the
    // bank across the four slots, not as separate voices.
    //
    // Slot-major `[slot][variant]` has to become the bank's variant-major
    // `[variant][slot]`.
    let transposed = |src: &[Vec<Vec<f32>>]| -> Vec<Vec<Vec<f32>>> {
        let variants = src.iter().map(|v| v.len()).max().unwrap_or(0);
        (0..variants)
            .map(|v| {
                (0..audio::SLOTS)
                    .map(|s| {
                        src[s]
                            .get(v)
                            .cloned()
                            .or_else(|| src[0].get(v).cloned())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect()
    };

    let mut raw = audio::RawBank::empty();
    raw.rate = first_sample_rate(&cache, rate);
    raw.key = vec![transposed(&slot_down), transposed(&slot_up)];
    raw.click = vec![click.clone(), click];
    let voices = vec![audio::bank_from_raw(&raw, rate)];

    let name = cfg
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| {
            dir.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Imported pack".into())
        });

    Some(Pack {
        id: format!("mv-{}", slug(&name)),
        name,
        origin: Origin::Mechvibes,
        voices,
    })
}

/// The sample rate the pack's files actually use, which is rarely the device
/// rate. Fall back to the device rate if nothing decoded.
fn first_sample_rate(
    cache: &HashMap<String, Option<(Vec<f32>, u32)>>,
    fallback: u32,
) -> u32 {
    cache
        .values()
        .flatten()
        .map(|(_, r)| *r)
        .find(|r| *r > 0)
        .unwrap_or(fallback)
}

/// A folder of audio files with no `config.json`: one file per voice.
///
/// The simplest possible custom pack - drop `.wav` files in a folder and they
/// become selectable sounds. Key slots still get their character.
fn load_samples(dir: &Path, rate: u32) -> Option<Pack> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && matches!(
                    p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
                    Some("wav") | Some("mp3") | Some("ogg") | Some("flac") | Some("aac") | Some("m4a")
                )
        })
        .collect();
    if files.is_empty() {
        return None;
    }
    // Sorted so the voice list is stable between launches.
    files.sort();

    let mut voices = Vec::with_capacity(files.len());
    for f in &files {
        let Some((audio, src_rate)) = decode(f) else {
            eprintln!("[packs] cannot decode {}", f.display());
            continue;
        };
        let mut raw = audio::RawBank::empty();
        raw.rate = src_rate;
        raw.key = vec![
            vec![vec![audio.clone()]],
            vec![vec![audio.clone()]],
        ];
        raw.click = vec![vec![audio.clone()], vec![audio]];
        voices.push(audio::bank_from_raw(&raw, rate));
    }
    if voices.is_empty() {
        return None;
    }

    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Imported samples".into());
    Some(Pack {
        id: format!("samples-{}", slug(&name)),
        name,
        origin: Origin::Samples,
        voices,
    })
}

/// Linux evdev key codes, which is what Mechvibes packs are keyed by.
///
/// Only the codes that change how a key should sound are listed; everything else
/// falls back to the pack default, which is also what Mechvibes itself does.
fn evdev_slot(code: u32) -> Option<KeySlot> {
    Some(match code {
        57 => KeySlot::Space, // space
        28 | 96 => KeySlot::Enter, // enter, keypad enter
        42 | 54 => KeySlot::Modifier, // shift
        29 | 97 => KeySlot::Modifier, // control
        56 | 100 => KeySlot::Modifier, // alt
        125 | 126 => KeySlot::Modifier, // meta
        58 | 15 | 14 => KeySlot::Modifier, // caps lock, tab, backspace
        _ => return None,
    })
}

fn decode_cached(
    dir: &Path,
    rel: &str,
    cache: &mut HashMap<String, Option<(Vec<f32>, u32)>>,
) -> Option<Vec<f32>> {
    if let Some(hit) = cache.get(rel) {
        return hit.as_ref().map(|(b, _)| b.clone());
    }
    let safe = safe_relative_path(dir, rel)?;
    let out = decode(&safe);
    cache.insert(rel.to_string(), out.clone());
    out.map(|(b, _)| b)
}

/// Resolves a pack-relative path, refusing anything that escapes the directory.
///
/// Mechvibes packs are user-generated and sometimes come from zip files off the
/// internet, so a traversal attempt must not be able to read outside the pack.
fn safe_relative_path(dir: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    for c in rel_path.components() {
        match c {
            std::path::Component::Normal(_) => {}
            // Reject ParentDir, RootDir, Prefix explicitly.
            _ => return None,
        }
    }
    let full = dir.join(rel_path);
    let base = dir.canonicalize().ok()?;
    let canon = full.canonicalize().ok()?;
    if !canon.starts_with(&base) {
        return None;
    }
    Some(canon)
}

/// Expands `GENERIC{0-4}.mp3` into the five files it stands for.
///
/// A brace range is how Mechvibes expresses "pick one of these at random", which
/// is where per-strike variation in community packs comes from.
fn expand_variants(spec: Option<&str>) -> Vec<String> {
    let Some(spec) = spec else {
        return Vec::new();
    };
    let spec = spec.trim();
    if spec.is_empty() {
        return Vec::new();
    }
    let Some(open) = spec.find('{') else {
        return vec![spec.to_string()];
    };
    let Some(close) = spec[open..].find('}').map(|i| i + open) else {
        return vec![spec.to_string()];
    };
    let (prefix, body, suffix) = (&spec[..open], &spec[open + 1..close], &spec[close + 1..]);
    let (a, b) = match body.split_once('-') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (body.trim(), body.trim()),
    };
    let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>()) else {
        return vec![spec.to_string()];
    };
    if b < a || b - a > 64 {
        return vec![spec.to_string()];
    }
    (a..=b)
        .map(|i| format!("{prefix}{i}{suffix}"))
        .filter(|n| n.contains('.'))
        .collect()
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Decodes any supported file to mono f32 at its native sample rate.
pub fn decode(path: &Path) -> Option<(Vec<f32>, u32)> {
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::probe::Hint;

    let md = std::fs::metadata(path).ok()?;
    if md.len() > MAX_AUDIO_BYTES {
        eprintln!("[packs] {} is too large to decode", path.display());
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(&ext.to_ascii_lowercase());
    }
    decode_source(MediaSourceStream::new(Box::new(file), Default::default()), &hint, path)
}

/// Same as [`decode`] but for audio already in memory, used for the slices
/// embedded in the binary.
pub fn decode_bytes(bytes: &'static [u8]) -> Option<(Vec<f32>, u32)> {
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::probe::Hint;

    let mut hint = Hint::new();
    hint.with_extension("wav");
    // The cursor must outlive the stream, which borrows from it.
    let cursor = std::io::Cursor::new(bytes);
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    decode_source(mss, &hint, std::path::Path::new("<embedded>"))
}

fn decode_source(
    mss: symphonia::core::io::MediaSourceStream,
    hint: &symphonia::core::probe::Hint,
    path: &Path,
) -> Option<(Vec<f32>, u32)> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::audio::{Channels, SignalSpec};

    // Symphonia 0.5: `format`, taking options by reference.
    let probed = match symphonia::default::get_probe().format(
        hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[packs] cannot probe {}: {e}", path.display());
            return None;
        }
    };
    let mut format = probed.format;

    let Some(track) = format.default_track().cloned() else {
        eprintln!("[packs] no audio track in {}", path.display());
        return None;
    };
    let params = track.codec_params.clone();
    let rate = params.sample_rate?;
    let channels = params.channels.map(|c| c.count()).unwrap_or(1);
    let spec = SignalSpec::new(rate, params.channels.unwrap_or(Channels::FRONT_CENTRE));

    let mut decoder = match symphonia::default::get_codecs().make(&params, &DecoderOptions::default()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[packs] no decoder for {}: {e}", path.display());
            return None;
        }
    };

    let mut mono: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(e) => {
                // A truncated final packet is common in the wild; keep what we got.
                if mono.is_empty() {
                    eprintln!("[packs] {}: {e}", path.display());
                }
                break;
            }
        };
        if packet.track_id() != track.id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else {
            continue;
        };
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }

        // `copy_interleaved_ref` handles every sample type symphonia decodes,
        // so there is no per-format conversion here.
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded.clone());
        let written = buf.len().min(buf.samples().len());
        let interleaved = &buf.samples()[..written];

        if channels <= 1 {
            mono.extend_from_slice(interleaved);
        } else {
            for f in 0..written / channels {
                let mut acc = 0.0f32;
                for c in 0..channels {
                    acc += interleaved[f * channels + c];
                }
                mono.push(acc / channels as f32);
            }
        }
    }

    if mono.is_empty() {
        return None;
    }
    Some((mono, rate))
}

/// Unpacks any `.zip` sitting directly in the packs directory, then deletes the
/// archive so it is not re-extracted every launch.
fn extract_archives(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("zip") {
            continue;
        }
        match extract_zip(&path, dir) {
            Ok(name) => println!("[packs] extracted {name}"),
            Err(e) => eprintln!("[packs] zip {}: {e}", path.display()),
        }
        let _ = std::fs::remove_file(&path);
    }
}

fn extract_zip(zip_path: &Path, dest_root: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(std::io::Error::other)?;

    // Mechvibes packs are often a loose folder inside the zip.
    let prefix: std::path::PathBuf = {
        let first = archive.by_index(0)?;
        first
            .enclosed_name()
            .and_then(|p| p.components().next().map(|_| p.to_path_buf()))
            .unwrap_or_default()
    };

    let mut total = 0u64;
    let mut count = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else {
            eprintln!("[packs] refusing entry with unsafe path");
            continue;
        };
        // Skip the wrapper folder when the zip has one.
        let rel = rel.strip_prefix(&prefix).unwrap_or(&rel).to_path_buf();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest_root.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if count >= MAX_FILES {
            eprintln!("[packs] zip has more than {MAX_FILES} files, truncating");
            break;
        }
        total += entry.size();
        if total > MAX_UNPACKED_BYTES {
            eprintln!("[packs] zip exceeds unpack size limit, truncating");
            break;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::with_capacity(entry.size() as usize);
        std::io::copy(&mut entry, &mut buf)?;
        std::fs::write(&out, buf)?;
        count += 1;
    }

    let name = if prefix.as_os_str().is_empty() {
        zip_path.file_stem().map(|s| s.to_string_lossy().to_string())
    } else {
        Some(prefix.to_string_lossy().to_string())
    };
    Ok(name.unwrap_or_else(|| "archive".into()))
}

/// Writes a recipe pack to the user's packs directory.
pub fn save_recipe(
    config_dir: &Path,
    id: &str,
    name: &str,
    voices: &[Voice],
) -> std::io::Result<PathBuf> {
    let safe_id = slug(id);
    let dir = config_dir.join("packs").join(format!("recipe-{safe_id}"));
    std::fs::create_dir_all(&dir)?;
    let family = Family {
        id: safe_id,
        name: name.to_string(),
        voices: voices.to_vec(),
    };
    let json = serde_json::to_string_pretty(&family).map_err(std::io::Error::other)?;
    let path = dir.join("pack.json");
    std::fs::write(&path, json)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Perceptual designer
// ---------------------------------------------------------------------------

/// Maps the four perceptual axes onto a voice's synthesis parameters.
///
/// The axes are the ones the mechanical keyboard community actually uses to
/// describe preference: **pitch, attack, resonance, loudness**. Because a recipe
/// voice is a physical model rather than a recording, those four words map onto
/// real parameters instead of needing an EQ curve.
pub fn morph(base: &Voice, pitch: f32, attack: f32, resonance: f32, loudness: f32) -> Voice {
    let pitch = pitch.clamp(0.0, 1.0);
    let attack = attack.clamp(0.0, 1.0);
    let resonance = resonance.clamp(0.0, 1.0);
    let loudness = loudness.clamp(0.0, 1.0);

    // Pitch: a little over two octaves of travel, so extremes stay useful.
    let freq_scale = 0.62 * (2.2f32 / 0.62).powf(pitch);
    // Attack: sharp transient to fully rounded.
    let impact_decay = 0.0004 + (0.0075 - 0.0004) * attack;
    let attack_ms = 0.10 + (3.0 - 0.10) * attack;
    let impact_hp = 5200.0 - 4200.0 * attack;
    // Resonance: tightly damped to ringing.
    let decay_scale = 0.30 + (2.6 - 0.30) * resonance;
    // Loudness maps to the render target peak. Scaling `amp` instead would be
    // undone by `render`'s normalisation.
    let mut v = base.clone();
    v.peak = 0.18 + 0.80 * loudness;
    for m in &mut v.modes {
        m.freq_hz *= freq_scale;
        m.decay_s *= decay_scale;
    }
    v.impact.decay_s = impact_decay;
    v.impact.lowpass_hz = impact_hp;
    v.attack_ms = attack_ms;
    // More resonance means more early reflection too, or it sounds like a
    // filtered click rather than a resonant body.
    v.room = base
        .room
        .iter()
        .map(|(d, g)| (*d, g * (0.5 + 1.1 * resonance)))
        .collect();
    v
}

#[cfg(test)]
mod recorded_tests {
    use super::*;
    use crate::audio::Gesture;

    /// Guards the whole build-time-to-runtime path: the slicer embeds these WAVs,
    /// so a bad slice must fail here rather than as silence at the user's keypress.
    #[test]
    fn embedded_slices_decode_into_usable_voices() {
        let packs = builtin_recorded_packs(48_000);
        assert!(!packs.is_empty(), "no recorded packs were built in");

        let mut total = 0;
        for p in &packs {
            assert!(p.origin == Origin::Recorded, "{} has wrong origin", p.name);
            for (i, v) in p.voices.iter().enumerate() {
                // press and release, one variant, four key slots
                assert_eq!(v.key.len(), 2, "{} voice {i} gestures", p.name);
                assert_eq!(v.click.len(), 2, "{} voice {i} clicks", p.name);

                let s = v.get(Gesture::Press, 0, KeySlot::Normal).expect("alpha slot");
                assert!(s.len() > 200, "{} voice {i} suspiciously short", p.name);

                // A recorded strike must not clip, and must not be silent.
                let peak = s.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
                assert!(peak > 0.1 && peak <= 1.01, "{} voice {i} peak {peak}", p.name);

                // Resampled to the device rate and length-plausible: 5-250 ms.
                let ms = s.len() as f32 * 1000.0 / 48_000.0;
                assert!(
                    (5.0..250.0).contains(&ms),
                    "{} voice {i} is {ms:.1} ms",
                    p.name
                );
                total += 1;
            }
        }
        assert!(total >= 2, "only {total} recorded voices built");
        println!("{total} recorded voices across {} packs", packs.len());
    }

    /// Every slice must be near-silent at the seam, or clicks will fire twice:
    /// once from the recording and once from the buffer boundary.
    #[test]
    fn slices_start_and_end_quiet() {
        // Only the boundary samples matter: a non-zero first or last sample is a
        // discontinuity, and the slicer ramps the rest of the edge on purpose.
        for s in crate::slices_generated::SLICES {
            let (audio, _rate) = decode_bytes(s.wav).expect("slice decodes");
            let hp = audio[0].abs();
            let tp = audio[audio.len() - 1].abs();
            assert!(hp < 1e-6, "{} starts at {hp}", s.path);
            assert!(tp < 1e-6, "{} ends at {tp}", s.path);
        }
    }
}
