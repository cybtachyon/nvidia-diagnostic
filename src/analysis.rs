//! M3 analysis (PLAN §5 Stage 1/3, §7 M3): load a monitor session (the
//! `monitor-*.json` sidecar plus its JSONL telemetry), build the 10 s
//! pre-window correlation for each failure marker, and generate the §6
//! evidence items that `score.rs` turns into a verdict.

use crate::model::{
    Analysis, Baseline, Evidence, FailureCorrelation, FailureMarker, TelemetrySample, XidEvent,
};
use crate::score::{print_verdict, weights_for};
use chrono::{DateTime, Datelike, Duration, NaiveDateTime, Utc};
use owo_colors::OwoColorize;
use std::path::Path;

/// 10 s pre-window before each failure marker (PLAN §5 Stage 1).
pub const PRE_WINDOW_SECS: i64 = 10;
/// Xids within ±2 s of a marker count as "at the failure" (PLAN §5 Stage 1).
pub const XID_NEAR_SECS: i64 = 2;
/// Peak draw at/over this fraction of the limit counts as "at limit"
/// (PLAN §6 "power at/over limit in pre-window").
const POWER_AT_LIMIT_FRAC: f64 = 0.99;
/// Fan speed at/over this percent counts as "pinned" (PLAN §6 cooling row).
const FAN_PINNED_PCT: u32 = 90;
/// "Well below trip" margin in °C (PLAN §6 cooling row).
const TRIP_MARGIN_C: u32 = 10;

/// A loaded monitor session (the `monitor-*.json` sidecar).
#[derive(serde::Deserialize)]
pub struct Session {
    pub started: DateTime<Utc>,
    pub ended: DateTime<Utc>,
    pub jsonl: String,
    pub xid_events: Vec<XidEvent>,
    pub markers: Vec<FailureMarker>,
}

/// Correlations plus the session Xids that no marker could claim.
pub struct Correlated {
    pub correlations: Vec<FailureCorrelation>,
    pub xids_unattributed: Vec<u32>,
}

/// Load one monitor session sidecar.
pub fn load_session(path: &Path) -> anyhow::Result<Session> {
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

/// Load the session's JSONL telemetry stream.
pub fn load_samples(path: &Path) -> anyhow::Result<Vec<TelemetrySample>> {
    let mut out = vec![];
    for line in std::fs::read_to_string(path)?.lines() {
        if !line.trim().is_empty() {
            out.push(serde_json::from_str(line)?);
        }
    }
    Ok(out)
}

/// Xid `ts` is a human timestamp from `dmesg -T` / `journalctl -k` (local
/// time, no timezone). Parse it as local wall-clock time and convert to UTC
/// with the machine's current offset. `dmesg -T` carries a year; the
/// `journalctl` line omits it, so the current year is assumed (a session
/// spanning New Year loses sub-year precision on those Xids).
pub fn parse_xid_ts(s: &str) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(s, "%a %b %d %H:%M:%S %Y")
        .ok()
        .or_else(|| {
            let year = Utc::now().year();
            NaiveDateTime::parse_from_str(&format!("{year} {s}"), "%Y %b %d %H:%M:%S").ok()
        })?;
    let local = chrono::Local::now();
    let offset = *local.offset();
    naive
        .and_local_timezone(offset)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Build the pre-window correlation for each failure marker.
///
/// Xid attribution: when Xid timestamps are parseable, a Xid is "nearby" a
/// marker within ±2 s. When none are parseable, a single-marker session
/// attributes all session Xids to that marker; multi-marker sessions leave
/// them unattributed (reported, not silently dropped).
pub fn correlate(session: &Session, samples: &[TelemetrySample]) -> Correlated {
    let mut markers: Vec<&FailureMarker> = session.markers.iter().collect();
    markers.sort_by_key(|a| a.ts);

    let parsed: Vec<Option<DateTime<Utc>>> = session
        .xid_events
        .iter()
        .map(|x| x.ts.as_deref().and_then(parse_xid_ts))
        .collect();
    let any_parsed = parsed.iter().any(Option::is_some);

    let mut correlations = vec![];
    let mut unattributed: Vec<u32> = vec![];
    for (i, m) in markers.iter().enumerate() {
        let t = m.ts;
        let start = t - Duration::seconds(PRE_WINDOW_SECS);
        let window: Vec<&TelemetrySample> = samples
            .iter()
            .filter(|s| s.ts >= start && s.ts <= t)
            .collect();

        let max_temp_gpu = window.iter().filter_map(|s| s.temp_gpu).max();
        let max_temp_mem = window.iter().filter_map(|s| s.temp_mem).max();
        // f64 is not `Ord`; total_cmp keeps the comparison total.
        let max_power_draw = window
            .iter()
            .filter_map(|s| s.power_draw)
            .max_by(|a, b| a.total_cmp(b));
        let power_limit = window.iter().find_map(|s| s.power_limit);
        let power_at_limit = max_power_draw
            .zip(power_limit)
            .is_some_and(|(draw, limit)| draw >= POWER_AT_LIMIT_FRAC * limit);
        let min_clock_graphics = window.iter().filter_map(|s| s.clock_graphics).min();
        let max_fan_pct = window.iter().filter_map(|s| s.fan_pct).max();
        let throttle_raws: Vec<u64> = window
            .iter()
            .filter_map(|s| s.throttle_reasons_raw)
            .filter(|&raw| raw != 0)
            .collect();
        let throttle_active = !throttle_raws.is_empty();
        let throttle_reasons_raw = throttle_raws.iter().max().copied();
        let link = window.iter().rev().find_map(|s| s.pcie_link);

        let xids_nearby: Vec<u32> = if any_parsed {
            session
                .xid_events
                .iter()
                .zip(parsed.iter())
                .filter_map(|(x, p)| {
                    p.filter(|p| (*p - t).num_seconds().abs() <= XID_NEAR_SECS)
                        .map(|_| x.id)
                })
                .collect()
        } else if markers.len() == 1 {
            session.xid_events.iter().map(|x| x.id).collect()
        } else {
            if i == 0 {
                unattributed = session.xid_events.iter().map(|x| x.id).collect();
            }
            vec![]
        };
        let mut xids_nearby = xids_nearby;
        xids_nearby.sort_unstable();
        xids_nearby.dedup();

        let note = if xids_nearby.is_empty() && !throttle_active && !power_at_limit {
            Some(
                "no Xid within ±2 s and clean pre-window → display/EDID path (lower priority)"
                    .to_string(),
            )
        } else {
            None
        };

        correlations.push(FailureCorrelation {
            marker: (*m).clone(),
            window_start: start,
            window_end: t,
            sample_count: window.len(),
            max_temp_gpu,
            max_temp_mem,
            max_power_draw,
            power_limit,
            power_at_limit,
            min_clock_graphics,
            max_fan_pct,
            throttle_active,
            throttle_reasons_raw,
            link,
            xids_nearby,
            note,
        });
    }
    Correlated {
        correlations,
        xids_unattributed: unattributed,
    }
}

/// Build the §6 evidence items: per-marker dynamic evidence plus static
/// baseline evidence (link, AER, user timeline).
/// The `session` parameter is kept for API stability (M4 folds experiment
/// results into the same evidence set); the correlation already carries the
/// per-marker Xid attribution.
pub fn build_evidence(
    _session: &Session,
    correlations: &[FailureCorrelation],
    baseline: Option<&Baseline>,
) -> Vec<Evidence> {
    let ev = |id: &str, description: String, source: &str| Evidence {
        id: id.to_string(),
        description,
        weight_per_suspect: weights_for(id),
        source: source.to_string(),
    };
    let mut out: Vec<Evidence> = vec![];

    // --- Per-marker dynamic evidence (PLAN §6, one item per marker per class) ---
    // 90 °C is the 5090 max-op temp default when the baseline lacks it.
    let trip: Option<u32> = baseline.and_then(|b| b.gpu.max_op_temp_c).or(Some(90));
    for (i, c) in correlations.iter().enumerate() {
        let at = format!(
            "marker {}/{} at {}",
            i + 1,
            correlations.len(),
            c.marker.ts.format("%H:%M:%S")
        );
        let has = |ids: &[u32]| c.xids_nearby.iter().any(|x| ids.contains(x));
        // Xid 154 (recovery action → OS Reboot) always follows a fatal
        // Xid; it is scored with the bus fall-off class (2026-10-12 crash).
        if has(&[79, 154]) {
            out.push(ev(
                "xid_79_at_failure",
                format!("Xid 79/154 (GPU fallen off the bus, OS reboot) at {at}"),
                "monitor session",
            ));
        }
        if has(&[43, 45, 68]) {
            out.push(ev(
                "xid_engine_at_failure",
                format!("Xid 43/45/68 (graphics/video engine exception) at {at}"),
                "monitor session",
            ));
        }
        if has(&[48, 63, 64, 95]) {
            out.push(ev(
                "xid_ecc_at_failure",
                format!("Xid 48/63/64/95 (ECC / row remapping) at {at}"),
                "monitor session",
            ));
        }
        if has(&[66, 92]) {
            out.push(ev(
                "xid_thermal_at_failure",
                format!("Xid 66/92 (thermal trip) at {at}"),
                "monitor session",
            ));
        }
        if c.throttle_active {
            out.push(ev(
                "throttle_in_prewindow",
                format!(
                    "throttle reason active in pre-window at {at} (bits 0x{:x})",
                    c.throttle_reasons_raw.unwrap_or(0)
                ),
                "monitor session",
            ));
        }
        if c.power_at_limit {
            out.push(ev(
                "power_at_limit_in_prewindow",
                format!(
                    "power draw {} W at/over limit {} W in pre-window at {at}",
                    c.max_power_draw
                        .map(|p| format!("{p:.1}"))
                        .unwrap_or_default(),
                    c.power_limit.map(|p| format!("{p:.0}")).unwrap_or_default()
                ),
                "monitor session",
            ));
        }
        // PLAN §6 cooling row: temp well below trip with fans not pinned.
        let below_trip = c
            .max_temp_gpu
            .zip(trip)
            .is_some_and(|(t, trip)| t + TRIP_MARGIN_C < trip);
        let fans_not_pinned = c.max_fan_pct.is_some_and(|f| f < FAN_PINNED_PCT);
        if below_trip && fans_not_pinned {
            out.push(ev(
                "temp_below_trip_fans_not_pinned",
                format!(
                    "temp {} °C well below trip {} °C, fans not pinned pre-event at {at}",
                    c.max_temp_gpu.unwrap_or(0),
                    trip.unwrap_or(90),
                ),
                "monitor session",
            ));
        }
    }

    // --- Static baseline evidence (PLAN §6 rows 7–10) ---
    if let Some(b) = baseline {
        if let (Some((cg, cw)), Some((sg, sw))) = (b.pcie.lnk_cap, b.pcie.lnk_sta)
            && (sg < cg || sw < cw)
        {
            out.push(ev(
                "lnksta_below_lnkcap",
                format!("PCIe link running Gen{sg} x{sw} below capability Gen{cg} x{cw}"),
                "baseline",
            ));
        }
        if b.pcie.aer_ue_errors == Some(true) {
            out.push(ev(
                "aer_uncorrectable",
                "AER uncorrectable error bits latched".to_string(),
                "baseline",
            ));
        }
        let changes = &b.user_timeline.changes_around;
        let lower: Vec<String> = changes.iter().map(|c| c.to_lowercase()).collect();
        if lower.iter().any(|c| c.contains("driver")) {
            out.push(ev(
                "onset_after_driver_update",
                "failure onset postdates a GPU driver update (user timeline)".to_string(),
                "baseline",
            ));
        }
        if lower.iter().any(|c| c.contains("bios")) {
            out.push(ev(
                "onset_after_bios_update",
                "failure onset postdates a BIOS update (user timeline)".to_string(),
                "baseline",
            ));
        }
    }
    out
}

/// Run the full M3 analysis: load session + samples + optional baseline,
/// correlate, build evidence, score into a verdict. Returns the analysis
/// plus the loaded session (the report prints its start/end times).
pub fn run_analysis(
    session_path: &Path,
    baseline_path: Option<&Path>,
) -> anyhow::Result<(Analysis, Session)> {
    let session = load_session(session_path)?;
    let samples = load_samples(Path::new(&session.jsonl))?;
    let baseline: Option<Baseline> = baseline_path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let correlated = correlate(&session, &samples);
    let evidence = build_evidence(&session, &correlated.correlations, baseline.as_ref());
    let verdict = crate::score::score_evidence(&evidence);
    let analysis = Analysis {
        session: session_path.display().to_string(),
        baseline: baseline_path.map(|p| p.display().to_string()),
        correlations: correlated.correlations,
        xids_unattributed: correlated.xids_unattributed,
        evidence,
        verdict,
    };
    Ok((analysis, session))
}

/// Terminal report: session header, per-marker correlations, evidence, verdict.
pub fn print_report(a: &Analysis, session: &Session) {
    println!();
    println!(
        "{} {} ({} → {} UTC)",
        "session analysis".bold(),
        a.session.bold(),
        session
            .started
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
            .dimmed(),
        session.ended.format("%H:%M:%S").to_string().dimmed()
    );
    if a.baseline.is_some() {
        println!(
            "{} {}",
            "baseline".dimmed(),
            a.baseline.as_deref().unwrap_or("").dimmed()
        );
    } else {
        println!(
            "{}",
            "note: no baseline provided — static evidence (link, AER, timeline) skipped".dimmed()
        );
    }
    if a.correlations.is_empty() {
        println!(
            "{}",
            "no failure markers in this session — nothing to correlate".dimmed()
        );
    }
    for (i, c) in a.correlations.iter().enumerate() {
        let kind = match c.marker.kind {
            crate::model::MarkerKind::UserMark => "user mark",
            crate::model::MarkerKind::DetectedReset => "detected reset",
        };
        println!();
        println!(
            "{} {} ({}) — pre-window {} s, {} samples",
            format!("marker {}/{}", i + 1, a.correlations.len()).bold(),
            c.marker.ts.format("%H:%M:%S").to_string().bold(),
            kind,
            PRE_WINDOW_SECS,
            c.sample_count
        );
        if c.sample_count == 0 {
            println!("  {} (no telemetry in window)", "no samples".yellow());
            continue;
        }
        let mut line = String::new();
        if let Some(t) = c.max_temp_gpu {
            line.push_str(&format!("temp {t}°C peak · "));
        }
        if let Some(p) = c.max_power_draw {
            line.push_str(&format!("power {p:.1} W peak · "));
        }
        if c.power_at_limit
            && let Some(l) = c.power_limit
        {
            line.push_str(&format!("AT LIMIT ({l:.0} W) · "));
        }
        if let Some(f) = c.max_fan_pct {
            line.push_str(&format!("fan {f}% · "));
        }
        if let Some((link_gen, link_width)) = c.link {
            line.push_str(&format!("link Gen{link_gen}x{link_width} · "));
        }
        if c.throttle_active {
            line.push_str(&format!(
                "throttle 0x{:x} · ",
                c.throttle_reasons_raw.unwrap_or(0)
            ));
        }
        let line = line.trim_end_matches(" · ");
        println!(
            "  {}",
            if line.is_empty() {
                "window quiet".to_string().dimmed().to_string()
            } else {
                line.to_string()
            }
        );
        if !c.xids_nearby.is_empty() {
            println!(
                "  {} {}",
                "xids nearby:".dimmed(),
                format!("{:?}", c.xids_nearby).red().bold()
            );
        }
        if let Some(n) = &c.note {
            println!("  {} {}", "note:".dimmed(), n.dimmed());
        }
    }
    if !a.xids_unattributed.is_empty() {
        println!(
            "{} {}",
            "xids in session, not time-attributable to a marker:".dimmed(),
            format!("{:?}", a.xids_unattributed).red()
        );
    }

    if !a.evidence.is_empty() {
        println!();
        println!("{}", "evidence".bold());
        for e in &a.evidence {
            let w: Vec<String> = e
                .weight_per_suspect
                .iter()
                .map(|(s, w)| format!("{} {:+}", s, w))
                .collect();
            println!(
                "  {:<32} {} {}",
                e.id,
                e.description.dimmed(),
                w.join(", ").dimmed()
            );
        }
    }

    print_verdict(&a.verdict);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MarkerKind;
    use chrono::{TimeZone, Utc};

    fn sample(
        ts: DateTime<Utc>,
        power: f64,
        limit: f64,
        temp: u32,
        fan: u32,
        throttle: u64,
    ) -> TelemetrySample {
        TelemetrySample {
            ts,
            power_draw: Some(power),
            power_limit: Some(limit),
            temp_gpu: Some(temp),
            fan_pct: Some(fan),
            throttle_reasons_raw: Some(throttle),
            ..Default::default()
        }
    }

    /// Regression: the 2026-10-12 hard-reset crash (Xid 79 + 154, captured
    /// from `journalctl -k -b -1`) must rank the 12VHPWR/PCIe connector
    /// first (PLAN §2: Xid 79 = GPU fallen off the bus).
    #[test]
    fn crash_session_ranks_pcie_connector_first() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/crash-20261012.session.json");
        let (a, _) = run_analysis(&fixture, None).expect("crash fixture must analyze");
        let top = &a.verdict.ranked[0];
        assert_eq!(top.suspect, crate::model::Suspect::PcieConnector);
        assert!(top.score > 0);
        assert!(top.evidence.iter().any(|id| id == "xid_79_at_failure"));
    }

    /// Round-trip a UTC instant through the local-offset path that
    /// `parse_xid_ts` uses, as `dmesg -T` would render it.
    fn dmesg_ts(dt: DateTime<Utc>) -> String {
        let local = dt.with_timezone(&chrono::Local);
        local
            .naive_local()
            .format("%a %b %d %H:%M:%S %Y")
            .to_string()
    }

    #[test]
    fn parse_xid_ts_round_trips_dmesg_and_journal_formats() {
        let dt = Utc.with_ymd_and_hms(2026, 10, 12, 1, 23, 45).unwrap();
        assert_eq!(parse_xid_ts(&dmesg_ts(dt)), Some(dt));
        let journal = dt
            .with_timezone(&chrono::Local)
            .naive_local()
            .format("%b %d %H:%M:%S")
            .to_string();
        assert_eq!(parse_xid_ts(&journal), Some(dt));
        assert_eq!(parse_xid_ts("not a timestamp"), None);
    }

    #[test]
    fn parse_xid_ts_handles_the_real_journal_line() {
        // The 2026-10-12 crash line, as `journalctl -k` renders it
        // (year-less; the current year is assumed). The invariant is the
        // local wall-clock rendering, not the UTC instant.
        let s = "Oct 12 07:08:38";
        let parsed = parse_xid_ts(s).expect("journal ts must parse");
        let local = parsed.with_timezone(&chrono::Local);
        assert_eq!(local.format("%H:%M:%S").to_string(), "07:08:38");
        assert_eq!(local.format("%b %d").to_string(), "Oct 12");
    }

    #[test]
    fn correlate_extracts_prewindow_and_xids() {
        let t = Utc::now();
        let samples = vec![
            sample(t - Duration::seconds(11), 100.0, 575.0, 50, 30, 0), // outside window
            sample(t - Duration::seconds(5), 574.9, 575.0, 69, 85, 0x400),
            sample(t - Duration::seconds(1), 575.0, 575.0, 71, 90, 0),
            sample(t + Duration::seconds(1), 575.0, 575.0, 71, 90, 0), // after marker
        ];
        let session = Session {
            started: t - Duration::seconds(60),
            ended: t + Duration::seconds(60),
            jsonl: String::new(),
            xid_events: vec![
                XidEvent {
                    ts: Some(dmesg_ts(t - Duration::seconds(1))),
                    bdf: None,
                    id: 79,
                    raw: String::new(),
                },
                XidEvent {
                    ts: Some(dmesg_ts(t - Duration::seconds(5))),
                    bdf: None,
                    id: 43,
                    raw: String::new(),
                },
                XidEvent {
                    ts: Some(dmesg_ts(t + Duration::seconds(50))),
                    bdf: None,
                    id: 48,
                    raw: String::new(),
                },
            ],
            markers: vec![FailureMarker {
                ts: t,
                kind: MarkerKind::UserMark,
                note: None,
            }],
        };
        let c = correlate(&session, &samples);
        let cr = &c.correlations[0];
        assert_eq!(cr.sample_count, 2);
        assert_eq!(cr.max_temp_gpu, Some(71));
        assert_eq!(cr.max_power_draw, Some(575.0));
        assert!(cr.power_at_limit);
        assert!(cr.throttle_active);
        assert_eq!(cr.throttle_reasons_raw, Some(0x400));
        // 79 is within ±2 s; 43 is 5 s away; 48 is after the marker.
        assert_eq!(cr.xids_nearby, vec![79]);
        assert!(cr.note.is_none()); // dirty window: throttle + power at limit
    }

    #[test]
    fn clean_window_gets_edid_note_and_no_xids() {
        let t = Utc::now();
        let samples = vec![sample(t - Duration::seconds(5), 100.0, 575.0, 45, 30, 0)];
        let session = Session {
            started: t - Duration::seconds(60),
            ended: t + Duration::seconds(60),
            jsonl: String::new(),
            xid_events: vec![],
            markers: vec![FailureMarker {
                ts: t,
                kind: MarkerKind::UserMark,
                note: None,
            }],
        };
        let c = correlate(&session, &samples);
        assert!(c.correlations[0].xids_nearby.is_empty());
        assert!(
            c.correlations[0]
                .note
                .as_deref()
                .is_some_and(|n| n.contains("display/EDID"))
        );
    }

    #[test]
    fn unparseable_ts_attributes_all_xids_to_single_marker() {
        let t = Utc::now();
        let session = Session {
            started: t,
            ended: t,
            jsonl: String::new(),
            xid_events: vec![
                XidEvent {
                    ts: None,
                    bdf: None,
                    id: 79,
                    raw: String::new(),
                },
                XidEvent {
                    ts: None,
                    bdf: None,
                    id: 79,
                    raw: String::new(),
                },
            ],
            markers: vec![FailureMarker {
                ts: t,
                kind: MarkerKind::DetectedReset,
                note: None,
            }],
        };
        let c = correlate(&session, &[]);
        assert_eq!(c.correlations[0].xids_nearby, vec![79]); // deduped
        assert!(c.xids_unattributed.is_empty());
    }

    #[test]
    fn unparseable_ts_with_multiple_markers_is_unattributed() {
        let t = Utc::now();
        let marker = |dt: DateTime<Utc>| FailureMarker {
            ts: dt,
            kind: MarkerKind::UserMark,
            note: None,
        };
        let session = Session {
            started: t,
            ended: t,
            jsonl: String::new(),
            xid_events: vec![XidEvent {
                ts: None,
                bdf: None,
                id: 43,
                raw: String::new(),
            }],
            markers: vec![marker(t), marker(t + Duration::seconds(30))],
        };
        let c = correlate(&session, &[]);
        assert!(c.correlations.iter().all(|cr| cr.xids_nearby.is_empty()));
        assert_eq!(c.xids_unattributed, vec![43]);
    }

    #[test]
    fn build_evidence_covers_dynamic_and_static_rows() {
        let t = Utc::now();
        let samples = vec![sample(t - Duration::seconds(5), 575.0, 575.0, 70, 90, 0x8)];
        let session = Session {
            started: t,
            ended: t,
            jsonl: String::new(),
            xid_events: vec![XidEvent {
                ts: Some(dmesg_ts(t - Duration::seconds(1))),
                bdf: None,
                id: 79,
                raw: String::new(),
            }],
            markers: vec![FailureMarker {
                ts: t,
                kind: MarkerKind::UserMark,
                note: None,
            }],
        };
        let correlated = correlate(&session, &samples);
        let baseline = Baseline {
            gpu: crate::model::GpuInfo {
                max_op_temp_c: Some(90),
                ..Default::default()
            },
            pcie: crate::model::PcieInfo {
                lnk_cap: Some((5, 16)),
                lnk_sta: Some((4, 16)),
                aer_ue_errors: Some(true),
                ..Default::default()
            },
            user_timeline: crate::model::TimelineAnswers {
                changes_around: vec!["GPU driver update".into(), "BIOS update".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let ev = build_evidence(&session, &correlated.correlations, Some(&baseline));
        let ids: Vec<&str> = ev.iter().map(|e| e.id.as_str()).collect();
        for expected in [
            "xid_79_at_failure",
            "throttle_in_prewindow",
            "power_at_limit_in_prewindow",
            "lnksta_below_lnkcap",
            "aer_uncorrectable",
            "onset_after_driver_update",
            "onset_after_bios_update",
        ] {
            assert!(ids.contains(&expected), "missing {expected}: {ids:?}");
        }
        assert!(!ids.contains(&"temp_below_trip_fans_not_pinned")); // 70 °C is not well below 90
    }
}
