//! The combination rules: factors in, suggestions out.
//!
//! The model proposes; code decides (D5). Each rule is named, versioned and individually
//! switchable, and every suggestion carries its rule, its confidence and the measured facts
//! behind it. There is no score, no rank, no black box — a suggestion is a readable function
//! of inspectable inputs, and switching a rule off removes exactly its suggestions.
//!
//! Confidence gates everything. A suggestion whose confidence clears [`RuleConfig`] is
//! emitted; anything below stays silent. Measurement quality weighs every confidence: a
//! factor built on unmeasured figures is down-weighted, and one built on a file that could
//! not be opened at all is dropped — the gate must never be cleared by a guess.
//!
//! Suggestions are judgments, not actions. Nothing here deletes, moves or compresses; the
//! verbs are "candidate for" and "worth a look", with the reversible action they foreshadow
//! named so a future Phase 3 can attach preflight and undo. Irreversible operations are
//! never reachable from here.

use crate::engine::{ModelOutput, meets_threshold, top_choice};
use crate::state::FileState;

/// How suggestions are assembled: floors, gates and switches.
#[derive(Debug, Clone)]
pub struct RuleConfig {
    /// Minimum confidence for a suggestion to be emitted. Below this the tool stays silent
    /// rather than guessing — the confidence principle, configured in one place.
    pub min_confidence: f64,
    /// Minimum occupied size for size-gated rules. Suggestions about smaller files are
    /// noise whatever their confidence; the floor keeps the signal worth reading.
    pub min_bytes: u64,
    /// Rule ids switched off, e.g. `["R3"]`. Judgment comes from rules you can switch off,
    /// so switching off is a first-class operation, not a code change.
    pub without: Vec<String>,
}

impl Default for RuleConfig {
    fn default() -> Self {
        Self {
            min_confidence: 0.5,
            min_bytes: 1 << 30,
            without: Vec::new(),
        }
    }
}

impl RuleConfig {
    pub fn is_enabled(&self, id: &str) -> bool {
        !self.without.iter().any(|off| off == id)
    }
}

/// What a suggestion claims, with the measured facts it stands on.
#[derive(Debug, Clone, PartialEq)]
pub enum SuggestionKind {
    /// A large archive that looks abandoned: a candidate for archive-move, which Phase 3
    /// will offer with preflight and undo.
    StaleArchive { allocated: u64, age_days: u64 },
    /// Regenerable build output: removable by rebuilding, never by guessing — a future
    /// clean command's input, not this tool's action.
    BuildOutput { allocated: u64 },
    /// Reports far more than it occupies: size rankings mislead here, and any reclaim
    /// figure built on the logical size would be wrong in the optimistic direction.
    SparseMisranked { logical: u64, allocated: u64 },
    /// Named-stream storage most tools never show, large enough to matter.
    StreamHeavy {
        stream_bytes: u64,
        stream_count: u32,
    },
    /// Several names, one dataset: removing names frees nothing until the last one goes,
    /// and the suggestion says exactly that.
    SharedNames { links: u32, allocated: u64 },
}

impl SuggestionKind {
    /// The verb, carefully un-actionable: candidates and cautions, never instructions.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::StaleArchive { .. } => "candidate for archive-move",
            Self::BuildOutput { .. } => "regenerable build output",
            Self::SparseMisranked { .. } => "ranking misleading: reports more than it occupies",
            Self::StreamHeavy { .. } => "named streams worth a look",
            Self::SharedNames { .. } => "shared names: removing one frees nothing",
        }
    }

    /// The measured facts the suggestion stands on, as label/value pairs.
    ///
    /// Lives here so every frontend renders the same evidence: the CLI joins the pairs
    /// into one line, richer interfaces lay them out as rows. Values are pre-formatted
    /// with the shared byte spellings; labels are stable strings, not sentences.
    pub fn evidence(&self) -> Vec<(&'static str, String)> {
        use hfs_core::format::human_bytes_exact;
        match self {
            Self::StaleArchive {
                allocated,
                age_days,
            } => vec![
                ("occupies", human_bytes_exact(*allocated)),
                ("last written", format!("{age_days} days ago")),
            ],
            Self::BuildOutput { allocated } => vec![
                ("occupies", human_bytes_exact(*allocated)),
                ("note", "regenerable by rebuilding".to_string()),
            ],
            Self::SparseMisranked { logical, allocated } => vec![
                ("reports", human_bytes_exact(*logical)),
                ("occupies", human_bytes_exact(*allocated)),
            ],
            Self::StreamHeavy {
                stream_bytes,
                stream_count,
            } => vec![(
                "streams",
                format!(
                    "{} across {stream_count} named {}",
                    human_bytes_exact(*stream_bytes),
                    if *stream_count == 1 {
                        "stream"
                    } else {
                        "streams"
                    },
                ),
            )],
            Self::SharedNames { links, allocated } => vec![
                ("names", links.to_string()),
                ("occupies", human_bytes_exact(*allocated)),
            ],
        }
    }
}

/// One suggestion: what, how confident, and by which rule.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub rule: &'static str,
    pub rule_version: u32,
    pub confidence: f64,
    pub kind: SuggestionKind,
}

/// One named, versioned, switchable rule.
pub struct RuleDef {
    pub id: &'static str,
    pub version: u32,
    pub description: &'static str,
    pub check: fn(&FileState, &ModelOutput, i64, &RuleConfig) -> Option<Suggestion>,
}

/// The full set, in stable order. Adding a rule extends what the tool can claim; removing
/// one (via [`RuleConfig::without`]) retracts exactly its suggestions.
pub const RULES: &[RuleDef] = &[
    RuleDef {
        id: "R1",
        version: 1,
        description: "large archives that look abandoned",
        check: stale_archive,
    },
    RuleDef {
        id: "R2",
        version: 1,
        description: "several names sharing one dataset",
        check: shared_names,
    },
    RuleDef {
        id: "R3",
        version: 1,
        description: "regenerable build output",
        check: build_output,
    },
    RuleDef {
        id: "R4",
        version: 1,
        description: "files reporting far more than they occupy",
        check: sparse_misranked,
    },
    RuleDef {
        id: "R5",
        version: 1,
        description: "large named-stream storage",
        check: stream_heavy,
    },
];

/// How much a factor is worth given what produced it. Full measurement counts whole;
/// partial measurement counts less; a file that could not be opened counts for nothing —
/// no suggestion may rest on figures that were never measured.
fn provenance_weight(state: &FileState) -> f64 {
    use hfs_core::FileProvenance;
    match state.provenance {
        FileProvenance::Full => 1.0,
        FileProvenance::NoIdentity | FileProvenance::NoStreams => 0.8,
        FileProvenance::Unknown => 0.5,
        FileProvenance::Denied => 0.0,
    }
}

/// The winning probability for one question, or 0 when the engine did not answer it.
fn probability(output: &ModelOutput, question: &str) -> f64 {
    output
        .factors
        .iter()
        .find(|factor| factor.question == question)
        .map(top_choice)
        .filter(|(option, _)| *option != "unknown")
        .map(|(_, probability)| probability)
        .unwrap_or(0.0)
}

/// Gate a candidate confidence: below threshold it becomes silence, not a weaker claim.
fn gate(confidence: f64, config: &RuleConfig) -> Option<f64> {
    meets_threshold(confidence, config.min_confidence).then_some(confidence)
}

const ARCHIVE_EXTS: &[&str] = &["iso", "img", "vhd", "vhdx", "zip", "7z", "rar", "tar", "gz"];
/// Build output below this is clutter, not signal: per-file suggestions about kilobytes of
/// object files would bury the directories that matter.
const BUILD_MIN_BYTES: u64 = 100 << 20;
/// Sparse files below this are curiosities; the misleading ones are the large ones.
const SPARSE_MIN_LOGICAL: u64 = 1 << 30;
/// Named streams below this are metadata (zone identifiers); the ones worth naming are big.
const STREAM_MIN_BYTES: u64 = 100 << 20;

fn stale_archive(
    state: &FileState,
    output: &ModelOutput,
    now: i64,
    config: &RuleConfig,
) -> Option<Suggestion> {
    if !ARCHIVE_EXTS.contains(&state.ext.as_str()) || state.allocated < config.min_bytes {
        return None;
    }
    let confidence = gate(
        probability(output, "staleness") * provenance_weight(state),
        config,
    )?;
    // The engine's top staleness must actually be "stale": a high probability on "active"
    // is confidence in the opposite direction, and direction matters more than magnitude.
    let is_stale = output
        .factors
        .iter()
        .find(|factor| factor.question == "staleness")
        .is_some_and(|factor| top_choice(factor).0 == "stale");
    if !is_stale {
        return None;
    }
    Some(Suggestion {
        rule: "R1",
        rule_version: 1,
        confidence,
        kind: SuggestionKind::StaleArchive {
            allocated: state.allocated,
            age_days: (state.age_at(now).unwrap_or(0) / 86_400 / 10_000_000) as u64,
        },
    })
}

fn shared_names(
    state: &FileState,
    _output: &ModelOutput,
    _now: i64,
    config: &RuleConfig,
) -> Option<Suggestion> {
    // A measured fact, not a model judgment: no factor needed, but measurement quality
    // still gates — shared names on an unopened file are a guess about its identity.
    if state.links <= 1 || state.allocated < config.min_bytes {
        return None;
    }
    let confidence = gate(0.9 * provenance_weight(state), config)?;
    Some(Suggestion {
        rule: "R2",
        rule_version: 1,
        confidence,
        kind: SuggestionKind::SharedNames {
            links: state.links,
            allocated: state.allocated,
        },
    })
}

fn build_output(
    state: &FileState,
    output: &ModelOutput,
    _now: i64,
    config: &RuleConfig,
) -> Option<Suggestion> {
    if !is_build_context(state) || state.allocated < BUILD_MIN_BYTES {
        return None;
    }
    let is_build = output
        .factors
        .iter()
        .find(|factor| factor.question == "category")
        .is_some_and(|factor| top_choice(factor).0 == "build");
    if !is_build {
        return None;
    }
    let confidence = gate(
        probability(output, "category") * provenance_weight(state),
        config,
    )?;
    Some(Suggestion {
        rule: "R3",
        rule_version: 1,
        confidence,
        kind: SuggestionKind::BuildOutput {
            allocated: state.allocated,
        },
    })
}

/// Build context is the directory or the extension: `target/` full of `.node` files and a
/// stray `.obj` elsewhere are the same judgment from different evidence.
fn is_build_context(state: &FileState) -> bool {
    const DIRS: &[&str] = &[
        "target",
        "node_modules",
        "debug",
        "release",
        "build",
        "dist",
        "__pycache__",
    ];
    const EXTS: &[&str] = &["obj", "o", "pyc", "class", "pdb"];
    DIRS.contains(&state.directory.to_ascii_lowercase().as_str())
        || EXTS.contains(&state.ext.as_str())
}

fn sparse_misranked(
    state: &FileState,
    _output: &ModelOutput,
    _now: i64,
    config: &RuleConfig,
) -> Option<Suggestion> {
    if !state.is_sparse_like() || state.logical < SPARSE_MIN_LOGICAL {
        return None;
    }
    let confidence = gate(0.85 * provenance_weight(state), config)?;
    Some(Suggestion {
        rule: "R4",
        rule_version: 1,
        confidence,
        kind: SuggestionKind::SparseMisranked {
            logical: state.logical,
            allocated: state.allocated,
        },
    })
}

fn stream_heavy(
    state: &FileState,
    _output: &ModelOutput,
    _now: i64,
    config: &RuleConfig,
) -> Option<Suggestion> {
    if state.stream_bytes < STREAM_MIN_BYTES {
        return None;
    }
    let confidence = gate(0.85 * provenance_weight(state), config)?;
    Some(Suggestion {
        rule: "R5",
        rule_version: 1,
        confidence,
        kind: SuggestionKind::StreamHeavy {
            stream_bytes: state.stream_bytes,
            stream_count: state.stream_count,
        },
    })
}

/// Run every enabled rule over one file's state and factors, in rule order.
pub fn suggest(
    state: &FileState,
    output: &ModelOutput,
    now: i64,
    config: &RuleConfig,
) -> Vec<Suggestion> {
    RULES
        .iter()
        .filter(|rule| config.is_enabled(rule.id))
        .filter_map(|rule| (rule.check)(state, output, now, config))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, HeuristicEngine};
    use hfs_core::FileProvenance;

    const NOW: i64 = 134_000_000_000_000_000;
    const DAY_TICKS: i64 = 86_400 * 10_000_000;
    const GB: u64 = 1 << 30;

    fn judged(state: &FileState, config: &RuleConfig) -> Vec<Suggestion> {
        let output = HeuristicEngine.ask(state, NOW);
        suggest(state, &output, NOW, config)
    }

    fn stale_iso() -> FileState {
        FileState::assemble("backup.iso", 4 * GB, 4 * GB, 0, 0, 1, FileProvenance::Full)
            .with_location("data", NOW - 400 * DAY_TICKS)
    }

    #[test]
    fn stale_archives_are_suggested_with_evidence() {
        let suggestions = judged(&stale_iso(), &RuleConfig::default());
        assert_eq!(suggestions.len(), 1);
        let suggestion = &suggestions[0];
        assert_eq!((suggestion.rule, suggestion.rule_version), ("R1", 1));
        assert!(suggestion.confidence >= 0.5);
        match suggestion.kind {
            SuggestionKind::StaleArchive {
                allocated,
                age_days,
            } => {
                assert_eq!(allocated, 4 * GB);
                assert!(age_days >= 399);
            }
            ref other => panic!("expected StaleArchive, got {other:?}"),
        }
    }

    #[test]
    fn small_fresh_or_unmeasured_archives_stay_silent() {
        let config = RuleConfig::default();
        // Fresh: the staleness factor points the other way.
        let fresh =
            FileState::assemble("backup.iso", 4 * GB, 4 * GB, 0, 0, 1, FileProvenance::Full)
                .with_location("data", NOW - 2 * DAY_TICKS);
        assert!(judged(&fresh, &config).is_empty());
        // Small: below the floor, whatever its age.
        let small =
            FileState::assemble("old.zip", 10 << 20, 10 << 20, 0, 0, 1, FileProvenance::Full)
                .with_location("data", NOW - 400 * DAY_TICKS);
        assert!(judged(&small, &config).is_empty());
        // Denied: never measured, never suggested.
        let denied = FileState::assemble(
            "backup.iso",
            4 * GB,
            4 * GB,
            0,
            0,
            1,
            FileProvenance::Denied,
        )
        .with_location("data", NOW - 400 * DAY_TICKS);
        assert!(judged(&denied, &config).is_empty());
    }

    #[test]
    fn switched_off_rules_retract_exactly_their_suggestions() {
        let config = RuleConfig {
            without: vec!["R1".to_string()],
            ..Default::default()
        };
        assert!(judged(&stale_iso(), &config).is_empty());
    }

    #[test]
    fn sparse_and_shared_files_are_called_out() {
        let config = RuleConfig::default();
        let sparse =
            FileState::assemble("disk.vhdx", 100 * GB, 2 * GB, 0, 0, 1, FileProvenance::Full)
                .with_location("vms", NOW - 10 * DAY_TICKS);
        let rules: Vec<&str> = judged(&sparse, &config).iter().map(|s| s.rule).collect();
        // Fresh, so not stale — but the ranking warning still fires.
        assert_eq!(rules, vec!["R4"]);

        let shared =
            FileState::assemble("setup.exe", 2 * GB, 2 * GB, 0, 0, 3, FileProvenance::Full)
                .with_location("downloads", NOW - 10 * DAY_TICKS);
        let rules: Vec<&str> = judged(&shared, &config).iter().map(|s| s.rule).collect();
        assert_eq!(rules, vec!["R2"]);
    }

    #[test]
    fn build_output_and_heavy_streams_fire() {
        let config = RuleConfig::default();
        let built = FileState::assemble(
            "app.node",
            500 << 20,
            500 << 20,
            0,
            0,
            1,
            FileProvenance::Full,
        )
        .with_location("target", NOW - 10 * DAY_TICKS);
        let rules: Vec<&str> = judged(&built, &config).iter().map(|s| s.rule).collect();
        assert_eq!(rules, vec!["R3"]);

        let streamed = FileState::assemble(
            "data.bin",
            1 * GB,
            1 * GB,
            200 << 20,
            2,
            1,
            FileProvenance::Full,
        )
        .with_location("data", NOW - 10 * DAY_TICKS);
        let rules: Vec<&str> = judged(&streamed, &config).iter().map(|s| s.rule).collect();
        assert_eq!(rules, vec!["R5"]);
    }
}
