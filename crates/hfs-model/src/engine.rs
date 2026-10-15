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
pub trait Engine {
    fn ask(&self, state: &FileState) -> ModelOutput;
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
    fn ask(&self, _state: &FileState) -> ModelOutput {
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

    fn state() -> FileState {
        FileState::assemble("photo.raw", 100, 4096, 0, 1, FileProvenance::Full)
    }

    #[test]
    fn stub_answers_every_question_with_maximum_uncertainty() {
        let output = StubEngine.ask(&state());
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
        let output = StubEngine.ask(&state());
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
}
