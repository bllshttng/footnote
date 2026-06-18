#!/usr/bin/env python3
"""Unit tests for completion-summary.py output-path routing (.completed/).

The last-resort branch of resolve_output_path() used to drop completion-*.md
straight into the .fno/ root, where they accumulate one-per-shipped-PR. The
dir-hygiene change routes that fallback into the existing .completed/
subfolder instead, creating it on demand.

Covers AC1-HP (unset config -> path under .fno/.completed/) and AC1-EDGE
(missing .completed/ is created, no crash).

Run: python3 tests/metrics/test_completion_summary_path.py
"""

import importlib.util
import os
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
MODULE_PATH = REPO_ROOT / "scripts" / "metrics" / "completion-summary.py"

_spec = importlib.util.spec_from_file_location("completion_summary", MODULE_PATH)
cs = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(cs)

PASS = 0
FAIL = 0


def check(name: str, cond: bool) -> None:
    global PASS, FAIL
    if cond:
        print(f"  PASS: {name}")
        PASS += 1
    else:
        print(f"  FAIL: {name}")
        FAIL += 1


def _hermetic(fn):
    """Run fn() in a temp cwd with HOME pointed at the temp dir so no real
    ~/.fno/settings.yaml (completions_path) can leak into the resolution."""
    prev_cwd = os.getcwd()
    prev_home = os.environ.get("HOME")
    with tempfile.TemporaryDirectory() as d:
        try:
            os.chdir(d)
            os.environ["HOME"] = d
            return fn(Path(d))
        finally:
            os.chdir(prev_cwd)
            if prev_home is not None:
                os.environ["HOME"] = prev_home


# ---- AC1-HP: unset completions_path -> path under .fno/.completed/ ----
print("AC1-HP: last-resort routes into .fno/.completed/")


def _hp(d: Path):
    out = cs.resolve_output_path(None, None, {"input": "demo feature"})
    rel = out.relative_to(Path(".fno"))
    check("returned under .fno/.completed/", rel.parts[0] == ".completed")
    check("filename is completion-*.md", out.name.startswith("completion-") and out.name.endswith(".md"))
    check(".completed/ was created", (d / ".fno" / ".completed").is_dir())


_hermetic(_hp)

# ---- AC1-EDGE: missing .completed/ created, write succeeds ----
print("AC1-EDGE: missing .completed/ is created and a write succeeds")


def _edge(d: Path):
    # .fno/ exists but .completed/ does not yet.
    (d / ".fno").mkdir()
    check(".completed/ absent before call", not (d / ".fno" / ".completed").exists())
    out = cs.resolve_output_path(None, None, {"input": "edge case"})
    out.write_text("# completion\n")  # the real writer's mkdir must have run
    check("write to resolved path succeeded", out.is_file())
    check(".completed/ created by resolver", (d / ".fno" / ".completed").is_dir())


_hermetic(_edge)

# ---- summary ----
total = PASS + FAIL
print()
if FAIL == 0:
    print(f"PASS: completion-summary path tests ({PASS}/{total})")
    raise SystemExit(0)
print(f"FAIL: completion-summary path tests ({PASS} passed, {FAIL} failed of {total})")
raise SystemExit(1)
