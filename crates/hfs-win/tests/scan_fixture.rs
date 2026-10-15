//! A synthetic volume in miniature.
//!
//! CONTRIBUTING requires test fixtures to be synthetic — generated, deterministic, and free
//! of real filesystem data. This is that fixture: a fixed tree with a hardlink pair, a named
//! stream and a nested directory, asserting the walk accounts each exactly once. Nothing here
//! is captured from a real machine, and the directory is removed afterwards.
//!
//! Sizes are chosen well above MFT-residency so allocation is cluster-rounded and
//! predictable on any cluster size, which is read from the volume itself rather than
//! assumed.

use std::io::Write as _;

use hfs_win::{EntryKind, WalkOptions, volume_info, walk_with};

fn round_up(bytes: u64, cluster: u64) -> u64 {
    (bytes + cluster - 1) / cluster * cluster
}

#[test]
fn fixture_tree_is_accounted_exactly_once() {
    let dir = std::env::temp_dir().join(format!("hfs-fixture-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).expect("fixture dirs");

    // Two datasets: 10_000 bytes with a 5_000-byte named stream, and 6_000 bytes nested.
    std::fs::write(dir.join("a.bin"), vec![1u8; 10_000]).expect("a.bin");
    let stream_path = dir.join("a.bin:note");
    let Ok(mut stream) = std::fs::File::create(&stream_path) else {
        // No named streams on this filesystem (anything non-NTFS). There is nothing the
        // stream assertions could meaningfully check here.
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    stream.write_all(&[2u8; 5_000]).expect("stream data");
    drop(stream);

    // A second name for the same data. Deleting one must free nothing, and the accounting
    // must say so by counting the group once.
    if std::fs::hard_link(dir.join("a.bin"), dir.join("b.bin")).is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    std::fs::write(dir.join("sub").join("c.bin"), vec![3u8; 6_000]).expect("c.bin");

    let cluster = volume_info(&dir.display().to_string())
        .map(|v| u64::from(v.cluster_bytes))
        .unwrap_or(4096);
    let main_a = round_up(10_000, cluster);
    let main_c = round_up(6_000, cluster);
    let stream_bytes = round_up(5_000, cluster);

    let walk = walk_with(
        &dir.display().to_string(),
        WalkOptions {
            collect_entries: true,
        },
    )
    .expect("fixture walks");
    let s = &walk.summary;

    assert_eq!(s.files, 3, "two names for a.bin plus c.bin");
    assert_eq!(s.denied_files, 0);
    assert_eq!(s.denied_directories, 0);
    assert_eq!(s.hardlink_groups, 1);
    assert_eq!(s.hardlink_extra_names, 1);
    assert_eq!(
        s.allocated_bytes,
        main_a + main_c,
        "the hardlink group contributes its data once"
    );
    assert_eq!(
        s.logical_bytes, 16_000,
        "logical is also counted once per group"
    );
    assert_eq!(s.stream_count, 1);
    assert_eq!(s.stream_files, 1);
    assert_eq!(s.stream_bytes, stream_bytes);
    assert_eq!(s.stream_unknown_files, 0);

    // Per-entry truth: the streamed file carries its stream figures and two links, the
    // plain file carries none, and neither is flagged unreadable.
    let entry_a = walk
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::File && e.path.ends_with("a.bin"))
        .expect("a.bin is an entry");
    assert_eq!(entry_a.links, 2);
    assert!(entry_a.streams_known);
    assert!(!entry_a.denied);
    assert_eq!(entry_a.stream_bytes, stream_bytes);

    let entry_c = walk
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::File && e.path.ends_with("c.bin"))
        .expect("c.bin is an entry");
    assert!(entry_c.streams_known);
    assert_eq!(entry_c.stream_bytes, 0);

    // Directories report their streams as unknown, not zero: `FileStreamInfo` fails on
    // directory handles, so a zero here would claim a measurement that was never made.
    // Unknowns on directories are a platform limitation, not per-directory failures, so
    // they are disclosed once in the report rather than counted in the totals above.
    let entry_sub = walk
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::Directory && e.path.ends_with("sub"))
        .expect("sub is an entry");
    assert!(!entry_sub.streams_known);

    let _ = std::fs::remove_dir_all(&dir);
}
