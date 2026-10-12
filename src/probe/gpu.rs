//! GPU probe: `nvidia-smi` state (PLAN §5 Stage 0).
//!
//! Targets the single RTX 5090. On a multi-GPU box the probe selects the card
//! by name (`probe::target`) and records a guardrail note in the summary.

use crate::probe::target::{guardrail_note, select_target_index};
use crate::probe::{BaselineCtx, Probe, ProbeResult};
use crate::util::cmd::{csv_fields, parse_f64, parse_u32};
use serde_json::json;

pub struct GpuProbe;

const QUERY: &str = "name,serial,driver_version,vbios_version,pci.bus_id,\
temperature.gpu,temperature.memory,power.draw,power.limit,\
clocks.current.graphics,clocks.max.sm,memory.used,memory.total,\
utilization.gpu,utilization.memory";

impl Probe for GpuProbe {
    fn name(&self) -> &str {
        "gpu"
    }

    fn run(&self, ctx: &BaselineCtx) -> ProbeResult {
        let r = &ctx.runner;

        let out = r.run(
            "nvidia-smi",
            &["--query-gpu", QUERY, "--format=csv,noheader,nounits"],
        );
        if out.missing {
            return ProbeResult::Degraded {
                reason: "nvidia-smi not found (NVIDIA driver not installed?)".into(),
            };
        }
        if !out.ok {
            return ProbeResult::Degraded {
                reason: format!("nvidia-smi failed ({})", out.failure_reason()),
            };
        }

        let rows: Vec<Vec<Option<String>>> = out
            .stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(csv_fields)
            .collect();
        if rows.is_empty() {
            return ProbeResult::Degraded {
                reason: "nvidia-smi reported no GPUs".into(),
            };
        }

        // Select the target card by name (PLAN §7 M5 multi-GPU guardrail);
        // the 5090 is not guaranteed to be row 0 on a multi-GPU box.
        let names: Vec<Option<String>> = rows
            .iter()
            .map(|r| r.first().and_then(|v| v.clone()))
            .collect();
        let (index, total, matched_by_name) = select_target_index(&names);
        let target_name = rows[index]
            .first()
            .and_then(|v| v.as_deref())
            .unwrap_or("?")
            .to_string();
        let guard = guardrail_note(total, index, &target_name, matched_by_name);

        let f = |i: usize| rows[index].get(i).and_then(|v| v.clone());
        let name = f(0);
        let serial = f(1);
        let driver_version = f(2);
        let vbios = f(3);
        let bdf = f(4);

        let temp_gpu = parse_u32(f(5).as_deref());
        let temp_mem = parse_u32(f(6).as_deref());
        let power_draw_w = parse_f64(f(7).as_deref());
        let power_limit_w = parse_f64(f(8).as_deref());
        let clock_graphics_mhz = parse_u32(f(9).as_deref());
        let clock_sm_mhz = parse_u32(f(10).as_deref());
        let mem_used_mb = parse_u32(f(11).as_deref());
        let mem_total_mb = parse_u32(f(12).as_deref());
        let util_gpu_pct = parse_u32(f(13).as_deref());
        let util_mem_pct = parse_u32(f(14).as_deref());

        // Deeper state: max operating temp, throttle reasons, ECC support.
        let (max_op_temp_c, throttle_reasons, ecc) = deep_state(r);

        let summary = format!(
            "{} @ {}°C, {}W (limit {}W){}",
            name.as_deref().unwrap_or("GPU"),
            temp_gpu.unwrap_or(0),
            format_power(power_draw_w),
            format_power(power_limit_w),
            guard
                .as_deref()
                .map(|n| format!("; {n}"))
                .unwrap_or_default()
        );

        ProbeResult::Ok {
            summary,
            data: json!({
                "name": name,
                "serial": serial,
                "driver_version": driver_version,
                "vbios": vbios,
                "bdf": bdf,
                "temp_gpu": temp_gpu,
                "temp_mem": temp_mem,
                "power_draw_w": power_draw_w,
                "power_limit_w": power_limit_w,
                "clock_graphics_mhz": clock_graphics_mhz,
                "clock_sm_mhz": clock_sm_mhz,
                "mem_used_mb": mem_used_mb,
                "mem_total_mb": mem_total_mb,
                "util_gpu_pct": util_gpu_pct,
                "util_mem_pct": util_mem_pct,
                "max_op_temp_c": max_op_temp_c,
                "throttle_reasons": throttle_reasons,
                "ecc": ecc,
                "multi_gpu_note": guard,
            }),
        }
    }
}

/// `nvidia-smi -q -d TEMPERATURE,PERFORMANCE,ECC` → max op temp, throttle
/// reasons, ECC state. Best-effort: any parse miss is `None`.
fn deep_state(
    r: &crate::util::cmd::CommandRunner,
) -> (Option<u32>, Vec<(String, String)>, Option<String>) {
    let out = r.run("nvidia-smi", &["-q", "-d", "TEMPERATURE,PERFORMANCE,ECC"]);
    if !out.ok {
        return (None, vec![], None);
    }
    let text = out.stdout;

    // Max operating temp spec; 0 means the driver does not publish one.
    let max_op_temp_c = text.lines().find_map(|l| {
        l.trim()
            .strip_prefix("GPU Max Operating T.Limit Temp Specification")
            .and_then(|rest| rest.split_once(':').map(|(_, v)| v.trim()))
            .and_then(|v| v.trim_end_matches('C').trim().parse().ok())
            .filter(|t| *t > 0)
    });

    // Throttle reasons live under "Clocks Event Reasons" (includes the
    // indented sub-reasons such as "HW Thermal Slowdown"); the section ends
    // at "Clocks Event Reasons Counters". Statuses are "Active",
    // "Not Active", or a cumulative time ("218775 us" = has fired).
    let mut throttle_reasons = vec![];
    let mut in_throttle = false;
    for line in text.lines() {
        let t = line.trim();
        if t == "Clocks Event Reasons" {
            in_throttle = true;
            continue;
        }
        if t == "Clocks Event Reasons Counters" {
            in_throttle = false;
            continue;
        }
        if in_throttle && let Some((name, status)) = t.split_once(':') {
            let name = name.trim();
            let status = status.trim();
            if !name.is_empty() && !status.is_empty() {
                throttle_reasons.push((name.to_string(), status.to_string()));
            }
        }
    }

    // ECC state: "ECC Mode" → "Current : N/A | Supported | Enabled".
    let mut ecc: Option<String> = None;
    let mut in_ecc_mode = false;
    for line in text.lines() {
        let t = line.trim();
        if t == "ECC Mode" {
            in_ecc_mode = true;
            continue;
        }
        if in_ecc_mode && let Some((_, v)) = t.split_once(':') {
            let v = v.trim();
            ecc = (v != "N/A").then(|| v.to_string());
            break;
        }
    }

    (max_op_temp_c, throttle_reasons, ecc)
}

fn format_power(w: Option<f64>) -> String {
    match w {
        Some(v) => format!("{v:.0}"),
        None => "?".into(),
    }
}
