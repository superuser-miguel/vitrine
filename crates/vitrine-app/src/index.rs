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

use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use vitrine_engine::scanner::Change;
use vitrine_engine::{classify, walk_images, Db, Enrichment, FileRecord};

/// The library index database path (app-private under Flatpak). Shared by the
/// writer thread and the window's read-only query connection.
pub fn index_db_path() -> PathBuf {
    glib::user_data_dir().join("vitrine").join("index.sqlite")
}

/// How many un-enriched files the driver pulls per round-trip to the writer.
const ENRICH_BATCH: i64 = 64;
/// Frame size (px) requested for perceptual hashing — dHash reduces to 8×8, so a
/// small decode is ample and keeps the enrichment pass cheap.
const PHASH_PX: u32 = 64;

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

/// Messages to the writer thread (which owns the one `Db`). Identity scans,
/// enrichment writes, and batch queries all share this FIFO channel — so a
/// batch's [`Request::Enrich`] writes are guaranteed applied before the next
/// [`Request::TakeBatch`] query runs, which is what keeps enrichment from
/// handing the same file out twice (no client-side de-dup needed).
enum Request {
    Scan(PathBuf),
    Enrich {
        path: String,
        enrichment: Enrichment,
    },
    TakeBatch {
        reply: async_channel::Sender<Vec<String>>,
    },
    /// Set (or, with `None`, clear) a rating — a user annotation write.
    SetRating {
        hash: String,
        rating: Option<i64>,
    },
    /// Set (or, with empty body, clear) a comment.
    SetComment {
        hash: String,
        body: String,
    },
    /// Apply/remove a tag across a selection of hashes.
    Tag {
        name: String,
        hashes: Vec<String>,
        add: bool,
    },
}

/// A cheap, cloneable handle for routing **annotation writes** to the single
/// writer thread. The UI holds one and fires writes non-blocking; reads happen
/// on the UI's own read connection (WAL sees the write once committed).
#[derive(Clone)]
pub struct Annotator {
    requests: async_channel::Sender<Request>,
}

impl Annotator {
    /// Set a 0–5 rating, or clear it with `None`.
    pub fn set_rating(&self, hash: &str, rating: Option<i64>) {
        let _ = self.requests.try_send(Request::SetRating {
            hash: hash.to_string(),
            rating,
        });
    }

    /// Set a comment (empty string clears it).
    pub fn set_comment(&self, hash: &str, body: &str) {
        let _ = self.requests.try_send(Request::SetComment {
            hash: hash.to_string(),
            body: body.to_string(),
        });
    }

    /// Apply (`add = true`) or remove a tag across `hashes`.
    pub fn tag(&self, name: &str, hashes: &[String], add: bool) {
        let _ = self.requests.try_send(Request::Tag {
            name: name.to_string(),
            hashes: hashes.to_vec(),
            add,
        });
    }
}

/// Handle to the background indexer: enqueue folders, receive progress, drive
/// enrichment. Lives on the main thread (not `Send`); the writer thread only
/// ever sees the `Send` channel ends and the DB path.
pub struct Indexer {
    requests: async_channel::Sender<Request>,
    pub progress: async_channel::Receiver<IndexProgress>,
    /// Guards against running more than one enrichment driver at a time.
    enriching: Rc<Cell<bool>>,
}

impl Indexer {
    /// Spawn the indexer, writing to `db_path` (created if needed).
    pub fn spawn(db_path: PathBuf) -> Indexer {
        let (req_tx, req_rx) = async_channel::unbounded::<Request>();
        let (prog_tx, prog_rx) = async_channel::unbounded::<IndexProgress>();

        std::thread::Builder::new()
            .name("vitrine-indexer".into())
            .spawn(move || worker(db_path, req_rx, prog_tx))
            .expect("spawn indexer thread");

        Indexer {
            requests: req_tx,
            progress: prog_rx,
            enriching: Rc::new(Cell::new(false)),
        }
    }

    /// Enqueue a folder to index (non-blocking; ignored if the worker is gone).
    pub fn request(&self, folder: PathBuf) {
        let _ = self.requests.try_send(Request::Scan(folder));
    }

    /// A handle for routing annotation writes to the writer thread.
    pub fn annotator(&self) -> Annotator {
        Annotator {
            requests: self.requests.clone(),
        }
    }

    /// Start (or, if already running, leave running) the enrichment driver: it
    /// decodes un-enriched files in the background — dimensions, EXIF, pHash —
    /// until the queue is empty, then runs `on_done` (used to refresh a
    /// metadata sort once the columns it reads are populated). Safe to call after
    /// every scan and on startup to mop up leftovers from a previous session.
    pub fn start_enrichment(&self, on_done: impl FnOnce() + 'static) {
        if self.enriching.replace(true) {
            return;
        }
        let requests = self.requests.clone();
        let flag = self.enriching.clone();
        glib::spawn_future_local(async move {
            run_enrichment(requests).await;
            flag.set(false);
            on_done();
        });
    }
}

fn worker(
    db_path: PathBuf,
    requests: async_channel::Receiver<Request>,
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

    // Single writer: process one request at a time. recv_blocking parks the
    // thread cheaply between requests.
    while let Ok(req) = requests.recv_blocking() {
        match req {
            Request::Scan(folder) => {
                if let Err(e) = scan(&db, &folder, &progress) {
                    glib::g_warning!("vitrine", "index scan {}: {e}", folder.display());
                }
            }
            Request::Enrich { path, enrichment } => {
                if let Err(e) = db.set_enrichment(&path, &enrichment) {
                    glib::g_warning!("vitrine", "enrich {path}: {e}");
                }
            }
            Request::TakeBatch { reply } => {
                let batch = db
                    .paths_needing_enrichment(ENRICH_BATCH)
                    .unwrap_or_default();
                let _ = reply.try_send(batch);
            }
            Request::SetRating { hash, rating } => {
                let r = match rating {
                    Some(r) => db.set_rating(&hash, r),
                    None => db.clear_rating(&hash),
                };
                if let Err(e) = r {
                    glib::g_warning!("vitrine", "set rating {hash}: {e}");
                }
            }
            Request::SetComment { hash, body } => {
                if let Err(e) = db.set_comment(&hash, &body) {
                    glib::g_warning!("vitrine", "set comment {hash}: {e}");
                }
            }
            Request::Tag { name, hashes, add } => {
                let r = if add {
                    db.apply_tag(&name, &hashes)
                } else {
                    db.remove_tag(&name, &hashes)
                };
                if let Err(e) = r {
                    glib::g_warning!("vitrine", "tag {name}: {e}");
                }
            }
        }
    }
}

/// The enrichment driver (main thread). Pulls a batch of un-enriched paths,
/// decodes them concurrently (gated by the shared decode limit), sends each
/// result back to the writer, then repeats until a batch comes back empty.
async fn run_enrichment(requests: async_channel::Sender<Request>) {
    loop {
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        if requests
            .send(Request::TakeBatch { reply: reply_tx })
            .await
            .is_err()
        {
            return;
        }
        let Ok(batch) = reply_rx.recv().await else {
            return;
        };
        if batch.is_empty() {
            return;
        }

        // Decode the whole batch concurrently (the decode gate bounds real
        // parallelism); await all so every Enrich write is enqueued before the
        // next TakeBatch, keeping the queue monotonic.
        let mut handles = Vec::with_capacity(batch.len());
        for path in batch {
            let requests = requests.clone();
            handles.push(glib::spawn_future_local(async move {
                let enrichment = enrich_one(&path).await;
                let _ = requests.send(Request::Enrich { path, enrichment }).await;
            }));
        }
        for handle in handles {
            let _ = handle.await;
        }
    }
}

/// Decode one file and derive its enrichment. A decode failure yields the 0×0
/// sentinel so the writer still clears `width IS NULL` and the file isn't
/// retried forever.
async fn enrich_one(path: &str) -> Enrichment {
    let file = gio::File::for_path(path);
    let Some(probe) = crate::decode::probe(&file, PHASH_PX).await else {
        return Enrichment::default();
    };
    let exif = probe
        .exif
        .as_deref()
        .map(vitrine_engine::parse_exif)
        .unwrap_or_default();
    let phash = phash_from_texture(&probe.frame);
    Enrichment {
        width: probe.width as i64,
        height: probe.height as i64,
        phash,
        format: probe.format,
        date_taken: exif.date_taken,
        camera: exif.camera,
        orientation: exif.orientation,
    }
}

/// Compute the perceptual hash of a decoded frame by downloading its pixels as
/// tightly-packed RGB8 (stride removed) and handing them to the engine.
fn phash_from_texture(texture: &gdk::Texture) -> Option<i64> {
    let width = texture.width() as usize;
    let height = texture.height() as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let mut downloader = gdk::TextureDownloader::new(texture);
    downloader.set_format(gdk::MemoryFormat::R8g8b8);
    let (bytes, stride) = downloader.download_bytes();
    let data = bytes.as_ref();

    let row = width * 3;
    let mut rgb = Vec::with_capacity(row * height);
    for y in 0..height {
        let start = y * stride;
        let end = start + row;
        if end > data.len() {
            return None;
        }
        rgb.extend_from_slice(&data[start..end]);
    }
    vitrine_engine::phash_rgb8(width as u32, height as u32, &rgb)
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
