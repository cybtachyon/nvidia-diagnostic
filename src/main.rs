//! `nvidia-diagnostic` — reproducible root-cause diagnosis for RTX 5090 context
//! loss (PLAN §4.1). M1: subcommand dispatch, interactive menu, Stage-0
//! baseline snapshot and the colored baseline table. M2: live telemetry
//! monitor (`monitor` subcommand). M3: session analysis, pre-window
//! correlation, §6 scoring and the verdict (`analyze` subcommand,
//! full-battery flow). M4: guided isolation experiments (`experiment`)
//! and the shareable report (`report`).

mod analysis;
mod events;
mod experiments;
mod model;
mod monitor;
mod probe;
mod report;
mod score;
mod telemetry;
mod util;
mod workload;

use anyhow::Result;
use chrono::DateTime;
use model::{
    Baseline, BiosInfo, DriverInfo, GpuInfo, PcieInfo, SystemInfo, TimelineAnswers, XidEvent, now,
};
use owo_colors::OwoColorize;
use probe::{BaselineCtx, Probe, ProbeResult};
use probe::{
    bios::BiosProbe, gpu::GpuProbe, logs::LogsProbe, pcie::PcieProbe, system::SystemProbe,
};
use serde_json::Value;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use util::cmd::{CommandRunner, read_file_lossy};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("baseline") => cmd_baseline(&args[1..]),
        Some("monitor") => cmd_monitor(&args[1..]),
        Some("stress") => cmd_stress(&args[1..]),
        Some("analyze") => cmd_analyze(&args[1..]),
        Some("experiment") => cmd_experiment(&args[1..]),
        Some("report") => cmd_report(&args[1..]),
        Some("reconstruct") => cmd_reconstruct(&args[1..]),
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
nvidia-diagnostic — reproducible root-cause diagnosis for RTX 5090 context loss

Usage:
  nvidia-diagnostic                  interactive menu
  nvidia-diagnostic baseline         preflight + Stage-0 baseline snapshot + colored table
                                (--json [PATH] also writes the baseline as JSON)
  nvidia-diagnostic monitor          live telemetry monitor: 10 Hz JSONL, 1 s Xid
                                scan, `m` marks black screens, auto reset detect
                                [--duration N] [--jsonl PATH] [--interval MS]
                                [--workload CMD [ARGS...]]
  nvidia-diagnostic stress <SECS>    fixed-duration stress window under the monitor
                                (PLAN §5 Stage 2) with an `indicatif` progress
                                bar; typically a `--workload` stress command
                                [--workload CMD [ARGS...]] [--jsonl PATH]
                                [--interval MS]
  nvidia-diagnostic analyze [SESSION]
                                 M3: pre-window correlation + §6 scoring + verdict
                                 for a monitor session (defaults to the newest
                                 monitor-*.json); [--baseline PATH] adds the
                                 static evidence (link, AER, user timeline).
                                 Writes a report-<ts>.json
  nvidia-diagnostic experiment <X1..X8>
                                 M4: guided isolation experiment. Shows the
                                 steps, retests under the monitor, asks the
                                 outcome, folds it into the scores and writes
                                 a report. [--session PATH] [--baseline PATH]
                                 [--outcome fixed|still|reproduced|not-reproduced]
  nvidia-diagnostic report [PATH]      M4: view / export a previous report (defaults
                                 to the newest report-*.json)
  nvidia-diagnostic reconstruct <JSONL>
                                 Rebuild a monitor-*.json session sidecar from a
                                 raw telemetry JSONL (after a force shutdown
                                 skipped the sidecar write). Scans the kernel
                                 log for Xids in the session window.
  nvidia-diagnostic --help           this help"
    );
}

fn interactive_menu() -> Result<()> {
    let items = vec![
        "Full diagnostic battery (recommended)",
        "Monitor only",
        "Run a stress window (fixed duration)",
        "Rerun an isolation experiment",
        "View / export previous report",
    ];
    let choice = inquire::Select::new("What do you want to do?", items.clone()).prompt()?;
    match choice {
        "Full diagnostic battery (recommended)" => cmd_battery(),
        "Monitor only" => cmd_monitor(&[]),
        "Run a stress window (fixed duration)" => cmd_stress(&[]),
        "Rerun an isolation experiment" => cmd_experiment(&[]),
        "View / export previous report" => cmd_report(&[]),
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// baseline subcommand
// ---------------------------------------------------------------------------

fn cmd_baseline(args: &[String]) -> Result<()> {
    // `--json [PATH]`
    let json_path = args
        .iter()
        .position(|a| a == "--json")
        .and_then(|i| args.get(i + 1).filter(|a| !a.starts_with('-')).cloned())
        .map(PathBuf::from);
    run_baseline(json_path)
}

/// Stage-0 baseline snapshot; `json_path` (when set) also writes the
/// baseline as JSON for the M3 analysis.
fn run_baseline(json_path: Option<PathBuf>) -> Result<()> {
    // --- Preflight (PLAN §4.1 step 1, §7 M5 edge cases: missing tools, no root) ---
    let is_root = euid_is_zero();
    let runner = CommandRunner::new();
    let kernel = runner.stdout("uname", &["-r"]);
    println!(
        "preflight: kernel {} · root {}",
        kernel.as_deref().unwrap_or("?").bold(),
        if is_root {
            "yes".green().to_string()
        } else {
            "no (root-only probes will degrade)".yellow().to_string()
        },
    );
    print_preflight_tools(&runner);

    // --- Stage 0: run all probes (none of them aborts the run) ---
    let ctx = BaselineCtx { runner };
    let probes: Vec<Box<dyn Probe>> = vec![
        Box::new(SystemProbe),
        Box::new(BiosProbe),
        Box::new(GpuProbe),
        Box::new(PcieProbe),
        Box::new(LogsProbe),
    ];
    let results: Vec<(String, ProbeResult)> = probes
        .iter()
        .map(|p| (p.name().to_string(), p.run(&ctx)))
        .collect();

    // Root-only probes (dmidecode, dmesg, lspci AER) degrade without root;
    // warn once per probe so the gap is visible next to the table.
    if !is_root {
        for p in &probes {
            if p.requires_root() {
                println!(
                    "{}",
                    format!("warning: `{}` needs root; result degraded", p.name()).yellow()
                );
            }
        }
    }

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
        let json = serde_json::to_string_pretty(&baseline)
            .map_err(|e| anyhow::anyhow!("serialize baseline: {e}"))?;
        std::fs::write(&path, json)?;
        println!("\n{} {}", "baseline written:".bold(), path.display().bold());
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

/// External tools the probes depend on, with what degrades when each is
/// missing (PLAN §7 M5 edge cases: missing tools, no root).
const PREFLIGHT_TOOLS: &[(&str, &str, &[&str])] = &[
    ("nvidia-smi", "GPU state + live telemetry", &["--version"]),
    (
        "lspci",
        "PCIe link state (sysfs fallback for speed/width only)",
        &["-v"],
    ),
    ("dmidecode", "BIOS version (root)", &["-s", "bios-version"]),
    ("dmesg", "kernel-log Xid scan", &["-T"]),
    (
        "journalctl",
        "kernel-log Xid scan (dmesg fallback)",
        &["-k", "--no-pager", "-n", "1"],
    ),
    ("modinfo", "nvidia module version", &["nvidia"]),
    ("lsmod", "loaded nvidia modules", &[]),
];

/// A tool is present when its binary is found, regardless of exit code.
fn tool_present(r: &CommandRunner, tool: &str, args: &[&str]) -> bool {
    !r.run(tool, args).missing
}

/// Compact tool-availability summary: present tools in green, each missing
/// tool with a one-line note on what degrades.
fn print_preflight_tools(r: &CommandRunner) {
    let mut present: Vec<&str> = vec![];
    let mut missing: Vec<(&str, &str)> = vec![];
    for (tool, degrade, args) in PREFLIGHT_TOOLS {
        if tool_present(r, tool, args) {
            present.push(tool);
        } else {
            missing.push((tool, degrade));
        }
    }
    println!("{} {}", "tools:".dimmed(), present.join(" · ").green());
    for (tool, degrade) in &missing {
        println!(
            "  {} {} — {}",
            "missing:".dimmed(),
            tool.red(),
            degrade.yellow()
        );
    }
}

/// Full battery (PLAN §4.1): baseline → live monitor under the user's
/// workload → M3 analysis + verdict, chained in one run.
fn cmd_battery() -> Result<()> {
    println!("Running the full battery: baseline → monitor → analysis.\n");
    let baseline_path = PathBuf::from(format!("baseline-{}.json", model::stamp()));
    run_baseline(Some(baseline_path.clone()))?;

    let monitor = inquire::Confirm::new(
        "Start the live monitor now? (run your CUDA workload; press m on each black screen, q to stop)",
    )
    .with_default(true)
    .prompt()?;
    if !monitor {
        println!(
            "{}",
            format!(
                "run the monitor under the workload, then: nvidia-diagnostic analyze --baseline {}",
                baseline_path.display()
            )
            .dimmed()
        );
        return Ok(());
    }
    let session = monitor::run_monitor(monitor::MonitorOptions {
        duration_secs: None,
        jsonl: None,
        workload: None,
        interval_ms: 100,
    })?;
    let (analysis, loaded) = analysis::run_analysis(&session, Some(&baseline_path))?;
    analysis::print_report(&analysis, &loaded);
    write_session_report(&session, Some(baseline_path.as_path()))
}

/// M4: fold the session into the shareable `report-<ts>.json` (PLAN §4.1
/// step 8) and print where it landed.
fn write_session_report(session_path: &Path, baseline_path: Option<&Path>) -> Result<()> {
    let rep = report::build_report(session_path, baseline_path, &[])?;
    let path = report::write_report(&rep)?;
    println!();
    println!("{} {}", "report written:".bold(), path.display().bold());
    Ok(())
}

// ---------------------------------------------------------------------------
// analyze subcommand (M3)
// ---------------------------------------------------------------------------

fn cmd_analyze(args: &[String]) -> Result<()> {
    let mut session: Option<String> = None;
    let mut baseline: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--baseline" => {
                i += 1;
                baseline = args.get(i).cloned();
            }
            a if a.starts_with('-') => anyhow::bail!("unknown analyze argument: {a}"),
            a if session.is_none() => session = Some(a.to_string()),
            a => anyhow::bail!("unexpected analyze argument: {a}"),
        }
        i += 1;
    }
    let session_path = match session {
        Some(p) => PathBuf::from(p),
        None => latest_session()?,
    };
    let baseline_path = baseline.map(PathBuf::from);
    let (analysis, loaded) = analysis::run_analysis(&session_path, baseline_path.as_deref())?;
    analysis::print_report(&analysis, &loaded);
    write_session_report(&session_path, baseline_path.as_deref())
}

/// The newest `monitor-*.json` in the working directory (by mtime).
fn latest_session() -> Result<PathBuf> {
    report::latest_matching("monitor-", ".json")
}

// ---------------------------------------------------------------------------
// experiment subcommand (M4)
// ---------------------------------------------------------------------------

fn cmd_experiment(args: &[String]) -> Result<()> {
    let mut session: Option<String> = None;
    let mut baseline: Option<String> = None;
    let mut outcome: Option<String> = None;
    let mut id: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                i += 1;
                session = args.get(i).cloned();
            }
            "--baseline" => {
                i += 1;
                baseline = args.get(i).cloned();
            }
            "--outcome" => {
                i += 1;
                outcome = args.get(i).cloned();
            }
            a if a.starts_with('-') => anyhow::bail!("unknown experiment argument: {a}"),
            a if id.is_none() => id = Some(a.to_string()),
            a => anyhow::bail!("unexpected experiment argument: {a}"),
        }
        i += 1;
    }
    let tty = std::io::stdin().is_terminal();
    let id = match id {
        Some(id) => id,
        None if tty => {
            let items: Vec<String> = experiments::EXPERIMENTS
                .iter()
                .map(|e| format!("{}: {}", e.id, e.name))
                .collect();
            let pick =
                inquire::Select::new("Which isolation experiment did you run?", items).prompt()?;
            pick.split_once(':')
                .map(|(l, _)| l.trim().to_string())
                .unwrap_or_default()
        }
        None => {
            anyhow::bail!(
                "usage: nvidia-diagnostic experiment <X1..X8> [--session PATH] [--baseline PATH] \
                 [--outcome fixed|still|reproduced|not-reproduced]"
            )
        }
    };
    let exp = experiments::by_id(&id)
        .ok_or_else(|| anyhow::anyhow!("unknown experiment: {id} (expected X1..X8)"))?;

    // --- Show the definition (PLAN §5 Stage 4) ---
    println!();
    println!("{} {}", exp.id, exp.name.bold());
    println!("{}", exp.description.dimmed());
    println!(
        "{} {}",
        "targets:".dimmed(),
        exp.targets
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "{} {}",
        "preconditions:".dimmed(),
        exp.preconditions.dimmed()
    );
    println!("{}", "steps:".dimmed());
    for (n, step) in exp.steps.iter().enumerate() {
        println!("  {}. {step}", n + 1);
    }

    // --- Confirm the user performed the physical change ---
    if tty {
        let done =
            inquire::Confirm::new("Did you perform the change (and reboot where a step says)?")
                .with_default(true)
                .prompt()?;
        if !done {
            println!(
                "{}",
                "perform the steps above, then re-run this command".dimmed()
            );
            return Ok(());
        }
    }

    // --- Retest under the monitor ---
    let session_path = match session {
        Some(p) => PathBuf::from(p),
        None => {
            let retest = if tty {
                inquire::Confirm::new(
                    "Start the monitor now to retest? (run your workload; m on each black screen, q to stop)",
                )
                .with_default(true)
                .prompt()?
            } else {
                true
            };
            if retest {
                monitor::run_monitor(monitor::MonitorOptions {
                    duration_secs: None,
                    jsonl: None,
                    workload: None,
                    interval_ms: 100,
                })?
            } else {
                latest_session()?
            }
        }
    };

    // --- Outcome (the post-check that folds into the scores) ---
    let outcome = match outcome {
        Some(s) => parse_outcome(&s, exp)?,
        None if tty => {
            let choices = experiments::outcome_choices(exp.id);
            let labels: Vec<String> = choices
                .iter()
                .map(|o| experiments::outcome_label(o).to_string())
                .collect();
            let pick = inquire::Select::new("After the retest, what happened?", labels.clone())
                .prompt()?;
            let idx = labels.iter().position(|l| l == &pick).unwrap_or(0);
            choices[idx]
        }
        None => anyhow::bail!("--outcome is required without a TTY"),
    };

    // --- Inherit prior experiment runs from the latest report, so the
    // scores accumulate across experiments. A re-run of the same
    // experiment replaces the earlier record. ---
    let mut runs = report::find_latest_report()
        .ok()
        .and_then(|p| report::load_report(&p).ok())
        .map(|r| r.experiments_run)
        .unwrap_or_default();
    runs.retain(|r| r.id != exp.id);
    runs.push(model::ExperimentRun {
        id: exp.id.to_string(),
        ts: now(),
        outcome,
        note: None,
    });

    let baseline_path = baseline.map(PathBuf::from);
    let rep = report::build_report(&session_path, baseline_path.as_deref(), &runs)?;
    let path = report::write_report(&rep)?;
    report::print_report(&rep);
    println!();
    println!("{} {}", "report written:".bold(), path.display().bold());
    Ok(())
}

fn parse_outcome(s: &str, exp: &experiments::Experiment) -> Result<model::ExperimentOutcome> {
    let normalized = s.trim().to_ascii_lowercase();
    let outcome = match normalized.as_str() {
        "fixed" => model::ExperimentOutcome::Fixed,
        "still" | "still-reproducing" | "reproducing" => model::ExperimentOutcome::StillReproducing,
        "reproduced" | "reproduced-in-second-system" => {
            model::ExperimentOutcome::ReproducedInSecondSystem
        }
        "not-reproduced" | "not-reproduced-in-second-system" => {
            model::ExperimentOutcome::NotReproducedInSecondSystem
        }
        other => {
            anyhow::bail!(
                "unknown outcome: {other} (expected fixed, still, reproduced, not-reproduced)"
            )
        }
    };
    if !experiments::outcome_choices(exp.id).contains(&outcome) {
        anyhow::bail!(
            "{outcome:?} is not a valid outcome for {}; valid: {}",
            exp.id,
            experiments::outcome_choices(exp.id)
                .iter()
                .map(|o| experiments::outcome_label(o))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// report subcommand (M4)
// ---------------------------------------------------------------------------

fn cmd_report(args: &[String]) -> Result<()> {
    let path = match args.first() {
        Some(p) => PathBuf::from(p),
        None => report::find_latest_report()?,
    };
    let rep = report::load_report(&path)?;
    report::print_report(&rep);
    Ok(())
}

// ---------------------------------------------------------------------------
// reconstruct subcommand (M4)
// ---------------------------------------------------------------------------

fn cmd_reconstruct(args: &[String]) -> Result<()> {
    let jsonl = match args.first() {
        Some(p) => PathBuf::from(p),
        None => anyhow::bail!("usage: nvidia-diagnostic reconstruct <TELEMETRY-JSONL>"),
    };
    let sidecar = analysis::reconstruct_session(&jsonl)?;
    let session = analysis::load_session(&sidecar)?;
    println!(
        "{} {} ({} Xid(s), {} marker(s))",
        "sidecar written:".bold(),
        sidecar.display().bold(),
        session.xid_events.len(),
        session.markers.len(),
    );
    println!(
        "{} {}",
        "next:".dimmed(),
        format!("nvidia-diagnostic analyze {}", sidecar.display()).dimmed()
    );
    Ok(())
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
                opts.interval_ms = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(100);
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
    monitor::run_monitor(opts).map(|_| ())
}

/// Fixed-duration stress window under the monitor (PLAN §5 Stage 2, §7 M5):
/// a thin wrapper over `monitor` that requires a duration (drives the
/// `indicatif` progress bar) and typically a `--workload` stress command.
fn cmd_stress(args: &[String]) -> Result<()> {
    let mut duration: Option<u64> = None;
    let mut workload: Option<Vec<String>> = None;
    let mut jsonl: Option<PathBuf> = None;
    let mut interval_ms: u64 = 100;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workload" => {
                i += 1;
                let rest: Vec<String> = args[i..].to_vec();
                if rest.is_empty() {
                    anyhow::bail!("--workload needs a command");
                }
                workload = Some(rest);
                i = args.len();
            }
            "--jsonl" => {
                i += 1;
                jsonl = args.get(i).map(std::path::PathBuf::from);
            }
            "--interval" => {
                i += 1;
                interval_ms = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(100);
            }
            a if !a.starts_with('-') && duration.is_none() => {
                duration = a.parse().ok();
            }
            other => anyhow::bail!("unknown stress argument: {other}"),
        }
        i += 1;
    }
    let Some(duration) = duration else {
        anyhow::bail!(
            "usage: nvidia-diagnostic stress <DURATION-SECS> [--workload CMD [ARGS...]] \
             [--jsonl PATH] [--interval MS]"
        );
    };
    monitor::run_monitor(monitor::MonitorOptions {
        duration_secs: Some(duration),
        jsonl,
        workload,
        interval_ms,
    })
    .map(|_| ())
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
        failure_start: (!failure_start.trim().is_empty()).then(|| failure_start.trim().to_string()),
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

fn assemble_baseline(results: &[(String, ProbeResult)], timeline: TimelineAnswers) -> Baseline {
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
            Some((_, r)) if r.is_ok() => "ok".green().to_string(),
            Some((_, _)) => "degraded".yellow().to_string(),
            None => "missing".red().to_string(),
        }
    };
    // Section header; the status badge and the one-line detail (the probe
    // summary on ok, the reason on degraded) come from the named probe
    // (e.g. the "driver" section is sourced by the "gpu" probe). The
    // "driver" header skips the detail to avoid duplicating the gpu line.
    let header = |section: &str, probe: &str, detail: bool| {
        let base = format!("{} [{}]", section.to_uppercase().bold(), status(probe));
        if detail && let Some((_, r)) = results.iter().find(|(n, _)| n == probe) {
            let d = if r.is_ok() {
                r.summary().dimmed().to_string()
            } else {
                r.summary().yellow().to_string()
            };
            format!("{base}  {d}")
        } else {
            base
        }
    };
    let opt = |v: Option<&str>| v.map(str::to_owned).unwrap_or_else(|| "—".to_string());
    let opt_color = |v: Option<&str>| match v {
        Some(x) => x.to_string(),
        None => "—".yellow().to_string(),
    };

    println!();
    println!(
        "{} {}",
        "nvidia-diagnostic — baseline snapshot".bold(),
        format_timestamp(&b.ts).dimmed()
    );

    // SYSTEM
    println!("\n{}", header("system", "system", true));
    println!(
        "{}{}",
        label("kernel"),
        b.system.kernel.as_deref().unwrap_or("—")
    );
    println!(
        "{}{}",
        label("arch"),
        b.system.arch.as_deref().unwrap_or("—")
    );
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
    println!("\n{}", header("bios", "bios", true));
    if b.bios.version.is_some() {
        println!("{}{}", label("version"), opt(b.bios.version.as_deref()));
        println!("{}{}", label("date"), opt(b.bios.date.as_deref()));
        println!("{}{}", label("vendor"), opt(b.bios.vendor.as_deref()));
    }

    // DRIVER
    println!("\n{}", header("driver", "gpu", false));
    println!(
        "{}{}",
        label("driver version"),
        opt_color(b.driver.driver_version.as_deref())
    );
    println!(
        "{}{}",
        label("vbios"),
        opt(b.driver.vbios_version.as_deref())
    );
    println!(
        "{}{}",
        label("module version"),
        opt_color(b.driver.module_version.as_deref())
    );

    // GPU
    println!("\n{}", header("gpu", "gpu", true));
    println!("{}{}", label("name"), opt(b.gpu.name.as_deref()).bold());
    println!("{}{}", label("serial"), opt(b.gpu.serial.as_deref()));
    println!("{}{}", label("vbios"), opt(b.gpu.vbios.as_deref()));
    println!("{}{}", label("bdf"), opt(b.gpu.bdf.as_deref()));
    let temp = b.gpu.temp_gpu.map(|t| t.to_string()).unwrap_or_default();
    let max = b
        .gpu
        .max_op_temp_c
        .map(|t| t.to_string())
        .unwrap_or_default();
    println!(
        "{}{} °C (max op {} °C)",
        label("temp"),
        if temp.is_empty() {
            "—".yellow().to_string()
        } else {
            temp
        },
        if max.is_empty() { "?".to_string() } else { max }
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
    println!("\n{}", header("pcie", "pcie", true));
    let (cap, sta) = (b.pcie.lnk_cap, b.pcie.lnk_sta);
    // Link state unavailable: the header already shows the reason.
    if let (Some((cg, cw)), Some((sg, sw))) = (cap, sta) {
        let degraded = sg < cg || sw < cw;
        let link = if degraded {
            format!("Gen{sg} x{sw} — BELOW capability Gen{cg} x{cw}")
                .red()
                .to_string()
        } else {
            format!("Gen{sg} x{sw} (cap Gen{cg} x{cw})")
                .green()
                .to_string()
        };
        println!("{}{}", label("link"), link);
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
    println!("\n{}", header("xid history", "logs", true));
    if b.xid_history.is_empty() {
        println!("{}no Xid events in kernel history", label("(none)"));
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
        "next: run `nvidia-diagnostic monitor` under the real workload, then `nvidia-diagnostic analyze` (PLAN §11)".dimmed()
    );
}

fn format_timestamp(ts: &DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}
