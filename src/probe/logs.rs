//! Log probe: Xid / NVRM history from `dmesg` (PLAN §5 Stage 0).
//!
//! The Xid stream is the most informative signal for a driver-level GPU
//! reset, so this probe is the seed for the Stage-1 correlation.

use crate::model::XidEvent;
use crate::probe::{BaselineCtx, Probe, ProbeResult};
use regex::Regex;
use serde_json::json;

pub struct LogsProbe;

/// `NVRM: Xid (PCI:01:00.0) 79`
const XID_RE: &str = r"NVRM: Xid \(PCI:([0-9a-fA-F]{2}:[0-9a-fA-F]{2}\.\d+)\)\s+(\d+)";
/// dmesg -T: `[Thu Oct 12 01:23:45 2025]`
const TS_DMESG_RE: &str = r"^\[([A-Z][a-z]{2} [A-Z][a-z]{2} \d+ \d{2}:\d{2}:\d{2} \d{4})\]";
/// journalctl -k: `Oct 12 01:23:45 host kernel:`
const TS_JOURNAL_RE: &str =
    r"^([A-Z][a-z]{2} \d+ \d{2}:\d{2}:\d{2}) \S+ kernel:";

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

/// Split kernel-log lines into (NVRM/Xid lines, parsed Xid events).
fn parse_nvrm_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
) -> (Vec<String>, Vec<XidEvent>) {
    let xid_re = Regex::new(XID_RE).unwrap();
    let ts_dmesg_re = Regex::new(TS_DMESG_RE).unwrap();
    let ts_journal_re = Regex::new(TS_JOURNAL_RE).unwrap();

    let mut nvrm_lines: Vec<String> = vec![];
    let mut xid_events: Vec<XidEvent> = vec![];

    for line in lines {
        if !line.contains("NVRM") && !line.contains("Xid") {
            continue;
        }
        nvrm_lines.push(line.trim().to_string());
        if let Some(caps) = xid_re.captures(line) {
            let ts = ts_dmesg_re
                .captures(line)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .or_else(|| {
                    ts_journal_re
                        .captures(line)
                        .and_then(|c| c.get(1))
                        .map(|m| m.as_str().to_string())
                });
            let id: u32 = caps
                .get(2)
                .map(|m| m.as_str().parse().unwrap_or(0))
                .unwrap_or(0);
            xid_events.push(XidEvent {
                ts,
                bdf: caps.get(1).map(|m| m.as_str().to_string()),
                id,
                raw: line.trim().to_string(),
            });
        }
    }

    (nvrm_lines, xid_events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xid_from_dmesg_t() {
        let (nvrm, events) = parse_nvrm_lines(
            ["[Thu Oct 12 01:23:45 2026] NVRM: Xid (PCI:01:00.0) 79, minor code 0x0"],
        );
        assert_eq!(nvrm.len(), 1);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.id, 79);
        assert_eq!(e.bdf.as_deref(), Some("01:00.0"));
        assert_eq!(e.ts.as_deref(), Some("Thu Oct 12 01:23:45 2026"));
    }

    #[test]
    fn parses_xid_from_journalctl() {
        let (_, events) = parse_nvrm_lines(
            ["Oct 12 01:23:45 host kernel: NVRM: Xid (PCI:41:00.0) 43, channel 0x0"],
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, 43);
        assert_eq!(events[0].bdf.as_deref(), Some("41:00.0"));
        assert_eq!(events[0].ts.as_deref(), Some("Oct 12 01:23:45"));
    }

    #[test]
    fn ignores_non_xid_nvrm_lines() {
        let (nvrm, events) = parse_nvrm_lines(
            [
                "Oct 12 01:12:37 host kernel: NVRM: loading NVIDIA UNIX Open Kernel Module",
                "Oct 12 01:12:37 host kernel: something else",
            ],
        );
        assert_eq!(nvrm.len(), 1);
        assert!(events.is_empty());
    }
}
