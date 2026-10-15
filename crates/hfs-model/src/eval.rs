//! The committed evaluation set: what "useful" means, in cases.
//!
//! Phase 2's exit criterion is measured, not assumed: suggestions must be correct often
//! enough to be useful. This file is the measure — sixteen synthetic cases with expected
//! rule outcomes, containing no real paths, no real sizes, no real anything. A rule change
//! that breaks a case fails the build; a rule change that needs a new case commits the case
//! alongside it, so the set always describes what the tool promises.
//!
//! The bar is exact match per case, including silence: cases that must produce nothing are
//! as load-bearing as cases that must produce something, because an eager tool that
//! suggests everywhere is worse than a quiet one. The `eval` command runs this same set
//! and prints the report; the test below enforces it.

use hfs_core::FileProvenance;

use crate::engine::{Engine, HeuristicEngine};
use crate::rules::{RuleConfig, suggest};
use crate::state::FileState;

/// Fixed "now" for the whole set, so cases never age out: a case written as "400 days old"
/// is 400 days old forever, and the suite does not start failing on birthdays.
pub const EVAL_NOW: i64 = 134_000_000_000_000_000;
const DAY_TICKS: i64 = 86_400 * 10_000_000;
const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;

/// One labeled case: file facts in, expected rule ids out (order-insensitive).
pub struct EvalCase {
    pub name: &'static str,
    pub file: &'static str,
    pub directory: &'static str,
    pub logical: u64,
    pub allocated: u64,
    pub stream_bytes: u64,
    pub stream_count: u32,
    pub links: u32,
    pub provenance: FileProvenance,
    /// Age in days, or `None` for an unmeasured timestamp.
    pub age_days: Option<u64>,
    pub expect: &'static [&'static str],
}

pub const EVAL_CASES: &[EvalCase] = &[
    EvalCase {
        name: "stale 4GB ISO",
        file: "backup.iso",
        directory: "data",
        logical: 4 * GB,
        allocated: 4 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(400),
        expect: &["R1"],
    },
    EvalCase {
        name: "fresh 4GB ISO",
        file: "backup.iso",
        directory: "data",
        logical: 4 * GB,
        allocated: 4 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(2),
        expect: &[],
    },
    EvalCase {
        name: "small stale zip",
        file: "old.zip",
        directory: "data",
        logical: 10 * MB,
        allocated: 10 * MB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(400),
        expect: &[],
    },
    EvalCase {
        name: "denied stale ISO",
        file: "backup.iso",
        directory: "data",
        logical: 4 * GB,
        allocated: 4 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Denied,
        age_days: Some(400),
        expect: &[],
    },
    EvalCase {
        name: "unknown-provenance stale ISO",
        file: "backup.iso",
        directory: "data",
        logical: 4 * GB,
        allocated: 4 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Unknown,
        age_days: Some(400),
        expect: &[],
    },
    EvalCase {
        name: "old build DLL in target",
        file: "engine.dll",
        directory: "target",
        logical: 500 * MB,
        allocated: 500 * MB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(10),
        expect: &["R3"],
    },
    EvalCase {
        name: "small build object",
        file: "util.obj",
        directory: "target",
        logical: MB,
        allocated: MB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(10),
        expect: &[],
    },
    EvalCase {
        name: "fresh sparse VM disk",
        file: "disk.vhdx",
        directory: "vms",
        logical: 100 * GB,
        allocated: 2 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(10),
        expect: &["R4"],
    },
    EvalCase {
        name: "stale sparse VM disk",
        file: "disk.vhdx",
        directory: "vms",
        logical: 100 * GB,
        allocated: 2 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(400),
        expect: &["R1", "R4"],
    },
    EvalCase {
        name: "shared 2GB installer",
        file: "setup.exe",
        directory: "downloads",
        logical: 2 * GB,
        allocated: 2 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 3,
        provenance: FileProvenance::Full,
        age_days: Some(10),
        expect: &["R2"],
    },
    EvalCase {
        name: "single fresh installer",
        file: "setup.exe",
        directory: "downloads",
        logical: 2 * GB,
        allocated: 2 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(10),
        expect: &[],
    },
    EvalCase {
        name: "stream-heavy data file",
        file: "data.bin",
        directory: "data",
        logical: GB,
        allocated: GB,
        stream_bytes: 200 * MB,
        stream_count: 2,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(10),
        expect: &["R5"],
    },
    EvalCase {
        name: "zone-identifier streams",
        file: "download.exe",
        directory: "downloads",
        logical: 50 * MB,
        allocated: 50 * MB,
        stream_bytes: 32,
        stream_count: 1,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(10),
        expect: &[],
    },
    EvalCase {
        name: "fresh photo",
        file: "photo.raw",
        directory: "photos",
        logical: 40 * MB,
        allocated: 40 * MB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(1),
        expect: &[],
    },
    EvalCase {
        name: "old 5GB log",
        file: "server.log",
        directory: "logs",
        logical: 5 * GB,
        allocated: 5 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: Some(400),
        expect: &[],
    },
    EvalCase {
        name: "ageless 5GB ISO",
        file: "backup.iso",
        directory: "data",
        logical: 5 * GB,
        allocated: 5 * GB,
        stream_bytes: 0,
        stream_count: 0,
        links: 1,
        provenance: FileProvenance::Full,
        age_days: None,
        expect: &[],
    },
];

/// What one run produced.
#[derive(Debug, Clone, Default)]
pub struct EvalReport {
    pub passed: usize,
    pub failed: usize,
    pub failures: Vec<String>,
}

/// Run every case through an engine and the rules. Exact rule sets must match, including
/// empty: a case that should stay silent and does not is a failure, not a near miss.
pub fn run_eval(engine: &dyn Engine, config: &RuleConfig) -> EvalReport {
    let mut report = EvalReport::default();
    for case in EVAL_CASES {
        let modified = case
            .age_days
            .map(|days| EVAL_NOW - days as i64 * DAY_TICKS)
            .unwrap_or(0);
        let state = FileState::assemble(
            case.file,
            case.logical,
            case.allocated,
            case.stream_bytes,
            case.stream_count,
            case.links,
            case.provenance,
        )
        .with_location(case.directory, modified);
        let output = engine.ask(&state, EVAL_NOW);
        let mut got: Vec<&str> = suggest(&state, &output, EVAL_NOW, config)
            .iter()
            .map(|suggestion| suggestion.rule)
            .collect();
        got.sort_unstable();
        let mut want: Vec<&str> = case.expect.to_vec();
        want.sort_unstable();
        if got == want {
            report.passed += 1;
        } else {
            report.failed += 1;
            report.failures.push(format!(
                "{}: expected [{}], got [{}]",
                case.name,
                want.join(", "),
                got.join(", ")
            ));
        }
    }
    report
}

/// The engine the report names. Kept beside the runner so the CLI and the test agree on
/// what "the evaluation" means without either duplicating it.
pub fn eval_engine() -> HeuristicEngine {
    HeuristicEngine
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_set_passes_whole() {
        let report = run_eval(&eval_engine(), &RuleConfig::default());
        assert!(
            report.failures.is_empty(),
            "evaluation failures:\n{}",
            report.failures.join("\n")
        );
        assert_eq!(report.passed, EVAL_CASES.len());
    }

    #[test]
    fn the_set_covers_every_rule_and_silence() {
        // A set that never expects R4 cannot catch R4 breaking; a set with no silence
        // cases cannot catch an eager tool. Both are asserted, so coverage cannot rot.
        let mut covered: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut silent = 0;
        for case in EVAL_CASES {
            if case.expect.is_empty() {
                silent += 1;
            }
            covered.extend(case.expect.iter().copied());
        }
        for rule in crate::rules::RULES {
            assert!(
                covered.contains(rule.id),
                "{} has no positive case in the evaluation set",
                rule.id
            );
        }
        assert!(silent >= 3, "the set needs silence cases; found {silent}");
    }
}
