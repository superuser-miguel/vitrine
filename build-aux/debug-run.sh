#!/usr/bin/env bash
#
# debug-run.sh — launch Vitrine with the live performance HUD.
#
# Streams the VITRINE_DEBUG pipeline stats once a second (fps / worst frame time /
# worst main-loop stall / in-flight decodes / queue depth / cache hit-rate / RSS),
# both to your terminal and to ~/vitrine-debug.log, so you can watch what happens
# when the UI slows down — and share the log afterward.
#
#   build-aux/debug-run.sh [FOLDER]         # normal run (warm cache)
#   build-aux/debug-run.sh --cold [FOLDER]  # force every thumbnail to decode
#
# Then just use the app normally, reproduce the slowdown, and watch the numbers.
# --cold is non-destructive: it skips the cache *reads* (so everything decodes)
# without touching your on-disk thumbnail caches.
set -uo pipefail

APP=io.github.superuser_miguel.Vitrine
LOG="$HOME/vitrine-debug.log"
RUNLOG="$(mktemp)"
ENVFLAGS=(--env=VITRINE_DEBUG=1)
MODE="warm"
ARGS=()
for a in "$@"; do
  case "$a" in
    --cold)   ENVFLAGS+=(--env=VITRINE_NOCACHE=1); MODE="cold" ;;
    -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
  | grep --line-buffered -E 'VDBG|SOAK|OFTEST|panic|CRITICAL' \
  | tee -a "$LOG" "$RUNLOG" || true

echo
echo "=== summary (this run) ==="
printf "  peak in-flight decodes : %s\n"  "$(grep -oP 'live=\K[0-9]+'      "$RUNLOG" | sort -n | tail -1)"
printf "  worst main-loop stall  : %s ms\n" "$(grep -oP 'stall=\K[0-9]+'   "$RUNLOG" | sort -n | tail -1)"
printf "  worst frame time       : %s ms\n" "$(grep -oP 'frame_max=\K[0-9]+' "$RUNLOG" | sort -n | tail -1)"
printf "  lowest fps             : %s\n"  "$(grep -oP 'fps=\K[0-9]+'       "$RUNLOG" | sort -n | head -1)"
printf "  peak RSS               : %s MB\n" "$(grep -oP 'rss=\K[0-9]+'      "$RUNLOG" | sort -n | tail -1)"
echo "  (full log appended to $LOG — share it or let Claude read it)"
rm -f "$RUNLOG"
