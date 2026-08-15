#!/usr/bin/env bash
# check-uv-install-compiles-bytecode.sh - every `uv tool install` must compile.
#
# A tool venv that ships no bytecode is written to by every process that uses
# it (pr-watch, hooks, any caller), and those __pycache__ writes race a later
# `--force`/`--reinstall` into ENOTEMPTY (os error 66), which strands the CLI
# with no entrypoint. `--compile-bytecode` at install time ships the .pyc up
# front so no process ever has a reason to write into the venv again. Full
# story: docs/architecture/cli-lazy-imports.md.
#
# This gate fails the moment a NEW provisioning call site lands without the
# flag. Prose mentions (log strings, error text, docstrings) that merely name
# the command are recorded in the baseline with the line they quote.
#
# Run:  bash scripts/ci/check-uv-install-compiles-bytecode.sh
#       bash scripts/ci/check-uv-install-compiles-bytecode.sh --self-test
# Exit: 0 clean, 1 an unflagged invocation or an unrecorded/removed prose
#       line, 2 misuse.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
BASELINE="$REPO_ROOT/scripts/ci/uv-install-bytecode-baseline.txt"

if [[ ! -f "$BASELINE" ]]; then
  echo "check-uv-install-compiles-bytecode: baseline missing at $BASELINE" >&2
  exit 2
fi

python3 - "$REPO_ROOT" "$BASELINE" "${1:-}" <<'PY'
import re
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(sys.argv[1])
baseline_path = Path(sys.argv[2])
mode = sys.argv[3] if len(sys.argv) > 3 else ""

# An invocation is `uv tool install` (bare or behind a quoted runner like
# "$FNO_UV") at command position - a backtick before `uv` means the line
# QUOTES the command (docs, log strings, error text) - a Rust args array, or
# a Python argv list. Arrays can span lines, so the flag search window
# extends to the closing bracket.
RE_SHELL = re.compile(r'(?<!`)(?:\buv\b|["\'][^"\']+["\'])\s+tool\s+install')
RE_RUST = re.compile(r'["\']tool["\']\s*,\s*["\']install["\']')
RE_PY = re.compile(r'["\']uv["\']\s*,\s*["\']tool["\']\s*,\s*["\']install["\']')
# Command position: the command words sit at line start OR right after a shell
# separator (`;`, `&&`, `||`, a pipe), optionally behind keyword/wrapper words
# (`if`, `then`, `exec`, `sudo`, a YAML `run:`), and are spelled either bare
# (`uv`) or as a quoted runner ("$FNO_UV"). Anchoring at line start ALONE let a
# real run site behind `exec`, `sudo`, or `&&` read as prose, and prose is
# baseline-able - so the gate could be silenced over a live unflagged install.
# `(` is deliberately NOT a separator: prose routinely parenthesizes the
# command mid-sentence.
RE_CMDPOS = re.compile(
    r'(?:^|[;|&]\s*)\s*'
    r'(?:(?:if|then|else|elif|do|exec|sudo|env|time|run:|!)\s+)*'
    r'(?:"[^"]*"|uv)\s+tool\s+install'
)
SKIP_SUFFIXES = {".md", ".lock", ".json", ".toml"}
# Self-reference: this checker's own --self-test fixtures quote unflagged
# invocations on purpose, and the baseline's entries ARE the quoted lines.
# Scanning either would flag the gate against itself.
SKIP_FILES = {
    "scripts/ci/check-uv-install-compiles-bytecode.sh",
    "scripts/ci/uv-install-bytecode-baseline.txt",
}
WINDOW_MAX = 10


def classify(line):
    """'inv' when the line runs the command, 'prose' when it names it."""
    # Comments first: a commented-out call site executes nothing, so it is
    # neither a run site nor quotable prose - just skip it.
    if line.lstrip().startswith(("#", "//")):
        return None
    if RE_RUST.search(line) or RE_PY.search(line) or '"tool"' in line or "'tool'" in line:
        return "inv"
    return "inv" if RE_CMDPOS.search(line) else "prose"


def window_for(path_lines, i):
    """The flag search window: to the closing bracket for arrays, across
    backslash continuations for shell."""
    line = path_lines[i]
    window = line
    j = i
    if RE_RUST.search(line) or RE_PY.search(line):
        while "]" not in window and j + 1 < len(path_lines) and j - i < WINDOW_MAX:
            j += 1
            window += " " + path_lines[j].strip()
    else:
        while path_lines[j].rstrip().endswith("\\") and j + 1 < len(path_lines) and j - i < WINDOW_MAX:
            j += 1
            window += " " + path_lines[j].strip()
    return window, line


def collect(root_dir, file_list):
    """[(rel, kind, lineno, window)] for every line that names or runs the command."""
    out = []
    for rel in file_list:
        p = root_dir / rel
        if p.suffix in SKIP_SUFFIXES or rel in SKIP_FILES or not p.is_file():
            continue
        try:
            lines = p.read_text(encoding="utf-8").splitlines()
        except (UnicodeDecodeError, OSError):
            continue
        for i, line in enumerate(lines):
            if not (RE_SHELL.search(line) or RE_RUST.search(line) or RE_PY.search(line)):
                continue
            kind = classify(line)
            if kind is None:
                continue
            window, raw = window_for(lines, i)
            out.append((rel, kind, i + 1, window, raw))
    return out


def check(pairs):
    """Split candidates into unflagged invocations and unflagged prose mentions."""
    violations, prose = [], []
    for rel, kind, lineno, window, raw in pairs:
        if "--compile-bytecode" in window:
            continue
        # Hoisted out of the f-string: a backslash inside an f-string
        # expression is a SyntaxError before Python 3.12, and this repo
        # supports 3.11.
        collapsed = re.sub(r"\s+", " ", raw.strip())
        key = f"{rel}::{collapsed}"
        (violations if kind == "inv" else prose).append((rel, lineno, key))
    return violations, prose


def run_self_test():
    # Positive controls: the scanner must MATCH a real invocation (an absence
    # here would certify an empty search) and the classifier must FLAG it,
    # while clean invocations, quoted prose, and comments stay silent.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        cases = {
            "a.sh": 'if uv tool install --force "$CLI_DIR"; then',
            "b.rs": '        .args(["tool", "install", "--force", source])',
            "c.py": 'cmd = [\n    "uv", "tool", "install",\n    "--reinstall", "--refresh",\n    str(resolved),\n]',
            "ok.sh": 'if uv tool install --force --compile-bytecode "$X"; then',
            "ok.rs": '.args(["tool", "install", "--force", "--compile-bytecode", source])',
            "ok.py": 'cmd = ["uv", "tool", "install", "--reinstall", "--compile-bytecode", src]',
            "prose.sh": 'log "preferring the published PyPI wheel: uv tool install fno (by name)..."',
            "prose2.sh": 'err "uv tool install failed; falling through to pip fallback."',
            "quoted.sh": '# run `uv tool install --force fno` manually to see the error',
            "quoted.rs": '/// `uv tool install fno` (the PyPI platform wheel) registers the tool.',
            # Single-quoted Python is standard style: it must flag the same.
            "single.py": "cmd = ['uv', 'tool', 'install', '--reinstall', src]",
            # Commented-out code executes nothing: skipped entirely.
            "commented.rs": '            // .args(["tool", "install", "--force", source])',
            # A run site is a run site behind a wrapper word or a separator.
            # Reading these as prose would let a baseline entry bless them.
            "exec.sh": 'exec uv tool install --force fno',
            "sudo.sh": 'sudo uv tool install --force fno',
            "chain.sh": 'command -v uv && uv tool install --force fno',
            "runner.sh": 'if ! "$FNO_UV" tool install --force "$SRC"; then',
            "yaml.yml": '      run: uv tool install --force fno',
        }
        for name, body in cases.items():
            (tmp / name).write_text(body + "\n", encoding="utf-8")
        pairs = collect(tmp, list(cases))
        violations, prose = check(pairs)
        assert sorted(v[0] for v in violations) == [
            "a.sh", "b.rs", "c.py", "chain.sh", "exec.sh", "runner.sh",
            "single.py", "sudo.sh", "yaml.yml",
        ], violations
        assert sorted(p[0] for p in prose) == ["prose.sh", "prose2.sh"], prose
    print("check-uv-install-compiles-bytecode self-test OK")
    sys.exit(0)


if mode == "--self-test":
    run_self_test()
if mode != "":
    print(f"check-uv-install-compiles-bytecode: unknown mode {mode!r}", file=sys.stderr)
    sys.exit(2)

tracked = subprocess.run(
    ["git", "ls-files"], cwd=root, capture_output=True, text=True, check=True
).stdout.splitlines()

violations, prose = check(collect(root, tracked))
baseline = {
    ln.strip()
    for ln in baseline_path.read_text(encoding="utf-8").splitlines()
    if ln.strip() and not ln.lstrip().startswith("#")
}
all_keys = {k for _, _, k in violations} | {k for _, _, k in prose}
unrecorded = all_keys - baseline
stale = baseline - all_keys
# The header promises "a real run site is NOT baseline-able"; enforce it,
# or a contributor quiets the gate by recording the very thing it exists
# to stop. Only classified-prose lines may carry a baseline entry.
baselined_runs = {k for _, _, k in violations} & baseline

fail = False
for k in sorted(baselined_runs):
    print(
        f"check-uv-install-compiles-bytecode: {k} "
        "RUNS `uv tool install` without --compile-bytecode; a baseline entry "
        "cannot bless a run site. Add the flag to the command.",
        file=sys.stderr,
    )
    fail = True
for k in sorted(unrecorded):
    print(
        f"check-uv-install-compiles-bytecode: {k} "
        "names or runs `uv tool install` without --compile-bytecode on the "
        "same command. A QUOTE of the command may be recorded in "
        f"{baseline_path.name} with a reason.",
        file=sys.stderr,
    )
    fail = True
for k in sorted(stale):
    print(
        "check-uv-install-compiles-bytecode: baseline entry matches nothing "
        f"(stale?): {k}",
        file=sys.stderr,
    )
    fail = True
if fail:
    sys.exit(1)
print(
    "check-uv-install-compiles-bytecode: ok "
    f"({len(violations) + len(prose)} candidate line(s), all flagged or recorded)"
)
PY
