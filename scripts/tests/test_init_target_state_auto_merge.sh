#!/usr/bin/env bash
# Tests for auto_merge fields in init-target-state.sh
# Verifies: auto_merge_enabled + auto_merge_approved are written to
#           target-state.md at init time. The merged_prs / merge_auto_queued /
#           merge_failed / conflicts_resolved arrays are NOT: the manifest became
#           write-once, and mutable per-run state left it with them.
# Also verifies TARGET_NO_MERGE=1 forces auto_merge_approved: false.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"

# scripts/lib/config.sh reads dotted keys (auto_merge.enabled) through yq and
# has no fallback, so without it every enabled-config case below fails for a
# reason that has nothing to do with the code under test. Fail loud and name
# the cause rather than emitting five misleading assertion failures.
if ! command -v yq >/dev/null 2>&1; then
    echo "FAIL: yq not installed - config.sh cannot read auto_merge.enabled," >&2
    echo "      so the enabled-config assertions here cannot be meaningful." >&2
    echo "      Install: brew install yq | apt install yq | mise use yq" >&2
    exit 1
fi

PASS=0
FAIL=0

pass() { echo "  PASS: $1"; ((PASS++)); }
fail() { echo "  FAIL: $1"; ((FAIL++)); }

assert_contains() {
    local desc="$1" needle="$2" haystack="$3"
    if echo "$haystack" | grep -qF "$needle"; then
        pass "$desc"
    else
        fail "$desc (pattern='$needle' not found in output)"
    fi
}

# ---- setup helper ----
# Creates a temp git repo with optional config.toml content, runs init,
# returns path to created state file via stdout.
setup_repo() {
    local settings_content="$1"
    local T
    T=$(mktemp -d)
    mkdir -p "$T/.fno"
    echo "$settings_content" > "$T/.fno/config.toml"
    git -C "$T" init -q 2>/dev/null
    git -C "$T" config user.email "test@test.com"
    git -C "$T" config user.name "Test"
    echo "$T"
}

run_init_in() {
    local tmpdir="$1"
    shift
    # An empty HOME: without it the developer's own ~/.fno/config.toml leaks in
    # and the "false when not set" defaults read whatever they have configured.
    local fake_home="$tmpdir/.home"
    mkdir -p "$fake_home"
    # Caller-supplied assignments come last so they override these defaults;
    # env applies leading NAME=VALUE pairs left to right.
    (
      cd "$tmpdir"
      if [[ $# -gt 0 ]]; then
        env HOME="$fake_home" TARGET_START=1 TARGET_INPUT="test feature" "$@" bash "$INIT_SCRIPT"
      else
        HOME="$fake_home" TARGET_START=1 TARGET_INPUT="test feature" bash "$INIT_SCRIPT"
      fi
    ) 2>/dev/null
}

# ---- Test 1: AC1-HP fields present when auto_merge disabled (default) ----

echo ""
echo "test_auto_merge_fields_present_disabled_by_default"

T=$(setup_repo "expertise = \"frontend\"")

run_in_result=$(run_init_in "$T")
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "AC1-HP: auto_merge_enabled field present" "auto_merge_enabled:" "$STATE"
assert_contains "AC1-HP: auto_merge_approved field present" "auto_merge_approved:" "$STATE"
assert_contains "AC1-HP: auto_merge_approved false when disabled" "auto_merge_approved: false" "$STATE"
assert_contains "AC1-HP: auto_merge_enabled false when not set" "auto_merge_enabled: false" "$STATE"
assert_contains "x-9d11: source default-off when nothing set" "auto_merge_source: default-off" "$STATE"

rm -rf "$T"

# ---- Test 1b: an explicit config refusal reads source config, not default-off ----

echo ""
echo "test_explicit_config_false_reads_source_config"

T=$(setup_repo "[auto_merge]
enabled = false")

run_init_in "$T"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "explicit enabled=false reads source config" "auto_merge_source: config" "$STATE"

rm -rf "$T"

# ---- Test 2: AC2-HP auto_merge_approved true when enabled + target allowed ----

echo ""
echo "test_auto_merge_approved_true_when_enabled"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "AC2-HP: auto_merge_enabled true when set" "auto_merge_enabled: true" "$STATE"
assert_contains "AC2-HP: auto_merge_approved true when enabled" "auto_merge_approved: true" "$STATE"
assert_contains "x-9d11: source config when enabled" "auto_merge_source: config" "$STATE"

rm -rf "$T"

# ---- Test 3: AC3-ERR TARGET_NO_MERGE=1 overrides to false ----

echo ""
echo "test_target_no_merge_forces_approved_false"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T" "TARGET_NO_MERGE=1"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "AC3-ERR: auto_merge_approved false with TARGET_NO_MERGE=1" "auto_merge_approved: false" "$STATE"
assert_contains "AC1-HP: source flag-no-merge with the flag carrier" "auto_merge_source: flag-no-merge" "$STATE"

rm -rf "$T"

# ---- Test 3b: free text is NOT a control input (x-9d11 defect 2) ----
# The fold no longer matches a `no-merge` token in TARGET_INPUT: prose can
# manufacture neither a grant (x-51a3) nor a refusal. Posture arrives by flag
# (TARGET_NO_MERGE, set from --no-merge), env, or config alone. An LLM-composed
# brief containing the word no-merge therefore resolves from config.

echo ""
echo "test_free_text_no_merge_is_inert"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T" "TARGET_INPUT=no-merge x-e938"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "free-text no-merge reflects config alone" "auto_merge_approved: true" "$STATE"

rm -rf "$T"

# ---- Test 3c: the `auto-merge` grant token is deliberately inert ----
# Asymmetric by design: honoring a prohibition found in prose fails safe,
# honoring a grant found in prose would let arbitrary text manufacture
# merge authority.

echo ""
echo "test_auto_merge_token_in_input_is_inert"

# enabled=false is the load-bearing config here: it proves the token grants
# nothing on its own. With enabled=true the result would be `true` either way
# and the case would assert nothing.
T=$(setup_repo "expertise = \"frontend\"")

run_init_in "$T" "TARGET_INPUT=auto-merge x-e938"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "auto-merge token does NOT grant approval" "auto_merge_approved: false" "$STATE"

rm -rf "$T"

# ---- Test 3c2: the dispatch string reaches the fold as prose only ----
# harness_map._AUTONOMOUS_COMMAND is "/target --no-merge {id}". The worker
# translates the flag onto `fno target start`, which sets TARGET_NO_MERGE -
# the refusal is carried by the flag path (test 3), never by the fold reading
# the command string. Prose that merely mentions the flag is inert here.

echo "test_dispatch_command_string_is_prose_not_posture"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T" "TARGET_INPUT=/target --no-merge x-e938"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "dispatch string as prose reflects config" "auto_merge_approved: true" "$STATE"

rm -rf "$T"

# ---- Test 3d: free text no longer defeats an env grant ----

echo ""
echo "test_free_text_no_merge_does_not_beat_inherited_auto_merge_grant"

# No production code sets TARGET_AUTO_MERGE, so the only way it is ever set is
# inheritance from an ancestor shell or spawning parent. Posture inputs are
# flag/env/config only (x-9d11): an inherited grant now stands unless the
# refusal arrives on its own carrier (TARGET_NO_MERGE, test 3d2).
T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T" "TARGET_AUTO_MERGE=1" "TARGET_INPUT=no-merge x-e938"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "free text does not defeat an inherited TARGET_AUTO_MERGE" "auto_merge_approved: true" "$STATE"

rm -rf "$T"

# ---- Test 3d2: TARGET_NO_MERGE still outranks everything ----

echo "test_explicit_no_merge_env_still_wins"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T" "TARGET_NO_MERGE=1" "TARGET_AUTO_MERGE=1" "TARGET_INPUT=auto-merge x-e938"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "TARGET_NO_MERGE=1 outranks the grant env var" "auto_merge_approved: false" "$STATE"

rm -rf "$T"

# ---- Test 3d3: the grant env var still works without a refusal ----

echo "test_auto_merge_env_grants_when_no_refusal"

T=$(setup_repo "expertise = \"frontend\"")

run_init_in "$T" "TARGET_AUTO_MERGE=1" "TARGET_INPUT=x-e938"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "TARGET_AUTO_MERGE=1 grants when nothing refuses" "auto_merge_approved: true" "$STATE"
assert_contains "x-9d11: source env-target-auto-merge on the env grant" "auto_merge_source: env-target-auto-merge" "$STATE"

rm -rf "$T"

# ---- Test 3d4: the token match is whole-token, not a substring ----
# `no-merger` and a path containing no-merge must NOT revoke a configured grant.

echo "test_token_match_is_whole_token"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T" "TARGET_INPUT=plans/no-merge-notes.md"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "a path containing no-merge does not revoke" "auto_merge_approved: true" "$STATE"

rm -rf "$T"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T" "TARGET_INPUT=no-merger x-e938"
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "no-merger does not revoke" "auto_merge_approved: true" "$STATE"

rm -rf "$T"

# ---- Test 3e: an empty TARGET_INPUT must not match the token ----

echo ""
echo "test_empty_input_does_not_match_token"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T" "TARGET_INPUT="
STATE=$(cat "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "empty input falls through to config" "auto_merge_approved: true" "$STATE"

rm -rf "$T"

# ---- Test 4: AC4-VERIFY the auto_merge fields are in the YAML frontmatter ----

echo ""
echo "test_fields_are_in_yaml_frontmatter"

T=$(setup_repo "[auto_merge]
enabled = true")

run_init_in "$T"
# Extract frontmatter only (between first --- and second ---)
FRONTMATTER=$(awk '/^---/{n++; if(n==2) exit} n==1{print}' "$T/.fno/target-state.md" 2>/dev/null || echo "")

assert_contains "AC4-VERIFY: auto_merge_enabled in frontmatter" "auto_merge_enabled:" "$FRONTMATTER"
assert_contains "AC4-VERIFY: auto_merge_approved in frontmatter" "auto_merge_approved:" "$FRONTMATTER"

rm -rf "$T"

# ---- Summary ----

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"

[[ $FAIL -eq 0 ]]
