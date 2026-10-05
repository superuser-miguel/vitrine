#!/usr/bin/env bash
# Publish Vitrine to its signed, auto-updating Flatpak repo.
#
#   build-aux/publish-repo.sh          # build + sign + force-push vitrine-repo
#
# Vitrine ships two ways: a one-off .flatpak bundle on GitHub Releases, and
# this hosted OSTree repo at https://superuser-miguel.github.io/vitrine-repo/
# that `flatpak update` tracks.
#
# Layout (shared with septima-repo, foresight-repo and the rest): the published
# repo is regenerated wholesale and force-pushed as a single commit each
# release, so its git history never accumulates superseded, content-addressed
# OSTree objects. It is a separate GitHub repo from the code, so the source
# history stays free of published binaries.
#
# Run it after the release tag is pushed and the release manifest is pinned to
# it: the build fetches that tag from GitHub, so the repo and the bundle are
# built from identical sources.
#
# Prerequisites:
#   - flatpak-builder, ostree, git, gpg
#   - the signing secret key in the local GPG keyring (see KEY). Losing it means
#     no trusted update can ever be published to this remote again.
#   - push access to git@github.com:superuser-miguel/vitrine-repo.git

set -euo pipefail
cd "$(dirname "$0")/.."

KEY="${VITRINE_GPG_KEY:-D67DB8E03D50A8C0}"   # signs the OSTree repo; the public key is baked into the .flatpakref
MANIFEST="build-aux/io.github.superuser_miguel.Vitrine.release.yml"
APP="io.github.superuser_miguel.Vitrine"
BRANCH="master"                                # what the manifest exports (no `branch:` key → master)
PAGES_URL="https://superuser-miguel.github.io/vitrine-repo"
SITE_URL="https://superuser-miguel.github.io/vitrine"
PUBLISH_REMOTE="git@github.com:superuser-miguel/vitrine-repo.git"

gpg --list-secret-keys "$KEY" >/dev/null 2>&1 \
    || { echo "error: no secret key $KEY — cannot sign the repo" >&2; exit 1; }

# Must share a filesystem with the flatpak-builder state dir, so NOT /tmp:
# that is tmpfs here, which flatpak-builder rejects ("state dir is not on the
# same filesystem as the target dir") and which would hold the whole cargo
# build in RAM. Kept inside the project and gitignored (/.publish-tmp.*).
here="$PWD"
work="$(mktemp -d "$here/.publish-tmp.XXXXXX")"
trap 'rm -rf "$work"' EXIT
repo="$work/repo"

echo ">> Building the signed release into a fresh OSTree repo…"
flatpak-builder --user --force-clean --state-dir="$work/state" \
    --repo="$repo" --gpg-sign="$KEY" "$work/build-dir" "$MANIFEST"

# Debug symbols are ~2x the app and the bundle carries none, so dropping the
# ref keeps both distribution channels byte-identical.
ostree --repo="$repo" refs --delete "runtime/$APP.Debug/x86_64/$BRANCH" 2>/dev/null || true

# Regenerates appstream + summary and signs both. Without a signed summary a
# client with GPGKey set refuses the remote outright.
echo ">> Generating static deltas + signing the summary…"
flatpak build-update-repo --generate-static-deltas --prune \
    --title="Vitrine" --default-branch="$BRANCH" \
    --gpg-sign="$KEY" "$repo"

echo ">> Assembling the publish tree (repo + .flatpakref + landing page)…"
pub="$work/publish"
mkdir -p "$pub"
cp -a "$repo" "$pub/repo"
touch "$pub/.nojekyll"   # serve OSTree byte-for-byte; Jekyll would rewrite it
cp "data/icons/hicolor/scalable/apps/$APP.svg" "$pub/icon.svg"

# Both files embed the public key: regenerate them whenever the signing key
# changes, not only when the app does.
PUB="$(gpg --export "$KEY" | base64 -w0)"

cat > "$pub/vitrine.flatpakref" <<EOF
[Flatpak Ref]
Title=Vitrine
Name=$APP
Branch=$BRANCH
Url=$PAGES_URL/repo/
Homepage=$SITE_URL/
Comment=Browse, review and tag your image collection
Description=A fast, catalog-aware image browser and reviewer for GNOME. Tags, ratings and collections follow your images by content, so they survive renames and moves.
Icon=$PAGES_URL/icon.svg
IsRuntime=false
RuntimeRepo=https://flathub.org/repo/flathub.flatpakrepo
SuggestRemoteName=vitrine
GPGKey=$PUB
EOF

cat > "$pub/vitrine.flatpakrepo" <<EOF
[Flatpak Repo]
Title=Vitrine
Url=$PAGES_URL/repo/
Homepage=$SITE_URL/
Comment=Signed OSTree remote for Vitrine releases
Description=The official Vitrine repository. Adding it lets flatpak update pull new versions.
Icon=$PAGES_URL/icon.svg
DefaultBranch=$BRANCH
GPGKey=$PUB
EOF

cat > "$pub/index.html" <<'EOF'
<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1"><title>Vitrine — Flatpak repo</title>
<style>:root{color-scheme:light dark}body{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;line-height:1.6}code,pre{background:#8881;border-radius:4px}code{padding:.1em .3em}pre{padding:.75em 1em;overflow-x:auto}</style>
<h1>Vitrine — signed Flatpak repo</h1>
<p>Automatic updates for <a href="https://superuser-miguel.github.io/vitrine/">Vitrine</a>, the catalog-aware image browser for GNOME.</p>
<pre><code>flatpak install --user https://superuser-miguel.github.io/vitrine-repo/vitrine.flatpakref
flatpak run io.github.superuser_miguel.Vitrine</code></pre>
<p>Updates then arrive with <code>flatpak update</code>. The repo is signed with the project's GPG key, which the .flatpakref carries.</p>
<p>Installed an earlier version from a <code>.flatpak</code> bundle? Uninstall it once, then install from the link above to start receiving updates.</p>
EOF

echo ">> Force-pushing as a single squashed commit…"
version="$(git describe --tags --abbrev=0 2>/dev/null || date +%Y-%m-%d)"
git -C "$pub" init -q -b main
git -C "$pub" add -A
git -C "$pub" -c user.name=superuser-miguel \
    -c user.email=16271056+superuser-miguel@users.noreply.github.com \
    -c commit.gpgsign=true -c user.signingkey="$KEY" \
    commit -q -m "Publish Vitrine ${version} — signed OSTree repo + .flatpakref"
git -C "$pub" remote add origin "$PUBLISH_REMOTE"
git -C "$pub" push -u --force origin main

echo
echo ">> Done. Verify from the public URL (Pages can take a minute):"
echo "   curl -o /dev/null -w '%{http_code}\\n' ${PAGES_URL}/repo/summary"
echo "   flatpak install --user ${PAGES_URL}/vitrine.flatpakref"
