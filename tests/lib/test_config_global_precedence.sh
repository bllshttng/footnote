#!/usr/bin/env bash
# test_config_global_precedence.sh - regression for ab-5d6c3d47.
#
# GLOBAL_SETTINGS must NEVER alias CONFIG_FILE. CONFIG_FILE (emitted by
# `fno paths shell-stub`) is the ACTIVE config = the project-local
# .fno/config.toml whenever one exists; aliasing it made both merge tiers
# point at the local file, hiding every global-only key from bash consumers.
#
# CRITICAL test discipline (see the design's Domain Pitfalls): the buggy
# resolution happens at config.sh SOURCE time, so every case sources config.sh
# FRESH in a subshell with a controlled environment. Pinning GLOBAL_SETTINGS
# after sourcing short-circuits the `${GLOBAL_SETTINGS:-...}` default and masks
# the exact bug, so the global-resolution cases must `unset GLOBAL_SETTINGS`.
# Each case also sets CONFIG_FILE explicitly (non-empty) so the stub-sourcing
# block at the top of config.sh is skipped and the test stays hermetic.
#
# Fixtures are flat config.toml (the post-hard-cut format config.sh reads via
# `yq -p toml`); there is no `config:` wrapper.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CONFIG_SH="$REPO_ROOT/scripts/lib/config.sh"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }
assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$actual" == "$expected" ]]; then
        pass "$desc"
    else
        fail "$desc (expected='$expected', got='$actual')"
    fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# ---------------------------------------------------------------------------
# AC1-HP: global-only key is read when a project-local file exists.
# (red/green case) local file holds only a local key; the requested key lives
# only in the global fixture. Pre-fix: GLOBAL_SETTINGS aliases CONFIG_FILE (the
# local file), the global key is invisible -> caller default. Post-fix: found.
# ---------------------------------------------------------------------------
echo ""
echo "AC1-HP: global-only key read when a local file exists"
mkdir -p "$TMP/ac1hp/.fno"
cat > "$TMP/ac1hp/.fno/config.toml" <<'TOML'
local_only_key = "local_value"
TOML
cat > "$TMP/ac1hp/global.toml" <<'TOML'
global_only_key = "GLOBAL_WINS"
TOML
result=$(
    unset GLOBAL_SETTINGS LOCAL_SETTINGS
    export CONFIG_FILE="$TMP/ac1hp/.fno/config.toml"
    export FNO_GLOBAL_SETTINGS_PATH="$TMP/ac1hp/global.toml"
    source "$CONFIG_SH"
    get_config "global_only_key" "DEFAULT"
)
assert_eq "global-only key resolves to the global value (not the default)" "GLOBAL_WINS" "$result"

# ---------------------------------------------------------------------------
# AC2-HP: a key present in both files still resolves local-over-global.
# ---------------------------------------------------------------------------
echo ""
echo "AC2-HP: local key wins over global for a shared key"
mkdir -p "$TMP/ac2hp/.fno"
cat > "$TMP/ac2hp/.fno/config.toml" <<'TOML'
shared_key = "from_local"
TOML
cat > "$TMP/ac2hp/global.toml" <<'TOML'
shared_key = "from_global"
TOML
result=$(
    unset GLOBAL_SETTINGS LOCAL_SETTINGS
    export CONFIG_FILE="$TMP/ac2hp/.fno/config.toml"
    export FNO_GLOBAL_SETTINGS_PATH="$TMP/ac2hp/global.toml"
    source "$CONFIG_SH"
    get_config "shared_key" "DEFAULT"
)
assert_eq "shared key resolves local-over-global" "from_local" "$result"

# ---------------------------------------------------------------------------
# AC1-ERR: neither config file exists -> default, exit 0 under set -u.
# ---------------------------------------------------------------------------
echo ""
echo "AC1-ERR: no config files present"
result=$(
    unset GLOBAL_SETTINGS LOCAL_SETTINGS
    export CONFIG_FILE="$TMP/absent/local.toml"
    export FNO_GLOBAL_SETTINGS_PATH="$TMP/absent/global.toml"
    source "$CONFIG_SH"
    get_config "anything" "FALLBACK"
)
ac1err_exit=$?
assert_eq "missing files -> caller default" "FALLBACK" "$result"
assert_eq "missing files -> exit 0 (no unbound-variable crash)" "0" "$ac1err_exit"

# ---------------------------------------------------------------------------
# AC1-EDGE: no local file; CONFIG_FILE resolves to the global file. The key
# lives only in global; LOCAL_SETTINGS falling to the absent relative path
# must not error, and the global value is still read.
# ---------------------------------------------------------------------------
echo ""
echo "AC1-EDGE: no local file, key only in global"
mkdir -p "$TMP/ac1edge"
cat > "$TMP/ac1edge/global.toml" <<'TOML'
global_key = "edge_global"
TOML
result=$(
    unset GLOBAL_SETTINGS LOCAL_SETTINGS
    # CONFIG_FILE == the resolved global file (no local file present).
    export CONFIG_FILE="$TMP/ac1edge/global.toml"
    export FNO_GLOBAL_SETTINGS_PATH="$TMP/ac1edge/global.toml"
    cd "$TMP/ac1edge"   # so the relative `.fno/config.toml` LOCAL fallback is absent
    source "$CONFIG_SH"
    get_config "global_key" "DEFAULT"
)
assert_eq "no-local: global key still read, no error on absent relative LOCAL" "edge_global" "$result"

# ---------------------------------------------------------------------------
# AC2-EDGE: empty FNO_GLOBAL_SETTINGS_PATH is treated as unset, falling back to
# $HOME/.fno/config.toml (matching Python's `:-` semantics). HOME is scoped
# to a temp dir inside the subshell so the real ~/.fno is never touched.
# ---------------------------------------------------------------------------
echo ""
echo "AC2-EDGE: empty FNO_GLOBAL_SETTINGS_PATH falls back to \$HOME"
mkdir -p "$TMP/ac2edge/home/.fno"
cat > "$TMP/ac2edge/home/.fno/config.toml" <<'TOML'
home_key = "from_home"
TOML
result=$(
    unset GLOBAL_SETTINGS LOCAL_SETTINGS
    export HOME="$TMP/ac2edge/home"
    export CONFIG_FILE="$TMP/ac2edge/absent-local.toml"
    export FNO_GLOBAL_SETTINGS_PATH=""   # empty == unset
    source "$CONFIG_SH"
    get_config "home_key" "DEFAULT"
)
assert_eq "empty FNO_GLOBAL_SETTINGS_PATH -> \$HOME/.fno/config.toml" "from_home" "$result"

# ---------------------------------------------------------------------------
# AC1-FR: caller-pinned LOCAL_SETTINGS and GLOBAL_SETTINGS survive the new
# resolution unchanged (the `${VAR:-...}` default and the `[[ -z ... ]]` guard
# both honor pins). CONFIG_FILE is set to a decoy to prove it is ignored when
# pins are present. Keeps test_config_auto_merge.sh / test_rebase_resolve.sh
# style env-pinning working.
# ---------------------------------------------------------------------------
echo ""
echo "AC1-FR: caller pins survive the new resolution"
mkdir -p "$TMP/acfr/local/.fno" "$TMP/acfr/global"
cat > "$TMP/acfr/local/.fno/config.toml" <<'TOML'
pinned_local_key = "local_pinned"
TOML
cat > "$TMP/acfr/global/config.toml" <<'TOML'
pinned_global_key = "global_pinned"
TOML
result_local=$(
    export LOCAL_SETTINGS="$TMP/acfr/local/.fno/config.toml"
    export GLOBAL_SETTINGS="$TMP/acfr/global/config.toml"
    export CONFIG_FILE="$TMP/acfr/decoy.toml"
    export FNO_GLOBAL_SETTINGS_PATH="$TMP/acfr/decoy-global.toml"
    source "$CONFIG_SH"
    get_config "pinned_local_key" "DEFAULT"
)
result_global=$(
    export LOCAL_SETTINGS="$TMP/acfr/local/.fno/config.toml"
    export GLOBAL_SETTINGS="$TMP/acfr/global/config.toml"
    export CONFIG_FILE="$TMP/acfr/decoy.toml"
    export FNO_GLOBAL_SETTINGS_PATH="$TMP/acfr/decoy-global.toml"
    source "$CONFIG_SH"
    get_config "pinned_global_key" "DEFAULT"
)
assert_eq "pinned LOCAL_SETTINGS honored (CONFIG_FILE decoy ignored)" "local_pinned" "$result_local"
assert_eq "pinned GLOBAL_SETTINGS honored (FNO_GLOBAL_SETTINGS_PATH decoy ignored)" "global_pinned" "$result_global"

# ---------------------------------------------------------------------------
# AC2-FR: the autolaunch symptom. With a sparse local file present, a nested
# global-only key (target.auto_launch_on_blueprint) must read true. Dotted keys
# require yq, so this case is yq-guarded (the design's Domain Pitfall).
# ---------------------------------------------------------------------------
echo ""
echo "AC2-FR: nested global-only key read with a sparse local file present (yq)"
if command -v yq >/dev/null 2>&1; then
    mkdir -p "$TMP/ac2fr/.fno"
    cat > "$TMP/ac2fr/.fno/config.toml" <<'TOML'
parking_lot_path = "/some/local/path"
TOML
    cat > "$TMP/ac2fr/global.toml" <<'TOML'
[target]
auto_launch_on_blueprint = true
TOML
    result=$(
        unset GLOBAL_SETTINGS LOCAL_SETTINGS
        export CONFIG_FILE="$TMP/ac2fr/.fno/config.toml"
        export FNO_GLOBAL_SETTINGS_PATH="$TMP/ac2fr/global.toml"
        source "$CONFIG_SH"
        get_config "target.auto_launch_on_blueprint" "false"
    )
    assert_eq "nested global-only key reads true (autolaunch symptom gone)" "true" "$result"
else
    echo "  SKIP: yq not installed - nested-key case not exercised"
fi

# ---- Summary ----
echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"

[[ $FAIL -gt 0 ]] && exit 1
exit 0
