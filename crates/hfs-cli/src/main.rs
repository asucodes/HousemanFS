//! Command-line entry point for HousemanFS.
//!
//! Every command in this release is read-only. There is no verb that modifies a volume, and
//! none is reachable from here.

use std::process::ExitCode;

use hfs_core::{Accounting, Denied, Reconciliation, reconcile};
use hfs_win::{WalkSummary, volume_info, walk};

const USAGE: &str = "\
housemanfs — read-only filesystem accounting

usage:
  housemanfs info <path>    volume facts for the volume containing <path>
  housemanfs scan <path>    walk <path> and report what occupies space

Scanning a volume root additionally reconciles the result against what the
filesystem says is in use, and reports what could not be accounted for.

Both commands are read-only. Nothing here modifies a volume.";

/// Tolerance for the residual, as a fraction of volume capacity.
///
/// Not zero, because a real volume always holds things a directory walk cannot see: filesystem
/// metadata, alternate data streams, and paths the caller may not read. The point is not to
/// eliminate the residual but to keep it small and to explain it.
const RESIDUAL_TOLERANCE: f64 = 0.02;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let result = match (args.first().map(String::as_str), args.get(1)) {
        (Some("info"), Some(path)) => info(path),
        (Some("scan"), Some(path)) => scan(path),
        _ => {
            println!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn info(path: &str) -> Result<(), String> {
    let v = volume_info(path).map_err(|e| e.to_string())?;

    println!("volume      {}", v.root);
    println!("filesystem  {}", v.filesystem);
    println!("serial      {:08X}", v.serial);
    println!("volume id   {}", v.volume_id);
    println!("cluster     {} bytes", v.cluster_bytes);
    println!("capacity    {}", human(v.total_bytes));
    println!("free        {}", human(v.free_bytes));
    println!("available   {}", human(v.available_bytes));
    println!(
        "used        {}",
        human(v.total_bytes.saturating_sub(v.free_bytes))
    );

    if v.available_bytes != v.free_bytes {
        println!();
        println!("note: free and available differ, which usually means quotas are in force.");
        println!("      the two are reported separately rather than conflated.");
    }

    Ok(())
}

fn scan(path: &str) -> Result<(), String> {
    let result = walk(path).map_err(|e| e.to_string())?;
    let s = &result.summary;

    println!("scanned     {path}");
    println!();
    println!("files       {}", s.files);
    println!("directories {}", s.directories);
    println!("reparse     {}", s.reparse_points);
    println!("unreadable  {}", s.denied);
    println!();
    println!("logical     {}", human(s.logical_bytes));
    println!("allocated   {}", human(s.allocated_bytes));
    println!("slack       {}", human(s.slack_bytes));

    if s.hardlinked_files > 0 {
        println!();
        println!(
            "hardlinks   {} files across {} groups",
            s.hardlinked_files, s.hardlink_groups
        );
        println!(
            "            {} extra names contributing no storage",
            s.hardlink_extra_names
        );
        println!();
        println!("note: each group is counted once. Counting every name would overcount the");
        println!("      volume and make the excess look reclaimable, which it is not.");
    }

    if s.unknown_identity_files > 0 {
        println!();
        println!(
            "note: {} files could not be identified, so any hardlink group involving them",
            s.unknown_identity_files
        );
        println!("      may have been counted more than once. Treat the total as approximate.");
    }

    if s.denied > 0 {
        println!();
        println!(
            "note: {} paths could not be read, so the totals above are incomplete.",
            s.denied
        );
    }

    // A residual only means anything if the scan covered a whole volume. Comparing a subtree's
    // size against the volume's used space would produce a meaningless number, so it is not
    // attempted.
    if is_volume_root(path) {
        reconcile_volume(path, s)?;
    } else {
        println!();
        println!("note: this was a partial scan. Volume reconciliation is only meaningful for");
        println!("      a whole volume, so it is not attempted here.");
    }

    Ok(())
}

/// Compare what we attributed against what the filesystem says is in use.
///
/// The residual is reported, not hidden. A tool that silently shows a large unexplained gap is
/// not trustworthy; one that names the gap and its likely causes is telling the truth, and the
/// truth is checkable.
fn reconcile_volume(path: &str, s: &WalkSummary) -> Result<(), String> {
    let v = volume_info(path).map_err(|e| e.to_string())?;

    let accounting = Accounting {
        total: v.total_bytes,
        free: v.free_bytes,
        // Alternate data streams and directory overhead are not yet enumerated, and filesystem
        // metadata is not measurable from a directory walk. Left as zero and `None` so the
        // residual absorbs them honestly rather than being flattered by an estimate.
        file_bytes: s.allocated_bytes,
        stream_bytes: 0,
        directory_bytes: 0,
        known_metadata: None,
        denied: Denied::new(s.denied),
    };

    let residual = accounting.residual();
    let ratio = accounting.residual_ratio();

    println!();
    println!("--- reconciliation ---");
    println!("capacity    {}", human(accounting.total));
    println!("used        {}", human(accounting.used()));
    println!("attributed  {}", human(accounting.attributed()));
    println!("unaccounted {}", human(residual));
    println!("            {:.2}% of capacity", ratio * 100.0);

    println!();
    println!("not yet measured, and therefore part of the figure above:");
    println!("  filesystem metadata   MFT, journals, bitmaps — not visible to a directory walk");
    println!("  alternate streams     not yet enumerated");
    println!("  directory overhead    not yet enumerated");
    if s.denied > 0 {
        println!("  unreadable paths      {} paths", s.denied);
    }

    match reconcile(&accounting, RESIDUAL_TOLERANCE) {
        Reconciliation::Explained { .. } => {
            println!();
            println!(
                "within the {:.0}% tolerance used by this build.",
                RESIDUAL_TOLERANCE * 100.0
            );
        }
        Reconciliation::Unexplained {
            unreadable_paths, ..
        } => {
            println!();
            println!(
                "beyond the {:.0}% tolerance used by this build.",
                RESIDUAL_TOLERANCE * 100.0
            );
            if unreadable_paths > 0 {
                println!(
                    "{unreadable_paths} paths were unreadable, which is the first thing to investigate."
                );
            }
            println!("This is a finding, not an error: the accounting is incomplete and says so.");
        }
    }

    Ok(())
}

/// Whether a path refers to a whole volume, e.g. `C:` or `D:\`.
fn is_volume_root(path: &str) -> bool {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let bytes = trimmed.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Format a byte count using binary units, labelled honestly.
///
/// Windows displays binary units but labels them GB/TB, which is why a "500 GB" drive shows as
/// 465 GB. Labelling them GiB costs nothing and avoids repeating that confusion.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {} ({bytes} bytes)", UNITS[unit])
    }
}
