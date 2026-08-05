#!/usr/bin/env bash
# Enforce the four-axis vocabulary: no identifier, dict/JSON key, or env var named
# for one axis (provider / harness / model) may hold a literal from another.
# See docs/architecture/four-axis-vocabulary.md for the axis definitions this guards.
set -euo pipefail

MODE="baseline"
MODE_SEEN=""
SELF_TEST=0
WRITE_BASELINE=0
BASELINE_FILE=""
BASELINE_FILE_SEEN=0
ROOT=""

usage() {
  cat <<'EOF'
Usage: check-axis-vocabulary.sh [--baseline|--strict|--self-test|--write-baseline] [--baseline-file PATH] [ROOT]

Modes:
  --baseline        CI ratchet (default): pass only when the checked-in finding set is
                    unchanged. A reduction passes after its baseline is updated in the
                    same PR that removes the violation.
  --strict          Full audit: report every violation and fail while any remain.
  --self-test       Plant a synthetic violation in each scanned language (python, rust,
                    shell, markdown) and assert the guard catches every one, then assert
                    a clean tree passes. Proves the scanner reaches every language.
  --write-baseline  Regenerate the baseline file from the current finding set. Use after
                    a wave removes violations; commit the updated baseline in that PR.

The default mode is --baseline. ROOT defaults to the current repository root.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --baseline|--strict)
      requested="${1#--}"
      if [[ -n "$MODE_SEEN" && "$MODE_SEEN" != "$requested" ]]; then
        echo "check-axis-vocabulary: choose exactly one mode" >&2
        exit 2
      fi
      MODE="$requested"
      MODE_SEEN="$requested"
      shift
      ;;
    --self-test)
      SELF_TEST=1
      shift
      ;;
    --write-baseline)
      WRITE_BASELINE=1
      shift
      ;;
    --baseline-file)
      if [[ $# -lt 2 ]]; then
        echo "check-axis-vocabulary: --baseline-file requires a path" >&2
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
      echo "check-axis-vocabulary: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [[ -n "$ROOT" ]]; then
        echo "check-axis-vocabulary: unexpected argument: $1" >&2
        exit 2
      fi
      ROOT="$1"
      shift
      ;;
  esac
done

ROOT="${ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
BASELINE_FILE="${BASELINE_FILE:-$ROOT/scripts/ci/axis-vocabulary-baseline.txt}"

if [[ "$MODE" == "strict" && $BASELINE_FILE_SEEN -eq 1 ]]; then
  echo "check-axis-vocabulary: --baseline-file is valid only with --baseline" >&2
  exit 2
fi

if [[ $SELF_TEST -eq 1 ]]; then
  export AXIS_SELF_TEST=1
fi

if [[ $WRITE_BASELINE -eq 1 ]]; then
  MODE="write-baseline"
fi

python3 - "$ROOT" "$MODE" "$BASELINE_FILE" <<'PY'
import os
import re
import sys
from pathlib import Path

root_arg = Path(sys.argv[1]).resolve()
mode = sys.argv[2]
baseline_path = Path(sys.argv[3])

# --- Axis definitions (mirror docs/architecture/four-axis-vocabulary.md) -------
# Harness values unambiguous enough to auto-flag under a provider name. gemini is
# deliberately excluded: it names a harness, a provider, and a model family, so
# auto-flagging fires on correct code. gemini sites are reviewed via the allowlist.
# opencode is flagged but allowlistable.
HARNESS_LITERALS = ("claude", "codex", "agy", "opencode")
VENDOR_LITERALS = ("anthropic", "openai", "zai", "deepseek", "google")

SCANNABLE_EXT = {".py", ".rs", ".sh", ".bash", ".md", ".yaml", ".yml"}

# Never scanned: build output, caches, the worktree forest under .claude/worktrees
# (sibling copies that would multiply every finding), the untracked vault symlink,
# and per-CLI config roots.
EXCLUDE_DIR = {
    "target", ".git", "node_modules", ".venv", "venv", "__pycache__",
    ".mypy_cache", ".pytest_cache", ".ruff_cache", "dist", "build",
    ".claude", ".codex", ".gemini", ".agents", "internal", ".next", "out",
    ".fno",
}

# The guard and its own baseline are not scanned: the literal sets below would
# self-trigger, and the baseline is data, not code.
SELF_FILES = {
    "scripts/ci/check-axis-vocabulary.sh",
    "scripts/ci/axis-vocabulary-baseline.txt",
}

_axis_word = re.compile(r"(?i)(provider|harness|model)")
# Word-bounded so "claude" does not match inside claude_mod / codex_session_id /
# agy_adapter: those are identifier references, not the axis literal.
_literal_in = re.compile(r"(?i)\b(claude|codex|agy|opencode|anthropic|openai|zai|deepseek|google)\b")
# A binding name is an identifier containing an axis word, with an optional
# attribute/quote prefix. Matched against the text immediately preceding a literal.
_name_before_sep = re.compile(r"(?i)([A-Za-z0-9_.]*?(?:provider|harness|model)[A-Za-z0-9_]*)[\"']?\s*[:=,]\s*[\"']?$")


def _classify(name: str, literal: str):
    low = name.lower()
    if "provider" in low and literal in HARNESS_LITERALS:
        return "provider-named binding holds a harness literal"
    if "harness" in low and literal in VENDOR_LITERALS:
        return "harness-named binding holds a vendor literal"
    if "model" in low and (literal in HARNESS_LITERALS or literal in VENDOR_LITERALS):
        kind = "harness" if literal in HARNESS_LITERALS else "vendor"
        return f"model-named binding holds a {kind} literal"
    return None


def _findings_for_line(rel: str, lineno: int, line: str):
    """Finding strings for axis conflation on one source line.

    For each axis literal on the line, look at the token immediately before it
    (across an assignment/key/env separator) and flag it when the binding's axis
    disagrees with the literal's axis. The separator class [:=,] plus optional
    quotes covers assignment, dict/json key, and env-injection shapes in one rule.
    """
    out = []
    seen = set()
    for m in _literal_in.finditer(line):
        literal = m.group(1).lower()
        bm = _name_before_sep.search(line[: m.start()])
        name = bm.group(1) if bm else None
        if not name:
            continue
        desc = _classify(name, literal)
        if not desc:
            continue
        key = (name.lower(), literal)
        if key in seen:
            continue
        seen.add(key)
        out.append(f'{rel}:{lineno}: {name}="{literal}" ({desc})')
    return out


def scan(root: Path):
    findings = []
    observed_by_ext = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in EXCLUDE_DIR]
        for fn in filenames:
            ext = os.path.splitext(fn)[1]
            if ext not in SCANNABLE_EXT:
                continue
            path = os.path.join(dirpath, fn)
            try:
                rel = os.path.relpath(path, root)
            except ValueError:
                continue
            if rel in SELF_FILES:
                continue
            try:
                with open(path, encoding="utf-8", errors="replace") as fh:
                    lines = fh.readlines()
            except OSError:
                continue
            reached = False
            for i, raw in enumerate(lines, start=1):
                line = raw.rstrip("\n")
                if not reached and _axis_word.search(line):
                    observed_by_ext[ext] = observed_by_ext.get(ext, 0) + 1
                    reached = True
                findings.extend(_findings_for_line(rel, i, line))
    return sorted(set(findings)), observed_by_ext


def _self_test():
    """Plant one violation per scanned language, assert each caught; assert a clean
    tree passes. Prints exactly `caught planted <lang> violation` per hit."""
    import tempfile

    specimens = {
        "python": ("v.py", 'provider = "claude"\n'),
        "rust": ("v.rs", 'cmd.env("FNO_AGENT_PROVIDER", "claude");\n'),
        "shell": ("v.sh", "PROVIDER=claude\n"),
        "markdown": ("v.md", '{"provider": "claude"}\n'),
    }
    failures = 0
    for lang, (name, body) in specimens.items():
        d = Path(tempfile.mkdtemp(prefix=f"axis-{lang}-"))
        (d / name).write_text(body, encoding="utf-8")
        found, _ = scan(d)
        if found:
            print(f"caught planted {lang} violation")
        else:
            print(f"FAILED to catch planted {lang} violation", file=sys.stderr)
            failures += 1
    d = Path(tempfile.mkdtemp(prefix="axis-clean-"))
    (d / "ok.py").write_text('provider = "anthropic"\n', encoding="utf-8")
    found, _ = scan(d)
    if found:
        print("FAILED: clean tree produced findings", file=sys.stderr)
        for f in found:
            print(f"  {f}", file=sys.stderr)
        failures += 1
    else:
        print("caught planted clean-tree (no findings) ok")
    return 1 if failures else 0


if os.environ.get("AXIS_SELF_TEST") == "1":
    sys.exit(_self_test())

findings, observed = scan(root_arg)

# Positive control (AC3): the scan must reach real content in BOTH Python and
# Rust, the two languages carrying the most axis-named bindings. A scan that
# observes no provider/harness/model tokens in either has not reached content and
# must fail rather than report a clean tree. This is the liveness check that makes
# a zero-finding result trustworthy through a cutover that removes findings.
missing = [lang for lang in (".py", ".rs") if observed.get(lang, 0) == 0]
if missing:
    print(
        "check-axis-vocabulary: positive control failed "
        f"(no axis-named bindings observed in {', '.join(missing)}); "
        "scan did not reach content",
        file=sys.stderr,
    )
    sys.exit(2)

print(
    "check-axis-vocabulary: positive control ok "
    f"(observed axis bindings: py={observed.get('.py', 0)}, "
    f"rs={observed.get('.rs', 0)}, sh={observed.get('.sh', 0) + observed.get('.bash', 0)}, "
    f"md={observed.get('.md', 0)})"
)

if mode == "write-baseline":
    header = [
        "# Known four-axis vocabulary findings held by the CI ratchet (check-axis-vocabulary.sh).",
        "# Each entry is exact: file, line, the binding, the literal, and the axis collision.",
        "# A binding named for one axis (provider/harness/model) may not hold a literal from another.",
        "# Remove an entry only in the same PR that removes the violation. Convert a genuinely",
        "# correct ambiguous site (opencode/gemini) to an `allowlist:` line with a one-line",
        "# justification; see docs/architecture/four-axis-vocabulary.md.",
        "# Regenerate: bash scripts/ci/check-axis-vocabulary.sh --write-baseline",
        "",
    ]
    # Preserve existing allowlist lines (correct ambiguous sites, time-boxed windows)
    # across regenerations so a wave's allowlist entry is not wiped by a later regen.
    allowlist_entries = []
    if baseline_path.exists():
        for line in baseline_path.read_text(encoding="utf-8").splitlines():
            s = line.strip()
            if s.startswith("allowlist:"):
                allowlist_entries.append(s)
    body = header + sorted(set(findings)) + [""] + sorted(set(allowlist_entries))
    baseline_path.write_text("\n".join(body) + "\n", encoding="utf-8")
    print(f"check-axis-vocabulary: wrote {len(findings)} findings to {baseline_path}")
    sys.exit(0)

if mode == "strict":
    if findings:
        print("check-axis-vocabulary: axis violations remain:", file=sys.stderr)
        for f in findings:
            print(f"  {f}", file=sys.stderr)
        print(
            "No identifier/key/env named for one axis may hold a literal from "
            "another; see docs/architecture/four-axis-vocabulary.md",
            file=sys.stderr,
        )
        sys.exit(1)
    print("check-axis-vocabulary: strict clean, 0 violations")
    sys.exit(0)

# --baseline: exact set comparison in BOTH directions (a resolved entry that is not
# removed from the baseline fails too), like check-company-boundaries.
try:
    raw = baseline_path.read_text(encoding="utf-8").splitlines()
except (OSError, UnicodeError) as exc:
    print(
        f"check-axis-vocabulary: baseline could not be read: {baseline_path}: {exc}",
        file=sys.stderr,
    )
    sys.exit(2)

baseline = [s.strip() for s in raw if s.strip() and not s.lstrip().startswith("#")]
finding_pat = re.compile(
    r"^.+?:\d+: .+?=\"(?:claude|codex|agy|opencode|anthropic|openai|zai|deepseek|google)\" \(.+\)$"
)
allowlist_pat = re.compile(r"^allowlist:\s+.+$")
malformed = [e for e in baseline if not finding_pat.match(e) and not allowlist_pat.match(e)]
if malformed or len(baseline) != len(set(baseline)):
    print("check-axis-vocabulary: baseline is malformed or contains duplicates", file=sys.stderr)
    for e in malformed:
        print(f"  {e}", file=sys.stderr)
    sys.exit(2)

current = list(findings)
new_or_changed = sorted(set(current) - set(baseline))
resolved = sorted(set(baseline) - set(current))
if new_or_changed or resolved:
    print("check-axis-vocabulary: baseline drift:", file=sys.stderr)
    for e in new_or_changed:
        print(f"  new or changed violation: {e}", file=sys.stderr)
    for e in resolved:
        print(f"  resolved or changed baseline entry: {e}", file=sys.stderr)
    print(
        "Update the baseline only in the same PR that removes the violation; "
        "new or changed violations are prohibited.",
        file=sys.stderr,
    )
    sys.exit(1)

if current:
    label = "violation" if len(current) == 1 else "violations"
    print(f"check-axis-vocabulary: baseline holds {len(current)} {label}; strict audit remains red")
else:
    print("check-axis-vocabulary: 0 violations, baseline empty")
sys.exit(0)
PY
