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
      surrounding whitespace fans out to one candidate per alternative (reusing
      the ``_parse_mux_usage`` idiom). Whitespace around the pipe is a markdown
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
    "runtime": ("scripts", "hooks", "crates"),
    "tests": ("cli/tests", "tests"),
}
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


def _clean(tok: str) -> str:
    return tok.strip(_STRIP)


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


def sweep_internal(root: Path, leaves: set[str]) -> tuple[Counter, list[str]]:
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


def sweep_buckets(root: Path, leaves: set[str], extra_prefixes=()) -> dict[str, Counter]:
    """Per-bucket reference counts, so a deletion knows what it costs."""
    out: dict[str, Counter] = {}
    for name, entries in BUCKETS.items():
        out[name] = sweep(
            root, leaves, binary_form=True, pipe_fan=True,
            paths=iter_paths(root, entries), extra_prefixes=extra_prefixes,
        )
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

    failures: list[str] = []
    ext_total: Counter = Counter()
    for name in ("user", "runtime", "tests"):
        ext_total.update(buckets[name])
    failures += [f"external/{f}" for f in check_controls(ext_total)]
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

    # Dual sweep: uncorrected reproduces the original 92 bound; corrected applies
    # both false-positive fixes. corrected_zero must be a subset of uncorrected_zero.
    counts_corr = sweep(root, leaves, binary_form=True, pipe_fan=True)
    counts_unc = sweep(root, leaves, binary_form=False, pipe_fan=False)

    failed = check_controls(counts_corr)
    # A broken sweep emits no candidate list, even in self-check.
    if failed and not args.self_check:
        print("verb-callers: positive control(s) failed - sweep is broken, no list:", file=sys.stderr)
        for f in failed:
            print(f"  {f}", file=sys.stderr)
        return 2

    zero_corr = [l for l in leaves_list if counts_corr.get(l, 0) == 0]
    zero_unc = {l for l in leaves_list if counts_unc.get(l, 0) == 0}
    not_subset = [l for l in zero_corr if l not in zero_unc]
    if not_subset:
        print("verb-callers: corrected zero set is NOT a subset of uncorrected - "
              "a fix removed references, which is impossible:", file=sys.stderr)
        for l in not_subset[:10]:
            print(f"  {l}", file=sys.stderr)
        return 2

    enrichment = load_enrichment(root) if not args.self_check else {}

    if args.self_check:
        ok = not failed and not not_subset
        print("controls:", "PASS" if not failed else "FAIL",
              {k: counts_corr.get(k, 0) for k in CONTROLS})
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
            registry = {l.split(' !')[0].strip() for l in enumerate_all_leaves()}
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
    for leaf in sorted(rows, key=lambda l: (counts_corr.get(l, 0), l)):
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
