#!/usr/bin/env bash
# scripts/ci/check-reachable-paths.sh
#
# Graduates the AGENTS.md pitfall "A guard placed on one of N reachable paths
# is decorative" into a CI gate. Eleven recorded specimens share one shape:
# an operation implemented, or a property read, on some reachable paths and
# not others, shipping green because the covered path is the one anybody
# tested. Prose could not prevent it because enumerating paths is work the
# author must remember at the exact moment they believe they are finished.
#
# Three engines, two drift detectors and one registry:
#
#   A  cross-language twin literals - a string literal >= 24 normalized
#      chars maintained in BOTH a .py and a .rs file. The stale-baseline leg
#      is the load-bearing one: editing one twin alone drops the pair from
#      the scan, the baseline entry goes stale, and CI names the untouched
#      sibling ("fixed the receipt in Rust, missed the Python twin").
#   B  auto-loaded prose duplication - a normalized line >= 60 chars stated
#      in both an auto-loaded surface (AGENTS.md, CLAUDE.md, using-fno, and
#      every .claude/rules/*.md; mirroring what check-preamble-budget.sh
#      budgets, kept in sync with it) and any other tracked .md. The copy
#      nobody loads at session start is the copy that drifts.
#   R  must-reference registry - "a site matching X must also reference Y in
#      the enclosing function". Covers BOTH halves of the class: writer half
#      (every env build on a spawn path must scrub), reader half (every
#      status-derivation helper must route through the read-time overlay).
#      A registry entry whose site pattern matches nothing in the tree is
#      itself a failure: a decorative guard guarding zero paths.
#
# Baseline contract (scripts/ci/reachable-paths-baseline.txt): current
# duplicates/offenders are held with a reason per line, and A/B lines carry
# their CARRIER SET (the files on each side). NEW findings fail (justify or
# single-source); CARRIER-SET drift on a baselined entry fails (a twin copied
# into one more file is a finding, not silence); STALE entries fail (one side
# changed alone, or an offender was fixed - update the sibling or delete the
# line). Neither passes forever by describing today.
#
# What this gate does NOT catch, and the discipline that still lives here
# because the graduated AGENTS.md entry is gone: text cannot see an operation
# implemented on N paths. Before trusting a guard, enumerate every path a
# caller can reach - in-process test, exec'd binary, skill layer, direct CLI,
# spawned worker - because a guard on only one of them reads as protection
# and ships green while the rest stay broken. The engines below catch the
# textual shapes; the enumeration is still the author's job.
#
# Run:  bash scripts/ci/check-reachable-paths.sh [--self-test|--dump-baseline]
# Exit: 0 clean, 1 findings or a control that did not fire, 2 misuse.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
BASELINE="${REPO_ROOT}/scripts/ci/reachable-paths-baseline.txt"

MODE="check"
case "${1:-}" in
  --self-test) MODE="self-test" ;;
  --dump-baseline) MODE="dump" ;;
  -h|--help) awk '/^set -uo pipefail/ {exit} {print}' "$0" | sed -e '$d'; exit 0 ;;
  "") ;;
  *) echo "check-reachable-paths: unknown argument: $1 (use --self-test or --dump-baseline)" >&2; exit 2 ;;
esac

run_scan() {
  # $1 = tree root, $2 = baseline file (may not exist yet), $3 = mode (check|dump)
  CHECK_ROOT="$1" CHECK_BASELINE="$2" CHECK_MODE="$3" python3 - <<'PYSCAN'
import fnmatch
import os
import re
import subprocess
import sys

ROOT = os.environ["CHECK_ROOT"]
MODE = os.environ.get("CHECK_MODE", "check")
BASELINE_PATH = os.environ.get("CHECK_BASELINE", "")

A_MIN = 24   # normalized chars for a twin literal
B_MIN = 60   # normalized chars for a duplicated prose line

# The auto-loaded set mirrors what check-preamble-budget.sh budgets:
# AGENTS.md, CLAUDE.md, using-fno, plus every .claude/rules/*.md (its
# nullglob loop). If that set changes, this must change in the same PR;
# two definitions of "auto-loaded" would be one more N-implementations
# instance.
AUTO_LOADED = ["AGENTS.md", "CLAUDE.md", "skills/using-fno/SKILL.md"]

# Engine R registry. Each entry: a site pattern that identifies ONE reachable
# path of an operation, and a required pattern the ENCLOSING FUNCTION must
# reference. Writer half and reader half are the same mechanical shape; the
# difference lives in the entry's semantics.
#
# Entries deliberately absent until their fix nodes land (the fix PR adds the
# entry, this list must never carry a pattern pinned to code that moved):
#   spawn-path-gate (dispatch second spawn path), pane-send-audit,
#   json-projection-keys, claim-release-on-closure.
REGISTRY = [
    {
        "name": "harness-env-scrub",
        "glob": "cli/src/fno/agents/harnesses/*.py",
        "site": r"dict\(os\.environ\)",
        "required": r"worker_environment|scrub_ambient_identity",
        "why": "every child-env build on a spawn path crosses the worker_environment identity-scrub floor (or scrubs directly)",
    },
    {
        "name": "status-derivation-helper",
        "glob": "cli/src/fno/graph/*.py",
        "site": r"def _(?:effective_)?status",
        "required": r"_apply_readiness_overlay|read_graph|load_graph",
        "why": "a status helper returning the stored field skips the read-time blocked derivation",
    },
    {
        "name": "provider-resolution",
        "glob": "cli/src/fno/agents/spawn_gate.py",
        "site": r"row\.provider",
        "required": r"resolve_provider|provider_for",
        "why": "a raw provider read bypasses the resolver; the resolver does not exist yet",
    },
]

def tracked(pattern):
    """Repo-relative posix paths matching a git ls-files pattern (real tree),
    or a walk of the fixture tree (CHECK_ROOT self-test mode)."""
    # A worktree carries .git as a FILE, not a dir; test existence, not type.
    if os.path.exists(os.path.join(ROOT, ".git")):
        out = subprocess.run(
            ["git", "ls-files", pattern], capture_output=True, text=True, cwd=ROOT
        ).stdout.split("\n")
        return [p for p in out if p]
    hits = []
    for dirpath, _dirs, files in os.walk(ROOT):
        for name in files:
            rel = os.path.relpath(os.path.join(dirpath, name), ROOT).replace(os.sep, "/")
            if fnmatch.fnmatch(rel, pattern):
                hits.append(rel)
    return sorted(hits)

def is_test_path(rel):
    base = rel.rsplit("/", 1)[-1]
    return (
        "/tests/" in rel
        or "/fixtures/" in rel
        or rel.startswith("tests/")
        or re.match(r"test_.*\.py$", base) is not None
        or re.match(r".*_test\.(py|rs)$", base) is not None
    )

def norm_literal(raw):
    s = re.sub(r"\\.", "", raw)
    s = re.sub(r"\s+", " ", s).strip().lower()
    return s

# Double quotes only, on both sides of the language pair. Known blind spot:
# a python twin phrased with single quotes is not scanned. Matching single
# quotes too is NOT a one-regex fix - an outer single-quoted string swallows
# the double-quoted literals inside it, and apostrophe prose pairs into junk
# twins. The real fix is tokenizing .py files (tokenize module) while keeping
# this regex for .rs; until then, write message twins double-quoted.
LITERAL_RE = re.compile(r'"((?:[^"\\\n]|\\.){%d,})"' % A_MIN)

def engine_a():
    langs = {}
    for kind, pat in (("py", "*.py"), ("rs", "*.rs")):
        bucket = {}
        for rel in tracked(pat):
            if is_test_path(rel):
                continue
            try:
                text = open(os.path.join(ROOT, rel), encoding="utf-8", errors="replace").read()
            except OSError:
                continue
            for match in LITERAL_RE.finditer(text):
                norm = norm_literal(match.group(1))
                if len(norm) >= A_MIN:
                    bucket.setdefault(norm, []).append(rel)
        langs[kind] = bucket
    twins = {}
    for norm in set(langs["py"]) & set(langs["rs"]):
        twins[norm] = (sorted(set(langs["py"][norm])), sorted(set(langs["rs"][norm])))
    return twins

def norm_prose(line):
    s = re.sub(r"[^a-z0-9 ]", " ", line.lower())
    return re.sub(r"\s+", " ", s).strip()

def engine_b():
    auto_lines = {}
    auto_files = AUTO_LOADED + tracked(".claude/rules/*.md")
    for rel in auto_files:
        path = os.path.join(ROOT, rel)
        if not os.path.isfile(path):
            continue
        for line in open(path, encoding="utf-8", errors="replace"):
            stripped = line.strip()
            if not stripped or stripped.startswith(("#", "<!--", "|--")):
                continue
            norm = norm_prose(stripped)
            if len(norm) >= B_MIN:
                auto_lines.setdefault(norm, rel)
    dups = {}
    for rel in tracked("*.md"):
        if rel in auto_files:
            continue
        for line in open(os.path.join(ROOT, rel), encoding="utf-8", errors="replace"):
            stripped = line.strip()
            if not stripped or stripped.startswith(("#", "<!--", "|--")):
                continue
            norm = norm_prose(stripped)
            if len(norm) >= B_MIN and norm in auto_lines:
                dups.setdefault(norm, set()).add(rel)
    return dups

def engine_r():
    """Returns (offenders, vacuous). offender key: name:file::function."""
    offenders = []
    vacuous = []
    for entry in REGISTRY:
        site_re = re.compile(entry["site"])
        req_re = re.compile(entry["required"])
        matches = 0
        for rel in tracked(entry["glob"]):
            path = os.path.join(ROOT, rel)
            try:
                lines = open(path, encoding="utf-8", errors="replace").read().splitlines()
            except OSError:
                continue
            spans = function_spans(lines)
            for i, line in enumerate(lines):
                # Comment text is not a site: a prose mention of the pattern
                # must neither create an offender nor keep a dead glob alive.
                if line.lstrip().startswith("#") or not site_re.search(line):
                    continue
                matches += 1
                fn, start, end = span_at(spans, i)
                body = "\n".join(lines[start:end])
                if not req_re.search(body):
                    offenders.append(f"{entry['name']}:{rel}::{fn or ''}")
        if matches == 0:
            vacuous.append(entry["name"])
    return sorted(set(offenders)), vacuous

def function_spans(lines):
    """Outermost def spans with class-body scope reset (nested defs fold into
    the enclosing def). `async def` parses like `def`. The module prologue
    before the first def is a span of its own (""), so a module-level site is
    judged rather than silently skipped - the mail-inject-callers rel:: key
    records its unattributed sites the same way."""
    spans = []
    fn, indent, start = "", None, None
    for i, line in enumerate(lines):
        if re.match(r"^class [A-Za-z_]", line):
            if fn:
                spans.append((fn, start, i))
            fn, indent, start = "", None, None
            continue
        m = re.match(r"^([ \t]*)(?:async[ \t]+)?def ([A-Za-z_]\w*)\b", line)
        if m and (indent is None or len(m.group(1)) <= indent):
            if fn:
                spans.append((fn, start, i))
            fn, indent, start = m.group(2), len(m.group(1)), i
    if fn:
        spans.append((fn, start, len(lines)))
    if spans:
        if spans[0][1] > 0:
            spans.insert(0, ("", 0, spans[0][1]))
    else:
        spans = [("", 0, len(lines))]
    return spans

def span_at(spans, idx):
    for fn, start, end in spans:
        if start <= idx < end:
            return fn, start, end
    return None, idx, idx + 1

def load_baseline():
    """entries[engine][key] = (meta, reason). A/B lines carry a carrier meta
    column (A: `py=...;rs=...`, B: `in=...`) so the ratchet holds the CARRIER
    SET, not just first occurrence: a baselined twin copied into one more file
    is a finding, not silence. R lines have no meta (the key names the site).
    """
    entries = {"A": {}, "B": {}, "R": {}}
    if not BASELINE_PATH or not os.path.isfile(BASELINE_PATH):
        return entries, []
    malformed = []
    for raw in open(BASELINE_PATH, encoding="utf-8"):
        line = raw.rstrip("\n")
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = line.split("\t")
        key, ident = parts[0], parts[1] if len(parts) > 1 else ""
        if key in ("A", "B"):
            if (
                len(parts) < 4
                or not parts[3].strip().startswith("#")
                or not parts[3].strip()[1:].strip()
            ):
                malformed.append(line)
                continue
            entries[key][ident] = (parts[2].strip(), parts[3].strip())
        elif key == "R":
            if (
                len(parts) < 3
                or not parts[2].strip().startswith("#")
                or not parts[2].strip()[1:].strip()
            ):
                malformed.append(line)
                continue
            entries[key][ident] = ("", parts[2].strip())
        else:
            malformed.append(line)
    return entries, malformed

def main():
    twins = engine_a()
    dups = engine_b()
    offenders, vacuous = engine_r()
    base, malformed = load_baseline()

    if MODE == "dump":
        for norm in sorted(twins):
            py, rs = twins[norm]
            meta = "py=" + "|".join(py) + ";rs=" + "|".join(rs)
            print(f"A\t{norm}\t{meta}\t# CLASSIFY protocol|msg-twin|data")
        for norm in sorted(dups):
            meta = "in=" + "|".join(sorted(dups[norm]))
            print(f"B\t{norm}\t{meta}\t# CLASSIFY pointer|restated")
        for off in sorted(offenders):
            print(f"R\t{off}\t# offender - carry a reason or fix in this PR")
        return 0

    def a_meta(norm):
        py, rs = twins[norm]
        return "py=" + "|".join(py) + ";rs=" + "|".join(rs)

    def b_meta(norm):
        return "in=" + "|".join(sorted(dups[norm]))

    a_new = [n for n in twins if n not in base["A"]]
    a_drift = [n for n in twins if n in base["A"] and base["A"][n][0] != a_meta(n)]
    a_stale = [n for n in base["A"] if n not in twins]
    b_new = [n for n in dups if n not in base["B"]]
    b_drift = [n for n in dups if n in base["B"] and base["B"][n][0] != b_meta(n)]
    b_stale = [n for n in base["B"] if n not in dups]
    r_new = [o for o in offenders if o not in base["R"]]
    r_stale = [o for o in base["R"] if o not in offenders]

    findings = []
    for n in sorted(a_new):
        findings.append(f"A new twin literal (in both .py and .rs): {n[:100]}  {a_meta(n)}")
    for n in sorted(a_drift):
        findings.append(
            f"A carrier set changed for a baselined twin: {n[:100]}  "
            f"baseline {base['A'][n][0]}  now {a_meta(n)}  "
            f"(update the baseline in the same PR, or single-source)"
        )
    for n in sorted(a_stale):
        # Echo the recorded meta so CI names the sibling pair the developer
        # did NOT touch (the header's stated contract).
        recorded = base["A"].get(n, ("", ""))[1]
        suffix = f"  recorded: {recorded}" if recorded else ""
        findings.append(f"A stale baseline entry (no longer a twin - one side changed alone): {n[:100]}{suffix}")
    for n in sorted(b_new):
        findings.append(f"B auto-loaded prose restated in {b_meta(n)}: {n[:100]}")
    for n in sorted(b_drift):
        findings.append(
            f"B carrier set changed for a baselined duplication: {n[:100]}  "
            f"baseline {base['B'][n][0]}  now {b_meta(n)}"
        )
    for n in sorted(b_stale):
        findings.append(f"B stale baseline entry (duplication gone): {n[:100]}")
    for o in sorted(r_new):
        why = next((e["why"] for e in REGISTRY if o.startswith(e["name"] + ":")), "")
        findings.append(f"R offender {o} - site matches, enclosing function lacks the required reference ({why})")
    for o in sorted(r_stale):
        findings.append(f"R stale baseline entry (offender fixed or moved): {o}")
    for name in vacuous:
        findings.append(f"R vacuous registry entry (site pattern matches nothing): {name}")
    for line in malformed:
        findings.append(
            f"malformed baseline line (A/B need ENGINE<TAB>id<TAB>meta<TAB># reason; R needs ENGINE<TAB>id<TAB># reason): {line[:100]}"
        )

    if findings:
        print("check-reachable-paths: findings:", file=sys.stderr)
        for f in findings:
            print(f"  {f}", file=sys.stderr)
        print(
            "  New twin/dup/offender: single-source it, or add a baseline line with a reason.",
            file=sys.stderr,
        )
        print(
            "  Stale entry: one side changed alone - update the sibling, or delete the line.",
            file=sys.stderr,
        )
        return 1

    print(
        f"check-reachable-paths: OK "
        f"(A twins={len(twins)} B dups={len(dups)} R offenders={len(offenders)} all baselined)"
    )
    return 0

sys.exit(main())
PYSCAN
}

self_test() {
  local tmp
  tmp="$(mktemp -d)"
  # Expand now: a single-quoted trap body would re-read $tmp at script exit,
  # after this local is gone, and die "unbound variable" under set -u.
  trap "rm -rf \"$tmp\"" EXIT

  # Compliant sites for the registry globs this fixture does not exercise,
  # so only the canary entry reports. Same content reused by the clean tree.
  write_compliant_graph() {
    mkdir -p "$1/cli/src/fno/graph"
    printf 'def _status_of(c):\n    entries = read_graph()\n    return entries\n' > "$1/cli/src/fno/graph/render.py"
  }
  write_compliant_gate() {
    mkdir -p "$1/cli/src/fno/agents"
    printf 'def lane_provider(row):\n    return resolve_provider(row.provider)\n' > "$1/cli/src/fno/agents/spawn_gate.py"
  }

  # Canary fixture: one violation per engine.
  mkdir -p "$tmp/canary/cli/src/fno/agents/harnesses" "$tmp/canary/docs"
  printf 'The relay compression contract line that must never be restated elsewhere in the tree.\n' > "$tmp/canary/AGENTS.md"
  printf 'Docs page.\nThe relay compression contract line that must never be restated elsewhere in the tree.\n' > "$tmp/canary/docs/other.md"
  printf 'X = "twin canary literal long enough to clear the threshold"\n' > "$tmp/canary/a.py"
  printf 'const S: &str = "twin canary literal long enough to clear the threshold";\n' > "$tmp/canary/b.rs"
  printf 'def build_child_env():\n    spawn_env = dict(os.environ)\n    return spawn_env\n' > "$tmp/canary/cli/src/fno/agents/harnesses/canary.py"
  write_compliant_graph "$tmp/canary"
  write_compliant_gate "$tmp/canary"

  : > "$tmp/empty-baseline.txt"

  local out
  out="$(run_scan "$tmp/canary" "$tmp/empty-baseline.txt" check 2>&1)"
  # Tool control: every engine must FIRE on its canary. An absence-only pass
  # has two explanations; only these strings distinguish "clean" from "blind".
  for marker in "A new twin literal" "B auto-loaded prose" "R offender"; do
    if ! grep -q "$marker" <<<"$out"; then
      echo "check-reachable-paths self-test: engine did not fire on its canary: $marker" >&2
      echo "$out" >&2
      return 1
    fi
  done

  # Clean control: same tree, violations removed - all engines go quiet.
  mkdir -p "$tmp/clean/cli/src/fno/agents/harnesses" "$tmp/clean/docs"
  printf 'The relay compression contract line that must never be restated elsewhere in the tree.\n' > "$tmp/clean/AGENTS.md"
  printf 'Docs page, no restatement.\n' > "$tmp/clean/docs/other.md"
  printf 'X = "twin canary literal long enough to clear the threshold"\n' > "$tmp/clean/a.py"
  printf 'const S: &str = "a different rust literal that matches no python string here";\n' > "$tmp/clean/b.rs"
  printf 'from fno.harness_identity import scrub_ambient_identity\ndef build_child_env():\n    spawn_env = dict(os.environ)\n    scrub_ambient_identity(spawn_env)\n    return spawn_env\n' > "$tmp/clean/cli/src/fno/agents/harnesses/canary.py"
  write_compliant_graph "$tmp/clean"
  write_compliant_gate "$tmp/clean"

  out="$(run_scan "$tmp/clean" "$tmp/empty-baseline.txt" check 2>&1)"
  # Finding lines carry a two-space indent; the OK summary does not. Matching
  # bare keywords would hit "R offenders=0" in the summary itself.
  if grep -qE "^  " <<<"$out"; then
    echo "check-reachable-paths self-test: clean control reported findings:" >&2
    echo "$out" >&2
    return 1
  fi

  # Stale control: a baseline entry whose twin no longer exists must fail.
  printf 'A\tnot a twin anywhere in this tree any more\tpy=x.py;rs=y.rs\t# reason\n' > "$tmp/stale-baseline.txt"
  out="$(run_scan "$tmp/clean" "$tmp/stale-baseline.txt" check 2>&1)"
  grep -q "A stale baseline entry" <<<"$out" || {
    echo "check-reachable-paths self-test: stale-entry control did not fire" >&2
    echo "$out" >&2
    return 1
  }

  # Carrier-drift control: a baselined twin whose carrier set changed (a new
  # file copied it, or a carrier was removed) must fail - not pass silently.
  printf 'A\ttwin canary literal long enough to clear the threshold\tpy=other.py;rs=b.rs\t# reason\n' > "$tmp/drift-baseline.txt"
  out="$(run_scan "$tmp/canary" "$tmp/drift-baseline.txt" check 2>&1)"
  grep -q "carrier set changed" <<<"$out" || {
    echo "check-reachable-paths self-test: carrier-drift control did not fire" >&2
    echo "$out" >&2
    return 1
  }

  # Vacuous control: a registry entry whose glob matches nothing must fail.
  mkdir -p "$tmp/noharness"
  printf 'X = 1\n' > "$tmp/noharness/a.py"
  out="$(run_scan "$tmp/noharness" "$tmp/empty-baseline.txt" check 2>&1)"
  grep -q "vacuous registry entry" <<<"$out" || {
    echo "check-reachable-paths self-test: vacuous-entry control did not fire" >&2
    echo "$out" >&2
    return 1
  }

  echo "check-reachable-paths self-test: OK (canaries fired, clean/stale/drift/vacuous controls held)"
  return 0
}

case "$MODE" in
  self-test) self_test ;;
  dump) run_scan "$REPO_ROOT" "$BASELINE" dump ;;
  check)
    if [[ ! -f "$BASELINE" ]]; then
      echo "check-reachable-paths: baseline missing at $BASELINE (generate with --dump-baseline)" >&2
      exit 2
    fi
    run_scan "$REPO_ROOT" "$BASELINE" check
    ;;
esac
