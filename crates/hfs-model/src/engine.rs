//! What an engine must return, and the engine-shaped hole.
//!
//! An engine takes assembled state and returns one calibrated probability per option per
//! question. "Calibrated" is doing the heavy lifting: a 0.8 must mean right four times out
//! of five, measured on a committed evaluation set — otherwise the confidence gate is
//! theatre. Building that evaluation set (synthetic, no real paths, per the fixture rule)
//! is part of wiring a real engine, not an afterthought.
//!
//! Combination rules live downstream of this crate: they take factors, apply versioned,
//! switchable logic, and emit suggestions only above threshold. The helpers here
//! ([`top_choice`], [`meets_threshold`]) are the shared vocabulary for those rules, kept in
//! the boundary so every rule reads confidence the same way.

use crate::questions::{QUESTION_SET_VERSION, QUESTIONS};
use crate::state::FileState;

/// One question answered: the option probabilities, summing to 1.
#[derive(Debug, Clone)]
pub struct FactorScores {
    pub question: &'static str,
    pub options: Vec<(&'static str, f64)>,
}

/// Everything the model said about one file.
#[derive(Debug, Clone)]
pub struct ModelOutput {
    pub factors: Vec<FactorScores>,
    /// Which engine produced this, e.g. `"stub"` or the model name and weights version.
    /// Recorded because a probability without its source is as untraceable as a number
    /// without its measurement.
    pub engine: &'static str,
    /// The question-set version answered. Rules must refuse outputs from versions they do
    /// not understand rather than misread them.
    pub question_set: u32,
}

/// Something that turns state into factors. Kept behind a trait so the engine (D4) can be
/// replaced without touching callers — the current engine will not be the engine in a
/// year's time, and the code should assume that from day one.
///
/// `now` arrives as Windows ticks so engines stay pure: no clock reads inside, which keeps
/// every answer reproducible from its inputs in tests.
pub trait Engine {
    fn ask(&self, state: &FileState, now: i64) -> ModelOutput;
}

/// Maximum uncertainty for every question: uniform probabilities, including over the
/// abstention option.
///
/// The stub is the honest floor. It lets the combination rules, the threshold logic and the
/// whole pipeline be built and tested before any model exists — against an engine that
/// claims nothing, so any suggestion the pipeline emits from stub factors is a pipeline
/// bug, not a model opinion. That property is asserted below and should never stop being
/// asserted.
#[derive(Debug, Clone, Copy, Default)]
pub struct StubEngine;

impl Engine for StubEngine {
    fn ask(&self, _state: &FileState, _now: i64) -> ModelOutput {
        ModelOutput {
            factors: QUESTIONS
                .iter()
                .map(|question| FactorScores {
                    question: question.id,
                    options: question
                        .options
                        .iter()
                        .map(|option| (*option, 1.0 / question.options.len() as f64))
                        .collect(),
                })
                .collect(),
            engine: "stub",
            question_set: QUESTION_SET_VERSION,
        }
    }
}

/// A transparent baseline engine: fixed heuristics, no learning, no weights.
///
/// This is not the decision model D4 describes — it is the scaffolding one. Every factor
/// comes from a readable rule over extension, age and size, with the constants in the open
/// below. Its probabilities are *uncalibrated*: 0.8 here means "the heuristic fired
/// strongly", not "right four times out of five". The evaluation set measures what that is
/// worth, and a learned engine replaces this one through the [`Engine`] trait when it
/// exists — the rules downstream will not care, because they read factors, not engines.
///
/// Why ship heuristics at all: the combination rules, the confidence gate and the whole
/// suggestion pipeline need something to run against, and a readable baseline that claims
/// little beats a black box that claims much. Nothing here is presented as intelligence.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicEngine;

impl HeuristicEngine {
    fn category(state: &FileState) -> FactorScores {
        // (option, probability when the extension matches). The match takes 0.7; the rest
        // spreads thinly, with abstention always retaining a share so nothing is certain.
        const MEDIA: &[&str] = &[
            "mp4", "mkv", "avi", "mov", "wmv", "webm", "mp3", "flac", "wav",
        ];
        const ARCHIVE: &[&str] = &["zip", "7z", "rar", "tar", "gz", "iso", "img", "vhd", "vhdx"];
        const BUILD: &[&str] = &["obj", "o", "pyc", "class", "pdb", "dll", "lib", "node"];
        const SYSTEM: &[&str] = &["sys", "dll", "mui", "cat", "inf"];

        let group = if MEDIA.contains(&state.ext.as_str()) {
            "media"
        } else if ARCHIVE.contains(&state.ext.as_str()) {
            "archive"
        } else if BUILD.contains(&state.ext.as_str()) || is_build_dir(&state.directory) {
            "build"
        } else if SYSTEM.contains(&state.ext.as_str()) {
            "system"
        } else if state.ext == "txt"
            || state.ext == "md"
            || state.ext == "csv"
            || state.ext == "log"
        {
            "data"
        } else {
            "unknown"
        };
        spread(
            "category",
            &["media", "archive", "build", "data", "system"],
            group,
        )
    }

    fn staleness(state: &FileState, now: i64) -> FactorScores {
        const DAY_TICKS: i64 = 86_400 * 10_000_000;
        match state.age_at(now) {
            None => spread("staleness", &["active", "aging", "stale"], "unknown"),
            Some(age) if age > 365 * DAY_TICKS => {
                spread("staleness", &["active", "aging", "stale"], "stale")
            }
            Some(age) if age > 90 * DAY_TICKS => {
                spread("staleness", &["active", "aging", "stale"], "aging")
            }
            Some(_) => spread("staleness", &["active", "aging", "stale"], "active"),
        }
    }

    fn compressibility(state: &FileState) -> FactorScores {
        const HIGH: &[&str] = &["txt", "md", "csv", "log", "xml", "json", "bmp"];
        const LOW: &[&str] = &["zip", "7z", "rar", "gz", "jpg", "jpeg", "png", "mp4", "mkv"];
        let group = if HIGH.contains(&state.ext.as_str()) {
            "high"
        } else if LOW.contains(&state.ext.as_str()) {
            "low"
        } else {
            "medium"
        };
        spread("compressibility", &["high", "medium", "low"], group)
    }
}

impl Engine for HeuristicEngine {
    fn ask(&self, state: &FileState, now: i64) -> ModelOutput {
        ModelOutput {
            factors: vec![
                Self::category(state),
                Self::staleness(state, now),
                Self::compressibility(state),
            ],
            engine: "heuristic-v1",
            question_set: QUESTION_SET_VERSION,
        }
    }
}

/// Directories whose contents are regenerable build output. Closed list, in the open.
fn is_build_dir(directory: &str) -> bool {
    matches!(
        directory.to_ascii_lowercase().as_str(),
        "target" | "node_modules" | "debug" | "release" | "build" | "dist" | "__pycache__"
    )
}

/// One option takes the lead, abstention always keeps a share, the rest split the
/// remainder. The numbers are deliberately blunt: this engine's "confidence" is a fixed
/// property of its rules, and dressing it in finer decimals would imply a calibration that
/// does not exist.
fn spread(question: &'static str, options: &[&'static str], winner: &str) -> FactorScores {
    let others_each = if winner == "unknown" {
        0.3 / options.len() as f64
    } else {
        0.2 / options.len().saturating_sub(1).max(1) as f64
    };
    let mut scored: Vec<(&'static str, f64)> = options
        .iter()
        .map(|option| (*option, if *option == winner { 0.7 } else { others_each }))
        .collect();
    scored.push(("unknown", if winner == "unknown" { 0.7 } else { 0.1 }));
    FactorScores {
        question,
        options: scored,
    }
}

/// The winning option and its probability. Ties break toward `unknown`: when the model is
/// equally torn, abstention wins by construction, and silence is the correct output.
pub fn top_choice(factor: &FactorScores) -> (&'static str, f64) {
    let mut best = ("unknown", 0.0);
    for (option, probability) in &factor.options {
        if *probability > best.1 || (*probability == best.1 && *option == "unknown") {
            best = (*option, *probability);
        }
    }
    best
}

/// Whether a probability clears the gate. Below threshold the tool says nothing rather than
/// guessing — the confidence principle, in one function, so every rule applies it
/// identically.
pub fn meets_threshold(probability: f64, threshold: f64) -> bool {
    probability >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use hfs_core::FileProvenance;

    const NOW: i64 = 134_000_000_000_000_000;

    fn state() -> FileState {
        FileState::assemble("photo.raw", 100, 4096, 0, 0, 1, FileProvenance::Full)
    }

    #[test]
    fn stub_answers_every_question_with_maximum_uncertainty() {
        let output = StubEngine.ask(&state(), NOW);
        assert_eq!(output.engine, "stub");
        assert_eq!(output.question_set, QUESTION_SET_VERSION);
        assert_eq!(output.factors.len(), QUESTIONS.len());
        for factor in &output.factors {
            let total: f64 = factor.options.iter().map(|(_, p)| p).sum();
            assert!(
                (total - 1.0).abs() < 1e-9,
                "probabilities must sum to 1, got {total}"
            );
            let first = factor.options[0].1;
            assert!(
                factor
                    .options
                    .iter()
                    .all(|(_, p)| (p - first).abs() < 1e-12),
                "the stub claims nothing: every option equal"
            );
        }
    }

    #[test]
    fn ties_break_toward_abstention() {
        let output = StubEngine.ask(&state(), NOW);
        // Uniform means every option ties, including "unknown" — and unknown must win.
        for factor in &output.factors {
            assert_eq!(top_choice(factor).0, "unknown");
        }
        // A clear winner still wins.
        let factor = FactorScores {
            question: "category",
            options: vec![("media", 0.7), ("unknown", 0.2), ("data", 0.1)],
        };
        assert_eq!(top_choice(&factor), ("media", 0.7));
    }

    #[test]
    fn the_gate_is_exact_at_threshold() {
        assert!(meets_threshold(0.8, 0.8));
        assert!(!meets_threshold(0.799, 0.8));
    }

    #[test]
    fn heuristic_factors_sum_to_one_and_abstain_when_unmeasured() {
        const DAY_TICKS: i64 = 86_400 * 10_000_000;
        let old_iso = FileState::assemble("backup.iso", 100, 8192, 0, 0, 1, FileProvenance::Full)
            .with_location("data", NOW - 400 * DAY_TICKS);
        let output = HeuristicEngine.ask(&old_iso, NOW);
        assert_eq!(output.engine, "heuristic-v1");
        for factor in &output.factors {
            let total: f64 = factor.options.iter().map(|(_, p)| p).sum();
            assert!(
                (total - 1.0).abs() < 1e-9,
                "{} probabilities must sum to 1, got {total}",
                factor.question
            );
        }
        let category = output
            .factors
            .iter()
            .find(|f| f.question == "category")
            .unwrap();
        assert_eq!(top_choice(category).0, "archive");
        let staleness = output
            .factors
            .iter()
            .find(|f| f.question == "staleness")
            .unwrap();
        assert_eq!(top_choice(staleness).0, "stale");

        // No timestamp, no staleness opinion: the engine abstains rather than guessing.
        let ageless = FileState::assemble("backup.iso", 100, 8192, 0, 0, 1, FileProvenance::Full);
        let output = HeuristicEngine.ask(&ageless, NOW);
        let staleness = output
            .factors
            .iter()
            .find(|f| f.question == "staleness")
            .unwrap();
        assert_eq!(top_choice(staleness).0, "unknown");
    }
}
