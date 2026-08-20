#!/usr/bin/env bash
# scripts/ci/check-comment-restates-docstring.sh
#
# Advisory lint: flag an inline comment block inside a Python function whose
# content tokens overlap the function's OWN DOCSTRING by >= 0.50.
#
# AXIS - read this before tuning the threshold. This instrument flags
# DUPLICATION of a docstring: content already stated above, in the function's
# own docstring, then repeated below as inline prose. It is NOT the comment-
# vs-the-line-below overlap lint that was measured and refused. That one scored
# a comment block against the identifiers on the next code line; at the
# threshold that caught its target it made 607 findings across 753 files (a
# grandfathered baseline by another name), and at a precise threshold it found
# 44 one-word comments and missed the target. It could not separate "restates
# the call" from "names the expected result of the call", because a comment on
# an assertion shares the callee's identifiers by construction.
#
# The discriminator here is different and safe: "already stated above." The cut
# is lossless because the docstring carries the content. Do NOT let this drift
# back toward the line-below axis - if you find yourself scoring a comment
# against the code on the next line, you have rebuilt the refused instrument.
#
# Safe where every density gate was not. The reference exemplar
# cli/src/fno/agents/harnesses/base.py is the densest file in the repository
# (over half its lines are documentation) and must score ZERO findings; the
# file-level density of base.py is exactly the property a percentage gate
# misreads as bloat. A finding there means the instrument has drifted toward
# density and the threshold needs re-examining, not lowering.
#
# Advisory on first landing: findings never affect the exit code (printed to
# stdout, always exit 0 on a successful run). A Python crash exits non-zero -
# that is a real bug and is allowed to surface; it is not the findings firing.
# The lint never auto-deletes; it reports and a human cuts.
#
# Reading protocol (load-bearing): the flag set is a READING LIST, never a fix
# list. Measured precision is 10 lossless cuts out of 172 findings (5.8%); the
# other 162 share vocabulary with the docstring but carry unique purpose
# (concurrency guards, edge cases, per-harness behavior, ponytail annotations)
# the docstring does not cover. A human decides every cut, and this tool has no
# authority to propose deletion. Treating the findings as a TODO and deleting
# them wholesale would strip exactly the load-bearing comments the policy
# reserves comments for. Flagging at 0.50 is recall; the lossless cut bar sits
# above it on purpose, and the gap between them is the finding.
#
# Graduation (when this starts failing CI, if ever): it becomes a blocking
# gate - exit 1 on any finding - only after it runs CLEAN (zero findings)
# across a run of real PRs once the 172-finding baseline is cut. At 5.8%
# precision that clean-run trigger may never fire, because most findings are
# real comments a human should keep. That is the stated expectation, not a
# failure: the tool earns its keep as the scan that holds the docstring-
# duplication axis, and a higher flag floor can be measured on real PRs if
# blocking is ever wanted. Do not delete this as dead weight while it is
# advisory: this script is the only place the axis is encoded, and losing it
# is what lets the refuted density and line-below instruments get rebuilt.
#
# Usage:
#   bash scripts/ci/check-comment-restates-docstring.sh [file_or_dir ...]
#   no args = scan cli/src/
#
# Portability: bash 3.2+, python3. Python owns the tree walk (no mapfile, no
# find portability concerns). Per-language ast/tokenize keeps Rust '#' an
# attribute sigil, not a comment; a Rust ('///') extension is deliberately not
# in this first pass.
set -euo pipefail

# Resolved BEFORE the cd: a relative $0 stops resolving once the shell is
# somewhere else, and the --self-check re-invocation below would then die
# inside a command substitution with no message.
SELF="$(cd "$(dirname "$0")" 2>/dev/null && pwd)/$(basename "$0")"

REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$REPO_ROOT"

# --self-check is what makes this script reachable at all. Until 2026-08-20 no
# runner invoked it, so the reading list it produces was never produced and its
# "advisory" status meant "off". The reading list is a human activity and does
# not belong in a CI log; instrument drift does. The header names the exact
# drift to watch: the densest file in the repository must score ZERO, and a
# finding there means the threshold has slid toward measuring density. That is
# a real assertion that can fail, so it is the half CI holds.
EXEMPLAR="cli/src/fno/agents/harnesses/base.py"

# _findings <file> - run the scan and echo its finding count, refusing to let a
# crash read as a number. `scan()` swallows every exception and returns [], and
# `collect()` returns [] for anything it does not recognize, so "findings: 0"
# is what a DEAD instrument prints too. The count alone cannot tell the two
# apart, which is why the caller pairs it with a positive control below.
_findings() {
    local out rc
    out=$(bash "$SELF" "$1")
    rc=$?
    if [[ $rc -ne 0 ]]; then
        echo "comment-restates-docstring: the scan itself exited $rc on $1" >&2
        printf '%s\n' "$out" >&2
        return 1
    fi
    printf '%s\n' "$out" | awk '/^findings:/ {print $2}'
}

if [[ "${1:-}" == "--self-check" ]]; then
    if [[ ! -f "$EXEMPLAR" ]]; then
        echo "comment-restates-docstring: exemplar missing at $EXEMPLAR - repoint it, do not drop the check" >&2
        exit 1
    fi

    # POSITIVE CONTROL first. A fixture whose comment repeats its own docstring
    # verbatim MUST be found. Without this the whole self-check asserts an
    # absence, and a broken tokenizer, an unparseable file or a regex that
    # stopped matching would all certify the gate green - the trap AGENTS.md
    # names as "assert a positive marker, never an absence".
    _fixdir="$(mktemp -d)" || { echo "comment-restates-docstring: mktemp failed" >&2; exit 1; }
    trap 'rm -rf "$_fixdir"' EXIT
    # The fixture has to clear every real threshold or it proves nothing:
    # 15+ distinct docstring content tokens, a comment BLOCK of 2+ contiguous
    # full-line comments, 10+ distinct comment tokens, and >= 0.50 overlap.
    cat > "$_fixdir/fixture.py" <<'FIXEOF'
def merge_accounts(primary, secondary):
    """Merge the secondary account record into the primary account record,
    reconcile duplicate contact entries, drop stale session tokens, write an
    audit trail, and return the merged primary account."""
    # Merge the secondary account record into the primary account record and
    # reconcile duplicate contact entries, drop stale session tokens, write an
    # audit trail, and return the merged primary account.
    return primary
FIXEOF
    pos=$(_findings "$_fixdir/fixture.py") || exit 1
    if [[ "$pos" == "0" || -z "$pos" ]]; then
        echo "comment-restates-docstring: positive control found nothing." >&2
        echo "A comment repeating its own docstring verbatim must be flagged. The instrument is not measuring anything; a zero on the exemplar below would be meaningless." >&2
        exit 1
    fi

    # NEGATIVE CONTROL. The densest file in the repository must score zero; a
    # finding there means the threshold slid toward measuring density, which is
    # the axis this lint exists NOT to measure.
    n=$(_findings "$EXEMPLAR") || exit 1
    if [[ "$n" != "0" ]]; then
        echo "comment-restates-docstring: the density exemplar $EXEMPLAR scored $n findings, expected 0." >&2
        echo "Two things produce this, and the fix differs. READ THE FINDING FIRST:" >&2
        echo "  bash scripts/ci/check-comment-restates-docstring.sh $EXEMPLAR" >&2
        echo "1. The comment really does restate its docstring. Cut the comment; the exemplar is a live source file and a real finding in it is a real finding." >&2
        echo "2. Nothing in the file changed and it still flags. Then the threshold has drifted toward measuring documentation density, which is the axis this lint exists NOT to measure. Re-examine the threshold; do not lower it." >&2
        exit 1
    fi
    echo "comment-restates-docstring: self-check ok (control flags $pos, exemplar scores 0)"
    exit 0
fi

# Advisory: findings never affect the exit code. python exits 0 on success; a
# crash (non-zero) is a real bug and is allowed to surface.
python3 - "$@" <<'PY'
import ast, io, re, sys, tokenize
from pathlib import Path

WORD = re.compile(r"[A-Za-z][A-Za-z0-9]*")
STOP = set("""a an the and or but if then else for to of in on at by with from is are was were be been
being this that these those it its as not no we you i do does did can could should would may might must
so than when while into over under out up down only just also very each per via use used using set sets
""".split())


def content_tokens(text):
    return [w.lower() for w in WORD.findall(text) if w.lower() not in STOP and len(w) > 1]


def collect(args):
    # Python owns discovery so bash needs no mapfile/find.
    if not args:
        args = ["cli/src"]
    out = []
    for a in args:
        p = Path(a)
        if p.is_dir():
            out.extend(sorted(str(x) for x in p.rglob("*.py") if x.is_file()))
        elif p.suffix == ".py" and p.is_file():
            out.append(str(p))
    return out


def scan(path):
    try:
        src = Path(path).read_text(errors="replace")
        tree = ast.parse(src)
    except Exception:
        return []  # unreadable / unparseable: skip, never crash the advisory run
    lines = src.splitlines()
    # Full-line '# ' comments only: a trailing comment (code before the #) does
    # not mark its whole line, so it is not counted as inline narration.
    coms = {}
    try:
        for t in tokenize.generate_tokens(io.StringIO(src).readline):
            if t.type == tokenize.COMMENT and not lines[t.start[0] - 1][:t.start[1]].strip():
                coms[t.start[0]] = t.string.lstrip("# ")
    except Exception:
        pass
    out = []
    for n in ast.walk(tree):
        if not isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        doc = ast.get_docstring(n)
        if not doc:
            continue
        dt = set(content_tokens(doc))
        if len(dt) < 15:
            continue
        end = n.end_lineno or n.lineno
        # Exclude nested def/class bodies: a comment inside an inner scope
        # belongs to that scope, not this function, so it is evaluated against
        # the inner docstring (if any) rather than this outer one.
        nested = [
            (c.lineno, c.end_lineno or c.lineno)
            for c in ast.walk(n)
            if c is not n and isinstance(c, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))
        ]
        body_lines = sorted(
            k for k in coms
            if n.lineno <= k <= end and not any(s <= k <= e for s, e in nested)
        )
        # Group contiguous comment lines into blocks.
        blocks, cur = [], []
        for ln in body_lines:
            if cur and ln == cur[-1] + 1:
                cur.append(ln)
            else:
                if cur:
                    blocks.append(cur)
                cur = [ln]
        if cur:
            blocks.append(cur)
        for b in blocks:
            if len(b) < 2:
                continue
            ct = set(content_tokens(" ".join(coms[l] for l in b)))
            if len(ct) < 10:
                continue
            ov = len(ct & dt) / len(ct)
            if ov >= 0.50:
                out.append((path, n.name, b[0], len(b), round(ov, 2),
                            len(doc.splitlines()), end - n.lineno,
                            [lines[l - 1].strip() for l in b]))
    return out


files = collect(sys.argv[1:])
findings = []
for f in files:
    findings.extend(scan(f))

print(f"scanned: {len(files)} python file(s)")
print(f"findings: {len(findings)} inline comment block(s) restating >=50% of their function's docstring")
if findings:
    print()
    for path, name, ln, nblk, ov, ndoc, fspan, blocklines in sorted(findings, key=lambda x: (-x[4], x[0])):
        print(f"{path}:{ln}  fn={name}  overlap={ov}  block={nblk}L  doc={ndoc}L  fnspan={fspan}L")
        for bl in blocklines:
            print(f"    | {bl}")
        print()
PY
