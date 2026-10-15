//! The decision-model boundary.
//!
//! HousemanFS uses a local *decision* model rather than a chat model. The distinction is the
//! whole design: a decision model takes assembled state plus typed questions and returns
//! calibrated probabilities over fixed options. It generates no text, so it cannot return a
//! file that does not exist or an action that was not defined.
//!
//! The boundary is strict and one-directional:
//!
//! ```text
//! metadata -> typed questions -> calibrated probabilities -> YOUR RULES -> suggestion
//!                                                                  ^
//!                                                  the model has no path past here
//! ```
//!
//! This crate owns everything left of the arrow: what the model is asked ([`questions`]),
//! what it is told ([`state`]), what an engine must return ([`engine`]), and how factors
//! become suggestions ([`rules`]). Deterministic rules combine the factors in code that can
//! be read, switched off and tested. A suggestion is emitted only when confidence clears a
//! threshold; otherwise the tool says nothing.
//!
//! # Status: rules with a baseline engine
//!
//! The combination rules exist and are measured against a committed evaluation set. The
//! engine answering the factors is [`HeuristicEngine`]: transparent fixed heuristics,
//! labelled as uncalibrated, standing in for the learned decision model D4 describes.
//! Selecting that engine and calibrating it is the remaining Phase 2 work; the rules will
//! not care, because they read factors, not engines. Nothing here touches the disk, the
//! index, or the interface — those seams are load-bearing.
//!
//! Rules for this crate, matching the rest of the workspace:
//!
//! - No operating-system code and no storage code. State arrives assembled; probabilities
//!   leave calibrated. Anything else belongs in another crate.
//! - No `unsafe`. There is nothing here that needs it.

#![forbid(unsafe_code)]

pub mod engine;
pub mod eval;
pub mod questions;
pub mod rules;
pub mod state;

pub use engine::{
    Engine, FactorScores, HeuristicEngine, ModelOutput, StubEngine, meets_threshold, top_choice,
};
pub use eval::{EVAL_CASES, EVAL_NOW, EvalCase, EvalReport, eval_engine, run_eval};
pub use questions::{QUESTION_SET_VERSION, QUESTIONS, QuestionDef};
pub use rules::{RULES, RuleConfig, Suggestion, SuggestionKind, suggest};
pub use state::FileState;
