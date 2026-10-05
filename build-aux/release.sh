#!/usr/bin/env bash
# Release checklist for Vitrine — checks what can be checked, edits what is
# mechanical, and prints the rest so a release never depends on memory.
#
#   build-aux/release.sh [check] [--full]   # read-only checklist (default)
#   build-aux/release.sh bump X.Y.Z         # set the version in all three places
#   build-aux/release.sh pin                # pin the release manifest to vX.Y.Z
#   build-aux/release.sh steps              # print the remaining manual commands
#
# The flow, in order:
#   1. release.sh bump X.Y.Z, then write the <release> notes in the metainfo
#   2. cargo check (Cargo.lock follows); regenerate build-aux/cargo-sources.json
#      if the external crate set changed — `check` diffs the two for you
#   3. release.sh check --full, until every item is ✓
#   4. commit, then follow `release.sh steps`: tag, push, `pin`, publish-repo,
#      bundle, gh release, verify the hosted repo
#
# Nothing here commits, tags, pushes or builds. `bump` and `pin` only edit
# files in the working tree, so their result shows up in `git diff` for review.

set -euo pipefail
cd "$(dirname "$0")/.."

APP="io.github.superuser_miguel.Vitrine"
KEY="${VITRINE_GPG_KEY:-D67DB8E03D50A8C0}"
METAINFO="data/$APP.metainfo.xml.in.in"
MANIFEST="build-aux/$APP.release.yml"
SOURCES="build-aux/cargo-sources.json"
PAGES_URL="https://superuser-miguel.github.io/vitrine-repo"
BRANCH="master"
# A metainfo release date further than this from today is probably a stale
# copy-paste rather than a deliberate pre-/post-date.
DATE_PAST_DAYS=30
DATE_FUTURE_DAYS=7

usage() { sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; }
die() { echo "error: $*" >&2; exit 1; }

# ---------------------------------------------------------------- versions ---

cargo_version() {
    python3 - <<'EOF'
import tomllib
with open("Cargo.toml", "rb") as f:
    print(tomllib.load(f)["workspace"]["package"]["version"])
EOF
}

meson_version() {
    sed -n "1,10s/^[[:space:]]*version:[[:space:]]*'\([^']*\)'.*/\1/p" meson.build | head -n1
}

# Prints "version date" of the newest (first) <release>, or nothing.
metainfo_top_release() {
    python3 - "$METAINFO" <<'EOF'
import sys, xml.etree.ElementTree as ET
rels = ET.parse(sys.argv[1]).getroot().find("releases")
r = rels.find("release") if rels is not None else None
if r is not None:
    print(r.get("version", ""), r.get("date", ""))
EOF
}

# Workspace crates' versions as recorded in Cargo.lock (no `source` line).
lock_workspace_versions() {
    python3 - <<'EOF'
import tomllib
with open("Cargo.lock", "rb") as f:
    lock = tomllib.load(f)
print(" ".join(sorted({p["version"] for p in lock["package"] if "source" not in p})))
EOF
}

# ------------------------------------------------------------------- check ---

FAIL=0
if [[ -t 1 ]]; then G=$'\e[32m' R=$'\e[31m' Y=$'\e[33m' B=$'\e[1m' N=$'\e[0m'; else G= R= Y= B= N=; fi
ok()   { printf '  %s✓%s %s\n' "$G" "$N" "$*"; }
bad()  { printf '  %s✗%s %s\n' "$R" "$N" "$*"; FAIL=1; }
warn() { printf '  %s!%s %s\n' "$Y" "$N" "$*"; }   # worth a look, does not block
note() { printf '      %s\n' "$*"; }
section() { printf '\n%s%s%s\n' "$B" "$*" "$N"; }

check_versions() {
    section "Version"
    local cv mv mi_v mi_d lv
    cv="$(cargo_version)"; mv="$(meson_version)"
    read -r mi_v mi_d <<<"$(metainfo_top_release)" || true
    lv="$(lock_workspace_versions)"
    VERSION="$cv"
    if [[ "$cv" == "$mv" && "$cv" == "${mi_v:-}" ]]; then
        ok "Cargo.toml, meson.build and metainfo agree: $cv"
    else
        bad "version fields disagree"
        note "Cargo.toml [workspace.package]: $cv"
        note "meson.build project():          ${mv:-<not found>}"
        note "metainfo newest <release>:      ${mi_v:-<none>}"
    fi
    if [[ "$lv" == "$cv" ]]; then
        ok "Cargo.lock workspace crates at $cv"
    else
        bad "Cargo.lock workspace crates at '$lv', not $cv — run cargo check"
    fi
}

# Is there a <release> for this version, with a believable date and real notes?
check_metainfo_release() {
    local verdict kind d delta
    verdict="$(python3 - "$METAINFO" "$VERSION" "$DATE_PAST_DAYS" "$DATE_FUTURE_DAYS" <<'EOF'
import sys, datetime, xml.etree.ElementTree as ET
path, ver, past, future = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
for r in ET.parse(path).getroot().iter("release"):
    if r.get("version") == ver:
        break
else:
    print("MISSING - -"); sys.exit()
d = r.get("date", "")
try:
    delta = (datetime.date.fromisoformat(d) - datetime.date.today()).days
except ValueError:
    print(f"BADDATE {d or '-'} -"); sys.exit()
text = "".join(r.itertext())
if not (-past <= delta <= future):
    print(f"FARDATE {d} {delta}")
elif "TODO" in text or not text.strip():
    print(f"TODO {d} {delta}")
else:
    print(f"OK {d} {delta}")
EOF
)"
    read -r kind d delta <<<"$verdict"
    case "$kind" in
        MISSING) bad "metainfo has no <release version=\"$VERSION\">" ;;
        BADDATE) bad "metainfo <release $VERSION> date '$d' is not YYYY-MM-DD" ;;
        FARDATE) bad "metainfo <release $VERSION> dated $d (${delta#-} days $([[ $delta -lt 0 ]] && echo ago || echo ahead)) — update to the release day" ;;
        TODO)    bad "metainfo <release $VERSION> description is empty or still says TODO" ;;
        OK)      if [[ "$delta" == 0 ]]; then ok "metainfo <release $VERSION> dated today ($d)"
                 else ok "metainfo <release $VERSION> dated $d (${delta#-} days $([[ $delta -lt 0 ]] && echo ago || echo ahead))"; fi ;;
        *)       bad "could not read the metainfo release entry" ;;
    esac
}

check_cargo_sources() {
    section "Vendored crates ($SOURCES)"
    local out
    if out="$(python3 - "$SOURCES" <<'EOF'
import sys, json, tomllib, os
with open("Cargo.lock", "rb") as f:
    lock = tomllib.load(f)
want, other = set(), []
for p in lock["package"]:
    src = p.get("source")
    if src is None:
        continue                      # workspace member
    if src.startswith("registry+"):
        want.add(f'{p["name"]}-{p["version"]}')
    else:
        other.append(f'{p["name"]} ({src.split("#")[0]})')
have = set()
for s in json.load(open(sys.argv[1])):
    url = s.get("url", "")
    if s.get("type") == "archive" and url.endswith(".crate"):
        have.add(os.path.basename(url)[: -len(".crate")])
missing, extra = sorted(want - have), sorted(have - want)
print(f"COUNT {len(want)} {len(have)}")
for m in missing: print(f"MISSING {m}")
for e in extra:   print(f"EXTRA {e}")
for o in other:   print(f"OTHER {o}")
sys.exit(1 if missing else 0)
EOF
)"; then :; fi
    local n_want n_have
    read -r _ n_want n_have <<<"$(grep '^COUNT' <<<"$out")"
    local missing extra other
    missing="$(sed -n 's/^MISSING //p' <<<"$out")"
    extra="$(sed -n 's/^EXTRA //p' <<<"$out")"
    other="$(sed -n 's/^OTHER //p' <<<"$out")"
    if [[ -z "$missing" ]]; then
        ok "all $n_want registry crates in Cargo.lock are vendored"
    else
        bad "$(wc -l <<<"$missing") crate(s) in Cargo.lock missing from $SOURCES — regenerate it:"
        while read -r m; do note "$m.crate"; done <<<"$missing"
        note "(see \`release.sh steps\` for the flatpak-cargo-generator command)"
    fi
    if [[ -n "$extra" ]]; then
        warn "$(wc -l <<<"$extra") vendored crate(s) no longer in Cargo.lock (harmless, but stale — regenerate):"
        while read -r e; do note "$e.crate"; done <<<"$extra"
    fi
    if [[ -n "$other" ]]; then
        warn "non-registry crates not diffed (check them in $SOURCES by hand):"
        while read -r o; do note "$o"; done <<<"$other"
    fi
}

check_git() {
    section "Git"
    local branch dirty untracked ahead behind
    branch="$(git rev-parse --abbrev-ref HEAD)"
    if [[ "$branch" == main ]]; then ok "on main"; else bad "on '$branch', not main"; fi

    dirty="$(git status --porcelain --untracked-files=no)"
    untracked="$(git status --porcelain | grep -c '^??' || true)"
    if [[ -z "$dirty" ]]; then
        ok "no uncommitted changes to tracked files"
    else
        bad "uncommitted changes:"
        while read -r l; do note "$l"; done <<<"$dirty"
    fi
    [[ "$untracked" -gt 0 ]] && warn "$untracked untracked path(s) (not part of the release; ignored)"

    # Compares against the last fetch: run `git fetch origin` first for a live answer.
    if git rev-parse -q --verify origin/main >/dev/null; then
        read -r behind ahead <<<"$(git rev-list --left-right --count origin/main...HEAD)"
        if [[ "$behind" -gt 0 ]]; then
            bad "$behind commit(s) behind origin/main — pull first"
        elif [[ "$ahead" -gt 0 ]]; then
            ok "$ahead commit(s) ahead of origin/main (as of last fetch)"
        else
            ok "in sync with origin/main (as of last fetch)"
        fi
    else
        bad "no origin/main ref — git fetch origin"
    fi

    if gpg --list-secret-keys "$KEY" >/dev/null 2>&1; then
        ok "signing key $KEY present"
    else
        bad "no secret key $KEY — cannot sign the tag or the OSTree repo"
    fi
    local sk; sk="$(git config user.signingkey || true)"
    if [[ "$sk" == "$KEY" || "$sk" == *"$KEY" ]]; then
        ok "user.signingkey = $sk"
    else
        bad "user.signingkey is '${sk:-<unset>}', expected $KEY"
    fi
    local k v
    for k in commit.gpgsign tag.gpgsign; do
        v="$(git config --bool "$k" || true)"
        if [[ "$v" == true ]]; then ok "$k = true"; else bad "$k is '${v:-<unset>}', must be true"; fi
    done
}

check_rust() {
    section "Rust gate (--full)"
    local log; log="$(mktemp)"
    local -a cmd
    local label
    for label in "cargo fmt --all --check" \
                 "cargo clippy --all-targets -- -D warnings" \
                 "cargo test --all"; do
        read -ra cmd <<<"$label"
        if "${cmd[@]}" >"$log" 2>&1; then
            ok "$label"
        else
            bad "$label"
            tail -n 20 "$log" | sed 's/^/      | /'
        fi
    done
    rm -f "$log"
}

check_metainfo_valid() {
    section "AppStream"
    if ! command -v appstreamcli >/dev/null; then
        bad "appstreamcli not installed — cannot validate the metainfo"
        return
    fi
    local tmp out
    tmp="$(mktemp --suffix=.metainfo.xml)"
    sed "s/@app-id@/$APP/g" "$METAINFO" >"$tmp"
    # Pedantic notes don't fail validation (exit 0); one is known and accepted.
    if out="$(appstreamcli validate --no-net "$tmp" 2>&1)"; then
        ok "appstreamcli validate --no-net: $(tail -n1 <<<"$out" | sed 's/^✔ //')"
    else
        bad "appstreamcli validate --no-net failed:"
        sed 's/^/      | /' <<<"$out"
    fi
    rm -f "$tmp"
}

# Pin as recorded in the release manifest: "tag commit".
manifest_pin() {
    python3 - "$MANIFEST" <<'EOF'
import sys, re
VITRINE_SRC = r"(^\s+url:\s*\S+/vitrine\.git\s*\n\s+tag:\s*)(\S+)(\s*\n\s+commit:\s*)([0-9a-f]+)"
# The vitrine source block only — other modules (blueprint) have tags too.
m = re.findall(VITRINE_SRC, open(sys.argv[1]).read(), re.M)
print(*(m[0][1:4:2] if len(m) == 1 else ("?", "?")))
EOF
}

check_pin() {
    section "Release manifest pin"
    local tag="v$VERSION" ptag pcommit tsha
    read -r ptag pcommit <<<"$(manifest_pin)"
    if ! tsha="$(git rev-parse -q --verify "refs/tags/$tag^{commit}")"; then
        if [[ "$ptag" == "$tag" ]]; then
            bad "manifest pins $tag but no such tag exists locally"
        else
            ok "$tag not tagged yet; manifest still pins $ptag (pin after tagging)"
        fi
        return
    fi
    if [[ "$ptag" == "$tag" && "$pcommit" == "$tsha" ]]; then
        ok "manifest pins $tag @ ${tsha:0:12}"
    else
        bad "manifest pins $ptag @ ${pcommit:0:12}, but $tag is ${tsha:0:12} — run release.sh pin"
    fi
    if git rev-parse -q --verify "refs/tags/$tag" >/dev/null \
       && [[ "$(git cat-file -t "refs/tags/$tag")" == tag ]] \
       && git verify-tag "$tag" >/dev/null 2>&1; then
        ok "$tag is a signed, verifiable tag"
    else
        bad "$tag is not a signed annotated tag (git tag -s)"
    fi
    if git merge-base --is-ancestor "$tsha" HEAD; then :; else
        warn "$tag is not an ancestor of HEAD"
    fi
}

cmd_check() {
    local full=0
    for a in "$@"; do
        case "$a" in
            --full) full=1 ;;
            *) die "unknown option for check: $a" ;;
        esac
    done
    echo "${B}Vitrine release checklist${N}  ($(date +%F))"
    check_versions
    check_metainfo_release
    check_cargo_sources
    check_git
    if [[ "$full" == 1 ]]; then check_rust; else
        section "Rust gate"; warn "skipped — rerun with --full for fmt, clippy and tests"
    fi
    check_metainfo_valid
    check_pin
    echo
    if [[ "$FAIL" == 0 ]]; then
        echo "${G}Ready:${N} nothing blocks v$VERSION. Next: build-aux/release.sh steps"
    else
        echo "${R}Blocked:${N} fix the ✗ items above."
    fi
    return "$FAIL"
}

# -------------------------------------------------------------------- bump ---

cmd_bump() {
    local new="${1:-}"
    [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "usage: release.sh bump X.Y.Z"
    local old; old="$(cargo_version)"
    [[ "$new" != "$old" ]] || die "already at $new"
    local today; today="$(date +%F)"
    python3 - "$new" "$today" "$METAINFO" <<'EOF'
import sys, re
new, today, metainfo = sys.argv[1:]

def sub_once(path, pattern, repl, flags=0):
    text = open(path).read()
    out, n = re.subn(pattern, repl, text, count=1, flags=flags)
    if n != 1:
        sys.exit(f"error: no version field found in {path}")
    open(path, "w").write(out)

# Only the [workspace.package] table's version, not rust-version or deps.
sub_once("Cargo.toml",
         r'(^\[workspace\.package\][^\[]*?^version\s*=\s*")[^"]*(")',
         rf"\g<1>{new}\g<2>", re.M | re.S)
# project()'s version, the first `version:` in meson.build.
sub_once("meson.build", r"(^\s*version:\s*')[^']*(')", rf"\g<1>{new}\g<2>", re.M)
# Workspace members in Cargo.lock (no `source`), as `cargo check` would.
lock = open("Cargo.lock").read()
lock = re.sub(r'(\[\[package\]\]\nname = "vitrine-[^"]*"\nversion = ")[^"]*("\n(?!source))',
              rf"\g<1>{new}\g<2>", lock)
open("Cargo.lock", "w").write(lock)

text = open(metainfo).read()
if re.search(rf'<release\s+version="{re.escape(new)}"', text):
    sys.exit(f"error: metainfo already has a <release> for {new}")
m = re.search(r"^([ \t]*)<releases>\n", text, re.M)
if not m:
    sys.exit("error: no <releases> in metainfo")
i = m.group(1)
entry = (f'{i}  <release version="{new}" date="{today}">\n'
         f'{i}    <description>\n'
         f'{i}      <p>TODO: describe this release.</p>\n'
         f'{i}    </description>\n'
         f'{i}  </release>\n')
open(metainfo, "w").write(text[:m.end()] + entry + text[m.end():])
EOF
    echo ">> Bumped $old → $new (Cargo.toml, meson.build, Cargo.lock, metainfo dated $today)."
    echo "   Next: write the <release> notes in $METAINFO (replace the TODO),"
    echo "   run cargo check, then build-aux/release.sh check --full."
    echo "   Nothing was committed — review with git diff."
}

# --------------------------------------------------------------------- pin ---

cmd_pin() {
    local version tag sha
    version="$(cargo_version)"; tag="v$version"
    sha="$(git rev-parse -q --verify "refs/tags/$tag^{commit}")" \
        || die "tag $tag does not exist — tag the release first (see release.sh steps)"
    python3 - "$MANIFEST" "$tag" "$sha" <<'EOF'
import sys, re
VITRINE_SRC = r"(^\s+url:\s*\S+/vitrine\.git\s*\n\s+tag:\s*)(\S+)(\s*\n\s+commit:\s*)([0-9a-f]+)"
path, tag, sha = sys.argv[1:]
text, n = re.subn(VITRINE_SRC, rf"\g<1>{tag}\g<3>{sha}", open(path).read(), flags=re.M)
if n != 1:
    sys.exit(f"error: expected one vitrine.git source with tag: + commit: in {path}, found {n}")
open(path, "w").write(text)
EOF
    echo ">> Pinned $MANIFEST to $tag @ $sha."
    echo "   Nothing was committed — commit and push it before publish-repo.sh."
}

# ------------------------------------------------------------------- steps ---

cmd_steps() {
    local v tag
    v="$(cargo_version)"; tag="v$v"
    cat <<EOF
Remaining release steps for Vitrine $v
(run from the repo root; \`release.sh check\` should be all ✓ before step 1)

 0. Only if Cargo.lock's external crates changed — regenerate the vendored sources
    in a scratch dir, copy back, and re-run check:
      d=\$(mktemp -d "\$HOME/.cache/vitrine-cargo-gen.XXXXXX") && cp Cargo.lock "\$d"/
      flatpak run --command=flatpak-cargo-generator --filesystem="\$d" \\
          org.flatpak.Builder "\$d"/Cargo.lock -o "\$d"/cargo-sources.json
      cp "\$d"/cargo-sources.json $SOURCES && rm -rf "\$d"

 1. Commit the release (signed automatically) and tag it:
      git commit -am "Vitrine $v"
      git tag -s $tag -m "Vitrine $v"

 2. Push main and the tag:
      git push origin main $tag

 3. Pin the release manifest to the tag, commit, push:
      build-aux/release.sh pin
      git commit -am "build-aux: pin the release manifest to $tag"
      git push origin main

 4. Publish the signed OSTree repo (vitrine-repo):
      build-aux/publish-repo.sh

 5. Build the offline bundle:
      flatpak-builder --user --force-clean --repo=repo-release build-dir-release \\
          $MANIFEST
      flatpak build-bundle repo-release Vitrine.flatpak $APP \\
          --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo

 6. Create the GitHub release (write the notes first; the metainfo
    <release $v> text is a good start):
      gh release create $tag Vitrine.flatpak --verify-tag \\
          --title "Vitrine $tag" --notes-file release-notes-$tag.md

 7. Verify the hosted repo through a temporary remote (Pages may lag a minute):
      curl -o /dev/null -w '%{http_code}\\n' $PAGES_URL/repo/summary    # expect 200
      flatpak remote-add --user vitrine-verify $PAGES_URL/vitrine.flatpakrepo
      ostree --repo=\$HOME/.local/share/flatpak/repo pull --commit-metadata-only \\
          vitrine-verify app/$APP/x86_64/$BRANCH
      c=\$(ostree --repo=\$HOME/.local/share/flatpak/repo rev-parse \\
          vitrine-verify:app/$APP/x86_64/$BRANCH)
      ostree --repo=\$HOME/.local/share/flatpak/repo show --gpg-verify-remote=vitrine-verify "\$c"
      flatpak remote-delete --user vitrine-verify
EOF
}

# -------------------------------------------------------------------- main ---

sub="${1:-check}"
[[ $# -gt 0 ]] && shift
case "$sub" in
    check) cmd_check "$@" ;;
    bump)  cmd_bump "$@" ;;
    pin)   cmd_pin "$@" ;;
    steps) cmd_steps "$@" ;;
    --full) cmd_check --full "$@" ;;
    -h|--help|help) usage ;;
    *) usage >&2; exit 2 ;;
esac
