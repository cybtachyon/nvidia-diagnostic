//! Log probe: Xid / NVRM history from `dmesg` (PLAN §5 Stage 0).
//!
//! The Xid stream is the most informative signal for a driver-level GPU
//! reset, so this probe is the seed for the Stage-1 correlation.

use crate::events::xid::parse_nvrm_lines;
use crate::probe::{BaselineCtx, Probe, ProbeResult};
use serde_json::json;

pub struct LogsProbe;

impl Probe for LogsProbe {
    fn name(&self) -> &str {
        "logs"
    }

    fn requires_root(&self) -> bool {
        // dmesg is restricted on modern kernels (kernel.dmesg_restrict=1).
        true
    }

    fn run(&self, ctx: &BaselineCtx) -> ProbeResult {
        let r = &ctx.runner;

        // Prefer `dmesg -T` (human timestamps); fall back to journalctl.
        let (out, via) = match r.run("dmesg", &["-T"]) {
            o if o.ok && !o.stdout.trim().is_empty() => (o, "dmesg".to_string()),
            _ => match r.run("journalctl", &["-k", "--no-pager"]) {
                o if o.ok && !o.stdout.trim().is_empty() => (o, "journalctl".to_string()),
                other => {
                    return ProbeResult::Degraded {
                        reason: format!(
                            "kernel log not readable (dmesg: {})",
                            other.failure_reason()
                        ),
                    };
                }
            },
        };

        let (nvrm_lines, xid_events) = parse_nvrm_lines(out.stdout.lines());

        let summary = if xid_events.is_empty() {
            format!(
                "no Xid events in kernel history ({via}); {} NVRM lines",
                nvrm_lines.len()
            )
        } else {
            let ids: Vec<u32> = xid_events.iter().map(|e| e.id).collect();
            format!("{} Xid event(s) in history: {ids:?}", xid_events.len())
        };

        ProbeResult::Ok {
            summary,
            data: json!({
                "xid_events": xid_events,
                "nvrm_lines": nvrm_lines,
                "source": via,
            }),
        }
    }
}

