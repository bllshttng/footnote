#!/usr/bin/env bash
# check-import-paths.sh - fail on imports that name a module no longer at its path.
#
# When a module moves down a layer (e.g. fno.agents.rust_runtime -> fno.rust_binary),
# production callers get repointed because they fail loudly. TEST-only callers
# can keep the old `from fno.agents import moved_module` spelling and fail at
# collection time - but changed-smoke selects tests from CHANGED files, and a
# module move does not change test_done.py, so the fast gate ran green on exactly
# the files that were broken (x-ac5f). This static lint catches the whole class
# without test selection: any `from fno.PKG import NAME` / `import fno.PKG.NAME`
# whose target is neither a submodule on disk nor a name re-exported by the
# package __init__ is a dead import path.
#
# Run: bash scripts/ci/check-import-paths.sh [repo-root]
# Default root is the current directory. Exits 0 clean; exits 1 with a report.
set -euo pipefail

REPO_ROOT="${1:-.}"
SRC="$REPO_ROOT/cli/src"
[[ -d "$SRC/fno" ]] || { echo "check-import-paths: no fno package at $SRC/fno" >&2; exit 1; }

python3 - "$SRC" "$REPO_ROOT/cli/tests" <<'PY'
import ast, sys
from pathlib import Path

src = Path(sys.argv[1])            # cli/src  (resolution base for the fno package)
scan_dirs = [p for p in (Path(x) for x in sys.argv[2:]) if p.exists()]
scan_dirs.append(src)              # scan sources too, not only tests

# fno package root is cli/src/fno.
FNO = src / "fno"


def _to_path(dotted_after_fno):
    """fno-internal dotted name -> (file, pkg_dir) under cli/src/fno."""
    base = FNO.joinpath(*dotted_after_fno.split("."))
    return base.with_suffix(".py"), base


def submodule_or_pkg_exists(dotted_after_fno):
    f, d = _to_path(dotted_after_fno)
    return f.is_file() or d.is_dir()


_init_cache: dict[tuple, tuple[set[str], bool] | None] = {}


def init_names(pkg_after_fno):
    """Names a package's __init__.py makes visible, plus whether it star-imports.

    Returns None when this is not a package (no __init__.py), so the caller can
    skip name resolution for it. A star import makes the visible set unknowable
    statically, so the caller treats the package as unresolvable (skip, never
    false-positive)."""
    key = tuple(pkg_after_fno)
    if key in _init_cache:
        return _init_cache[key]
    init = FNO.joinpath(*pkg_after_fno, "__init__.py")
    if not init.is_file():
        _init_cache[key] = None
        return None
    names: set[str] = set()
    has_star = False
    try:
        tree = ast.parse(init.read_text(encoding="utf-8"))
    except SyntaxError:
        _init_cache[key] = None
        return None
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for a in node.names:
                names.add((a.asname or a.name).split(".")[0])
        elif isinstance(node, ast.ImportFrom):
            if any(a.name == "*" for a in node.names):
                has_star = True
            for a in node.names:
                if a.name != "*":
                    names.add(a.asname or a.name)
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            names.add(node.name)
        elif isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Name):
                    names.add(t.id)
        elif isinstance(node, ast.AnnAssign):
            # `NAME: type = value` is the dominant form in a typed codebase
            # (e.g. `WORKTREE_LOCAL_KEYS: frozenset[str] = ...`); without this
            # branch every annotated module-level binding reads as unexported.
            if isinstance(node.target, ast.Name):
                names.add(node.target.id)
    res = (names, has_star)
    _init_cache[key] = res
    return res


def import_target_ok(dotted_after_fno):
    """`import fno.X.Y` / `from fno.X import Y` submodule form: the path must exist."""
    return submodule_or_pkg_exists(dotted_after_fno)


def from_name_ok(pkg_after_fno, name):
    """`from fno.PKG import NAME`: NAME is a submodule on disk or re-exported."""
    if name == "*":
        return True
    # Submodule / subpackage on disk.
    f, d = _to_path(".".join([*pkg_after_fno, name]))
    if f.is_file() or d.is_dir():
        return True
    # Re-exported by the package __init__.
    seen = init_names(pkg_after_fno)
    if seen is None:
        return True  # not a package we can resolve -> don't false-positive
    names, has_star = seen
    if has_star:
        return True  # star import -> visible set unknowable statically -> skip
    return name in names


problems = []
files = sorted({p for d in scan_dirs for p in d.rglob("*.py")})
for path in files:
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except (SyntaxError, UnicodeDecodeError):
        continue
    rel = path.relative_to(src.parent)
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for a in node.names:
                head = a.name.split(".")[0]
                if head != "fno":
                    continue
                rest = a.name.split(".")[1:]  # e.g. ["agents", "registry"]
                if len(rest) >= 2 and not import_target_ok(".".join(rest)):
                    problems.append(f"{rel}:{node.lineno}: import {a.name} (no module at that path)")
        elif isinstance(node, ast.ImportFrom):
            if node.level != 0:
                continue  # relative import: skip (resolved against the importing file)
            mod = node.module or ""
            if not mod.startswith("fno.") and mod != "fno":
                continue
            pkg = mod.split(".")[1:]  # e.g. ["agents"]
            for a in node.names:
                if not from_name_ok(pkg, a.name):
                    problems.append(
                        f"{rel}:{node.lineno}: from {mod} import {a.name} (no submodule or re-export at that path)"
                    )

if problems:
    print(f"check-import-paths: {len(problems)} dead import path(s):", file=sys.stderr)
    for p in problems:
        print(f"  {p}", file=sys.stderr)
    print(
        "  A module moved and left a caller on the old path. Repoint the import to the\n"
        "  new location (or add a re-export in the package __init__ if the name is meant\n"
        "  to be public). changed-smoke cannot see this: it selects tests from changed\n"
        "  files, and a module move does not change the test importing it.",
        file=sys.stderr,
    )
    sys.exit(1)
print("check-import-paths: clean")
PY
