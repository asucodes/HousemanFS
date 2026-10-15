//! End to end beneath the worker: walk a fixture, index it, and prove the candidates.
//!
//! The binary crate exposes nothing to integration tests, so this drives the same
//! pipeline the duplicate worker runs — real walk, real index, real files — with a
//! synthetic tree and nothing else. The worker's resolver is covered by unit tests
//! against stub answers; here the answers are bytes on disk.

#[test]
fn duplicate_candidates_collapse_hardlinks_end_to_end() {
    let dir = std::env::temp_dir().join(format!("hfs-dupe2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Two identical files, one same-size-but-different file, and a hardlink pair.
    let same = vec![9u8; 100_000];
    std::fs::write(dir.join("copy-a.bin"), &same).unwrap();
    std::fs::write(dir.join("copy-b.bin"), &same).unwrap();
    let mut different = same.clone();
    different[50_000] = 8;
    std::fs::write(dir.join("other.bin"), &different).unwrap();
    std::fs::write(dir.join("linked.bin"), &same).unwrap();
    std::fs::hard_link(dir.join("linked.bin"), dir.join("linked-alias.bin")).unwrap();

    let mut control = hfs_win::WalkControl::default();
    let scan =
        hfs_import::scan_path(&dir.display().to_string(), &mut control).expect("fixture scans");
    assert_eq!(scan.summary.files, 5);

    let db = dir.join("test.db");
    let mut index = hfs_index::Index::open(&db).expect("index opens");
    index
        .write_scan(&scan.meta, scan.dirs.iter(), scan.files.iter())
        .expect("index writes");

    let groups = index.size_groups(0, 200, 20).expect("candidates");
    assert_eq!(groups.len(), 1, "one shared size across all five files");
    assert_eq!(
        groups[0].members.len(),
        4,
        "two copies, the different file, and the hardlink family collapsed to one member"
    );
    assert_eq!(groups[0].distinct_ids(), 4);
    let linked = groups[0]
        .members
        .iter()
        .find(|m| m.links == 2)
        .expect("the hardlink family carries its link count");
    assert!(linked.path.ends_with("linked.bin") || linked.path.ends_with("linked-alias.bin"));

    let _ = std::fs::remove_dir_all(&dir);
}
