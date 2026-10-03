use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::process::Command;
use std::time::{Duration, Instant};

/// Minimum gap between two key events to be treated as a fresh press.
/// The OS auto-repeats far faster than a human types, so without this filter
/// holding a key produces a machine-gun of sounds.
const REPEAT_FILTER: Duration = Duration::from_millis(35);

const BTN_REPEAT_FILTER: Duration = Duration::from_millis(20);

/// 256 frames at 44.1 kHz = 5.8 ms. Worth requesting even though PipeWire
/// negotiates its own quantum for the stream.
const TARGET_BUFFER_FRAMES: u32 = 256;

struct Event {
    fired_at: Instant,
}

/// cpal's ALSA backend does not populate the output timestamp, so it cannot
/// report device-side latency. Ask PipeWire directly instead: it prints
/// `node.latency` for our stream, and the ALSA period for the active card.
fn probe_pipewire() {
    let Ok(out) = Command::new("pactl")
        .args(["list", "sink-inputs"])
        .output()
    else {
        println!("(pactl unavailable - skipping latency probe)\n");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);

    let mut stream_latency = String::from("n/a");
    let mut ours = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("application.name") && t.contains("spike") {
            ours = true;
        }
        if ours {
            if let Some(v) = t.strip_prefix("node.latency = ") {
                stream_latency = v.trim_matches('"').to_string();
                break;
            }
        }
    }
    println!("PipeWire stream latency : {stream_latency}");

    // `X/Y` where Y is the sample rate of the sink.
    if let Some((frames, rate)) = stream_latency.split_once('/') {
        if let (Ok(f), Ok(r)) = (frames.parse::<f64>(), rate.parse::<f64>()) {
            if r > 0.0 {
                println!("  -> {:.2} ms of stream buffer", f * 1000.0 / r);
            }
        }
    }
    println!();
}

fn probe_alsa_period() {
    let mut found = false;
    for card in 0..8 {
        let path = format!("/proc/asound/card{card}");
        if !std::path::Path::new(&path).exists() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for e in entries.flatten() {
            let pcm = e.path();
            let Ok(subdirs) = std::fs::read_dir(pcm.join("sub0")) else {
                continue;
            };
            let _ = subdirs;
            let hw = pcm.join("sub0/hw_params");
            let Ok(content) = std::fs::read_to_string(&hw) else {
                continue;
            };
            if content.trim() == "closed" {
                continue;
            }
            found = true;
            let mut rate = 0u32;
            let mut period = 0u32;
            for line in content.lines() {
                if let Some(v) = line.strip_prefix("rate: ") {
                    rate = v.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0);
                }
                if let Some(v) = line.strip_prefix("period_size: ") {
                    period = v.trim().parse().unwrap_or(0);
                }
            }
            let name = std::fs::read_to_string(pcm.join("sub0/info")).unwrap_or_default();
            let id = name
                .lines()
                .find(|l| l.starts_with("name:"))
                .map(|l| l.trim_start_matches("name: ").trim().to_string())
                .unwrap_or_default();
            if period > 0 && rate > 0 {
                println!("ALSA active device      : card{card} {id}");
                println!("  rate                  : {rate} Hz");
                println!("  period_size           : {period}");
                println!("  -> driver period      : {:.2} ms", period as f64 * 1000.0 / rate as f64);
                println!("  DOMINANT LATENCY TERM");
            }
        }
    }
    if !found {
        println!("ALSA period            : (no active device - start the stream and re-check)");
    }
    println!();
}

fn render_click(rate: u32) -> Vec<f32> {
    let n = (rate as f32 * 0.045) as usize;
    let mut out = vec![0.0f32; n];

    let mut phase = 0.0f32;
    let step = 2.0 * std::f32::consts::PI * 150.0 / rate as f32;
    let mut lp = 0.0f32;
    let mut seed: u32 = 0x1234_5678;

    for i in 0..n {
        let t = i as f32 / rate as f32;

        let body_env = (-t / 0.018).exp();
        phase += step;
        let body = phase.sin() * body_env * 0.55;

        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let white = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        lp = lp * 0.6 + white * 0.4;
        let collision = lp * (-t / 0.0035).exp() * 0.35;

        out[i] = (body + collision).clamp(-1.0, 1.0);
    }

    let fade = 200.min(n / 2);
    for i in 0..fade {
        out[n - 1 - i] *= i as f32 / fade as f32;
    }
    out
}

#[derive(Default)]
struct Aggregate {
    buffers: u64,
    events: u64,
    sw_latency_sum: Duration,
    sw_latency_max: Duration,
}

fn fmt_avg(sum: Duration, n: u64) -> f64 {
    if n == 0 {
        0.0
    } else {
        sum.as_secs_f64() * 1000.0 / n as f64
    }
}

fn main() {
    // `--simulate N`: inject N synthetic presses through XTest and exit.
    // Useful as an input-injection tool for testing the real app, since xdotool
    // is not installed.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--simulate-chord") {
        // Ctrl+Alt+M, the mute hotkey.
        //
        // XTest-injected events appear to be dropped when the injecting process
        // exits immediately, especially releases, so we keep the display alive
        // briefly after the sequence to let the server drain.
        let seq = [
            rdev::EventType::KeyPress(rdev::Key::ControlLeft),
            rdev::EventType::KeyPress(rdev::Key::Alt),
            rdev::EventType::KeyPress(rdev::Key::KeyM),
        ];
        for e in &seq {
            let _ = rdev::simulate(e);
            std::thread::sleep(Duration::from_millis(60));
        }
        std::thread::sleep(Duration::from_millis(120));
        for e in seq.iter().rev() {
            let _ = rdev::simulate(e);
            std::thread::sleep(Duration::from_millis(60));
        }
        std::thread::sleep(Duration::from_millis(300));
        println!("simulated ctrl+alt+m");
        return;
    }
    if args.get(1).map(String::as_str) == Some("--simulate") {
        let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
        let keys = [
            rdev::Key::KeyH,
            rdev::Key::KeyI,
            rdev::Key::Space,
            rdev::Key::ShiftLeft,
        ];
        for i in 0..n {
            let k = keys[i % keys.len()];
            let _ = rdev::simulate(&rdev::EventType::KeyPress(k));
            std::thread::sleep(Duration::from_millis(90));
            let _ = rdev::simulate(&rdev::EventType::KeyRelease(k));
            std::thread::sleep(Duration::from_millis(40));
        }
        println!("simulated {n} presses");
        return;
    }

    let host = cpal::default_host();

    let device = host
        .default_output_device()
        .expect("no default output device - check your sound card is not muted");

    let config = device
        .default_output_config()
        .expect("could not query the default output config");

    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let click = Arc::new(render_click(sample_rate));
    let click_len = click.len();

    let mut stream_config: cpal::StreamConfig = config.clone().into();
    stream_config.buffer_size = cpal::BufferSize::Fixed(TARGET_BUFFER_FRAMES);

    println!("=== DEVICE (as opened by cpal via ALSA) ===");
    println!("name              : {}", device.name().unwrap_or_default());
    println!("sample rate       : {} Hz", sample_rate);
    println!("channels          : {}", channels);
    println!("requested buffer  : {TARGET_BUFFER_FRAMES} frames");
    println!();

    let (tx, rx): (Sender<Event>, Receiver<Event>) = bounded(256);
    let (stat_tx, stat_rx): (Sender<Aggregate>, Receiver<Aggregate>) = unbounded();
    let total_events = Arc::new(AtomicU64::new(0));

    let err_fn = |err: cpal::StreamError| eprintln!("[audio error] {err}");

    let mut cursor = 0usize;
    let mut playing = false;
    let mut agg = Aggregate::default();

    let click_c = click.clone();
    let stat_tx_c = stat_tx.clone();

    let stream = device
        .build_output_stream(
            &stream_config,
            move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                let mut started = false;
                let mut sw_latency = Duration::ZERO;

                while let Ok(ev) = rx.try_recv() {
                    if !playing {
                        playing = true;
                        cursor = 0;
                        started = true;
                        sw_latency = ev.fired_at.elapsed();
                    }
                }

                let ch = channels.max(1);
                let frames = out.len() / ch;
                for f in 0..frames {
                    let s = if playing { click_c[cursor] } else { 0.0 };
                    for c in 0..ch {
                        out[f * ch + c] = s;
                    }
                    if playing {
                        cursor += 1;
                        if cursor >= click_len {
                            cursor = 0;
                            playing = false;
                        }
                    }
                }

                agg.buffers += 1;
                if started {
                    agg.events += 1;
                    agg.sw_latency_sum += sw_latency;
                    agg.sw_latency_max = agg.sw_latency_max.max(sw_latency);
                }

                if agg.buffers % 1024 == 0 {
                    let snapshot = std::mem::take(&mut agg);
                    let _ = stat_tx_c.try_send(snapshot);
                }
            },
            err_fn,
            None,
        )
        .expect("failed to build the output stream - the device may reject this buffer size");

    stream.play().expect("failed to start the audio stream");

    println!("=== LATENCY BUDGET (measured, not estimated) ===");
    probe_pipewire();
    probe_alsa_period();
    println!("=== RUNNING ===");
    println!("Type on the keyboard and click the mouse now.");
    println!("Ctrl+C to exit.\n");

    let total_events_monitor = total_events.clone();
    std::thread::spawn(move || {
        let mut buffers_seen = 0u64;
        let mut last_events = 0u64;
        while let Ok(snap) = stat_rx.recv() {
            buffers_seen += snap.buffers;
            let live = total_events_monitor.load(Ordering::Relaxed);
            println!(
                "t={:>5.1}s  events(total/live)={:>5}/{:<5} hook->audio avg={:>5.2} ms max={:>5.2} ms",
                buffers_seen as f64 * 5.8 / 1000.0,
                live,
                live - last_events,
                fmt_avg(snap.sw_latency_sum, snap.events),
                snap.sw_latency_max.as_secs_f64() * 1000.0,
            );
            last_events = live;
        }
    });

    let mut last_key: Option<Instant> = None;
    let mut last_btn: Option<Instant> = None;

    let result = rdev::listen(move |event| match event.event_type {
        rdev::EventType::KeyPress { .. } => {
            let now = Instant::now();
            if let Some(prev) = last_key {
                if now.duration_since(prev) < REPEAT_FILTER {
                    return;
                }
            }
            last_key = Some(now);
            total_events.fetch_add(1, Ordering::Relaxed);
            let _ = tx.try_send(Event { fired_at: now });
        }
        rdev::EventType::ButtonPress { .. } => {
            let now = Instant::now();
            if let Some(prev) = last_btn {
                if now.duration_since(prev) < BTN_REPEAT_FILTER {
                    return;
                }
            }
            last_btn = Some(now);
            total_events.fetch_add(1, Ordering::Relaxed);
            let _ = tx.try_send(Event { fired_at: now });
        }
        _ => {}
    });

    if let Err(e) = result {
        eprintln!("[hook error] {e:?}");
        eprintln!("On X11 this usually means DISPLAY is not reachable from this process.");
        std::process::exit(1);
    }
}
