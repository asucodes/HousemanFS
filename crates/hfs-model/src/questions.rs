//! What the model is asked.
//!
//! The question set is the equivalent of a prompt, with one decisive advantage: it is a
//! versioned artifact, not a string. The engine must answer each question with exactly one
//! of the supplied options, which is what makes invented files and undefined actions
//! structurally impossible rather than merely unlikely.
//!
//! Every question includes an abstention option (`unknown`). An engine that cannot tell is
//! expected to say so, and the confidence gate downstream treats low confidence as silence.
//! Changing these options changes what the tool can claim, so the set carries a version and
//! every model output names the version it answered.

/// Version of the question set. Outputs record this; rules must check it before trusting
/// an output, because options added or removed change the meaning of every probability.
pub const QUESTION_SET_VERSION: u32 = 1;

/// One typed question: an identifier and the only answers permitted.
#[derive(Debug, Clone, Copy)]
pub struct QuestionDef {
    pub id: &'static str,
    pub options: &'static [&'static str],
}

/// The questions, in full. Three factors, each with an abstention:
///
/// - `category`: what kind of thing the file is, from its name and shape.
/// - `staleness`: whether the file looks live, aging, or abandoned. (Access times are a
///   weak signal on Windows and the engine must treat them as such; the question stays
///   because even a weak signal, calibrated, beats none.)
/// - `compressibility`: whether the data would likely shrink under compression. The answer
///   is a property of the data, never a promise of bytes freed — freed bytes come from
///   measured deltas (D9), not from this factor.
pub const QUESTIONS: &[QuestionDef] = &[
    QuestionDef {
        id: "category",
        options: &["media", "archive", "build", "data", "system", "unknown"],
    },
    QuestionDef {
        id: "staleness",
        options: &["active", "aging", "stale", "unknown"],
    },
    QuestionDef {
        id: "compressibility",
        options: &["high", "medium", "low", "unknown"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_question_has_an_abstention() {
        assert!(!QUESTIONS.is_empty());
        for question in QUESTIONS {
            assert!(
                question.options.contains(&"unknown"),
                "{} must let the engine abstain",
                question.id
            );
            assert!(
                question.options.len() >= 2,
                "{} needs a real choice, not only abstention",
                question.id
            );
        }
    }

    #[test]
    fn question_ids_are_stable() {
        let ids: Vec<&str> = QUESTIONS.iter().map(|q| q.id).collect();
        assert_eq!(ids, vec!["category", "staleness", "compressibility"]);
    }
}
