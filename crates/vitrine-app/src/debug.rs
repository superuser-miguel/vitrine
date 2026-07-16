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

/// A snapshot of the counters at one instant.
#[derive(Clone, Copy)]
pub struct Counters {
    pub inflight: u64,
    pub done: u64,
    pub hits: u64,
    pub misses: u64,
}

pub fn snapshot() -> Counters {
    Counters {
        inflight: DECODES_INFLIGHT.load(Ordering::Relaxed),
        done: DECODES_DONE.load(Ordering::Relaxed),
        hits: CACHE_HITS.load(Ordering::Relaxed),
        misses: CACHE_MISSES.load(Ordering::Relaxed),
    }
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
