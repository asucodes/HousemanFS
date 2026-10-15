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
//! what it is told ([`state`]), and what an engine must return ([`engine`]). Deterministic
//! rules combine the factors into suggestions elsewhere, in code that can be read and tested.
//! A suggestion is emitted only when confidence clears a threshold; otherwise the tool says
//! nothing.
//!
//! # Status: scaffold
//!
//! The boundary exists and is tested, but no engine is wired yet. [`StubEngine`] stands in:
//! it returns maximum uncertainty for every question, which is the honest floor any real
//! engine must rise above. Selecting the engine (D4 names Laya, behind this crate's
//! abstraction so it can be replaced) and writing the combination rules is Phase 2 work.
//! Nothing here touches the disk, the index, or the interface — those seams are load-bearing.
//!
//! Rules for this crate, matching the rest of the workspace:
//!
//! - No operating-system code and no storage code. State arrives assembled; probabilities
//!   leave calibrated. Anything else belongs in another crate.
//! - No `unsafe`. There is nothing here that needs it.

#![forbid(unsafe_code)]

pub mod engine;
pub mod questions;
pub mod state;

pub use engine::{Engine, FactorScores, ModelOutput, StubEngine, meets_threshold, top_choice};
pub use questions::{QUESTION_SET_VERSION, QUESTIONS, QuestionDef};
pub use state::FileState;
