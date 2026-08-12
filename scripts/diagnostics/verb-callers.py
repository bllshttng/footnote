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

Usage:
  python3 scripts/diagnostics/verb-callers.py            # full table, all leaves
  python3 scripts/diagnostics/verb-callers.py --zero     # only zero-ref leaves
  python3 scripts/diagnostics/verb-callers.py --summary  # counts + delta + clusters
  python3 scripts/diagnostics/verb-callers.py --self-check  # controls + parity, exit code

The sweep itself is stdlib-only. ``file:line`` and help columns require the fno
environment (click/typer); run under ``uv run --project cli python ...`` for the
enriched table, or accept blank columns under a bare interpreter.
"""
from __future__ import annotations

import argparse
import os
import re
import string
import subprocess
import sys
from collections import Counter
from itertools import product
from pathlib import Path

# --- corpus -----------------------------------------------------------------

# Everything a contributor reads, EXCLUDING cli/src (where verbs are defined).
CORPUS_DIRS = [
    "skills", "docs", "scripts", "hooks", "agents",
    "commands", "crates", "cli/tests", "tests",
]
CORPUS_FILES = ["AGENTS.md", "README.md"]

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
    taught = set(raw) & leaves
    unknown = sorted(set(raw) - leaves)
    return taught, unknown


def iter_corpus(root: Path):
    for entry in CORPUS_DIRS + CORPUS_FILES:
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


def sweep(root: Path, leaves: set[str], *, binary_form: bool, pipe_fan: bool) -> Counter:
    """Count references. Returns a Counter keyed by leaf path."""
    counts: Counter = Counter()
    prefixes = PREFIXES if binary_form else {"fno", "fno-py"}
    for path in iter_corpus(root):
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


def check_controls(counts: Counter) -> list[str]:
    """Return a list of failed control descriptions (empty means pass)."""
    failed = []
    for leaf, floor in CONTROLS.items():
        got = counts.get(leaf, 0)
        if got < floor:
            failed.append(f"{leaf}: {got} < {floor}")
    return failed


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
    ap.add_argument("--out", help="also write the full table to this path")
    ap.add_argument("--curriculum", type=Path,
                    help="taught-verb file; report the untaught complement and its cull candidates")
    args = ap.parse_args(argv)

    root = repo_root()
    leaves_list = load_leaves(root)
    leaves = set(leaves_list)

    # Corrected sweep applies both false-positive fixes. The uncorrected sweep
    # runs later (only when its self-consistency invariant is needed); --curriculum
    # returns off the corrected sweep alone and skips that second corpus walk.
    counts_corr = sweep(root, leaves, binary_form=True, pipe_fan=True)

    failed = check_controls(counts_corr)
    # A broken sweep emits no candidate list, even in self-check.
    if failed and not args.self_check:
        print("verb-callers: positive control(s) failed - sweep is broken, no list:", file=sys.stderr)
        for f in failed:
            print(f"  {f}", file=sys.stderr)
        return 2

    zero_corr = [l for l in leaves_list if counts_corr.get(l, 0) == 0]

    if args.curriculum:
        # The complement (untaught leaves) intersected with the zero-caller set
        # is the deletion-candidate list the curriculum forces. Reuses the same
        # sweep, controls, and skills/target-safe walk as the rest of the tool;
        # adds only the curriculum layer.
        taught, unknown = load_curriculum(args.curriculum, leaves)
        if unknown:
            print("verb-callers: curriculum verbs not in baseline (typos or cut verbs):",
                  file=sys.stderr)
            for u in unknown:
                print(f"  {u}", file=sys.stderr)
            return 2
        zero_set = set(zero_corr)
        complement = [l for l in leaves_list if l not in taught]
        cull = sorted(l for l in complement if l in zero_set)
        print(f"baseline leaves: {len(leaves_list)}")
        print(f"taught (curriculum): {len(taught)}")
        print(f"complement (untaught): {len(complement)}")
        print(f"cull candidates in complement (zero external callers): {len(cull)}")
        print(f"complement kept (has external caller): {len(complement) - len(cull)}")
        if cull:
            print("cull candidates:")
            for l in cull:
                print(f"  {l}")
        return 0

    # Uncorrected sweep + the corrected-is-subset-of-uncorrected invariant guard
    # the zero-list output. Not needed for --curriculum, which returned above.
    counts_unc = sweep(root, leaves, binary_form=False, pipe_fan=False)
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
        # Parity: the sweep's leaf universe (the baseline file) must match the
        # live registry, else a stale baseline silently drifts the zero count.
        # Comparing enumerate_python_leaves vs iter_python_leaves would be
        # tautological (both read the same iterator), so compare the file against
        # enumerate_all_leaves - the same source the ratchet regenerates from.
        try:
            from fno.lint_verb_ratchet import enumerate_all_leaves
            registry = {l.split(' !')[0].strip() for l in enumerate_all_leaves()}
            baseline_set = set(leaves_list)
            print(f"baseline/registry parity: baseline={len(baseline_set)} "
                  f"registry={len(registry)} identical={baseline_set == registry}")
            ok = ok and (baseline_set == registry)
        except Exception as e:
            print(f"baseline/registry parity: SKIPPED ({e})")
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
