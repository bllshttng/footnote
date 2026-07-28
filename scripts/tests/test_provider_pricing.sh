#!/usr/bin/env bash
# Tests for get_provider_pricing in scripts/lib/config.sh.
# Phase 02 of provider rotation failover (ab-9728b70b).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS+1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

# Each test runs in its own temp HOME so real ~/.fno is never touched.
setup_tmp_home() {
    TMP_HOME="$(mktemp -d)"
    export HOME="$TMP_HOME"
    mkdir -p "$TMP_HOME/.fno"
    # PWD-based local settings: clear any inherited override
    export PWD="$TMP_HOME"
    cd "$TMP_HOME" || exit 1
    # config.sh assigns these with `${VAR:-default}`, so a value set by an
    # earlier setup_tmp_home SURVIVES the re-source and keeps pointing at the
    # previous (now deleted) temp HOME. Clear them first or the re-source is a
    # no-op: every test after the first then reads a nonexistent file, which
    # fails the tests that expect a value and passes the rc=1 tests VACUOUSLY.
    unset LOCAL_SETTINGS GLOBAL_SETTINGS CONFIG_FILE
    # Re-source config.sh so PATH-derived vars pick up the new HOME.
    source "$SCRIPT_DIR/../lib/config.sh"
}

teardown_tmp_home() {
    rm -rf "$TMP_HOME"
}

# ---- Test 1: returns input rate when present ----
setup_tmp_home
cat > "$TMP_HOME/.fno/config.toml" <<'YAML'
[providers]
active = "claude-anthropic"

[[providers.records]]
id = "claude-anthropic"
name = "Claude Direct"
cli = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
[providers.records.pricing]
input_per_million_usd = 15.0
output_per_million_usd = 75.0
cache_read_per_million_usd = 1.5
cache_write_per_million_usd = 18.75
YAML
val=$(get_provider_pricing claude-anthropic input)
[[ "$val" == "15.0" ]] && pass "input rate returned" || fail "input rate (got '$val')"
val=$(get_provider_pricing claude-anthropic output)
[[ "$val" == "75.0" ]] && pass "output rate returned" || fail "output rate (got '$val')"
val=$(get_provider_pricing claude-anthropic cache_read)
[[ "$val" == "1.5" ]] && pass "cache_read rate returned" || fail "cache_read rate (got '$val')"
val=$(get_provider_pricing claude-anthropic cache_write)
[[ "$val" == "18.75" ]] && pass "cache_write rate returned" || fail "cache_write rate (got '$val')"
teardown_tmp_home

# ---- Test 2: rc=1 when provider missing ----
setup_tmp_home
cat > "$TMP_HOME/.fno/config.toml" <<'YAML'
[providers]
active = "claude-anthropic"

[[providers.records]]
id = "claude-anthropic"
name = "Claude Direct"
cli = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
[providers.records.pricing]
input_per_million_usd = 15.0
output_per_million_usd = 75.0
YAML
if get_provider_pricing nonexistent input >/dev/null 2>&1; then
    fail "rc=1 expected for missing provider, got rc=0"
else
    pass "rc=1 when provider missing"
fi
teardown_tmp_home

# ---- Test 3: rc=1 when pricing block absent ----
setup_tmp_home
cat > "$TMP_HOME/.fno/config.toml" <<'YAML'
[providers]
active = "claude-anthropic"

[[providers.records]]
id = "claude-anthropic"
name = "Claude Direct"
cli = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
YAML
if get_provider_pricing claude-anthropic input >/dev/null 2>&1; then
    fail "rc=1 expected when pricing absent, got rc=0"
else
    pass "rc=1 when pricing absent"
fi
teardown_tmp_home

# ---- Test 4: unknown rate kind exits 1 with stderr ----
setup_tmp_home
cat > "$TMP_HOME/.fno/config.toml" <<'YAML'
[providers]
active = "x"

[[providers.records]]
id = "x"
name = "x"
cli = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
pricing = {input_per_million_usd = 1.0, output_per_million_usd = 2.0}
YAML
err=$(get_provider_pricing x bogus 2>&1 >/dev/null)
rc=$?
if [[ $rc -ne 0 ]] && [[ "$err" == *"unknown rate"* ]]; then
    pass "unknown rate fails loudly"
else
    fail "unknown rate (rc=$rc, err='$err')"
fi
teardown_tmp_home

# ---- Test 5: yq path tolerates 2-space indentation ----
# The legacy awk fallback assumes 4-space indentation under records[].
# When yq is available, get_provider_pricing must handle any valid YAML
# indentation (gemini-code-assist finding on PR #208).
if command -v yq &>/dev/null; then
    setup_tmp_home
    cat > "$TMP_HOME/.fno/config.toml" <<'YAML'
[providers]
active = "claude-anthropic"

[[providers.records]]
id = "claude-anthropic"
name = "Claude Direct"
cli = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
[providers.records.pricing]
input_per_million_usd = 15.0
output_per_million_usd = 75.0
YAML
    val=$(get_provider_pricing claude-anthropic input)
    [[ "$val" == "15.0" ]] && pass "yq path: 2-space indentation" || fail "yq path: 2-space indentation (got '$val')"
    val=$(get_provider_pricing claude-anthropic output)
    [[ "$val" == "75.0" ]] && pass "yq path: 2-space output rate" || fail "yq path: 2-space output (got '$val')"
    teardown_tmp_home

    # ---- Test 6: yq path tolerates flow-style pricing map ----
    setup_tmp_home
    cat > "$TMP_HOME/.fno/config.toml" <<'YAML'
[providers]
active = "x"

[[providers.records]]
id = "x"
name = "x"
cli = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
pricing = {input_per_million_usd = 3.0, output_per_million_usd = 15.0}
YAML
    val=$(get_provider_pricing x input)
    [[ "$val" == "3" || "$val" == "3.0" ]] && pass "yq path: flow-style pricing" || fail "yq path: flow-style (got '$val')"
    teardown_tmp_home
else
    echo "  SKIP: yq not installed - tests 5 and 6 (non-canonical YAML) skipped"
fi

# ---- Test 7: the canonical [accounts] block reads the same as [providers] ----
# This reader parses config.toml directly and never reaches the Python loader's
# rename choke point, so both spellings need coverage here. A miss is silent
# (empty + rc=1), which is indistinguishable from "no pricing configured".
setup_tmp_home
cat > "$TMP_HOME/.fno/config.toml" <<'TOML'
[accounts]
active = "readyrule"

[[accounts.records]]
id = "readyrule"
name = "readyrule"
harness = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
[accounts.records.pricing]
input_per_million_usd = 15.0
TOML
val=$(get_provider_pricing readyrule input)
[[ "$val" == "15.0" ]] && pass "canonical accounts block" || fail "canonical accounts block (got '$val')"
teardown_tmp_home

# ---- Test 8: a shadowed legacy block must not leak pricing ----
# The yq and awk paths must agree on WHICH block is authoritative. An awk
# alternation matching both scans the shadowed legacy block too, so a record
# living only there returns a price no reader can see.
setup_tmp_home
cat > "$TMP_HOME/.fno/config.toml" <<'TOML'
[[accounts.records]]
id = "real"
[accounts.records.pricing]
input_per_million_usd = 1.0

[[providers.records]]
id = "ghost"
[providers.records.pricing]
input_per_million_usd = 99.0
TOML
val=$(get_provider_pricing real input)
[[ "$val" == "1.0" ]] && pass "canonical block resolves" || fail "canonical block (got '$val')"
val=$(get_provider_pricing ghost input)
[[ -z "$val" ]] && pass "shadowed legacy block does not leak" || fail "shadowed leak (got '$val')"
teardown_tmp_home

echo ""
echo "Result: ${PASS} pass, ${FAIL} fail"
[[ "$FAIL" == 0 ]]
