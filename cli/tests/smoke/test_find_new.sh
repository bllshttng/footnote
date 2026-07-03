#!/usr/bin/env bash
# Smoke test: `fno find` + `fno new` end-to-end in a disposable HOME.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/.fno"
echo '{"entries": []}' > "$TMP/.fno/graph.json"

export HOME="$TMP"

# fno new creates an entry and emits the id on stdout
new_id=$(uv run fno-py new "Smoke research task" --domain research 2>/dev/null)
if [[ ! "$new_id" =~ ^ab-[0-9a-f]{8}$ ]]; then
  echo "FAIL: fno new did not emit a valid ab- id: $new_id"
  exit 1
fi

# Entry actually landed in graph.json
count=$(python3 -c "
import json
d = json.load(open('$TMP/.fno/graph.json'))
print(sum(1 for e in d['entries'] if e['id'] == '$new_id'))
")
if [[ "$count" != "1" ]]; then
  echo "FAIL: expected 1 entry with id $new_id, got $count"
  exit 1
fi

# fno find resolves the entry
find_out=$(uv run fno-py find "research task" 2>/dev/null)
if ! echo "$find_out" | grep -q "$new_id"; then
  echo "FAIL: fno find did not return $new_id:"
  echo "$find_out"
  exit 1
fi

# fno find --json returns valid JSON array
json_out=$(uv run fno-py find "research task" --json 2>/dev/null)
python3 -c "
import json, sys
data = json.loads('''$json_out''')
assert isinstance(data, list), 'not a list'
assert data[0]['id'] == '$new_id', f\"expected $new_id, got {data[0]['id']}\"
" || { echo "FAIL: fno find --json output invalid"; exit 1; }

# fno find for nonexistent exits 1
set +e
uv run fno-py find nonexistent-xyzzy-smoke >/dev/null 2>&1
find_rc=$?
set -e
if [[ "$find_rc" -ne 1 ]]; then
  echo "FAIL: expected rc=1 for no match, got $find_rc"
  exit 1
fi

# fno new --domain fuzzy-match triggers suggestion and exits 2
set +e
uv run fno-py new "Another task" --domain res 2>/dev/null >/dev/null
new_rc=$?
set -e
if [[ "$new_rc" -ne 2 ]]; then
  echo "FAIL: expected rc=2 for fuzzy-domain suggestion, got $new_rc"
  exit 1
fi

# --force-domain bypasses the suggestion
force_id=$(uv run fno-py new "Forced task" --domain res --force-domain 2>/dev/null)
if [[ ! "$force_id" =~ ^ab-[0-9a-f]{8}$ ]]; then
  echo "FAIL: fno new --force-domain did not emit a valid id"
  exit 1
fi
# Confirm domain is the verbatim "res", not "research"
dom=$(python3 -c "
import json
d = json.load(open('$TMP/.fno/graph.json'))
e = next(x for x in d['entries'] if x['id'] == '$force_id')
print(e['domain'])
")
if [[ "$dom" != "res" ]]; then
  echo "FAIL: expected domain=res after --force-domain, got: $dom"
  exit 1
fi

echo "PASS: test_find_new.sh"
