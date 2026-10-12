//! Core data model (PLAN §4.3, §4.4). M1 uses `Baseline` and its parts; the
//! telemetry/evidence/verdict types are defined now so M2–M4 build on them.
//! M4 adds the shareable `Report` and the experiment-run records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// UTC timestamp for records.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// File-name stamp (`20251012T012345Z`) for report/session artifacts.
pub fn stamp() -> String {
    now().format("%Y%m%dT%H%M%SZ").to_string()
}

// ---------------------------------------------------------------------------
// Stage 0 — baseline
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SystemInfo {
    pub kernel: Option<String>,
    pub arch: Option<String>,
    pub os: Option<String>,
    /// `modinfo nvidia | grep ^version`
    pub nvidia_module_version: Option<String>,
    /// Module names from `lsmod` (nvidia, nvidia_modeset, nvidia_uvm, ...)
    pub nvidia_modules_loaded: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BiosInfo {
    pub version: Option<String>,
    pub date: Option<String>,
    pub vendor: Option<String>,
    /// Set when the probe degraded (e.g. dmidecode missing or no root).
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DriverInfo {
    pub driver_version: Option<String>,
    pub vbios_version: Option<String>,
    /// Kernel-module version from `modinfo`, to compare against the userland
    /// driver version (mismatch is itself a signal).
    pub module_version: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct GpuInfo {
    pub name: Option<String>,
    pub serial: Option<String>,
    pub vbios: Option<String>,
    /// PCI bus id as reported by nvidia-smi, e.g. `00000000:01:00.0`.
    pub bdf: Option<String>,
    pub temp_gpu: Option<u32>,
    pub temp_mem: Option<u32>,
    pub power_draw_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub clock_graphics_mhz: Option<u32>,
    pub clock_sm_mhz: Option<u32>,
    pub mem_used_mb: Option<u32>,
    pub mem_total_mb: Option<u32>,
    pub util_gpu_pct: Option<u32>,
    pub util_mem_pct: Option<u32>,
    /// Max operating temperature (°C) from `nvidia-smi -q -d TEMPERATURE`.
    pub max_op_temp_c: Option<u32>,
    /// Throttle reasons with active state, e.g. `("SW Power Capping", "Not Active")`.
    pub throttle_reasons: Vec<(String, String)>,
    /// ECC support summary, e.g. "Supported".
    pub ecc: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PcieInfo {
    pub bdf: Option<String>,
    /// (generation, width) the link is capable of, e.g. (5, 16).
    pub lnk_cap: Option<(u8, u8)>,
    /// (generation, width) the link is currently running at.
    pub lnk_sta: Option<(u8, u8)>,
    /// Raw AER `UESta:` / `CESta:` strings (absent if lspci lacks AER detail).
    pub aer_ue: Option<String>,
    pub aer_ce: Option<String>,
    /// True when a latched (uncorrectable/correctable) AER error bit is set.
    pub aer_ue_errors: Option<bool>,
    pub aer_ce_errors: Option<bool>,
}

/// One `NVRM: Xid (PCI:xx:00.0) <id>` event.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct XidEvent {
    /// Human timestamp from `dmesg -T` when available.
    pub ts: Option<String>,
    pub bdf: Option<String>,
    pub id: u32,
    pub raw: String,
}

/// Answers collected in the preflight timeline interview.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TimelineAnswers {
    pub failure_start: Option<String>,
    /// Free-choice list: "BIOS update", "GPU driver update", ...
    pub changes_around: Vec<String>,
    pub notes: Option<String>,
}

/// Stage-0 snapshot (PLAN §4.3).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Baseline {
    pub ts: DateTime<Utc>,
    pub system: SystemInfo,
    pub bios: BiosInfo,
    pub driver: DriverInfo,
    pub gpu: GpuInfo,
    pub pcie: PcieInfo,
    pub xid_history: Vec<XidEvent>,
    pub user_timeline: TimelineAnswers,
}

// ---------------------------------------------------------------------------
// Stage 1 — live telemetry
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TelemetrySample {
    pub ts: DateTime<Utc>,
    pub temp_gpu: Option<u32>,
    pub temp_mem: Option<u32>,
    pub power_draw: Option<f64>,
    pub power_limit: Option<f64>,
    pub clock_graphics: Option<u32>,
    pub clock_sm: Option<u32>,
    pub fan_pct: Option<u32>,
    pub throttle_reasons: Vec<(String, String)>,
    /// Raw `clocks_throttle_reasons.active` bitmask (10 Hz CSV source; the
    /// decoded names in `throttle_reasons` can only cover known bits).
    pub throttle_reasons_raw: Option<u64>,
    pub util_gpu: Option<u32>,
    pub util_mem: Option<u32>,
    pub pcie_link: Option<(u8, u8)>,
}

/// A failure moment: the user pressed `m`, or the tool auto-detected a reset.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FailureMarker {
    pub ts: DateTime<Utc>,
    pub kind: MarkerKind,
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MarkerKind {
    #[default]
    UserMark,
    DetectedReset,
}

// ---------------------------------------------------------------------------
// Stage 3/5 — scoring & verdict
// ---------------------------------------------------------------------------

/// Suspect categories scored in M3 (PLAN §6); consumed by `score.rs`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Suspect {
    #[default]
    Driver,
    Bios,
    PcieConnector,
    Psu,
    CardHardware,
    Cooling,
}

impl std::fmt::Display for Suspect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Suspect::Driver => "GPU driver",
            Suspect::Bios => "Motherboard BIOS",
            Suspect::PcieConnector => "12VHPWR / PCIe connector",
            Suspect::Psu => "PSU",
            Suspect::CardHardware => "Card hardware",
            Suspect::Cooling => "Cooling",
        };
        f.write_str(s)
    }
}

/// A piece of evidence with signed weights per suspect (PLAN §6).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Evidence {
    pub id: String,
    pub description: String,
    pub weight_per_suspect: BTreeMap<Suspect, i8>,
    pub source: String,
}

/// A suspect with its M3 score and the evidence ids that contributed.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RankedSuspect {
    pub suspect: Suspect,
    pub score: i32,
    /// Evidence ids contributing to this suspect.
    pub evidence: Vec<String>,
}

/// The ranked M3 verdict with the next suggested experiment.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Verdict {
    pub ranked: Vec<RankedSuspect>,
    pub next_experiment: Option<String>,
}

// ---------------------------------------------------------------------------
// Stage 3 — M3 analysis output (PLAN §4.4, §5 Stage 1/3)
// ---------------------------------------------------------------------------

/// 10 s pre-window correlation for one failure marker (PLAN §5 Stage 1).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FailureCorrelation {
    pub marker: FailureMarker,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub sample_count: usize,
    pub max_temp_gpu: Option<u32>,
    pub max_temp_mem: Option<u32>,
    pub max_power_draw: Option<f64>,
    pub power_limit: Option<f64>,
    /// Pre-window peak draw at/over 99% of the limit (PLAN §6 "at/over limit").
    pub power_at_limit: bool,
    pub min_clock_graphics: Option<u32>,
    pub max_fan_pct: Option<u32>,
    /// Any active throttle reason in the pre-window (bitmask != 0).
    pub throttle_active: bool,
    /// Highest active-throttle bitmask seen in the pre-window.
    pub throttle_reasons_raw: Option<u64>,
    /// Last known PCIe link state inside the pre-window.
    pub link: Option<(u8, u8)>,
    /// Xid ids within ±2 s of the marker (time-attributed; deduplicated).
    pub xids_nearby: Vec<u32>,
    /// Interpretation note (e.g. no Xid + clean pre-window → display/EDID path).
    pub note: Option<String>,
}

/// The full M3 analysis of one monitor session (PLAN §7 M3, §4.4).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Analysis {
    /// Monitor session sidecar this analysis was built from.
    pub session: String,
    /// Baseline JSON used for the static evidence, if any.
    pub baseline: Option<String>,
    pub correlations: Vec<FailureCorrelation>,
    /// Session Xids that could not be time-attributed to a marker.
    pub xids_unattributed: Vec<u32>,
    pub evidence: Vec<Evidence>,
    pub verdict: Verdict,
}

// ---------------------------------------------------------------------------
// Stage 4/5 — M4 experiments & report (PLAN §4.4, §5 Stage 4)
// ---------------------------------------------------------------------------

/// Min/avg/max of one numeric telemetry field over a whole session
/// (PLAN §4.4 `telemetry_summary`).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Stats {
    pub min: Option<f64>,
    pub avg: Option<f64>,
    pub max: Option<f64>,
}

/// Session-wide telemetry summary (PLAN §4.4). The per-failure windows are
/// carried by the `FailureCorrelation` list in the report, so this struct
/// only holds the global min/avg/max.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TelemetrySummary {
    pub sample_count: u64,
    pub duration_secs: f64,
    pub temp_gpu_c: Stats,
    pub power_draw_w: Stats,
    pub clock_graphics_mhz: Stats,
    pub fan_pct: Stats,
    pub util_gpu_pct: Stats,
}

/// Outcome of one user-executed isolation experiment (PLAN §5 Stage 4).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExperimentOutcome {
    /// Failure no longer reproduces after the change (X1–X6, X8).
    Fixed,
    /// Failure still reproduces after the change (X1–X6, X8).
    StillReproducing,
    /// X7: failure reproduces in the known-good second system (card side).
    ReproducedInSecondSystem,
    /// X7: failure does not reproduce in the second system (system side).
    NotReproducedInSecondSystem,
}

/// One recorded experiment run, folded into the report's `experiments_run`
/// and (when scored) into the §6 evidence set.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExperimentRun {
    /// Experiment id, e.g. `X3`.
    pub id: String,
    pub ts: DateTime<Utc>,
    pub outcome: ExperimentOutcome,
    pub note: Option<String>,
}

/// The shareable report (PLAN §4.4): the unit of replication. Anyone can
/// diff two reports (before/after a change, or two machines).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Report {
    pub ts: DateTime<Utc>,
    /// Monitor session sidecar this report was built from.
    pub session: String,
    pub baseline: Option<Baseline>,
    /// User timeline answers, duplicated from the baseline for readers who
    /// only look at the top level (PLAN §4.4 `timeline`).
    pub timeline: Option<TimelineAnswers>,
    pub telemetry: TelemetrySummary,
    pub xid_events: Vec<XidEvent>,
    pub markers: Vec<FailureMarker>,
    pub correlations: Vec<FailureCorrelation>,
    pub experiments_run: Vec<ExperimentRun>,
    pub evidence: Vec<Evidence>,
    /// Per-suspect scores (same data as `verdict.ranked`).
    pub scores: Vec<RankedSuspect>,
    pub verdict: Verdict,
}
