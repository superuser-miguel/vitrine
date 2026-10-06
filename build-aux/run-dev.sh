#!/bin/sh
# Build the dev manifest and run it from the tree, WITHOUT installing it.
#
# The installed Vitrine is the published one, from the hosted repo. A dev build
# installed beside it (`flatpak-builder --install` lands on the same `master`
# branch) would replace it, and a --user install shadows the system one. So dev
# builds run from build-dir instead, and the installed app stays untouched.
#
# This is `flatpak-builder --run` plus what it leaves out: the document-portal
# mount (a folder picked through the file chooser comes back as a
# /run/user/$UID/doc/… path, which must exist inside the sandbox) and the app's
# own data dir (--with-appdir), so the dev build sees the same library, tags and
# settings as the installed one. The sandbox permissions are read from the
# manifest, so it runs with what ships and nothing wider.
#
# Usage:  build-aux/run-dev.sh [--no-build] [FOLDER]
#         build-aux/run-dev.sh                    # build, then run the app
#         build-aux/run-dev.sh --no-build         # run the last build as is
#         build-aux/run-dev.sh ~/Pictures/Foo     # open a folder at launch
#         build-aux/run-dev.sh --cmd sh           # a shell in the dev sandbox
#
# VITRINE_* variables in the environment are passed into the sandbox, e.g.
#         VITRINE_DEBUG=1 build-aux/run-dev.sh --no-build
# (build-aux/debug-run.sh --dev does that for you, with the HUD and a log.)
set -eu

HERE="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$HERE/build-aux/io.github.superuser_miguel.Vitrine.yml"
BUILD_DIR="$HERE/build-dir"
STATE_DIR="$HERE/.flatpak-builder"
LOG="$STATE_DIR/run-dev.log"
APP=io.github.superuser_miguel.Vitrine

build=1
if [ "${1:-}" = "--no-build" ]; then
    build=0
    shift
fi

if [ "$build" = 1 ]; then
    # Minutes from cold, seconds when cached. It must not look hung: an
    # interrupted build empties build-dir and leaves nothing for --no-build.
    mkdir -p "$STATE_DIR"
    echo "Building (a few minutes from cold, seconds when cached). Don't interrupt:" >&2
    echo "build-dir is emptied first. Full log: .flatpak-builder/run-dev.log" >&2
    # --state-dir: flatpak-builder caches in the *current* directory by
    # default, so running this from anywhere but the repo root would start a
    # second, cold cache there. --disable-rofiles-fuse: works with or without FUSE.
    if ! flatpak-builder --user --force-clean --disable-rofiles-fuse \
        --state-dir="$STATE_DIR" \
        "$BUILD_DIR" "$MANIFEST" 2>&1 | tee "$LOG" \
        | grep --line-buffered -E '^(Building module|Cache hit|Starting build|Committing stage|Finishing|Pruning)' >&2
    then
        :   # grep's status says nothing about the build; the check below does
    fi
    if [ ! -x "$BUILD_DIR/files/bin/vitrine" ]; then
        tail -n 40 "$LOG" >&2
        echo "build failed — full log: .flatpak-builder/run-dev.log" >&2
        exit 1
    fi
fi

if [ ! -x "$BUILD_DIR/files/bin/vitrine" ]; then
    echo "No finished build in build-dir (never built, or a build was interrupted)." >&2
    echo "Run this again without --no-build." >&2
    exit 1
fi

# Vitrine is a single-instance GApplication: if the installed copy is already
# running, launching this one just hands the request to it and exits — and you
# would be testing the installed build without knowing it.
if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
    --method org.freedesktop.DBus.NameHasOwner "$APP" 2>/dev/null | grep -q true; then
    echo "Vitrine is already running. Quit it first (Ctrl+Q), then run this again —" >&2
    echo "otherwise the launch goes to that copy, not to this dev build." >&2
    exit 1
fi

# Every `  - --flag` under finish-args:, trailing comments dropped.
finish_args="$(awk '/^finish-args:/{p=1;next} p&&/^[^ #]/{p=0} p&&/^  - --/{print $2}' "$MANIFEST")"

# `flatpak build` ignores overrides, so carry over the *global* filesystem
# overrides (user + system: GTK config, themes). Without them the dev build can
# look different from the installed app, which makes side-by-side checks unfair.
override_fs=""
for scope in --user --system; do
    for fs in $(flatpak override "$scope" --show 2>/dev/null \
        | sed -n 's/^filesystems=//p' | tr ';' ' '); do
        override_fs="$override_fs --filesystem=$fs"
    done
done

# Pass VITRINE_* through (flatpak build does not inherit the host environment).
env_args=""
for var in $(env | sed -n 's/^\(VITRINE_[A-Z0-9_]*\)=.*/\1/p'); do
    eval "val=\${$var}"
    env_args="$env_args --env=$var=$val"
done

if [ "${1:-}" = "--cmd" ]; then
    shift
else
    set -- vitrine "$@"
fi

# shellcheck disable=SC2086  # flag lists, split on purpose
exec flatpak build --with-appdir --allow=devel \
    --talk-name='org.freedesktop.portal.*' --talk-name=org.a11y.Bus \
    --filesystem="/run/user/$(id -u)/doc" \
    $finish_args $override_fs $env_args "$BUILD_DIR" "$@"
