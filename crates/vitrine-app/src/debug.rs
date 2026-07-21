//! Opt-in instrumentation (`VITRINE_DEBUG`) — a MangoHUD-style readout of the
//! thumbnail pipeline: decode throughput, cache hit rate, render frame time,
//! worst main-loop stall, and RSS. Pure *observation* — relaxed atomic counters
//! plus a periodic sampler; it changes no behaviour and is near-zero cost.
//!
//! Enable with `VITRINE_DEBUG=1`. A stats line (prefix `VDBG`) is written to
//! **stderr** every second, so it forwards to a file with `2>> vitrine-debug.log`
//! — durable, greppable, and analysable after the fact. The window also samples
//! frame time + stall (see `Window::setup_debug_hud`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

static DECODES_INFLIGHT: AtomicU64 = AtomicU64::new(0);
static DECODES_DONE: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
// Enrichment probes are not interactive decodes, so they get their own pair —
// V-23 was misread for three logs because its work was invisible to the HUD.
static ENRICH_INFLIGHT: AtomicU64 = AtomicU64::new(0);
static ENRICH_DONE: AtomicU64 = AtomicU64::new(0);

/// Whether the HUD/log is enabled (`VITRINE_DEBUG` set in the environment).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VITRINE_DEBUG").is_some())
}

/// `VITRINE_NOCACHE`: skip the thumbnail cache reads so every load decodes —
/// a non-destructive way to exercise the *cold* path without wiping the on-disk
/// caches (which are shared with GNOME).
pub fn force_decode() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VITRINE_NOCACHE").is_some())
}

/// A cached thumbnail was served without decoding.
pub fn cache_hit() {
    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}
/// A thumbnail had to be decoded (cache miss).
pub fn cache_miss() {
    CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
}
/// A glycin thumbnail decode started.
pub fn decode_begin() {
    DECODES_INFLIGHT.fetch_add(1, Ordering::Relaxed);
}
/// A glycin thumbnail decode finished (success or failure).
pub fn decode_end() {
    DECODES_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    DECODES_DONE.fetch_add(1, Ordering::Relaxed);
}
/// A background enrichment item started (decode + pHash + cache warm).
pub fn enrich_begin() {
    ENRICH_INFLIGHT.fetch_add(1, Ordering::Relaxed);
}
/// A background enrichment item finished (success or failure).
pub fn enrich_end() {
    ENRICH_INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    ENRICH_DONE.fetch_add(1, Ordering::Relaxed);
}

/// An annotation write was handed to the writer thread: which op, how many rows
/// it carries, how deep the writer queue already was, and whether the writer
/// accepted it.
///
/// `queued` is the head-of-line signal — the writer is a single thread shared
/// with folder scans, so a user write sitting behind a big scan shows up here as
/// a rising queue while the UI has already claimed success.
///
/// `accepted=false` means the channel is closed, i.e. the writer thread is gone.
/// The queue is unbounded, so that is the *only* way a send can fail — and every
/// later write will fail the same way. Blocked and dead look identical in the UI;
/// they are told apart here.
pub fn write(op: &str, rows: usize, queued: usize, accepted: bool) {
    if enabled() {
        eprintln!(
            "VDBG-WRITE ms={} op={op} rows={rows} queued={queued} accepted={accepted}",
            since_start_ms()
        );
    }
}

/// A drag was prepared on a grid cell. `hash=false` means the cell had no content
/// hash yet, so the drag is silently refused — the item is not indexed (or not
/// stamped) rather than the drag being broken.
pub fn drag_prepare(hash: bool) {
    if enabled() {
        eprintln!("VDBG-DRAG ms={} hash={hash}", since_start_ms());
    }
}

/// Something was dropped on a drop target: which target, the payload type that
/// actually arrived, and how many items resolved out of it. `items=0` on a
/// well-formed drop means the payload could not be resolved to indexed images.
pub fn drop_event(target: &str, payload: &str, items: usize) {
    if enabled() {
        eprintln!(
            "VDBG-DROP ms={} target={target} payload={payload} items={items}",
            since_start_ms()
        );
    }
}

/// How a file drop resolved, broken down by *why* each file did or didn't land.
///
/// An empty result has several unrelated causes — the files aren't indexed, they
/// couldn't be read, or the index wouldn't open — and `items=0` alone cannot tell
/// them apart. `by_hash` counts files matched on content after their path missed,
/// which is the portal-path case.
pub fn drop_resolution(
    resolved: usize,
    by_path: usize,
    by_hash: usize,
    unhashable: usize,
    unknown: usize,
) {
    if enabled() {
        eprintln!(
            "VDBG-DROP ms={} target=catalog payload=files-resolved items={resolved} \
             by_path={by_path} by_hash={by_hash} unreadable={unhashable} not_indexed={unknown}",
            since_start_ms()
        );
    }
}

/// A tag apply/remove was issued from the UI, before it reaches the writer.
/// Pairs with the `VDBG-WRITE op=tag` line that follows it.
pub fn tag_action(op: &str, name: &str, items: usize) {
    if enabled() {
        eprintln!(
            "VDBG-TAG ms={} op={op} name={name:?} items={items}",
            since_start_ms()
        );
    }
}

/// A snapshot of the counters at one instant.
#[derive(Clone, Copy)]
pub struct Counters {
    pub inflight: u64,
    pub done: u64,
    pub hits: u64,
    pub misses: u64,
    pub enrich_inflight: u64,
    pub enrich_done: u64,
}

pub fn snapshot() -> Counters {
    Counters {
        inflight: DECODES_INFLIGHT.load(Ordering::Relaxed),
        done: DECODES_DONE.load(Ordering::Relaxed),
        hits: CACHE_HITS.load(Ordering::Relaxed),
        misses: CACHE_MISSES.load(Ordering::Relaxed),
        enrich_inflight: ENRICH_INFLIGHT.load(Ordering::Relaxed),
        enrich_done: ENRICH_DONE.load(Ordering::Relaxed),
    }
}

/// Milliseconds since process start (monotonic) — timestamps for VDBG-* lines.
pub fn since_start_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Resident set size in MB (Linux `/proc/self/statm`; field 2 = resident pages).
pub fn rss_mb() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let pages: u64 = s
        .split_whitespace()
        .nth(1)
        .and_then(|f| f.parse().ok())
        .unwrap_or(0);
    pages * 4 / 1024 // pages * 4096 bytes / (1024*1024)
}
