#!/usr/bin/env bash
# Enforce the module-granularity dependency boundary for company orchestration.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"

python3 - "$ROOT" <<'PY'
import ast
import sys
from pathlib import Path

# This is the sole declared layer map. The architecture document cites it.
LAYERS = (
    (0, "platform", ("fno.paths", "fno.config", "fno.handoff", "fno.events")),
    (
        1,
        "core",
        (
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
    matches = []
    for number, name, prefixes in LAYERS:
        for prefix in prefixes:
            if module == prefix or module.startswith(prefix + "."):
                matches.append((len(prefix), number, name))
    if not matches:
        return None
    _, number, name = max(matches)
    return number, name


def imported_modules(node: ast.AST, source_module: str) -> list[str]:
    if isinstance(node, ast.Import):
        return [alias.name for alias in node.names]
    if not isinstance(node, ast.ImportFrom):
        return []
    if node.level == 0:
        return [node.module] if node.module else []
    package = source_module if source_module.endswith(".__init__") else source_module.rpartition(".")[0]
    parts = package.split(".") if package else []
    keep = max(0, len(parts) - node.level + 1)
    base = parts[:keep]
    if node.module:
        base.extend(node.module.split("."))
    return [".".join(base)] if base else []


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
for path, source, tree in parsed:
    source_module = module_name(path)
    source_layer = layer_for(source_module)
    if source_layer is None:
        continue
    lines = source.splitlines()
    for node in ast.walk(tree):
        for imported in imported_modules(node, source_module):
            if (
                source_module == "fno.roles.models"
                and imported == "fno.company.contracts"
            ):
                positive_control = True
            target_layer = layer_for(imported)
            if target_layer is None or source_layer[0] >= target_layer[0]:
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

print("check-company-boundaries: positive control ok (roles -> core edge observed)")
if violations:
    print("check-company-boundaries: prohibited dependencies:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    print(
        "Legal direction is downward only; see docs/architecture/company-boundaries.md",
        file=sys.stderr,
    )
    sys.exit(1)

print(
    f"check-company-boundaries: {len(LAYERS)} layers, "
    f"{len(files)} modules, 0 violations"
)
PY
