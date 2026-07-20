#!/usr/bin/env bash
#
# debug-run.sh — launch Vitrine with the live performance HUD.
#
# Streams the VITRINE_DEBUG pipeline stats once a second (fps / worst frame time /
# worst main-loop stall / in-flight decodes / queue depth / cache hit-rate / RSS),
# both to your terminal and to a timestamped log in Troubleshoot/, so you can watch
# when the UI slows down — and share the log afterward.
#
#   build-aux/debug-run.sh [FOLDER]           # normal run (warm cache)
#   build-aux/debug-run.sh --cold [FOLDER]    # force every thumbnail to decode
#   build-aux/debug-run.sh --interact [FOLDER] # tag/drag/drop/write probes only
#
# Then just use the app normally, reproduce the slowdown, and watch the numbers.
# --cold is non-destructive: it skips the cache *reads* (so everything decodes)
# without touching your on-disk thumbnail caches.
#
# --interact drops the per-thumbnail fill probes (FILMBIND/FILL/GRIDFILL), which
# run to tens of thousands of lines and bury everything else. Use it when you are
# testing tagging, dragging, or collections rather than scroll performance.
set -uo pipefail

APP=io.github.superuser_miguel.Vitrine
# One timestamped log per run, kept in the repo's Troubleshoot/ alongside the
# issue register — not dropped in $HOME. Sortable 24h stamp so runs order
# chronologically (a 12h stamp puts 01:04pm before 12:37pm).
REPO="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
LOGDIR="${VITRINE_LOGDIR:-$REPO/Troubleshoot}"
mkdir -p "$LOGDIR"
LOG="$LOGDIR/vitrine-debug_$(date +%Y-%m-%d_%H-%M-%S).log"
RUNLOG="$(mktemp)"
ENVFLAGS=(--env=VITRINE_DEBUG=1)
MODE="warm"
# Everything worth keeping. --interact narrows this to the interaction probes.
KEEP='VDBG|SOAK|OFTEST|panic|CRITICAL|WARNING'
ARGS=()
for a in "$@"; do
  case "$a" in
    --cold)   ENVFLAGS+=(--env=VITRINE_NOCACHE=1); MODE="cold" ;;
    --interact)
      KEEP='VDBG-(WRITE|TAG|DROP|DRAG)|^VDBG fps|panic|CRITICAL|WARNING'
      MODE="interact" ;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)        ARGS+=("$a") ;;
  esac
done

{
  echo
  echo "================ run $(date)  mode=$MODE ================"
} >> "$LOG"

echo "Vitrine debug HUD  (mode: $MODE)"
echo "  log:     $LOG"
echo "  columns: fps  frame_max  stall  decode[live/done/rate]  queued  cache_hit  rss"
echo "  live=in-flight decodes (healthy ≈ ≤32);  stall=worst main-loop freeze this second"
echo "  → use the app, reproduce the slowdown; Ctrl-C or close the app to stop."
echo

# Merge stderr, keep only the HUD lines, show them live AND append to both logs.
flatpak run "${ENVFLAGS[@]}" "$APP" "${ARGS[@]}" 2>&1 \
  | grep --line-buffered -E "$KEEP" \
  | tee -a "$LOG" "$RUNLOG" || true

# Count a probe field, tolerating a run where that probe never fired.
count() { grep -c -- "$1" "$RUNLOG" 2>/dev/null || echo 0; }

echo
echo "=== summary (this run) ==="
printf "  peak in-flight decodes : %s\n"  "$(grep -oP 'live=\K[0-9]+'      "$RUNLOG" | sort -n | tail -1)"
printf "  worst main-loop stall  : %s ms\n" "$(grep -oP 'stall=\K[0-9]+'   "$RUNLOG" | sort -n | tail -1)"
printf "  worst frame time       : %s ms\n" "$(grep -oP 'frame_max=\K[0-9]+' "$RUNLOG" | sort -n | tail -1)"
printf "  lowest fps             : %s\n"  "$(grep -oP 'fps=\K[0-9]+'       "$RUNLOG" | sort -n | head -1)"
printf "  peak RSS               : %s MB\n" "$(grep -oP 'rss=\K[0-9]+'      "$RUNLOG" | sort -n | tail -1)"

echo
echo "=== writes / interaction ==="
printf "  annotation writes      : %s accepted, %s DROPPED\n" \
  "$(count 'accepted=true')" "$(count 'accepted=false')"
printf "  peak writer queue      : %s\n" \
  "$(grep -oP 'VDBG-WRITE .*queued=\K[0-9]+' "$RUNLOG" | sort -n | tail -1)"
printf "  tag actions            : %s\n"  "$(count 'VDBG-TAG')"
printf "  drops handled          : %s (%s resolved nothing)\n" \
  "$(count 'VDBG-DROP')" "$(grep -c 'VDBG-DROP.*items=0' "$RUNLOG" 2>/dev/null || echo 0)"
printf "  drags refused (no hash): %s\n"  "$(count 'VDBG-DRAG ms=.*hash=false')"

# These two are the whole point of the write probe — call them out explicitly.
if [ "$(count 'accepted=false')" -gt 0 ]; then
  echo
  echo "  ⚠  Writes were DROPPED — the index writer thread has exited."
  echo "     Every annotation after that point was silently lost."
fi
if [ "$(grep -oP 'VDBG-WRITE .*queued=\K[0-9]+' "$RUNLOG" | sort -n | tail -1 | awk '$1>100')" ]; then
  echo
  echo "  ⚠  Writer queue exceeded 100 — user writes are queued behind a scan."
  echo "     Tags/drops will land eventually, but not when the toast says so."
fi

echo "  (full log: $LOG — share it or let Claude read it)"
rm -f "$RUNLOG"
