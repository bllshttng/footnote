# shellcheck shell=bash
# Seed and read an agents registry fixture across the table import.
#
# Before the import registry.json is a plain file, so a fixture is a write.
# After the first fno-agents read it is a fence directory and the rows live in
# graph.db, so the fixture goes through `fno-agents registry-commit`.
# The binary is $REGISTRY_SEED_BIN, else $FNO_AGENTS_BIN, else fno-agents.

# registry_seed <path>: replace the registry with the JSON document on stdin.
registry_seed() {
  local path="$1" doc
  doc="$(cat)"
  if [[ ! -d "$path" ]]; then
    printf '%s\n' "$doc" >"$path"
    return
  fi
  python3 - "$path" "${REGISTRY_SEED_BIN:-${FNO_AGENTS_BIN:-fno-agents}}" "$doc" <<'PY'
import json, subprocess, sys
path, binary, doc = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
def commit(payload):
    r = subprocess.run([binary, "registry-commit"], input=json.dumps(payload), capture_output=True, text=True)
    if r.returncode:
        sys.exit(f"registry-commit: {r.stderr.strip()}")
    return json.loads(r.stdout)
revision = commit({"op": "read", "path": path})["revision"]
rows = doc.get("agents", doc.get("entries", []))
commit({"path": path, "schema_version": doc["schema_version"], "agents": rows, "revision": revision, "replace": True})
PY
}

# registry_cat <path>: print the registry document.
registry_cat() {
  local path="$1"
  if [[ ! -d "$path" ]]; then
    cat "$path"
    return
  fi
  printf '{"op":"read","path":"%s"}' "$path" \
    | "${REGISTRY_SEED_BIN:-${FNO_AGENTS_BIN:-fno-agents}}" registry-commit \
    | python3 -c 'import json, sys; print(json.dumps(json.load(sys.stdin)["document"]))'
}
