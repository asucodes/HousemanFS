//! Building and querying the index.
//!
//! A scan takes minutes; a finder has to answer in milliseconds. The index is what separates
//! the two, and this module is the bridge between the platform layer and it.

use std::fmt::Write as _;
use std::path::Path;

use hfs_core::FileProvenance;
use hfs_core::{human_age, human_bytes_exact as human, parse_byte_size, windows_ticks_now};
use hfs_index::{Index, RefreshReport};
use hfs_win::volume_info;

/// Where the index lives unless told otherwise.
pub const DEFAULT_INDEX: &str = "housemanfs.db";

/// Scan `path` and store the result, replacing whatever was there.
pub fn build(path: &str, db: &str, out: &mut String) -> Result<(), String> {
    let _ = writeln!(out, "index       {db}");
    let scan = collect(path, out)?;

    let mut index = Index::open(db).map_err(|e| e.to_string())?;
    index
        .write_scan(&scan.meta, scan.dirs.iter(), scan.files.iter())
        .map_err(|e| e.to_string())?;

    print_stored(out, &scan);
    Ok(())
}

/// Re-scan and fold the result into the existing index, touching only what changed.
///
/// Refuses to run against an index built from a different volume: volume identity survives
/// remounting, so a mismatch means the index is not this path's past, and merging them
/// would corrupt both histories. Rebuild instead. The check runs before the walk, because a
/// refused refresh must not cost a full scan.
pub fn refresh(db: &str, path: &str, out: &mut String) -> Result<(), String> {
    let mut index = open(db)?;
    let stored = index.scan_metadata();
    let root = if path.is_empty() {
        stored.root.clone()
    } else {
        path.to_string()
    };
    if root.is_empty() {
        return Err("no path to refresh: pass one explicitly".to_string());
    }

    let live = volume_info(&root).map_err(|e| e.to_string())?;
    if *live.volume_id.as_bytes() != stored.volume_id && stored.volume_id != [0u8; 16] {
        return Err(format!(
            "index covers volume {} but {root} is a different volume; rebuild with `index`",
            stored.root
        ));
    }

    let scan = collect(&root, out)?;
    let report = index
        .refresh_scan(
            &scan.meta,
            scan.dirs.iter(),
            scan.files.iter(),
            windows_ticks_now(),
        )
        .map_err(|e| e.to_string())?;

    print_refresh(out, &report);
    Ok(())
}

/// Walk `path` and convert the entries, printing what the terminal needs to hear.
///
/// The conversion itself lives in `hfs_import`, shared with every other frontend: both
/// commands must agree on how entries become rows, or their results would silently
/// diverge. This function only walks, reports progress in words, and hands rows over.
fn collect(path: &str, out: &mut String) -> Result<hfs_import::ConvertedScan, String> {
    let _ = writeln!(out, "scanning    {path}");
    let mut control = hfs_win::WalkControl::default();
    let scan = hfs_import::scan_path(path, &mut control)?;
    if !scan.backup.is_active() {
        let _ = writeln!(
            out,
            "privilege   no backup privilege — protected files are sized by report, not allocation"
        );
    }
    Ok(scan)
}

/// What a full build stored, in the words the build reports.
fn print_stored(out: &mut String, scan: &hfs_import::ConvertedScan) {
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "stored      {} directories, {} files",
        scan.dirs.len(),
        scan.files.len()
    );
    let _ = writeln!(
        out,
        "occupying   {} of file data",
        human(scan.summary.allocated_bytes)
    );
    if scan.summary.stream_count > 0 {
        let _ = writeln!(
            out,
            "            {} in {} named {}",
            human(scan.summary.stream_bytes),
            scan.summary.stream_count,
            plural(scan.summary.stream_count, "stream", "streams"),
        );
    }
    if scan.summary.denied_directories > 0 {
        let _ = writeln!(
            out,
            "note        {} directories were unreadable and are absent",
            scan.summary.denied_directories
        );
    }
}

/// What a refresh changed. Only nonzero lines print: a quiet refresh that found nothing is
/// the common case, and a wall of zeros would bury the days when something moved.
fn print_refresh(out: &mut String, report: &RefreshReport) {
    let _ = writeln!(out);
    let _ = writeln!(out, "refresh     complete");
    let files = report.added_files + report.removed_files + report.modified_files;
    if files == 0 && report.moved_files == 0 {
        let _ = writeln!(out, "            nothing changed");
    }
    if report.added_files > 0 {
        let _ = writeln!(out, "added       {} files", report.added_files);
    }
    if report.removed_files > 0 {
        let _ = writeln!(out, "removed     {} files", report.removed_files);
    }
    if report.modified_files > 0 {
        let _ = writeln!(out, "modified    {} files", report.modified_files);
    }
    if report.moved_files > 0 {
        let _ = writeln!(
            out,
            "moved       {} files (same data, new location)",
            report.moved_files
        );
    }
    if report.added_dirs + report.removed_dirs > 0 {
        let _ = writeln!(
            out,
            "directories +{} -{}",
            report.added_dirs, report.removed_dirs
        );
    }
    if report.stale_files + report.stale_dirs > 0 {
        let _ = writeln!(
            out,
            "note        {} files and {} directories were not rescanned and are kept as-is",
            report.stale_files, report.stale_dirs
        );
    }
}

/// The largest files by occupied size.
pub fn top(db: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let rows = index.largest_files(limit).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "largest files");
    let _ = writeln!(out);
    for hit in rows {
        let _ = writeln!(
            out,
            "{:>12}  {}",
            human(hit.allocated),
            truncate(&hit.path, 100)
        );
        if hit.links > 1 {
            let _ = writeln!(out, "             {} names share this data", hit.links);
        }
        write_stream_note(out, hit.stream_bytes, hit.stream_count);
    }
    Ok(())
}

/// The largest directory subtrees.
pub fn dirs(db: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let rows = index.largest_dirs(limit).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "largest directories");
    let _ = writeln!(out);
    for hit in rows {
        let _ = writeln!(
            out,
            "{:>12}  {:>7} {}  {}",
            human(hit.allocated),
            hit.files,
            noun(hit.files),
            truncate(&hit.path, 80)
        );
        if hit.streams > 0 {
            let _ = writeln!(
                out,
                "             including {} in named streams",
                human(hit.streams)
            );
        }
    }
    Ok(())
}

/// Files whose name contains a term.
pub fn search(db: &str, term: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let rows = index.search_name(term, limit).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "matching {term:?}");
    let _ = writeln!(out);
    if rows.is_empty() {
        let _ = writeln!(out, "nothing found");
        return Ok(());
    }
    for hit in rows {
        let _ = writeln!(
            out,
            "{:>12}  {}",
            human(hit.allocated),
            truncate(&hit.path, 100)
        );
        write_stream_note(out, hit.stream_bytes, hit.stream_count);
    }
    Ok(())
}

/// The measurement behind one indexed path.
///
/// This is the provenance promise made concrete: for any figure the index reports, show
/// which measurement produced it — and say plainly which parts were never measured.
pub fn explain(db: &str, path: &str, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let scan = index.scan_metadata();

    let Some(record) = index.explain_path(path).map_err(|e| e.to_string())? else {
        let _ = writeln!(out, "{path} is not in the index");
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "the index covers {} and nothing else; a path outside it was never scanned.",
            if scan.root.is_empty() {
                "nothing".to_string()
            } else {
                scan.root.clone()
            }
        );
        return Ok(());
    };

    let _ = writeln!(out, "{}", record.path);
    let _ = writeln!(out, "kind        {}", record.kind);
    if record.kind == "file" {
        let _ = writeln!(out, "reported    {}", human(record.logical));
    }
    let _ = writeln!(
        out,
        "occupies    {} ({})",
        human(record.allocated),
        if record.provenance.allocation_measured() {
            "measured"
        } else {
            "reported, not measured"
        }
    );
    if record.links > 1 {
        let _ = writeln!(
            out,
            "identity    {} names share this data; counted once",
            record.links
        );
    }
    if record.kind == "file" {
        // An age, not a verdict. Whether old means "abandoned" is a model factor with a
        // confidence attached, not something the index declares.
        let _ = writeln!(
            out,
            "modified    {}",
            human_age(record.modified, windows_ticks_now())
        );
    }
    match (record.stream_count, record.provenance) {
        (0, FileProvenance::Full | FileProvenance::NoIdentity) => {
            let _ = writeln!(out, "streams     none — measured");
        }
        (0, _) => {
            let _ = writeln!(out, "streams     unknown — never enumerated for this entry");
        }
        (n, _) => {
            let _ = writeln!(
                out,
                "streams     {} across {} {} — measured on allocation",
                human(record.stream_bytes),
                n,
                plural(u64::from(n), "stream", "streams"),
            );
        }
    }
    let _ = writeln!(out, "measured by {}", record.provenance.describe());
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "indexed from {} ({}), scanned as {} with backup privilege {}; {} {} unreadable there.",
        scan.root,
        scan.filesystem,
        if scan.elevated {
            "elevated"
        } else {
            "standard user"
        },
        if scan.backup_privilege.is_empty() {
            "unknown"
        } else {
            &scan.backup_privilege
        },
        scan.denied_directories.saturating_add(scan.denied_files),
        plural(
            scan.denied_directories.saturating_add(scan.denied_files),
            "path was",
            "paths were"
        ),
    );
    Ok(())
}

/// The index as versioned JSON: the extension surface.
///
/// Anything may build on this output in any language, without running code inside a process
/// that can read the whole disk. The contract is versioned (`contract_version`) separately
/// from the storage schema (`schema_version`): the storage may change how it keeps facts,
/// but this shape changes only deliberately. This is a snapshot — the largest lists are
/// capped by `--limit` — not a full dump; it answers "what does the index say", not "give
/// me every row".
pub fn export(db: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let document = index.export_snapshot(limit).map_err(|e| e.to_string())?;

    out.push_str(&serde_json::to_string_pretty(&document).map_err(|e| e.to_string())?);
    out.push('\n');
    Ok(())
}

/// One line about named streams, printed only when there is something to say.
fn write_stream_note(out: &mut String, bytes: u64, count: u32) {
    if count > 0 {
        let _ = writeln!(
            out,
            "             plus {} in {} named {}",
            human(bytes),
            count,
            plural(u64::from(count), "stream", "streams"),
        );
    }
}

/// Split `>=500MB` into its operator and amount.
fn split_comparison(rest: &str) -> Option<(&str, &str)> {
    for op in [">=", "<=", ">", "<"] {
        if let Some(amount) = rest.strip_prefix(op) {
            if !amount.is_empty() {
                return Some((op, amount));
            }
        }
    }
    None
}

/// Parse `30d` into Windows ticks. Months are 30 days and years 365: approximate, and the
/// approximation is stated rather than hidden behind calendar arithmetic nobody asked for.
fn parse_age(amount: &str) -> Option<i64> {
    const TICK: i64 = 10_000_000;
    const DAY: i64 = 86_400 * TICK;
    let lower = amount.to_ascii_lowercase();
    // Leading digits, trailing unit. A missing unit is not an age — `30` alone could be
    // days, seconds or nothing, and guessing would answer a different question.
    let split = lower.find(|c: char| !c.is_ascii_digit())?;
    let (digits, unit) = lower.split_at(split);
    let days_per: i64 = match unit {
        "d" | "day" | "days" => 1,
        "w" | "week" | "weeks" => 7,
        "m" | "month" | "months" => 30,
        "y" | "year" | "years" => 365,
        _ => return None,
    };
    digits
        .parse::<i64>()
        .ok()?
        .checked_mul(days_per)?
        .checked_mul(DAY)
}

/// Kind words with fixed, documented meanings. A closed list, shown in full in the usage
/// text: adding a kind changes what questions can be asked, so it happens here in the
/// open, not inside a model where nobody can see it.
fn kind_alias(word: &str) -> Option<&'static [&'static str]> {
    match word {
        "video" | "videos" => Some(&["mp4", "mkv", "avi", "mov", "wmv", "webm"]),
        "audio" | "music" => Some(&["mp3", "flac", "wav", "ogg", "m4a"]),
        "image" | "images" | "photo" | "photos" => {
            Some(&["jpg", "jpeg", "png", "gif", "bmp", "tiff", "webp", "raw"])
        }
        "archive" | "archives" => Some(&["zip", "7z", "rar", "tar", "gz"]),
        "installer" | "installers" => Some(&["exe", "msi"]),
        "disk" | "disks" | "iso" | "isos" | "image-file" => Some(&["iso", "img", "vhd", "vhdx"]),
        _ => None,
    }
}

/// Pick the singular or plural form. A report that says "1 streams" is a small
/// carelessness, and carelessness compounds in a tool that asks to be trusted.
pub(crate) fn plural(count: u64, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

/// Answer a structured question over the index.
///
/// The query language is small and deterministic: size, extension, name, age, sharing and
/// streams, in any order. It is pattern matching, not understanding — ask "old isos"
/// works because "old" and "isos" are known words, not because anything comprehended the
/// sentence. A future language-model frontend would compile into [`AskQuery`], which bounds
/// what it can express; until then, this parser is the whole of "ask".
pub fn ask(db: &str, raw: &str, out: &mut String) -> Result<(), String> {
    let query = parse_ask(raw)?;
    let index = open(db)?;
    let rows = index.ask(&query).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "asking {raw:?}");
    let _ = writeln!(out);
    if rows.is_empty() {
        let _ = writeln!(out, "nothing matches");
        return Ok(());
    }
    for hit in rows {
        let _ = writeln!(
            out,
            "{:>12}  {:>12}  {}",
            human(hit.allocated),
            human_age(hit.modified, windows_ticks_now()),
            truncate(&hit.path, 90)
        );
        if hit.links > 1 {
            let _ = writeln!(out, "             {} names share this data", hit.links);
        }
        write_stream_note(out, hit.stream_bytes, hit.stream_count);
    }
    Ok(())
}

/// Compile a question into the typed query. Unknown words are an error naming what is
/// understood, because silently dropping a clause would answer a different question than
/// the one asked — the same policy as the LIKE escaping: never reinterpret input quietly.
fn parse_ask(raw: &str) -> Result<hfs_index::AskQuery, String> {
    const HELP: &str = "try: size>500MB ext:iso older than 1y shared streams name:backup limit 10";
    let mut query = hfs_index::AskQuery {
        limit: 20,
        ..Default::default()
    };
    let words: Vec<&str> = raw.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        let lower = word.to_ascii_lowercase();
        if lower == "largest" || lower == "biggest" {
            // Accepted and ignored: results always arrive largest first.
        } else if lower == "shared" {
            query.shared_only = true;
        } else if lower == "streams" {
            query.streams_only = true;
        } else if let Some(rest) = lower.strip_prefix("size") {
            let (op, amount) = split_comparison(rest).ok_or_else(|| {
                format!("don't understand {word:?}. Size looks like size>500MB. {HELP}")
            })?;
            let bytes = parse_byte_size(amount).ok_or_else(|| {
                format!("don't understand {amount:?}. Sizes look like 500MB or 2GB. {HELP}")
            })?;
            match op {
                ">" | ">=" => query.min_allocated = Some(bytes),
                "<" | "<=" => query.max_allocated = Some(bytes),
                _ => return Err(format!("don't understand {word:?}. {HELP}")),
            }
        } else if let Some(rest) = lower.strip_prefix("ext:") {
            for ext in rest.split(',') {
                let clean = ext.trim().trim_start_matches('.').to_string();
                if clean.is_empty() {
                    return Err(format!("don't understand {word:?}. {HELP}"));
                }
                query.exts.push(clean);
            }
        } else if let Some(rest) = word.strip_prefix("name:") {
            if rest.is_empty() {
                return Err(format!("don't understand {word:?}. {HELP}"));
            }
            query.name = Some(rest.to_string());
        } else if lower == "older" || lower == "newer" {
            let direction = lower.as_str();
            let than = words.get(i + 1).copied().unwrap_or("");
            let amount = words.get(i + 2).copied().unwrap_or("");
            if than != "than" || amount.is_empty() {
                return Err(format!(
                    "don't understand {word:?}. Age looks like `older than 30d`. {HELP}"
                ));
            }
            let ticks = parse_age(amount).ok_or_else(|| {
                format!("don't understand {amount:?}. Ages look like 30d, 6m or 2y. {HELP}")
            })?;
            let cutoff = windows_ticks_now() - ticks;
            if direction == "older" {
                query.older_than = Some(cutoff);
            } else {
                query.newer_than = Some(cutoff);
            }
            i += 2;
        } else if lower == "limit" {
            let amount = words.get(i + 1).copied().unwrap_or("");
            query.limit = amount
                .parse()
                .map_err(|_| format!("don't understand {amount:?} as a limit. {HELP}"))?;
            i += 1;
        } else if let Some(exts) = kind_alias(&lower) {
            query.exts.extend(exts.iter().map(|s| s.to_string()));
        } else if i == words.len() - 1 && lower.parse::<u32>().is_ok() {
            query.limit = lower.parse().unwrap_or(20);
        } else {
            return Err(format!("don't understand {word:?}. {HELP}"));
        }
        i += 1;
    }
    Ok(query)
}
/// Judge the largest indexed files: per-file factors plus versioned rules.
///
/// Each suggestion names its rule, its confidence and the measured facts behind it; below
/// the threshold the tool says nothing, and says so. This is judgment with evidence, not
/// optimization: nothing here acts, and the verbs are candidates, not instructions.
pub fn suggest(
    db: &str,
    limit: u32,
    threshold: Option<&str>,
    without: Option<&str>,
    min_bytes: Option<&str>,
    out: &mut String,
) -> Result<(), String> {
    use hfs_model::{Engine, FileState, HeuristicEngine, RuleConfig, suggest};

    let config = RuleConfig {
        min_confidence: match threshold {
            None => RuleConfig::default().min_confidence,
            Some(raw) => raw
                .parse::<f64>()
                .map_err(|_| format!("don't understand {raw:?} as a confidence; try 0.5"))?,
        },
        min_bytes: match min_bytes {
            None => RuleConfig::default().min_bytes,
            Some(raw) => parse_byte_size(raw)
                .ok_or_else(|| format!("don't understand {raw:?} as a size; try 1GB"))?,
        },
        without: without
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|rule| !rule.is_empty())
            .map(str::to_string)
            .collect(),
    };

    let index = open(db)?;
    let total = index.summary().map_err(|e| e.to_string())?.files;
    let hits = index.largest_files(limit).map_err(|e| e.to_string())?;
    let engine = HeuristicEngine;
    let now = windows_ticks_now();

    let _ = writeln!(
        out,
        "judging     {} of {} indexed files with heuristic-v1, confidence >= {}%",
        hits.len(),
        total,
        (config.min_confidence * 100.0) as u32
    );
    let _ = writeln!(out);

    let mut shown = 0;
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
        let suggestions = suggest(&state, &output, now, &config);
        if suggestions.is_empty() {
            continue;
        }
        shown += 1;
        let _ = writeln!(out, "{}  {}", human(hit.allocated), truncate(&hit.path, 90));
        for suggestion in &suggestions {
            let _ = writeln!(
                out,
                "  {} v{} ({}%): {} — {}",
                suggestion.rule,
                suggestion.rule_version,
                (suggestion.confidence * 100.0) as u32,
                suggestion.kind.verb(),
                evidence(&suggestion.kind),
            );
        }
    }

    if shown == 0 {
        let _ = writeln!(
            out,
            "no suggestions above the gate. Silence is the correct output when nothing \
             clears it."
        );
    }
    Ok(())
}

/// The measured facts one suggestion stands on, in words: the shared evidence pairs
/// joined into the CLI's one-line rendering.
fn evidence(kind: &hfs_model::SuggestionKind) -> String {
    kind.evidence()
        .iter()
        .map(|(label, value)| format!("{label} {value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Split a full path into its file name and parent directory name. The model gets names,
/// not identities: enough context to judge, nothing to memorise.
fn split_name(path: &str) -> (String, String) {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let mut parts = trimmed.rsplit(['\\', '/']);
    let name = parts.next().unwrap_or("").to_string();
    let directory = parts.next().unwrap_or("").to_string();
    // A file on a volume root has `C:` as its parent; the drive letter is not a directory
    // in any sense the rules care about.
    let directory = if directory.len() == 2 && directory.as_bytes()[1] == b':' {
        String::new()
    } else {
        directory
    };
    (name, directory)
}

/// Run the committed evaluation set and report the score.
///
/// The same set the test suite enforces, printed for humans: what was expected and where
/// they differ. A nonzero failure count fails the command, so CI can run the tool
/// against itself.
pub fn eval(out: &mut String) -> Result<(), String> {
    use hfs_model::{EVAL_CASES, RuleConfig, eval_engine, run_eval};

    let report = run_eval(&eval_engine(), &RuleConfig::default());
    let _ = writeln!(
        out,
        "evaluation heuristic-v1 vs {} committed cases",
        EVAL_CASES.len()
    );
    let _ = writeln!(out);
    if report.failures.is_empty() {
        let _ = writeln!(out, "{} passed, 0 failed", report.passed);
        Ok(())
    } else {
        for failure in &report.failures {
            let _ = writeln!(out, " FAIL {failure}");
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "{} passed, {} failed", report.passed, report.failed);
        Err(format!("{} evaluation cases failed", report.failed))
    }
}

/// Totals by extension.
pub fn extensions(db: &str, limit: u32, out: &mut String) -> Result<(), String> {
    let index = open(db)?;
    let rows = index.by_extension(limit).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "occupied by extension");
    let _ = writeln!(out);
    for (ext, allocated, count) in rows {
        let label = if ext.is_empty() {
            "(none)".to_string()
        } else {
            ext
        };
        let _ = writeln!(
            out,
            "{:>12}  {:>7} {}  {}",
            human(allocated),
            count,
            noun(count),
            label
        );
    }
    Ok(())
}

fn open(db: &str) -> Result<Index, String> {
    if !Path::new(db).exists() {
        return Err(format!(
            "no index at {db}. Build one with: housemanfs index <path>"
        ));
    }
    Index::open(db).map_err(|e| e.to_string())
}

/// "file" or "files", so a single result does not read as "1 files".
fn noun(count: u64) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

/// Shorten a path for display, keeping the beginning and the end.
///
/// Deep paths are common and the middle is rarely the interesting part.
fn truncate(path: &str, limit: usize) -> String {
    if path.chars().count() <= limit {
        return path.to_string();
    }
    let keep = limit - 3;
    let head = keep / 2;
    let tail = keep - head;
    let start: String = path.chars().take(head).collect();
    let end: String = path.chars().rev().take(tail).collect::<String>();
    let end: String = end.chars().rev().collect();
    format!("{start}...{end}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hfs_core::TICKS_PER_SECOND;

    #[test]
    fn ages_read_as_ages() {
        assert_eq!(human_age(0, 0), "unknown");
        assert_eq!(human_age(-5, 0), "unknown");
        let now = windows_ticks_now();
        // A file written this instant is seconds old, never from the future.
        assert!(human_age(now - 30 * TICKS_PER_SECOND, now).starts_with("30 second"));
        assert!(human_age(now - 3 * 86_400 * TICKS_PER_SECOND, now).starts_with("3 day"));
        assert!(human_age(now - 400 * 86_400 * TICKS_PER_SECOND, now).starts_with("1 year"));
    }

    #[test]
    fn the_clock_keeps_sub_second_precision() {
        // Two consecutive reads must not go backwards, and the clock must resolve below
        // one second — truncation once made every fresh file read as future-dated.
        let first = windows_ticks_now();
        let second = windows_ticks_now();
        assert!(second >= first);
        assert!(second % TICKS_PER_SECOND != 0 || first % TICKS_PER_SECOND != 0);
    }

    #[test]
    fn ask_parses_every_clause() {
        let query = parse_ask(
            "largest videos size>500MB ext:iso older than 30d shared streams name:backup limit 5",
        )
        .unwrap();
        assert_eq!(query.min_allocated, Some(500 * (1u64 << 20)));
        assert!(query.exts.contains(&"iso".to_string()));
        // The kind word contributes its extensions alongside the explicit one.
        assert!(query.exts.contains(&"mp4".to_string()));
        assert!(query.older_than.is_some());
        assert!(query.shared_only);
        assert!(query.streams_only);
        assert_eq!(query.name.as_deref(), Some("backup"));
        assert_eq!(query.limit, 5);
    }

    #[test]
    fn ask_rejects_what_it_cannot_answer() {
        assert!(parse_ask("color:red").is_err());
        assert!(parse_ask("size>big").is_err());
        assert!(parse_ask("older than").is_err());
        assert!(parse_ask("older next 30d").is_err());
    }

    #[test]
    fn ask_sizes_are_binary() {
        let query = parse_ask("size>=2GB size<1TB").unwrap();
        assert_eq!(query.min_allocated, Some(2 * (1u64 << 30)));
        assert_eq!(query.max_allocated, Some(1u64 << 40));
    }
}
