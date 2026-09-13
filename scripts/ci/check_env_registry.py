#!/usr/bin/env python3
"""Env-var registry: every environment variable either tree reads has a row
in docs/env-vars.md, and every row has a reader.

295 config keys carry a Meta row in cli/src/fno/config/registry.py; the ~218
environment variable names read across the Python and Rust trees had no
inventory at all (node x-72fc, measured 2026-09-12). This check closes that:
the doc IS the registry, the scanner is the ratchet.

Failures (each names the file:line and the doc):
  1. a read with no table row,
  2. a table row no read site has,
  3. a row whose Read by column (py / rs / py+rs) disagrees with the sites.

Scans (regex feeds, per the plan):
  py  cli/src/fno/**/*.py   environ.get(, os.getenv(, getenv(, environ[,
                            environ.setdefault(, environ.pop( with a literal
                            UPPER_SNAKE name
  rs  crates/**/*.rs        env::var( and env::var_os( with a literal name,
                            any path part named target excluded

Known limit: a name passed through a variable is not seen. Names read only in
tests or fixtures are out of scope (cli/tests and crates/*/tests are not
scanned).

--update rewrites the table, preserving the Meaning text of surviving rows;
new rows get `unclear: <first site>` and dead rows drop.

Usage:
  python3 scripts/ci/check_env_registry.py              # check against the doc
  python3 scripts/ci/check_env_registry.py --update     # regenerate the table
  python3 scripts/ci/check_env_registry.py --selftest   # fixture selftest

Exit: 0 pass, 1 a refusal, 2 selftest failure.
"""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
from collections import defaultdict
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
DOC_REL = Path("docs/env-vars.md")
PY_ROOT = Path("cli/src/fno")
RS_ROOT = Path("crates")

# Reads AND writes: a write defines the contract for a name as much as a read.
PY_READ_RE = re.compile(
    r"""(?x)
    environ\.(?:get|setdefault|pop)\(\s*['"]([A-Z][A-Z0-9_]+)['"]
    | os\.getenv\(\s*['"]([A-Z][A-Z0-9_]+)['"]
    | (?<![\w.])getenv\(\s*['"]([A-Z][A-Z0-9_]+)['"]
    | environ\[\s*['"]([A-Z][A-Z0-9_]+)['"]\s*\]
    """
)
RS_READ_RE = re.compile(r'\benv::var(?:_os)?\(\s*"([A-Z][A-Z0-9_]+)"')


def scan(root: Path) -> dict[str, dict[str, list[str]]]:
    """Return {name: {"py": [file:line, ...], "rs": [...]}}."""
    sites: dict[str, dict[str, list[str]]] = defaultdict(lambda: defaultdict(list))
    for path in sorted((root / PY_ROOT).rglob("*.py")):
        rel = path.relative_to(root)
        for lineno, line in enumerate(
            path.read_text(encoding="utf-8", errors="replace").splitlines(), 1
        ):
            for m in PY_READ_RE.finditer(line):
                name = next(g for g in m.groups() if g)
                sites[name]["py"].append(f"{rel}:{lineno}")
    for path in sorted((root / RS_ROOT).rglob("*.rs")):
        rel = path.relative_to(root)
        if any(part == "target" for part in rel.parts):
            continue
        if "tests" in rel.parts:
            continue
        for lineno, line in enumerate(
            path.read_text(encoding="utf-8", errors="replace").splitlines(), 1
        ):
            for m in RS_READ_RE.finditer(line):
                sites[m.group(1)]["rs"].append(f"{rel}:{lineno}")
    return sites


def read_doc(root: Path) -> dict[str, tuple[str, str]]:
    """Return {name: (read_by, meaning)} from the doc table."""
    doc = {}
    text = (root / DOC_REL).read_text(encoding="utf-8")
    for line in text.splitlines():
        m = re.match(r"^\|\s*`([A-Z][A-Z0-9_]+)`\s*\|\s*([\w+]+)\s*\|(.*)\|\s*$", line)
        if m:
            doc[m.group(1)] = (m.group(2), m.group(3).strip())
    return doc


def render(sites: dict[str, dict[str, list[str]]], meanings: dict[str, str]) -> str:
    lines = [
        "# Environment variable registry",
        "",
        "Every env var name either tree reads, one row each. The registry IS this table. `scripts/ci/check_env_registry.py` fails a read with no row, a row with no reader, and a wrong Read by. `--update` regenerates the table and preserves Meaning text.",
        "",
        "A meaning not derivable from the read site stays `unclear: <file:line>`, never invented. A name passed through a variable is not seen (scanner limit).",
        "",
        "| Name | Read by | Meaning |",
        "|------|---------|---------|",
    ]
    for name in sorted(sites):
        read_by = "+".join(sorted(sites[name]))
        first = sites[name][next(iter(sorted(sites[name])))][0]
        lines.append(f"| `{name}` | {read_by} | {meanings.get(name, f'unclear: {first}')} |")
    return "\n".join(lines) + "\n"


def run(root: Path, update: bool = False) -> int:
    doc_path = root / DOC_REL
    sites = scan(root)
    if update:
        old = read_doc(root) if doc_path.exists() else {}
        meanings = {name: meaning for name, (_, meaning) in old.items()}
        doc_path.parent.mkdir(parents=True, exist_ok=True)
        doc_path.write_text(render(sites, meanings), encoding="utf-8")
        print(f"env-registry: wrote {len(sites)} rows to {DOC_REL}")
        return 0
    doc = read_doc(root)
    fails = []
    for name, by in sorted(sites.items()):
        want = "+".join(sorted(by))
        if name not in doc:
            fails.append(
                f"{name} read at {by[sorted(by)[0]][0]} has no row in {DOC_REL}"
            )
        elif doc[name][0] != want:
            fails.append(
                f"{name}: doc says Read by {doc[name][0]}, sites say {want} "
                f"(first: {by[sorted(by)[0]][0]}); fix {DOC_REL}"
            )
    for name in sorted(set(doc) - set(sites)):
        fails.append(f"{name}: row in {DOC_REL} has no read site; delete the row")
    if fails:
        print(f"env-registry: FAIL ({len(fails)})\n" + "\n".join(fails), file=sys.stderr)
        return 1
    print(f"env-registry: ok ({len(sites)} names, all rows agree)")
    return 0


def selftest() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        py = root / PY_ROOT
        rs = root / RS_ROOT / "fno" / "src"
        py.mkdir(parents=True)
        rs.mkdir(parents=True)
        (py / "a.py").write_text(
            'import os\nx = os.environ.get("FOO_BAR")\ny = os.environ["BAZ_QUX"]\n',
            encoding="utf-8",
        )
        (rs / "b.rs").write_text(
            'fn f() { if let Ok(v) = std::env::var("FOO_BAR") {} }\n', encoding="utf-8"
        )
        # A read inside a tests dir is out of scope by contract.
        tests = root / RS_ROOT / "fno" / "tests"
        tests.mkdir()
        (tests / "t.rs").write_text('fn t() { env::var("TEST_ONLY"); }\n', encoding="utf-8")

        run(root, update=True)
        doc = read_doc(root)
        if set(doc) != {"FOO_BAR", "BAZ_QUX"}:
            print(f"selftest: rows wrong: {sorted(doc)}", file=sys.stderr)
            return 2
        if doc["FOO_BAR"][0] != "py+rs":
            print("selftest: FOO_BAR must read py+rs", file=sys.stderr)
            return 2
        if run(root) != 0:
            print("selftest: consistent tree did not pass", file=sys.stderr)
            return 2

        # A hand-written Meaning must survive --update.
        doc_path = root / DOC_REL
        doc_path.write_text(
            doc_path.read_text(encoding="utf-8").replace(
                "unclear: cli/src/fno/a.py:2", "the worker marker"
            ),
            encoding="utf-8",
        )

        (py / "a.py").write_text(
            'import os\nx = os.environ.get("NEW_NAME")\n', encoding="utf-8"
        )
        if run(root) != 1:
            print("selftest: unrowed read did not fail", file=sys.stderr)
            return 2

        (py / "a.py").write_text(
            'import os\nx = os.environ.get("FOO_BAR")\ny = os.environ["BAZ_QUX"]\n',
            encoding="utf-8",
        )
        (rs / "b.rs").write_text("", encoding="utf-8")
        if run(root) != 1:
            print("selftest: wrong read-by did not fail", file=sys.stderr)
            return 2

        (rs / "b.rs").write_text('fn g() { env::var_os("GONE_ROW"); }\n', encoding="utf-8")
        run(root, update=True)
        doc = read_doc(root)
        if set(doc) != {"FOO_BAR", "BAZ_QUX", "GONE_ROW"}:
            print("selftest: --update did not reconcile rows", file=sys.stderr)
            return 2
        if doc["FOO_BAR"][1] != "the worker marker":
            print("selftest: --update dropped a hand-written Meaning", file=sys.stderr)
            return 2
        if run(root) != 0:
            print("selftest: updated tree did not pass", file=sys.stderr)
            return 2
    print("selftest: ok")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--update", action="store_true", help="regenerate the doc table")
    parser.add_argument("--selftest", action="store_true", help="fixture selftest")
    args = parser.parse_args()
    if args.selftest:
        return selftest()
    return run(REPO_ROOT, update=args.update)


if __name__ == "__main__":
    sys.exit(main())
