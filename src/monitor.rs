//! The `monitor` subcommand (PLAN §4.1 step 3, §5 Stage 1): live telemetry
//! while the user's real CUDA workload runs. `m` marks each black-screen
//! moment, the tool auto-detects driver resets, and everything is written
//! to a JSONL stream plus a small session sidecar for M3 analysis.

use crate::model::{now, FailureMarker, MarkerKind, TelemetrySample, XidEvent};
use crate::telemetry::sampler as sampler;
use crate::telemetry::sampler::{detect_log_source, LogSource, SamplerConfig, SamplerEvent};
use crate::util::cmd::CommandRunner;
use crate::workload::Workload;
use owo_colors::OwoColorize;
use serde_json::json;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct MonitorOptions {
    pub duration_secs: Option<u64>,
    pub jsonl: Option<PathBuf>,
    /// Workload to spawn under the monitor (e.g. the inference server).
    pub workload: Option<Vec<String>>,
    pub interval_ms: u64,
}

pub fn run_monitor(opts: MonitorOptions) -> anyhow::Result<()> {
    let jsonl = opts
        .jsonl
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("telemetry-{}.jsonl", stamp())));

    // --- Preflight: warn (never abort) about degraded inputs ---
    let runner = CommandRunner::new();
    if runner.run("nvidia-smi", &["--version"]).missing {
        println!(
            "{}",
            "warning: nvidia-smi not found — telemetry will be degraded (NVIDIA driver missing?)"
                .yellow()
        );
    }
    let log_source = detect_log_source(&runner);
    match &log_source {
        Some(LogSource::Dmesg) => {}
        Some(LogSource::Journal) => println!(
            "{}",
            "note: dmesg restricted — Xid scan uses journalctl".dimmed()
        ),
        None => println!(
            "{}",
            "warning: kernel log unreadable (dmesg + journalctl) — Xid scan disabled".yellow()
        ),
    }

    // --- Optional workload ---
    let mut workload: Option<Workload> = match &opts.workload {
        Some(argv) => {
            let w = Workload::spawn(argv)?;
            println!(
                "{} {} (pid {})",
                "workload started:".bold(),
                argv.join(" "),
                w.pid()
            );
            Some(w)
        }
        None => None,
    };

    // --- Sampler ---
    let cfg = SamplerConfig {
        interval_ms: opts.interval_ms,
        jsonl_path: jsonl.clone(),
        bdf: None,
    };
    let handle = sampler::start(cfg)?;
    let started = now();
    let start_time = Instant::now();

    println!(
        "\n{} {}",
        "monitoring →".bold(),
        jsonl.display().bold()
    );
    println!(
        "{}",
        format!(
            "keys: m = mark black screen · q = stop · Ctrl-C = stop (sample every {} ms)",
            opts.interval_ms
        )
        .dimmed()
    );

    let mut markers: Vec<FailureMarker> = vec![];
    let mut xids: Vec<XidEvent> = vec![];
    let mut latest: Option<TelemetrySample> = None;
    let mut link: Option<(u8, u8)> = None;
    let mut last_hud = Instant::now();
    let mut workload_done = false;

    let tty = TtyRaw::open();
    let stdin_is_tty = std::io::stdin().is_terminal();
    install_sigint();
    let sigint = &SIGINT;

    loop {
        // --- Keys (100 ms granularity). Non-TTY runs have no key input;
        // they stop via --duration or Ctrl-C (SIGINT handler above). ---
        let key = match tty.as_ref() {
            Some(t) => t.next_key(),
            None if stdin_is_tty => read_stdin_key(),
            None => None,
        };
        if let Some(k) = key {
            match k {
                b'm' | b'M' => {
                    let m = FailureMarker {
                        ts: now(),
                        kind: MarkerKind::UserMark,
                        note: Some("user marked black screen (m)".into()),
                    };
                    markers.push(m.clone());
                    println!(
                        "\n{} {} {}",
                        "● mark".red().bold(),
                        m.ts.format("%H:%M:%S").to_string().bold(),
                        "(black screen)".dimmed()
                    );
                }
                b'q' | b'Q' => break,
                _ => {}
            }
        }

        // --- Drain sampler events (non-blocking) ---
        loop {
            match handle.events.try_recv() {
                Ok(SamplerEvent::Sample(s)) => latest = Some(s),
                Ok(SamplerEvent::Xid(x)) => {
                    xids.push(x.clone());
                    println!(
                        "\n{} Xid {} (PCI:{}) {}",
                        "● xid".red().bold(),
                        x.id.to_string().red().bold(),
                        x.bdf.as_deref().unwrap_or("?"),
                        x.raw
                            .strip_prefix('[')
                            .and_then(|r| r.split_once(']').map(|x| x.1))
                            .unwrap_or(&x.raw)
                            .trim()
                            .dimmed()
                    );
                }
                Ok(SamplerEvent::Nvrm(line)) => {
                    println!("\n{} {}", "nvrm".dimmed(), line.dimmed());
                }
                Ok(SamplerEvent::Reset(m)) => {
                    markers.push(m.clone());
                    println!(
                        "\n{} {} {}",
                        "● reset".red().bold(),
                        m.ts.format("%H:%M:%S").to_string().bold(),
                        m.note.as_deref().unwrap_or("driver reset detected").red()
                    );
                }
                Ok(SamplerEvent::SmiStale(true)) => {
                    println!("\n{}", "nvidia-smi stopped responding".yellow().bold());
                }
                Ok(SamplerEvent::SmiStale(false)) => {
                    println!("\n{}", "nvidia-smi recovered".green().bold());
                }
                Ok(SamplerEvent::Link(l)) => link = l,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // --- Workload exit note (once) ---
        if !workload_done
            && let Some(w) = workload.as_mut()
                && w.poll().is_some() {
                    workload_done = true;
                    println!(
                        "\n{}",
                        "note: workload exited — monitoring continues (q to stop)".yellow()
                    );
                }

        // --- HUD / duration / stop conditions ---
        let elapsed = start_time.elapsed();
        if opts
            .duration_secs
            .is_some_and(|d| elapsed >= Duration::from_secs(d))
        {
            println!("\n{}", "duration reached — stopping".dimmed());
            break;
        }
        if sigint.load(Ordering::SeqCst) {
            println!("\n{}", "interrupted — stopping".dimmed());
            break;
        }

        if Instant::now() >= last_hud + Duration::from_secs(1) {
            last_hud = Instant::now();
            if std::io::stdout().is_terminal() {
                print!("\r{}", hud_line(&latest, &xids, &markers, link, elapsed.as_secs()));
                let _ = std::io::stdout().flush();
            } else if elapsed.as_secs().is_multiple_of(5) {
                println!("{}", hud_line(&latest, &xids, &markers, link, elapsed.as_secs()));
            }
        }
    }

    // --- Teardown ---
    handle.stop();
    let workload_info = workload.as_ref().map(|w| {
        json!({
            "argv": w.cmd(),
            "pid": w.pid(),
        })
    });
    if let Some(w) = workload.as_mut()
        && w.poll().is_none() {
            w.kill();
            println!("{}", "workload stopped".dimmed());
        }
    let ended = now();
    drop(tty);

    // --- Summary + sidecar (the M3 analysis input) ---
    let sample_count = count_lines(&jsonl);
    let user_marks = markers.iter().filter(|m| m.kind == MarkerKind::UserMark).count();
    let detected = markers.iter().filter(|m| m.kind == MarkerKind::DetectedReset).count();
    let xid_ids: Vec<u32> = xids.iter().map(|x| x.id).collect();

    println!();
    println!("{}", "monitor summary".bold());
    println!(
        "{} {} · {} samples ({} Hz) · {} Xid(s) {} · {} mark(s) ({} user, {} detected)",
        "duration".dimmed(),
        format_duration(ended - started),
        sample_count,
        (sample_count as f64 / (ended - started).num_seconds().max(1) as f64).round() as u32,
        xids.len(),
        if xid_ids.is_empty() {
            "none".dimmed().to_string()
        } else {
            format!("{xid_ids:?}")
                .red()
                .to_string()
        },
        markers.len(),
        user_marks,
        detected
    );
    println!("{} {}", "telemetry:".dimmed(), jsonl.display());

    let sidecar = format!("monitor-{}.json", stamp());
    let report = json!({
        "started": started,
        "ended": ended,
        "jsonl": jsonl.to_string_lossy(),
        "samples": sample_count,
        "xid_events": xids,
        "markers": markers,
        "workload": workload_info,
        "xid_source": log_source.map(|s| s.name().to_string()),
        "interval_ms": opts.interval_ms,
    });
    std::fs::write(&sidecar, serde_json::to_string_pretty(&report)?)?;
    println!("{} {}", "session written:".dimmed(), sidecar.bold());

    Ok(())
}

fn stamp() -> String {
    now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn format_duration(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    format!("{}m {:02}s", s / 60, s % 60)
}

fn count_lines(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count() as u64)
        .unwrap_or(0)
}

/// One-line HUD: latest telemetry + event counts.
fn hud_line(
    latest: &Option<TelemetrySample>,
    xids: &[XidEvent],
    markers: &[FailureMarker],
    link: Option<(u8, u8)>,
    elapsed: u64,
) -> String {
    let s = latest.as_ref();
    let temp = s.and_then(|v| v.temp_gpu).map(|t| format!("{t}°C")).unwrap_or_else(|| "?".into());
    let power = s
        .and_then(|v| v.power_draw)
        .map(|p| format!("{p:.0}W"))
        .unwrap_or_else(|| "?".into());
    let limit = s
        .and_then(|v| v.power_limit)
        .map(|p| format!("/{p:.0}W"))
        .unwrap_or_default();
    let gfx = s
        .and_then(|v| v.clock_graphics)
        .map(|c| format!("{c}MHz"))
        .unwrap_or_else(|| "?".into());
    let util = s
        .and_then(|v| v.util_gpu)
        .map(|u| format!("{u}%"))
        .unwrap_or_else(|| "?".into());
    let fan = s
        .and_then(|v| v.fan_pct)
        .map(|f| format!("{f}%"))
        .unwrap_or_else(|| "?".into());
    let link_s = link
        .map(|(g, w)| format!("Gen{g}x{w}"))
        .unwrap_or_else(|| "?".into());
    let throttle = s
        .and_then(|v| v.throttle_reasons_raw)
        .filter(|raw| *raw != 0)
        .map(|raw| format!("throttle 0x{raw:x}"))
        .unwrap_or_default();
    format!(
        "t+{elapsed}s · {temp} · {power}{limit} · gfx {gfx} · util {util} · fan {fan} · link {link_s} · xid {n_x} · marks {n_m} {throttle}",
        n_x = xids.len(),
        n_m = markers.len()
    )
}

// ---------------------------------------------------------------------------
// raw-mode key input
// ---------------------------------------------------------------------------

/// `/dev/tty` in raw mode (no echo, no line buffering, 100 ms read timeout)
/// so a single `m` press registers immediately.
struct TtyRaw {
    fd: libc::c_int,
    orig: libc::termios,
}

impl TtyRaw {
    fn open() -> Option<Self> {
        unsafe {
            let fd = libc::open(c"/dev/tty".as_ptr(), libc::O_RDONLY);
            if fd < 0 {
                return None;
            }
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) != 0 {
                libc::close(fd);
                return None;
            }
            let mut raw = orig;
            raw.c_iflag &= !(libc::ICRNL | libc::IXON | libc::BRKINT);
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 1; // 100 ms
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                libc::close(fd);
                return None;
            }
            Some(Self { fd, orig })
        }
    }

    /// Read one key, or `None` after the 100 ms timeout.
    fn next_key(&self) -> Option<u8> {
        let mut b = [0u8; 1];
        let n = unsafe {
            libc::read(
                self.fd,
                b.as_mut_ptr() as *mut libc::c_void,
                b.len(),
            )
        };
        (n == 1).then_some(b[0])
    }
}

impl Drop for TtyRaw {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
            libc::close(self.fd);
        }
    }
}

/// Stdin-is-a-TTY fallback (raw /dev/tty unavailable): read `m` / `q`
/// lines from stdin (blocking).
fn read_stdin_key() -> Option<u8> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => None, // EOF → stop the monitor
        Ok(_) => line
            .trim()
            .chars()
            .next()
            .map(|c| c as u8)
            .filter(|&c| c == b'm' || c == b'M' || c == b'q' || c == b'Q'),
        Err(_) => None,
    }
}

static SIGINT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    SIGINT.store(true, Ordering::SeqCst);
}

fn install_sigint() {
    unsafe {
        libc::signal(
            libc::SIGINT,
            on_sigint as *const () as usize,
        );
    }
}
