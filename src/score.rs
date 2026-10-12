//! M3 scoring (PLAN §6, §7 M3): the §6 weight table as data, evidence →
//! ranked verdict, and the colored verdict printout.
//!
//! Evidence ids are the single source of truth for the table: `analysis.rs`
//! builds `Evidence` items with these ids and `weights_for` fills their
//! `weight_per_suspect`. Tuning the model means editing `WEIGHTS` only.

use crate::model::{Evidence, RankedSuspect, Suspect, Verdict};
use owo_colors::OwoColorize;
use std::collections::BTreeMap;

/// All suspects, in declaration order (used for deterministic tie-breaks).
pub const SUSPECTS: [Suspect; 6] = [
    Suspect::Driver,
    Suspect::Bios,
    Suspect::PcieConnector,
    Suspect::Psu,
    Suspect::CardHardware,
    Suspect::Cooling,
];

/// §6 weight table: evidence id → signed weights per suspect.
const WEIGHTS: &[(&str, &[(Suspect, i8)])] = &[
    (
        "xid_79_at_failure",
        &[
            (Suspect::Driver, 1),
            (Suspect::Bios, 2),
            (Suspect::PcieConnector, 3),
            (Suspect::Psu, 2),
        ],
    ),
    (
        "xid_engine_at_failure",
        &[(Suspect::Driver, 3), (Suspect::CardHardware, 1)],
    ),
    ("xid_ecc_at_failure", &[(Suspect::CardHardware, 4)]),
    ("xid_thermal_at_failure", &[(Suspect::Cooling, 4)]),
    ("throttle_in_prewindow", &[(Suspect::Cooling, 3)]),
    (
        "power_at_limit_in_prewindow",
        &[(Suspect::PcieConnector, 1), (Suspect::Psu, 3)],
    ),
    (
        "lnksta_below_lnkcap",
        &[(Suspect::Bios, 2), (Suspect::PcieConnector, 2)],
    ),
    (
        "aer_uncorrectable",
        &[(Suspect::Bios, 1), (Suspect::PcieConnector, 2)],
    ),
    ("onset_after_driver_update", &[(Suspect::Driver, 2)]),
    ("onset_after_bios_update", &[(Suspect::Bios, 2)]),
    ("fixed_after_x1_driver_rollback", &[(Suspect::Driver, 4)]),
    ("fixed_after_x2_bios_rollback", &[(Suspect::Bios, 4)]),
    ("fixed_after_x3_reseat", &[(Suspect::PcieConnector, 4)]),
    ("fixed_after_x4_psu_swap", &[(Suspect::Psu, 4)]),
    // M4 additions for X5/X6/X8 (no §6 rows; weights match the targeted
    // hypotheses, slightly weaker than the direct X3/X4 reseat/swap rows).
    (
        "fixed_after_x5_slot_change",
        &[(Suspect::Bios, 2), (Suspect::PcieConnector, 2)],
    ),
    ("fixed_after_x6_rebar_toggle", &[(Suspect::Bios, 3)]),
    ("fixed_after_x8_fan_curve", &[(Suspect::Cooling, 3)]),
    ("reproduced_in_second_system", &[(Suspect::CardHardware, 3)]),
    (
        "not_reproduced_in_second_system",
        &[(Suspect::CardHardware, -2)],
    ),
    ("temp_below_trip_fans_not_pinned", &[(Suspect::Cooling, -2)]),
];

/// Signed weights per suspect for one evidence id; empty for unknown ids.
pub fn weights_for(id: &str) -> BTreeMap<Suspect, i8> {
    WEIGHTS
        .iter()
        .find(|(e, _)| *e == id)
        .map(|(_, w)| w.iter().copied().collect())
        .unwrap_or_default()
}

/// Sum evidence weights per suspect into a ranked verdict (PLAN §6).
/// Ties break on suspect declaration order; `next_experiment` is set only
/// when the top suspect has a positive score.
pub fn score_evidence(evidence: &[Evidence]) -> Verdict {
    let mut totals: BTreeMap<Suspect, i32> = BTreeMap::new();
    for e in evidence {
        for (s, w) in &e.weight_per_suspect {
            *totals.entry(*s).or_insert(0) += i32::from(*w);
        }
    }
    let mut ranked: Vec<RankedSuspect> = SUSPECTS
        .iter()
        .map(|s| RankedSuspect {
            suspect: *s,
            score: totals.get(s).copied().unwrap_or(0),
            evidence: evidence
                .iter()
                .filter(|e| e.weight_per_suspect.get(s).copied().unwrap_or(0) != 0)
                .map(|e| e.id.clone())
                .collect(),
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.suspect.cmp(&b.suspect))
    });
    let next_experiment = ranked
        .first()
        .filter(|r| r.score > 0)
        .map(|r| next_experiment_for(&r.suspect).to_string());
    Verdict {
        ranked,
        next_experiment,
    }
}

/// The cheapest discriminating experiment per suspect (PLAN §5.5).
pub fn next_experiment_for(s: &Suspect) -> &'static str {
    match s {
        Suspect::Driver => "X1 — driver rollback to the last known-good version, reboot, retest",
        Suspect::Bios => "X2 — BIOS rollback to the previous version, retest",
        Suspect::PcieConnector => "X3 — power off, reseat 3× 12VHPWR + PCIe card, retest",
        Suspect::Psu => "X4 — swap the PSU for a known-good unit, retest",
        Suspect::CardHardware => "X7 — card into a known-good second system, run stress",
        Suspect::Cooling => {
            "X8 — fan curve / airflow check (nvidia-smi --fan-control, best-effort)"
        }
    }
}

/// Colored evidence list (reused by the M4 report printout).
pub fn print_evidence(evidence: &[Evidence]) {
    if evidence.is_empty() {
        return;
    }
    println!();
    println!("{}", "evidence".bold());
    for e in evidence {
        let w: Vec<String> = e
            .weight_per_suspect
            .iter()
            .map(|(s, w)| format!("{s} {w:+}"))
            .collect();
        println!(
            "  {:<32} {} {}",
            e.id,
            e.description.dimmed(),
            w.join(", ").dimmed()
        );
    }
}

/// Colored verdict printout (PLAN §4.1 step 6, §5 Stage 5).
pub fn print_verdict(v: &Verdict) {
    println!();
    println!("{}", "verdict".bold());
    for (i, r) in v.ranked.iter().enumerate() {
        let name = r.suspect.to_string();
        let name = if i == 0 && r.score > 0 {
            name.red().bold().to_string()
        } else {
            name.to_string()
        };
        let score = format!("{:+}", r.score);
        let score = if r.score > 0 {
            score.red().bold().to_string()
        } else if r.score < 0 {
            score.green().to_string()
        } else {
            score.dimmed().to_string()
        };
        println!(
            "  {}. {name:<28} {score:>5}  {}",
            i + 1,
            r.evidence.join(", ").dimmed()
        );
    }

    let top = v.ranked.first();
    match top {
        Some(t) if t.score > 0 => {
            let runner_up = v.ranked.get(1).map(|r| r.score).unwrap_or(0);
            let gap = t.score - runner_up;
            if gap < 3 {
                println!();
                println!(
                    "{}",
                    format!(
                        "confidence: low — top-vs-runner-up gap {gap} (< 3); run the discriminating experiment: {}",
                        v.next_experiment.as_deref().unwrap_or("none")
                    )
                    .yellow()
                );
            } else {
                println!();
                println!(
                    "{}",
                    format!(
                        "confidence: high (gap {gap}); suggested next step: {}",
                        v.next_experiment.as_deref().unwrap_or("none")
                    )
                    .green()
                );
            }
        }
        _ => println!(
            "{}",
            "no incriminating evidence in this session (clean run; see PLAN §8 replication check)"
                .dimmed()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str) -> Evidence {
        Evidence {
            id: id.into(),
            description: id.into(),
            weight_per_suspect: weights_for(id),
            source: "test".into(),
        }
    }

    #[test]
    fn weights_for_known_and_unknown() {
        let w = weights_for("xid_79_at_failure");
        assert_eq!(w.get(&Suspect::PcieConnector), Some(&3));
        assert_eq!(w.get(&Suspect::Psu), Some(&2));
        assert_eq!(w.get(&Suspect::Cooling), None);
        assert!(weights_for("no_such_evidence").is_empty());
    }

    #[test]
    fn m4_experiment_rows_are_weighted() {
        let w5 = weights_for("fixed_after_x5_slot_change");
        assert_eq!(w5.get(&Suspect::Bios), Some(&2));
        assert_eq!(w5.get(&Suspect::PcieConnector), Some(&2));
        assert_eq!(
            weights_for("fixed_after_x6_rebar_toggle").get(&Suspect::Bios),
            Some(&3)
        );
        assert_eq!(
            weights_for("fixed_after_x8_fan_curve").get(&Suspect::Cooling),
            Some(&3)
        );
    }

    #[test]
    fn ranks_by_score_with_deterministic_tie_break() {
        let v = score_evidence(&[
            ev("xid_79_at_failure"),
            ev("xid_79_at_failure"),
            ev("power_at_limit_in_prewindow"),
        ]);
        // PcieConnector: 3 + 3 + 1 = 7; Psu: 2 + 2 + 3 = 7 (tie);
        // Bios: 2 + 2 = 4; Driver: 1 + 1 = 2.
        assert_eq!(v.ranked[0].suspect, Suspect::PcieConnector);
        assert_eq!(v.ranked[0].score, 7);
        // Tie at 7 breaks on declaration order (PcieConnector before Psu).
        assert_eq!(v.ranked[1].suspect, Suspect::Psu);
        assert_eq!(v.ranked[1].score, 7);
        assert_eq!(v.ranked[2].suspect, Suspect::Bios);
        assert_eq!(v.ranked[2].score, 4);
        assert_eq!(
            v.next_experiment.as_deref(),
            Some(next_experiment_for(&Suspect::PcieConnector))
        );
    }

    #[test]
    fn negative_evidence_lands_last_when_others_are_zero() {
        let v = score_evidence(&[ev("temp_below_trip_fans_not_pinned")]);
        // Zero scores outrank the negative; the first zero in declaration
        // order (Driver) is ranked first, Cooling last.
        assert_eq!(v.ranked[0].suspect, Suspect::Driver);
        assert_eq!(v.ranked[0].score, 0);
        let last = v.ranked.last().unwrap();
        assert_eq!(last.suspect, Suspect::Cooling);
        assert_eq!(last.score, -2);
        // No positive top score: no suggested experiment.
        assert!(v.next_experiment.is_none());
    }

    #[test]
    fn no_evidence_is_all_zero() {
        let v = score_evidence(&[]);
        assert!(
            v.ranked
                .iter()
                .all(|r| r.score == 0 && r.evidence.is_empty())
        );
        assert!(v.next_experiment.is_none());
    }

    #[test]
    fn evidence_list_tracks_contributing_ids() {
        let v = score_evidence(&[ev("xid_ecc_at_failure")]);
        let card = v
            .ranked
            .iter()
            .find(|r| r.suspect == Suspect::CardHardware)
            .unwrap();
        assert_eq!(card.evidence, vec!["xid_ecc_at_failure"]);
        let driver = v
            .ranked
            .iter()
            .find(|r| r.suspect == Suspect::Driver)
            .unwrap();
        assert!(driver.evidence.is_empty());
    }
}
