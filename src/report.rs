//! M4 report (PLAN §4.4, §7 M4): assemble the shareable `report-<ts>.json`
//! from a monitor session, an optional baseline, and recorded experiment
//! runs; write, load, and print it. The JSON is the unit of replication:
//! anyone can diff two reports (before/after a change, or two machines).

use crate::analysis::{self, Session};
use crate::experiments;
use crate::model::{
    Baseline, Evidence, ExperimentRun, MarkerKind, Report, Stats, TelemetrySample, TelemetrySummary,
};
use crate::model::{now, stamp};
use crate::score::{self, print_evidence, print_verdict, weights_for};
use owo_colors::OwoColorize;
use std::path::Path;

/// Assemble the report (PLAN §4.4). Experiment outcomes add §6 evidence on
/// top of the session evidence, and the verdict is re-scored (PLAN §5
/// Stage 4: "After each, fold results into scores").
pub fn build_report(
    session_path: &Path,
    baseline_path: Option<&Path>,
    experiments: &[ExperimentRun],
) -> anyhow::Result<Report> {
    let (analysis, session) = analysis::run_analysis(session_path, baseline_path)?;
    let baseline: Option<Baseline> = baseline_path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok());

    let mut evidence = analysis.evidence;
    for run in experiments {
        if let Some(id) = experiments::evidence_for(&run.id, &run.outcome) {
            evidence.push(Evidence {
                id: id.to_string(),
                description: format!(
                    "experiment {} ({}) → {}",
                    run.id,
                    run.ts.format("%Y-%m-%d %H:%M:%S"),
                    experiments::outcome_label(&run.outcome)
                ),
                weight_per_suspect: weights_for(id),
                source: "experiment".to_string(),
            });
        }
    }
    let verdict = score::score_evidence(&evidence);
    let samples = analysis::load_samples(Path::new(&session.jsonl))?;

    Ok(Report {
        ts: now(),
        session: session_path.display().to_string(),
        baseline: baseline.clone(),
        timeline: baseline.as_ref().map(|b| b.user_timeline.clone()),
        telemetry: summarize(&session, &samples),
        xid_events: session.xid_events,
        markers: session.markers,
        correlations: analysis.correlations,
        experiments_run: experiments.to_vec(),
        evidence,
        scores: verdict.ranked.clone(),
        verdict,
    })
}

/// Session-wide min/avg/max per field (PLAN §4.4 `telemetry_summary`).
fn summarize(session: &Session, samples: &[TelemetrySample]) -> TelemetrySummary {
    let stats = |pick: fn(&TelemetrySample) -> Option<f64>| {
        let values: Vec<f64> = samples.iter().filter_map(pick).collect();
        if values.is_empty() {
            return Stats::default();
        }
        let sum: f64 = values.iter().sum();
        Stats {
            min: values.iter().copied().reduce(f64::min),
            avg: Some(sum / values.len() as f64),
            max: values.iter().copied().reduce(f64::max),
        }
    };
    let duration_secs = (session.ended - session.started).num_seconds() as f64;
    TelemetrySummary {
        sample_count: samples.len() as u64,
        duration_secs,
        temp_gpu_c: stats(|s| s.temp_gpu.map(f64::from)),
        temp_hotspot_c: stats(|s| s.temp_hotspot.map(f64::from)),
        power_draw_w: stats(|s| s.power_draw),
        clock_graphics_mhz: stats(|s| s.clock_graphics.map(f64::from)),
        fan_pct: stats(|s| s.fan_pct.map(f64::from)),
        util_gpu_pct: stats(|s| s.util_gpu.map(f64::from)),
    }
}

/// Write the report as `report-<ts>.json` in the working directory.
pub fn write_report(report: &Report) -> anyhow::Result<std::path::PathBuf> {
    let path = std::path::PathBuf::from(format!("report-{}.json", stamp()));
    std::fs::write(&path, serde_json::to_string_pretty(report)?)?;
    Ok(path)
}

/// Load a report from disk.
pub fn load_report(path: &Path) -> anyhow::Result<Report> {
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

/// The newest file matching `prefix*suffix` in the working directory
/// (by mtime). Sessions (`monitor-*.json`) and reports (`report-*.json`)
/// both use this.
pub fn latest_matching(prefix: &str, suffix: &str) -> anyhow::Result<std::path::PathBuf> {
    let mut best: Option<(std::path::PathBuf, Option<std::time::SystemTime>)> = None;
    for entry in std::fs::read_dir(".")? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(prefix) && name.ends_with(suffix) {
            // `Option` ordering: a missing mtime sorts below any real one.
            let mtime = entry.metadata().and_then(|m| m.modified()).ok();
            if best.as_ref().is_none_or(|(_, m)| mtime > *m) {
                best = Some((path, mtime));
            }
        }
    }
    best.map(|(p, _)| p).ok_or_else(|| {
        anyhow::anyhow!(
            "no {prefix}*{suffix} found in the working directory; run the monitor first"
        )
    })
}

/// The newest `report-*.json` in the working directory.
pub fn find_latest_report() -> anyhow::Result<std::path::PathBuf> {
    latest_matching("report-", ".json")
}

/// Terminal summary of a report (PLAN §4.1 step 8, §5 Stage 5).
pub fn print_report(r: &Report) {
    println!();
    println!(
        "{} {} · generated {}",
        "report".bold(),
        r.session.bold(),
        r.ts.format("%Y-%m-%d %H:%M:%S UTC").to_string().dimmed()
    );
    if let Some(b) = &r.baseline {
        println!(
            "{} {}",
            "baseline".dimmed(),
            b.ts.format("%Y-%m-%d %H:%M:%S UTC").to_string().dimmed()
        );
    } else {
        println!(
            "{}",
            "note: no baseline attached (static evidence skipped)".dimmed()
        );
    }

    let t = &r.telemetry;
    println!();
    println!(
        "{} ({} samples, {} s)",
        "telemetry".bold(),
        t.sample_count,
        t.duration_secs
    );
    let stats_row = |label: &str, s: &Stats, unit: &str| {
        if s.min.is_none() {
            println!("  {:<16} {}", label, "no samples".dimmed());
            return;
        }
        let f = |v: Option<f64>| v.map(|v| format!("{v:.1}")).unwrap_or_else(|| "?".into());
        println!(
            "  {:<16} min {} · avg {} · max {} {}",
            label,
            f(s.min),
            f(s.avg),
            f(s.max),
            unit.dimmed()
        );
    };
    stats_row("temp_gpu", &t.temp_gpu_c, "°C");
    stats_row("temp_hotspot", &t.temp_hotspot_c, "°C");
    stats_row("power_draw", &t.power_draw_w, "W");
    stats_row("clock_graphics", &t.clock_graphics_mhz, "MHz");
    stats_row("fan", &t.fan_pct, "%");
    stats_row("util_gpu", &t.util_gpu_pct, "%");

    let xids: Vec<u32> = r.xid_events.iter().map(|x| x.id).collect();
    let user = r
        .markers
        .iter()
        .filter(|m| m.kind == MarkerKind::UserMark)
        .count();
    let detected = r
        .markers
        .iter()
        .filter(|m| m.kind == MarkerKind::DetectedReset)
        .count();
    println!();
    println!(
        "{} {}",
        "xids in session:".dimmed(),
        if xids.is_empty() {
            "none".to_string().dimmed().to_string()
        } else {
            format!("{xids:?}").red().bold().to_string()
        }
    );
    println!(
        "{} {} ({} user, {} detected)",
        "markers:".dimmed(),
        r.markers.len(),
        user,
        detected
    );
    for m in &r.markers {
        println!(
            "  {} {}",
            m.ts.format("%H:%M:%S").to_string().bold(),
            m.note.as_deref().unwrap_or("").dimmed()
        );
    }

    if !r.correlations.is_empty() {
        crate::analysis::print_correlations(&r.correlations);
    }

    if !r.experiments_run.is_empty() {
        println!();
        println!("{}", "experiments run".bold());
        for run in &r.experiments_run {
            println!(
                "  {} {} — {} ({})",
                run.id.bold(),
                experiments::by_id(&run.id)
                    .map(|e| e.name)
                    .unwrap_or("unknown"),
                experiments::outcome_label(&run.outcome),
                run.ts.format("%Y-%m-%d %H:%M:%S").to_string().dimmed()
            );
        }
    }

    print_evidence(&r.evidence);
    print_verdict(&r.verdict);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExperimentOutcome, Suspect};
    use chrono::Utc;

    fn fixture_session() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/crash-20261012.session.json")
    }

    fn empty_session() -> Session {
        let t = Utc::now();
        Session {
            started: t,
            ended: t,
            jsonl: String::new(),
            xid_events: vec![],
            markers: vec![],
        }
    }

    #[test]
    fn summarize_computes_min_avg_max() {
        let t = Utc::now();
        let mk = |temp: u32, power: f64| TelemetrySample {
            ts: t,
            temp_gpu: Some(temp),
            power_draw: Some(power),
            ..Default::default()
        };
        let samples = vec![mk(50, 100.0), mk(70, 300.0), mk(60, 200.0)];
        let s = summarize(&empty_session(), &samples);
        assert_eq!(s.sample_count, 3);
        assert_eq!(s.temp_gpu_c.min, Some(50.0));
        assert_eq!(s.temp_gpu_c.avg, Some(60.0));
        assert_eq!(s.temp_gpu_c.max, Some(70.0));
        assert_eq!(s.power_draw_w.avg, Some(200.0));
        assert!(s.fan_pct.min.is_none()); // no fan data in the samples
    }

    #[test]
    fn summarize_empty_session_is_all_none() {
        let s = summarize(&empty_session(), &[]);
        assert_eq!(s.sample_count, 0);
        assert!(s.temp_gpu_c.avg.is_none());
        assert!(s.power_draw_w.max.is_none());
    }

    #[test]
    fn build_report_from_crash_fixture() {
        let fixture = fixture_session();
        let report = build_report(&fixture, None, &[]).expect("crash fixture must build");
        assert_eq!(report.xid_events.len(), 2);
        assert_eq!(report.markers.len(), 1);
        assert!(
            report
                .correlations
                .iter()
                .any(|c| c.xids_nearby.contains(&79))
        );
        assert_eq!(report.verdict.ranked[0].suspect, Suspect::PcieConnector);
        assert!(report.evidence.iter().any(|e| e.id == "xid_79_at_failure"));
        // The JSON is the unit of replication: it must round-trip.
        let json = serde_json::to_string(&report).expect("report must serialize");
        let back: Report = serde_json::from_str(&json).expect("report must deserialize");
        assert_eq!(
            back.verdict.ranked[0].suspect,
            report.verdict.ranked[0].suspect
        );
        assert_eq!(back.xid_events.len(), report.xid_events.len());
    }

    #[test]
    fn experiment_outcome_extends_evidence_and_score() {
        let fixture = fixture_session();
        let base = build_report(&fixture, None, &[]).expect("base report must build");
        let run = ExperimentRun {
            id: "X3".into(),
            ts: Utc::now(),
            outcome: ExperimentOutcome::Fixed,
            note: None,
        };
        let report = build_report(&fixture, None, std::slice::from_ref(&run))
            .expect("report with experiment must build");
        let pcie = |r: &Report| {
            r.scores
                .iter()
                .find(|s| s.suspect == Suspect::PcieConnector)
                .unwrap()
                .score
        };
        assert!(
            report
                .evidence
                .iter()
                .any(|e| e.id == "fixed_after_x3_reseat")
        );
        assert_eq!(pcie(&report), pcie(&base) + 4);
        assert_eq!(report.experiments_run.len(), 1);
    }
}
