//! M4 guided isolation experiments (PLAN §4.2, §5 Stage 4, §7 M4): the X1–X8
//! definitions plus the outcome-to-§6-evidence mapping that folds a
//! user-performed change into the scores. The tool guides and retests; the
//! physical work is user-executed (PLAN §9).

use crate::model::{ExperimentOutcome, Suspect};

/// One isolation experiment (PLAN §5 Stage 4).
pub struct Experiment {
    /// `X1`–`X8`.
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// What must hold before the retest is meaningful.
    pub preconditions: &'static str,
    /// User steps; the tool retest (baseline + monitor + stress) follows.
    pub steps: &'static [&'static str],
    /// Hypotheses this experiment discriminates (PLAN §5.5 "Targets").
    pub targets: &'static [Suspect],
}

/// All eight isolation experiments (PLAN §5 Stage 4 table).
pub const EXPERIMENTS: &[Experiment] = &[
    Experiment {
        id: "X1",
        name: "driver rollback",
        description: "Roll back to the last known-good GPU driver version, reboot, retest.",
        preconditions: "The last known-good driver version is identified.",
        steps: &[
            "Install the known-good driver (apt/dkms), removing the current one.",
            "Reboot.",
            "Retest: nvidia-diagnostic monitor under the failing workload.",
        ],
        targets: &[Suspect::Driver],
    },
    Experiment {
        id: "X2",
        name: "BIOS rollback",
        description: "Roll the motherboard BIOS back to the previous version, retest.",
        preconditions: "The previous BIOS image is available (both versions land in the report).",
        steps: &[
            "Flash the previous BIOS version (record both versions).",
            "Reboot.",
            "Retest: nvidia-diagnostic monitor under the failing workload.",
        ],
        targets: &[Suspect::Bios],
    },
    Experiment {
        id: "X3",
        name: "reseat 12VHPWR + PCIe card",
        description: "Power off, reseat the 3× 12VHPWR connectors and the PCIe card, retest. The classic 4090/5090 failure mode.",
        preconditions: "The system is powered off and the card is accessible.",
        steps: &[
            "Power off, unplug the 3× 12VHPWR connectors, reseat each one.",
            "Reseat the PCIe card in its slot.",
            "Retest: nvidia-diagnostic monitor under the failing workload.",
        ],
        targets: &[Suspect::PcieConnector],
    },
    Experiment {
        id: "X4",
        name: "PSU swap",
        description: "Swap the PSU for a known-good unit, retest. A 575 W TGP with large transients stresses weak supplies.",
        preconditions: "A known-good PSU of adequate rating is available.",
        steps: &[
            "Power off, swap the PSU.",
            "Retest: nvidia-diagnostic monitor under the failing workload.",
        ],
        targets: &[Suspect::Psu],
    },
    Experiment {
        id: "X5",
        name: "PCIe slot change / Gen5→Gen4 clamp",
        description: "Move the card to another slot, or clamp the link Gen5→Gen4 in BIOS, retest.",
        preconditions: "A second PCIe slot is available, or the BIOS exposes a link-speed clamp.",
        steps: &[
            "Move the card to the other slot, or set the link speed clamp in BIOS.",
            "Reboot where needed.",
            "Retest: nvidia-diagnostic monitor under the failing workload.",
        ],
        targets: &[Suspect::Bios, Suspect::PcieConnector],
    },
    Experiment {
        id: "X6",
        name: "toggle Resizable BAR / Above-4G decoding",
        description: "Toggle Resizable BAR and Above-4G Decoding in BIOS, retest.",
        preconditions: "The BIOS exposes both options.",
        steps: &[
            "Toggle Resizable BAR and Above-4G Decoding (one at a time is better).",
            "Reboot.",
            "Retest: nvidia-diagnostic monitor under the failing workload.",
        ],
        targets: &[Suspect::Bios],
    },
    Experiment {
        id: "X7",
        name: "card into a known-good second system",
        description: "Run the card in a known-good second system under stress. Reproduces there: card side. Not: system side.",
        preconditions: "A known-good second system with a free PCIe slot and adequate PSU.",
        steps: &[
            "Move the card to the second system.",
            "Run a stress workload (e.g. CUDA burn) under nvidia-diagnostic monitor.",
            "Record whether the failure reproduces.",
        ],
        targets: &[Suspect::CardHardware],
    },
    Experiment {
        id: "X8",
        name: "fan curve / airflow check",
        description: "Check airflow and, best-effort, the fan curve (nvidia-smi --fan-control; the 5090 may not allow manual fan control).",
        preconditions: "The card is accessible; fan control availability is best-effort.",
        steps: &[
            "Check case airflow and dust on the card heatsink.",
            "Try nvidia-smi --fan-control (best-effort on the 5090).",
            "Retest: nvidia-diagnostic monitor under the failing workload.",
        ],
        targets: &[Suspect::Cooling],
    },
];

/// Look up an experiment by `X1`–`X8` (case-insensitive) or bare `1`–`8`.
pub fn by_id(id: &str) -> Option<&'static Experiment> {
    let norm = id.trim().to_ascii_uppercase();
    let key = norm.strip_prefix('X').unwrap_or(norm.as_str());
    let want = format!("X{key}");
    EXPERIMENTS.iter().find(|e| e.id == want.as_str())
}

/// The §6 evidence id an outcome contributes; `None` when the outcome is not
/// scored (e.g. "still reproducing" has no §6 row).
pub fn evidence_for(id: &str, outcome: &ExperimentOutcome) -> Option<&'static str> {
    let exp = by_id(id)?;
    Some(match (exp.id, outcome) {
        ("X1", ExperimentOutcome::Fixed) => "fixed_after_x1_driver_rollback",
        ("X2", ExperimentOutcome::Fixed) => "fixed_after_x2_bios_rollback",
        ("X3", ExperimentOutcome::Fixed) => "fixed_after_x3_reseat",
        ("X4", ExperimentOutcome::Fixed) => "fixed_after_x4_psu_swap",
        ("X5", ExperimentOutcome::Fixed) => "fixed_after_x5_slot_change",
        ("X6", ExperimentOutcome::Fixed) => "fixed_after_x6_rebar_toggle",
        ("X7", ExperimentOutcome::ReproducedInSecondSystem) => "reproduced_in_second_system",
        ("X7", ExperimentOutcome::NotReproducedInSecondSystem) => "not_reproduced_in_second_system",
        ("X8", ExperimentOutcome::Fixed) => "fixed_after_x8_fan_curve",
        _ => return None,
    })
}

/// Short user-facing label for an outcome (prompts, report printout).
pub fn outcome_label(outcome: &ExperimentOutcome) -> &'static str {
    match outcome {
        ExperimentOutcome::Fixed => "fixed",
        ExperimentOutcome::StillReproducing => "still reproducing",
        ExperimentOutcome::ReproducedInSecondSystem => "reproduced in second system",
        ExperimentOutcome::NotReproducedInSecondSystem => "not reproduced in second system",
    }
}

/// The outcome choices the prompt shows for one experiment. X7 asks the
/// second-system question; the rest ask fixed / still reproducing.
pub fn outcome_choices(id: &str) -> &'static [ExperimentOutcome] {
    match by_id(id).map(|e| e.id) {
        Some("X7") => &[
            ExperimentOutcome::ReproducedInSecondSystem,
            ExperimentOutcome::NotReproducedInSecondSystem,
        ],
        _ => &[
            ExperimentOutcome::Fixed,
            ExperimentOutcome::StillReproducing,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experiments_cover_x1_to_x8() {
        assert_eq!(EXPERIMENTS.len(), 8);
        for (i, e) in EXPERIMENTS.iter().enumerate() {
            assert_eq!(e.id, format!("X{}", i + 1));
            assert!(!e.name.is_empty());
            assert!(!e.steps.is_empty());
            assert!(!e.targets.is_empty());
        }
    }

    #[test]
    fn by_id_accepts_x_prefix_bare_number_and_case() {
        assert_eq!(by_id("X3").map(|e| e.id), Some("X3"));
        assert_eq!(by_id("x3").map(|e| e.id), Some("X3"));
        assert_eq!(by_id("3").map(|e| e.id), Some("X3"));
        assert_eq!(by_id(" 1 ").map(|e| e.id), Some("X1"));
        assert!(by_id("9").is_none());
        assert!(by_id("X9").is_none());
        assert!(by_id("foo").is_none());
    }

    #[test]
    fn evidence_for_maps_scored_outcomes() {
        assert_eq!(
            evidence_for("X1", &ExperimentOutcome::Fixed),
            Some("fixed_after_x1_driver_rollback")
        );
        assert_eq!(
            evidence_for("X3", &ExperimentOutcome::Fixed),
            Some("fixed_after_x3_reseat")
        );
        assert_eq!(
            evidence_for("X7", &ExperimentOutcome::ReproducedInSecondSystem),
            Some("reproduced_in_second_system")
        );
        assert_eq!(
            evidence_for("X7", &ExperimentOutcome::NotReproducedInSecondSystem),
            Some("not_reproduced_in_second_system")
        );
        assert_eq!(
            evidence_for("X8", &ExperimentOutcome::Fixed),
            Some("fixed_after_x8_fan_curve")
        );
        // "Still reproducing" is not scored: it rules out nothing new.
        assert_eq!(
            evidence_for("X1", &ExperimentOutcome::StillReproducing),
            None
        );
        // An outcome that does not fit the experiment is not scored.
        assert_eq!(
            evidence_for("X3", &ExperimentOutcome::ReproducedInSecondSystem),
            None
        );
    }

    #[test]
    fn outcome_choices_x7_is_the_second_system_question() {
        assert_eq!(
            outcome_choices("X7"),
            &[
                ExperimentOutcome::ReproducedInSecondSystem,
                ExperimentOutcome::NotReproducedInSecondSystem
            ]
        );
        assert_eq!(
            outcome_choices("X3"),
            &[
                ExperimentOutcome::Fixed,
                ExperimentOutcome::StillReproducing
            ]
        );
    }
}
