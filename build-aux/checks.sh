#!/bin/sh
# Local mirror of the CI gate (PLAN §0 task 6). Run from the repo root:
#   ./build-aux/checks.sh
set -eu

cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

echo ">> cargo fmt --all --check"
cargo fmt --all --check

echo ">> cargo clippy --all-targets --all-features -D warnings"
cargo clippy --all-targets --all-features -- -D warnings

echo ">> cargo test --all"
cargo test --all

echo ">> engine purity: vitrine-engine must not pull GTK/GLib/adw/ashpd"
# PLAN §1 house rule 2 + §0 task 6. Any match is a boundary violation.
if cargo tree -p vitrine-engine -e normal \
    | grep -Ei '\b(gtk4|glib|gio|libadwaita|adw|ashpd)\b'; then
  echo "!! vitrine-engine has a forbidden UI dependency (see above)" >&2
  exit 1
fi
echo "   ok — engine is UI-free"

echo "All checks passed."
