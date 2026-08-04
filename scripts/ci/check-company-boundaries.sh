#!/usr/bin/env bash
# Enforce the module-granularity dependency boundary for company orchestration.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"

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

python3 - "$ROOT" <<'PY'
import ast
import os
import sys
from pathlib import Path

# This is the sole declared layer map. The architecture document cites it.
LAYERS = (
    (0, "platform", ("fno.paths", "fno.config", "fno.handoff", "fno.events")),
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

print("check-company-boundaries: positive control ok (roles -> core edge observed)")
print(
    "check-company-boundaries: no enforcement for fno-skills "
    "(no Python package, markdown and shell under skills/) or fno-mux "
    "(Rust, crates/fno/src/mux_cli.rs)"
)
if violations or cycle:
    print("check-company-boundaries: prohibited dependencies:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    if cycle:
        rendered = " -> ".join(f"L{number} {names[number]}" for number in cycle)
        print(f"  layer cycle: {rendered}", file=sys.stderr)
    print(
        "Legal direction is downward only; see docs/architecture/company-boundaries.md",
        file=sys.stderr,
    )
    sys.exit(1)

print(
    f"check-company-boundaries: {len(LAYERS)} layers, "
    f"{mapped_modules} modules, 0 violations"
)
PY
