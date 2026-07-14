//! Background library indexing.
//!
//! Browsing is unchanged and never waits on the index (the perf work stays
//! intact). Opening a folder just *enqueues* it; a single background thread
//! walks it, BLAKE3-hashes new/changed files, and upserts them into the SQLite
//! index — content-hash keyed so tags survive renames, with move/delete
//! reconciliation. This first pass records identity + filesystem facts only; the
//! decode-based enrichment (EXIF, pHash) and the sort/filter/sidebar that read
//! the index come in later commits.
//!
//! Threading (house rule 6, no tokio): a `std::thread` owns the one writer
//! `Db`; requests and progress cross via `async-channel`, and the UI reads
//! progress on the GLib main context.

use std::collections::HashSet;
use std::path::PathBuf;

use gtk::glib;

use vitrine_engine::scanner::Change;
use vitrine_engine::{classify, walk_images, Db, FileRecord};

/// Progress emitted by the indexer for the UI.
#[derive(Debug, Clone)]
pub enum IndexProgress {
    /// A scan started for a folder with `total` image files.
    Started { total: usize },
    /// `done` of `total` processed so far.
    Advanced { done: usize, total: usize },
    /// The current scan finished; `added` new/changed rows were written.
    Finished { added: usize },
}

/// Handle to the background indexer: enqueue folders, receive progress.
pub struct Indexer {
    requests: async_channel::Sender<PathBuf>,
    pub progress: async_channel::Receiver<IndexProgress>,
}

impl Indexer {
    /// Spawn the indexer, writing to `db_path` (created if needed).
    pub fn spawn(db_path: PathBuf) -> Indexer {
        let (req_tx, req_rx) = async_channel::unbounded::<PathBuf>();
        let (prog_tx, prog_rx) = async_channel::unbounded::<IndexProgress>();

        std::thread::Builder::new()
            .name("vitrine-indexer".into())
            .spawn(move || worker(db_path, req_rx, prog_tx))
            .expect("spawn indexer thread");

        Indexer {
            requests: req_tx,
            progress: prog_rx,
        }
    }

    /// Enqueue a folder to index (non-blocking; ignored if the worker is gone).
    pub fn request(&self, folder: PathBuf) {
        let _ = self.requests.try_send(folder);
    }
}

fn worker(
    db_path: PathBuf,
    requests: async_channel::Receiver<PathBuf>,
    progress: async_channel::Sender<IndexProgress>,
) {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let db = match Db::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            glib::g_warning!("vitrine", "index open {}: {e}", db_path.display());
            return;
        }
    };

    // Process one folder at a time (single writer). recv_blocking parks the
    // thread cheaply between requests.
    while let Ok(folder) = requests.recv_blocking() {
        if let Err(e) = scan(&db, &folder, &progress) {
            glib::g_warning!("vitrine", "index scan {}: {e}", folder.display());
        }
    }
}

type ScanResult = Result<(), Box<dyn std::error::Error>>;

fn scan(
    db: &Db,
    folder: &std::path::Path,
    progress: &async_channel::Sender<IndexProgress>,
) -> ScanResult {
    let files = walk_images(folder);
    let total = files.len();
    let _ = progress.try_send(IndexProgress::Started { total });

    // Reconcile deletions *first*: mark every DB path under this root that is no
    // longer present as missing. This is what lets a rename relink below — the
    // file's old name is now a "missing" row whose content_hash the new name can
    // reclaim, keeping one row (and its indexed_at) instead of leaving a stale
    // missing row beside a fresh one.
    let seen: HashSet<String> = files
        .iter()
        .map(|sf| sf.path.to_string_lossy().to_string())
        .collect();
    db.reconcile_deleted(&folder.to_string_lossy(), &seen)?;

    let now = now_secs();
    let mut added = 0usize;

    for (i, sf) in files.iter().enumerate() {
        let path = sf.path.to_string_lossy().to_string();

        let existing = db.file_by_path(&path)?;
        if classify(existing.as_ref(), sf.size, sf.mtime) != Change::Unchanged {
            if let Ok(hash) = vitrine_engine::blake3_file(&sf.path) {
                added += 1;
                // Same bytes at a vanished path → a move; relink to keep the row
                // (and its indexed_at); otherwise upsert identity + fs facts.
                match db.missing_file_by_hash(&hash)? {
                    Some(moved_from) if existing.is_none() => {
                        db.relink_path(&moved_from.path, &path, sf.mtime)?;
                    }
                    _ => {
                        db.upsert_file(&FileRecord {
                            path,
                            content_hash: hash,
                            size: sf.size,
                            mtime: sf.mtime,
                            indexed_at: now,
                            ..Default::default()
                        })?;
                    }
                }
            }
        }

        // Throttle UI updates: every 64 files and at the end.
        if i % 64 == 0 || i + 1 == total {
            let _ = progress.try_send(IndexProgress::Advanced { done: i + 1, total });
        }
    }

    let _ = progress.try_send(IndexProgress::Finished { added });
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
