#!/usr/bin/env python3
"""Verb census - for each baseline CLI leaf, find its callers and assign a verdict.

Inputs:
  - scripts/ci/verb-baseline.txt  (the denominator; every invocable leaf)

Output:
  - JSON, one record per verb with per-bucket hit counts, a verdict, the first
    caller found, and (with --curriculum) whether the curriculum teaches it.

The census answers two questions the verb-cut decision consumes:
  1. Which verbs have no caller anywhere (deletion candidates)?
  2. Which verbs does the curriculum teach, and what is the complement?

It is deliberately a standalone stdlib script, not a registered `fno` verb:
adding a verb to measure the verb surface would be measuring its own shadow.

Usage:
  python scripts/ci/verb-census.py                      # census + headline summary
  python scripts/ci/verb-census.py --json               # full JSON record stream
  python scripts/ci/verb-census.py --curriculum PATH    # add taught/complement split
  python scripts/ci/verb-census.py --check-controls     # exit 1 if a control regresses

A zero-caller result and a broken instrument look identical, so the script ships
with positive controls: known-live verbs that must be found. Run --check-controls
in CI to catch a matching regression (the blueprint's first regex rejected a
preceding hyphen and so missed every `fno-agents <verb>` Rust-front caller; the
Rust-front controls below exist specifically to stop that class returning).
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Iterable

# --------------------------------------------------------------------------- #
# Paths and corpus
# --------------------------------------------------------------------------- #

# This file lives at <repo>/scripts/ci/verb-census.py.
REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_BASELINE = REPO_ROOT / "scripts" / "ci" / "verb-baseline.txt"

# Dirs that are not repo content, or that re-state the surface we measure.
# NOTE: "target" is deliberately absent. As a path SEGMENT it collides with
# real source trees - skills/target (a skill), tests/target (fixtures), and
# cli/src/fno/target (the target command package) - and excluding it segment-
# wide hides their callers, flipping live verbs toward the deletion list.
# Rust build output is handled separately in should_exclude.
EXCLUDE_DIRS = {
    ".git", "graphify-out", "node_modules", ".venv", "__pycache__",
    ".understand-anything", ".claude", ".fno", "internal", ".pytest_cache",
    ".mypy_cache", ".ruff_cache", "dist", "build",
}


def should_exclude(parts: tuple[str, ...]) -> bool:
    """True if a repo-relative path's parts mark it non-content.

    Rust build output (`target/` at the workspace root, or `crates/<c>/target/`)
    is excluded by position, not by the bare segment, so the skills/target,
    tests/target, and cli/src/fno/target source trees stay in the corpus.
    """
    if any(seg in EXCLUDE_DIRS for seg in parts):
        return True
    if parts[0] == "target":
        return True
    if len(parts) >= 3 and parts[0] == "crates" and parts[2] == "target":
        return True
    return False

# A verb must not count the census's own data files as a caller. The baseline,
# this script (its DEFAULT_CONTROLS literals), and the curriculum file (its
# taught-verb literals) all spell verb paths without invoking anything; counting
# them would mask a verb that genuinely has no caller behind a self-reference.
SELF_EXCLUDE = {
    "scripts/ci/verb-baseline.txt",
    "scripts/ci/verb-census.py",
    "scripts/ci/curriculum.txt",
}

# Skip binary/large-non-text suffixes; the census matches text.
BINARY_SUFFIXES = {
    ".png", ".jpg", ".jpeg", ".gif", ".ico", ".webp", ".svg", ".pdf",
    ".zip", ".gz", ".tar", ".lock", ".toml.lock", ".bin", ".dat",
    ".mp4", ".mov", ".mp3", ".wav", ".woff", ".woff2", ".ttf", ".eot",
    ".icns", ".dylib", ".so", ".a", ".o", ".class",
}

BUCKET_ORDER = ["docs", "agentsurface", "machinery", "impl", "tests", "other"]


def classify_bucket(rel_path: str) -> str:
    """Bucket a repo-relative path by where it lives.

    Order matters: `tests` is checked before `machinery`/`impl` so that
    `crates/x/tests/` and `cli/tests/` land in `tests`, not in the bucket of
    their parent tree. `cli/src/fno/test_cmd.py` has no `tests` path segment,
    so it correctly falls through to `impl` (it is source, not a test dir).
    """
    parts = Path(rel_path).parts
    if parts and parts[0] == "docs":
        return "docs"
    if parts and parts[0] in {"skills", "agents", "commands"}:
        return "agentsurface"
    if "tests" in parts:
        return "tests"
    if parts and parts[0] in {"hooks", "scripts", "crates"}:
        return "machinery"
    if len(parts) >= 2 and parts[0] == "cli" and parts[1] == "src":
        return "impl"
    return "other"


def iter_corpus(repo_root: Path) -> Iterable[tuple[str, str]]:
    """Yield (rel_path, text) for every searchable text file in the repo."""
    for path in repo_root.rglob("*"):
        if not path.is_file():
            continue
        # Fast exits before the (relatively) expensive resolve/decode.
        try:
            rel = path.relative_to(repo_root).as_posix()
        except ValueError:
            continue
        if should_exclude(Path(rel).parts):
            continue
        if rel in SELF_EXCLUDE:
            continue
        if path.suffix.lower() in BINARY_SUFFIXES:
            continue
        raw = path.read_bytes()
        if b"\x00" in raw[:8192]:
            continue  # binary by detection
        yield rel, raw.decode("utf-8", errors="replace")


# --------------------------------------------------------------------------- #
# Verb parsing and pattern compilation
# --------------------------------------------------------------------------- #

# Characters allowed between adjacent tokens of one verb. One pattern then
# matches shell prose (`fno backlog get`), Python subprocess lists
# (`["backlog", "get"]`), Rust arg arrays (`["backlog", "get"]`), and the
# hyphen-joined Rust front door (`fno-agents loop-check` -> the tokens
# `agents` and `loop-check` sit separated by a single space).
SEP = r"[\s,'\"\[\]]+"


def load_verbs(baseline_path: Path) -> list[str]:
    """Parse the baseline into a sorted list of verb leaf paths.

    Strips comment lines and `!--flag` hidden-option tokens; the flags are an
    ungated axis the ratchet guards, not part of the invocation path.
    """
    verbs: list[str] = []
    for line in baseline_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        line = re.sub(r"\s*!--\S+", "", line)
        verbs.append(line)
    return sorted(set(verbs))


def compile_pattern(verb: str) -> re.Pattern[str]:
    """Compile a caller-matching regex for one verb.

    Multi-token verbs match their tokens adjacent, separated only by SEP, with
    the first token bounded by a non-word lookbehind that PERMITS a preceding
    hyphen. That last clause is load-bearing: `fno-agents loop-check` reaches
    the `agents loop-check` leaf, and a lookbehind that rejected `-` (the
    blueprint's original bug) would report every Rust-front caller as dead.

    Single-token verbs (help, status, version, ...) additionally require an
    `fno`-shaped front door immediately before, because the bare token matches
    most of the repository and proves nothing about invocation.
    """
    tokens = verb.split()
    escaped = [re.escape(t) for t in tokens]
    # Trailing boundary rejects a following word char OR hyphen, so a leaf that
    # prefixes a longer hyphenated leaf does not steal its callers: `backlog
    # batch ship` does not match `backlog batch ship-closeable`, and `agents
    # loop` does not match `agents loop-check`. The leading lookbehind still
    # permits a preceding hyphen so the `fno-agents` Rust front door matches.
    tail = r"(?![\w-])"
    if len(escaped) == 1:
        # fno, fno-py, fno-agents, ... then separators then the token.
        # `fno(?:-[a-z]+)?` does not match `fnoteworthy` (no separator follows
        # `fno`), so a longer word sharing the prefix is not a false front door.
        body = rf"(?<!\w)fno(?:-[a-z]+)?{SEP}{escaped[0]}{tail}"
    else:
        joined = SEP.join(escaped)
        body = rf"(?<!\w){joined}{tail}"
    return re.compile(body)


# --------------------------------------------------------------------------- #
# Verdict logic
# --------------------------------------------------------------------------- #

# Decision priority: the highest bucket in this order that has a hit decides.
# docs > agentsurface > machinery > impl > tests; `other` is informational only
# and never rescues a verb from CUT-1, because a stray mention in a non-load-
# bearing file is not a caller anyone depends on.
def decide_verdict(buckets: dict[str, int]) -> str:
    if buckets["docs"]:
        return "KEEP-DOC-GAP"
    if buckets["agentsurface"]:
        return "KEEP-INTERNAL-skill"
    if buckets["machinery"]:
        return "KEEP-INTERNAL-machinery"
    if buckets["impl"]:
        return "CUT-3"
    if buckets["tests"]:
        return "CUT-2"
    return "CUT-1"


# --------------------------------------------------------------------------- #
# Census
# --------------------------------------------------------------------------- #

def run_census(verbs: list[str], repo_root: Path) -> list[dict]:
    patterns = {v: compile_pattern(v) for v in verbs}
    records: dict[str, dict] = {
        v: {"verb": v, "buckets": {b: 0 for b in BUCKET_ORDER},
            "verdict": "", "first_caller": None}
        for v in verbs
    }
    for rel, text in iter_corpus(repo_root):
        bucket = classify_bucket(rel)
        for v, pat in patterns.items():
            if pat.search(text):
                rec = records[v]
                rec["buckets"][bucket] += 1
                if rec["first_caller"] is None:
                    rec["first_caller"] = rel
    for rec in records.values():
        rec["verdict"] = decide_verdict(rec["buckets"])
    return sorted(records.values(), key=lambda r: r["verb"])


# --------------------------------------------------------------------------- #
# Positive controls
# --------------------------------------------------------------------------- #

# Three matching regimes, each with its own probe so a regression in one cannot
# hide behind the other two passing:
#   - Python-front multi-token:  backlog get, target init, mail send, ...
#   - Rust-front multi-token:    agents loop-check, agents finalize
#                                (the hyphen-lookbehind bug class)
#   - single-token (front door): whoami, version
DEFAULT_CONTROLS = [
    "backlog get",
    "target init",
    "mail send",
    "claim acquire",
    "pr merge",
    "backlog next",
    "agents spawn",
    "backlog idea",
    "agents loop-check",
    "agents finalize",
    "whoami",
    "version",
]


def control_failures(records: list[dict], controls: list[str]) -> list[str]:
    """A control fails if it has no caller outside `impl`.

    `impl` self-references (the verb's own Click/clap definition in cli/src)
    would let a broken repo-wide walk pass, so the control must find the verb
    somewhere else: a doc, a skill, a hook/script/crate, a test, or other.
    """
    by_verb = {r["verb"]: r for r in records}
    failed: list[str] = []
    for c in controls:
        rec = by_verb.get(c)
        if rec is None:
            failed.append(f"{c}: not in baseline (control misconfigured)")
            continue
        outside = sum(n for b, n in rec["buckets"].items() if b != "impl")
        if outside == 0:
            failed.append(
                f"{c}: no caller outside impl "
                f"(buckets={rec['buckets']}) - matching regression suspected"
            )
    return failed


# --------------------------------------------------------------------------- #
# Curriculum / complement
# --------------------------------------------------------------------------- #

def load_curriculum(path: Path, verbs: set[str]) -> tuple[set[str], list[str]]:
    """Read taught verbs; report any that are not baseline leaves.

    A taught verb that is not a real leaf is a curriculum error (a typo or a
    verb that was cut), not a census finding, so it is surfaced loudly.
    """
    raw: set[str] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        raw.add(line)
    unknown = sorted(raw - verbs)
    taught = raw & verbs  # only baseline verbs; unknowns are surfaced separately
    return taught, unknown


# --------------------------------------------------------------------------- #
# Output
# --------------------------------------------------------------------------- #

def verdict_summary(records: list[dict]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for r in records:
        counts[r["verdict"]] = counts.get(r["verdict"], 0) + 1
    return counts


def print_summary(records: list[dict], taught: set[str] | None) -> None:
    total = len(records)
    print(f"baseline leaves:    {total}")
    # The deletion-census measurement is over the complement (untaught), so
    # when a curriculum is bound the verdict breakdown filters to it; without a
    # curriculum the breakdown covers the whole baseline.
    if taught is not None:
        complement = total - len(taught)
        print(f"taught (curriculum): {len(taught)}")
        print(f"complement:         {complement}")
        scope = [r for r in records if not r["taught"]]
        scope_label = "complement"
    else:
        scope = records
        scope_label = "baseline"
    counts = verdict_summary(scope)
    cut = counts.get("CUT-1", 0) + counts.get("CUT-2", 0) + counts.get("CUT-3", 0)
    keep = counts.get("KEEP-INTERNAL-skill", 0) + counts.get("KEEP-INTERNAL-machinery", 0)
    doc_gap = counts.get("KEEP-DOC-GAP", 0)
    print(f"CUT candidates ({scope_label}): {cut}  (CUT-1={counts.get('CUT-1', 0)}, "
          f"CUT-2={counts.get('CUT-2', 0)}, CUT-3={counts.get('CUT-3', 0)})")
    print(f"KEEP-INTERNAL ({scope_label}):  {keep}  (skill={counts.get('KEEP-INTERNAL-skill', 0)}, "
          f"machinery={counts.get('KEEP-INTERNAL-machinery', 0)})")
    print(f"KEEP-DOC-GAP ({scope_label}):   {doc_gap}")


# --------------------------------------------------------------------------- #
# Main
# --------------------------------------------------------------------------- #

def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    p.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    p.add_argument("--curriculum", type=Path,
                   help="taught-verb file; adds taught/complement split")
    p.add_argument("--json", action="store_true",
                   help="emit full JSON records instead of the summary")
    p.add_argument("--check-controls", action="store_true",
                   help="exit 1 if any positive control is not found outside impl")
    p.add_argument("--control-verbs", type=Path,
                   help="override the built-in control set (one verb per line)")
    args = p.parse_args(argv)

    verbs = load_verbs(args.baseline)
    verb_set = set(verbs)

    # A focused controls check only needs the control verbs' patterns, not all
    # 367 - scanning the whole corpus against 12 patterns is seconds, not the
    # ~3min the full census takes. The full scan still runs when the caller also
    # wants JSON output or the curriculum summary.
    controls_only = (
        args.check_controls and not args.json and not args.curriculum
    )
    if controls_only:
        ctrl_set = set(args.control_verbs.read_text().splitlines()) if args.control_verbs else set(DEFAULT_CONTROLS)
        scan_verbs = [v for v in verbs if v in ctrl_set]
    else:
        scan_verbs = verbs
    records = run_census(scan_verbs, args.repo_root)

    taught: set[str] | None = None
    if args.curriculum:
        taught, unknown = load_curriculum(args.curriculum, verb_set)
        if unknown:
            print("ERROR: curriculum verbs not in baseline (typos or cut verbs):",
                  file=sys.stderr)
            for u in unknown:
                print(f"  {u}", file=sys.stderr)
            return 2
        for r in records:
            r["taught"] = r["verb"] in taught

    if args.check_controls:
        controls = DEFAULT_CONTROLS
        if args.control_verbs:
            controls = [
                ln.strip() for ln in args.control_verbs.read_text().splitlines()
                if ln.strip() and not ln.strip().startswith("#")
            ]
        failed = control_failures(records, controls)
        if failed:
            print("POSITIVE-CONTROL FAILURES:", file=sys.stderr)
            for f in failed:
                print(f"  {f}", file=sys.stderr)
            return 1
        print(f"positive controls: all {len(controls)} found outside impl")
        if controls_only:
            return 0  # focused check done; no full census to summarise

    if args.json:
        print(json.dumps(records, indent=2, sort_keys=True))
    else:
        print_summary(records, taught)

    return 0


if __name__ == "__main__":
    sys.exit(main())
