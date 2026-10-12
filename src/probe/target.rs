//! GPU target selection (PLAN §7 M5, §9): the tool targets a single RTX 5090
//! on a possibly multi-GPU box. `select_target_index` picks the matching card
//! by name from the `nvidia-smi` rows; `resolve_gpu_target` runs the query and
//! returns the selected card's full BDF so the probes and the sampler always
//! watch the right card (the 5090 is not guaranteed to be row 0).

use crate::util::cmd::{CommandRunner, csv_fields};

/// Name fragment that identifies the target card (RTX 5090 / GB202).
/// Matched case-insensitively as a substring of the `nvidia-smi` name.
pub const GPU_TARGET_NAME: &str = "RTX 5090";

/// The selected target GPU and how it was chosen.
pub struct GpuTarget {
    /// `nvidia-smi` name, e.g. `NVIDIA GeForce RTX 5090`.
    pub name: String,
    /// Full BDF, e.g. `00000000:01:00.0`.
    pub bdf_full: String,
    /// Row index in the `nvidia-smi` multi-GPU output.
    pub index: usize,
    /// Total GPUs visible.
    pub total: usize,
    /// True when a row matched `GPU_TARGET_NAME`; false on the first-row
    /// fallback (no RTX 5090 name present).
    pub matched_by_name: bool,
}

/// Pick the target card's row index from a `nvidia-smi` name column.
///
/// Returns `(index, total, matched_by_name)`. Exactly one name match selects
/// that row; no match falls back to the first row (`matched_by_name` false);
/// more than one match selects the first match (a duplicate 5090 is a user
/// configuration issue, not a selection failure).
pub fn select_target_index(names: &[Option<String>]) -> (usize, usize, bool) {
    let total = names.len();
    if total == 0 {
        return (0, 0, false);
    }
    let target = GPU_TARGET_NAME.to_ascii_lowercase();
    let matched = names.iter().position(|n| {
        n.as_deref()
            .is_some_and(|n| n.to_ascii_lowercase().contains(&target))
    });
    match matched {
        Some(i) => (i, total, true),
        None => (0, total, false),
    }
}

/// One-line multi-GPU guardrail note; `None` for a single-GPU box.
pub fn guardrail_note(
    total: usize,
    index: usize,
    name: &str,
    matched_by_name: bool,
) -> Option<String> {
    if total <= 1 {
        return None;
    }
    let suffix = if matched_by_name {
        ""
    } else {
        "; no RTX 5090 name match, using the first GPU"
    };
    Some(format!(
        "{total} GPUs visible; targeting index {index} ({name}){suffix}"
    ))
}

impl GpuTarget {
    /// The multi-GPU guardrail note for this selection; `None` on a single-GPU box.
    pub fn guardrail_note(&self) -> Option<String> {
        guardrail_note(self.total, self.index, &self.name, self.matched_by_name)
    }
}

/// Run `nvidia-smi` and select the target card by name.
pub fn resolve_gpu_target(r: &CommandRunner) -> Option<GpuTarget> {
    let out = r.run(
        "nvidia-smi",
        &[
            "--query-gpu=name,pci.bus_id,serial",
            "--format=csv,noheader,nounits",
        ],
    );
    let text = out.stdout_if_ok()?;
    let rows: Vec<Vec<Option<String>>> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(csv_fields)
        .collect();
    if rows.is_empty() {
        return None;
    }
    let names: Vec<Option<String>> = rows
        .iter()
        .map(|r| r.first().and_then(|v| v.clone()))
        .collect();
    let (index, total, matched_by_name) = select_target_index(&names);
    let row = &rows[index];
    let name = row.first().and_then(|v| v.clone()).unwrap_or_default();
    let bdf_full = row.get(1).and_then(|v| v.clone()).unwrap_or_default();
    Some(GpuTarget {
        name,
        bdf_full,
        index,
        total,
        matched_by_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(s: &[&str]) -> Vec<Option<String>> {
        s.iter().map(|s| Some(s.to_string())).collect()
    }

    #[test]
    fn single_gpu_is_the_target() {
        let (index, total, matched) = select_target_index(&names(&["NVIDIA GeForce RTX 5090"]));
        assert_eq!((index, total, matched), (0, 1, true));
    }

    #[test]
    fn picks_the_5090_row_among_others() {
        // The 5090 is not first: the guardrail must select index 1.
        let (index, total, matched) = select_target_index(&names(&[
            "NVIDIA GeForce RTX 4090",
            "NVIDIA GeForce RTX 5090",
            "NVIDIA A100",
        ]));
        assert_eq!((index, total, matched), (1, 3, true));
    }

    #[test]
    fn name_match_is_case_insensitive_substring() {
        let (index, total, matched) = select_target_index(&names(&["nvidia geforce rtx 5090"]));
        assert_eq!((index, total, matched), (0, 1, true));
    }

    #[test]
    fn no_match_falls_back_to_first_row() {
        let (index, total, matched) = select_target_index(&names(&["NVIDIA A100"]));
        assert_eq!((index, total, matched), (0, 1, false));
    }

    #[test]
    fn empty_column_is_index_zero_total_zero() {
        assert_eq!(select_target_index(&[]), (0, 0, false));
    }

    #[test]
    fn guardrail_note_only_for_multi_gpu() {
        assert!(guardrail_note(1, 0, "RTX 5090", true).is_none());
        let note = guardrail_note(3, 1, "NVIDIA GeForce RTX 5090", true).unwrap();
        assert!(note.contains("3 GPUs visible"));
        assert!(note.contains("targeting index 1"));
        let fallback = guardrail_note(2, 0, "NVIDIA A100", false).unwrap();
        assert!(fallback.contains("no RTX 5090 name match"));
    }

    #[test]
    fn resolve_gpu_target_selects_the_5090_bdf() {
        // A fake `nvidia-smi` that prints two cards, 5090 second.
        let r = CommandRunner::new();
        let out = r.run("nvidia-smi", &["--version"]);
        // Only meaningful where nvidia-smi exists; skip elsewhere.
        if out.missing {
            return;
        }
        let target = resolve_gpu_target(&r).expect("a 5090 box must resolve a target");
        assert!(target.bdf_full.contains(':'));
    }
}
