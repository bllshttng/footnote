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
# cli/src/fno/agents/providers/base.py is the densest file in the repository
# (over half its lines are documentation) and must score ZERO findings; the
# file-level density of base.py is exactly the property a percentage gate
# misreads as bloat. A finding there means the instrument has drifted toward
# density and the threshold needs re-examining, not lowering.
#
# Advisory on first landing: always exits 0, findings printed to stdout. The
# lint never auto-deletes; it reports and a human cuts.
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

REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$REPO_ROOT"

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
    src = Path(path).read_text(errors="replace")
    try:
        tree = ast.parse(src)
    except Exception:
        return []
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
        body_lines = sorted(k for k in coms if n.lineno <= k <= end)
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
