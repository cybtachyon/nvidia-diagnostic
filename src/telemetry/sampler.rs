//! Stage-1 telemetry sampler (PLAN §4.2, §5 Stage 1).
//!
//! A background thread that:
//! - samples `nvidia-smi` at 10 Hz, writing one `TelemetrySample` per line
//!   to a JSONL file (the correlation source for M3);
//! - scans the kernel log for new Xid/NVRM lines every 1 s;
//! - polls the PCIe link state (`lspci` LnkSta) every 2 s;
//! - detects `nvidia-smi` going stale (a driver-reset signal).
//!
//! Everything it observes is streamed to the monitor over an mpsc channel.

use crate::events::xid::{XidScanner, ScannedLine};
use crate::model::{now, FailureMarker, MarkerKind, TelemetrySample, XidEvent};
use crate::probe::pcie::{gen_from_gts, parse_link_line};
use crate::util::cmd::{csv_fields, parse_f64, parse_u32, read_file_lossy, CommandRunner};
use chrono::{DateTime, Utc};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 10 Hz `nvidia-smi` CSV query. `clocks_throttle_reasons.active` is a hex
/// bitmask and must stay the LAST field (it is the only one that can
/// contain commas when the driver renders names).
const SMI_QUERY: &str = "temperature.gpu,temperature.memory,power.draw,power.limit,\
clocks.current.graphics,clocks.current.sm,fan.speed,utilization.gpu,\
utilization.memory,clocks_throttle_reasons.active";
const SMI_ARGS: &[&str] = &["--query-gpu", SMI_QUERY, "--format=csv,noheader,nounits"];

/// Events the sampler streams to the monitor.
pub enum SamplerEvent {
    /// Latest telemetry sample, at most 1 Hz (drives the HUD).
    Sample(TelemetrySample),
    /// A new Xid event from the kernel log.
    Xid(XidEvent),
    /// A new NVRM line that is not a parseable Xid (report context).
    Nvrm(String),
    /// Auto-detected driver-level GPU reset.
    Reset(FailureMarker),
    /// `nvidia-smi` stopped responding (`true`) / recovered (`false`).
    SmiStale(bool),
    /// PCIe link state after a 2 s poll.
    Link(Option<(u8, u8)>),
}

pub struct SamplerConfig {
    /// Sample period in milliseconds (default 100 = 10 Hz).
    pub interval_ms: u64,
    /// Where the JSONL telemetry stream is written.
    pub jsonl_path: PathBuf,
    /// GPU PCI bus id in full form (e.g. `00000000:01:00.0`); resolved
    /// from `nvidia-smi` when `None`.
    pub bdf: Option<String>,
}

pub struct SamplerHandle {
    stop: mpsc::Sender<()>,
    pub events: mpsc::Receiver<SamplerEvent>,
    thread: JoinHandle<()>,
}

impl SamplerHandle {
    /// Signal the sampler to stop and join it (consumes the handle).
    pub fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.thread.join();
    }
}

/// Start the sampler thread; the JSONL file is created now.
pub fn start(cfg: SamplerConfig) -> std::io::Result<SamplerHandle> {
    let file = std::fs::File::create(&cfg.jsonl_path)?;
    let (tx, rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("telemetry-sampler".into())
        .spawn(move || run_sampler(cfg, file, tx, stop_rx));
    Ok(SamplerHandle {
        stop: stop_tx,
        events: rx,
        thread: thread?,
    })
}

fn run_sampler(
    cfg: SamplerConfig,
    file: std::fs::File,
    tx: mpsc::Sender<SamplerEvent>,
    stop_rx: mpsc::Receiver<()>,
) {
    let runner = CommandRunner::new();
    let mut writer = BufWriter::new(file);
    let mut scanner = XidScanner::new();
    let source = detect_log_source(&runner);
    let bdf = cfg.bdf.clone().or_else(|| resolve_bdf(&runner));

    // Seed the Xid scanner with the current kernel-log tail so the 1 s
    // scans only report lines that appear *during* the monitor run.
    if let Some(source) = &source {
        if let Some(lines) = fetch_kernel_log(&runner, source) {
            drop(scanner.scan(lines.iter().map(String::as_str)));
        }
    }

    let mut last_link: Option<(u8, u8)> = None;
    let mut last_link_poll = Instant::now() - Duration::from_secs(2);
    let mut last_xid_scan = Instant::now() - Duration::from_secs(1);
    let mut last_sample_tx = Instant::now();
    let mut smi_failing = false;
    let interval = Duration::from_millis(cfg.interval_ms.max(50));

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        let tick = Instant::now();

        // --- 10 Hz nvidia-smi sample → JSONL ---
        let out = runner.run("nvidia-smi", SMI_ARGS);
        if out.ok {
            if let Some(row) = out.stdout.lines().find(|l| !l.trim().is_empty()) {
                let mut sample = parse_sample_row(row, now());
                if tick - last_link_poll >= Duration::from_secs(2) {
                    last_link_poll = tick;
                    if let Some(b) = &bdf {
                        if let Some(link) = poll_link(&runner, b) {
                            last_link = Some(link);
                            let _ = tx.send(SamplerEvent::Link(Some(link)));
                        }
                    }
                }
                sample.pcie_link = last_link;
                if let Ok(line) = serde_json::to_string(&sample) {
                    let _ = writeln!(writer, "{line}");
                    let _ = writer.flush();
                }
                if smi_failing {
                    smi_failing = false;
                    let _ = tx.send(SamplerEvent::SmiStale(false));
                }
                if tick - last_sample_tx >= Duration::from_secs(1) {
                    last_sample_tx = tick;
                    let _ = tx.send(SamplerEvent::Sample(sample));
                }
            }
        } else if !smi_failing {
            // ok → failing transition: the GPU may have just been reset.
            smi_failing = true;
            let _ = tx.send(SamplerEvent::SmiStale(true));
            let _ = tx.send(SamplerEvent::Reset(FailureMarker {
                ts: now(),
                kind: MarkerKind::DetectedReset,
                note: Some("nvidia-smi stopped responding (possible GPU reset)".into()),
            }));
        }

        // --- 1 s incremental Xid scan ---
        if tick - last_xid_scan >= Duration::from_secs(1) {
            last_xid_scan = tick;
            if let Some(source) = &source {
                if let Some(lines) = fetch_kernel_log(&runner, source) {
                    for sl in scanner.scan(lines.iter().map(String::as_str)) {
                        emit_scanned(&tx, sl);
                    }
                }
            }
        }

        let elapsed = tick.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

fn emit_scanned(tx: &mpsc::Sender<SamplerEvent>, sl: ScannedLine) {
    let xid = sl.xid;
    if let Some(x) = &xid {
        let _ = tx.send(SamplerEvent::Xid(x.clone()));
    } else {
        let _ = tx.send(SamplerEvent::Nvrm(sl.raw.clone()));
    }
    if sl.reset {
        let _ = tx.send(SamplerEvent::Reset(FailureMarker {
            ts: now(),
            kind: MarkerKind::DetectedReset,
            note: Some(format!("kernel log: {}", sl.raw)),
        }));
    }
}

// ---------------------------------------------------------------------------
// kernel-log source
// ---------------------------------------------------------------------------

/// Where the 1 s Xid scan reads the kernel log from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Dmesg,
    Journal,
}

impl LogSource {
    pub fn name(&self) -> &str {
        match self {
            LogSource::Dmesg => "dmesg",
            LogSource::Journal => "journalctl",
        }
    }
}

/// `dmesg -T` when readable (e.g. running as root), else `journalctl -k`.
pub fn detect_log_source(r: &CommandRunner) -> Option<LogSource> {
    match r.run("dmesg", &["-T"]) {
        o if o.ok && !o.stdout.trim().is_empty() => Some(LogSource::Dmesg),
        _ => match r.run("journalctl", &["-k", "--no-pager", "-n", "2000"]) {
            o if o.ok => Some(LogSource::Journal),
            _ => None,
        },
    }
}

/// Fetch the current kernel-log tail from the given source.
fn fetch_kernel_log(r: &CommandRunner, source: &LogSource) -> Option<Vec<String>> {
    let (cmd, args): (&str, &[&str]) = match source {
        LogSource::Dmesg => ("dmesg", &["-T"]),
        LogSource::Journal => ("journalctl", &["-k", "--no-pager", "-n", "2000"]),
    };
    let out = r.run(cmd, args);
    out.ok.then(|| out.stdout.lines().map(String::from).collect())
}

// ---------------------------------------------------------------------------
// nvidia-smi / lspci parsing
// ---------------------------------------------------------------------------

/// Parse one 10 Hz CSV row into a `TelemetrySample` (link state filled in
/// separately by the 2 s poll).
pub fn parse_sample_row(row: &str, ts: DateTime<Utc>) -> TelemetrySample {
    let f: Vec<Option<String>> = csv_fields(row);
    let g = |i: usize| f.get(i).and_then(|v| v.clone());
    let throttle_raw = g(9).and_then(|v| {
        u64::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()
    });
    TelemetrySample {
        ts,
        temp_gpu: parse_u32(g(0).as_deref()),
        temp_mem: parse_u32(g(1).as_deref()),
        power_draw: parse_f64(g(2).as_deref()),
        power_limit: parse_f64(g(3).as_deref()),
        clock_graphics: parse_u32(g(4).as_deref()),
        clock_sm: parse_u32(g(5).as_deref()),
        fan_pct: parse_u32(g(6).as_deref()),
        throttle_reasons: decode_throttle_bits(throttle_raw),
        throttle_reasons_raw: throttle_raw,
        util_gpu: parse_u32(g(7).as_deref()),
        util_mem: parse_u32(g(8).as_deref()),
        pcie_link: None,
    }
}

/// `clocks_throttle_reasons.active` bit names (nvidia-smi documented
/// mapping; unknown bits are reported generically so no signal is lost).
const THROTTLE_BIT_NAMES: &[(u64, &str)] = &[
    (0x1, "SW Power Capping"),
    (0x2, "HW Power Brake Slowdown"),
    (0x4, "SW Thermal Slowdown"),
    (0x8, "HW Thermal Slowdown"),
    (0x10, "SW Thermal Shutdown"),
    (0x20, "HW Br Slowdown"),
    (0x40, "HW Thermal Shutdown"),
    (0x80, "SW Power Brake Slowdown"),
    (0x100, "SW DVS"),
    (0x200, "Display SW Power Capping"),
    (0x400, "Reliability"),
];

/// Decode the active-throttle bitmask into `(name, "Active")` pairs.
pub fn decode_throttle_bits(raw: Option<u64>) -> Vec<(String, String)> {
    let Some(raw) = raw else {
        return vec![];
    };
    let mut out: Vec<(String, String)> = THROTTLE_BIT_NAMES
        .iter()
        .filter(|(bit, _)| raw & bit != 0)
        .map(|(_, name)| (name.to_string(), "Active".to_string()))
        .collect();
    let known: u64 = THROTTLE_BIT_NAMES.iter().fold(0, |a, (b, _)| a | b);
    let unknown = raw & !known;
    if unknown != 0 {
        out.push((format!("bits 0x{unknown:x}"), "Active".to_string()));
    }
    out
}

/// One PCIe link-state poll: `lspci -vvv` LnkSta when readable (root),
/// else the world-readable sysfs `current_link_*` files.
fn poll_link(r: &CommandRunner, bdf_full: &str) -> Option<(u8, u8)> {
    let bdf_short = shorten_bdf(bdf_full);
    let out = r.run("lspci", &["-vvv", "-s", &bdf_short]);
    if out.ok {
        if let Some(link) = out
            .stdout
            .lines()
            .find(|l| l.contains("LnkSta:"))
            .and_then(parse_link_line)
        {
            return Some(link);
        }
    }
    // sysfs uses a 4-digit PCI domain; nvidia-smi reports 8. Split on the
    // FIRST colon (domain:bus:dev.func has two of them).
    let (domain, rest) = bdf_full.split_once(':').unwrap_or(("", bdf_full));
    let domain4 = &domain[..domain.len().min(4)];
    let base = format!("/sys/bus/pci/devices/{domain4}:{rest}");
    let speed = read_file_lossy(&format!("{base}/current_link_speed"))?;
    let width = read_file_lossy(&format!("{base}/current_link_width"))?;
    // `current_link_speed` is `32.0 GT/s PCIe` — take the leading number.
    let speed: f64 = speed.trim().split_whitespace().next()?.parse().ok()?;
    let width: u8 = width.trim().parse().ok()?;
    gen_from_gts(speed).map(|g| (g, width))
}

/// `nvidia-smi` reports `00000000:01:00.0`; lspci wants `01:00.0`.
/// Idempotent: an already-short BDF is returned unchanged.
pub fn shorten_bdf(full: &str) -> String {
    let parts: Vec<&str> = full.rsplit(':').take(2).collect();
    match parts.as_slice() {
        [func, bus] => format!("{bus}:{func}"),
        _ => full.to_string(),
    }
}

/// Full-form BDF (`00000000:01:00.0`) from `nvidia-smi`.
fn resolve_bdf(r: &CommandRunner) -> Option<String> {
    let out = r.run(
        "nvidia-smi",
        &["--query-gpu=pci.bus_id", "--format=csv,noheader"],
    );
    out.stdout_if_ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| {
                    let t = l.trim();
                    (!t.is_empty()).then(|| t.to_string())
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_csv_row() {
        // Captured on the 5090 test machine (driver 615.78.08).
        let row = "69, N/A, 480.16, 575.00, 2880, 2880, 85, 100, 72, 0x0000000000000400";
        let s = parse_sample_row(row, now());
        assert_eq!(s.temp_gpu, Some(69));
        assert_eq!(s.temp_mem, None); // N/A on this card
        assert_eq!(s.power_draw, Some(480.16));
        assert_eq!(s.power_limit, Some(575.0));
        assert_eq!(s.clock_graphics, Some(2880));
        assert_eq!(s.clock_sm, Some(2880));
        assert_eq!(s.fan_pct, Some(85));
        assert_eq!(s.util_gpu, Some(100));
        assert_eq!(s.util_mem, Some(72));
        assert_eq!(s.throttle_reasons_raw, Some(0x400));
        assert_eq!(s.throttle_reasons, vec![("Reliability".to_string(), "Active".to_string())]);
        assert_eq!(s.pcie_link, None);
    }

    #[test]
    fn parses_row_with_no_throttle_and_na_clocks() {
        let s = parse_sample_row("45, N/A, 71.2, 575.0, N/A, N/A, 30, 0, 0, 0x0", now());
        assert_eq!(s.clock_graphics, None);
        assert_eq!(s.throttle_reasons_raw, Some(0));
        assert!(s.throttle_reasons.is_empty());
    }

    #[test]
    fn decodes_known_and_unknown_throttle_bits() {
        assert!(decode_throttle_bits(None).is_empty());
        let both = decode_throttle_bits(Some(0x1 | 0x8));
        assert_eq!(
            both,
            vec![
                ("SW Power Capping".to_string(), "Active".to_string()),
                ("HW Thermal Slowdown".to_string(), "Active".to_string()),
            ]
        );
        let unknown = decode_throttle_bits(Some(0x10000));
        assert_eq!(unknown, vec![("bits 0x10000".to_string(), "Active".to_string())]);
    }

    #[test]
    fn shortens_full_bdf() {
        assert_eq!(shorten_bdf("00000000:01:00.0"), "01:00.0");
        assert_eq!(shorten_bdf("01:00.0"), "01:00.0");
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    /// Machine-dependent: only meaningful where the 5090's sysfs entry
    /// exists (skipped silently elsewhere).
    #[test]
    fn poll_link_resolves_sysfs_fallback() {
        if read_file_lossy("/sys/bus/pci/devices/0000:01:00.0/current_link_speed").is_none() {
            return;
        }
        let r = CommandRunner::new();
        let link = poll_link(&r, "00000000:01:00.0");
        assert!(link.is_some(), "sysfs link state should be readable");
    }
}
