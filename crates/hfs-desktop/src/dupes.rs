//! Duplicate detection: same size is a suspicion, same bytes are the verdict.
//!
//! The pipeline has three stages, each more conclusive than the last: group by reported
//! size (the index), split by the first 64 KiB (one short read per file), confirm by
//! comparing the full contents of whatever still ties — byte for byte, not by hash, so
//! there is no collision caveat to disclose and no digest to trust. A file that cannot
//! be read is excluded from contention: unreadable files are unknown, and unknowns do
//! not join groups.
//!
//! Reading file contents is read-only. The read-only invariant bans *writes* to scanned
//! volumes; measuring never required contents before, and this view does. Nothing here
//! creates, modifies or deletes anything.
//!
//! The grouping logic takes its reads as parameters, so tests prove it with stub answers
//! and no filesystem. The worker thread supplies real file reads.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;

use hfs_index::{Index, SizeGroup};

/// First-stage comparison covers this much of each file. Files at or below this size are
/// decided by the prefix alone — there is nothing more of them to read.
const PREFIX_BYTES: u64 = 64 * 1024;
/// Read chunk for full comparisons. Cancellation is checked per chunk.
const CHUNK_BYTES: usize = 1024 * 1024;

/// One confirmed duplicate set: same bytes, distinct datasets.
pub struct DuplicateGroup {
    pub members: Vec<DuplicateMember>,
    /// What the copies would occupy reduced to one: total minus the largest kept.
    /// A potential, not a promise — freeing requires deleting, which this tool does not
    /// do, and shared extents or compression can make reality smaller. Verify (D9) before
    /// acting on it anywhere.
    pub reclaimable: u64,
}

/// One copy: a dataset, possibly under several names.
pub struct DuplicateMember {
    pub path: String,
    pub allocated: u64,
    /// Names sharing this dataset. Copies within one dataset free nothing; the view says
    /// so per member rather than letting the group total mislead.
    pub links: u32,
}

pub struct DupReport {
    pub groups: Vec<DuplicateGroup>,
    /// Members that could not be read and were excluded from contention.
    pub skipped: u64,
    pub bytes_read: u64,
    pub groups_checked: usize,
}

enum Message {
    Progress { members: u64, bytes: u64 },
    Done(Result<DupReport, String>),
}

/// A running duplicate search. Poll it; do not block on it.
pub struct DupWorker {
    rx: Receiver<Message>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    finished: bool,
}

impl DupWorker {
    pub fn start(db: String, min_bytes: u64) -> Self {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let handle = std::thread::spawn(move || {
            let report = find_duplicates(&db, min_bytes, &tx, &flag);
            let _ = tx.send(Message::Done(report));
        });
        Self {
            rx,
            cancel,
            handle: Some(handle),
            finished: false,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Drain progress: latest (members read, bytes read), and the report exactly once.
    pub fn poll(&mut self) -> (Option<(u64, u64)>, Option<Result<DupReport, String>>) {
        let mut progress = None;
        let mut done = None;
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Message::Progress { members, bytes } => progress = Some((members, bytes)),
                Message::Done(report) => {
                    done = Some(report);
                    self.finished = true;
                }
            }
        }
        (progress, done)
    }
}

impl Drop for DupWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn find_duplicates(
    db: &str,
    min_bytes: u64,
    tx: &std::sync::mpsc::Sender<Message>,
    cancel: &Arc<AtomicBool>,
) -> Result<DupReport, String> {
    let index = Index::open(db).map_err(|e| e.to_string())?;
    let groups = index
        .size_groups(min_bytes, 200, 20)
        .map_err(|e| e.to_string())?;
    let read_members = std::cell::Cell::new(0u64);
    let read_bytes = std::cell::Cell::new(0u64);
    let skipped = std::cell::Cell::new(0u64);
    let confirmed = resolve_groups(
        &groups,
        &mut |path| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let prefix = read_prefix(path)?;
            read_members.set(read_members.get() + 1);
            read_bytes.set(read_bytes.get() + prefix.len() as u64);
            let _ = tx.send(Message::Progress {
                members: read_members.get(),
                bytes: read_bytes.get(),
            });
            Some(prefix)
        },
        &mut |paths| {
            let mut compared = 0u64;
            let sets = partition_identical(paths, cancel, &mut compared);
            read_bytes.set(read_bytes.get() + compared);
            let _ = tx.send(Message::Progress {
                members: read_members.get(),
                bytes: read_bytes.get(),
            });
            sets
        },
        &mut |dropped| {
            skipped.set(skipped.get() + dropped);
        },
    );
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled — partial groups are discarded".to_string());
    }
    Ok(DupReport {
        groups: confirmed,
        skipped: skipped.get(),
        bytes_read: read_bytes.get(),
        groups_checked: groups.len(),
    })
}

/// Read the first bytes of a path: the comparison prefix.
///
/// Returns `None` when the file cannot be read. Cancellation is checked by the caller
/// between files; a 64 KiB read is short enough that none is needed mid-read.
fn read_prefix(path: &str) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut prefix = vec![0u8; PREFIX_BYTES as usize];
    let mut filled = 0usize;
    while filled < prefix.len() {
        let read = file.read(&mut prefix[filled..]).ok()?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    prefix.truncate(filled);
    Some(prefix)
}

/// Compare two paths byte for byte, counting the bytes compared.
///
/// `None` means unreadable (or cancelled mid-compare), never "different": unknown and
/// unequal are separate outcomes and share no code path.
fn files_identical(a: &str, b: &str, cancel: &Arc<AtomicBool>, bytes: &mut u64) -> Option<bool> {
    let mut fa = std::fs::File::open(a).ok()?;
    let mut fb = std::fs::File::open(b).ok()?;
    let mut ba = vec![0u8; CHUNK_BYTES];
    let mut bb = vec![0u8; CHUNK_BYTES];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let ra = fa.read(&mut ba).ok()?;
        let rb = fb.read(&mut bb).ok()?;
        *bytes += (ra + rb) as u64;
        if ra != rb || ba[..ra] != bb[..rb] {
            return Some(false);
        }
        if ra == 0 {
            return Some(true);
        }
    }
}

/// Partition paths into sets of byte-identical files (union-find over pairwise
/// comparison). Unreadable paths join no set. Singletons are kept here and dropped by
/// the caller: one file duplicates nothing, and only the caller knows that.
fn partition_identical(
    paths: &[String],
    cancel: &Arc<AtomicBool>,
    bytes: &mut u64,
) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..paths.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for i in 0..paths.len() {
        for j in (i + 1)..paths.len() {
            if cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            if find(&mut parent, i) == find(&mut parent, j) {
                continue;
            }
            // `Some(false)` differs; `None` is unknown. Neither unions, and the two
            // stay separate for exactly the reasons they are.
            if files_identical(&paths[i], &paths[j], cancel, bytes) == Some(true) {
                let root = find(&mut parent, i);
                let other = find(&mut parent, j);
                parent[other] = root;
            }
        }
    }
    let mut sets: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for (index, _) in paths.iter().enumerate() {
        sets.entry(find(&mut parent, index)).or_default().push(index);
    }
    sets.into_values().collect()
}

/// Resolve candidate groups into confirmed duplicates.
///
/// `prefix_of` reads first bytes, `partition` confirms full ties, `drop_count` hears how
/// many members were excluded. Pure over its parameters: the worker passes file reads,
/// tests pass stub answers.
fn resolve_groups(
    groups: &[SizeGroup],
    prefix_of: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    partition: &mut dyn FnMut(&[String]) -> Vec<Vec<usize>>,
    drop_count: &mut dyn FnMut(u64),
) -> Vec<DuplicateGroup> {
    let mut confirmed = Vec::new();
    for group in groups {
        // Stage one: split by prefix bytes.
        let mut by_prefix: std::collections::HashMap<Vec<u8>, Vec<usize>> =
            std::collections::HashMap::new();
        for (index, member) in group.members.iter().enumerate() {
            match prefix_of(&member.path) {
                None => drop_count(1),
                Some(bytes) => {
                    by_prefix.entry(bytes).or_default().push(index);
                }
            }
        }
        for indexes in by_prefix.into_values() {
            if indexes.len() < 2 {
                continue;
            }
            // Small files end here: the prefix covered everything, so equal prefixes
            // are equal files and no second pass exists to run.
            if group.logical <= PREFIX_BYTES {
                confirmed.push(finish(
                    indexes
                        .iter()
                        .map(|&index| &group.members[index])
                        .collect(),
                ));
                continue;
            }
            // Large files: compare the tied paths byte for byte.
            let paths: Vec<String> = indexes
                .iter()
                .map(|&index| group.members[index].path.clone())
                .collect();
            for set in partition(&paths) {
                if set.len() < 2 {
                    continue;
                }
                confirmed.push(finish(
                    set.into_iter().map(|n| &group.members[indexes[n]]).collect(),
                ));
            }
        }
    }
    // Largest potential first: the view leads with what matters.
    confirmed.sort_by_key(|group: &DuplicateGroup| std::cmp::Reverse(group.reclaimable));
    confirmed
}

fn finish(members: Vec<&hfs_index::SizeMember>) -> DuplicateGroup {
    let total: u64 = members.iter().map(|m| m.allocated).sum();
    let kept = members.iter().map(|m| m.allocated).max().unwrap_or(0);
    DuplicateGroup {
        members: members
            .into_iter()
            .map(|m| DuplicateMember {
                path: m.path.clone(),
                allocated: m.allocated,
                links: m.links,
            })
            .collect(),
        reclaimable: total.saturating_sub(kept),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hfs_index::{SizeGroup, SizeMember};

    fn member(path: &str, id: u8) -> SizeMember {
        SizeMember {
            path: path.to_string(),
            allocated: 1_000_000,
            id: vec![id],
            links: 1,
        }
    }

    fn small_group(members: Vec<SizeMember>) -> SizeGroup {
        SizeGroup {
            logical: 100,
            members,
        }
    }

    #[test]
    fn equal_prefixes_confirm_small_files() {
        let groups = vec![small_group(vec![member("a", 1), member("b", 2), member("c", 3)])];
        let mut dropped = 0;
        let confirmed = resolve_groups(
            &groups,
            &mut |_| Some(vec![7u8; 10]),
            &mut |_| panic!("small files never reach full comparison"),
            &mut |n| dropped += n,
        );
        assert_eq!(dropped, 0);
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].members.len(), 3);
        // Three 1 MB copies reduced to one: 2 MB potential.
        assert_eq!(confirmed[0].reclaimable, 2_000_000);
    }

    #[test]
    fn differing_prefixes_split_and_unreadable_members_drop_out() {
        let groups = vec![small_group(vec![member("a", 1), member("b", 2), member("c", 3)])];
        let mut dropped = 0;
        let confirmed = resolve_groups(
            &groups,
            &mut |path| match path {
                "a" => Some(vec![7u8; 10]),
                "b" => Some(vec![8u8; 10]),
                _ => None,
            },
            &mut |_| panic!("no ties, no comparison"),
            &mut |n| dropped += n,
        );
        assert_eq!(dropped, 1, "the unreadable member is counted");
        assert!(confirmed.is_empty(), "nothing ties, nothing confirms");
    }

    #[test]
    fn large_prefix_ties_break_on_full_comparison() {
        let groups = vec![SizeGroup {
            logical: PREFIX_BYTES + 1,
            members: vec![member("a", 1), member("b", 2)],
        }];
        let mut dropped = 0;
        let confirmed = resolve_groups(
            &groups,
            &mut |_| Some(vec![7u8; 10]),
            &mut |_| vec![vec![0], vec![1]],
            &mut |n| dropped += n,
        );
        assert_eq!(dropped, 0);
        assert!(confirmed.is_empty(), "comparison found no identical sets");
    }

    #[test]
    fn byte_comparison_proves_equality_on_real_files() {
        let dir = std::env::temp_dir();
        let tag = std::process::id();
        let a = dir.join(format!("hfs-dup-a-{tag}.bin"));
        let b = dir.join(format!("hfs-dup-b-{tag}.bin"));
        let c = dir.join(format!("hfs-dup-c-{tag}.bin"));
        std::fs::write(&a, b"identical bytes here").unwrap();
        std::fs::write(&b, b"identical bytes here").unwrap();
        std::fs::write(&c, b"identical bytes HERX").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (sa, sb, sc) = (
            a.display().to_string(),
            b.display().to_string(),
            c.display().to_string(),
        );
        let mut bytes = 0;
        assert_eq!(
            files_identical(&sa, &sb, &cancel, &mut bytes),
            Some(true),
            "same bytes prove identical"
        );
        assert_eq!(
            files_identical(&sa, &sc, &cancel, &mut bytes),
            Some(false),
            "different bytes prove different"
        );
        assert_eq!(
            files_identical(&sa, &format!("{sa}-missing"), &cancel, &mut bytes),
            None,
            "unreadable is unknown, not unequal"
        );
        let sets = partition_identical(&[sa, sb, sc], &cancel, &mut bytes);
        assert_eq!(sets.len(), 2, "a pair plus a singleton");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        let _ = std::fs::remove_file(&c);
    }
}
