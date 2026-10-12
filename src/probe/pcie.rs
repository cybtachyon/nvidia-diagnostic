//! PCIe probe: `lspci -vvv` — LnkCap vs LnkSta and AER counters (PLAN §5
//! Stage 0). Link running below capability is a BIOS/PCIe-path signal.

use crate::probe::target::resolve_gpu_target;
use crate::probe::{BaselineCtx, Probe, ProbeResult};
use crate::util::cmd::CommandRunner;
use regex::Regex;
use serde_json::json;

pub struct PcieProbe;

/// Map a PCIe speed in GT/s to a generation number.
/// Public so the Stage-1 monitor can decode the sysfs
/// `current_link_speed` fallback (non-root link state).
pub fn gen_from_gts(speed: f64) -> Option<u8> {
    match speed {
        2.5 => Some(1),
        5.0 => Some(2),
        8.0 => Some(3),
        16.0 => Some(4),
        32.0 => Some(5),
        64.0 => Some(6),
        _ => None,
    }
}

/// Parse `Speed 32GT/s, Width x16`-style text from a LnkCap/LnkSta line.
/// Public so the Stage-1 monitor can poll LnkSta every 2 s (PLAN §5).
pub fn parse_link_line(line: &str) -> Option<(u8, u8)> {
    let re = Regex::new(r"Speed\s+(\d+(?:\.\d+)?)\s*GT/s.*?Width\s+x(\d+)").ok()?;
    let caps = re.captures(line)?;
    let speed: f64 = caps.get(1)?.as_str().parse().ok()?;
    let width: u8 = caps.get(2)?.as_str().parse().ok()?;
    gen_from_gts(speed).map(|gen_no| (gen_no, width))
}

impl Probe for PcieProbe {
    fn name(&self) -> &str {
        "pcie"
    }

    fn requires_root(&self) -> bool {
        // LnkCap/LnkSta are usually world-readable; AER detail is not.
        true
    }

    fn run(&self, ctx: &BaselineCtx) -> ProbeResult {
        let r: &CommandRunner = &ctx.runner;

        // Locate the target card's BDF (PLAN §7 M5 multi-GPU guardrail).
        // nvidia-smi reports the full form `00000000:01:00.0`; lspci accepts
        // the short `01:00.0`.
        let Some(target) = resolve_gpu_target(r) else {
            return ProbeResult::Degraded {
                reason: "could not determine GPU PCI bus id (nvidia-smi unavailable?)".into(),
            };
        };
        let bdf_full = target.bdf_full.clone();
        let bdf_short = bdf_full
            .rsplit_once(':')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| bdf_full.clone());

        let out = r.run("lspci", &["-vvv", "-s", &bdf_short]);
        if out.missing {
            return ProbeResult::Degraded {
                reason: "lspci not installed (PCIe link state unknown)".into(),
            };
        }
        if !out.ok {
            return ProbeResult::Degraded {
                reason: format!("lspci failed ({})", out.failure_reason()),
            };
        }

        let text = out.stdout.clone();
        let access_denied = text.contains("Capabilities: <access denied>");
        let lnk_cap = text
            .lines()
            .find(|l| l.contains("LnkCap:"))
            .and_then(parse_link_line);
        let lnk_sta = text
            .lines()
            .find(|l| l.contains("LnkSta:"))
            .and_then(parse_link_line);
        if lnk_cap.is_none() && lnk_sta.is_none() {
            return ProbeResult::Degraded {
                reason: if access_denied {
                    "lspci: PCIe capabilities not accessible without root (link state unknown)"
                        .into()
                } else {
                    "lspci: LnkCap/LnkSta not found in output".into()
                },
            };
        }
        let aer_ue = text
            .lines()
            .find(|l| l.trim_start().starts_with("UESta:"))
            .map(|l| l.trim().to_string());
        let aer_ce = text
            .lines()
            .find(|l| l.trim_start().starts_with("CESta:"))
            .map(|l| l.trim().to_string());

        // Any `+` in an AER status string means a latched error bit.
        let aer_flag = |s: &Option<String>| -> Option<bool> { s.as_ref().map(|v| v.contains('+')) };
        let ue_err = aer_flag(&aer_ue);
        let ce_err = aer_flag(&aer_ce);

        let degraded_link = matches!(
            (lnk_cap, lnk_sta),
            (Some(c), Some(s)) if s.0 < c.0 || s.1 < c.1
        );

        let mut summary = match (&lnk_cap, &lnk_sta) {
            (Some((cg, cw)), Some((sg, sw))) if !degraded_link => {
                format!("PCIe Gen{sg} x{sw} (cap Gen{cg} x{cw}) — at capability")
            }
            (Some((cg, cw)), Some((sg, sw))) => {
                format!("PCIe Gen{sg} x{sw} running BELOW capability Gen{cg} x{cw} ⚠",)
            }
            _ => "PCIe link state unavailable".to_string(),
        };
        // Surface the multi-GPU guardrail (PLAN §7 M5) next to the link state.
        if let Some(note) = target.guardrail_note() {
            summary.push_str(&format!("; {note}"));
        }

        ProbeResult::Ok {
            summary,
            data: json!({
                "bdf": bdf_full,
                "lnk_cap": lnk_cap,
                "lnk_sta": lnk_sta,
                "aer_ue": aer_ue,
                "aer_ce": aer_ce,
                "aer_ue_errors": ue_err,
                "aer_ce_errors": ce_err,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_link_cap_and_sta() {
        assert_eq!(
            parse_link_line("LnkCap:\tSpeed 32GT/s, Width x16, ASPM L1"),
            Some((5, 16))
        );
        assert_eq!(
            parse_link_line("LnkSta:\tSpeed 16GT/s (downgraded), Width x8"),
            Some((4, 8))
        );
        assert_eq!(
            parse_link_line("LnkSta: Speed 2.5GT/s, Width x1"),
            Some((1, 1))
        );
        assert_eq!(parse_link_line("no link info here"), None);
    }

    #[test]
    fn maps_gt_s_to_generation() {
        assert_eq!(gen_from_gts(2.5), Some(1));
        assert_eq!(gen_from_gts(5.0), Some(2));
        assert_eq!(gen_from_gts(8.0), Some(3));
        assert_eq!(gen_from_gts(16.0), Some(4));
        assert_eq!(gen_from_gts(32.0), Some(5));
        assert_eq!(gen_from_gts(64.0), Some(6));
        assert_eq!(gen_from_gts(12.0), None);
    }
}
