//! Core data model (PLAN §4.3). M1 uses `Baseline` and its parts; the
//! telemetry/evidence/verdict types are defined now so M2–M4 build on them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// UTC timestamp for records.
pub fn now() -> DateTime<Utc> {
    Utc::now()
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

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerKind {
    UserMark,
    DetectedReset,
}

impl Default for MarkerKind {
    fn default() -> Self {
        MarkerKind::UserMark
    }
}

// ---------------------------------------------------------------------------
// Stage 3/5 — scoring & verdict
// ---------------------------------------------------------------------------

#[derive(
    Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord,
)]
pub enum Suspect {
    Driver,
    Bios,
    PcieConnector,
    Psu,
    CardHardware,
    Cooling,
}

impl Default for Suspect {
    fn default() -> Self {
        Suspect::Driver
    }
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

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RankedSuspect {
    pub suspect: Suspect,
    pub score: i32,
    /// Evidence ids contributing to this suspect.
    pub evidence: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Verdict {
    pub ranked: Vec<RankedSuspect>,
    pub next_experiment: Option<String>,
}
