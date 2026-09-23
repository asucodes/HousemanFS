//! Command-line entry point for HousemanFS.
//!
//! Every command in this release is read-only. There is no verb that modifies a volume, and
//! none is reachable from here.

use std::fmt::Write as _;
use std::process::ExitCode;

use hfs_core::{Accounting, Denied, Reconciliation, reconcile};
use hfs_win::{WalkSummary, is_elevated, ntfs_metadata, volume_info, walk};

const USAGE: &str = "\
housemanfs — read-only filesystem accounting

usage:
  housemanfs info <path>    volume facts for the volume containing <path>
  housemanfs scan <path>    walk <path> and report what occupies space

options:
  --out <file>              write the report to a file instead of stdout

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
    let mut command: Option<String> = None;
    let mut path: Option<String> = None;
    let mut out_file: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out_file = args.next(),
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => {
                if command.is_none() {
                    command = Some(arg);
                } else if path.is_none() {
                    path = Some(arg);
                }
            }
        }
    }

    let (Some(command), Some(path)) = (command, path) else {
        println!("{USAGE}");
        return ExitCode::FAILURE;
    };

    // Validate the output path before doing any work. A whole-volume scan can take minutes, and
    // discovering afterwards that the path was unusable wastes the scan and loses the result.
    // Failing fast turns a lost report into an immediate, obvious error.
    if let Some(file) = &out_file {
        if let Err(err) = std::fs::File::create(file) {
            eprintln!("error: cannot write to {file}: {err}");
            eprintln!();
            eprintln!("note: %TEMP% is cmd syntax. In PowerShell use $env:TEMP, or pass a");
            eprintln!("      plain relative path such as --out report.txt");
            return ExitCode::FAILURE;
        }
    }

    let mut out = String::new();

    let result = match command.as_str() {
        "info" => info(&path, &mut out),
        "scan" => scan(&path, &mut out),
        _ => {
            println!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }

    match out_file {
        Some(file) => {
            // Writing to a file exists so that an elevated run — which gets its own console and
            // cannot have its output redirected by the caller — can still hand back a report.
            if let Err(err) = std::fs::write(&file, &out) {
                eprintln!("error: could not write {file}: {err}");
                return ExitCode::FAILURE;
            }
            println!("report written to {file}");
        }
        None => print!("{out}"),
    }

    ExitCode::SUCCESS
}

fn info(path: &str, out: &mut String) -> Result<(), String> {
    let v = volume_info(path).map_err(|e| e.to_string())?;

    let _ = writeln!(out, "volume      {}", v.root);
    let _ = writeln!(out, "filesystem  {}", v.filesystem);
    let _ = writeln!(out, "serial      {:08X}", v.serial);
    let _ = writeln!(out, "volume id   {}", v.volume_id);
    let _ = writeln!(out, "cluster     {} bytes", v.cluster_bytes);
    let _ = writeln!(out, "capacity    {}", human(v.total_bytes));
    let _ = writeln!(out, "free        {}", human(v.free_bytes));
    let _ = writeln!(out, "available   {}", human(v.available_bytes));
    let _ = writeln!(
        out,
        "used        {}",
        human(v.total_bytes.saturating_sub(v.free_bytes))
    );

    if v.available_bytes != v.free_bytes {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "note: free and available differ, which usually means quotas are in force."
        );
        let _ = writeln!(
            out,
            "      the two are reported separately rather than conflated."
        );
    }

    Ok(())
}

fn scan(path: &str, out: &mut String) -> Result<(), String> {
    let result = walk(path).map_err(|e| e.to_string())?;
    let s = &result.summary;

    let _ = writeln!(out, "scanned     {path}");
    let _ = writeln!(
        out,
        "privilege   {}",
        if is_elevated() {
            "elevated"
        } else {
            "standard user"
        }
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "files       {}", s.files);
    let _ = writeln!(out, "directories {}", s.directories);
    let _ = writeln!(out, "reparse     {}", s.reparse_points);
    let _ = writeln!(
        out,
        "unreadable  {} directories, {} files",
        s.denied_directories, s.denied_files
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "logical     {}", human(s.logical_bytes));
    let _ = writeln!(out, "allocated   {}", human(s.allocated_bytes));
    let _ = writeln!(out, "slack       {}", human(s.slack_bytes));

    if s.hardlinked_files > 0 {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "hardlinks   {} files across {} groups",
            s.hardlinked_files, s.hardlink_groups
        );
        let _ = writeln!(
            out,
            "            {} extra names contributing no storage",
            s.hardlink_extra_names
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "note: each group is counted once. Counting every name would overcount the"
        );
        let _ = writeln!(
            out,
            "      volume and make the excess look reclaimable, which it is not."
        );
    }

    if s.unknown_identity_files > 0 {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "note: {} files could not be identified, so any hardlink group involving them",
            s.unknown_identity_files
        );
        let _ = writeln!(
            out,
            "      may have been counted more than once. Treat the total as approximate."
        );
    }

    if s.denied_directories > 0 || s.denied_files > 0 {
        let _ = writeln!(out);
        if s.denied_directories > 0 {
            let _ = writeln!(
                out,
                "note: {} directories could not be opened. Everything beneath them is",
                s.denied_directories
            );
            let _ = writeln!(
                out,
                "      missing from the totals above, which is the likeliest home of any gap."
            );
        }
        if s.denied_files > 0 {
            let _ = writeln!(
                out,
                "note: {} files could not be opened. Their reported size, {}, is included,",
                s.denied_files,
                human(s.denied_file_bytes)
            );
            let _ = writeln!(out, "      but their true allocation is unknown.");
        }
    }

    // A residual only means anything if the scan covered a whole volume. Comparing a subtree's
    // size against the volume's used space would produce a meaningless number, so it is not
    // attempted.
    if is_volume_root(path) {
        reconcile_volume(path, s, out)?;
    } else {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "note: this was a partial scan. Volume reconciliation is only meaningful for"
        );
        let _ = writeln!(out, "      a whole volume, so it is not attempted here.");
    }

    Ok(())
}

/// Compare what we attributed against what the filesystem says is in use.
///
/// The residual is reported, not hidden. A tool that silently shows a large unexplained gap is
/// not trustworthy; one that names the gap and its likely causes is telling the truth, and the
/// truth is checkable.
fn reconcile_volume(path: &str, s: &WalkSummary, out: &mut String) -> Result<(), String> {
    let v = volume_info(path).map_err(|e| e.to_string())?;

    // NTFS keeps its own bookkeeping — the master file table, the change journal — which a
    // directory walk cannot see and which occupies real space. Querying it needs a handle to
    // the volume, and therefore administrator rights. Without them this returns `None` and the
    // residual absorbs the difference, which is exactly what the privilege line warns about.
    let metadata = ntfs_metadata(path).ok().flatten();

    let known_metadata = metadata
        .as_ref()
        .map(|m| m.mft_bytes.saturating_add(m.usn_journal_max_bytes));

    let accounting = Accounting {
        total: v.total_bytes,
        free: v.free_bytes,
        file_bytes: s.allocated_bytes,
        // Alternate data streams and directory overhead are not yet enumerated. Left at zero so
        // the residual absorbs them honestly rather than being flattered by an estimate.
        stream_bytes: 0,
        directory_bytes: 0,
        known_metadata,
        denied: Denied::new(s.denied_directories.saturating_add(s.denied_files)),
    };

    let residual = accounting.residual();
    let ratio = accounting.residual_ratio();

    let _ = writeln!(out);
    let _ = writeln!(out, "--- reconciliation ---");
    let _ = writeln!(out, "capacity    {}", human(accounting.total));
    let _ = writeln!(out, "used        {}", human(accounting.used()));
    let _ = writeln!(out, "attributed  {}", human(accounting.attributed()));
    let _ = writeln!(out, "unaccounted {}", human(residual));
    let _ = writeln!(out, "            {:.2}% of capacity", ratio * 100.0);

    let _ = writeln!(out);
    let _ = writeln!(out, "components of the figure above:");
    match &metadata {
        Some(m) => {
            let _ = writeln!(
                out,
                "  master file table     {} — measured",
                human(m.mft_bytes)
            );
            if m.usn_journal_max_bytes > 0 {
                let _ = writeln!(
                    out,
                    "  change journal        {} maximum — measured",
                    human(m.usn_journal_max_bytes)
                );
            }
            let _ = writeln!(out, "  alternate streams     not yet enumerated");
            let _ = writeln!(out, "  directory overhead    not yet enumerated");
        }
        None => {
            let _ = writeln!(
                out,
                "  filesystem metadata   not measured — requires an elevated scan"
            );
            let _ = writeln!(out, "  alternate streams     not yet enumerated");
            let _ = writeln!(out, "  directory overhead    not yet enumerated");
        }
    }
    if s.denied_directories > 0 {
        let _ = writeln!(
            out,
            "  unreadable directories {} — contents entirely unknown",
            s.denied_directories
        );
    }
    if s.denied_files > 0 {
        let _ = writeln!(
            out,
            "  unreadable files       {} totalling {}",
            s.denied_files,
            human(s.denied_file_bytes)
        );
    }

    if s.denied_directories > 0 && !is_elevated() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{} directories are unreadable at this privilege level. Running the scan from an",
            s.denied_directories
        );
        let _ = writeln!(
            out,
            "elevated shell would open them and account for their contents."
        );
        let _ = writeln!(
            out,
            "The largest is usually System Volume Information, which holds shadow copies"
        );
        let _ = writeln!(out, "and can occupy up to a tenth of the volume.");
    }

    match reconcile(&accounting, RESIDUAL_TOLERANCE) {
        Reconciliation::Explained { .. } => {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "within the {:.0}% tolerance used by this build.",
                RESIDUAL_TOLERANCE * 100.0
            );
        }
        Reconciliation::Unexplained {
            unreadable_paths, ..
        } => {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "beyond the {:.0}% tolerance used by this build.",
                RESIDUAL_TOLERANCE * 100.0
            );
            if unreadable_paths > 0 {
                let _ = writeln!(
                    out,
                    "{unreadable_paths} paths were unreadable, which is the first thing to investigate."
                );
            }
            let _ = writeln!(
                out,
                "This is a finding, not an error: the accounting is incomplete and says so."
            );
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
