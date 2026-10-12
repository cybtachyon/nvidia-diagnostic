//! BIOS probe: `dmidecode` (root) — the suspect behind H2.

use crate::probe::{BaselineCtx, Probe, ProbeResult};
use serde_json::json;

pub struct BiosProbe;

impl Probe for BiosProbe {
    fn name(&self) -> &str {
        "bios"
    }

    fn requires_root(&self) -> bool {
        true
    }

    fn run(&self, ctx: &BaselineCtx) -> ProbeResult {
        let r = &ctx.runner;

        // dmidecode accepts only ONE -s selector per invocation.
        let string = |sel: &str| -> Option<String> {
            let out = r.run("dmidecode", &["-s", sel]);
            if out.missing {
                return None;
            }
            out.stdout_if_ok()
        };

        if string("bios-version").is_none() {
            let out = r.run("dmidecode", &["-s", "bios-version"]);
            let reason = if out.missing {
                "dmidecode not installed (BIOS version unknown)".to_string()
            } else {
                format!("dmidecode failed ({})", out.failure_reason())
            };
            return ProbeResult::Degraded { reason };
        }

        let version = string("bios-version");
        let date = string("bios-date");
        let vendor = string("bios-vendor");

        ProbeResult::Ok {
            summary: format!(
                "BIOS {} ({}) {}",
                version.as_deref().unwrap_or("?"),
                date.as_deref().unwrap_or("?"),
                vendor.as_deref().unwrap_or("?")
            ),
            data: json!({
                "version": version,
                "date": date,
                "vendor": vendor,
                "note": null,
            }),
        }
    }
}
