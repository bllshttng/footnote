#!/usr/bin/env bash
# Enforce the module-granularity dependency boundary for company orchestration.
set -euo pipefail

MODE="baseline"
MODE_SEEN=""
BASELINE_FILE=""
BASELINE_FILE_SEEN=0
ROOT=""

usage() {
  cat <<'EOF'
Usage: check-company-boundaries.sh [--baseline|--strict] [--baseline-file PATH] [ROOT]

Modes:
  --baseline  CI ratchet: pass only when the checked-in finding set is unchanged.
              A reduction passes after its baseline is updated in the same PR.
  --strict    Full audit: report every violation and fail while existing violations remain.

The default mode is --baseline. ROOT defaults to the current repository root.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --baseline|--strict)
      requested="${1#--}"
      if [[ -n "$MODE_SEEN" && "$MODE_SEEN" != "$requested" ]]; then
        echo "check-company-boundaries: choose exactly one mode" >&2
        exit 2
      fi
      MODE="$requested"
      MODE_SEEN="$requested"
      shift
      ;;
    --baseline-file)
      if [[ $# -lt 2 ]]; then
        echo "check-company-boundaries: --baseline-file requires a path" >&2
        exit 2
      fi
      BASELINE_FILE="$2"
      BASELINE_FILE_SEEN=1
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      echo "check-company-boundaries: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [[ -n "$ROOT" ]]; then
        echo "check-company-boundaries: unexpected argument: $1" >&2
        exit 2
      fi
      ROOT="$1"
      shift
      ;;
  esac
done

ROOT="${ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
BASELINE_FILE="${BASELINE_FILE:-$ROOT/scripts/ci/company-boundary-baseline.txt}"

if [[ "$MODE" == "strict" && $BASELINE_FILE_SEEN -eq 1 ]]; then
  echo "check-company-boundaries: --baseline-file is valid only with --baseline" >&2
  exit 2
fi

# Keep this reader byte-for-byte equivalent to the freshness gate's reader so
# both checks agree on which root files are build-time pack projections.
_pack_marker_value() {
  awk 'NR==1 && /^---[[:space:]]*$/ {f=1; next} f && /^---[[:space:]]*$/ {exit} f && /^pack:/ {sub(/^pack:[[:space:]]*/,""); gsub(/["'\'']/,""); print; exit}' "$1" 2>/dev/null
}

_pack_marker_line() {
  awk 'NR==1 && /^---[[:space:]]*$/ {f=1; next} f && /^---[[:space:]]*$/ {exit} f && /^pack:/ {print NR; exit}' "$1" 2>/dev/null
}

ATTRIBUTED=()
ORPHANS=0
_classify_projection() {
  local path="$1" rel="$2" marker line
  marker="$(_pack_marker_value "$path")"
  [[ -n "$marker" ]] || return 0
  line="$(_pack_marker_line "$path")"
  if [[ -f "$ROOT/plugins/$marker/plugin.yaml" ]]; then
    ATTRIBUTED+=("$rel -> $marker")
    return 0
  fi
  echo "  $rel:$line: pack marker '$marker' names no plugins/$marker/plugin.yaml" >&2
  ORPHANS=1
}

while IFS= read -r path; do
  [[ -f "$path" ]] || continue
  _classify_projection "$path" "agents/$(basename "$path")"
done < <(find "$ROOT/agents" -maxdepth 1 -name '*.md' -type f 2>/dev/null | sort)
while IFS= read -r path; do
  [[ -f "$path/SKILL.md" ]] || continue
  _classify_projection "$path/SKILL.md" "skills/$(basename "$path")"
done < <(find "$ROOT/skills" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort)

if [[ ${#ATTRIBUTED[@]} -gt 0 ]]; then
  printf 'check-company-boundaries: projections attributed: '
  (IFS=', '; echo "${ATTRIBUTED[*]}")
fi
if [[ $ORPHANS -ne 0 ]]; then
  echo "check-company-boundaries: orphaned pack projection" >&2
  exit 1
fi

python3 - "$ROOT" "$MODE" "$BASELINE_FILE" <<'PY'
import ast
import os
import re
import sys
from pathlib import Path

# This is the sole declared layer map. The architecture document cites it.
LAYERS = (
    (
        0,
        "platform",
        (
            "fno.paths",
            "fno.config",
            "fno.handoff",
            "fno.events",
            # Leaf utilities with callers on both sides of the runtime boundary.
            # Declared, not merely moved: an undeclared module is unmapped, and
            # the scan skips unmapped modules entirely, so relocating code
            # without adding it here would clear a finding while leaving the
            # dependency in place.
            "fno.dispatch_flags",
            "fno.drive_authority",
            "fno.rust_binary",
        ),
    ),
    (
        1,
        "core",
        (
            "fno.company",
            "fno.company.contracts",
            "fno.graph",
            "fno.claims",
            "fno.plan",
            "fno.approvals",
            "fno.delivery",
        ),
    ),
    (2, "roles", ("fno.roles",)),
    (
        3,
        "composition",
        (
            "fno.company.campaign",
            "fno.company.coordinator",
            "fno.company.topology",
            "fno.company.execution",
            "fno.company.join",
            "fno.company.cli",
        ),
    ),
    (4, "plugins", ("fno.plugins",)),
    (5, "runtime", ("fno.runtime", "fno.agents")),
)

root = Path(sys.argv[1]).resolve()
mode = sys.argv[2]
baseline_path = Path(sys.argv[3])
source_root = root / "cli" / "src" / "fno"
if not source_root.is_dir():
    print(
        f"check-company-boundaries: {source_root} not found; scan could not run",
        file=sys.stderr,
    )
    sys.exit(2)


def module_name(path: Path) -> str:
    rel = path.relative_to(root / "cli" / "src").with_suffix("")
    parts = list(rel.parts)
    if parts[-1] == "__init__":
        parts.pop()
    return ".".join(parts)


def layer_for(module: str):
    if os.environ.get("FNO_BOUNDARY_TEST_COMPANY_PACKAGE_CORE") == "1" and (
        module == "fno.company" or module.startswith("fno.company.")
    ):
        return 1, "core"
    matches = []
    for number, name, prefixes in LAYERS:
        for prefix in prefixes:
            if module == prefix or module.startswith(prefix + "."):
                matches.append((len(prefix), number, name))
    if not matches:
        return None
    _, number, name = max(matches)
    return number, name


def imported_modules(
    node: ast.AST, source_module: str, *, source_is_package: bool
) -> list[str]:
    if isinstance(node, ast.Import):
        return [alias.name for alias in node.names]
    if not isinstance(node, ast.ImportFrom):
        return []
    if node.level == 0:
        base = node.module or ""
    else:
        package = source_module if source_is_package else source_module.rpartition(".")[0]
        parts = package.split(".") if package else []
        keep = max(0, len(parts) - node.level + 1)
        base_parts = parts[:keep]
        if node.module:
            base_parts.extend(node.module.split("."))
        base = ".".join(base_parts)
    candidates = [base] if base else []
    candidates.extend(
        f"{base}.{alias.name}" if base else alias.name
        for alias in node.names
        if alias.name != "*"
    )
    return list(dict.fromkeys(candidates))


files = sorted(source_root.rglob("*.py"))
parsed = []
try:
    for path in files:
        source = path.read_text(encoding="utf-8")
        parsed.append((path, source, ast.parse(source, filename=str(path))))
except (OSError, SyntaxError, UnicodeError) as exc:
    print(f"check-company-boundaries: scan could not parse {path}: {exc}", file=sys.stderr)
    sys.exit(2)

positive_control = False
violations = []
layer_edges = set()
mapped_modules = 0
for path, source, tree in parsed:
    source_module = module_name(path)
    source_layer = layer_for(source_module)
    if source_layer is None:
        continue
    mapped_modules += 1
    lines = source.splitlines()
    for node in ast.walk(tree):
        seen_target_layers = set()
        for imported in imported_modules(
            node, source_module, source_is_package=path.name == "__init__.py"
        ):
            if (
                source_module == "fno.roles.models"
                and imported == "fno.company.contracts"
            ):
                positive_control = True
            target_layer = layer_for(imported)
            if target_layer is None:
                continue
            if target_layer[0] in seen_target_layers:
                continue
            seen_target_layers.add(target_layer[0])
            if source_layer[0] != target_layer[0]:
                layer_edges.add((source_layer[0], target_layer[0]))
            if source_layer[0] >= target_layer[0]:
                continue
            statement = lines[node.lineno - 1].strip()
            rel = path.relative_to(root)
            violations.append(
                f"{rel}:{node.lineno}: L{source_layer[0]} {source_layer[1]} -> "
                f"L{target_layer[0]} {target_layer[1]} / {statement}"
            )

if not positive_control:
    print(
        "check-company-boundaries: positive control failed "
        "(no roles -> core edge observed); scan did not reach content",
        file=sys.stderr,
    )
    sys.exit(2)


def find_cycle(edges):
    graph = {}
    for source, target in edges:
        graph.setdefault(source, set()).add(target)
    visited = set()
    active = []

    def visit(node):
        if node in active:
            start = active.index(node)
            return active[start:] + [node]
        if node in visited:
            return None
        active.append(node)
        for target in sorted(graph.get(node, ())):
            cycle = visit(target)
            if cycle:
                return cycle
        active.pop()
        visited.add(node)
        return None

    for node in sorted(graph):
        cycle = visit(node)
        if cycle:
            return cycle
    return None


cycle = find_cycle(layer_edges)
names = {number: name for number, name, _ in LAYERS}
rendered_cycle = None
if cycle:
    rendered_cycle = " -> ".join(f"L{number} {names[number]}" for number in cycle)

print("check-company-boundaries: positive control ok (roles -> core edge observed)")
print(
    "check-company-boundaries: no enforcement for fno-skills "
    "(no Python package, markdown and shell under skills/) or fno-mux "
    "(Rust, crates/fno/src/mux_cli.rs)"
)
if mode == "strict" and (violations or cycle):
    print("check-company-boundaries: prohibited dependencies:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    if rendered_cycle:
        print(f"  layer cycle: {rendered_cycle}", file=sys.stderr)
    print(
        "Legal direction is downward only; see docs/architecture/company-boundaries.md",
        file=sys.stderr,
    )
    sys.exit(1)

if mode == "baseline":
    try:
        raw_baseline = baseline_path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as exc:
        print(
            f"check-company-boundaries: baseline could not be read: {baseline_path}: {exc}",
            file=sys.stderr,
        )
        sys.exit(2)

    baseline = [
        line.strip()
        for line in raw_baseline
        if line.strip() and not line.lstrip().startswith("#")
    ]
    violation_pattern = re.compile(
        r"^cli/src/fno/.+\.py:\d+: L\d+ [a-z]+ -> L\d+ [a-z]+ / .+$"
    )
    cycle_pattern = re.compile(
        r"^layer cycle: L\d+ [a-z]+(?: -> L\d+ [a-z]+)+$"
    )
    malformed = [
        entry
        for entry in baseline
        if not violation_pattern.match(entry) and not cycle_pattern.match(entry)
    ]
    if malformed or len(baseline) != len(set(baseline)):
        print(
            "check-company-boundaries: baseline is malformed or contains duplicates",
            file=sys.stderr,
        )
        for entry in malformed:
            print(f"  {entry}", file=sys.stderr)
        sys.exit(2)

    current = list(violations)
    if rendered_cycle:
        current.append(f"layer cycle: {rendered_cycle}")
    new_or_changed = sorted(set(current) - set(baseline))
    resolved_or_changed = sorted(set(baseline) - set(current))
    if new_or_changed or resolved_or_changed:
        print("check-company-boundaries: baseline drift:", file=sys.stderr)
        for entry in new_or_changed:
            print(f"  new or changed violation: {entry}", file=sys.stderr)
        for entry in resolved_or_changed:
            print(f"  resolved or changed baseline entry: {entry}", file=sys.stderr)
        print(
            "Update the baseline in the same PR only when a violation is removed; "
            "new or changed violations are prohibited.",
            file=sys.stderr,
        )
        sys.exit(1)

    cycle_count = 1 if rendered_cycle else 0
    if violations or cycle_count:
        dependency_label = "dependency" if len(violations) == 1 else "dependencies"
        cycle_label = "cycle" if cycle_count == 1 else "cycles"
        print(
            f"check-company-boundaries: baseline holds: {len(violations)} prohibited "
            f"{dependency_label} and {cycle_count} {cycle_label}; strict audit remains red"
        )
        sys.exit(0)

print(
    f"check-company-boundaries: {len(LAYERS)} layers, "
    f"{mapped_modules} modules, 0 violations"
)
PY
