#!/usr/bin/env bash
# Smoke test: `fno done` - help, no-match, and --note on a disposable graph.
set -euo pipefail

# Every command that discards its output says something when it fails. Several
# below are `>/dev/null 2>&1` with no guard, so under `set -e` a failure exited
# with NO output at all: the smoke runner printed this script's header and then
# nothing, and the reader learned only that something failed. Observed on
# origin/main as a silent EXIT=6.
#
# Deliberately NOT an ERR trap: bash fires ERR under the same CONDITIONS as
# errexit, and those are about command position, not about `set -e` being on -
# so a trap would also fire inside the `set +e` block below, where a non-zero
# exit is the expected result being measured.
die() { echo "FAIL: test_done.sh: $1" >&2; exit 1; }

cd "$(git rev-parse --show-toplevel)/cli" || die "cannot cd to the cli/ project root"
uv sync --quiet || die "uv sync failed (cli/ project dependencies unavailable)"

# Use a temp directory and point fno at it via HOME.
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# Arrange: ~/.fno/graph.json under a disposable HOME with one entry.
mkdir -p "$TMP/.fno"
cat > "$TMP/.fno/graph.json" <<'JSON'
{
  "entries": [
    {
      "id": "ab-smoke0001",
      "title": "Smoke test entry",
      "status": "ready",
      "domain": "trading",
      "created_at": "2026-04-22T00:00:00+00:00"
    }
  ]
}
JSON

export HOME="$TMP"
export FNO_GRAPH_JSON_LOCK=/tmp/fno-smoke-done.lock
# The carve-out gate resolves its ledger from the CANONICAL repo root, never
# from HOME, so overriding HOME alone left this test reading the developer's
# real .fno/carveouts.jsonl. Any unharvested deferred carve-out in the checkout
# then made `fno done` refuse with exit 6 over work this sandbox never
# declared. Green in CI throughout, because carveouts.jsonl is gitignored and
# a fresh checkout has none: the test passed exactly where nobody runs it and
# failed on every machine where somebody does.
export FNO_REPO_ROOT="$TMP"

# Closing with --pr now demands gh-resolved MERGED evidence, and CI has no
# authenticated gh. Stub one that reports merged so the cases below keep
# testing what they are about (the ledger rollup), not the merge gate.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/gh" <<'SH'
#!/usr/bin/env bash
for arg in "$@"; do
  if [[ "$arg" == "nameWithOwner" ]]; then echo "smoke/repo"; exit 0; fi
done
if [[ " $* " == *" api repos/"*"/pulls/99 "* ]]; then
  printf '{"number":99,"state":"closed","merged":true,"html_url":"https://github.com/smoke/repo/pull/99","merged_at":"2026-04-22T18:00:00Z","head":{"sha":"deadbeef","ref":"feature/smoke"},"base":{"ref":"main"}}\n'
  exit 0
fi
printf '{"number":99,"state":"MERGED","url":"https://github.com/smoke/repo/pull/99","mergedAt":"2026-04-22T18:00:00Z","mergeCommit":{"oid":"deadbeef"}}\n'
SH
chmod +x "$TMP/bin/gh"
export PATH="$TMP/bin:$PATH"

# AC: `fno done --help` succeeds and mentions 'done'
help_out=$(uv run fno-py done --help 2>&1)
if ! echo "$help_out" | grep -qi "done"; then
  echo "FAIL: 'fno done --help' did not mention 'done':"
  echo "$help_out"
  exit 1
fi

# AC: `fno done nonexistent-xyzzy` exits 2 with no-match message
set +e
nomatch_out=$(uv run fno-py done nonexistent-xyzzy-smoke 2>&1)
nomatch_rc=$?
set -e
if [[ "$nomatch_rc" -ne 2 ]]; then
  echo "FAIL: expected rc=2 for no match, got $nomatch_rc"
  echo "$nomatch_out"
  exit 1
fi
if ! echo "$nomatch_out" | grep -qi "no match\|no entry"; then
  echo "FAIL: no-match message missing:"
  echo "$nomatch_out"
  exit 1
fi

# AC: `fno done ab-smoke0001 --note "smoke test"` sets completion_note
uv run fno-py done ab-smoke0001 --note "smoke test marker" >/dev/null 2>&1 \
  || die "'fno done ab-smoke0001 --note' exited $? (rerun without >/dev/null to see it)"
status=$(python3 -c "
import json
d = json.load(open('$TMP/.fno/graph.json'))
e = next(x for x in d['entries'] if x['id'] == 'ab-smoke0001')
print(e.get('status'), e.get('completion_note'))
")
if [[ "$status" != "done smoke test marker" ]]; then
  echo "FAIL: expected 'done smoke test marker', got: $status"
  exit 1
fi

# AC: Ledger rollup fills session_id / cost_usd / cost_sessions / points
# Seed a second ab node AND a matching ledger entry, then run fno done.
cat > "$TMP/.fno/graph.json" <<'JSON'
{
  "entries": [
    {
      "id": "ab-smoke0002",
      "title": "Rollup smoke target",
      "status": "ready",
      "domain": "code",
      "plan_path": "/smoke/plans/feature-x",
      "created_at": "2026-04-22T00:00:00+00:00"
    }
  ]
}
JSON
cat > "$TMP/.fno/ledger.json" <<'JSON'
{
  "entries": [
    {
      "plan_path": "/smoke/plans/feature-x",
      "sessions": ["smoke-sess-1"],
      "cost_usd": 4.25,
      "completed": "2026-04-22T19:00:00+00:00",
      "points": 5
    }
  ]
}
JSON
uv run fno-py done ab-smoke0002 --pr 99 >/dev/null 2>&1 || true
rollup=$(python3 -c "
import json
d = json.load(open('$TMP/.fno/graph.json'))
e = next(x for x in d['entries'] if x['id'] == 'ab-smoke0002')
print(e.get('session_id'), e.get('cost_usd'), e.get('points'), len(e.get('cost_sessions') or []))
")
if [[ "$rollup" != "smoke-sess-1 4.25 5 1" ]]; then
  echo "FAIL: rollup fields not filled. got: $rollup"
  exit 1
fi

# AC: `--backfill` works on an already-done node without flipping status.
python3 -c "
import json
d = json.load(open('$TMP/.fno/graph.json'))
e = next(x for x in d['entries'] if x['id'] == 'ab-smoke0002')
e['session_id'] = None
e['cost_usd'] = None
e['cost_sessions'] = []
e['points'] = None
json.dump(d, open('$TMP/.fno/graph.json', 'w'), indent=2)
"
uv run fno-py done ab-smoke0002 --backfill >/dev/null 2>&1 \
  || die "'fno done ab-smoke0002 --backfill' exited $? (rerun without >/dev/null to see it)"
backfilled=$(python3 -c "
import json
d = json.load(open('$TMP/.fno/graph.json'))
e = next(x for x in d['entries'] if x['id'] == 'ab-smoke0002')
print(e.get('status'), e.get('session_id'), e.get('cost_usd'))
")
if [[ "$backfilled" != "done smoke-sess-1 4.25" ]]; then
  echo "FAIL: --backfill did not refill. got: $backfilled"
  exit 1
fi

echo "PASS: test_done.sh"
