#!/usr/bin/env python3
"""Report external callers of every fno verb leaf.

A diagnostic (not a CI gate): for each leaf in ``scripts/ci/verb-baseline.txt``,
count how many times it is named as ``fno``/``fno-py``/``fno-agents <leaf>`` in
the corpus a contributor reads (everything except ``cli/src``, where verbs are
defined). A leaf that scores zero is a cull candidate, not a verdict - the
operator triages the zero set.

Two false-positive classes that an uncorrected ``fno <leaf>`` sweep misses are
fixed here, both monotone-conservative (each can only ADD references, so the
corrected zero set is a subset of the uncorrected one):

  (a) Pipe-joined help tables. ``using-fno`` writes ``fno agents spawn|ask|...``;
      only ``spawn`` follows ``fno agents`` as a discrete token, so a literal
      scan credits only ``spawn``. A verb-path token containing ``|`` with no
      surrounding whitespace fans out to one candidate per alternative.
      Whitespace around the pipe is a markdown
      table cell and is left alone - the cell is split into separate tokens and
      the bare ``|`` is not a verb token.
  (b) Binary-form invocation. ``fno-agents loop-check`` credits the
      ``agents loop-check`` namespace; ``fno-py <leaf>`` credits the plain one.

Positive controls are ENFORCED, not printed: a control that does not fire means
the sweep is broken, and no candidate list is emitted. A zero-reference result
without a firing control is a claim, not a measurement.

``--zero`` answers "does anything a contributor reads name this leaf", which is
NOT enough to delete on. ``--dead`` answers the deletion question: it adds a
sweep of ``cli/src`` (call sites separated from definition sites by the AST) and
buckets every reference, so a leaf named only by its own test still counts as
dead while a leaf invoked by an argv built in ``cli/src`` does not. Measured
2026-08-12: 87 leaves are named nowhere a contributor reads, and 34 of those are
invoked from ``cli/src``. Deleting on the ``--zero`` answer would have broken 34
live callers with no failing test to say so.

``--dynamic`` is the other half of that safety property. A verb reached through
a constructed command - a shell array, an argv assembled from a variable, a
wrapper function that renames the binary - matches no literal sweep and would be
counted as dead. Those sites are reported rather than resolved, and shell
wrappers that forward ``"$@"`` to an fno binary are discovered and treated as
binaries so their callers are not invisible.

Usage:
  python3 scripts/diagnostics/verb-callers.py            # full table, all leaves
  python3 scripts/diagnostics/verb-callers.py --zero     # only zero-ref leaves
  python3 scripts/diagnostics/verb-callers.py --dead     # deletion candidates
  python3 scripts/diagnostics/verb-callers.py --dynamic  # unresolvable call sites
  python3 scripts/diagnostics/verb-callers.py --summary  # counts + delta + clusters
  python3 scripts/diagnostics/verb-callers.py --self-check  # controls + parity, exit code

The sweep itself is stdlib-only. ``file:line`` and help columns require the fno
environment (click/typer); run under ``uv run --project cli python ...`` for the
enriched table, or accept blank columns under a bare interpreter.
"""
from __future__ import annotations

import argparse
import ast
import os
import re
import string
import subprocess
import sys
from collections import Counter
from itertools import product
from pathlib import Path
from typing import Optional as _Optional

Optional_str = _Optional[str]

# --- corpus -----------------------------------------------------------------

# Everything a contributor reads, EXCLUDING cli/src (where verbs are defined).
CORPUS_DIRS = [
    "skills", "docs", "scripts", "hooks", "agents",
    "commands", "crates", "cli/tests", "tests",
]
CORPUS_FILES = ["AGENTS.md", "README.md"]

# Buckets, for the deletion question. "Is this leaf named anywhere" is the wrong
# question: a leaf named ONLY by its own test is deletable (the test goes with
# it), while a leaf named only by a hook is load-bearing. So every reference is
# attributed to the bucket that decides what deleting it costs.
#
#   user     a human or agent reads it and may type it        -> deleting breaks a documented path
#   runtime  a script, hook, or the Rust front executes it    -> deleting breaks a caller
#   internal cli/src builds an argv for it                    -> deleting breaks a caller
#   tests    only the test suite names it                     -> deletable, test goes too
#
# DEAD == user + runtime + internal all zero. Tests are excluded from that sum
# on purpose; they are the one bucket a deletion is allowed to take with it.
BUCKETS: dict[str, tuple[str, ...]] = {
    "user": ("skills", "docs", "agents", "commands", "AGENTS.md", "README.md"),
    "runtime": ("scripts", "hooks"),
    "tests": ("cli/tests", "tests"),
}
CRATES_ROOT = "crates"
INTERNAL_ROOT = "cli/src"

# Skip dirs reachable from the corpus that hold no contributor-readable text:
# bytecode caches and venvs. Root-level state dirs (.claude, .fno, internal, ...)
# are intentionally NOT here: the walk descends only the named corpus subdirs
# (skills/, docs/, ...), never the repo root, so those dirs are unreachable, and
# a literal ".claude" here would trip scripts/ci/check-placement-rule.sh (it
# guards .claude path construction). "target" is handled separately by the
# `under_skills` conditional in iter_corpus, so skills/target/ (a real skill
# bundle) is walked while crates/target/ (Rust build output) is skipped. Putting
# "target" in this set would unconditionally remove it and silently swallow
# skills/target/, the exact rg-glob pitfall this tool exists to avoid.
SKIP_DIRS = {
    "node_modules", "__pycache__", "venv", ".venv",
}

# A verb-path token: lowercase letters, digits, hyphens, and (for fan-out) pipes.
# Uppercase stops a path; flags and brackets are not verb tokens.
_VERB_RE = re.compile(r"[a-z][a-z0-9-|]*")

# Outer punctuation to strip so `` `fno-agents` `` and ``(fno-py)`` match; hyphen
# and pipe are internal structure and stay.
_STRIP = "".join(c for c in string.punctuation if c not in "-|")

PREFIXES = {"fno", "fno-py", "fno-agents"}

# Positive controls: a sweep where these do not fire is broken, not changed.
CONTROLS = {"agents spawn": 100, "mail send": 100, "target init": 25, "backlog next": 25}

# One control set per SIGNAL, not one per tool. The AST walk and the cli/src
# text sweep answer the same question by different means, so a single shared
# control would go green on either one alone and a silently broken AST walk
# would read as "no internal callers" - the exact absence-with-two-explanations
# shape that turns a correct sweep into a wrong deletion. Each floor below is a
# verb whose argv is built as a list literal in cli/src, verified by hand:
#   agents spawn  cli/src/fno/evals/runner.py, cli/src/fno/observer/cli.py
#   mail send     cli/src/fno/events/cli.py
#   backlog done  cli/src/fno/graph/cli.py
AST_CONTROLS = {"agents spawn": 2, "mail send": 1, "backlog done": 1}
INTERNAL_TEXT_CONTROLS = {"agents spawn": 20, "target init": 20, "backlog update": 10}

# Verbs reachable ONLY through shell command substitution, where the binary
# name is glued to a variable assignment and is not a bare token:
#   hooks/helpers/init-target-state.sh   _OWNED_OUT="$(fno target resolve-owned-identity
#   skills/pr/SKILL.md                   policy_json="$(candidate_fno pr evidence-required
# Both scored zero and sat in the dead set until an independent whole-repo walk
# contradicted it. They are controls now: a tokenizer change that loses this
# shape emits no candidate list instead of a wrong one.
#   scripts/lib/eval-sweep-throttle.sh   status="$("$fno_cmd" loops status
# is the third shape: the binary lives in a VARIABLE, resolved at runtime.
SUBSTITUTION_CONTROLS = {
    "target": 1,
    "pr": 1,
    "loops": 1,
}

# The FOURTH shape: a Rust argv ARRAY. `Command::new(fno_bin()).args(["claim",
# "sweep", "--json"])` puts no binary token beside the verb, so the tokenizer
# above credits nothing and a live verb scores zero.
#
#   crates/fno/src/needs_overlay.rs      .args(["needs", ...])   -> `agents needs`
#   crates/fno-agents/src/finalize.rs    .args(["pr", "base-lineage-check", ...])
#
# Key on the ARRAY, never on the Command expression. A first attempt keyed on
# `Command::new(<ident>_bin())` and missed needs_overlay.rs outright, because the
# real call reads `tokio::process::Command::new(crate::digest_overlay::fno_agents_bin())`
# and the qualifier's `::` and parens defeat that pattern.
_RUST_ARGS_RE = re.compile(r"\.args\(\s*&?\s*\[([^\]]*)\]", re.S)
_RUST_STR_RE = re.compile(r'"((?:[^"\\]|\\.)*)"')
_RUST_CMD_LITERAL_RE = re.compile(r'Command::new\(\s*"([^"]+)"')

# An array is skipped ONLY when the nearest preceding `Command::new` names one
# of THESE literals. The set is enumerated on purpose and must never be inverted
# into "skip any literal that is not fno": that rule kills a live verb the day
# someone adds a wrapper under a new name. This file's own rule decides the
# direction - a false positive keeps a verb alive, a false negative deletes a
# live one - so over-collection is the safe error and this list stays long.
FOREIGN_BINARIES = {
    "bash", "cargo", "echo", "env", "false", "gh", "git", "jq", "node", "npm",
    "open", "printf", "python", "python3", "sh", "sleep", "tmux", "true", "uv",
}

# One control per SHAPE, not one per site. Four shapes, each verified as a real
# baselined leaf at a real call site:
#   agents needs           the fno-agents prefix case (array is ["needs"])
#   notify                 single-token leaf
#   plan                   collapsed dispatcher credited from `plan fidelity`
#   pr                     collapsed dispatcher credited from the hyphenated
#                          `pr base-lineage-check` action, separately from the
#                          `gh pr view` arrays next to it
RUST_ARGV_CONTROLS = {
    "agents needs": 1,
    "notify": 1,
    "plan": 1,
    "pr": 1,
}

# A Rust integration-test reference must prove the test sweep ran, never keep a
# runtime capability alive. `test` is named by a crates/**/tests/** fixture in
# the real corpus, so this marker fails if those paths fall back into runtime.
CRATES_TEST_CONTROLS = {"test": 1}

ORIGINAL_ZERO_BOUND = 92  # the uncorrected sweep's zero count; the delta baseline


def repo_root() -> Path:
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], stderr=subprocess.DEVNULL
        )
        return Path(out.decode().strip())
    except Exception:
        return Path(__file__).resolve().parents[2]


def load_leaves(root: Path) -> list[str]:
    """The leaf universe from the baseline, ``!--flag`` tokens stripped."""
    baseline = root / "scripts" / "ci" / "verb-baseline.txt"
    leaves = []
    for line in baseline.read_text().splitlines():
        line = line.rstrip("\n")
        if not line or line[:1] == "#":
            continue
        leaves.append(line.split(" !")[0].strip())
    return sorted(set(leaves))


def load_curriculum(path: Path, leaves: set[str]) -> tuple[set[str], list[str]]:
    """Taught verbs from a file, one per line, ``#`` comments (full-line or trailing).

    Returns ``(taught ∩ leaves, sorted unknown)`` so a curriculum typo or a verb
    that was cut surfaces loudly instead of silently shrinking the complement.
    """
    raw: list[str] = []
    for line in path.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            raw.append(line)
    typing_surface: set[str] = set()
    allocation = path.with_name("verb-collapse-map.tsv")
    if allocation.is_file():
        for row in allocation.read_text().splitlines()[1:]:
            current_leaf, *_ = row.split("\t")
            typing_surface.add(current_leaf)

    taught: set[str] = set()
    unknown: list[str] = []
    for entry in sorted(set(raw)):
        if entry in leaves:
            taught.add(entry)
            continue
        if entry in typing_surface:
            tokens = entry.split()
            dispatcher = next(
                (
                    " ".join(tokens[:size])
                    for size in range(len(tokens) - 1, 0, -1)
                    if " ".join(tokens[:size]) in leaves
                ),
                None,
            )
            if dispatcher is not None:
                taught.add(dispatcher)
                continue
        unknown.append(entry)
    return taught, unknown


def iter_paths(root: Path, entries):
    """Walk the named repo-relative entries, yielding every file."""
    for entry in entries:
        base = root / entry
        if base.is_file():
            yield base
            continue
        if not base.is_dir():
            continue
        # `target` is a build-output dir everywhere except under skills/, where
        # skills/target is a real skill bundle.
        under_skills = entry == "skills"
        for dp, dn, fn in os.walk(base):
            dn[:] = [
                d for d in dn
                if d not in SKIP_DIRS and (under_skills or d != "target")
            ]
            for name in fn:
                yield Path(dp) / name


def iter_corpus(root: Path):
    yield from iter_paths(root, CORPUS_DIRS + CORPUS_FILES)


def iter_cull_corpus(root: Path):
    """Contributor corpus with Rust integration tests removed from cull counts."""
    for path in iter_corpus(root):
        if not _is_crates_test_path(root, path):
            yield path


def _is_crates_test_path(root: Path, path: Path) -> bool:
    """Whether a path below crates/ is in a Rust integration-test tree."""
    try:
        rel = path.relative_to(root / CRATES_ROOT)
    except ValueError:
        return False
    return "tests" in rel.parts


def iter_crates_paths(root: Path, *, tests: bool):
    """Split crates source between runtime and tests without double-crediting."""
    for path in iter_paths(root, (CRATES_ROOT,)):
        if _is_crates_test_path(root, path) is tests:
            yield path


def _clean(tok: str) -> str:
    return tok.strip(_STRIP)


# Shell glues the binary name to whatever precedes it: `x="$(fno` and
# `policy_json="$(candidate_fno` are single whitespace-delimited tokens, and
# stripping OUTER punctuation leaves them intact because the leading characters
# are letters. Both were read as "not a binary", so every verb reached only
# through command substitution scored zero.
#
# Two live verbs were in that hole - `fno target resolve-owned-identity` (a
# hook) and `fno pr evidence-required` (a skill) - and both sat in the dead set
# until an independent whole-repo walk contradicted it. Splitting on shell
# punctuation and keeping the last segment recovers them.
_SHELL_GLUE = re.compile(r"[^\w./-]+")

# A shell VARIABLE holding the binary: `"$fno_cmd" loops status` in
# scripts/lib/eval-sweep-throttle.sh. The value is resolved at runtime (there,
# by a function), so no static reading can prove what it holds; the name is the
# only signal available. Any `$...` reference whose identifier contains "fno" is
# treated as the binary, which is deliberately loose - a false positive keeps a
# verb alive, a false negative deletes a live one.
_SHELL_VAR_RE = re.compile(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)")


def _binary_key(tok: str) -> str:
    """The binary name a token ends in, ignoring shell glue and any path."""
    if "$" in tok:
        for name in _SHELL_VAR_RE.findall(tok):
            if "fno" in name.lower():
                return "fno"
    tail = _SHELL_GLUE.split(tok)[-1] if tok else tok
    return tail.rsplit("/", 1)[-1]


def _verb_token(tok: str) -> str | None:
    # Markdown table cells escape the alternation pipe as `\|`; unescape it so a
    # `spawn\|ask\|peek` token fans out instead of being rejected at the backslash.
    c = _clean(tok).replace("\\|", "|")
    return c if _VERB_RE.fullmatch(c) else None


def _fan(path: list[str]) -> list[tuple[str, ...]]:
    """Expand pipe-joined tokens into one tuple per alternative combination."""
    alts = [tok.split("|") for tok in path]
    return list(product(*alts))


def sweep(
    root: Path,
    leaves: set[str],
    *,
    binary_form: bool,
    pipe_fan: bool,
    paths=None,
    extra_prefixes=(),
) -> Counter:
    """Count references. Returns a Counter keyed by leaf path."""
    counts: Counter = Counter()
    prefixes = set(PREFIXES if binary_form else {"fno", "fno-py"}) | set(extra_prefixes)
    for path in (iter_corpus(root) if paths is None else paths):
        try:
            text = path.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        tokens = text.split()
        for i, tok in enumerate(tokens):
            key = _clean(tok)
            if key not in prefixes:
                key = _binary_key(tok)
                if key not in prefixes:
                    continue
            start = ["agents"] if key == "fno-agents" else []
            path_tokens = list(start)
            j = i + 1
            while j < len(tokens) and len(path_tokens) < 3:
                vt = _verb_token(tokens[j])
                if vt is None:
                    break
                path_tokens.append(vt)
                j += 1
            if not path_tokens:
                continue
            combos = _fan(path_tokens) if pipe_fan else [tuple(path_tokens)]
            for combo in combos:
                _credit_longest(combo, leaves, counts)
    return counts


def _credit_longest(combo: tuple[str, ...], leaves: set[str], counts: Counter) -> None:
    """Credit the longest prefix of ``combo`` that is a real leaf (one credit)."""
    for n in range(min(3, len(combo)), 0, -1):
        cand = " ".join(combo[:n])
        if cand in leaves:
            counts[cand] += 1
            return


# --- internal sweep: cli/src, call sites separated from definition sites -----
#
# The external sweep excludes ``cli/src`` because that is where verbs are
# DEFINED, and a definition would credit every leaf with a reference to itself.
# That exclusion makes the zero set mean "nothing a contributor reads names
# this" and nothing stronger - which is not enough to delete on. A leaf invoked
# by an argv built inside ``cli/src`` scores zero externally and is very much
# alive.
#
# The separation is free, and it is worth stating why rather than trusting it:
# a DEFINITION names only the leaf's own last token (``@capture_cli.command(
# "promote")``), while a CALL must spell the whole path (``[*fno_py_cmd(),
# "backlog", "capture", "promote"]``). Matching only on multi-token paths that
# resolve against the baseline therefore cannot see a definition at all. The
# probe is one-directional and that is deliberate.


def _const_str(node) -> Optional_str:
    return node.value if isinstance(node, ast.Constant) and isinstance(node.value, str) else None


def _leading_consts(elts) -> list[str]:
    """The run of leading string constants in a list literal.

    A leading non-constant is SKIPPED rather than terminating the scan, because
    the canonical self-shellout is ``[*fno_py_cmd(), "backlog", "get", node]``:
    the binary arrives as a starred call and the verb path follows it. Once a
    constant run has begun, a non-constant ends it (that is an argument, not
    more verb path).
    """
    out: list[str] = []
    for e in elts:
        s = _const_str(e)
        if s is None:
            if out:
                break
            continue
        # A path-ish or flag-ish constant is not a verb token.
        if s.startswith("-") or "/" in s:
            if out:
                break
            continue
        # The binary name is not part of the verb path. Written literally
        # (``["fno", "mail", "send", ...]``) it must be dropped, or the path
        # resolves as ``fno mail send`` and matches no leaf - which reads as
        # "no internal caller" for a verb that plainly has one.
        if not out and s in PREFIXES:
            continue
        out.append(s)
        if len(out) >= 3:
            break
    return out


def _dynamic_after_binary(elts) -> bool:
    """True when a binary-prefixed argv reaches a non-constant in verb position.

    ``[*fno_py_cmd(), group, "get"]`` can invoke ANY verb under ``group``, so the
    site cannot be resolved statically and must not be read as evidence that the
    verbs it could reach are dead. These sites are reported, never silently
    dropped: an unresolvable call site is the one thing that can turn a correct
    literal sweep into a wrong deletion.
    """
    seen_binary = False
    for e in elts[:4]:
        s = _const_str(e)
        if s is not None:
            if s.rsplit("/", 1)[-1] in PREFIXES:
                seen_binary = True
                continue
            if seen_binary:
                return False  # verb position is a constant; resolvable
            continue
        if not seen_binary:
            # Only a call to a NAMED argv-prefix helper counts as the binary.
            # Treating any leading call as one flagged 296 sites, nearly all of
            # them ordinary lists that never invoke fno at all - a report nobody
            # reads is the same as no report.
            if isinstance(e, ast.Starred) and _is_argv_prefix_call(e.value):
                seen_binary = True
            continue
        return True
    return False


# Helpers whose return value IS an fno argv prefix, so `[*helper(), "pr", ...]`
# is an invocation. Found by searching cli/src for functions returning a bare
# binary list; ``fno_py_cmd`` is the only one today, and a new one that is not
# listed here degrades to "not flagged as dynamic", never to a wrong deletion
# (the AST and text sweeps still credit its constant verb tokens).
ARGV_PREFIX_HELPERS = {"fno_py_cmd"}


def _is_argv_prefix_call(node) -> bool:
    if not isinstance(node, ast.Call):
        return False
    fn = node.func
    name = getattr(fn, "id", None) or getattr(fn, "attr", None)
    return name in ARGV_PREFIX_HELPERS


def sweep_internal(root: Path, leaves: set[str]) -> tuple[Counter, Counter, list[str]]:
    """Count ``cli/src`` call sites per leaf, and collect unresolvable ones.

    Two signals, both conservative (they may only ADD references, so a leaf can
    never be pushed INTO the dead set by a miss here):

      1. AST: a list/tuple literal whose leading string constants resolve to a
         leaf path. Catches ``subprocess.run([*fno_py_cmd(), "pr", "merge", n])``
         and its bare-``"fno"`` variants.
      2. Text: the same ``fno <leaf>`` token sweep the external corpus gets,
         which catches a shellout written as one string, a help line, or an
         error message that teaches the verb.

    Returns ``(ast_counts, text_counts, dynamic_sites)`` SEPARATELY rather than
    pre-summed, because each signal carries its own enforced control. A merged
    counter would let a silently broken AST walk hide behind a working text
    sweep - an absence with two explanations, which is the failure mode the
    pitfalls corpus names.
    """
    ast_counts: Counter = Counter()
    dynamic: list[str] = []
    py_files = [p for p in iter_paths(root, [INTERNAL_ROOT]) if p.suffix == ".py"]
    for p in py_files:
        try:
            src = p.read_text(encoding="utf-8", errors="ignore")
            tree = ast.parse(src)
        except (OSError, SyntaxError):
            continue
        rel = str(p.relative_to(root))
        for node in ast.walk(tree):
            if not isinstance(node, (ast.List, ast.Tuple)) or not node.elts:
                continue
            consts = _leading_consts(node.elts)
            if consts:
                _credit_longest(tuple(consts), leaves, ast_counts)
            if _dynamic_after_binary(node.elts):
                dynamic.append(f"{rel}:{node.lineno}")
    text_counts = sweep(root, leaves, binary_form=True, pipe_fan=True, paths=py_files)
    return ast_counts, text_counts, sorted(set(dynamic))


# A shell invocation whose verb path comes from an array or a variable:
# ``fno "${cmd[@]}"``, ``fno $verb``, ``fno-py "$1"``. A literal token sweep
# cannot see the verb, so each one is a hole in the dead set exactly the size of
# what it can reach. Reported, never resolved: resolving would mean interpreting
# shell, and a wrong resolution here silently authorises a deletion.
_SHELL_DYNAMIC_RE = re.compile(r"\bfno(?:-py|-agents)?\s+\"?\$[\{(]?[A-Za-z_@*]")

# A shell function that forwards ``"$@"`` to an fno binary RENAMES it, and the
# rename is the hole: ``candidate_fno backlog get`` names a real leaf that no
# sweep for ``fno backlog get`` can see. So the wrapper's own name is harvested
# and treated as a binary prefix, which is why this is a discovery pass rather
# than a hardcoded alias list - a new wrapper is picked up without an edit here.
_SHELL_WRAPPER_RE = re.compile(
    r"^\s*(?:function\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\(\)\s*\{", re.M
)


def discover_shell_wrappers(root: Path) -> set[str]:
    """Names of shell functions whose body forwards ``"$@"`` to an fno binary."""
    found: set[str] = set()
    for p in iter_corpus(root):
        if p.suffix not in (".sh", ".bash", ".zsh"):
            continue
        try:
            text = p.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        for m in _SHELL_WRAPPER_RE.finditer(text):
            # The body is everything to the next line that closes at column 0;
            # a shallow read is enough to spot the forward, and over-matching
            # only ever ADDS a prefix (conservative in the safe direction).
            body = text[m.end(): m.end() + 800]
            body = body.split("\n}", 1)[0]
            if re.search(r"\bfno(?:-py|-agents)?\b[^\n]*\"\$@\"", body) or re.search(
                r"-m\s+fno\.cli[^\n]*\"\$@\"", body
            ):
                found.add(m.group(1))
    return found


def sweep_shell_dynamic(root: Path) -> list[str]:
    sites: list[str] = []
    for p in iter_corpus(root):
        if p.suffix not in (".sh", ".bash", ".zsh") and "scripts" not in p.parts:
            continue
        try:
            text = p.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        for n, line in enumerate(text.splitlines(), 1):
            if _SHELL_DYNAMIC_RE.search(line):
                sites.append(f"{p.relative_to(root)}:{n}")
    return sorted(set(sites))


def _foreign_command(text: str, pos: int) -> bool:
    """True when the nearest preceding ``Command::new`` names a foreign binary.

    Only a STRING LITERAL counts. Anything computed (``fno_bin()``, a variable,
    a qualified path) is credited, because the array is the signal and guessing
    at a non-literal is how a live verb gets deleted.

    The lookback must not cross a statement boundary. A builder-style command
    (``let mut c = Command::new(fno_bin()); ... c.args(["backlog", "done"]);``)
    can have an unrelated one-line ``Command::new("git")…`` between the two, and
    charging the fno array to that git call is the ONE direction this file
    forbids - it drops a live caller and authorises a deletion. A ``;`` between
    the two ends the chain, so the array belongs to something else.
    """
    window = text[max(0, pos - 400):pos]
    idx = window.rfind("Command::new(")
    if idx == -1 or ";" in window[idx:]:
        return False
    m = _RUST_CMD_LITERAL_RE.match(window, idx)
    return bool(m) and m.group(1).rsplit("/", 1)[-1] in FOREIGN_BINARIES


def sweep_rust_argv(
    root: Path, leaves: set[str], *, tests: bool | None = None,
) -> Counter:
    """Count leaves named by a Rust argv array under ``crates/``."""
    counts: Counter = Counter()
    crates = root / "crates"
    if not crates.is_dir():
        return counts
    for dp, dn, fn in os.walk(crates):
        dn[:] = [d for d in dn if d not in SKIP_DIRS and d != "target"]
        for name in fn:
            if not name.endswith(".rs"):
                continue
            path = Path(dp) / name
            if tests is not None and _is_crates_test_path(root, path) is not tests:
                continue
            try:
                text = path.read_text(encoding="utf-8", errors="ignore")
            except OSError:
                continue
            for m in _RUST_ARGS_RE.finditer(text):
                if _foreign_command(text, m.start()):
                    continue
                combo: list[str] = []
                for tok in _RUST_STR_RE.findall(m.group(1)):
                    if not _VERB_RE.fullmatch(tok) or len(combo) == 3:
                        break
                    combo.append(tok)
                if not combo:
                    continue
                before = sum(counts.values())
                _credit_longest(tuple(combo), leaves, counts)
                if sum(counts.values()) == before:
                    # An `fno-agents` argv omits the `agents` its leaf carries:
                    # the array reads ["needs"] and the leaf is `agents needs`.
                    _credit_longest(("agents", *combo[:2]), leaves, counts)
    return counts


def sweep_buckets(root: Path, leaves: set[str], extra_prefixes=()) -> dict[str, Counter]:
    """Per-bucket reference counts, so a deletion knows what it costs."""
    out: dict[str, Counter] = {}
    for name, entries in BUCKETS.items():
        out[name] = sweep(
            root, leaves, binary_form=True, pipe_fan=True,
            paths=iter_paths(root, entries), extra_prefixes=extra_prefixes,
        )
    out["runtime"].update(sweep(
        root, leaves, binary_form=True, pipe_fan=True,
        paths=iter_crates_paths(root, tests=False), extra_prefixes=extra_prefixes,
    ))
    out["tests"].update(sweep(
        root, leaves, binary_form=True, pipe_fan=True,
        paths=iter_crates_paths(root, tests=True), extra_prefixes=extra_prefixes,
    ))
    return out


def load_enrichment(root: Path) -> dict[str, tuple[str, str]]:
    """Best-effort ``{leaf: (file:line, help)}`` from the live registry.

    Returns ``{}`` when the fno environment (click/typer) is unavailable, so the
    sweep and its controls still run under a bare interpreter.
    """
    try:
        import inspect

        from fno.lint_verb_ratchet import VerbRatchetError, iter_python_leaves
    except Exception:
        return {}
    table: dict[str, tuple[str, str]] = {}
    # A refusal is NOT the same as "no fno environment". The enumerator refuses
    # when the imported package is not this checkout's source, and swallowing
    # that into an empty table would report an unenriched sweep as if the
    # registry simply had nothing to add - an absence reading as an answer. Let
    # the sweep continue (enrichment is best-effort) but say why, with the
    # named, actionable message rather than a bare traceback out of a diagnostic.
    # Caught around the walk itself rather than by probing first: the enumerator
    # imports every lazy subcommand module, so a separate probe call paid for
    # that whole traversal twice.
    try:
        rel_root = root.resolve()
        for path, cmd in iter_python_leaves():
            file_line = ""
            help_line = ""
            cb = getattr(cmd, "callback", None) if cmd else None
            # Typer wraps the user function; ``__wrapped__`` is the real source.
            real = getattr(cb, "__wrapped__", cb) if cb else None
            if real is not None:
                try:
                    sf = Path(inspect.getsourcefile(real))
                    line = inspect.getsourcelines(real)[1]
                    try:
                        file_line = str(sf.resolve().relative_to(rel_root)) + f":{line}"
                    except ValueError:
                        file_line = f"{sf.name}:{line}"
                except (TypeError, OSError):
                    file_line = ""
            help_text = (getattr(cmd, "help", None) or "").strip()
            if not help_text and real is not None:
                help_text = (inspect.getdoc(real) or "").strip()
            help_line = help_text.splitlines()[0] if help_text else ""
            table[path] = (file_line, help_line)
    except VerbRatchetError as exc:
        print(f"verb enrichment SKIPPED: {exc}", file=sys.stderr)
        return {}
    except Exception:
        pass
    return table


def check_controls(counts: Counter, controls: dict[str, int] = None) -> list[str]:
    """Return a list of failed control descriptions (empty means pass)."""
    failed = []
    for leaf, floor in (CONTROLS if controls is None else controls).items():
        got = counts.get(leaf, 0)
        if got < floor:
            failed.append(f"{leaf}: {got} < {floor}")
    return failed


def dead_set(root: Path, leaves_list: list[str]):
    """Leaves with a positive marker of unreachability, plus the evidence.

    A leaf is DEAD when the ``user``, ``runtime``, and ``internal`` buckets are
    all zero. The ``tests`` bucket is deliberately excluded from that sum: a
    leaf named only by its own test is deletable and the test goes with it.

    Every control on every signal must fire first. A zero here is a measurement
    only if the instrument demonstrably finds a caller when one exists; without
    that, a zero means "the search broke" just as easily as "nothing calls it",
    and the two are indistinguishable from the output. So this returns ``None``
    for the dead set when any control fails, rather than an empty or partial
    list a caller might read as an answer.
    """
    leaves = set(leaves_list)
    wrappers = discover_shell_wrappers(root)
    buckets = sweep_buckets(root, leaves, extra_prefixes=wrappers)
    ast_counts, itext_counts, dynamic = sweep_internal(root, leaves)
    # `crates` already sits in the runtime bucket, so an argv-array credit is a
    # runtime reference like any other shell-out from that tree.
    rust_counts = sweep_rust_argv(root, leaves, tests=False)
    buckets["runtime"].update(rust_counts)
    buckets["tests"].update(sweep_rust_argv(root, leaves, tests=True))
    crates_test_counts = sweep(
        root, leaves, binary_form=True, pipe_fan=True,
        paths=iter_crates_paths(root, tests=True), extra_prefixes=wrappers,
    )

    failures: list[str] = []
    ext_total: Counter = Counter()
    for name in ("user", "runtime", "tests"):
        ext_total.update(buckets[name])
    failures += [f"external/{f}" for f in check_controls(ext_total)]
    failures += [
        f"substitution/{f}" for f in check_controls(ext_total, SUBSTITUTION_CONTROLS)
    ]
    failures += [f"rust-argv/{f}" for f in check_controls(rust_counts, RUST_ARGV_CONTROLS)]
    failures += [
        f"crates-tests/{f}"
        for f in check_controls(crates_test_counts, CRATES_TEST_CONTROLS)
    ]
    failures += [f"ast/{f}" for f in check_controls(ast_counts, AST_CONTROLS)]
    failures += [f"internal-text/{f}" for f in check_controls(itext_counts, INTERNAL_TEXT_CONTROLS)]

    internal = Counter()
    internal.update(ast_counts)
    internal.update(itext_counts)

    if failures:
        return None, buckets, internal, dynamic, failures, wrappers

    dead = [
        leaf for leaf in leaves_list
        if buckets["user"].get(leaf, 0) == 0
        and buckets["runtime"].get(leaf, 0) == 0
        and internal.get(leaf, 0) == 0
    ]
    return dead, buckets, internal, dynamic, failures, wrappers


def cluster_breakdown(zero_leaves: list[str]) -> list[tuple[str, int]]:
    c: Counter = Counter()
    for leaf in zero_leaves:
        c[leaf.split()[0]] += 1
    return c.most_common()


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--zero", action="store_true", help="print only zero-reference leaves")
    ap.add_argument("--summary", action="store_true", help="print counts, delta, clusters only")
    ap.add_argument("--self-check", action="store_true", help="controls + leaf parity; exit code")
    ap.add_argument("--dead", action="store_true",
                    help="leaves with zero user, runtime, AND cli/src references")
    ap.add_argument("--dynamic", action="store_true",
                    help="cli/src argv sites whose verb position is not a constant")
    ap.add_argument("--out", help="also write the full table to this path")
    ap.add_argument("--curriculum", type=Path,
                    help="taught-verb file; report the untaught complement and its cull candidates")
    args = ap.parse_args(argv)

    root = repo_root()
    leaves_list = load_leaves(root)
    leaves = set(leaves_list)

    if args.dead or args.dynamic:
        dead, buckets, internal, dynamic, failures, wrappers = dead_set(root, leaves_list)
        if failures:
            print("verb-callers: positive control(s) failed - no list emitted:", file=sys.stderr)
            for f in failures:
                print(f"  {f}", file=sys.stderr)
            return 2
        if args.dynamic:
            shell = sweep_shell_dynamic(root)
            print(f"shell wrappers treated as fno binaries: "
                  f"{', '.join(sorted(wrappers)) or '(none)'}")
            print(f"cli/src argv sites with a non-constant verb position: {len(dynamic)}")
            for site in dynamic:
                print(f"  {site}")
            print(f"\nshell invocations with a variable verb path: {len(shell)}")
            for site in shell:
                print(f"  {site}")
            print(
                "\nEach site can reach a verb this sweep cannot name. Read every one "
                "before deleting anything, or the literal sweep is wrong by exactly "
                "the set they reach."
            )
            return 0
        print(f"{'leaf':38} {'user':>5} {'run':>5} {'test':>5} {'int':>5}")
        print("-" * 62)
        for leaf in dead:
            print(f"{leaf:38} {buckets['user'].get(leaf,0):>5} "
                  f"{buckets['runtime'].get(leaf,0):>5} "
                  f"{buckets['tests'].get(leaf,0):>5} {internal.get(leaf,0):>5}")
        by_group = cluster_breakdown(dead)
        # The gap between these two numbers is the whole reason the cli/src
        # sweep exists. A leaf with no user and no runtime reference looks dead
        # to any sweep that stops at the corpus a contributor reads; the ones
        # rescued here are invoked by an argv built inside cli/src, and deleting
        # them on the strength of the smaller corpus breaks a live caller with
        # no failing test to say so.
        looks_dead = [
            leaf for leaf in leaves_list
            if buckets["user"].get(leaf, 0) == 0 and buckets["runtime"].get(leaf, 0) == 0
        ]
        print(f"\nnamed nowhere a contributor reads: {len(looks_dead)}")
        print(f"  of those, rescued by a cli/src caller: {len(looks_dead) - len(dead)}")
        print(f"dead: {len(dead)} of {len(leaves_list)} leaves "
              f"(zero user + runtime + cli/src refs; tests-only counts as dead)")
        print("by group: " + ", ".join(f"{g}={n}" for g, n in by_group))
        print(f"unresolvable cli/src argv sites: {len(dynamic)} "
              f"(see --dynamic; each can reach a verb no literal sweep names)")
        if args.out:
            Path(args.out).write_text("\n".join(dead) + "\n")
        return 0

    # Dual sweep: the corrected pass applies both false-positive fixes; the
    # uncorrected one reproduces the original 92 bound and backs the
    # monotonicity invariant below, which guards every output mode
    # (corrected_zero must be a subset of uncorrected_zero).
    counts_corr = sweep(
        root, leaves, binary_form=True, pipe_fan=True, paths=iter_cull_corpus(root)
    )
    # The Rust argv arrays are references the token sweep structurally cannot
    # see, so they belong in EVERY mode, not just --dead. --curriculum's cull
    # list is the deletion-decision path; leaving it blind to this shape is the
    # exact failure the shape was added to fix. Folding them in only ADDS
    # references, so the subset invariant below still holds.
    rust_counts = sweep_rust_argv(root, leaves, tests=False)
    counts_corr.update(rust_counts)
    crates_test_counts = sweep(
        root, leaves, binary_form=True, pipe_fan=True,
        paths=iter_crates_paths(root, tests=True),
    )

    # Checked against the rust counter alone, never the merged one: a floor a
    # working text sweep could satisfy on its own is not a control on this sweep.
    failed = check_controls(counts_corr)
    failed += [f"rust-argv/{f}" for f in check_controls(rust_counts, RUST_ARGV_CONTROLS)]
    failed += [
        f"crates-tests/{f}"
        for f in check_controls(crates_test_counts, CRATES_TEST_CONTROLS)
    ]
    # A broken sweep emits no candidate list, including from self-check.
    if failed:
        print("verb-callers: positive control(s) failed - sweep is broken, no list emitted:", file=sys.stderr)
        for f in failed:
            print(f"  {f}", file=sys.stderr)
        return 2

    zero_corr = [leaf for leaf in leaves_list if counts_corr.get(leaf, 0) == 0]

    # Monotonicity invariant: the two false-positive fixes can only ADD
    # references, so the corrected zero set must be a subset of the uncorrected
    # one. This guards every output mode, especially --curriculum, whose cull
    # list is the deletion-decision path and the most consequential output here.
    # A regression that invents false zero-callers fails loudly before any cull
    # list is emitted, not after.
    counts_unc = sweep(
        root, leaves, binary_form=False, pipe_fan=False, paths=iter_cull_corpus(root)
    )
    zero_unc = {leaf for leaf in leaves_list if counts_unc.get(leaf, 0) == 0}
    not_subset = [leaf for leaf in zero_corr if leaf not in zero_unc]
    if not_subset:
        print("verb-callers: corrected zero set is NOT a subset of uncorrected - "
              "a fix removed references, which is impossible:", file=sys.stderr)
        for leaf in not_subset[:10]:
            print(f"  {leaf}", file=sys.stderr)
        return 2

    # --self-check is an instrument-health mode that takes precedence over every
    # output mode (--curriculum, --summary, --zero, default), matching the
    # return-early ordering the other modes already share.
    if args.self_check:
        _, _, _, _, health_failures, _ = dead_set(root, leaves_list)
        if health_failures:
            print(
                "verb-callers: positive control(s) failed - no list emitted:",
                file=sys.stderr,
            )
            for failure in health_failures:
                print(f"  {failure}", file=sys.stderr)
            return 2
        ok = not not_subset
        print(
            "controls: PASS "
            "(substitution, rust-argv, crates-tests, ast, internal-text)",
            {k: counts_corr.get(k, 0) for k in CONTROLS},
        )
        print(f"uncorrected zero: {len(zero_unc)}  corrected zero: {len(zero_corr)}  "
              f"rescued by fixes: {len(zero_unc) - len(zero_corr)}")
        print(f"vs the original {ORIGINAL_ZERO_BOUND} bound: {len(zero_corr)} now "
              f"({ORIGINAL_ZERO_BOUND - len(zero_corr)} below; remainder is corpus drift)")
        print(f"subset invariant: {'OK' if not not_subset else 'BROKEN'}")
        # FRESHNESS, not parity. This compares the baseline file against the
        # generator that wrote it, so agreement proves the file is current and
        # nothing more. It was printed as "baseline/registry parity: identical"
        # and read as independent corroboration of the surface, which it never
        # was - the two sides share a source by construction.
        #
        # It is still worth running: a stale baseline silently drifts the zero
        # count, and this sweep's whole universe comes from that file. The
        # independent check on the surface lives in the ratchet itself, where
        # the Rust dispatchers and the live binary are read separately and must
        # agree.
        try:
            from fno.lint_verb_ratchet import enumerate_all_leaves
            registry = {
                leaf.split(' !')[0].strip() for leaf in enumerate_all_leaves()
            }
            baseline_set = set(leaves_list)
            print(f"baseline freshness (file vs its own generator, NOT an "
                  f"independent check): baseline={len(baseline_set)} "
                  f"registry={len(registry)} current={baseline_set == registry}")
            ok = ok and (baseline_set == registry)
        except Exception as e:
            print(f"baseline freshness: SKIPPED ({e})")
        # Corpus coverage: skills/target/ is a real skill bundle, not a build
        # dir. A `target` exclusion that swallows it (the rg-glob pitfall) leaves
        # controls green while silently under-counting, so assert it is walked.
        try:
            walked = list(iter_corpus(root))
            n_st = sum(1 for p in walked if "skills/target" in str(p))
            print(f"corpus coverage: skills/target files walked = {n_st}")
            ok = ok and n_st > 0
        except Exception as e:
            print(f"corpus coverage: SKIPPED ({e})")
        return 0 if ok else 1

    if args.curriculum:
        # The complement (untaught leaves) intersected with the zero-caller set
        # is the deletion-candidate list the curriculum forces. Reuses the same
        # sweep, controls, skills/target-safe walk, and monotonicity invariant
        # as every other mode; adds only the curriculum layer.
        taught, unknown = load_curriculum(args.curriculum, leaves)
        if unknown:
            print("verb-callers: curriculum verbs not in baseline (typos or cut verbs):",
                  file=sys.stderr)
            for u in unknown:
                print(f"  {u}", file=sys.stderr)
            return 2
        zero_set = set(zero_corr)
        complement = [leaf for leaf in leaves_list if leaf not in taught]
        cull = sorted(leaf for leaf in complement if leaf in zero_set)
        print(f"baseline leaves: {len(leaves_list)}")
        print(f"taught (curriculum): {len(taught)}")
        print(f"complement (untaught): {len(complement)}")
        print(f"cull candidates in complement (zero external callers): {len(cull)}")
        print(f"complement kept (has external caller): {len(complement) - len(cull)}")
        if cull:
            print("cull candidates:")
            for leaf in cull:
                print(f"  {leaf}")
        return 0

    enrichment = load_enrichment(root)

    if args.summary:
        print(f"control values: {', '.join(f'{k}={counts_corr.get(k,0)}' for k in CONTROLS)}")
        print(f"uncorrected zero-ref leaves: {len(zero_unc)}")
        print(f"corrected zero-ref leaves:   {len(zero_corr)}")
        print(f"rescued by the two fixes (uncorrected -> corrected): "
              f"{len(zero_unc) - len(zero_corr)}")
        print(f"vs the original {ORIGINAL_ZERO_BOUND} bound: {len(zero_corr)} now "
              f"({ORIGINAL_ZERO_BOUND - len(zero_corr)} below it; the rest of the gap "
              f"is corpus drift since that measurement)")
        print("corrected zero-ref by top-level group:")
        for grp, n in cluster_breakdown(zero_corr):
            print(f"  {grp}: {n}")
        return 0 if not failed else 2

    rows = zero_corr if args.zero else leaves_list
    enrichment_live = bool(enrichment)
    lines = []
    header = f"{'leaf':34} {'refs':>5}  {'file:line':28}  help"
    lines.append(header)
    lines.append("-" * len(header))
    for leaf in sorted(rows, key=lambda item: (counts_corr.get(item, 0), item)):
        fl, hl = enrichment.get(leaf, ("", ""))
        if not enrichment_live:
            fl = fl or "(needs fno env)"
        mark = "*" if counts_corr.get(leaf, 0) == 0 else " "
        lines.append(f"{mark}{leaf:33} {counts_corr.get(leaf, 0):>5}  {fl:28}  {hl}")
    footer = (
        f"\ncorrected zero-ref: {len(zero_corr)}  "
        f"(uncorrected {len(zero_unc)}, rescued by fixes: "
        f"{len(zero_unc) - len(zero_corr)})\n"
        f"* = zero external callers (cull candidate); file:line/help blank if fno env absent"
    )
    lines.append(footer)
    report = "\n".join(lines)
    print(report)
    if args.out:
        Path(args.out).write_text(report + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
