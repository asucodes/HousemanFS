//! Command-line entry point for HousemanFS.
//!
//! Every command in this release is read-only. There is no verb that modifies a volume, and
//! none is reachable from here.

use std::process::ExitCode;

use hfs_win::{volume_info, walk};

const USAGE: &str = "\
housemanfs — read-only filesystem accounting

usage:
  housemanfs info <path>    volume facts for the volume containing <path>
  housemanfs scan <path>    walk <path> and report what occupies space

Both commands are read-only. Nothing here modifies a volume.";

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

    if s.shared_files > 0 {
        println!();
        println!(
            "shared      {} files with more than one name",
            s.shared_files
        );
        println!(
            "            up to {} of storage referenced by them",
            human(s.shared_allocated_upper_bound)
        );
        println!();
        println!("note: that figure is an upper bound, not reclaimable space. Two names for");
        println!("      the same data both report the same allocation, and removing one frees");
        println!("      nothing. They are excluded from the allocated total above.");
    }

    if s.denied > 0 {
        println!();
        println!(
            "note: {} paths could not be read, so the totals above are incomplete.",
            s.denied
        );
    }

    Ok(())
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
        format!("{:.2} {} ({bytes} bytes)", value, UNITS[unit])
    }
}
