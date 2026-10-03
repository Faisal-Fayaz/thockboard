//! Turns long keyboard recordings into short, normalised, single-strike assets.
//!
//! The reference clips are CC0 recordings of a person typing 50-word passages.
//! What matters is the tail, where individual keys are struck back-to-back
//! rather than overlapped, so onset detection there yields clean single impacts
//! instead of the mush of continuous typing.
//!
//! Everything here is a build-time tool. It reads from `reference/` and writes
//! assets into the app. It never ships and never runs in the app.

use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// How far back from the detected onset to start the slice. The impact has a
/// few milliseconds of rise, so starting before it keeps the transient intact.
const PRE_ROLL_S: f32 = 0.006;
/// Ramp at each edge. Recordings usually sit near their noise floor at the slice
/// boundary, but that is not guaranteed, and a hard cut is an audible click on
/// every keypress. The fade-in completes before the onset so the transient peak
/// keeps its full amplitude.
const FADE_IN_S: f32 = 0.004;
const FADE_OUT_S: f32 = 0.004;
/// Slice length. Measured switches ring for 10-70 ms; 45 ms captures the whole
/// event for every profile without carrying dead air.
const SLICE_LEN_S: f32 = 0.045;
/// Minimum spacing between accepted strikes, so one keypress is not counted as
/// several.
const MIN_GAP_S: f32 = 0.070;
/// Below this many accepted strikes we assume the isolated tail was shorter and
/// widen the search.
const MIN_STRIKES: usize = 6;
/// Envelope hop size, seconds. Onset indices are in these units.
const ENVELOPE_HOP_S: f32 = 0.002;
/// Cuts desk rumble, HVAC and handling noise. Real switch content starts well
/// above this; anything lower is room, not board.
const HIGHPASS_HZ: f32 = 150.0;
/// The recordings carry a hiss floor that dominates above a few kHz: measured,
/// 65% of the energy sat above 3.2kHz with no dominant resonance, which is noise
/// rather than switch audio. Keystrokes roll off well before Nyquist.
const LOWPASS_HZ: f32 = 12_000.0;
/// Two strikes closer than this in perceptual space are the same hit twice.
const DEDUP_THRESHOLD: f32 = 0.30;
/// Slices to keep per board.
const MAX_PER_BOARD: usize = 10;
const TARGET_PEAK: f32 = 0.89;
/// Quality gate. The tail region has keys struck back-to-back, so a naive search
/// also returns two impacts in one slice and anything with room tone. Real single
/// strikes are far more impulsive than that.
const MIN_CREST: f32 = 4.0;
/// Energy in the first third relative to the last third. A single impact decays,
/// so this is high; overlapping strikes or steady noise sit near 1.
const MIN_FRONTLOADING: f32 = 3.0;

/// Filename fragment -> board name, so the packs are named after the real
/// hardware rather than an upload filename.
const BOARD_ALIASES: &[(&str, &str)] = &[
    ("hhkb", "HHKB Topre"),
    ("topre", "HHKB Topre"),
    ("whitefox", "WhiteFox Hako Violet"),
    ("hako", "WhiteFox Hako Violet"),
    ("fc660", "Leopold FC660M"),
    ("stu556", "Leopold FC660M"),
    ("mechanical keyboard typing", "Mechanical"),
];

// ---------------------------------------------------------------------------
// FFT
// ---------------------------------------------------------------------------

/// In-place iterative radix-2 Cooley-Tukey.
struct Fft {
    re: Vec<f32>,
    im: Vec<f32>,
    rev: Vec<usize>,
    cos: Vec<f32>,
    sin: Vec<f32>,
}

impl Fft {
    fn new(n: usize) -> Self {
        assert!(n.is_power_of_two());
        let bits = n.trailing_zeros();
        let mut rev = vec![0usize; n];
        for i in 0..n {
            let mut r = 0usize;
            for b in 0..bits {
                r = (r << 1) | ((i >> b) & 1);
            }
            rev[i] = r;
        }
        let mut cos = vec![0.0; n / 2];
        let mut sin = vec![0.0; n / 2];
        for i in 0..n / 2 {
            cos[i] = (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos();
            sin[i] = (2.0 * std::f32::consts::PI * i as f32 / n as f32).sin();
        }
        Self {
            re: vec![0.0; n],
            im: vec![0.0; n],
            rev,
            cos,
            sin,
        }
    }

    /// Transforms `input` (zero-padded or truncated to `n`) into the FFT.
    fn forward(&mut self, input: &[f32]) -> Vec<f32> {
        let n = self.re.len();
        for i in 0..n {
            self.re[i] = input.get(i).copied().unwrap_or(0.0);
            self.im[i] = 0.0;
        }
        for i in 0..n {
            let j = self.rev[i];
            if j > i {
                self.re.swap(i, j);
                self.im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            let mut i = 0;
            while i < n {
                for k in 0..half {
                    // Twiddle for this stage: exp(-2*pi*i*k/len), precomputed
                    // as index k * (n/len). Using 2*k here overruns the table.
                    let j = k * step;
                    let wr = self.cos[j];
                    let wi = -self.sin[j];
                    let a = i + k;
                    let b = a + half;
                    let xr = self.re[b] * wr - self.im[b] * wi;
                    let xi = self.re[b] * wi + self.im[b] * wr;
                    self.re[b] = self.re[a] - xr;
                    self.im[b] = self.im[a] - xi;
                    self.re[a] += xr;
                    self.im[a] += xi;
                }
                i += len;
            }
            len <<= 1;
        }
        (0..n / 2)
            .map(|i| (self.re[i].powi(2) + self.im[i].powi(2)).sqrt())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

fn decode(path: &Path) -> Option<(Vec<f32>, u32)> {
    use symphonia::core::audio::{Channels, SampleBuffer, SignalSpec};
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(e) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(&e.to_ascii_lowercase());
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;
    let mut format = probed.format;
    let track = format.default_track()?.clone();
    let params = track.codec_params.clone();
    let rate = params.sample_rate?;
    let channels = params.channels.map(|c| c.count()).unwrap_or(1);
    let spec = SignalSpec::new(rate, params.channels.unwrap_or(Channels::FRONT_CENTRE));

    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .ok()?;
    let mut mono = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break,
        };
        if packet.track_id() != track.id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        if decoded.frames() == 0 {
            continue;
        }
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded.clone());
        let w = buf.len().min(buf.samples().len());
        let il = &buf.samples()[..w];
        if channels <= 1 {
            mono.extend_from_slice(il);
        } else {
            for f in 0..w / channels {
                let mut acc = 0.0;
                for c in 0..channels {
                    acc += il[f * channels + c];
                }
                mono.push(acc / channels as f32);
            }
        }
    }
    if mono.is_empty() {
        None
    } else {
        Some((mono, rate))
    }
}

/// Short-time energy envelope in dB.
fn envelope(x: &[f32], rate: u32) -> Vec<f32> {
    let hop = ((rate as f32) * ENVELOPE_HOP_S) as usize;
    let win = ((rate as f32) * 0.005) as usize; // 5 ms
    if hop == 0 || win == 0 {
        return Vec::new();
    }
    x.chunks(hop)
        .map(|chunk| {
            let s: f32 = chunk.iter().take(win).map(|v| v * v).sum();
            let rms = (s / chunk.len().max(1) as f32).sqrt();
            20.0 * rms.max(1e-9).log10()
        })
        .collect()
}

/// Indices of strikes: local maxima in the envelope, strongest first, with a
/// minimum separation so one keypress is not counted repeatedly.
fn find_strikes(env: &[f32], from_frac: f32) -> Vec<usize> {
    if env.len() < 8 {
        return Vec::new();
    }
    let peak = env.iter().cloned().fold(f32::MIN, f32::max);
    let threshold = (peak - 24.0).max(-70.0);
    let guard = 3usize;
    // `env` is indexed in 2 ms hops, so the gap has to be too. Comparing a
    // sample count against frame indices rejected nearly every strike.
    let min_gap = ((MIN_GAP_S / ENVELOPE_HOP_S) as usize).max(1);

    let start = (env.len() as f32 * from_frac) as usize;
    let mut cands: Vec<usize> = Vec::new();
    for i in start + guard..env.len().saturating_sub(guard) {
        if env[i] < threshold {
            continue;
        }
        let is_peak = env[i] >= env[i - 1..=i + 1].iter().cloned().fold(f32::MIN, f32::max);
        if is_peak {
            cands.push(i);
        }
    }
    // Strongest first, then greedily accept with separation.
    cands.sort_by(|a, b| env[*b].partial_cmp(&env[*a]).unwrap_or(std::cmp::Ordering::Equal));
    let mut chosen: Vec<usize> = Vec::new();
    for c in cands {
        if chosen.iter().all(|&o| c.abs_diff(o) >= min_gap) {
            chosen.push(c);
        }
    }
    chosen.sort_unstable();
    chosen
}

/// One-pole high-pass, to drop desk rumble the microphone picked up.
/// Cosine ramp in and out so the buffer boundaries are continuous.
fn fade_edges(x: &mut [f32], rate: u32) {
    let n = x.len();
    let fin = ((rate as f32 * FADE_IN_S) as usize).min(n / 2);
    let fout = ((rate as f32 * FADE_OUT_S) as usize).min(n / 2);
    for i in 0..fin {
        let g = 0.5 - 0.5 * (std::f32::consts::PI * i as f32 / fin as f32).cos();
        x[i] *= g;
    }
    for i in 0..fout {
        let g = 0.5 - 0.5 * (std::f32::consts::PI * i as f32 / fout as f32).cos();
        x[n - 1 - i] *= g;
    }
}

/// Removes DC offset. Recorded clips carry enough of it to dominate the low end
/// and drag the measured peak frequency down toward the high-pass corner.
/// Two cascaded one-pole sections, so 12dB/octave.
fn lowpass(x: &mut [f32], rate: u32) {
    let a = 1.0 - (-2.0 * std::f32::consts::PI * LOWPASS_HZ / rate as f32).exp();
    for _ in 0..2 {
        let mut y = 0.0;
        for s in x.iter_mut() {
            y += a * (*s - y);
            *s = y;
        }
    }
}

fn remove_dc(x: &mut [f32]) {
    let mean = x.iter().sum::<f32>() / x.len() as f32;
    for v in x.iter_mut() {
        *v -= mean;
    }
}

/// (crest factor, front-loading ratio). Both are scale-invariant.
fn quality(x: &[f32]) -> (f32, f32) {
    let peak = x.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    let rms = (x.iter().map(|&v| v * v).sum::<f32>() / x.len() as f32).sqrt();
    let crest = if rms > 1e-9 { peak / rms } else { 0.0 };
    let third = x.len() / 3;
    let e1: f32 = x[..third].iter().map(|&v| v * v).sum();
    let e2: f32 = x[x.len() - third..].iter().map(|&v| v * v).sum();
    (crest, if e2 > 1e-12 { e1 / e2 } else { 0.0 })
}

fn highpass(x: &mut [f32], rate: u32) {
    let a = (-2.0 * std::f32::consts::PI * HIGHPASS_HZ / rate as f32).exp();
    let mut prev_x = 0.0;
    let mut prev_y = 0.0;
    for s in x.iter_mut() {
        let y = a * (prev_y + *s - prev_x);
        prev_x = *s;
        prev_y = y;
        *s = y;
    }
}

fn normalise(x: &mut [f32]) {
    let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak > 1e-6 {
        let k = TARGET_PEAK / peak;
        for s in x.iter_mut() {
            *s = (*s * k).clamp(-1.0, 1.0);
        }
    }
}

/// Log-spaced band energies: a cheap perceptual fingerprint for de-duplication.
fn fingerprint(x: &[f32], rate: u32) -> Vec<f32> {
    let n = 1024usize;
    let mut fft = Fft::new(n);
    let window: Vec<f32> = (0..n)
        .map(|i| {
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos()
        })
        .collect();
    let input: Vec<f32> = x.iter()
        .take(n)
        .enumerate()
        .map(|(i, v)| v * window[i])
        .collect();
    let mags = fft.forward(&input);
    let nyquist = rate as f32 / 2.0;
    let bands = 24;
    let lo = 200.0f32;
    let hi = 12_000.0f32.min(nyquist);
    let mut out = Vec::with_capacity(bands);
    for b in 0..bands {
        let f0 = lo * (hi / lo).powf(b as f32 / bands as f32);
        let f1 = lo * (hi / lo).powf((b + 1) as f32 / bands as f32);
        let i0 = (f0 / nyquist * mags.len() as f32) as usize;
        let i1 = (((f1 / nyquist) * mags.len() as f32).ceil() as usize).max(i0 + 1);
        let e: f32 = mags[i0..i1.min(mags.len())].iter().sum();
        out.push(e.max(1e-9).log10());
    }
    let mean = out.iter().sum::<f32>() / out.len() as f32;
    out.iter().map(|v| (v - mean) * 0.1).collect()
}

fn distance(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum()
}

// ---------------------------------------------------------------------------
// Statistics used for calibration
// ---------------------------------------------------------------------------

struct Stats {
    peak_hz: f32,
    centroid_hz: f32,
    duration_ms: f32,
    /// Share of energy above 2 kHz. HEAD Acoustics identifies this ratio as the
    /// driver of whether a keyboard noise reads as pleasant or harsh.
    sharpness: f32,
}

fn stats(x: &[f32], rate: u32) -> Stats {
    let n = 1024usize;
    let mut fft = Fft::new(n);
    let window: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos())
        .collect();
    let input: Vec<f32> = x.iter().take(n).enumerate().map(|(i, v)| v * window[i]).collect();
    let mags = fft.forward(&input);
    let nyquist = rate as f32 / 2.0;

    let peak_bin = mags
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let peak_hz = peak_bin as f32 / mags.len() as f32 * nyquist;

    let total: f32 = mags.iter().sum();
    let centroid = if total > 1e-12 {
        mags.iter()
            .enumerate()
            .map(|(i, m)| (i as f32 / mags.len() as f32 * nyquist) * m)
            .sum::<f32>()
            / total
    } else {
        0.0
    };

    let split = (2000.0 / nyquist * mags.len() as f32) as usize;
    let hi: f32 = mags[split.min(mags.len())..].iter().sum();
    let sharpness = if total > 1e-12 { hi / total } else { 0.0 };

    // Decay time: from the envelope peak until 40 dB down. Scanning from sample
    // zero would report 0 ms, because every slice fades in from silence.
    let env = envelope(x, rate);
    let (peak_i, &peak) = env
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((0, &0.0));
    let mut dur = 0.0;
    for v in &env[peak_i..] {
        if *v < peak - 40.0 {
            break;
        }
        dur += ENVELOPE_HOP_S * 1000.0;
    }

    Stats {
        peak_hz,
        centroid_hz: centroid,
        duration_ms: dur,
        sharpness,
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn board_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    for (k, v) in BOARD_ALIASES {
        if stem.contains(k) {
            return (*v).to_string();
        }
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Imported".into())
}

fn write_wav(path: &Path, x: &[f32], rate: u32) -> Result<(), Box<dyn std::error::Error>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in x {
        w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    w.finalize()?;
    Ok(())
}

/// Checks the measurement primitives against signals with known answers, without
/// running them through the slicing pipeline. The quality gate correctly rejects
/// a smooth synthetic tone as not being a keystroke, so the DSP has to be
/// verified directly.
fn selftest() -> Result<(), Box<dyn std::error::Error>> {
    let rate = 44_100u32;
    let mut failures = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("  {} {name}: {detail}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failures += 1;
        }
    };

    // Pure tone: peak, centroid and sharpness all land on the tone frequency.
    for f0 in [1000.0f32, 2500.0] {
        let n = 1984usize;
        let x: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * f0 * i as f32 / rate as f32).sin())
            .collect();
        let s = stats(&x[264..], rate);
        check(
            &format!("{f0:.0}Hz tone peak"),
            (s.peak_hz - f0).abs() < f0 * 0.05,
            format!("got {:.0}Hz", s.peak_hz),
        );
        check(
            &format!("{f0:.0}Hz tone centroid"),
            (s.centroid_hz - f0).abs() < f0 * 0.15,
            format!("got {:.0}Hz", s.centroid_hz),
        );
        let expect_sharp = if f0 < 2000.0 { "< 0.05" } else { "~1.00" };
        let sharp_ok = if f0 < 2000.0 { s.sharpness < 0.05 } else { s.sharpness > 0.95 };
        check(
            &format!("{f0:.0}Hz tone sharpness {expect_sharp}"),
            sharp_ok,
            format!("got {:.3}", s.sharpness),
        );
    }

    // Broadband noise is diffuse: low crest, no front-loading. A single impact is
    // the opposite. This is the gate's whole basis.
    let n = 1984usize;
    let mut seed = 12345u32;
    let mut noise = Vec::with_capacity(n);
    for _ in 0..n {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        noise.push((seed >> 8) as f32 / 8388608.0 - 1.0);
    }
    let (nc, nf) = quality(&noise);
    check("noise crest is low", nc < 3.0, format!("got {nc:.2}"));
    check("noise not front-loaded", nf < 2.0, format!("got {nf:.2}"));

    // Decaying impact: crest high, energy front-loaded.
    let imp: Vec<f32> = (0..n)
        .map(|i| {
            let e = (-(i as f32) / (0.010 * rate as f32)).exp();
            (2.0 * std::f32::consts::PI * 1200.0 * i as f32 / rate as f32).sin() * e
        })
        .collect();
    let (ic, ifr) = quality(&imp);
    check("impact crest is high", ic > 4.0, format!("got {ic:.2}"));
    check("impact front-loaded", ifr > 3.0, format!("got {ifr:.2}"));

    // Fades reach exactly zero at both ends.
    let mut f = vec![0.7f32; n];
    fade_edges(&mut f, rate);
    check("fade-in reaches zero", f[0].abs() < 1e-6, format!("got {}", f[0]));
    check(
        "fade-out reaches zero",
        f[n - 1].abs() < 1e-6,
        format!("got {}", f[n - 1]),
    );

    // Identical strikes must be caught as duplicates, different ones must not.
    let fp = fingerprint(&imp, rate);
    check("fingerprint is self-similar", distance(&fp, &fingerprint(&imp, rate)) < 1e-6, String::new());
    let mut loud = imp.clone();
    for v in loud.iter_mut() {
        *v *= 0.5;
    }
    check(
        "fingerprint ignores loudness",
        distance(&fp, &fingerprint(&loud, rate)) < 1e-3,
        String::new(),
    );
    let mut bright = imp.clone();
    for (i, v) in bright.iter_mut().enumerate() {
        *v = (2.0 * std::f32::consts::PI * 3000.0 * i as f32 / rate as f32).sin()
            * (-(i as f32) / (0.010 * rate as f32)).exp();
    }
    check(
        "fingerprint separates a brighter strike",
        distance(&fp, &fingerprint(&bright, rate)) > 0.5,
        String::new(),
    );

    if failures > 0 {
        return Err(format!("{failures} selftest failure(s)").into());
    }
    println!("selftest: all checks passed");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--selftest") {
        return selftest();
    }

    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "../reference".into()),
    );
    let out_dir = PathBuf::from(
        std::env::args()
            .nth(2)
            .unwrap_or_else(|| "../app/src-tauri/assets/slices".into()),
    );

    if !root.is_dir() {
        println!("reference directory {} does not exist", root.display());
        return Ok(());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    collect_audio(&root, &mut files);
    files.sort();
    if files.is_empty() {
        println!("no audio files in {}", root.display());
        return Ok(());
    }

    fs::create_dir_all(&out_dir)?;

    let mut boards: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for f in files {
        let b = board_name(&f);
        match boards.iter_mut().find(|(n, _)| *n == b) {
            Some((_, v)) => v.push(f),
            None => boards.push((b, vec![f])),
        }
    }

    let mut manifest: Vec<serde_json::Value> = Vec::new();
    let mut board_names: Vec<(String, String)> = Vec::new();
    let mut generated: Vec<(String, PathBuf)> = Vec::new();
    let mut all_stats: Vec<(String, String, Stats)> = Vec::new();

    for (board, sources) in &boards {
        let board_dir = out_dir.join(slug(board));
        fs::create_dir_all(&board_dir)?;

        let mut kept: Vec<(Vec<f32>, Vec<f32>, Stats)> = Vec::new();
        let mut rejected = 0usize;
        let mut rate: u32 = 44_100;

        for src in sources {
            let Some((audio, src_rate)) = decode(src) else {
                println!("  ! cannot decode {}", src.display());
                continue;
            };
            rate = src_rate;
            // Isolate the tail where keys are struck individually.
            let env = envelope(&audio, src_rate);
            let mut onsets: Vec<usize> = find_strikes(&env, 0.70);
            if onsets.len() < MIN_STRIKES {
                onsets = find_strikes(&env, 0.40);
            }
            if onsets.len() < MIN_STRIKES {
                onsets = find_strikes(&env, 0.0);
            }

            let pre = ((src_rate as f32) * PRE_ROLL_S) as usize;
            let len = ((src_rate as f32) * SLICE_LEN_S) as usize;

            for &frame in &onsets {
                if kept.len() >= MAX_PER_BOARD {
                    break;
                }
                let on = frame * ((rate as f32) * ENVELOPE_HOP_S) as usize;
                let start = on.saturating_sub(pre);
                if start + len >= audio.len() {
                    continue;
                }
                let mut slice = audio[start..start + len].to_vec();
                remove_dc(&mut slice);
                // Two poles: one leaves the sub-100Hz rumble that drags the
                // measured peak down towards the corner frequency.
                highpass(&mut slice, src_rate);
                highpass(&mut slice, src_rate);
                lowpass(&mut slice, src_rate);
                normalise(&mut slice);

                let (crest, front) = quality(&slice);
                if crest < MIN_CREST || front < MIN_FRONTLOADING {
                    rejected += 1;
                    continue;
                }

                // Measure before the fade. The ramp is our processing, not the
                // recording, and its step-like onset inflates centroid and
                // sharpness by a noticeable margin.
                // Analyse only the event itself. The pre-roll is silence by
                // construction, and including it shifts where the transient sits
                // under the Hann taper, which moves centroid and sharpness.
                let event = &slice[pre..];
                let fp = fingerprint(event, src_rate);
                if kept.iter().any(|(_, k, _)| distance(k, &fp) < DEDUP_THRESHOLD) {
                    continue;
                }
                let st = stats(event, src_rate);
                // Fade last, so the ramp finishes at exactly the target peak.
                fade_edges(&mut slice, src_rate);
                kept.push((slice, fp, st));
            }
            println!(
                "  {} -> {} kept, {} rejected as not a clean single strike from {}",
                src.file_name().unwrap().to_string_lossy(),
                kept.len(),
                rejected,
                board
            );
        }

        kept.sort_by(|a, b| {
            b.2.centroid_hz
                .partial_cmp(&a.2.centroid_hz)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        kept.truncate(MAX_PER_BOARD);

        let mut names = Vec::new();
        let mut board_stats = Vec::new();
        for (i, (slice, _, st)) in kept.iter().enumerate() {
            let name = format!("{:02}.wav", i + 1);
            let path = board_dir.join(&name);
            write_wav(&path, slice, rate)?;
            generated.push((format!("{}/{}", slug(board), name), path));
            names.push(name.clone());
            board_stats.push(st);
            all_stats.push((board.clone(), name.clone(), Stats {
                peak_hz: st.peak_hz,
                centroid_hz: st.centroid_hz,
                duration_ms: st.duration_ms,
                sharpness: st.sharpness,
            }));
        }

        board_names.push((slug(board), board.clone()));
        manifest.push(serde_json::json!({
            "name": board.clone(),
            "sources": sources
                .iter()
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            "license": "CC0",
            "slices": names,
        }));
    }

    // Manifest
    fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    // Generated Rust so the app embeds the slices with no runtime file lookups.
    let mut rs = String::new();
    rs.push_str("// @generated by tools/slicer. Do not edit.\n\n");
    rs.push_str("/// A short recorded keystroke, embedded in the binary.\n");
    rs.push_str("pub struct Slice {\n    pub path: &'static str,\n    pub wav: &'static [u8],\n}\n\n");
    rs.push_str("pub static SLICES: &[Slice] = &[\n");
    for (name, path) in &generated {
        rs.push_str(&format!(
            "    Slice {{ path: \"{}\", wav: include_bytes!(\"../assets/slices/{}\") }},\n",
            name,
            path.to_string_lossy()
                .split_once("assets/slices/")
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().to_string())
        ));
    }
    rs.push_str("];\n\n");
    // Display names live here too: re-deriving "HHKB" from the slug "hhkb-topre"
    // in the app would turn it into "Hhkb".
    rs.push_str("/// (directory slug, display name), straight from the manifest.\n");
    rs.push_str("pub static BOARDS: &[(&str, &str)] = &[\n");
    for (s, n) in &board_names {
        rs.push_str(&format!(
            "    ({:?}, {:?}),\n",
            s,
            n
        ));
    }
    rs.push_str("];\n\n");
    // Deliberately not embedded: the manifest stays on disk as documentation.
    // CC0 requires no attribution and there is no UI field for it.
    // out_dir is <src-tauri>/assets/slices, so the crate root is two levels up.
    let gen_path = out_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("src/slices_generated.rs"));
    match &gen_path {
        // Only meaningful when output went to <src-tauri>/assets/slices; an
        // ad-hoc output directory has no crate to generate into.
        Some(g) if g.parent().is_some_and(|d| d.is_dir()) => fs::write(g, rs)?,
        _ => println!("(no crate root at out_dir, skipping generated Rust)"),
    }

    // Calibration report
    println!("\n=== calibration (recorded reference) ===");
    println!(
        "{:<26} {:>8} {:>10} {:>9} {:>9}",
        "board/slice", "peak Hz", "centroid", "dur ms", "sharp"
    );
    let mut by_board: std::collections::BTreeMap<String, Vec<&Stats>> = Default::default();
    for (b, _, s) in &all_stats {
        by_board.entry(b.clone()).or_default().push(s);
    }
    for (b, rows) in &by_board {
        let n = rows.len().max(1) as f32;
        let avg = |f: fn(&Stats) -> f32| rows.iter().map(|r| f(r)).sum::<f32>() / n;
        println!(
            "{:<26} {:>8.0} {:>10.0} {:>9.1} {:>9.2}",
            b,
            avg(|s| s.peak_hz),
            avg(|s| s.centroid_hz),
            avg(|s| s.duration_ms),
            avg(|s| s.sharpness)
        );
    }

    println!("\n{} slices written to {}", generated.len(), out_dir.display());
    if let Some(g) = &gen_path {
        if g.parent().is_some_and(|d| d.is_dir()) {
            println!("generated {}", g.display());
        }
    }
    Ok(())
}

/// Recurses, because Freesound packs download as a directory with the sounds
/// inside it alongside the license file.
fn collect_audio(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_audio(&p, out);
        } else if matches!(
            p.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase).as_deref(),
            Some("mp3") | Some("wav") | Some("ogg") | Some("flac") | Some("m4a") | Some("aac")
        ) {
            out.push(p);
        }
    }
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
