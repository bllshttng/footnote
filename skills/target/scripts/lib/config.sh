#!/usr/bin/env bash
# Project config loader for target
# Reads from settings.yaml (config: section) with global → local override
#
# Lookup order (local wins):
#   1. .fno/settings.yaml          (project-local override)
#   2. ~/.fno/settings.yaml  (global defaults)
#
# Config section format (inside settings.yaml):
#   config:
#     expertise: frontend
#     max_iterations: 20
#     no_external: true
#     no_docs: false
#     budget_cap: 20
#     notifications:
#       enabled: true
#
# Backwards compatibility:
#   Falls back to .fno/config.yaml if no settings.yaml found

# Use CONFIG_FILE from paths.sh if available; fall back to hardcoded default.
if [[ -z "${CONFIG_FILE:-}" ]] && command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$_PATHS_SH" ]] && source "$_PATHS_SH" 2>/dev/null || true
    unset _PATHS_SH
fi
# GLOBAL_SETTINGS is the per-user global config and must NEVER alias CONFIG_FILE
# (the ACTIVE config = the project-local file when one exists; aliasing it hid
# every global-only key from bash consumers - ab-5d6c3d47). Honor
# FNO_GLOBAL_SETTINGS_PATH so bash matches Python's _global_settings_path().
GLOBAL_SETTINGS="${GLOBAL_SETTINGS:-${FNO_GLOBAL_SETTINGS_PATH:-$HOME/.fno/settings.yaml}}"
# LOCAL_SETTINGS is the active project config. Prefer CONFIG_FILE (the stub's
# resolved local/canonical-root path) when it names a file distinct from the
# global one, so a linked worktree whose .fno/settings.yaml is not symlinked
# still reads project keys; otherwise the repo-relative fallback.
if [[ -z "${LOCAL_SETTINGS:-}" ]]; then
    if [[ -n "${CONFIG_FILE:-}" && "${CONFIG_FILE}" != "${GLOBAL_SETTINGS}" ]]; then
        LOCAL_SETTINGS="$CONFIG_FILE"
    else
        LOCAL_SETTINGS=".fno/settings.yaml"
    fi
fi

# Emit a one-time warning when yq is absent and a dotted key is requested.
# Printed to stderr; suppressed after the first call per shell session.
_YQ_MISSING_WARNED=0
_warn_no_yq_once() {
    local key="$1"
    if [[ "$_YQ_MISSING_WARNED" -eq 0 ]]; then
        echo "[fno/config] warn: yq not found - nested key '${key}' (and other dotted keys like auto_merge.*) cannot be read from settings.yaml. Install yq via: brew install yq  |  apt install yq  |  mise use yq" >&2
        _YQ_MISSING_WARNED=1
    fi
}
LEGACY_CONFIG="${LEGACY_CONFIG:-.fno/config.yaml}"
# Claude Code native project settings (JSON)
CLAUDE_SETTINGS="${CLAUDE_SETTINGS:-.claude/settings.json}"
CLAUDE_SETTINGS_LOCAL="${CLAUDE_SETTINGS_LOCAL:-.claude/settings.local.json}"

# Extract a value from Claude Code's .claude/settings.json or settings.local.json
# Maps Claude Code native keys to abilities config keys:
#   plansDirectory → plans.focused_path, plans.full_path
_get_from_claude_settings() {
    local file="$1" key="$2"
    [[ -f "$file" ]] || return 1
    command -v jq &>/dev/null || return 1

    # Map abilities config keys → Claude Code JSON keys
    local json_key=""
    case "$key" in
        plans.focused_path|plans.full_path) json_key="plansDirectory" ;;
        *) return 1 ;;  # Only mapped keys are supported
    esac

    local value
    value=$(jq -r ".${json_key} // empty" "$file" 2>/dev/null)
    if [[ -n "$value" ]]; then
        echo "$value"
        return 0
    fi
    return 1
}

# Extract a value from the config: section of a settings.yaml file
# Uses sed to isolate the config: block, then grep for the key
_get_from_settings() {
    local file="$1" key="$2"
    [[ -f "$file" ]] || return 1

    # For dotted keys like "notifications.enabled", use yq if available
    if [[ "$key" == *.* ]]; then
        if command -v yq &>/dev/null; then
            local value
            value=$(yq ".config.${key}" "$file" 2>/dev/null)
            if [[ -n "$value" && "$value" != "null" ]]; then
                echo "$value"
                return 0
            fi
            return 1
        else
            _warn_no_yq_once "$key"
            return 1
        fi
    fi

    # Simple keys: extract from config: block (indented by 2+ spaces)
    local value
    value=$(sed -n '/^config:/,/^[^ ]/{
        /^  '"${key}"':/p
    }' "$file" 2>/dev/null \
        | head -1 \
        | sed "s/^[[:space:]]*${key}:[[:space:]]*//" \
        | tr -d '"' | tr -d "'")
    if [[ -n "$value" ]]; then
        echo "$value"
        return 0
    fi
    return 1
}

# Extract a value from legacy flat config.yaml (backwards compat)
_get_from_legacy() {
    local file="$1" key="$2"
    [[ -f "$file" ]] || return 1

    if [[ "$key" == *.* ]]; then
        if command -v yq &>/dev/null; then
            local value
            value=$(yq ".$key" "$file" 2>/dev/null)
            if [[ -n "$value" && "$value" != "null" ]]; then
                echo "$value"
                return 0
            fi
            return 1
        else
            _warn_no_yq_once "$key"
            return 1
        fi
    fi

    local value
    value=$(grep -E "^${key}:" "$file" 2>/dev/null \
        | head -1 \
        | sed "s/^${key}:[[:space:]]*//" \
        | tr -d '"' | tr -d "'")
    if [[ -n "$value" ]]; then
        echo "$value"
        return 0
    fi
    return 1
}

# Extract a value from the workspace topology in a settings.yaml file.
# Canonical location is config.work; legacy top-level work: and the older
# workspace: section are still read for back-compat. Always tries the canonical
# path first, so a folded (config.work) file and an un-migrated (top-level work)
# file both resolve.
_get_from_workspace() {
    local file="$1" key="$2"
    [[ -f "$file" ]] || return 1

    # Dotted keys resolve via yq; canonical config.work first, then the legacy
    # top-level work:/workspace: sections, so folded and un-migrated files resolve.
    if [[ "$key" == *.* ]] && command -v yq &>/dev/null; then
        local value root
        for root in ".config.work" ".work" ".workspace"; do
            value=$(yq "${root}.${key}" "$file" 2>/dev/null)
            [[ -n "$value" && "$value" != "null" ]] && { echo "$value"; return 0; }
        done
        return 1
    fi

    # No-yq fallback for simple keys: legacy top-level only (the canonical
    # config.work map is read via yq above, which the dotted keys always use).
    local value section
    for section in "work" "workspace"; do
        value=$(sed -n '/^'"${section}"':/,/^[^ ]/{ /^  '"${key}"':/p; }' "$file" 2>/dev/null \
            | head -1 | sed "s/^[[:space:]]*${key}:[[:space:]]*//" | tr -d '"' | tr -d "'")
        [[ -n "$value" ]] && { echo "$value"; return 0; }
    done
    return 1
}

get_config() {
    local key="${1:?key required}"
    local default="${2:-}"

    local value

    # 1. Claude Code settings.local.json (highest priority — user overrides)
    if value=$(_get_from_claude_settings "$CLAUDE_SETTINGS_LOCAL" "$key"); then
        echo "$value"
        return 0
    fi

    # 2. Claude Code settings.json (project-level, checked into repo)
    if value=$(_get_from_claude_settings "$CLAUDE_SETTINGS" "$key"); then
        echo "$value"
        return 0
    fi

    # 3. Local settings.yaml
    if value=$(_get_from_settings "$LOCAL_SETTINGS" "$key"); then
        echo "$value"
        return 0
    fi

    # 4. Global settings.yaml
    if value=$(_get_from_settings "$GLOBAL_SETTINGS" "$key"); then
        echo "$value"
        return 0
    fi

    # 5. Legacy .fno/config.yaml (backwards compat)
    if value=$(_get_from_legacy "$LEGACY_CONFIG" "$key"); then
        echo "$value"
        return 0
    fi

    echo "$default"
}

# Read from work: section of settings.yaml (local → global)
# Also checks legacy workspace: section for backwards compatibility
get_workspace() {
    local key="${1:?key required}"
    local default="${2:-}"

    local value

    # 1. Local settings.yaml
    if value=$(_get_from_workspace "$LOCAL_SETTINGS" "$key"); then
        echo "$value"
        return 0
    fi

    # 2. Global settings.yaml
    if value=$(_get_from_workspace "$GLOBAL_SETTINGS" "$key"); then
        echo "$value"
        return 0
    fi

    echo "$default"
}

# Check if a config key is truthy (true, yes, 1)
config_is_true() {
    local key="${1:?key required}"
    local value
    value=$(get_config "$key" "false")
    [[ "$value" == "true" || "$value" == "yes" || "$value" == "1" ]]
}

# Alias for clarity — skills can use either get_config or get_config_or_default
# Both accept KEY DEFAULT and return the default when the key is missing.
get_config_or_default() {
    get_config "$@"
}

# ── Auto-merge config getters ────────────────────────────────────
# Read auto_merge settings from config.auto_merge.* section.
# The shipping skills (target, megawalk) read from this
# shared top-level section - not scoped per-skill.

# Usage: get_auto_merge_enabled  → "true" or "false"
get_auto_merge_enabled() {
    get_config "auto_merge.enabled" "false"
}

# Usage: get_auto_merge_strategy  → "merge"|"squash"|"rebase"
get_auto_merge_strategy() {
    local value
    value=$(get_config "auto_merge.merge_strategy" "merge")
    case "$value" in
        merge|squash|rebase) echo "$value" ;;
        *)
            echo "Warning: invalid merge_strategy '$value' in settings.yaml - must be merge|squash|rebase. Falling back to 'merge'." >&2
            echo "merge"
            ;;
    esac
}

# Usage: get_auto_merge_delete_branch  → "true" or "false"
get_auto_merge_delete_branch() {
    get_config "auto_merge.delete_branch_on_merge" "true"
}

# Usage: get_auto_merge_require_checks  → "true" or "false"
get_auto_merge_require_checks() {
    get_config "auto_merge.require_checks_pass" "true"
}

# Usage: get_auto_merge_conflict_resolution  → "opus"|"fail"
get_auto_merge_conflict_resolution() {
    local value
    value=$(get_config "auto_merge.conflict_resolution" "opus")
    case "$value" in
        opus|fail) echo "$value" ;;
        *)
            echo "Warning: invalid conflict_resolution '$value' - must be opus|fail. Falling back to 'opus'." >&2
            echo "opus"
            ;;
    esac
}

# Usage: is_auto_merge_allowed_for <skill>  → exit 0 if allowed, 1 if restricted
# Takes one arg: "target" | "megawalk"
is_auto_merge_allowed_for() {
    local skill="$1"
    [[ "$(get_auto_merge_enabled)" == "true" ]] || return 1
    # If allowed_invokers is not set, all skills are allowed
    local allowed
    allowed=$(get_config "auto_merge.allowed_invokers" "")
    if [[ -z "$allowed" ]]; then
        return 0
    fi
    # allowed is a YAML list rendered as a string like "[target, megawalk]"
    if echo "$allowed" | grep -q "\b$skill\b"; then
        return 0
    fi
    return 1
}

# ── Domain profile functions ──────────────────────────────────────
# Read domain profiles from settings.yaml `domains:` section.
# Domain profiles define what skill/command runs at each pipeline phase.
# Undeclared phases inherit from code defaults.

# Warn once when yq is missing and a non-code domain is queried
_warn_no_yq_once() {
    [[ -n "${_YQ_WARNED:-}" ]] && return
    echo "WARNING: yq not installed — domain profile features degraded (falling back to code defaults)" >&2
    _YQ_WARNED=1
}

# Code defaults: the implicit "code" domain phase mapping
# Associative arrays require bash 4+ — use function lookup for bash 3.2 compat
_code_default_phase() {
    local phase="$1"
    case "$phase" in
        execute)  echo "fno:do waves" ;;
        review)   echo "fno:review" ;;
        validate) echo "" ;;  # detected from project (npm run build, pytest, etc.)
        ship)     echo "fno:pr create" ;;
        external) echo "fno:pr check" ;;
        docs)     echo "fno:ship-docs" ;;
        *)        echo "" ;;
    esac
}

# Get a domain's phase override from settings.yaml
# Falls back to code default if the domain or phase is not defined.
# Usage: get_domain_phase "research" "review"  → "fno:fact-check" or code default
get_domain_phase() {
    local domain="${1:?domain required}"
    local phase="${2:?phase required}"

    # "code" domain always uses defaults
    if [[ "$domain" == "code" ]]; then
        _code_default_phase "$phase"
        return 0
    fi

    # Try to read from settings.yaml domains section (requires yq)
    local value=""
    if command -v yq &>/dev/null; then
        for settings_file in "$LOCAL_SETTINGS" "$GLOBAL_SETTINGS"; do
            if [[ -f "$settings_file" ]]; then
                value=$(yq ".domains.${domain}.phases.${phase}" "$settings_file" 2>/dev/null)
                if [[ -n "$value" && "$value" != "null" ]]; then
                    echo "$value"
                    return 0
                fi
            fi
        done
    else
        # No yq available — warn and fall back to code defaults.
        # sed-based YAML parsing is too fragile for nested domain profiles.
        _warn_no_yq_once
    fi

    # Fall back to code default
    _code_default_phase "$phase"
}

# Resolve which domain to use via the lookup chain:
#   1. --domain CLI flag (explicit)
#   2. Plan's domain: field (from the plan doc's frontmatter)
#   3. Settings default (config.default_domain)
#   4. "code" (implicit default)
# Usage: resolve_domain "$FLAG_DOMAIN" "$PLAN_DOMAIN" "$SETTINGS_DEFAULT"
resolve_domain() {
    local flag="${1:-}" plan_domain="${2:-}" settings_default="${3:-}"
    if [[ -n "$flag" ]]; then echo "$flag"
    elif [[ -n "$plan_domain" ]]; then echo "$plan_domain"
    elif [[ -n "$settings_default" ]]; then echo "$settings_default"
    else echo "code"
    fi
}

# Check if a domain allows autonomous execution
# Returns 0 (true) if allowed, 1 (false) if blocked.
# Usage: domain_allows_claw "trading" → returns 1 (false)
domain_allows_claw() {
    local domain="${1:?domain required}"

    # "code" domain always allows claw
    if [[ "$domain" == "code" ]]; then
        return 0
    fi

    # Without yq, fail-safe: DENY autonomous execution for non-code domains.
    # This prevents allow_claw: false from being silently bypassed.
    if ! command -v yq &>/dev/null; then
        _warn_no_yq_once
        return 1  # fail-safe: deny
    fi

    local value=""
    for settings_file in "$LOCAL_SETTINGS" "$GLOBAL_SETTINGS"; do
        if [[ -f "$settings_file" ]]; then
            value=$(yq ".domains.${domain}.allow_claw" "$settings_file" 2>/dev/null)
            if [[ -n "$value" && "$value" != "null" ]]; then
                [[ "$value" == "true" ]]
                return $?
            fi
        fi
    done

    # Default: allow claw (domain exists but has no allow_claw setting)
    return 0
}

# Check if a domain exists in settings.yaml
# Usage: domain_exists "research" → returns 0 (true) if defined
domain_exists() {
    local domain="${1:?domain required}"

    if [[ "$domain" == "code" ]]; then
        return 0  # code always exists (implicit)
    fi

    if ! command -v yq &>/dev/null; then
        _warn_no_yq_once
        return 1  # can't check without yq
    fi

    for settings_file in "$LOCAL_SETTINGS" "$GLOBAL_SETTINGS"; do
        if [[ -f "$settings_file" ]]; then
            local exists
            exists=$(yq ".domains.${domain}" "$settings_file" 2>/dev/null)
            if [[ -n "$exists" && "$exists" != "null" ]]; then
                return 0
            fi
        fi
    done

    return 1
}

# ── Config keys reference ──────────────────────────────────────
# Lookup order (first match wins):
#   1. .claude/settings.local.json   (user overrides, not committed)
#   2. .claude/settings.json         (project-level, committed)
#   3. .fno/settings.yaml      (abilities config, local override)
#   4. ~/.fno/settings.yaml  (abilities config, global)
#   5. .fno/config.yaml        (legacy, backwards compat)
#
# Claude Code settings.json key mapping:
#   "plansDirectory" → plans.focused_path, plans.full_path
#
# config.expertise: ""              # Default expertise injection
# config.max_iterations: 40         # Iteration cap
# config.budget_cap: 25             # Max spend in USD
# config.no_external: false         # Skip external AI review          (-E)
# config.no_docs: false             # Skip docs generation             (-D)
# config.no_browser: false          # Skip browser testing             (-B)
# config.no_how_to: false           # Skip how-to guide generation     (-H)
# config.autonomous_max_turns: 15   # Max turns per session (autonomous/claw)
# config.autonomous_budget: 25      # Budget per session (autonomous/claw)
# config.external_reviewer: gemini  # gemini | coderabbit | claude | codex | none
#
# config.plans.focused_path: ""  # Flat plan save location (prefer .claude/settings.json plansDirectory)
# config.plans.full_path: ""    # Folder plan save location (prefer .claude/settings.json plansDirectory)
#
# config.docs.how_to_guides: false             # How-to generation (off by default)
# config.docs.how_to_path: "docs/howto"        # How-to save location
# config.docs.architecture_path: "docs/architecture"  # Architecture docs location
# config.docs.test_plan_path: "docs/test-plans"       # Test plan location
# config.docs.roles: []                        # User roles for how-to guides
#
# config.linear.enabled: (absent)              # Linear integration (absent = disabled)
# config.linear.team: ""                       # Linear team prefix
# config.linear.workspace: ""                  # Linear workspace slug
#
# config.default_domain: code              # Default domain for target
#
# domains.{name}.phases.execute: skill     # Override execute phase
# domains.{name}.phases.review: skill      # Override review phase
# domains.{name}.phases.validate: cmd      # Override validate phase
# domains.{name}.phases.ship: skill        # Override ship phase
# domains.{name}.phases.external: skill    # Override external phase
# domains.{name}.phases.docs: skill        # Override docs phase
# domains.{name}.allow_claw: true          # Allow autonomous mode (default: true)
#
# CLI shorthand: -DEBH = --lean/--quick (skip all optional phases)

# ── Provider rate-card getters (Phase 02 of provider rotation failover) ──
# Read pricing.* fields from a specific record under config.providers.records.
# v0 surfaces only the four named rates; the math that consumes them lives
# in the cost ledger (Spec 2.5 follow-up).
#
# Usage: get_provider_pricing <provider_id> <rate>
#   <rate> ∈ input | output | cache_read | cache_write
# Returns: float on stdout, empty string + rc=1 if absent/unknown.

get_provider_pricing() {
    local provider_id="${1:?provider_id required}"
    local rate="${2:?rate required (input|output|cache_read|cache_write)}"
    local key
    case "$rate" in
        input)       key="input_per_million_usd" ;;
        output)      key="output_per_million_usd" ;;
        cache_read)  key="cache_read_per_million_usd" ;;
        cache_write) key="cache_write_per_million_usd" ;;
        *)
            echo "get_provider_pricing: unknown rate '$rate' (want input|output|cache_read|cache_write)" >&2
            return 1
            ;;
    esac

    local file
    for file in "$LOCAL_SETTINGS" "$GLOBAL_SETTINGS"; do
        [[ -f "$file" ]] || continue
        local value
        # Prefer yq for robust YAML parsing (handles arbitrary indentation,
        # quoted ids, flow-style maps). Fall back to awk if yq is missing -
        # the awk path assumes 4-space indentation under records[].
        if command -v yq &>/dev/null; then
            value=$(yq -r \
                ".config.providers.records[] | select(.id == \"$provider_id\") | .pricing.${key} // \"\"" \
                "$file" 2>/dev/null)
            # yq prints "null" when a path is absent without `// \"\"`; guard anyway.
            [[ "$value" == "null" ]] && value=""
            if [[ -n "$value" ]]; then
                echo "$value"
                return 0
            fi
            continue
        fi
        _warn_no_yq_once "providers.records[].pricing.${key}"
        value=$(awk -v target="$provider_id" -v want="$key" '
            /^[[:space:]]*records:/ { in_records=1; next }
            in_records && /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/ {
                # New record: capture its id
                cur_id = $0
                sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/, "", cur_id)
                gsub(/["'\'']/, "", cur_id)
                gsub(/[[:space:]]/, "", cur_id)
                in_pricing = 0
                next
            }
            in_records && /^[[:space:]]*pricing:/ {
                if (cur_id == target) in_pricing = 1
                next
            }
            in_pricing && cur_id == target {
                # match "    key: value"
                line = $0
                if (line ~ "^[[:space:]]+" want ":") {
                    sub("^[[:space:]]+" want ":[[:space:]]*", "", line)
                    gsub(/["'\'']/, "", line)
                    gsub(/[[:space:]]/, "", line)
                    print line
                    exit
                }
                # New top-level key under the record (no leading 4+ spaces) ends pricing
                if (line ~ "^[[:space:]]{2,4}[a-zA-Z]") in_pricing = 0
            }
            # End of records block
            in_records && /^[a-zA-Z]/ { in_records = 0 }
        ' "$file" 2>/dev/null)
        if [[ -n "$value" ]]; then
            echo "$value"
            return 0
        fi
    done
    return 1
}
