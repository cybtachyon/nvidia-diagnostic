//! `nvidia-debug` — reproducible root-cause diagnosis for RTX 5090 context
//! loss (PLAN §4.1). M1: subcommand dispatch, interactive menu, Stage-0
//! baseline snapshot and the colored baseline table. M2: live telemetry
//! monitor (`monitor` subcommand).

mod events;
mod model;
mod monitor;
mod probe;
mod telemetry;
mod util;
mod workload;

use anyhow::Result;
use chrono::DateTime;
use model::{
    now, Baseline, BiosInfo, DriverInfo, GpuInfo, PcieInfo, SystemInfo, TimelineAnswers,
    XidEvent,
};
use owo_colors::OwoColorize;
use std::io::IsTerminal;
use probe::{bios::BiosProbe, gpu::GpuProbe, logs::LogsProbe, pcie::PcieProbe, system::SystemProbe};
use probe::{BaselineCtx, Probe, ProbeResult};
use serde_json::Value;
use util::cmd::{read_file_lossy, CommandRunner};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("baseline") => cmd_baseline(&args[1..]),
        Some("monitor") => cmd_monitor(&args[1..]),
        Some("experiment") => not_yet("experiment", "M4"),
        Some("report") => not_yet("report", "M4"),
        Some(a) if a == "--help" || a == "-h" => {
            print_usage();
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown subcommand: {other}\n");
            print_usage();
            Err(anyhow::anyhow!("unknown subcommand"))
        }
        None => interactive_menu(),
    }
}

fn print_usage() {
    println!(
        "\
nvidia-debug — reproducible root-cause diagnosis for RTX 5090 context loss

Usage:
  nvidia-debug                  interactive menu
  nvidia-debug baseline         preflight + Stage-0 baseline snapshot + colored table
                                (--json [PATH] also writes the baseline as JSON)
  nvidia-debug monitor          live telemetry monitor: 10 Hz JSONL, 1 s Xid
                                scan, `m` marks black screens, auto reset detect
                                [--duration N] [--jsonl PATH] [--interval MS]
                                [--workload CMD [ARGS...]]
  nvidia-debug experiment <N>   rerun after isolation experiment N (M4)
  nvidia-debug report           view / export previous report (M4)
  nvidia-debug --help           this help"
    );
}

fn not_yet(name: &str, milestone: &str) -> Result<()> {
    println!("`{name}` is not implemented yet (lands in {milestone}); see PLAN.md §11.");
    Ok(())
}

fn interactive_menu() -> Result<()> {
    let items = vec![
        "Full diagnostic battery (recommended)",
        "Monitor only",
        "Rerun an isolation experiment",
        "View / export previous report",
    ];
    let choice = inquire::Select::new("What do you want to do?", items.clone()).prompt()?;
    match choice {
        "Full diagnostic battery (recommended)" => {
            println!("Running the full battery — Stage 0 baseline now (analysis lands in M3; run `monitor` under the workload).\n");
            cmd_baseline(&[])
        }
        "Monitor only" => cmd_monitor(&[]),
        "Rerun an isolation experiment" => not_yet("experiment", "M4"),
        "View / export previous report" => not_yet("report", "M4"),
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// baseline subcommand
// ---------------------------------------------------------------------------

fn cmd_baseline(args: &[String]) -> Result<()> {
    // `--json [PATH]`
    let json_path = args.iter().position(|a| a == "--json").map(|i| args.get(i + 1).filter(|a| !a.starts_with('-')).cloned());

    // --- Preflight (PLAN §4.1 step 1) ---
    let is_root = euid_is_zero();
    let kernel = CommandRunner::new().stdout("uname", &["-r"]);
    println!(
        "preflight: kernel {} · nvidia-smi {} · root {}",
        kernel.as_deref().unwrap_or("?").bold(),
        if which_ok("nvidia-smi") {
            "found".green().to_string()
        } else {
            "NOT FOUND".red().to_string()
        },
        if is_root {
            "yes".green().to_string()
        } else {
            "no (root-only probes will degrade)".yellow().to_string()
        },
    );
    if !which_ok("nvidia-smi") {
        println!(
            "{}",
            "warning: nvidia-smi missing — GPU probes will degrade; install the NVIDIA driver first"
                .yellow()
        );
    }

    // --- Stage 0: run all probes (none of them aborts the run) ---
    let ctx = BaselineCtx {
        runner: CommandRunner::new(),
        is_root,
    };
    let probes: Vec<Box<dyn Probe>> = vec![
        Box::new(SystemProbe),
        Box::new(BiosProbe),
        Box::new(GpuProbe),
        Box::new(PcieProbe),
        Box::new(LogsProbe),
    ];
    let results: Vec<(String, ProbeResult)> =
        probes.iter().map(|p| (p.name().to_string(), p.run(&ctx))).collect();

    // --- Timeline interview (interactive only) ---
    let timeline = if std::io::stdin().is_terminal() {
        ask_timeline()?
    } else {
        TimelineAnswers::default()
    };

    // --- Assemble the model ---
    let baseline = assemble_baseline(&results, timeline);

    // --- Colored baseline table ---
    print_baseline_table(&baseline, &results);

    // --- Optional JSON export ---
    if let Some(path) = json_path {
        let path = path.unwrap_or_else(|| {
            format!(
                "baseline-{}.json",
                baseline.ts.format("%Y%m%dT%H%M%SZ")
            )
        });
        let json = serde_json::to_string_pretty(&baseline)
            .map_err(|e| anyhow::anyhow!("serialize baseline: {e}"))?;
        std::fs::write(&path, json)?;
        println!("\n{} {}", "baseline written:".bold(), path.bold());
    }

    Ok(())
}

fn euid_is_zero() -> bool {
    let status = read_file_lossy("/proc/self/status");
    status
        .as_deref()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
        })
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|uid| uid == 0)
}

fn which_ok(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// monitor subcommand
// ---------------------------------------------------------------------------

fn cmd_monitor(args: &[String]) -> Result<()> {
    let mut opts = monitor::MonitorOptions {
        duration_secs: None,
        jsonl: None,
        workload: None,
        interval_ms: 100,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--duration" => {
                i += 1;
                opts.duration_secs = args.get(i).and_then(|s| s.parse().ok());
            }
            "--jsonl" => {
                i += 1;
                opts.jsonl = args.get(i).map(std::path::PathBuf::from);
            }
            "--interval" => {
                i += 1;
                opts.interval_ms = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(100);
            }
            // Everything after `--workload` is the workload command.
            "--workload" => {
                i += 1;
                let rest: Vec<String> = args[i..].to_vec();
                if rest.is_empty() {
                    anyhow::bail!("--workload needs a command");
                }
                opts.workload = Some(rest);
                i = args.len();
            }
            other => anyhow::bail!("unknown monitor argument: {other}"),
        }
        i += 1;
    }
    monitor::run_monitor(opts)
}

fn ask_timeline() -> Result<TimelineAnswers> {
    let failure_start = inquire::Text::new(
        "When did the failures start? (e.g. 'last Tuesday after the BIOS update')",
    )
    .prompt()?;

    let change_items = [
        "BIOS update",
        "GPU driver update",
        "PSU / power change",
        "New or heavier workload",
        "None / not sure",
    ];
    let changes_around: Vec<String> = inquire::MultiSelect::new(
        "What changed around that time?",
        change_items.into_iter().map(String::from).collect(),
    )
    .prompt()?;

    Ok(TimelineAnswers {
        failure_start: (!failure_start.trim().is_empty())
            .then(|| failure_start.trim().to_string()),
        changes_around,
        notes: None,
    })
}

fn data_of<'a>(results: &'a [(String, ProbeResult)], name: &str) -> Option<&'a Value> {
    results
        .iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, r)| match r {
            ProbeResult::Ok { data, .. } => Some(data),
            _ => None,
        })
}

fn from_data<T: serde::de::DeserializeOwned + Default>(
    results: &[(String, ProbeResult)],
    name: &str,
) -> T {
    data_of(results, name)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

fn assemble_baseline(
    results: &[(String, ProbeResult)],
    timeline: TimelineAnswers,
) -> Baseline {
    let system: SystemInfo = from_data(results, "system");
    let bios: BiosInfo = from_data(results, "bios");
    let gpu: GpuInfo = from_data(results, "gpu");
    let pcie: PcieInfo = from_data(results, "pcie");

    // Driver info is split across two probes: userland version from
    // nvidia-smi (carried in the gpu payload), kernel-module version from
    // modinfo (system payload).
    let driver_version = data_of(results, "gpu")
        .and_then(|v| v.get("driver_version"))
        .and_then(Value::as_str)
        .map(String::from);
    let driver = DriverInfo {
        driver_version,
        vbios_version: gpu.vbios.clone(),
        module_version: system.nvidia_module_version.clone(),
    };

    // Xid history from the logs probe.
    let xid_history: Vec<XidEvent> = data_of(results, "logs")
        .and_then(|v| v.get("xid_events"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    Baseline {
        ts: now(),
        system,
        bios,
        driver,
        gpu,
        pcie,
        xid_history,
        user_timeline: timeline,
    }
}

// ---------------------------------------------------------------------------
// colored baseline table
// ---------------------------------------------------------------------------

const LABEL_W: usize = 22;

fn label(s: &str) -> String {
    format!("{s:width$}", width = LABEL_W)
}

fn print_baseline_table(b: &Baseline, results: &[(String, ProbeResult)]) {
    let status = |probe: &str| -> String {
        match results.iter().find(|(n, _)| n == probe) {
            Some((_, ProbeResult::Ok { .. })) => "ok".green().to_string(),
            Some((_, ProbeResult::Degraded { .. })) => "degraded".yellow().to_string(),
            None => "missing".red().to_string(),
        }
    };
    // Section header; the status badge is taken from the named probe
    // (e.g. the "driver" section is sourced by the "gpu" probe).
    let header = |section: &str, probe: &str| {
        format!("{} [{}]", section.to_uppercase().bold(), status(probe))
    };
    let reason = |probe: &str| -> Option<String> {
        results.iter().find(|(n, _)| n == probe).and_then(|(_, r)| match r {
            ProbeResult::Degraded { reason } => Some(reason.clone()),
            _ => None,
        })
    };
    let opt = |v: Option<&str>| v.map(str::to_owned).unwrap_or_else(|| "—".to_string());
    let opt_color = |v: Option<&str>| match v {
        Some(x) => x.to_string(),
        None => "—".yellow().to_string(),
    };

    println!();
    println!(
        "{} {}",
        "nvidia-debug — baseline snapshot".bold(),
        format_timestamp(&b.ts).dimmed()
    );

    // SYSTEM
    println!("\n{}", header("system", "system"));
    println!("{}{}", label("kernel"), b.system.kernel.as_deref().unwrap_or("—"));
    println!("{}{}", label("arch"), b.system.arch.as_deref().unwrap_or("—"));
    println!("{}{}", label("os"), b.system.os.as_deref().unwrap_or("—"));
    println!(
        "{}{}",
        label("nvidia module"),
        opt_color(b.system.nvidia_module_version.as_deref())
    );
    if !b.system.nvidia_modules_loaded.is_empty() {
        println!(
            "{}{}",
            label("modules loaded"),
            b.system.nvidia_modules_loaded.join(", ").dimmed()
        );
    }

    // BIOS
    println!("\n{}", header("bios", "bios"));
    if b.bios.version.is_some() {
        println!("{}{}", label("version"), opt(b.bios.version.as_deref()));
        println!("{}{}", label("date"), opt(b.bios.date.as_deref()));
        println!("{}{}", label("vendor"), opt(b.bios.vendor.as_deref()));
    } else {
        let why = reason("bios").unwrap_or_else(|| "unknown".to_string());
        println!("{}{}", label("version"), why.yellow());
    }

    // DRIVER
    println!("\n{}", header("driver", "gpu"));
    println!(
        "{}{}",
        label("driver version"),
        opt_color(b.driver.driver_version.as_deref())
    );
    println!("{}{}", label("vbios"), opt(b.driver.vbios_version.as_deref()));
    println!(
        "{}{}",
        label("module version"),
        opt_color(b.driver.module_version.as_deref())
    );

    // GPU
    println!("\n{}", header("gpu", "gpu"));
    println!("{}{}", label("name"), opt(b.gpu.name.as_deref()).bold());
    println!("{}{}", label("serial"), opt(b.gpu.serial.as_deref()));
    println!("{}{}", label("vbios"), opt(b.gpu.vbios.as_deref()));
    println!("{}{}", label("bdf"), opt(b.gpu.bdf.as_deref()));
    let temp = b.gpu.temp_gpu.map(|t| t.to_string()).unwrap_or_default();
    let max = b.gpu.max_op_temp_c.map(|t| t.to_string()).unwrap_or_default();
    println!(
        "{}{} °C (max op {} °C)",
        label("temp"),
        if temp.is_empty() {
            "—".yellow().to_string()
        } else {
            temp
        },
        if max.is_empty() {
            "?".to_string()
        } else {
            max
        }
    );
    println!(
        "{}{} W (limit {} W)",
        label("power"),
        b.gpu
            .power_draw_w
            .map(|p| format!("{p:.0}"))
            .unwrap_or_else(|| "?".into()),
        b.gpu
            .power_limit_w
            .map(|p| format!("{p:.0}"))
            .unwrap_or_else(|| "?".into())
    );
    println!(
        "{}{} MHz / {} MHz",
        label("clocks (gfx/sm)"),
        b.gpu
            .clock_graphics_mhz
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into()),
        b.gpu
            .clock_sm_mhz
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into())
    );
    println!(
        "{}{} MiB / {} MiB",
        label("memory"),
        b.gpu
            .mem_used_mb
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into()),
        b.gpu
            .mem_total_mb
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into())
    );
    if !b.gpu.throttle_reasons.is_empty() {
        let active: Vec<String> = b
            .gpu
            .throttle_reasons
            .iter()
            .filter(|(_, s)| s != "Not Active")
            .map(|(n, s)| format!("{n}={s}"))
            .collect();
        println!(
            "{}{}",
            label("throttle reasons"),
            if active.is_empty() {
                "none active".green().to_string()
            } else {
                active.join(", ").red().to_string()
            }
        );
    }
    if let Some(ecc) = b.gpu.ecc.as_deref() {
        println!("{}{}", label("ecc"), ecc);
    }

    // PCIE
    println!("\n{}", header("pcie", "pcie"));
    let (cap, sta) = (b.pcie.lnk_cap, b.pcie.lnk_sta);
    match (cap, sta) {
        (Some((cg, cw)), Some((sg, sw))) => {
            let degraded = sg < cg || sw < cw;
            let link = if degraded {
                format!("Gen{sg} x{sw} — BELOW capability Gen{cg} x{cw}").red().to_string()
            } else {
                format!("Gen{sg} x{sw} (cap Gen{cg} x{cw})").green().to_string()
            };
            println!("{}{}", label("link"), link);
        }
        _ => {
            let msg = reason("pcie")
                .map(|r| format!("unavailable — {r}"))
                .unwrap_or_else(|| "unavailable".to_string());
            println!("{}{}", label("link"), msg.yellow());
        }
    }
    let aer = |v: &Option<String>, flag: Option<bool>| -> String {
        match (v, flag) {
            (_, Some(true)) => "errors latched".red().to_string(),
            (Some(s), Some(false)) => format!("clean ({s})").dimmed().to_string(),
            _ => "n/a".dimmed().to_string(),
        }
    };
    let ue = data_of(results, "pcie")
        .and_then(|v| v.get("aer_ue_errors"))
        .and_then(Value::as_bool);
    let ce = data_of(results, "pcie")
        .and_then(|v| v.get("aer_ce_errors"))
        .and_then(Value::as_bool);
    println!(
        "{}UE {}, CE {}",
        label("aer"),
        aer(&b.pcie.aer_ue, ue),
        aer(&b.pcie.aer_ce, ce)
    );

    // XID HISTORY
    println!("\n{}", header("xid history", "logs"));
    if b.xid_history.is_empty() {
        match reason("logs") {
            Some(r) => println!(
                "{}{}",
                label("(none)"),
                format!("no Xid events readable — {r}").yellow()
            ),
            None => println!("{}no Xid events in kernel history", label("(none)")),
        }
    } else {
        for e in &b.xid_history {
            let msg = e
                .raw
                .strip_prefix('[')
                .and_then(|rest| rest.split_once(']').map(|x| x.1))
                .unwrap_or(&e.raw)
                .trim();
            println!(
                "{}Xid {} (PCI:{}) {}",
                label(e.ts.as_deref().unwrap_or("")),
                e.id,
                e.bdf.as_deref().unwrap_or("?"),
                msg.dimmed()
            );
        }
    }

    // TIMELINE
    println!("\n{}", "timeline".bold());
    println!(
        "{}{}",
        label("failures since"),
        b.user_timeline
            .failure_start
            .as_deref()
            .unwrap_or("(not asked)")
            .dimmed()
    );
    if !b.user_timeline.changes_around.is_empty() {
        println!(
            "{}{}",
            label("changes around then"),
            b.user_timeline.changes_around.join(", ")
        );
    }

    println!();
    println!(
        "{}",
        "next: run `nvidia-debug monitor` under the real workload (scoring lands in M3, PLAN.md §11)".dimmed()
    );
}

fn format_timestamp(ts: &DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}
