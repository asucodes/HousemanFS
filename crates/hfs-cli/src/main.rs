//! Command-line entry point for HousemanFS.
//!
//! Every command in this release is read-only. There is no verb that modifies a volume, and
//! none is reachable from here.

mod index;

use std::fmt::Write as _;
use std::process::ExitCode;

use hfs_core::{RESIDUAL_TOLERANCE, Reconciliation, human_bytes_exact as human};
use hfs_win::{
    BackupPrivilege, WalkSummary, enable_backup_privilege, holds_backup_privilege, is_elevated,
    volume_info, walk,
};

const USAGE: &str = "\
housemanfs — read-only filesystem accounting

usage:
  housemanfs                the finder (needs an index; see below)
  housemanfs info <path>    volume facts for the volume containing <path>
  housemanfs scan <path>    walk <path> and report what occupies space
  housemanfs index <path>   scan <path> and store the result for fast queries
  housemanfs refresh [path] re-scan and update the index in place
  housemanfs top [n]        largest files in the index (default 20)
  housemanfs dirs [n]       largest directory subtrees (default 20)
  housemanfs find <term>    files whose name contains <term>
  housemanfs ask QUERY      structured questions: size>500MB ext:iso older than 1y
                  shared streams name:backup videos limit 10
  housemanfs suggest      judgment over the largest indexed files, with evidence
  housemanfs eval           run the committed evaluation set
  housemanfs ext [n]        occupied space by extension
  housemanfs explain <path> the measurement behind one indexed path
  housemanfs export         the index as versioned JSON (see --out)

options:
  --out <file>              write the report to a file instead of stdout
  --index <file>            index to use (default housemanfs.db)
  --limit <n>               how many rows to show
  --threshold <p>           minimum confidence for a suggestion (default 0.5)
  --without <rules>         switch rules off, e.g. R1,R3
  --min-bytes <size>        smallest file a suggestion considers (default 1GB)

Scanning a volume root additionally reconciles the result against what the
filesystem says is in use, and reports what could not be accounted for.

Every command is read-only. Nothing here modifies a volume.";

fn main() -> ExitCode {
    let mut command: Option<String> = None;
    let mut path: Option<String> = None;
    let mut out_file: Option<String> = None;
    let mut db = index::DEFAULT_INDEX.to_string();
    let mut limit: u32 = 20;
    let mut threshold: Option<String> = None;
    let mut without: Option<String> = None;
    let mut min_bytes: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out_file = args.next(),
            "--index" => {
                if let Some(value) = args.next() {
                    db = value;
                }
            }
            "--limit" => {
                if let Some(value) = args.next() {
                    limit = value.parse().unwrap_or(limit);
                }
            }
            "--threshold" => threshold = args.next(),
            "--without" => without = args.next(),
            "--min-bytes" => min_bytes = args.next(),
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

    // No arguments means the finder, not a wall of usage text. It is the thing a returning user
    // most likely wants, and `--help` is there for anyone who wants the rest.
    let command = command.unwrap_or_else(|| "finder".to_string());
    let path = path.unwrap_or_default();

    // Queries are meaningless without a target, and an empty one would otherwise silently
    // scan or query nothing while reporting success.
    if matches!(
        command.as_str(),
        "info" | "scan" | "index" | "find" | "explain" | "ask"
    ) && path.is_empty()
    {
        eprintln!("error: `{command}` needs an argument");
        return ExitCode::FAILURE;
    }

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
        "index" => index::build(&path, &db, &mut out),
        "refresh" => index::refresh(&db, &path, &mut out),
        "top" => index::top(&db, limit, &mut out),
        "dirs" => index::dirs(&db, limit, &mut out),
        "find" => index::search(&db, &path, limit, &mut out),
        "ask" => index::ask(&db, &path, &mut out),
        "suggest" => index::suggest(
            &db,
            limit,
            threshold.as_deref(),
            without.as_deref(),
            min_bytes.as_deref(),
            &mut out,
        ),
        "eval" => index::eval(&mut out),
        "ext" => index::extensions(&db, limit, &mut out),
        "explain" => index::explain(&db, &path, &mut out),
        "export" => index::export(&db, limit, &mut out),
        "finder" | "ui" => {
            // The finder takes over the terminal, so it cannot also write into the report
            // buffer. It is dispatched before any output is produced.
            return match hfs_tui::run(&db) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("error: {err}");
                    ExitCode::FAILURE
                }
            };
        }
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
    // Ask for the backup privilege before touching the volume. It is what allows the scan to
    // open files that refuse even an administrator — SYSTEM-owned files, and directories held
    // open by a service. Asking is not the same as having it, so the outcome is reported
    // rather than assumed.
    let backup = enable_backup_privilege();

    let result = walk(path).map_err(|e| e.to_string())?;
    let s = &result.summary;

    // A privilege that could not be obtained is not a reason to stop. The scan is still useful
    // with less visibility, and the point of reporting the outcome is to let the user judge the
    // numbers, not to refuse to produce them.
    let privilege_note = match backup {
        BackupPrivilege::Enabled => "backup privilege enabled".to_string(),
        // These two are different problems: one means the process is not an administrator, the
        // other means the privilege is there and something else stopped it being switched on.
        BackupPrivilege::NotHeld if holds_backup_privilege() => {
            "backup privilege held but could not be enabled".to_string()
        }
        BackupPrivilege::NotHeld => "backup privilege not held by this token".to_string(),
        // The code is printed rather than flattened to "failed", because the difference
        // between access denied and bad length decides what to try next.
        BackupPrivilege::Failed(code) => {
            format!("backup privilege could not be enabled (os error {code})")
        }
    };

    let _ = writeln!(out, "scanned     {path}");
    let _ = writeln!(
        out,
        "privilege   {}, {}",
        if is_elevated() {
            "elevated"
        } else {
            "standard user"
        },
        privilege_note
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
    let _ = writeln!(out, "dir index   {}", human(s.directory_bytes));

    if s.stream_count > 0 {
        let _ = writeln!(
            out,
            "streams     {} across {} {} on {} {}",
            human(s.stream_bytes),
            s.stream_count,
            index::plural(s.stream_count, "stream", "streams"),
            s.stream_files,
            index::plural(s.stream_files, "file", "files"),
        );
        let _ = writeln!(
            out,
            "            named data the directory listing never shows"
        );
    }

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
            for path in &s.denied_directory_samples {
                let _ = writeln!(out, "      {path}");
            }
            let shown = s.denied_directory_samples.len();
            if (s.denied_directories as usize) > shown {
                let _ = writeln!(
                    out,
                    "      ... and {} more",
                    s.denied_directories as usize - shown
                );
            }
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
    // The numbers come from the shared accounting; this function only renders them. The
    // desktop's dashboard reads the same struct, so the finding is identical on screen.
    let found = hfs_import::account_volume(path, s)?;
    let accounting = &found.accounting;

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
    match found.mft_bytes {
        Some(bytes) => {
            let _ = writeln!(out, "  master file table     {} — measured", human(bytes));
        }
        None => match &found.metadata_error {
            Some(err) => {
                let _ = writeln!(out, "  filesystem metadata   query refused: {err}");
            }
            None => {
                let _ = writeln!(
                    out,
                    "  filesystem metadata   not measured — requires an elevated scan"
                );
            }
        },
    }
    if found.system_measured > 0 {
        let _ = writeln!(
            out,
            "  ntfs system files     {} across {} files — measured",
            human(found.system_bytes),
            found.system_measured
        );
        if found.system_failed > 0 {
            let _ = writeln!(
                out,
                "                        {} could not be opened, so the total is low",
                found.system_failed
            );
        }
    }
    // Only shown when it contributed. Once the system files are measured by name the journal is
    // already inside that total, and listing it again would overstate what was counted.
    if found.system_measured == 0 {
        if let Some(m) = &found.metadata {
            if m.usn_journal_max_bytes > 0 {
                let _ = writeln!(
                    out,
                    "  change journal        {} maximum — measured",
                    human(m.usn_journal_max_bytes)
                );
            }
        }
    }
    let _ = writeln!(
        out,
        "  directory overhead    {} — measured",
        human(s.directory_bytes)
    );
    if s.stream_count > 0 {
        let _ = writeln!(
            out,
            "  alternate streams     {} across {} streams — measured",
            human(s.stream_bytes),
            s.stream_count
        );
        if s.stream_unknown_files > 0 {
            let _ = writeln!(
                out,
                "                        {stream_unknown} files' streams could not be queried,",
                stream_unknown = s.stream_unknown_files
            );
            let _ = writeln!(out, "                        so the total is low");
        }
        // Directory streams are never in this total: the enumeration API fails on directory
        // handles, and the fallback reports logical sizes without allocations. One line,
        // always, so the figure is never read as complete.
        let _ = writeln!(
            out,
            "                        streams on directories are not enumerated"
        );
    } else if s.stream_unknown_files > 0 {
        // Nothing was found, but not every file was examined. Reporting "none" here would be
        // exactly the optimistic lie the residual exists to prevent.
        let _ = writeln!(
            out,
            "  alternate streams     unknown — not enumerable on {unknown} files",
            unknown = s.stream_unknown_files
        );
    } else {
        let _ = writeln!(out, "  alternate streams     none found — measured");
    }
    if s.denied_directories > 0 {
        let _ = writeln!(
            out,
            "  unreadable directories {} — contents entirely unknown",
            s.denied_directories
        );
        for path in &s.denied_directory_samples {
            let _ = writeln!(out, "    {path}");
        }
        let shown = s.denied_directory_samples.len();
        if (s.denied_directories as usize) > shown {
            let _ = writeln!(
                out,
                "    ... and {} more",
                s.denied_directories as usize - shown
            );
        }
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

    match found.reconciliation {
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
