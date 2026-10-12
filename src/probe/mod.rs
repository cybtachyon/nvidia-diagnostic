//! Stage-0 probes (PLAN §4.2, §5 Stage 0).
//!
//! A probe never aborts the run: a failed or missing command yields
//! `ProbeResult::Degraded { reason }` and the report flags the gap. Root-only
//! probes degrade to a warning when not running as root.

pub mod bios;
pub mod gpu;
pub mod logs;
pub mod pcie;
pub mod system;
pub mod target;

use crate::util::cmd::CommandRunner;
use serde_json::Value;

/// Context handed to every probe. Root availability is owned by the runner
/// (`cmd_baseline` warns on root-only probes); probes only need the runner.
pub struct BaselineCtx {
    pub runner: CommandRunner,
}

/// Outcome of a probe.
///
/// `data` uses the same field names as the corresponding `model.rs` struct, so
/// `main` can `serde_json::from_value` it straight into the model.
pub enum ProbeResult {
    Ok {
        /// One-line human summary for the baseline table.
        summary: String,
        data: Value,
    },
    Degraded {
        reason: String,
    },
}

impl ProbeResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, ProbeResult::Ok { .. })
    }

    pub fn summary(&self) -> &str {
        match self {
            ProbeResult::Ok { summary, .. } => summary,
            ProbeResult::Degraded { reason } => reason,
        }
    }
}

/// A named, self-describing probe (PLAN §4.2).
pub trait Probe {
    fn name(&self) -> &str;

    /// Root-only probes (dmidecode, dmesg on restricted kernels) report this;
    /// the runner degrades them with a warning instead of failing.
    fn requires_root(&self) -> bool {
        false
    }

    fn run(&self, ctx: &BaselineCtx) -> ProbeResult;
}
