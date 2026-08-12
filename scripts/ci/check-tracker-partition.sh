#!/usr/bin/env bash
# check-tracker-partition.sh - enforce the sidecar/tracker zero-overlap rule.
#
# The bring-your-own-id design rests on a partition: the sidecar (footnote-owned
# state) and the tracker read interface (fields an external backend supplies)
# must not share a field name. Zero overlap means zero sync, which is what
# keeps a two-sources-of-truth bug out of the design. The ``id`` key is the one
# allowed overlap: both sides carry it as the join key, never as a synced value.
#
# A field named on both sides (other than ``id``) is the failure mode this gate
# exists to catch - the first convenience mirror of e.g. ``title`` into the
# sidecar would reintroduce exactly the two-writer bug option 2 was rejected for.
#
# Run:  bash scripts/ci/check-tracker-partition.sh [repo-root]
#       bash scripts/ci/check-tracker-partition.sh --self-test
# Default root is the current directory. Exits 0 clean; exits 1 on an overlap
# or a self-test failure. ``--self-test`` runs the positive controls (extraction
# and detection must both fire on synthetic input) and exits without scanning.
set -euo pipefail

REPO_ROOT="."
if [[ "${1:-}" == "--self-test" ]]; then
  python3 - "$REPO_ROOT" --self-test <<'PY'
import ast, sys
from pathlib import Path

# inlined so the gate is self-contained
def extract_fields(src, class_name):
    tree = ast.parse(src)
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef) and node.name == class_name:
            fields = set()
            for stmt in node.body:
                if isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                    fields.add(stmt.target.id)
            return fields
    raise KeyError(f"no class {class_name!r} in source")

ALLOWED_OVERLAP = frozenset({"id"})

def extra_overlap(tracker, sidecar):
    return (set(tracker) & set(sidecar)) - ALLOWED_OVERLAP

# Positive control 1: extraction returns exactly the declared annotated fields
# and skips methods / plain assignments (model_config).
snippet = '''
class TrackerNode(BaseModel):
    id: str
    title: Optional[str] = None
    model_config = ConfigDict()
    def helper(self): ...
'''
got = extract_fields(snippet, "TrackerNode")
assert got == {"id", "title"}, f"extraction broken: got {got}"
# Positive control 2: detection fires on a real (non-key) overlap.
assert extra_overlap({"id", "title"}, {"id", "title", "cwd"}) == {"title"}, "overlap detection broken"
print("check-tracker-partition self-test OK")
PY
  exit $?
fi

[[ "${1:-}" != "" ]] && REPO_ROOT="$1"
TYPES="$REPO_ROOT/cli/src/fno/tracker/types.py"
SIDECAR="$REPO_ROOT/cli/src/fno/tracker/sidecar.py"
for f in "$TYPES" "$SIDECAR"; do
  [[ -f "$f" ]] || { echo "check-tracker-partition: missing $f" >&2; exit 1; }
done

python3 - "$TYPES" "$SIDECAR" <<'PY'
import ast, sys
from pathlib import Path

types_src = Path(sys.argv[1]).read_text()
sidecar_src = Path(sys.argv[2]).read_text()

def extract_fields(src, class_name):
    tree = ast.parse(src)
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef) and node.name == class_name:
            fields = set()
            for stmt in node.body:
                if isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                    fields.add(stmt.target.id)
            return fields
    raise KeyError(f"no class {class_name!r} in {class_name} source")

ALLOWED_OVERLAP = frozenset({"id"})

tracker = extract_fields(types_src, "TrackerNode")
sidecar = extract_fields(sidecar_src, "Sidecar")
overlap = tracker & sidecar
# Require the overlap to be EXACTLY {id}: the join key must be present on both
# sides (an empty overlap is not "clean", it is a missing key) and nothing else
# may be shared (a shared data field is the two-writer bug). Asserting the
# positive key, not an absence, so a lockstep rename of `id` fails loudly.
if "id" not in overlap:
    print(
        "check-tracker-partition: FAIL - the `id` join key is missing from one "
        f"side (overlap is {sorted(overlap)}). Both TrackerNode and Sidecar must "
        "carry `id`; without it the partition has no join key.",
        file=sys.stderr,
    )
    sys.exit(1)
extra = overlap - ALLOWED_OVERLAP
if extra:
    print(
        "check-tracker-partition: FAIL - sidecar and tracker share non-key "
        f"fields {sorted(extra)}. The sidecar may hold only fields the tracker "
        "cannot express; a shared name is the two-writer bug the partition exists "
        "to prevent.",
        file=sys.stderr,
    )
    sys.exit(1)
print(f"check-tracker-partition: OK - overlap is exactly the join key {sorted(overlap)}")
PY
