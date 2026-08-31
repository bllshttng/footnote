#!/usr/bin/env bash
# scripts/ci/check-retired-command-strings.sh
#
# A ruling that lives only in a decision record loses to a string the agent
# reads at the moment of acting. The decision record fires when someone goes
# looking; a runtime string fires when someone is about to act. This gate
# carries a ruling to the second moment.
#
# It reads scripts/ci/retired-commands.txt and fails when a caller-facing
# string shows a retired command in RUNNABLE form.
#
# Three narrowings, applied in order. Only the third makes a judgment:
#
#   1. The command must be retired. The scan looks for registry members, not
#      for every command, so most of a naive sweep never enters the report.
#   2. The occurrence must be in RUNNABLE form - an argument placeholder, a
#      concrete short id, or the command left dangling at end of line by a
#      wrapped string. A reader can copy "claude rm <id>" and run it;
#      "claude rm failed to start" names the same command and cannot be run.
#      This is mechanical and needs no reading of intent. Comment-only lines
#      are out of scope by construction.
#   3. The author declares the verdict. A surviving hit fails unless it
#      carries a `retired-ok:` marker on its own line or the line above,
#      saying why that string does not tell its reader to run the command.
#      Three kinds of string earn one: a failure message naming the shellout
#      that failed, an API docstring naming what the function wraps, and a
#      prohibition that names the command in order to forbid it.
#
#      scripts/ci/retired-ok-paths.txt declares the same thing by PATH, for
#      the one case where an in-file marker is not free: a byte-budgeted file
#      re-read on a schedule pays for the marker every time. It is the weaker
#      instrument, so the success line names every path it cleared.
#
# Narrowing 2 has a known ceiling: a bare-form instruction with no argument
# at all ("run claude rm") passes. Widening it to catch that means separating
# instruction from description by reading the sentence, and no regex does
# that - the attempt produces the noise that kills a gate. Upgrade path: if a
# bare-form instruction ever ships, add its imperative cue to the pattern,
# not a classifier.
#
# THREE controls, all load-bearing, because an absence-only pass has two
# explanations ("clean" and "the instrument never matched anything"):
#
#   tool     the pattern must match an in-script canary invocation.
#   surface  each surface must resolve to tracked files of its own. A glob
#            pathspec that matches nothing fails SILENTLY, and a whole-scan
#            control cannot see it while another surface still returns hits.
#            An earlier draft scanned `crates/*/src/**/*.rs`, matched zero
#            files, and reported a clean Rust tree that held the sharpest
#            specimen in the repo.
#   target   the canary FIXTURE's marked line must be seen and cleared. It
#            asserts on the fixture and not on production text, so a real
#            cleanup can take the tree to zero marked sites without trapping
#            the gate red.
#
# Exit 0 clean; 1 on a surviving hit, a malformed registry, or a control that
# did not fire.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
cd "$REPO_ROOT"

REGISTRY="scripts/ci/retired-commands.txt"
OK_PATHS="scripts/ci/retired-ok-paths.txt"
MARKER='retired-ok:'
CONFIG_REGISTRY="scripts/ci/retired-config-leaves.txt"
CONFIG_CANARY="scripts/ci/fixtures/retired-config-leaf-canary.py"
CONFIG_MARKER='retired-config-ok:'

fail() {
    echo "check-retired-command-strings: $*" >&2
    exit 1
}

[ -f "$REGISTRY" ] || fail "registry not found at $REGISTRY"

# --- 1. Parse the registry -------------------------------------------------
# A malformed line exits non-zero naming the line number rather than being
# skipped: a silently dropped entry is a retired command the gate stops
# looking for, which is the failure this whole file exists to prevent.
declare -a CMDS INSTEAD RULING
lineno=0
while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    case "$line" in ''|'#'*) continue ;; esac
    IFS='|' read -r cmd instead ruling <<<"$line"
    [ -n "$cmd" ] && [ -n "$instead" ] && [ -n "$ruling" ] ||
        fail "$REGISTRY:$lineno: malformed entry, want <command>|<what to say instead>|<ruling id>"
    CMDS+=("$cmd"); INSTEAD+=("$instead"); RULING+=("$ruling")
done <"$REGISTRY"

[ "${#CMDS[@]}" -gt 0 ] || fail "$REGISTRY holds no entries; nothing to police"

# --- 1a. Parse retired configuration leaves -------------------------------
# A retired config key remains a live surface if it survives in a model,
# reader, example, or caller-facing explanation. Keep the disposition in a
# small, exact tombstone list so the audit can account for every removal.
[ -f "$CONFIG_REGISTRY" ] || fail "config registry not found at $CONFIG_REGISTRY"
declare -a CONFIG_KEYS CONFIG_DISPOSITIONS CONFIG_REPLACEMENTS
lineno=0
while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    case "$line" in ''|'#'*) continue ;; esac
    IFS='|' read -r key disposition replacement <<<"$line"
    [[ "$key" =~ ^[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)+$ ]] ||
        fail "$CONFIG_REGISTRY:$lineno: malformed config path '$key'"
    case "$disposition" in
        constant|derived|one-shot|deleted) ;;
        *) fail "$CONFIG_REGISTRY:$lineno: invalid disposition '$disposition'" ;;
    esac
    [ -n "$replacement" ] ||
        fail "$CONFIG_REGISTRY:$lineno: missing replacement/remedy"
    CONFIG_KEYS+=("$key")
    CONFIG_DISPOSITIONS+=("$disposition")
    CONFIG_REPLACEMENTS+=("$replacement")
done <"$CONFIG_REGISTRY"
# Zero active entries is a valid state: a tombstone can be filed before the
# removal it describes lands. Rather than fail as vacuous, the config scan
# goes dormant and says so on the success line, so a pass never silently
# omits its scope.
CONFIG_DORMANT=0
[ "${#CONFIG_KEYS[@]}" -gt 0 ] || CONFIG_DORMANT=1

[ -f "$CONFIG_CANARY" ] ||
    fail "config canary not found at $CONFIG_CANARY"
grep -qF "$CONFIG_MARKER" "$CONFIG_CANARY" ||
    fail "config canary lacks $CONFIG_MARKER; config scan is vacuous"

# --- 1b. Parse the path allowlist ------------------------------------------
# A whole-file exemption for the one case an in-file marker is not free: a
# byte-budgeted file re-read on a schedule pays for the marker every time.
declare -a OK_PATH_LIST
if [ -f "$OK_PATHS" ]; then
    lineno=0
    while IFS= read -r line || [ -n "$line" ]; do
        lineno=$((lineno + 1))
        case "$line" in ''|'#'*) continue ;; esac
        IFS='|' read -r okpath okwhy <<<"$line"
        [ -n "$okpath" ] && [ -n "$okwhy" ] ||
            fail "$OK_PATHS:$lineno: malformed entry, want <repo-relative path>|<why the marker cannot live in the file>"
        [ -f "$okpath" ] ||
            fail "$OK_PATHS:$lineno: $okpath does not exist; a stale exemption silently widens the gate"
        OK_PATH_LIST+=("$okpath")
    done <"$OK_PATHS"
fi

# Runnable form, three shapes:
#   1. an argument placeholder - `claude rm <id>`, `{row}`, `$job`
#   2. a concrete short id - `claude rm 7c5dcf5d`, which is strictly MORE
#      copyable than a placeholder and was missed by an earlier draft
#   3. the command at end of line, which is how a wrapped format string
#      hides one: `f"Orphan supervisor: claude rm "` on one line and
#      `f"{short_id} to clean later."` on the next shipped a live
#      instruction straight past a line-scoped scan
#
# SUBVERB is why an earlier draft passed vacuously on `fno law set <subject>`.
# The registry retires two-token ROOTS, but the string a reader copies names a
# LEAF under one, so the argument sits a word further along than shapes 1-3
# look. One optional lowercase word closes that, and it applies to all three
# shapes: `fno law set {subject}` is an f-string, which is how nearly every
# caller-facing string in cli/src is written, and a concrete short id is
# strictly more copyable than a placeholder.
#
# This DOES catch descriptive strings of the same shape. The measured specimen
# is `format!("claude rm exited {code}")` in daemon.rs, where `exited` reads as
# a sub-verb to any regex. That is not a reason to narrow: it is the first kind
# of string the `retired-ok:` marker exists to excuse, and the tree holds one
# of them. Declaring one marker is cheaper than a blind spot that hides every
# f-string instruction under a retired root. An earlier draft of THIS line
# narrowed shape 4 to `<...>` only, on the theory that interpolation means
# description; the daemon.rs specimen refutes it, since the brace there sits
# after a word, not against the root.
#
# SUBVERB pairs with the ARGUMENT shapes (1 and 2) only, never with shape 3.
# Shape 3 is "command dangling at end of line", and one optional word there
# turns any trailing prose into a hit: `"... came from fno test earlier"` would
# match where only `"... fno test"` did before. That is a flood, and unlike the
# daemon.rs specimen it is unbounded prose rather than one declarable site.
# Up to TWO words, not one: retired roots have grandchildren. `fno workspace`
# is a retired root and `fno workspace worktree ensure <name>` is the runnable
# string under it, so a one-word bound passed it vacuously. Two is the depth
# the real registry needs; raise it only against a specimen.
SUBVERB='([[:space:]]+[a-z][a-z-]*){0,2}'
ARG="(${SUBVERB}"'([[:space:]]+[<{$]|[[:space:]]+[0-9a-f]{8})|[[:space:]]*"[[:space:]]*$)'
PATTERN=""
for cmd in "${CMDS[@]}"; do
    PATTERN="${PATTERN}${PATTERN:+|}${cmd}${ARG}"
done
PATTERN="(${PATTERN})"

# --- 2. Tool control -------------------------------------------------------
CANARY="run ${CMDS[0]} <short_id> to clean up"
printf '%s\n' "$CANARY" | grep -qE "$PATTERN" ||
    fail "pattern does not match the canary; every future sweep would pass vacuously"

# The sub-verb form needs its own canaries, or SUBVERB could rot to a no-op
# and the bare canary above would still pass. All three argument shapes are
# asserted, because an earlier draft covered only the angle-bracket one and
# let every f-string instruction under a retired root through.
for shape in "set <subject> <decision>" "set {subject}" "set 7c5dcf5d" \
             "worktree ensure <name>"; do
    printf '%s\n' "use \`${CMDS[0]} ${shape}\` instead" | grep -qE "$PATTERN" ||
        fail "pattern misses sub-verb shape '${shape}'; a leaf under a retired root would pass"
done

# Negative controls, one per way the sub-verb widening could flood. The first
# is a mid-sentence mention. The second is the shape SUBVERB deliberately does
# NOT reach: a wrapped string whose trailing prose ends the line, which would
# turn every sentence mentioning a retired command into a hit.
NEGATIVE_CANARY="${CMDS[0]} is now ${INSTEAD[0]}"
printf '%s\n' "$NEGATIVE_CANARY" | grep -qE "$PATTERN" &&
    fail "pattern matches a descriptive mention; it would flood the report"

TRAILING_PROSE_CANARY="\"the value came from ${CMDS[0]} earlier\""
printf '%s\n' "$TRAILING_PROSE_CANARY" | grep -qE "$PATTERN" &&
    fail "sub-verb reached the end-of-line shape; trailing prose would flood the report"

# --- 3. Scan the caller-facing surfaces ------------------------------------
# A surface is a directory plus an extension, never a `**` pathspec (see the
# surface control above). Everywhere a caller reads at the moment of acting.
# `docs` is not optional:
# the operator guide is the most caller-facing surface there is, and leaving
# it out would exempt the text most likely to be followed by hand.
SURFACES=(
    "cli/src:py"
    "crates:rs"
    "skills:md"
    "docs:md"
    "scripts:sh"
    "hooks:sh"
    "agents:md"
    "commands:md"
)

# The command scan above owns per-surface controls. Config leaves use the same
# tracked caller-facing tree plus committed YAML/TOML files, which lets a
# nested TOML table be checked without inventing a second surface taxonomy.
CONFIG_FILES=""
CONFIG_DIRS=""
for surface in "${SURFACES[@]}"; do
    dir="${surface%%:*}"
    case " $CONFIG_DIRS " in *" $dir "*) ;; *) CONFIG_DIRS="${CONFIG_DIRS}${CONFIG_DIRS:+$'\n'}${dir}" ;; esac
done
CONFIG_DIRS="${CONFIG_DIRS}${CONFIG_DIRS:+$'\n'}.github"
while IFS= read -r dir; do
    [ -n "$dir" ] || continue
    for ext in py rs sh md yaml yml toml; do
        ext_files="$(git ls-files -- "$dir" | grep -E "\.${ext}\$" || true)"
        [ -n "$ext_files" ] && CONFIG_FILES="${CONFIG_FILES}${CONFIG_FILES:+$'\n'}${ext_files}"
    done
done < <(printf '%s\n' "$CONFIG_DIRS")
[ -n "$CONFIG_DIRS" ] || fail "no config-scan directories; config scan is vacuous"
[ -n "$CONFIG_FILES" ] || fail "no tracked config-scan files; config scan is vacuous"

CONFIG_HITS=""
CONFIG_CANARY_SEEN=0
for idx in "${!CONFIG_KEYS[@]}"; do
    key="${CONFIG_KEYS[$idx]}"
    while IFS= read -r hit; do
        [ -n "$hit" ] || continue
        file="${hit%%:*}"; rest="${hit#*:}"; num="${rest%%:*}"
        case "$file" in
            "$CONFIG_REGISTRY"|"$CONFIG_CANARY")
                [ "$file" = "$CONFIG_CANARY" ] && CONFIG_CANARY_SEEN=1
                continue
                ;;
        esac
        CONFIG_HITS="${CONFIG_HITS}${CONFIG_HITS:+$'\n'}${file}:${num}:${key}"
    done < <(git grep -nF -- "$key" -- $CONFIG_FILES || true)
done

# A dotted path is not required in TOML examples. Flatten the active table
# while scanning key assignments so `[review]` plus `github_apps = [...]`
# cannot evade an exact-path search.
while IFS= read -r file; do
    [ -n "$file" ] || continue
    case "$file" in *.toml) ;; *) continue ;; esac
    current=""
    toml_num=0
    while IFS= read -r toml_line || [ -n "$toml_line" ]; do
        toml_num=$((toml_num + 1))
        case "$toml_line" in
            \[*\]) current="${toml_line#\[}"; current="${current%\]}"; continue ;;
        esac
        for idx in "${!CONFIG_KEYS[@]}"; do
            key="${CONFIG_KEYS[$idx]}"; prefix="${key%.*}"; leaf="${key##*.}"
            if [ "$current" = "$prefix" ] && [[ "$toml_line" =~ ^[[:space:]]*${leaf}[[:space:]]*= ]]; then
                CONFIG_HITS="${CONFIG_HITS}${CONFIG_HITS:+$'\n'}${file}:${toml_num}:${key}"
            fi
        done
    done <"$file"
done <<<"$CONFIG_FILES"

[ "$CONFIG_DORMANT" -eq 1 ] ||
    [ "$CONFIG_CANARY_SEEN" -eq 1 ] ||
    fail "config scan did not see and clear its marked canary; scan is vacuous"

if [ -n "$CONFIG_HITS" ]; then
    echo "check-retired-command-strings: retired config leaves remain live:" >&2
    while IFS= read -r hit; do
        [ -n "$hit" ] || continue
        file="${hit%%:*}"; rest="${hit#*:}"; num="${rest%%:*}"; key="${rest##*:}"
        for idx in "${!CONFIG_KEYS[@]}"; do
            [ "$key" = "${CONFIG_KEYS[$idx]}" ] || continue
            echo "${file}:${num}: ${key} (${CONFIG_DISPOSITIONS[$idx]}); replacement/remedy: ${CONFIG_REPLACEMENTS[$idx]}" >&2
            break
        done
    done <<<"$CONFIG_HITS"
    exit 1
fi

RAW=""
for surface in "${SURFACES[@]}"; do
    dir="${surface%%:*}"; ext="${surface##*:}"
    # `grep -c`, never `grep -q`: -q exits on the first match and SIGPIPEs
    # `git ls-files`, which under `set -o pipefail` makes the pipeline status
    # 141 as soon as the file list outgrows the pipe buffer. That read as
    # "this surface holds no files" and failed the gate on the real tree
    # (cli/src emits ~17 KB, over the 16 KiB macOS buffer) while every
    # synthetic-tree test stayed green. -c consumes all input.
    tracked="$(git ls-files -- "$dir" | grep -cE "\.${ext}\$" || true)"
    [ "${tracked:-0}" -gt 0 ] ||
        fail "surface ${dir} holds no tracked .${ext} files; the surface list drifted and its scan is vacuous"
    # git grep over the directory, then filter by extension here: the
    # extension test stays in this script rather than in a pathspec, where a
    # glob that matches nothing is indistinguishable from a clean surface.
    hits="$(git grep -nE "$PATTERN" -- "$dir" | grep -E "^[^:]+\.${ext}:" || true)"
    RAW="${RAW}${hits}${hits:+$'\n'}"
done

[ -n "$RAW" ] ||
    fail "zero raw hits across every surface; the pattern drifted"

# There is deliberately no test-module exclusion. It would have to find where
# a `#[cfg(test)]` module ENDS, and every cheap proxy is wrong: the first
# match in crates/fno/src/client.rs is an indented attribute on a function at
# line 1174, so "first match to end of file" blanks 14k lines of production
# code, and "column-0 match to end of file" still swallows the production
# code that follows a mid-file test module. Measured instead: the runnable-form
# pattern already excludes almost every test assertion in the tree, because
# they name the command in bare form; the two that survive are the absence
# assertions in daemon.rs, which carry markers. Measured: the same Rust sites
# with the exclusion and without.
CANARY_FILE="scripts/ci/fixtures/retired-command-canary.sh"
BAD=""
MARKED=0
INSPECTED=0
CANARY_SEEN=0
ALLOWED=0
ALLOWED_PATHS=""
while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    file="${hit%%:*}"; rest="${hit#*:}"; num="${rest%%:*}"; text="${rest#*:}"
    stripped="${text#"${text%%[![:space:]]*}"}"

    # Comment-only lines describe the implementation; they never reach a caller.
    case "$file" in
        *.rs) case "$stripped" in '//'*) continue ;; esac ;;
        *.py|*.sh) case "$stripped" in '#'*) continue ;; esac ;;
    esac

    INSPECTED=$((INSPECTED + 1))

    path_ok=""
    for okpath in ${OK_PATH_LIST[@]+"${OK_PATH_LIST[@]}"}; do
        [ "$file" = "$okpath" ] && { path_ok=1; break; }
    done
    if [ -n "$path_ok" ]; then
        ALLOWED=$((ALLOWED + 1))
        case " $ALLOWED_PATHS " in *" $file "*) ;; *) ALLOWED_PATHS="${ALLOWED_PATHS}${ALLOWED_PATHS:+ }$file" ;; esac
        continue
    fi

    if [ "$num" -gt 1 ]; then
        prev="$(sed -n "$((num - 1))p" "$file")"
    else
        prev=""
    fi
    if [[ "$text" == *"$MARKER"* || "$prev" == *"$MARKER"* ]]; then
        MARKED=$((MARKED + 1))
        [ "$file" = "$CANARY_FILE" ] && CANARY_SEEN=1
        continue
    fi
    BAD="${BAD}${file}:${num}:${stripped}"$'\n'
done <<<"$RAW"

# --- 4. Report -------------------------------------------------------------
# The report comes before the target control on purpose. A drifted marker
# spelling turns every declared site into a violation, which is loud and
# names the sites; only a GREEN result can be vacuous, so that is where the
# control has to stand.
if [ -n "$BAD" ]; then
    echo "check-retired-command-strings: retired command(s) shown in runnable form:" >&2
    printf '%s' "$BAD" >&2
    echo "" >&2
    for idx in "${!CMDS[@]}"; do
        echo "  '${CMDS[$idx]}' was retired by ${RULING[$idx]}. Say instead: ${INSTEAD[$idx]}" >&2
    done
    echo "" >&2
    echo "Two ways out, and the second is a real option, not a formality:" >&2
    echo "  1. Rewrite the string so it names the replacement above." >&2
    echo "  2. Add '${MARKER} <why this does not tell its reader to run it>' on" >&2
    echo "     the line, or the line above it. A failure message naming the" >&2
    echo "     shellout that failed, a docstring naming what a function wraps," >&2
    echo "     and a prohibition that names the command to forbid it all earn one." >&2
    exit 1
fi

# --- 5. Target control -----------------------------------------------------
# Reached only on the green path. The canary fixture carries one marked site;
# not seeing it cleared means the marker spelling drifted, so every declared
# site in the tree would read as a violation. Asserting on the FIXTURE and
# not on production text is deliberate: a real cleanup may legitimately take
# the tree to zero marked sites, and that must stay distinguishable from a
# broken marker.
[ "$CANARY_SEEN" -eq 1 ] ||
    fail "clean, but the ${MARKER} canary at ${CANARY_FILE} was never seen and cleared; the marker spelling or the fixture drifted, so this pass is vacuous"

echo "retired-command strings OK: inspected ${INSPECTED} runnable-form site(s), ${MARKED} declared at the site, ${ALLOWED} declared by path, controls fired"
if [ "$CONFIG_DORMANT" -eq 1 ]; then
    echo "config scan dormant: no retired leaves registered"
fi
# Name the paths, never just the count. A whole-file exemption that shows up
# only as a number reads the same as no exemption at all.
if [ -n "$ALLOWED_PATHS" ]; then
    echo "  cleared by ${OK_PATHS}, so no line in them was read:"
    for p in $ALLOWED_PATHS; do echo "    $p"; done
fi
