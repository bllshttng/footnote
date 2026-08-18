#!/usr/bin/env bash
# check-preamble-budget.sh - CI gate for footnote-owned SessionStart markdown.
#
# Run: bash scripts/ci/check-preamble-budget.sh [--quiet] [--json] [repo-root]
#       bash scripts/ci/check-preamble-budget.sh --injections <N> [repo-root]
# Default root is the current directory. Exits 0 at or below the byte ceiling
# and exits 1 when discovery fails or the measured preamble exceeds it.
# --injections <N> adds a directional per-session cost report (each file's bytes
# times N) on top of the single-copy gate; it never changes the exit code, since
# the per-session injection count is session-length-driven, not a constant.

set -euo pipefail

# Raised from 37326 by 89 bytes for delivery_completion DoneDelivery terminal
# documentation and activation logic in AGENTS.md.
# Raised from 37415 to 38000 by two changes that landed together. The
# citizens-vs-limbs spawn-primitive contract in skills/using-fno/SKILL.md
# (x-af92) took 585 bytes: its value IS the SessionStart injection, the long
# form lives out-of-preamble in docs/architecture/coordination.md, and the
# prior preamble had 8B headroom so any meaningful contract required a raise.
# The DoneUnreviewed terminal in the ship-vocabulary line took 45 more, since
# the auto-loaded preamble must name every public TerminationReason.
# Raised from 38000 to 38100 by the prescribed-verbs fix. AGENTS.md's
# search line prescribed `rg -uu` as the over-exclusion remedy, which is inert:
# the trap has two layers (a ripgreprc `--glob=!target` and an unanchored
# `target/` in ~/.ignore), and `-u` governs ignore files, never a `--glob`. The
# working remedy `RIPGREP_CONFIG_PATH= rg -uu` plus a one-clause why is longer
# than the inert form it replaces, and the preamble sat at 37999/38000 (1B spare).
# Raised by a further 300 for the stage-table pointer in AGENTS.md, which names
# config.agents.profiles.<verb> and links the role-based-model-routing doc: the
# per-stage axis is undiscoverable from the preamble without it (~66 tok/turn).
#
# LOWERED from 38400 to 36726, the first lowering in this file's history. Every
# entry above raises it; five raises and no cut is how the preamble reached 55
# bytes of headroom with no commit to blame. .claude/rules/worktrees.md gave up
# 2619 bytes (6839 -> 4220) by disclosing hook internals, removal, and the
# enforcement wiring to docs/architecture/worktree-mechanics.md, and this line
# banks that cut instead of leaving it as headroom for the next five edits.
# 36726 is the measured 35726 plus half the band, so a later cut and a later
# growth get the same room. An intermediate 37400 was written here first and
# this comment kept naming it after the value moved, which left the only audit
# trail for this constant 674 bytes wrong.
# The gate is now two-sided (see RATCHET_NUDGE_BYTES below): a cut large enough
# to push spare past the band fails until the ceiling follows it down.
# RAISED to 36830, the minimum that fits the measured text, deliberately with
# ZERO spare rather than a round number. The corpus header says to fund growth
# by trading bytes here and never by raising this. That instruction assumes
# tradeable restatement exists in the pitfalls corpus. It was measured and it
# does not: 181 honest bytes in 6089, across three compression passes and two
# external reviews, with every byte past them carrying a claim. Successive
# estimates of the same cut read 548, then 445, then 302, then 181, each one
# revised down as a structural preservation check missed qualifiers that a
# word-level diff and two reviewers caught. Eleven were recovered.
#
# The header governs the CORPUS and exists to stop a new pitfalls entry buying
# room by moving this number. Main's overage came from documenting two new
# verbs, not from corpus growth, so the rule was never aimed at this case.
# A rule whose stated premise is false gets changed deliberately.
#
# Zero spare is the point: the next preamble byte of any kind fails this gate
# and has to fund itself. Do not read 36830 as room.
CEILING_BYTES=36830
# The working band under the ceiling. Spare above this fails the gate and names
# the value to write, so a cut is banked in the same PR that makes it rather
# than becoming headroom.
#
# Set the ceiling at measured + band/2, so both directions get the same room.
# Sitting just under the band instead leaves almost no slack for a cut, and the
# gate then fires on the very edits it wants to encourage; sitting at
# measured + band leaves none at all, since any cut at all trips it.
RATCHET_NUDGE_BYTES=2000

# Second budget: the `description` of every model-invoked skill and agent. These
# are context pointers the harness holds on every turn to decide what to fire.
# Measured 14690 B: 6750 across 22 skills and 7940 across 17 agents, against
# Matt Pocock's 3114 B across 15 skills.
# The agent half was nearly missed three times. It sat outside the file set
# entirely; four agents write their description as a YAML block scalar, so a
# reader that measured the `>` marker scores those at 1 byte instead of ~490;
# and agents/verifier.md wrote an unquoted value spanning 18 lines, which a
# line-based reader scores at 114 B while it really carries 704.
# Two-sided from the start on the same reasoning as the file ceiling above: a
# one-sided ceiling is the thing that only ever ratchets up.
DESCRIPTIONS_CEILING_BYTES=15440
DESCRIPTIONS_BAND_BYTES=1500
QUIET=0
JSON_MODE=0
REPO_ROOT="."
REPO_ROOT_SET=0
INJECTIONS=1

# Pre-extract --injections <N> (a value option) before the flag loop.
FILTERED=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --injections)
      [[ $# -ge 2 ]] || { echo "check-preamble-budget: --injections requires an integer" >&2; exit 1; }
      INJECTIONS="$2"
      shift 2
      ;;
    *)
      FILTERED+=("$1")
      shift
      ;;
  esac
done
set -- ${FILTERED[@]+"${FILTERED[@]}"}

for arg in "$@"; do
  case "$arg" in
    -q|--quiet)
      QUIET=1
      ;;
    --json)
      JSON_MODE=1
      ;;
    -*)
      echo "check-preamble-budget: unknown option: $arg" >&2
      exit 1
      ;;
    *)
      if (( REPO_ROOT_SET )); then
        echo "check-preamble-budget: expected at most one repo root" >&2
        exit 1
      fi
      REPO_ROOT="$arg"
      REPO_ROOT_SET=1
      ;;
  esac
done

if (( QUIET && JSON_MODE )); then
  echo "check-preamble-budget: --quiet and --json are mutually exclusive" >&2
  exit 1
fi

if [[ ! "$CEILING_BYTES" =~ ^[0-9]+$ ]]; then
  echo "check-preamble-budget: CEILING_BYTES must be a non-negative integer" >&2
  exit 1
fi

if [[ ! "$INJECTIONS" =~ ^[0-9]+$ ]] || (( INJECTIONS < 1 )); then
  echo "check-preamble-budget: --injections must be a positive integer" >&2
  exit 1
fi

[[ -d "$REPO_ROOT" ]] || {
  echo "check-preamble-budget: repo root not found: $REPO_ROOT" >&2
  exit 1
}

# The two-sided band asks whether THIS repo's ceiling still tracks THIS repo's
# content, so it applies only when the gate measures its own tree. A fixture
# root passed as an argument is not what the constants budget, and holding it
# to them would make every small fixture look like an unbanked cut. The
# over-ceiling half still applies to every root, fixtures included.
# PREAMBLE_BAND_FORCE=1 applies the band to any root. It exists so the two
# failure paths below are reachable from a fixture: a gate branch no test can
# enter is the decorative-guard shape this file is here to refuse.
SCRIPT_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
MEASURED_ROOT="$(cd "$REPO_ROOT" && pwd -P)"
BAND_APPLIES=0
[[ "$MEASURED_ROOT" == "$SCRIPT_REPO_ROOT" ]] && BAND_APPLIES=1
[[ "${PREAMBLE_BAND_FORCE:-}" == "1" ]] && BAND_APPLIES=1

FILES=(
  "$REPO_ROOT/AGENTS.md"
  "$REPO_ROOT/CLAUDE.md"
  "$REPO_ROOT/skills/using-fno/SKILL.md"
)

for fixed in "${FILES[@]}"; do
  [[ -f "$fixed" ]] || {
    echo "check-preamble-budget: required file not found: ${fixed#"$REPO_ROOT"/}" >&2
    exit 1
  }
done

shopt -s nullglob
for rule in "$REPO_ROOT"/.claude/rules/*.md; do
  FILES+=("$rule")
done
shopt -u nullglob

TOTAL_BYTES=0
RECORDS=""
MANIFEST_RECORDS=""
hash_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$1"
  fi
}
for path in "${FILES[@]}"; do
  relative="${path#"$REPO_ROOT"/}"
  [[ -f "$path" ]] || {
    echo "check-preamble-budget: discovered path is not a regular file: $relative" >&2
    exit 1
  }
  [[ -r "$path" ]] || {
    echo "check-preamble-budget: discovered file is not readable: $relative" >&2
    exit 1
  }
  bytes=$(LC_ALL=C wc -c < "$path")
  bytes=$((bytes))
  content_hash=$(hash_file "$path")
  TOTAL_BYTES=$((TOTAL_BYTES + bytes))
  RECORDS+="${bytes}"$'\t'"${relative}"$'\n'
  MANIFEST_RECORDS+="${bytes}"$'\t'"${content_hash}"$'\t'"${relative}"$'\n'
done

APPROX_TOKENS=$((TOTAL_BYTES / 4))
SPARE_BYTES=$((CEILING_BYTES - TOTAL_BYTES))
APPROX_TOKEN_K=$(awk -v bytes="$TOTAL_BYTES" 'BEGIN { printf "%.1f", bytes / 4000 }')

# Model-invoked skill descriptions are always-loaded too: the harness holds every
# one in context on every turn so it can decide what to fire. They sit outside
# FILES, so before this block the gate measured ~85% of the always-loaded text
# and reported it as the whole. Budgeted separately because they are a different
# surface with a different cure - a description is a context pointer, tuned by
# sharpening its wording, not by trading bytes against AGENTS.md.
DESC_RECORDS="$(python3 - "$REPO_ROOT" <<'PY'
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
sources = sorted(root.glob("skills/*/SKILL.md")) + sorted(root.glob("agents/*.md"))
rows = []
for path in sources:
    text = path.read_text(encoding="utf-8")
    m = re.match(r"^---\n(.*?)\n---\n", text, re.S)
    if not m:
        continue
    block = m.group(1)
    if re.search(r"^disable-model-invocation:\s*true\s*$", block, re.M):
        continue
    lines = block.split("\n")
    idx = next(
        (i for i, line in enumerate(lines) if re.match(r"^description:", line)), None
    )
    if idx is None:
        continue
    raw = lines[idx].split(":", 1)[1].strip()
    # A block scalar (| or >) carries its text on the following indented lines.
    # Measuring the marker instead would count 1 byte for a 700-byte pointer,
    # which is the silent undercount this budget exists to prevent. Four agents
    # in this repo are written that way, so refusing them is not an option.
    if raw.startswith(("|", ">")):
        folded = raw.startswith(">")
        body = []
        for line in lines[idx + 1:]:
            if line.strip() and not line.startswith((" ", "\t")):
                break
            body.append(line.strip())
        while body and not body[-1]:
            body.pop()
        raw = (" " if folded else "\n").join(body)
    elif len(raw) >= 2 and raw[0] == raw[-1] and raw[0] in "\"'":
        quote = raw[0]
        raw = raw[1:-1]
        if quote == '"':
            raw = raw.replace('\\"', '"')
    label = path.parent.name if path.name == "SKILL.md" else f"agent:{path.stem}"
    rows.append((len(raw.encode("utf-8")), label))
for size, name in sorted(rows, reverse=True):
    print(f"{size}\t{name}")
PY
)" || exit 1

DESC_TOTAL=0
DESC_COUNT=0
while IFS=$'\t' read -r bytes name; do
  [[ -z "$name" ]] && continue
  DESC_TOTAL=$((DESC_TOTAL + bytes))
  DESC_COUNT=$((DESC_COUNT + 1))
done <<< "$DESC_RECORDS"
DESC_SPARE=$((DESCRIPTIONS_CEILING_BYTES - DESC_TOTAL))
# Sums the two budgets. skills/using-fno/SKILL.md contributes to both, and that
# is not a double count: the harness injects that file's whole body (frontmatter
# included) at SessionStart AND lists its description in the skill registry, so
# those bytes really are in context twice. Verified by reading both in a live
# session's context. This figure covers what the gate measures, which is not a
# claim to cover everything a harness injects.
MEASURED_ALWAYS_LOADED=$((TOTAL_BYTES + DESC_TOTAL))

# Independent cross-check on the reader. Two properties make it independent
# rather than a second copy of the same mistake:
#   - it enumerates by find at ANY depth, where the reader globs depth-1 only,
#     so a skill nested at skills/<group>/<name>/SKILL.md makes the two DISAGREE
#     instead of both silently missing it;
#   - it matches by grep, not by frontmatter parsing, so a fence the reader's
#     regex cannot open (a BOM, a reordered block) still shows up here.
# The comparison is equality, not "is it zero". A partial reader failure leaves
# a non-zero count, so a zero-only check would pass while dropping bytes - the
# absence-shaped condition this budget exists to refuse.
EXPECT_DESC=0
while IFS= read -r candidate; do
  [[ -z "$candidate" ]] && continue
  # Read the frontmatter slice only. Grepping the whole file made a BODY line
  # at column zero (a doc that shows `disable-model-invocation: true` as an
  # example) change this count, so the two readers disagreed over a file
  # neither had mis-parsed and the failure blamed a nested skill. awk here
  # rather than the reader's own list, so the enumerations stay independent.
  front="$(awk 'NR==1 && $0!="---"{exit} NR==1{next} /^---$/{exit} {print}' "$candidate")"
  grep -qE '^description:' <<< "$front" || continue
  grep -qE '^disable-model-invocation:[[:space:]]*true' <<< "$front" && continue
  EXPECT_DESC=$((EXPECT_DESC + 1))
done < <(
  { find "$REPO_ROOT/skills" -name 'SKILL.md' -type f 2>/dev/null || true; } \
  ; { find "$REPO_ROOT/agents" -maxdepth 1 -name '*.md' -type f 2>/dev/null || true; }
)

if (( EXPECT_DESC != DESC_COUNT )); then
  {
    echo "check-preamble-budget: the two readers disagree - grep sees ${EXPECT_DESC} model-invoked descriptions, the frontmatter reader parsed ${DESC_COUNT}."
    echo "  Neither number is trustworthy while they differ. Likely causes: a skill"
    echo "  nested below skills/<name>/SKILL.md (the reader globs depth-1 only), or"
    echo "  a frontmatter block the reader's regex cannot open."
  } >&2
  exit 1
fi

if (( JSON_MODE )); then
  printf '%s' "$MANIFEST_RECORDS" | python3 -c '
import json
import sys

total = int(sys.argv[1])
ceiling = int(sys.argv[2])
sources = []
for raw in sys.stdin:
    raw = raw.rstrip("\n")
    if not raw:
        continue
    size, content_hash, path = raw.split("\t", 2)
    size = int(size)
    sources.append({
        "path": path,
        "bytes": size,
        "estimated_tokens": (size + 3) // 4,
        "content_hash": content_hash,
    })
descriptions = []
for raw in sys.argv[5].splitlines():
    if not raw.strip():
        continue
    size, name = raw.split("\t", 1)
    descriptions.append({"skill": name, "bytes": int(size)})
print(json.dumps({
    "total_bytes": total,
    "estimated_tokens": (total + 3) // 4,
    "ceiling_bytes": ceiling,
    "sources": sources,
    "descriptions": {
        "total_bytes": int(sys.argv[3]),
        "ceiling_bytes": int(sys.argv[4]),
        "count": len(descriptions),
        "skills": descriptions,
    },
    "measured_always_loaded_bytes": total + int(sys.argv[3]),
}, separators=(",", ":")))
' "$TOTAL_BYTES" "$CEILING_BYTES" "$DESC_TOTAL" "$DESCRIPTIONS_CEILING_BYTES" "$DESC_RECORDS"
elif (( QUIET )); then
  echo "preamble: ${TOTAL_BYTES} / ${CEILING_BYTES} B (~${APPROX_TOKEN_K}K tok/turn); descriptions: ${DESC_TOTAL} / ${DESCRIPTIONS_CEILING_BYTES} B"
else
  if (( SPARE_BYTES >= 0 )); then
    echo "check-preamble-budget: ${TOTAL_BYTES} / ${CEILING_BYTES} bytes (~${APPROX_TOKENS} tok at 4 B/tok), ${SPARE_BYTES} to spare"
  else
    echo "check-preamble-budget: ${TOTAL_BYTES} / ${CEILING_BYTES} bytes (~${APPROX_TOKENS} tok at 4 B/tok), $((-SPARE_BYTES)) over"
  fi

  while IFS=$'\t' read -r bytes relative; do
    [[ -z "$relative" ]] && continue
    marker=""
    [[ "$relative" == "skills/using-fno/SKILL.md" ]] && marker="  [shipped to every consumer]"
    printf '  %8d  %s%s\n' "$bytes" "$relative" "$marker"
  done < <(printf '%s' "$RECORDS" | LC_ALL=C sort -rn -k1,1)

  if (( DESC_COUNT > 0 )); then
    echo
    echo "  descriptions (always-loaded skill + agent pointers): ${DESC_TOTAL} / ${DESCRIPTIONS_CEILING_BYTES} bytes across ${DESC_COUNT} model-invoked skills and agents, ${DESC_SPARE} to spare"
    while IFS=$'\t' read -r bytes name; do
      [[ -z "$name" ]] && continue
      printf '  %8d  %s\n' "$bytes" "$name"
    done <<< "$DESC_RECORDS"
    echo "  measured always-loaded (these two budgets): ${MEASURED_ALWAYS_LOADED} bytes (~$((MEASURED_ALWAYS_LOADED / 4)) tok/turn)"
  fi

  if (( INJECTIONS > 1 )); then
    echo
    echo "  per-injection (directional, count=${INJECTIONS}): a harness re-injects"
    echo "  the preamble on every world-state refresh and compaction, so the real"
    echo "  per-session cost scales with the count. Re-measure it per session; it"
    echo "  is session-length-driven, not a constant. Gate ceiling stays single-copy."
    while IFS=$'\t' read -r bytes relative; do
      [[ -z "$relative" ]] && continue
      printf '  %8d  %s\n' "$(( bytes * INJECTIONS ))" "$relative"
    done < <(printf '%s' "$RECORDS" | LC_ALL=C sort -rn -k1,1)
    printf '  %8d  total at %d injections\n' "$(( TOTAL_BYTES * INJECTIONS ))" "$INJECTIONS"
  fi
fi

DESC_FAILED=0
if (( DESC_COUNT == 0 )); then
  # No model-invoked skills in this tree (fixture repos, or a consumer that
  # vendors the rules without the skills). Nothing to budget. The reader-broke
  # case is already refused above, so this branch cannot hide a failure.
  :
elif (( DESC_TOTAL > DESCRIPTIONS_CEILING_BYTES )); then
  # Every failure below writes its fix line in EVERY mode. --quiet and --json
  # shape stdout; a caller that gets exit 1 with no reason has to go read the
  # script to learn what to change, and the named value is the whole point of a
  # two-sided band. Stderr does not corrupt the JSON document on stdout.
  DESC_FAILED=1
  {
    echo "check-preamble-budget: descriptions are ${DESC_TOTAL} bytes, $((DESC_TOTAL - DESCRIPTIONS_CEILING_BYTES)) over the ${DESCRIPTIONS_CEILING_BYTES}-byte ceiling."
    echo "  A description is a context pointer held in context on every turn."
    echo "  Fix: sharpen the wording (front-load the leading word, one trigger per"
    echo "  branch, cut identity the body already carries), or set"
    echo "  DESCRIPTIONS_CEILING_BYTES in this script with the reason in the PR body."
  } >&2
elif (( BAND_APPLIES && DESC_SPARE > DESCRIPTIONS_BAND_BYTES )); then
  DESC_FAILED=1
  {
    echo "check-preamble-budget: descriptions are ${DESC_TOTAL} bytes, leaving ${DESC_SPARE} spare, more than the ${DESCRIPTIONS_BAND_BYTES}-byte band."
    echo "  Fix: set DESCRIPTIONS_CEILING_BYTES=$((DESC_TOTAL + DESCRIPTIONS_BAND_BYTES / 2)) in this script, in this PR."
  } >&2
fi

# DESC_FAILED is carried, not exited on, so a PR that busts BOTH budgets sees
# both remediations in one run. Exiting here printed the descriptions fix and
# swallowed the file ceiling's "Largest:" guidance, which cost a second red CI
# to read advice the same run already had in hand.
if (( TOTAL_BYTES <= CEILING_BYTES )); then
  # Two-sided: under the ceiling by more than the band is also a failure. This
  # used to print an advisory and exit 0, which meant no cut was ever banked -
  # CEILING_BYTES was raised five times and lowered never, so each cut silently
  # became room for the next growth. The message names the value to write so
  # the fix is a paste, not arithmetic.
  if (( BAND_APPLIES && SPARE_BYTES > RATCHET_NUDGE_BYTES )); then
    SUGGESTED=$((TOTAL_BYTES + RATCHET_NUDGE_BYTES / 2))
    {
      echo "check-preamble-budget: ${TOTAL_BYTES} / ${CEILING_BYTES} bytes leaves ${SPARE_BYTES} spare, more than the ${RATCHET_NUDGE_BYTES}-byte band."
      echo "  The preamble shrank and the ceiling did not follow it down, so the cut"
      echo "  is unbanked: it stays available as headroom for the next growth."
      echo "  Fix: set CEILING_BYTES=${SUGGESTED} in this script, in this PR."
    } >&2
    exit 1
  fi
  exit "$DESC_FAILED"
fi

OVERAGE=$((TOTAL_BYTES - CEILING_BYTES))
OVERAGE_TOKENS=$(((OVERAGE + 3) / 4))

if (( ! QUIET && ! JSON_MODE )); then
  LARGEST=""
  count=0
  while IFS=$'\t' read -r bytes relative; do
    [[ -z "$relative" ]] && continue
    [[ -n "$LARGEST" ]] && LARGEST+=", "
    LARGEST+="${relative} ${bytes}"
    count=$((count + 1))
    (( count == 3 )) && break
  done < <(printf '%s' "$RECORDS" | LC_ALL=C sort -rn -k1,1)

  {
    echo "check-preamble-budget: ${TOTAL_BYTES} bytes exceeds the ${CEILING_BYTES}-byte ceiling by ${OVERAGE} (~${OVERAGE_TOKENS} tok/turn)."
    echo "  Largest: ${LARGEST}"
    echo
    echo "  Every byte here is re-read on every turn of every session on every lane."
    echo "  Fix, in order of preference:"
    echo "    1. Trade: cut an equivalent amount from the same file."
    echo "    2. Move it out of the preamble: docs/ and linked rule files that the"
    echo "       harness does not auto-load are not paid at startup."
    echo "    3. Raise CEILING_BYTES in this script, in this PR, with the reason in the PR body."
  } >&2
fi
exit 1
