#!/usr/bin/env bash
# The fresh-container install smoke: prove that a published asset installs and updates on a
# machine that has never seen this project (docs/spec/99-phase9b-release-kickoff.md,
# deliverable 4). It is the proxy for "a stranger can install it", which no test tier covers:
# every tier runs against the built tree, with the toolchain already on the box.
#
# Each leg starts an empty `ubuntu:24.04` container, installs `curl` and `ca-certificates`,
# downloads the asset and `SHA256SUMS`, verifies the checksum, runs `--version`, then serves a
# newer release from `python3 -m http.server` inside the container and drives
# `lastcall update --check` and `lastcall update` against it through
# `LASTCALL_UPDATE_BASE_URL` (loopback only, which is why the server is inside the container).
# The updated binary's bytes are compared with the served asset's, so the run proves the
# replacement really is what was downloaded and verified.
#
#   scripts/install-smoke.sh [<tag> [<newer-tag>]]
#   scripts/install-smoke.sh --from-dir <dir> [<ignored> ...]
#   scripts/install-smoke.sh --platform linux/amd64 v0.1.0
#
#   <tag>         the release to install. Default: the repository's latest release.
#   <newer-tag>   a second, newer release whose real assets drive the update leg. Without it
#                 the smoke serves the installed binary back under the next patch version, so
#                 the update path is exercised end to end even when only one release exists.
#   --from-dir    take the assets (and `SHA256SUMS`, generated here if absent) from a local
#                 directory instead of the network: a rehearsal's artifacts, fetched with
#                 `gh run download` outside this script. Nothing but the base image and
#                 `apt-get` then touches the network.
#   --platform    run one platform instead of both. Both is the point: without the amd64 leg
#                 on an Apple-silicon host (Rosetta or QEMU), x86_64 Linux ships untested.
#   --keep        keep the scratch directory and print its path.
#
# Two environment overrides, for a machine that cannot reach Docker Hub or a fork:
# `LASTCALL_SMOKE_IMAGE` (default `ubuntu:24.04`) and `LASTCALL_SMOKE_REPO`
# (default `aarontimko/lastcall`). Substituting the image weakens the "fresh machine" claim,
# so say which image a transcript used whenever it is not the default.
set -euo pipefail

repo="${LASTCALL_SMOKE_REPO:-aarontimko/lastcall}"
image="${LASTCALL_SMOKE_IMAGE:-ubuntu:24.04}"
port=8099
platforms=(linux/amd64 linux/arm64)
from_dir=""
keep=0
args=()

die() { echo "install-smoke: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --from-dir) [ $# -ge 2 ] || die "--from-dir needs a directory"; from_dir="$2"; shift 2 ;;
        --platform) [ $# -ge 2 ] || die "--platform needs a value"; platforms=("$2"); shift 2 ;;
        --keep) keep=1; shift ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        --*) die "unknown option $1" ;;
        *) args+=("$1"); shift ;;
    esac
done

tag="${args[0]:-}"
newer_tag="${args[1]:-}"

command -v docker >/dev/null 2>&1 || { echo "install-smoke: docker is not installed; the container legs cannot run" >&2; exit 2; }
docker info >/dev/null 2>&1 || { echo "install-smoke: the docker daemon is not running; start it and try again" >&2; exit 2; }

work="$(mktemp -d)"
cleanup() {
    if [ "$keep" = 1 ]; then echo "install-smoke: scratch kept at $work"; return; fi
    [ -n "${work:-}" ] && [ -d "$work" ] && rm -r -- "$work"
}
trap cleanup EXIT

# The next version after this one: a prerelease is followed by the version it is a candidate
# for, anything else by the next patch.
next_version() {
    local v="$1"
    case "$v" in
        *-*) echo "${v%%-*}"; return ;;
    esac
    local major="${v%%.*}" rest="${v#*.}" minor patch
    minor="${rest%%.*}"; patch="${rest#*.}"
    echo "${major}.${minor}.$((patch + 1))"
}

target_for() {
    case "$1" in
        linux/amd64) echo x86_64-unknown-linux-gnu ;;
        linux/arm64) echo aarch64-unknown-linux-gnu ;;
        *) die "no release target for platform $1" ;;
    esac
}

mount_dir=""
if [ -n "$from_dir" ]; then
    [ -d "$from_dir" ] || die "$from_dir is not a directory"
    mount_dir="$work/assets"
    mkdir -p "$mount_dir"
    # `gh run download` nests each artifact in its own directory; flatten what it left.
    found=0
    while IFS= read -r f; do
        cp "$f" "$mount_dir/"
        found=$((found + 1))
    done < <(find "$from_dir" -type f -name 'lastcall-*' ! -name '*.txt')
    [ "$found" -gt 0 ] || die "no lastcall-* assets under $from_dir"
    if [ -f "$from_dir/SHA256SUMS" ]; then
        cp "$from_dir/SHA256SUMS" "$mount_dir/"
    else
        echo "install-smoke: no SHA256SUMS in $from_dir, generating one from the assets themselves"
        echo "install-smoke: the checksum leg is self-referential in this mode; only a release's own SHA256SUMS proves the download"
        (cd "$mount_dir" && sha256sum lastcall-* > SHA256SUMS)
    fi
    # lastcall-<version>-<target>: the version is everything between the first dash and the
    # target triple, so read it off one file name.
    one="$(cd "$mount_dir" && ls lastcall-* | head -1)"
    rest="${one#lastcall-}"
    version="${rest%%-*}"
    case "$rest" in
        "$version"-rc.*) version="$version-$(echo "${rest#"$version"-}" | cut -d- -f1)" ;;
    esac
    tag="(none: --from-dir $from_dir)"
else
    if [ -z "$tag" ]; then
        command -v gh >/dev/null 2>&1 || die "no tag given and gh is not installed"
        tag="$(gh release view --repo "$repo" --json tagName --jq .tagName)" \
            || die "no releases in $repo yet; name a tag or use --from-dir"
    fi
    version="${tag#v}"
fi

if [ -n "$newer_tag" ]; then
    newer_version="${newer_tag#v}"
else
    newer_version="$(next_version "$version")"
    newer_tag="v$newer_version"
fi

cat > "$work/inner.sh" <<'INNER'
#!/usr/bin/env bash
set -euo pipefail
say() { printf '\n=== %s\n' "$*"; }
fail() { echo "install-smoke: $*" >&2; exit 1; }

say "the container"
sed -n 's/^PRETTY_NAME=//p' /etc/os-release
echo "arch: $(uname -m)"

say "prerequisites a bare machine does not have"
export DEBIAN_FRONTEND=noninteractive
apt-get -qq update
apt-get -qq install -y --no-install-recommends curl ca-certificates python3 >/dev/null
curl --version | head -1
python3 --version

install -d /opt/lastcall /srv/release
cd /opt/lastcall
asset="lastcall-${VERSION}-${TARGET}"

if [ -n "${FROM_DIR:-}" ]; then
    say "install $asset from the mounted directory"
    [ -f "/assets/$asset" ] || fail "/assets/$asset is missing"
    cp "/assets/$asset" "/assets/SHA256SUMS" .
else
    say "install $asset from the $TAG release"
    curl -fsSLO "https://github.com/$REPO/releases/download/$TAG/$asset"
    curl -fsSLO "https://github.com/$REPO/releases/download/$TAG/SHA256SUMS"
fi

say "verify the checksum before running anything"
sha256sum -c --ignore-missing SHA256SUMS
mv "$asset" lastcall
chmod +x lastcall

say "it runs"
./lastcall --version

say "lay out a $NEWER_TAG release on 127.0.0.1:$PORT"
d=/srv/release
newer_asset="lastcall-${NEWER_VERSION}-${TARGET}"
install -d "$d/repos/$REPO/releases" "$d/$REPO/releases/download/$NEWER_TAG"
if [ -n "${NEWER_TAG_IS_REAL:-}" ]; then
    curl -fsSL -o "$d/$REPO/releases/download/$NEWER_TAG/$newer_asset" \
        "https://github.com/$REPO/releases/download/$NEWER_TAG/$newer_asset"
    curl -fsSL -o "$d/$REPO/releases/download/$NEWER_TAG/SHA256SUMS" \
        "https://github.com/$REPO/releases/download/$NEWER_TAG/SHA256SUMS"
else
    echo "no second release was named: serving this binary back as $NEWER_VERSION"
    cp ./lastcall "$d/$REPO/releases/download/$NEWER_TAG/$newer_asset"
    (cd "$d/$REPO/releases/download/$NEWER_TAG" && sha256sum "$newer_asset" > SHA256SUMS)
fi
printf '{"tag_name":"%s","prerelease":false,"draft":false}\n' "$NEWER_TAG" \
    > "$d/repos/$REPO/releases/latest"
python3 -m http.server "$PORT" -b 127.0.0.1 --directory "$d" > /srv/http.log 2>&1 &
server=$!
# Retries rather than a sleep: the server is ready when it answers, not after N seconds.
# curl reports the first refused connection on stderr and then retries, so stderr is dropped.
curl -fsS --retry 30 --retry-delay 1 --retry-connrefused \
    "http://127.0.0.1:$PORT/repos/$REPO/releases/latest" > /dev/null 2>&1 \
    || fail "the local release server never came up"

export LASTCALL_UPDATE_BASE_URL="http://127.0.0.1:$PORT/"

say "it sees the newer release"
./lastcall update --check | tee /tmp/check.out
grep -q "$NEWER_VERSION available" /tmp/check.out || fail "--check did not offer $NEWER_VERSION"

say "and takes it"
./lastcall update | tee /tmp/update.out
grep -q "$NEWER_VERSION" /tmp/update.out || fail "update did not report $NEWER_VERSION"
have="$(sha256sum lastcall | cut -d' ' -f1)"
want="$(sha256sum "$d/$REPO/releases/download/$NEWER_TAG/$newer_asset" | cut -d' ' -f1)"
[ "$have" = "$want" ] || fail "the binary on disk is not the asset that was served"
echo "the replaced binary is byte-for-byte the served asset ($have)"
./lastcall --version
if [ -n "${NEWER_TAG_IS_REAL:-}" ]; then
    ./lastcall --version | grep -q "$NEWER_VERSION" || fail "--version does not say $NEWER_VERSION"
fi

kill "$server" 2>/dev/null || true
say "OK: $TARGET"
INNER

echo "install-smoke: repository $repo"
echo "install-smoke: installing $version (tag $tag), updating to $newer_version (tag $newer_tag)"
[ -n "$from_dir" ] && echo "install-smoke: assets from $from_dir"

failed=()
ran=()
for platform in "${platforms[@]}"; do
    target="$(target_for "$platform")"
    echo
    echo "######## $platform ($target)"
    if [ -n "$from_dir" ] && [ ! -f "$mount_dir/lastcall-$version-$target" ]; then
        echo "SKIP $platform: $mount_dir has no lastcall-$version-$target"
        continue
    fi
    mounts=(-v "$work/inner.sh:/smoke/inner.sh:ro")
    [ -n "$from_dir" ] && mounts+=(-v "$mount_dir:/assets:ro")
    if docker run --rm --platform "$platform" \
        "${mounts[@]}" \
        -e REPO="$repo" -e TAG="$tag" -e VERSION="$version" -e TARGET="$target" \
        -e NEWER_TAG="$newer_tag" -e NEWER_VERSION="$newer_version" -e PORT="$port" \
        -e FROM_DIR="${from_dir:+/assets}" \
        -e NEWER_TAG_IS_REAL="${args[1]:-}" \
        "$image" bash /smoke/inner.sh
    then
        ran+=("$platform")
    else
        failed+=("$platform")
    fi
done

echo
echo "######## summary"
[ ${#ran[@]} -gt 0 ] && echo "passed: ${ran[*]}"
if [ ${#failed[@]} -gt 0 ]; then
    echo "failed: ${failed[*]}"
    exit 1
fi
[ ${#ran[@]} -gt 0 ] || { echo "no leg ran"; exit 1; }
