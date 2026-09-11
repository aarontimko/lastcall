#!/usr/bin/env bash
# The probe "curl" for Phase 9b deliverable 2. Never a real curl: `lastcall update` and the
# TUI's once-a-day check are the only two places the program reaches the network, and no
# test may reach it. `PtyCommand::isolated_lastcall` and the update integration tests
# prepend a directory holding this script under the name `curl`, so the child's `curl`
# resolves here and every byte comes off the local disk.
#
# It serves a directory named by the environment, laid out the way the release API is:
#
#   $LASTCALL_TEST_RELEASE_DIR   the served directory. **Unset is exit 99**, deliberately:
#                                a scene that reaches for the network must fail loudly
#                                rather than spend the developer's GitHub rate limit.
#   <dir>/latest.json            answers `…/releases/latest`
#   <dir>/list.json              answers `…/releases?per_page=<n>`
#   <dir>/<asset>                answers `…/download/<tag>/<asset>` (SHA256SUMS included)
#   <dir>/<name>.status          the HTTP status to answer with, if not 200
#   <dir>/<name>.headers         extra header lines, one per line, for the `-D -` shape
#   $LASTCALL_PROBE_CURL_LOG     append one `url: <url>` line per invocation
#
# It implements exactly the two shapes `commands/update.rs` builds and nothing else: the
# JSON call (`-D -`, body and headers on stdout) and the asset call (`-o <file>`). Both end
# their stdout with the three digits `-w '%{http_code}'` would have written, which is what
# the caller parses.
#
# `set -u` and no `set -e`: a missing file is a 404, which is an answer, not a crash.
set -u

dir="${LASTCALL_TEST_RELEASE_DIR:-}"
if [ -z "$dir" ]; then
    echo "probe curl: LASTCALL_TEST_RELEASE_DIR is unset; this scene tried to reach the network" >&2
    exit 99
fi

dest=""
dump_headers=0
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o)
            dest="$2"
            shift 2
            ;;
        -D)
            [ "$2" = "-" ] && dump_headers=1
            shift 2
            ;;
        -A | -w | --max-time | --speed-limit | --speed-time)
            shift 2
            ;;
        -*)
            shift
            ;;
        *)
            url="$1"
            shift
            ;;
    esac
done

if [ -n "${LASTCALL_PROBE_CURL_LOG:-}" ]; then
    printf 'url: %s\n' "$url" >>"$LASTCALL_PROBE_CURL_LOG"
fi

case "$url" in
    */releases/latest) name="latest.json" ;;
    */releases'?'per_page=*) name="list.json" ;;
    */download/*/*) name="${url##*/}" ;;
    *) name="" ;;
esac

body="$dir/$name"
status=200
if [ -n "$name" ] && [ -f "$body.status" ]; then
    status="$(cat "$body.status")"
elif [ -z "$name" ] || [ ! -f "$body" ]; then
    status=404
fi

if [ -n "$dest" ]; then
    if [ -f "$body" ]; then
        cp "$body" "$dest"
    fi
    printf '%s' "$status"
    exit 0
fi

if [ "$dump_headers" = "1" ]; then
    printf 'HTTP/1.1 %s probe\r\n' "$status"
    if [ -n "$name" ] && [ -f "$body.headers" ]; then
        while IFS= read -r line; do
            printf '%s\r\n' "$line"
        done <"$body.headers"
    fi
    printf '\r\n'
fi
if [ -f "$body" ]; then
    cat "$body"
fi
printf '%s' "$status"
