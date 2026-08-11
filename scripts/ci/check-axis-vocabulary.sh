#!/usr/bin/env bash
# Enforce the four-axis vocabulary in two independent scans:
#   contents - no identifier, dict/JSON key, or env var named for one axis
#              (provider / harness / model) may hold a literal from another;
#   names    - no DIRECTORY named for one axis may hold another axis's
#              implementation, judged against the DECLARED_PATH_AXIS map below.
# Only the contents scan is baselined. See docs/architecture/four-axis-vocabulary.md
# for the axis definitions this guards.
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
# Anchored to the git root, never to ROOT. ROOT may be a subtree, and deriving
# the baseline from it resolved to a path that does not exist: the allowlist
# silently stopped suppressing and justified ambiguous sites reappeared as
# violations, while `--baseline <subtree>` exited 2 outright.
BASELINE_ROOT="$(git -C "$ROOT" rev-parse --show-toplevel 2>/dev/null || echo "$ROOT")"
BASELINE_FILE="${BASELINE_FILE:-$BASELINE_ROOT/scripts/ci/axis-vocabulary-baseline.txt}"

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

# Class guard: every path key routes through _repo_key, and a seventh site that
# keys directly is how this class returns. Four review passes found one instance
# of it per pass, in four different constants, because each fix corrected a
# constant rather than the class. A helper alone is a snapshot; this is what
# makes it hold. Legitimate uses carry a `repo-key-exempt:` note naming why.
SELF_PATH="${BASH_SOURCE[0]}"
# Exported so --self-test can re-invoke the real entry point end to end. A suite
# that only calls the helper functions proves the fixtures ran, never that the
# production path still calls them.
export AXIS_SELF_PATH="$SELF_PATH"
_relpath_total=$(grep -c 'os\.path\.relpath(' "$SELF_PATH" || true)
if [[ "${_relpath_total:-0}" -eq 0 ]]; then
  echo "check-axis-vocabulary: repo-key guard found no os.path.relpath at all;" >&2
  echo "  the probe is broken, not the file. Refusing to report it clean." >&2
  exit 2
fi
_relpath_stray=$(grep -n 'os\.path\.relpath(' "$SELF_PATH" | grep -v 'repo-key-exempt' || true)
if [[ -n "$_relpath_stray" ]]; then
  echo "check-axis-vocabulary: path key bypasses _repo_key:" >&2
  echo "$_relpath_stray" | sed 's/^/  /' >&2
  echo "  Route it through _repo_key, or mark it '# repo-key-exempt: <why>'." >&2
  exit 2
fi

python3 - "$ROOT" "$MODE" "$BASELINE_FILE" <<'PY'
import os
import re
import subprocess
import sys
from collections import Counter
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

# --- Declared axis for axis-NAMED directories ---------------------------------
# The content scan above reads what the code says. It cannot read what the code
# is CALLED: a path is a traversal input to os.walk, never a subject. That blind
# spot let two directories named `providers` coexist for months, one holding
# harness adapters and one holding real providers, so a reader who learned one
# could not predict the other.
#
# Classifying a directory by its contents was measured and rejected. The
# harness-adapter package held 298 harness literals to 5 vendor; the correctly
# named provider package held 1024 to 49, because rotation and failover
# legitimately name the harnesses whose accounts they rotate. Any threshold that
# flags the wrong directory also flags the right one, so inference here is not
# merely weak, it is inverted.
#
# What this map holds is a HUMAN DECLARATION of the axis a directory's contents
# implement. The gate compares that declaration against the axis the directory's
# NAME states and fails on disagreement. It never opens the directory to check
# the declaration is true. The teeth are in the completeness half below: a new
# axis-named directory that nobody declared fails, which is exactly the event
# this guard exists to catch.
#
# Inline rather than a file on purpose: three axis-named directories exist in the
# whole repository, and a separate file would buy a parser, a format validator,
# and a malformed-entry branch to hold one line.
# Declare NOT_AN_AXIS when a name merely contains an axis word without being
# about that axis, such as a `models/` package of Pydantic types. Without it the
# only green outcomes for such a directory are renaming a correct name or
# writing a declaration this doc defines as its implemented axis, knowingly
# false. The sentinel is still a reviewable claim, not a silent skip.
NOT_AN_AXIS = "not-an-axis"

DECLARED_PATH_AXIS = {
    # Provider records, accounts, rotation, failover, benchmarks.
    "cli/src/fno/adapters/providers": "provider",
    # Harness subprocess adapters: claude.py, codex.py, opencode.py.
    "cli/src/fno/agents/harnesses": "harness",
    # Per-harness pages and the harness adapter/dispatch table.
    "docs/harnesses": "harness",
}

SCANNABLE_EXT = {".py", ".rs", ".sh", ".bash", ".md", ".yaml", ".yml"}

# Never scanned: build output, caches, the worktree forest under .claude/worktrees
# (sibling copies that would multiply every finding), the untracked vault symlink,
# and per-CLI config roots.
EXCLUDE_DIR = {
    ".git", "node_modules", ".venv", "venv", "__pycache__",
    ".mypy_cache", ".pytest_cache", ".ruff_cache", "dist", "build",
    ".claude", ".codex", ".gemini", ".agents", "internal", ".next", "out",
    ".fno",
}

# `target` is NOT in the set above, because pruning it by bare name pruned five
# directories and only two were build output. Measured: skills/target (66
# tracked files), cli/src/fno/target (7), and tests/target (5) are source that
# neither scan ever opened, while crates/fno/target and crates/fno-agents/target
# hold zero tracked files between them. The exclusion excluded only real code.
#
# Anchored to where build output actually lives. The remaining bare names above
# share the latent shape: any of them is prunable at any depth. None of them
# currently collides with real content in this repo, and each gets an anchor
# here the day it does.
EXCLUDE_ANCHORED = {
    "crates/fno/target",
    "crates/fno-agents/target",
}

# An adversarial probe, deliberately placed where the walk used to be blind
# rather than where it was always going to pass. The two earlier positive
# controls asserted only that the three DECLARED_PATH_AXIS directories were
# reached, and all three sit outside every excluded name, so they sampled the
# region that could not fail. A regression to bare-name pruning fails here.
REQUIRED_REACH = ("skills/target",)

# The guard and its own baseline are not scanned: the literal sets below would
# self-trigger, and the baseline is data, not code.
SELF_FILES = {
    "scripts/ci/check-axis-vocabulary.sh",
    "scripts/ci/axis-vocabulary-baseline.txt",
}

_axis_word = re.compile(r"(?i)(provider|harness|model)")
# A literal that is a COMPLETE value, not a sub-word of a larger token: either a
# quoted string whose entire content is the axis word ("claude", NOT
# "claude-sonnet-5"), or a bare token bounded by a separator/space/paren. The
# lookbehind/lookahead anchor the value without consuming it, so the binding-name
# search back from m.start() still sees the opening quote or separator, and so
# "claude" inside claude_mod / codex_session_id never matches.
_literal_in = re.compile(
    r"(?i)(?:(?<=[\"'])|(?<=[:=,\s(]))(claude|codex|agy|opencode|anthropic|openai|zai|deepseek|google)(?=[\"'\s,);]|$)"
)
# A binding name is an identifier containing an axis word, with an optional
# attribute/quote prefix. Matched against the text immediately preceding a literal.
# Matches the binding name immediately preceding a literal across assignment,
# dict/json key, env-injection, AND dict-subscript shapes:
#   name = "x"  ·  "name": "x"  ·  .env("name", "x")  ·  env["name"] = "x"
# The optional `\]` after the closing quote is what reaches the subscript shape.
_name_before_sep = re.compile(r"(?i)([A-Za-z0-9_.]*?(?:provider|harness|model)[A-Za-z0-9_]*)[\"']?\]?\s*[:=,]\s*[\"']?$")


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


def _stated_axis(name: str):
    """The axis a path component's NAME states, or None."""
    m = _axis_word.search(name)
    return m.group(1).lower() if m else None


def _repo_root_for(root: Path) -> Path:
    """The git root above `root`, else `root` itself.

    DECLARED_PATH_AXIS keys are repo-root-relative, so a directory scanned
    under a SUBTREE root must still be keyed from the repo root. Keying from
    the scan root instead made every correct directory read as undeclared:
    `check-axis-vocabulary.sh --strict cli` reported
    `src/fno/adapters/providers/` as a violation and advised renaming it.
    """
    proc = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
    )
    if proc.returncode == 0 and proc.stdout.strip():
        # Resolved: git prints the real path, while a caller's root can carry a
        # symlinked prefix (on macOS /var vs /private/var). Comparing the two
        # unresolved yields a relpath full of `..` rather than a repo-relative
        # key, and every declaration misses.
        return Path(proc.stdout.strip()).resolve()
    return root.resolve()


def _repo_key(path, root: Path, repo_root: Path) -> str:
    """THE repo-root-relative POSIX key for any path in this gate.

    Every keying decision routes through here, and adding one that does not is
    how this class recurs. Four separate constants in this branch were each
    anchored to the wrong root and each fixed alone: the declared map, the
    baseline path, the bare `target` exclusion, and the anchored exclusion. A
    review pass found one instance every round, including the rounds that fixed
    it, because a fix at one call site is not a fix of the class.

    Resolved on both sides, since git prints real paths while a caller's root
    can carry a symlinked prefix (macOS /var against /private/var). A key that
    escapes the repo root falls back to scan-relative rather than being trusted.
    """
    try:
        key = os.path.relpath(os.path.realpath(path), repo_root)  # repo-key-exempt: this IS the helper
    except ValueError:
        key = os.path.relpath(path, root)  # repo-key-exempt: this IS the helper
    if key.startswith(".."):
        key = os.path.relpath(path, root)  # repo-key-exempt: this IS the helper
    return key.replace(os.sep, "/")


def scan(root: Path):
    findings = []
    observed_by_ext = {}
    axis_dirs = {}
    probes_reached = set()
    repo_root = _repo_root_for(root)
    for dirpath, dirnames, filenames in os.walk(root):
        kept = []
        for d in dirnames:
            if d in EXCLUDE_DIR:
                continue
            if _repo_key(os.path.join(dirpath, d), root, repo_root) in EXCLUDE_ANCHORED:
                continue
            kept.append(d)
        dirnames[:] = kept
        here = _repo_key(dirpath, root, repo_root)
        for probe in REQUIRED_REACH:
            if here == probe or here.startswith(probe + "/"):
                probes_reached.add(probe)
        # Collected in the SAME walk the content scan already performs, so the
        # name check costs no second traversal of the tree.
        # The scan ROOT is judged like any other directory. Excluding it left the
        # name gate blind to the one directory a caller names explicitly:
        # `--strict cli/src/fno/adapters/providers` printed "name scan ok, 0
        # axis-named directories" about a directory whose name states an axis.
        # A receipt contradicting the walk it describes is the defect this gate
        # exists to catch, and the gate was committing it.
        # Read off the real basename rather than the repo key. When the scan
        # root IS the repo root the key is ".", which carries no name at all, so
        # keying the check made the root invisible again in exactly the case a
        # caller points the gate straight at a directory outside any repo. Not a
        # repo key and not routed through the helper: this is a NAME.
        _name = os.path.basename(os.path.abspath(dirpath))
        stated = _stated_axis(_name)
        if stated:
            axis_dirs[here if here != "." else _name] = stated
        for fn in filenames:
            ext = os.path.splitext(fn)[1]
            if ext not in SCANNABLE_EXT:
                continue
            path = os.path.join(dirpath, fn)
            rel = _repo_key(path, root, repo_root)
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
    return sorted(set(findings)), observed_by_ext, axis_dirs, probes_reached


def _name_findings(axis_dirs: dict):
    """Violations of the declared-axis rule, one string each.

    Two rules, and the first is where the teeth are:

    1. An axis-named directory absent from DECLARED_PATH_AXIS fails. Nobody
       looked at it, so nothing vouches for the name it carries.
    2. A declared directory whose declaration disagrees with the axis its name
       states fails.
    """
    out = []
    for rel, stated in sorted(axis_dirs.items()):
        declared = DECLARED_PATH_AXIS.get(rel)
        if declared == NOT_AN_AXIS:
            continue
        if declared is None:
            out.append(
                f"{rel}/: undeclared axis-named directory (name states "
                f"'{stated}'); add it to DECLARED_PATH_AXIS with the axis its "
                f"contents implement, or rename it"
            )
        elif declared != stated:
            out.append(
                f"{rel}/: directory name states '{stated}' but its declared "
                f"axis is '{declared}'"
            )
    return out


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
        found, _, _, _ = scan(d)
        if found:
            print(f"caught planted {lang} violation")
        else:
            print(f"FAILED to catch planted {lang} violation", file=sys.stderr)
            failures += 1
    d = Path(tempfile.mkdtemp(prefix="axis-clean-"))
    (d / "ok.py").write_text('provider = "anthropic"\n', encoding="utf-8")
    found, _, _, _ = scan(d)
    if found:
        print("FAILED: clean tree produced findings", file=sys.stderr)
        for f in found:
            print(f"  {f}", file=sys.stderr)
        failures += 1
    else:
        print("caught planted clean-tree (no findings) ok")

    # Name scan. The planted directory is undeclared, which is the rule that
    # catches a NEW mis-named directory on the PR that adds it.
    d = Path(tempfile.mkdtemp(prefix="axis-name-"))
    (d / "providers").mkdir()
    (d / "providers" / "x.py").write_text("x = 1\n", encoding="utf-8")
    _, _, dirs, _ = scan(d)
    if _name_findings(dirs):
        print("caught planted undeclared-directory violation")
    else:
        print("FAILED to catch planted undeclared-directory violation", file=sys.stderr)
        failures += 1

    # A declared directory whose declaration agrees with its name is silent.
    # Asserted against a real DECLARED_PATH_AXIS entry rather than a stub, so a
    # map emptied by a bad edit fails here instead of passing on a fixture.
    declared_rel, declared_axis = next(iter(DECLARED_PATH_AXIS.items()))
    d = Path(tempfile.mkdtemp(prefix="axis-declared-"))
    (d / declared_rel).mkdir(parents=True)
    (d / declared_rel / "x.py").write_text("x = 1\n", encoding="utf-8")
    _, _, dirs, _ = scan(d)
    # Asserts the POSITIVE: the expected key was actually computed, then that it
    # produced no finding. Filtering findings by a `declared_rel` prefix instead
    # was unfalsifiable - a TMPDIR inside a git repo prefixes every key, no
    # finding starts with declared_rel, and the test passes while the real
    # verdict is "undeclared violation".
    if declared_rel not in dirs:
        print(
            f"FAILED: declared '{declared_rel}' was never keyed; got {sorted(dirs)}",
            file=sys.stderr,
        )
        failures += 1
    elif _name_findings(dirs):
        print(
            f"FAILED: declared '{declared_rel}' ({declared_axis}) still flagged",
            file=sys.stderr,
        )
        for v in _name_findings(dirs):
            print(f"  {v}", file=sys.stderr)
        failures += 1
    else:
        print("caught planted declared-directory (no name findings) ok")

    # A SUBTREE scan of a real git repo must key declarations from the repo
    # root. Needs `git init`: without a repo above it, the scan root and the
    # repo root coincide and the bug cannot reproduce.
    d = Path(tempfile.mkdtemp(prefix="axis-subtree-"))
    subprocess.run(["git", "init", "-q", str(d)], capture_output=True)
    (d / declared_rel).mkdir(parents=True)
    (d / declared_rel / "x.py").write_text("x = 1\n", encoding="utf-8")
    subtree = d / Path(declared_rel).parts[0]
    _, _, dirs, _ = scan(subtree)
    if declared_rel not in dirs:
        print(
            f"FAILED: subtree scan keyed {sorted(dirs)}, not repo-relative "
            f"'{declared_rel}'",
            file=sys.stderr,
        )
        failures += 1
    elif _name_findings(dirs):
        print(
            f"FAILED: scanning the subtree {subtree.name}/ flagged a declared "
            "directory",
            file=sys.stderr,
        )
        failures += 1
    else:
        print("caught planted subtree-root scan (no name findings) ok")

    # The scan ROOT itself, which the walk used to skip. Pointing the gate at an
    # axis-named directory is the most direct question a caller can ask it, and
    # the answer was "0 axis-named directories" every time. Every specimen above
    # plants its subject BELOW the root, so none of them could see this.
    d = Path(tempfile.mkdtemp(prefix="axis-asroot-"))
    (d / "providers").mkdir()
    (d / "providers" / "x.py").write_text("x = 1\n", encoding="utf-8")
    _, _, dirs, _ = scan(d / "providers")
    root_findings = _name_findings(dirs)
    if len(root_findings) == 1 and "undeclared axis-named" in root_findings[0]:
        print("caught planted axis-named scan root ok")
    else:
        print(
            f"FAILED to judge the scan root itself (dirs={sorted(dirs)}, "
            f"findings={root_findings})",
            file=sys.stderr,
        )
        failures += 1

    # NOT_AN_AXIS silences a name that merely contains an axis word.
    d = Path(tempfile.mkdtemp(prefix="axis-notaxis-"))
    (d / "models").mkdir()
    (d / "models" / "x.py").write_text("x = 1\n", encoding="utf-8")
    _, _, dirs, _ = scan(d)
    before = len(_name_findings(dirs))
    DECLARED_PATH_AXIS["models"] = NOT_AN_AXIS
    try:
        after = len(_name_findings(dirs))
    finally:
        del DECLARED_PATH_AXIS["models"]
    # Anchored exclusion. A bare-name `target` prunes a source directory that
    # merely shares the name, which is how skills/target stayed invisible: 66
    # tracked files that neither scan ever opened. The specimen is a source tree
    # named `target` OUTSIDE the anchored build paths, and its violation must be
    # found. Multi-level on purpose: the single-level trees above cannot reach a
    # second os.walk iteration, which is how a bug in the walk survived them.
    d = Path(tempfile.mkdtemp(prefix="axis-anchored-"))
    (d / "skills" / "target" / "references").mkdir(parents=True)
    (d / "skills" / "target" / "references" / "v.py").write_text(
        'provider = "claude"\n', encoding="utf-8"
    )
    found, _, _, probes = scan(d)
    if found and "skills/target" in probes:
        print("caught planted source-dir-named-target violation")
    else:
        print(
            "FAILED to catch a violation under a source directory named target "
            f"(findings={len(found)}, probes={sorted(probes)})",
            file=sys.stderr,
        )
        failures += 1

    if before == 1 and after == 0:
        print("caught planted not-an-axis declaration ok")
    else:
        print(
            f"FAILED not-an-axis: undeclared={before} (want 1), "
            f"declared={after} (want 0)",
            file=sys.stderr,
        )
        failures += 1

    # Mismatch branch: a directory whose NAME states one axis while the map
    # DECLARES another. Untested until now, so the whole `declared != stated`
    # arm could be deleted and this suite stayed green. Reuses the tree above,
    # whose single axis-named directory is keyed `providers`.
    DECLARED_PATH_AXIS["providers"] = "model"
    try:
        mismatch = _name_findings({"providers": "provider"})
    finally:
        del DECLARED_PATH_AXIS["providers"]
    if len(mismatch) == 1 and "declared axis is 'model'" in mismatch[0]:
        print("caught planted axis mismatch ok")
    else:
        print(f"FAILED mismatch branch: {mismatch}", file=sys.stderr)
        failures += 1

    # Production wiring, end to end through the real entry point. Every specimen
    # above calls a helper directly, so deleting the block that CALLS them left
    # --self-test green while the gate reported nothing. Asserted on a positive
    # marker naming the planted directory, never on a nonzero exit: a temp root
    # exits nonzero anyway because the declared map is absent there.
    script = os.environ.get("AXIS_SELF_PATH")
    if not script:
        print("FAILED wiring: AXIS_SELF_PATH unset", file=sys.stderr)
        failures += 1
    else:
        d = Path(tempfile.mkdtemp(prefix="axis-wiring-"))
        (d / "providers").mkdir()
        (d / "providers" / "v.py").write_text("x = 1\n", encoding="utf-8")
        env = dict(os.environ)
        env.pop("AXIS_SELF_TEST", None)
        run = subprocess.run(
            ["bash", script, "--strict", str(d)],
            capture_output=True,
            text=True,
            env=env,
        )
        marker = "providers/: undeclared axis-named directory"
        if marker in run.stdout + run.stderr:
            print("caught planted directory through the real entry point ok")
        else:
            print(
                "FAILED wiring: the production name verdict never reported the "
                f"planted directory (exit {run.returncode})",
                file=sys.stderr,
            )
            failures += 1

    return 1 if failures else 0


if os.environ.get("AXIS_SELF_TEST") == "1":
    sys.exit(_self_test())

findings, observed, axis_dirs, probes_reached = scan(root_arg)

# --- Name scan (independent of the content scan and its baseline) -------------
# Runs in every mode and never contributes a baseline line: a directory naming
# the wrong axis is not a finding to be ratcheted down over time, it is a rename.
#
# Positive control first. Zero axis-named directories means the traversal broke
# (wrong root, an EXCLUDE_DIR change, a walk that never reached content), not a
# clean tree. An absence has two explanations and cannot tell them apart, so a
# zero-collection result exits 2 rather than reporting green.
#
# Scoped to a whole-repo scan: a SUBTREE root legitimately contains no
# axis-named directory, and failing there would turn the control into a false
# red of its own.
#
# The bar is the DECLARED set, not merely non-empty. Checking `not axis_dirs`
# passed on partial traversal loss: excluding `cli` or breaking the walk below
# it left `docs/harnesses` alone holding the set open while both Python
# packages went unjudged, and the receipt still printed a confident count. A
# count nothing asserts is decoration.
_whole_repo_scan = root_arg.resolve() == _repo_root_for(root_arg).resolve()

# "Declared but absent" and "declared but unreached" are two states, and putting
# them behind one exit sent every reader to the wrong place. A directory that was
# renamed or deleted without updating the map is a MAP violation the author fixes
# in the map (exit 1). A directory that exists on disk but the walk never visited
# is an INSTRUMENT failure the author fixes in the traversal (exit 2). The far
# more common cause is the first, and it was reported as the second.
_declared_missing, _declared_unreached = [], []
if _whole_repo_scan:
    for _rel in sorted(DECLARED_PATH_AXIS):
        if _rel in axis_dirs:
            continue
        if (_repo_root_for(root_arg) / _rel).is_dir():
            _declared_unreached.append(_rel)
        else:
            _declared_missing.append(_rel)

name_violations = _name_findings(axis_dirs)
name_violations += [
    f"{rel}/: declared in DECLARED_PATH_AXIS but no such directory exists; "
    "remove the entry or restore the path"
    for rel in _declared_missing
]

# Instrument failures, collected rather than raised, so the verdict below still
# prints. A control that exits before the result it guards hides the very thing
# the reader came for.
_instrument_failures = []
if _declared_unreached:
    _instrument_failures.append(
        "declared directories present on disk but never visited: "
        + ", ".join(_declared_unreached)
    )
# Same split the declared map already got. A probe path that no longer exists is
# a STALE CONSTANT the author fixes in REQUIRED_REACH, not a broken traversal,
# and this repo renames directories routinely. Reporting it as an instrument
# failure sends the reader into os.walk when the fix is three hundred lines away.
_probe_stale, _probe_missed = [], []
if _whole_repo_scan:
    for _probe in REQUIRED_REACH:
        if _probe in probes_reached:
            continue
        if (_repo_root_for(root_arg) / _probe).is_dir():
            _probe_missed.append(_probe)
        else:
            _probe_stale.append(_probe)
name_violations += [
    f"{probe}/: REQUIRED_REACH names a path that does not exist; update the "
    "probe to a live directory inside an excluded region"
    for probe in sorted(_probe_stale)
]
if _probe_missed:
    _instrument_failures.append(
        "adversarial probe paths present on disk but never visited: "
        + ", ".join(sorted(_probe_missed))
    )
if _whole_repo_scan and not axis_dirs:
    _instrument_failures.append(
        "no axis-named directories collected anywhere under the root"
    )

# Printed on EVERY run, pass or fail. A green here means less than a reader will
# assume, and the blind spot this guard was written for is exactly a green that
# was read as covering more than it did.
_declared_count = sum(1 for rel in axis_dirs if rel in DECLARED_PATH_AXIS)
_noun = "directory" if len(axis_dirs) == 1 else "directories"
print(
    f"check-axis-vocabulary: name scan "
    f"{'ok' if not name_violations else 'FAILED'} "
    f"({len(axis_dirs)} axis-named {_noun}, {_declared_count} declared)\n"
    "  scope: judges DIRECTORY names against DECLARED_PATH_AXIS, a human\n"
    "  declaration. It does NOT open a directory to verify that declaration is\n"
    "  true, so a wrong declaration passes green. It does not judge file names\n"
    "  or symbol names; those are the content scan's subject and live in the\n"
    "  baseline.\n"
    f"  not scanned: {', '.join(sorted(EXCLUDE_ANCHORED))}, and any directory\n"
    f"  named {', '.join(sorted(EXCLUDE_DIR))}. Anything under those paths is\n"
    "  judged by neither scan.",
    file=sys.stderr if name_violations or _instrument_failures else sys.stdout,
)

if _instrument_failures:
    print(
        "check-axis-vocabulary: name scan positive control failed:",
        file=sys.stderr,
    )
    for f in _instrument_failures:
        print(f"  {f}", file=sys.stderr)
    print(
        "The traversal did not cover the tree, so no verdict above is "
        "trustworthy. This is an instrument failure, not a finding.",
        file=sys.stderr,
    )
    sys.exit(2)

if name_violations:
    print("check-axis-vocabulary: directory-name violations:", file=sys.stderr)
    for v in name_violations:
        print(f"  {v}", file=sys.stderr)
    print(
        "A directory named for one axis may not hold another axis's "
        "implementation; see docs/architecture/four-axis-vocabulary.md",
        file=sys.stderr,
    )
    sys.exit(1)


# An allowlist entry MUST carry a one-line justification: `allowlist: <path>:<line> <why>`.
# This single regex is the authority for BOTH suppression (a bare line parses to no
# key, so it cannot suppress) and baseline format validation (a bare line is
# malformed -> exit 2). A bare `allowlist: file:line` with no justification therefore
# fails in both modes - it can never silently absorb a finding.
_ALLOWLIST_RE = re.compile(r"^allowlist:\s+(.+?):(\d+)\s+(\S.*)$")
# The `:<line>:` between an entry's path and its binding. Stripped to compare
# entries by identity rather than by position; see _identity below.
_LINE_IN_ENTRY = re.compile(r"^(.+?):\d+: ")


def _allowlist_keys(path: Path):
    """file:line keys allowlisted out of the violation set (correct ambiguous
    sites, time-boxed compat windows), with a required justification per line."""
    keys = set()
    if path.exists():
        for line in path.read_text(encoding="utf-8").splitlines():
            s = line.strip()
            m = _ALLOWLIST_RE.match(s)
            if m:
                keys.add(f"{m.group(1)}:{m.group(2)}")
    return keys


def _finding_key(f: str) -> str:
    m = re.match(r"^(.+?:\d+):", f)
    return m.group(1) if m else f


# Suppress allowlisted sites before any reporting or baseline diff. An allowlist
# entry with no justification cannot be parsed (the regex requires text after the
# file:line), so a bare "smuggle it past review" line fails to suppress.
allowlisted = _allowlist_keys(baseline_path)
findings = [f for f in findings if _finding_key(f) not in allowlisted]

# Positive control (AC3): the scan must reach real content in BOTH Python and
# Rust, the two languages carrying the most axis-named bindings. A scan that
# observes no provider/harness/model tokens in either has not reached content and
# must fail rather than report a clean tree. This is the liveness check that makes
# a zero-finding result trustworthy through a cutover that removes findings.
#
# Scoped to a whole-repo scan, for the reason the name control already is: a
# SUBTREE legitimately contains no Python or Rust at all, so demanding both
# turned every such scan into a false red. Measured before this change, on
# origin/main as well: `--baseline docs` and `--baseline crates` each exited 2
# naming a language the subtree does not contain. A control that fires where it
# cannot pass is not strictness, it is noise that trains readers to ignore it.
#
# The subtree case does not get a weaker control pretending to be the same one.
# It gets no control and says so, because the honest scope line is the point.
missing = [lang for lang in (".py", ".rs") if observed.get(lang, 0) == 0]
if missing and _whole_repo_scan:
    print(
        "check-axis-vocabulary: positive control failed "
        f"(no axis-named bindings observed in {', '.join(missing)}); "
        "scan did not reach content",
        file=sys.stderr,
    )
    sys.exit(2)

_counts = (
    f"(observed axis bindings: py={observed.get('.py', 0)}, "
    f"rs={observed.get('.rs', 0)}, sh={observed.get('.sh', 0) + observed.get('.bash', 0)}, "
    f"md={observed.get('.md', 0)})"
)
if _whole_repo_scan:
    print(f"check-axis-vocabulary: positive control ok {_counts}")
else:
    print(
        f"check-axis-vocabulary: subtree scan, no content positive control {_counts}"
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

# Split the checked-in baseline into violation entries and allowlist entries.
# Allowlist lines are metadata (consumed by the suppression pass), not findings
# to diff - but they ARE validated: each must carry a path, a line, and a
# one-line justification (_ALLOWLIST_RE). A bare `allowlist: file:line` with no
# justification is exactly how a finding would be smuggled past review, so a
# missing justification or a malformed entry fails loudly here rather than
# silently doing nothing.
non_comment = [s.strip() for s in raw if s.strip() and not s.lstrip().startswith("#")]
allowlist_lines = [s for s in non_comment if s.startswith("allowlist:")]
baseline = [s for s in non_comment if not s.startswith("allowlist:")]
finding_pat = re.compile(
    r"^.+?:\d+: .+?=\"(?:claude|codex|agy|opencode|anthropic|openai|zai|deepseek|google)\" \(.+\)$"
)
bad_allowlist = [s for s in allowlist_lines if not _ALLOWLIST_RE.match(s)]
malformed = [e for e in baseline if not finding_pat.match(e)] + bad_allowlist
if malformed or len(baseline) != len(set(baseline)) or len(allowlist_lines) != len(set(allowlist_lines)):
    print("check-axis-vocabulary: baseline is malformed or contains duplicates", file=sys.stderr)
    for e in malformed:
        print(f"  {e}", file=sys.stderr)
    sys.exit(2)

current = list(findings)


def _identity(entry: str) -> str:
    """An entry's identity WITHOUT its line number.

    The baseline still records `path:line:` because a human chasing a finding
    needs somewhere to look. Comparing on it was the bug: an edit anywhere
    ABOVE a baselined violation renumbers it, and a pure renumbering read as
    one resolved entry plus one new violation. That turned main red twice in
    four hours on days nobody touched the vocabulary at all - once from a
    two-line config refactor, once from a merge that shifted 22 daemon.rs
    entries at a stroke. The violation is the NAME-holds-the-wrong-AXIS
    binding, not the row it happens to sit on, so identity drops the line.

    Counted rather than set-compared, so N identical bindings in one file
    still require N baseline entries: adding a sixth `legacy_provider="codex"`
    to a file that already has five is a new violation and must fail.
    """
    return _LINE_IN_ENTRY.sub(r"\1: ", entry, count=1)


# A SUBTREE scan produces findings only from that subtree, so diffing it against
# the whole-repo baseline reports every violation outside the subtree as resolved.
# Measured: `--baseline cli` exited 1 and blamed unrelated `crates/...` entries.
# Both sides are narrowed to the subtree so the comparison describes one region.
if not _whole_repo_scan:
    _subtree = _repo_key(root_arg, root_arg, _repo_root_for(root_arg)) + "/"
    baseline = [e for e in baseline if e.startswith(_subtree)]
    current = [e for e in current if e.startswith(_subtree)]

cur_counts = Counter(_identity(e) for e in current)
base_counts = Counter(_identity(e) for e in baseline)
new_or_changed = sorted((cur_counts - base_counts).elements())
resolved = sorted((base_counts - cur_counts).elements())
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
    print(
        "Line-pinned-baseline note: the baseline keys violations by file:line, so "
        "ANY PR that inserts lines above a baselined violation inherits a drift "
        "here even when it changes no axis binding - the violation moved, it did "
        "not change. That is renumbering, not a new finding; regenerate the "
        "baseline in the same PR (--write-baseline, preserving allowlist entries "
        "by hand if regeneration drops them). The drift is invisible until CI "
        "runs, so a merge that shifts baselined lines turns main red on the NEXT "
        "PR, not its own - keep this baseline regenerated alongside line-moving "
        "changes, not deferred.",
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
