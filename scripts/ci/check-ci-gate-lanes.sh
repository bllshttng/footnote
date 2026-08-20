#!/usr/bin/env bash
# check-ci-gate-lanes.sh - a gate nobody runs is not a gate.
#
# Every shell script in scripts/ci carries a row in ci-gate-lanes.tsv naming
# the file that RUNS it and the lane it fires in. This gate diffs the manifest
# against the live directory, the same shape check-workflow-manifest.sh uses
# for workflow files, and adds the assertion that manifest cannot have: the
# declared invoker must really invoke the script. A row that names an invoker
# which only MENTIONS the file is the decorative case this exists to catch.
#
# Why a machine has to answer this. On 2026-08-20 a hand grep of the same
# question named six unreachable guards. One was right, four were reachable
# through cli/src/fno/test_cmd.py, and a sixth file that really was unreachable
# was missed. Reachability is not a question a reviewer can be asked to
# re-derive.
#
# Run:  bash scripts/ci/check-ci-gate-lanes.sh [--self-test]
# Exit: 0 every row holds, 1 a violation, 2 misuse.

set -uo pipefail

REPO_ROOT="${CI_GATE_LANES_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
MANIFEST="$REPO_ROOT/scripts/ci/ci-gate-lanes.tsv"
GATE_DIR="$REPO_ROOT/scripts/ci"

# ONE pattern, used by both halves of this gate. That is deliberate: the
# declared-invoker check and the undeclared-invoker scan must agree by
# construction. When they drifted apart (the scan unanchored, the check
# anchored), an `sh` inside an ordinary word matched for one and not the other,
# and the two halves demanded contradictory rows that no manifest edit could
# satisfy.
#
# Two invocation shapes are recognized. An interpreter word followed by a path
# covers `bash scripts/ci/x.sh`, `. scripts/ci/x.sh` and `bash "$HERE/x.sh"`.
# A leading `./` covers the direct-exec form, which matters because every gate
# in scripts/ci is chmod +x, so `- run: ./scripts/ci/x.sh` is a real lane that
# was previously unrecordable.
SQ="'"
LEAD="(^|[[:space:];&|(\"$SQ])"
RUN="((bash|sh|source|\\.)[[:space:]]+([^[:space:];&|]*[/\"$SQ])?|\\./([^[:space:];&|]*/)?)"

FAIL=0
note_fail() { echo "ci-gate-lanes: $*" >&2; FAIL=1; }

# invokes <file> <basename> - true when a non-comment line in <file> RUNS the
# script. Three misses this pattern exists to refuse, each one found while
# measuring the live tree:
#   a bare \bsh\b matches the ".sh" extension, so every line naming any shell
#   script reads as an invocation;
#   a `paths:` filter entry names the file and runs nothing;
#   "bash tests/ci/test_preflight.sh" is not an invocation of preflight.sh, so
#   the path prefix must end in / or a quote, or be absent entirely.
#
# No pipeline here, deliberately. The first draft was
#   grep -v '^[[:space:]]*#' "$file" | grep -Eq "$re"
# and under `set -o pipefail` it reported 0, 0, 0, 2, 1 violations across five
# identical runs: `grep -q` exits on the first match, the upstream grep takes
# SIGPIPE, and pipefail promotes 141 to the status of the whole test. An
# intermittently lying guard is worse than no guard, and this file exists to
# make that class visible. One grep, then a plain loop.
invokes() {
    local file="$1" name="$2" esc matched ln trimmed
    [ -f "$file" ] || return 1
    esc="${name//./\\.}"
    matched=$(grep -E "${LEAD}${RUN}${esc}" "$file" 2>/dev/null) || return 1
    [ -n "$matched" ] || return 1
    while IFS= read -r ln; do
        trimmed="${ln#"${ln%%[![:space:]]*}"}"
        case "$trimmed" in '#'*|'//'*) continue ;; esac
        return 0
    done <<< "$matched"
    return 1
}

self_test() {
    local tmp rc out
    tmp="$(mktemp -d)" || { echo "ci-gate-lanes: mktemp failed" >&2; exit 2; }
    trap 'rm -rf "$tmp"' EXIT
    mkdir -p "$tmp/scripts/ci" "$tmp/.github/workflows"
    echo 'exit 0' > "$tmp/scripts/ci/check-ok.sh"
    echo 'exit 0' > "$tmp/scripts/ci/check-unlisted.sh"
    echo 'exit 0' > "$tmp/scripts/ci/check-mentioned.sh"
    echo 'exit 0' > "$tmp/scripts/ci/check-underdeclared.sh"
    # check-ok.sh is RUN. check-mentioned.sh is only NAMED, the way a paths:
    # filter names a file - the decorative case the whole gate exists to catch.
    # check-underdeclared.sh is run by w2.yml and its row names only w.yml,
    # which is the case the undeclared-invoker scan exists for.
    printf 'on:\n  push:\n    paths:\n      - scripts/ci/check-mentioned.sh\njobs:\n  a:\n    steps:\n      - run: bash scripts/ci/check-ok.sh\n      - run: bash scripts/ci/check-underdeclared.sh\n' \
        > "$tmp/.github/workflows/w.yml"
    printf 'jobs:\n  b:\n    steps:\n      - run: bash scripts/ci/check-underdeclared.sh\n' \
        > "$tmp/.github/workflows/w2.yml"
    printf 'check-ok.sh\tci\t.github/workflows/w.yml\tfixture, stays ci on purpose\n' > "$tmp/scripts/ci/ci-gate-lanes.tsv"
    printf 'check-mentioned.sh\tci\t.github/workflows/w.yml\tfixture\n' >> "$tmp/scripts/ci/ci-gate-lanes.tsv"
    printf 'check-gone.sh\tci\t.github/workflows/w.yml\tfixture\n' >> "$tmp/scripts/ci/ci-gate-lanes.tsv"
    printf 'check-underdeclared.sh\tci\t.github/workflows/w.yml\tfixture\n' >> "$tmp/scripts/ci/ci-gate-lanes.tsv"

    out="$(CI_GATE_LANES_ROOT="$tmp" bash "$0" 2>&1)"
    rc=$?
    # Assert the SPECIFIC strings, never the exit code alone: a script that dies
    # on a missing mktemp also exits non-zero and would certify this green.
    if [ "$rc" -eq 0 ]; then
        echo "ci-gate-lanes: self-test FAILED - fixture with an unlisted script passed" >&2
        exit 1
    fi
    case "$out" in *"unlisted: check-unlisted.sh"*) ;; *)
        echo "ci-gate-lanes: self-test FAILED - no 'unlisted: check-unlisted.sh' in output" >&2
        echo "$out" >&2; exit 1 ;;
    esac
    case "$out" in *"missing: check-gone.sh"*) ;; *)
        echo "ci-gate-lanes: self-test FAILED - no 'missing: check-gone.sh' in output" >&2
        echo "$out" >&2; exit 1 ;;
    esac
    case "$out" in *"check-mentioned.sh: '.github/workflows/w.yml' does not invoke it"*) ;; *)
        echo "ci-gate-lanes: self-test FAILED - a mention-only invoker was accepted as a real one" >&2
        echo "$out" >&2; exit 1 ;;
    esac
    # The undeclared-invoker scan is the load-bearing half of this gate, and
    # every assertion above exercises only the DECLARED half. Without this case
    # a `return 0` at the top of scan_surface still prints "self-test ok", so
    # the half that catches a row naming one of two real invokers could break
    # silently - which is this gate's own defect, in its own self-test.
    case "$out" in *"check-underdeclared.sh: '.github/workflows/w2.yml' invokes it but the row does not list it"*) ;; *)
        echo "ci-gate-lanes: self-test FAILED - the undeclared-invoker scan did not report w2.yml" >&2
        echo "$out" >&2; exit 1 ;;
    esac
    # Positive control on the matcher: the fixture's one real invocation must
    # NOT be reported, or the gate is refusing everything and the assertions
    # above pass for the wrong reason.
    case "$out" in *"check-ok.sh: "*)
        echo "ci-gate-lanes: self-test FAILED - a real invocation was reported as broken" >&2
        echo "$out" >&2; exit 1 ;;
    esac
    echo "ci-gate-lanes: self-test ok"
    exit 0
}

case "${1:-}" in
    --self-test) self_test ;;
    "") ;;
    *) echo "ci-gate-lanes: unknown argument '$1'" >&2; exit 2 ;;
esac

[ -f "$MANIFEST" ] || { echo "ci-gate-lanes: manifest missing at $MANIFEST" >&2; exit 2; }
[ -d "$GATE_DIR" ] || { echo "ci-gate-lanes: gate dir missing at $GATE_DIR" >&2; exit 2; }

LIVE="$(mktemp)"; LISTED="$(mktemp)"
trap 'rm -f "$LIVE" "$LISTED"' EXIT

# `ls`, not GNU `find -printf`, and a `while read` loop, not `mapfile`: this
# runs unmodified on stock macOS bash 3.2, same as check-workflow-manifest.sh.
(cd "$GATE_DIR" && ls -1 -- *.sh 2>/dev/null) | sort -u > "$LIVE"
grep -v '^[[:space:]]*#' "$MANIFEST" | grep -v '^[[:space:]]*$' \
    | awk -F'\t' '{print $1}' | sort -u > "$LISTED"

while IFS= read -r s; do
    [ -n "$s" ] && note_fail "unlisted: $s has no row in ci-gate-lanes.tsv - name its invoker, or delete the script"
done < <(comm -23 "$LIVE" "$LISTED")

while IFS= read -r s; do
    [ -n "$s" ] && note_fail "missing: $s has a row but no file in scripts/ci - drop the row"
done < <(comm -13 "$LIVE" "$LISTED")

ROWS=0
while IFS=$'\t' read -r script lane invoker note; do
    case "$script" in ''|'#'*) continue ;; esac
    ROWS=$((ROWS + 1))

    case "$lane" in
        local|ci|post-push|entrypoint) ;;
        *) note_fail "$script: lane '$lane' is not one of local, ci, post-push, entrypoint"; continue ;;
    esac

    # The invoker column is a COMMA-SEPARATED LIST, because a gate really can
    # sit on more than one path and the manifest has to be able to say so. A
    # gate wired into both guards.yml (which has no paths filter, so it covers
    # a docs-only PR) and preflight.sh (which fires before the push) has two
    # invokers, and collapsing that to one would make the file lie about the
    # coverage it is supposed to describe.
    has_early=0
    has_ci=0
    if [ "$lane" = entrypoint ]; then
        [ "$invoker" = "-" ] || note_fail "$script: an entrypoint is invoked by a person, so its invoker column is '-', not '$invoker'"
    else
        old_ifs="$IFS"; IFS=','
        for one in $invoker; do
            IFS="$old_ifs"
            if [ ! -f "$REPO_ROOT/$one" ]; then
                note_fail "$script: declared invoker '$one' does not exist"
            elif ! invokes "$REPO_ROOT/$one" "$script"; then
                note_fail "$script: '$one' does not invoke it - a row may not assert an invoker it does not have"
            else
                case "$one" in
                    cli/src/fno/test_cmd.py|scripts/ci/preflight.sh) has_early=1 ;;
                    # A composite action counts as a CI surface: a gate invoked
                    # from one is really reached in CI, and refusing to record
                    # that would leave the row unable to name a real invoker.
                    .github/workflows/*|.github/actions/*) has_ci=1 ;;
                    scripts/ci/*)
                        # One level of indirection: a gate run by another gate
                        # is reachable only if that one is. Its row is the
                        # authority.
                        parent_lane="$(grep -v '^[[:space:]]*#' "$MANIFEST" \
                            | awk -F'\t' -v k="${one#scripts/ci/}" '$1 == k {print $2}')"
                        case "$parent_lane" in
                            ci|post-push) has_ci=1 ;;
                            local) has_early=1 ;;
                            *) note_fail "$script: invoker '$one' is not itself reachable (its lane reads '${parent_lane:-no row}')" ;;
                        esac
                        ;;
                    *) note_fail "$script: '$one' is not a lane-bearing invoker - use a workflow, test_cmd.py, preflight.sh, or another listed gate" ;;
                esac
            fi
            IFS=','
        done
        IFS="$old_ifs"
    fi

    case "$lane" in
        local)
            [ "$has_early" -eq 1 ] || note_fail "$script: lane 'local' means it fires before the push, so at least one invoker must be cli/src/fno/test_cmd.py or scripts/ci/preflight.sh"
            ;;
        ci|post-push)
            [ "$has_ci" -eq 1 ] || note_fail "$script: lane '$lane' must be reached from .github/workflows or another listed gate"
            [ "$has_early" -eq 0 ] || note_fail "$script: lane '$lane' contradicts its invokers - it already fires before the push, so its lane is 'local'"
            ;;
    esac

    # Every lane that is not 'local' owes a reason. "Not yet moved" is not one:
    # the note has to name a runtime, a network dependency, or the PR artifact
    # the gate needs, so a row cannot sit in a late lane by default.
    case "$lane" in
        ci|post-push|entrypoint)
            [ -n "${note// /}" ] || note_fail "$script: lane '$lane' needs a note saying why it cannot fire before the push"
            ;;
    esac
done < <(grep -v '^[[:space:]]*#' "$MANIFEST" | grep -v '^[[:space:]]*$')

# Undeclared invokers ------------------------------------------------------
# Verifying the invokers a row DECLARES is only half the job. A row that names
# one of its two real invokers is still a row that lies about coverage, and the
# gate would never notice: delete the guards.yml step for a gate whose row only
# names preflight.sh and the manifest stays green while a CI lane disappears.
# So enumerate the real invokers too and require the row to list all of them.
REAL="$(mktemp)"
trap 'rm -f "$LIVE" "$LISTED" "$REAL"' EXIT

scan_surface() {
    local surface="$1" base hit
    [ -f "$REPO_ROOT/$surface" ] || return 0
    base="$(basename "$surface")"
    grep -v '^[[:space:]]*#' "$REPO_ROOT/$surface" 2>/dev/null \
        | grep -Eo "${LEAD}${RUN}[A-Za-z0-9._-]+\.sh" \
        | sed -E "s#.*[/[:space:]\"$SQ]##" \
        | while IFS= read -r hit; do
            [ -n "$hit" ] || continue
            [ "$hit" = "$base" ] && continue
            [ -f "$GATE_DIR/$hit" ] || continue
            printf '%s\t%s\n' "$hit" "$surface"
          done
}

# Both spellings, and composite actions too. The surfaces this scan reads have
# to be the surfaces a gate can actually be invoked from, or an under-declared
# row becomes invisible again by the back door: today the tree has 18 .yml and
# no .yaml, and .github/actions/guards-setup already exists as a composite this
# repo uses, so both shapes are one edit away from being real.
for wf in "$REPO_ROOT"/.github/workflows/*.yml "$REPO_ROOT"/.github/workflows/*.yaml; do
    [ -f "$wf" ] || continue
    scan_surface ".github/workflows/$(basename "$wf")"
done >> "$REAL"
for act in "$REPO_ROOT"/.github/actions/*/action.yml "$REPO_ROOT"/.github/actions/*/action.yaml; do
    [ -f "$act" ] || continue
    scan_surface ".github/actions/$(basename "$(dirname "$act")")/$(basename "$act")"
done >> "$REAL"
scan_surface "cli/src/fno/test_cmd.py" >> "$REAL"
for g in "$GATE_DIR"/*.sh; do
    [ -f "$g" ] || continue
    scan_surface "scripts/ci/$(basename "$g")"
done >> "$REAL"

while IFS=$'\t' read -r script lane invoker note; do
    case "$script" in ''|'#'*) continue ;; esac
    [ "$lane" = entrypoint ] && continue
    while IFS= read -r found; do
        [ -n "$found" ] || continue
        case ",$invoker," in
            *",$found,"*) ;;
            *) note_fail "$script: '$found' invokes it but the row does not list it - the invoker column records every path, not a favourite one" ;;
        esac
    done < <(awk -F'\t' -v k="$script" '$1 == k {print $2}' "$REAL" | LC_ALL=C sort -u)
done < <(grep -v '^[[:space:]]*#' "$MANIFEST" | grep -v '^[[:space:]]*$')

if [ "$FAIL" -ne 0 ]; then
    echo "ci-gate-lanes: FAILED" >&2
    exit 1
fi

echo "ci-gate-lanes: $ROWS rows checked, every gate has a real invoker"
