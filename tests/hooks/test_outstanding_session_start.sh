#!/usr/bin/env bash
# hooks/outstanding-session-start.sh: the shrunk producer. A fresh cache
# renders this session's own questions plus one count line and never calls
# the 3s fold; a missing or stale cache falls back to the fold (and keeps
# its failure semantics). `fno` is a stub reading its fold output from
# files; jq is whatever is on PATH.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
HOOK="hooks/outstanding-session-start.sh"
[[ -f "$HOOK" ]] || { echo "FAIL: $HOOK not found from $(pwd)"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
STUB="$TMP/bin"
mkdir -p "$STUB"

pass=0
fail=0

MANIFEST_SESSION="s-manifest"

# The stub `fno`: answers the manifest read; the fold prints $TMP/fold-body
# and exits $TMP/fold-rc (defaults: empty body, rc 0).
{
    printf '#!/usr/bin/env bash\n'
    printf 'if [[ "$1 $2" == "do state" ]]; then\n'
    printf '  echo "session_id: %s"\n' "$MANIFEST_SESSION"
    printf '  exit 0\n'
    printf 'fi\n'
    printf 'if [[ "$1" == "inbox" ]]; then\n'
    printf '  [[ -f "%s/fold-body" ]] && cat "%s/fold-body"\n' "$TMP" "$TMP"
    printf '  exit $(cat "%s/fold-rc" 2>/dev/null || echo 0)\n' "$TMP"
    printf 'fi\n'
    printf 'exit 0\n'
} > "$STUB/fno"
chmod +x "$STUB/fno"

set_fold() {
    local body="$1" rc="$2"
    printf '%s' "$body" > "$TMP/fold-body"
    printf '%s' "$rc" > "$TMP/fold-rc"
}

check() {
    local name="$1" want="$2" got="$3"
    if [[ "$got" == "$want" ]]; then
        echo "  PASS: $name"
        pass=$((pass + 1))
    else
        echo "  FAIL: $name"
        echo "    wanted: $want"
        echo "    got: ${got:-<empty>}"
        fail=$((fail + 1))
    fi
}

run_hook() {
    FNO_HOME="$TMP/home" PATH="$STUB:$PATH" bash "$HOOK" 2>"$TMP/hook-err"
    local rc=$?
    if [[ -s "$TMP/hook-err" ]]; then
        printf '[hook stderr rc=%s] %s\n' "$rc" "$(cat "$TMP/hook-err")" >&2
    elif [[ $rc -ne 0 ]]; then
        printf '[hook rc=%s, no stderr]\n' "$rc" >&2
    fi
}

write_cache() {
    mkdir -p "$TMP/home/attention"
    cat > "$TMP/home/attention/items.json"
}

rm_cache() {
    rm -rf "$TMP/home/attention"
    rm -f "$TMP/fold-body" "$TMP/fold-rc"
}

echo "=== outstanding-session-start producer ==="

# A fresh cache: own questions plus one count, and the fold never runs.
set_fold "FOLD SHOULD NOT RUN" 0
write_cache <<'JSON'
{"as_of": 1, "items": [
  {"kind": "question", "title": "Which reading?", "ready": true,
   "asker": {"handle": "worker-1", "session_id": "s-manifest"}},
  {"kind": "question", "title": "Someone else's ask", "ready": true,
   "asker": {"handle": "worker-2", "session_id": "s-other"}},
  {"kind": "question", "title": "Not ready yet", "ready": false,
   "asker": {"handle": "worker-1", "session_id": "s-manifest"}},
  {"kind": "mine", "title": "the user's own line", "ready": true,
   "asker": null}
]}
JSON
out="$(run_hook)"
check "a fresh cache shows this session's own ready question" \
    "## Outstanding for you

- question: Which reading?
3 open across the fleet." \
    "$out"

# A missing cache: the full fold answers.
rm_cache
set_fold $'## Outstanding for you\n\ncarve-out line' 0
out="$(run_hook)"
check "a missing cache falls back to the fold" \
    "## Outstanding for you

carve-out line" \
    "$out"

# A stale cache: the fold answers too.
write_cache <<'JSON'
{"as_of": 1, "items": []}
JSON
touch -t 202001010000 "$TMP/home/attention/items.json"
set_fold "stale fell back" 0
out="$(run_hook)"
check "a stale cache falls back to the fold" "stale fell back" "$out"

# Fold failure semantics survive the fallback.
rm_cache
set_fold "" 1
out="$(run_hook)"
check "an unreadable store says so, loudly" \
    "## Outstanding for you

could not be read (fno inbox outstanding exit 1). Run it directly." \
    "$out"

rm_cache
set_fold "" 2
out="$(run_hook)"
check "an old deployed verb stays silent" "" "$out"

echo
echo "Results: $pass passed, $fail failed"
[[ $fail -eq 0 ]] || exit 1
