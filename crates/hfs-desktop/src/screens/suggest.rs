//! The suggest screen: judgment with evidence, plus the baseline's score.
//!
//! The same five versioned rules the CLI runs, over the same baseline engine, with the
//! same gate — thresholds, switches and floors as widgets instead of flags. Every
//! suggestion names its rule, version, confidence and measured evidence, and clicking one
//! selects the file for the evidence panel. Below the gate the screen says nothing, and
//! says so, exactly like the terminal.

use hfs_core::format::{human_bytes, parse_byte_size};
use hfs_model::{Engine, FileState, HeuristicEngine, RULES, RuleConfig, suggest};

use super::browse::row;
use crate::{App, SuggestRow};

const RULE_IDS: [&str; 5] = ["R1", "R2", "R3", "R4", "R5"];

impl App {
    pub fn show_suggest(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Suggest");
            ui.small(format!(
                "baseline scores {}/{} on the committed set",
                self.eval_report.passed,
                self.eval_report.passed + self.eval_report.failed
            ));
        });
        ui.small("Judgment with evidence. Nothing here acts; the verbs are candidates, not instructions.");
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("Confidence");
            ui.add(egui::Slider::new(&mut self.threshold, 0.3..=0.95).text("gate"));
            ui.label("Min size");
            ui.text_edit_singleline(&mut self.min_bytes_text);
        });
        ui.horizontal(|ui| {
            ui.label("Rules");
            for (index, id) in RULE_IDS.iter().enumerate() {
                let description = RULES
                    .iter()
                    .find(|rule| rule.id == *id)
                    .map(|rule| rule.description)
                    .unwrap_or("");
                ui.checkbox(&mut self.without[index], *id)
                    .on_hover_text(format!("{id}: {description} (checked = off)"));
            }
            if ui.button("Judge largest 500").clicked() {
                self.run_suggest();
            }
        });
        if !self.suggest_ran {
            ui.small("Judges the 500 largest indexed files. Small files are noise whatever their confidence.");
            return;
        }
        if self.suggest_rows.is_empty() {
            ui.label("No suggestions above the gate. Silence is the correct output when nothing clears it.");
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for entry in self.suggest_rows.clone() {
                ui.horizontal(|ui| {
                    ui.monospace(format!("{:>12}", human_bytes(entry.allocated)));
                    let selected = self.selection.as_deref() == Some(entry.path.as_str());
                    if row(ui, &entry.path, selected) {
                        self.selection = Some(entry.path.clone());
                    }
                });
                ui.horizontal(|ui| {
                    ui.monospace(format!(
                        "  {} v{} ({}%)",
                        entry.rule,
                        entry.version,
                        (entry.confidence * 100.0) as u32
                    ));
                    ui.label(format!(
                        "{} — {}",
                        entry.verb,
                        join_evidence(&entry.evidence)
                    ));
                });
            }
        });
    }

    fn run_suggest(&mut self) {
        let Some(index) = &self.index else {
            return;
        };
        let without: Vec<String> = RULE_IDS
            .iter()
            .enumerate()
            .filter(|(index, _)| self.without[*index])
            .map(|(_, id)| id.to_string())
            .collect();
        let min_bytes =
            parse_byte_size(self.min_bytes_text.trim()).unwrap_or(RuleConfig::default().min_bytes);
        let config = RuleConfig {
            min_confidence: self.threshold as f64,
            min_bytes,
            without,
        };
        let engine = HeuristicEngine;
        let now = hfs_core::format::windows_ticks_now();
        let mut rows = Vec::new();
        if let Ok(hits) = index.largest_files(500) {
            for hit in &hits {
                let (name, directory) = split_name(&hit.path);
                let state = FileState::assemble(
                    &name,
                    hit.logical,
                    hit.allocated,
                    hit.stream_bytes,
                    hit.stream_count,
                    hit.links,
                    hit.provenance,
                )
                .with_location(&directory, hit.modified);
                let output = engine.ask(&state, now);
                for suggestion in suggest(&state, &output, now, &config) {
                    rows.push(SuggestRow {
                        path: hit.path.clone(),
                        allocated: hit.allocated,
                        rule: suggestion.rule.to_string(),
                        version: suggestion.rule_version,
                        confidence: suggestion.confidence,
                        verb: suggestion.kind.verb().to_string(),
                        evidence: suggestion
                            .kind
                            .evidence()
                            .into_iter()
                            .map(|(label, value)| (label.to_string(), value))
                            .collect(),
                    });
                }
            }
        }
        self.suggest_rows = rows;
        self.suggest_ran = true;
    }
}

fn join_evidence(evidence: &[(String, String)]) -> String {
    evidence
        .iter()
        .map(|(label, value)| format!("{label} {value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Split a full path into file name and parent directory name. Names, not identities:
/// enough context to judge, nothing to memorise.
fn split_name(path: &str) -> (String, String) {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let mut parts = trimmed.rsplit(['\\', '/']);
    let name = parts.next().unwrap_or("").to_string();
    let directory = parts.next().unwrap_or("").to_string();
    let directory = if directory.len() == 2 && directory.as_bytes()[1] == b':' {
        String::new()
    } else {
        directory
    };
    (name, directory)
}
