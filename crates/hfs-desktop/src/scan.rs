//! The background scan: walking without freezing the interface.
//!
//! A whole-volume scan takes minutes; the UI thread must never wait for it. Scanning runs
//! on a worker thread that reports progress through a channel, and the interface polls the
//! channel once per frame. Cancellation sets a flag the walk checks per directory; a
//! cancelled scan stores nothing, because partial results marked as partial beat no
//! results — but silently adopting them as complete would be worse than both.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use hfs_index::{Index, RefreshReport};
use hfs_win::WalkControl;

/// What one poll reports: the latest progress, and the outcome exactly once.
type Poll = (Option<(u64, u64)>, Option<Result<Outcome, String>>);

/// What the worker says, in order: progress, then exactly one ending.
enum Message {
    Progress { files: u64, dirs: u64 },
    Done(Result<Outcome, String>),
}

/// What a finished scan hands back: where the index lives, and what a refresh changed.
/// Everything else the dashboard reads from the index itself, so the worker never ships
/// numbers two sources could disagree on.
pub struct Outcome {
    pub db_path: String,
    pub refresh: Option<RefreshReport>,
}

/// A running scan. Poll it; do not block on it.
pub struct ScanWorker {
    rx: Receiver<Message>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Set when the worker reported its ending. Polling stops mattering then, but the
    /// handle is joined on drop either way — a thread must never outlive the scan.
    finished: bool,
}

impl ScanWorker {
    /// Start scanning `path` into the index at `db`. `full` replaces the index; otherwise
    /// the scan folds into whatever is there (refusing a foreign volume, like the CLI).
    pub fn start(path: String, db: String, full: bool) -> Self {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let handle = std::thread::spawn(move || run_scan(path, db, full, tx, flag));
        Self {
            rx,
            cancel,
            handle: Some(handle),
            finished: false,
        }
    }

    /// Ask the walk to stop at the next directory boundary. The partial results are
    /// discarded: an index must never present a stopped scan as a finished one.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Drain progress. Returns the latest counts, and the outcome exactly once, when the
    /// worker finishes.
    pub fn poll(&mut self) -> Poll {
        let mut progress = None;
        let mut done = None;
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Message::Progress { files, dirs } => progress = Some((files, dirs)),
                Message::Done(outcome) => {
                    done = Some(outcome);
                    self.finished = true;
                }
            }
        }
        (progress, done)
    }
}

impl Drop for ScanWorker {
    fn drop(&mut self) {
        // If the UI goes away mid-scan, stop the walk first so the thread has a reason to
        // finish, then wait for it. Abandoning a thread that holds a volume handle open is
        // not something a read-only tool does casually.
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_scan(path: String, db: String, full: bool, tx: Sender<Message>, cancel: Arc<AtomicBool>) {
    let outcome = scan(path, db, full, &tx, &cancel);
    let _ = tx.send(Message::Done(outcome));
}

fn scan(
    path: String,
    db: String,
    full: bool,
    tx: &Sender<Message>,
    cancel: &Arc<AtomicBool>,
) -> Result<Outcome, String> {
    if !full {
        // Same volume, or nothing merges — checked before the walk, because a refused
        // refresh must not cost a full scan.
        let live = hfs_win::volume_info(&path).map_err(|e| e.to_string())?;
        let stored = Index::open(&db).map_err(|e| e.to_string())?.scan_metadata();
        let root = if path.is_empty() {
            stored.root.clone()
        } else {
            path.clone()
        };
        if *live.volume_id.as_bytes() != stored.volume_id && stored.volume_id != [0u8; 16] {
            return Err(format!(
                "index covers {} but {root} is a different volume",
                stored.root
            ));
        }
    }

    let mut control = WalkControl {
        progress: Some(&mut |files, dirs| {
            let _ = tx.send(Message::Progress { files, dirs });
        }),
        every_files: 5_000,
        cancel: Some(cancel),
    };
    let scan = hfs_import::scan_path(&path, &mut control)?;

    // A walk that stopped early converts fine, but its rows must never be stored: they
    // describe a prefix of the volume, and the index has no way to mark "the rest is
    // missing" per row. Discard and say so.
    if cancel.load(Ordering::Relaxed) {
        return Err("cancelled — nothing was stored".to_string());
    }

    let mut index = Index::open(&db).map_err(|e| e.to_string())?;
    let refresh = if full {
        index
            .write_scan(&scan.meta, scan.dirs.iter(), scan.files.iter())
            .map_err(|e| e.to_string())?;
        None
    } else {
        Some(
            index
                .refresh_scan(&scan.meta, scan.dirs.iter(), scan.files.iter())
                .map_err(|e| e.to_string())?,
        )
    };

    Ok(Outcome {
        db_path: db,
        refresh,
    })
}
