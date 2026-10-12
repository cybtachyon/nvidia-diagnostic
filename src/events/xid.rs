//! Xid / NVRM kernel-log event parsing (PLAN §4.2, §5 Stage 1).
//!
//! The Xid stream is the most informative signal for a driver-level GPU
//! reset. This module owns the regexes plus the incremental scanner the
//! Stage-1 monitor uses for its 1 s kernel-log scan; `probe/logs.rs`
//! reuses the same parser for the Stage-0 history.

use crate::model::XidEvent;
use regex::Regex;
use std::collections::HashSet;

/// `NVRM: Xid (PCI:01:00.0) 79`
pub const XID_RE: &str = r"NVRM: Xid \(PCI:([0-9a-fA-F]{2}:[0-9a-fA-F]{2}\.\d+)\)\s+(\d+)";
/// dmesg -T: `[Thu Oct 12 01:23:45 2025]`
const TS_DMESG_RE: &str = r"^\[([A-Z][a-z]{2} [A-Z][a-z]{2} \d+ \d{2}:\d{2}:\d{2} \d{4})\]";
/// journalctl -k: `Oct 12 01:23:45 host kernel:`
const TS_JOURNAL_RE: &str = r"^([A-Z][a-z]{2} \d+ \d{2}:\d{2}:\d{2}) \S+ kernel:";

/// Lowercased NVRM-line fragments that indicate a driver-level GPU reset
/// (PLAN §4.1 step 3: "driver log lines / Xid 79 / `nvidia-smi` going
/// stale"). Xid 79 is handled separately by `is_reset_line`.
const RESET_PATTERNS: &[&str] = &[
    "is being reset",
    "gpu reset",
    "resetting gpu",
    "reinit",
    "recovery",
];

/// True when a kernel-log line looks like a driver-level GPU reset.
pub fn is_reset_line(line: &str, xid_id: Option<u32>) -> bool {
    if xid_id == Some(79) {
        return true;
    }
    let lower = line.to_lowercase();
    RESET_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Compiled regexes for one parsing session.
struct NvrmParser {
    xid: Regex,
    ts_dmesg: Regex,
    ts_journal: Regex,
}

impl NvrmParser {
    fn new() -> Self {
        Self {
            xid: Regex::new(XID_RE).unwrap(),
            ts_dmesg: Regex::new(TS_DMESG_RE).unwrap(),
            ts_journal: Regex::new(TS_JOURNAL_RE).unwrap(),
        }
    }

    /// Classify one kernel-log line: `None` when it is not an NVRM/Xid line,
    /// `Some(event)` when it carries a parseable Xid.
    fn classify(&self, line: &str) -> Option<XidEvent> {
        if !line.contains("NVRM") && !line.contains("Xid") {
            return None;
        }
        self.xid.captures(line).map(|caps| {
            let ts = self
                .ts_dmesg
                .captures(line)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .or_else(|| {
                    self.ts_journal
                        .captures(line)
                        .and_then(|c| c.get(1))
                        .map(|m| m.as_str().to_string())
                });
            let id: u32 = caps
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            XidEvent {
                ts,
                bdf: caps.get(1).map(|m| m.as_str().to_string()),
                id,
                raw: line.trim().to_string(),
            }
        })
    }
}

/// Split kernel-log lines into (NVRM/Xid lines, parsed Xid events).
pub fn parse_nvrm_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
) -> (Vec<String>, Vec<XidEvent>) {
    let parser = NvrmParser::new();
    let mut nvrm_lines: Vec<String> = vec![];
    let mut xid_events: Vec<XidEvent> = vec![];
    for line in lines {
        if !line.contains("NVRM") && !line.contains("Xid") {
            continue;
        }
        nvrm_lines.push(line.trim().to_string());
        if let Some(event) = parser.classify(line) {
            xid_events.push(event);
        }
    }
    (nvrm_lines, xid_events)
}

/// One newly-seen kernel-log line from the incremental scan.
pub struct ScannedLine {
    pub raw: String,
    /// `Some` when the line carries a parseable Xid.
    pub xid: Option<XidEvent>,
    /// True when the line looks like a driver-level GPU reset.
    pub reset: bool,
}

/// Deduplicating incremental scanner: feed it the current kernel-log tail
/// once per second; it yields only the NVRM/Xid lines it has not seen
/// before. The first scan seeds the seen-set with the whole history, so
/// pre-monitor events are not re-reported.
pub struct XidScanner {
    parser: NvrmParser,
    seen: HashSet<String>,
}

impl Default for XidScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl XidScanner {
    pub fn new() -> Self {
        Self {
            parser: NvrmParser::new(),
            seen: HashSet::new(),
        }
    }

    /// Scan `lines`, returning the NVRM/Xid lines never seen before.
    pub fn scan<'a>(&mut self, lines: impl IntoIterator<Item = &'a str>) -> Vec<ScannedLine> {
        let mut out = Vec::new();
        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !trimmed.contains("NVRM") && !trimmed.contains("Xid") {
                continue;
            }
            if !self.seen.insert(trimmed.to_string()) {
                continue;
            }
            let xid = self.parser.classify(trimmed);
            let reset = is_reset_line(trimmed, xid.as_ref().map(|e| e.id));
            out.push(ScannedLine {
                raw: trimmed.to_string(),
                xid,
                reset,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xid_from_dmesg_t() {
        let (nvrm, events) = parse_nvrm_lines([
            "[Thu Oct 12 01:23:45 2026] NVRM: Xid (PCI:01:00.0) 79, minor code 0x0",
        ]);
        assert_eq!(nvrm.len(), 1);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.id, 79);
        assert_eq!(e.bdf.as_deref(), Some("01:00.0"));
        assert_eq!(e.ts.as_deref(), Some("Thu Oct 12 01:23:45 2026"));
    }

    #[test]
    fn parses_xid_from_journalctl() {
        let (_, events) = parse_nvrm_lines([
            "Oct 12 01:23:45 host kernel: NVRM: Xid (PCI:41:00.0) 43, channel 0x0",
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, 43);
        assert_eq!(events[0].bdf.as_deref(), Some("41:00.0"));
        assert_eq!(events[0].ts.as_deref(), Some("Oct 12 01:23:45"));
    }

    #[test]
    fn ignores_non_xid_nvrm_lines() {
        let (nvrm, events) = parse_nvrm_lines([
            "Oct 12 01:12:37 host kernel: NVRM: loading NVIDIA UNIX Open Kernel Module",
            "Oct 12 01:12:37 host kernel: something else",
        ]);
        assert_eq!(nvrm.len(), 1);
        assert!(events.is_empty());
    }

    #[test]
    fn xid_79_is_a_reset_line() {
        assert!(is_reset_line("NVRM: Xid (PCI:01:00.0) 79", Some(79)));
        assert!(!is_reset_line("NVRM: Xid (PCI:01:00.0) 43", Some(43)));
    }

    #[test]
    fn nvrm_reset_phrases_are_detected() {
        assert!(is_reset_line("NVRM: GPU 0000:01:00.0 is being reset", None));
        assert!(is_reset_line("nvrm: reinit adapter after fault", None));
        assert!(!is_reset_line("NVRM: Xid (PCI:01:00.0) 63", None));
    }

    #[test]
    fn scanner_dedups_and_reports_new_lines() {
        let mut s = XidScanner::new();
        let history = [
            "Oct 12 01:12:37 host kernel: NVRM: loading module",
            "Oct 12 01:23:45 host kernel: NVRM: Xid (PCI:01:00.0) 79, minor code 0x0",
            "Oct 12 01:23:46 host kernel: pcieport: nothing interesting",
        ];
        // First scan seeds the seen-set: everything is "new".
        let first = s.scan(history.iter().copied());
        assert_eq!(first.len(), 2);
        assert!(first[1].xid.is_some());
        assert!(first[1].reset);

        // Second scan of the same tail: nothing new.
        assert!(s.scan(history.iter().copied()).is_empty());

        // A genuinely new Xid is reported once.
        let later = [
            "Oct 12 01:23:45 host kernel: NVRM: Xid (PCI:01:00.0) 79, minor code 0x0",
            "Oct 12 02:01:02 host kernel: NVRM: Xid (PCI:01:00.0) 43, channel 0x1",
        ];
        let second = s.scan(later.iter().copied());
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].xid.as_ref().unwrap().id, 43);
        assert!(!second[0].reset);
    }
}
