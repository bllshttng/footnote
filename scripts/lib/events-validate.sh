#!/usr/bin/env bash
# scripts/lib/events-validate.sh
#
# Bash-side validator for events.jsonl entries. Loads the YAML schema once
# per shell process via yq (or Python fallback) and exposes:
#
#   validate_event TYPE JSON_PAYLOAD
#       rc=0  valid
#       rc=1  invalid (envelope or per-type shape mismatch)
#       rc=2  substrate failure (schema unavailable, parse failed)
#
# Diagnostic style: one line on stderr, naming the failed field.
#
# Compatibility:
#   - bash 3.2 (macOS default). No associative arrays, no process substitution
#     for sourcing.
#   - jq must be on PATH for envelope/shape checks.
#   - yq v4 preferred for cache build; python3 + PyYAML fallback otherwise.
#
# The cache is per-process (`/tmp/events-schema-$$.cache`) and cleaned via
# EXIT trap. Empty cache files force a re-parse on the next call.

set -uo pipefail

# Resolve the schema path with a fallback chain so downstream consumer
# projects (anywhere the fno scripts are invoked from outside this
# plugin's repo) can still find the canonical schema that ships with the
# plugin. Precedence:
#   1. EVENTS_SCHEMA_PATH env var (explicit override)
#   2. ${project repo}/cli/src/fno/events/schema.yaml (this repo)
#   3. lib-relative ../../cli/src/fno/events/schema.yaml (the schema
#      bundled beside THIS lib inside the plugin; self-located via BASH_SOURCE
#      so it resolves from ANY cwd with NO env var set)
#   4. ${FNO_REPO_ROOT}/cli/src/fno/events/schema.yaml (legacy fallback)
#   5. ${CLAUDE_PLUGIN_ROOT}/cli/src/fno/events/schema.yaml (legacy fallback)
# The first readable path wins. If none is readable, the original
# project-root path is preserved so the existing "schema unavailable: <path>"
# diagnostic continues to name a useful location.
#
# NOTE: FNO_REPO_ROOT scopes PROJECT/CONFIG resolution (paths.py:resolve_repo_root),
# NOT schema resolution. Tier 3 self-locates the bundled schema, so an operator
# must NEVER export FNO_REPO_ROOT to fix a "schema unavailable" miss - doing so
# silently repoints `fno config get` at that root (the foreign-project read this
# resolver exists to prevent). Tiers 4-5 remain only for backward compatibility.
_ev_resolve_schema_path() {
    if [[ -n "${EVENTS_SCHEMA_PATH:-}" ]]; then
        printf '%s' "$EVENTS_SCHEMA_PATH"
        return 0
    fi
    local project_root project_path
    project_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
    project_path="${project_root:-.}/cli/src/fno/events/schema.yaml"
    if [[ -r "$project_path" ]]; then
        printf '%s' "$project_path"
        return 0
    fi
    # Tier 3: lib-relative self-location. This file lives at
    # <plugin-root>/scripts/lib/events-validate.sh, so ../../docs/... is the
    # schema that ships with the plugin. BASH_SOURCE[0] is this file regardless
    # of cwd or who sourced it (mirrors phase-verifier.sh), so
    # the bundled schema resolves with no env var set on the bash code path
    # (`fno gate set` -> set-gate.sh -> here). The `:-` guard keeps `set -u`
    # from tripping when sourced from zsh, which does not populate BASH_SOURCE;
    # zsh callers fall through to the env tiers below (no regression, no crash).
    local self_src lib_root lib_candidate
    self_src="${BASH_SOURCE[0]:-}"
    if [[ -n "$self_src" ]]; then
        lib_root="$(cd "$(dirname "$self_src")/../.." 2>/dev/null && pwd)"
        if [[ -n "$lib_root" ]]; then
            lib_candidate="${lib_root}/cli/src/fno/events/schema.yaml"
            if [[ -r "$lib_candidate" ]]; then
                printf '%s' "$lib_candidate"
                return 0
            fi
        fi
    fi
    local root candidate
    for root in "${FNO_REPO_ROOT:-}" "${CLAUDE_PLUGIN_ROOT:-}"; do
        [[ -z "$root" ]] && continue
        candidate="${root}/cli/src/fno/events/schema.yaml"
        if [[ -r "$candidate" ]]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    printf '%s' "$project_path"
}
EVENTS_SCHEMA_PATH="$(_ev_resolve_schema_path)"
EVENTS_SCHEMA_CACHE="${EVENTS_SCHEMA_CACHE:-/tmp/events-schema-$$.cache}"

_ev_warn() { printf '%s\n' "$*" >&2; }

_ev_load_schema_cache() {
    if [[ -s "$EVENTS_SCHEMA_CACHE" ]]; then
        # Validate cache is non-empty JSON; jq -e is fine on a top-level object.
        if jq -e 'type == "object"' "$EVENTS_SCHEMA_CACHE" >/dev/null 2>&1; then
            return 0
        fi
        # Cache truncated or corrupted; re-parse below.
        rm -f "$EVENTS_SCHEMA_CACHE"
    fi

    if [[ ! -r "$EVENTS_SCHEMA_PATH" ]]; then
        _ev_warn "schema unavailable: $EVENTS_SCHEMA_PATH"
        return 2
    fi

    # Prefer yq v4 (`-o=json`). Fall back to python3 yaml.
    if command -v yq >/dev/null 2>&1; then
        if yq -o=json '.' "$EVENTS_SCHEMA_PATH" > "$EVENTS_SCHEMA_CACHE" 2>/dev/null; then
            if jq -e 'type == "object"' "$EVENTS_SCHEMA_CACHE" >/dev/null 2>&1; then
                return 0
            fi
        fi
        rm -f "$EVENTS_SCHEMA_CACHE"
    fi

    for _ev_py in "python3" "uv run --no-project --with pyyaml python3"; do
        if $_ev_py -c '
import json, sys
try:
    import yaml
except ImportError:
    sys.exit(2)
with open(sys.argv[1], "r", encoding="utf-8") as fh:
    json.dump(yaml.safe_load(fh), sys.stdout)
' "$EVENTS_SCHEMA_PATH" > "$EVENTS_SCHEMA_CACHE" 2>/dev/null; then
            if jq -e 'type == "object"' "$EVENTS_SCHEMA_CACHE" >/dev/null 2>&1; then
                return 0
            fi
        fi
        rm -f "$EVENTS_SCHEMA_CACHE"
    done

    _ev_warn "schema unavailable: parse failed"
    return 2
}

# validate_event TYPE PAYLOAD_JSON
#
# TYPE must match an entry in event_types[].name in the manifest.
# PAYLOAD_JSON is a single-line JSON document conforming to the envelope.
validate_event() {
    local type="${1:?type required}"
    local payload="${2:?payload required}"

    if ! _ev_load_schema_cache; then
        return 2
    fi

    # Envelope shape: required fields present.
    # Use `// empty` for optional checks - never `jq -e .field` (rejects null).
    local field val
    for field in ts type source data; do
        val=$(jq -r --arg f "$field" '.[$f] // empty' <<<"$payload" 2>/dev/null)
        if [[ -z "$val" ]]; then
            _ev_warn "event missing required field: $field"
            return 1
        fi
    done

    # source allowed? Enum match first, then the worker regex patterns (x-2901:
    # worker:<id> / stream-worker:<id> are pattern sources, not enum members).
    local src allowed_pattern src_patterns src_ok pat
    src=$(jq -r '.source' <<<"$payload" 2>/dev/null)
    allowed_pattern=$(jq -r '.envelope.properties.source.enum | join("|")' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
    if [[ -z "$allowed_pattern" ]]; then
        _ev_warn "schema malformed: envelope.source.enum missing"
        return 2
    fi
    src_ok=0
    # Anchor with ^...$ for exact enum match.
    if [[ "$src" =~ ^(${allowed_pattern})$ ]]; then
        src_ok=1
    else
        # patterns are already anchored in the schema; test each as a regex.
        src_patterns=$(jq -r '.envelope.properties.source.patterns[]? // empty' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
        if [[ -n "$src_patterns" ]]; then
            while IFS= read -r pat; do
                [[ -z "$pat" ]] && continue
                if [[ "$src" =~ $pat ]]; then
                    src_ok=1
                    break
                fi
            done <<<"$src_patterns"
        fi
    fi
    if [[ "$src_ok" -ne 1 ]]; then
        _ev_warn "unknown source: $src (allowed: $(echo "$allowed_pattern" | tr '|' ',' ) or patterns)"
        return 1
    fi

    # event type known?
    local known
    known=$(jq -r --arg t "$type" '.event_types[] | select(.name == $t) | .name' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
    if [[ -z "$known" ]]; then
        _ev_warn "unknown event type: $type"
        return 1
    fi

    # Required data fields per type, with conditional-gate handling for
    # phase_transition. We read required fields one per line (bash 3.2 compat:
    # no associative arrays, no process substitution).
    #
    # Use `has(...)` for presence rather than `// empty`: jq's `//` treats
    # boolean `false` as null-equivalent, so a valid `gate_bearing: false`
    # field would be reported as missing.
    local required_fields gate_bearing
    required_fields=$(jq -r --arg t "$type" '.event_types[] | select(.name == $t) | .data.required[]?' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
    gate_bearing=$(jq -r 'if (.data | has("gate_bearing")) then (.data.gate_bearing | tostring) else "absent" end' <<<"$payload" 2>/dev/null)

    local f v
    while IFS= read -r f; do
        [[ -z "$f" ]] && continue
        if [[ "$type" == "phase_transition" && "$f" == "gate" && "$gate_bearing" != "true" ]]; then
            continue
        fi
        v=$(jq -r --arg f "$f" 'if (.data | has($f)) then (.data[$f] | tostring) else "" end' <<<"$payload" 2>/dev/null)
        if [[ -z "$v" ]]; then
            _ev_warn "event type $type missing required data field: $f"
            return 1
        fi
    done <<<"$required_fields"

    # Conditional invariant: gate_bearing=true requires data.gate.
    if [[ "$type" == "phase_transition" && "$gate_bearing" == "true" ]]; then
        local gate_val
        gate_val=$(jq -r '.data.gate // empty' <<<"$payload" 2>/dev/null)
        if [[ -z "$gate_val" ]]; then
            _ev_warn "phase_transition with gate_bearing=true must include data.gate"
            return 1
        fi
        # Gate must be in allowlist.
        local known_gate
        known_gate=$(jq -r --arg g "$gate_val" '.gates[] | select(. == $g)' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
        if [[ -z "$known_gate" ]]; then
            _ev_warn "unknown gate: $gate_val"
            return 1
        fi
    fi

    # mission_complete: status enum check.
    # Use mc_status (not status) - zsh treats `status` as a readonly builtin
    # name so callers that source this file from zsh would crash.
    if [[ "$type" == "mission_complete" ]]; then
        local mc_status enum_match
        mc_status=$(jq -r '.data.status // empty' <<<"$payload" 2>/dev/null)
        if [[ -n "$mc_status" ]]; then
            enum_match=$(jq -r --arg s "$mc_status" '.event_types[] | select(.name == "mission_complete") | .data.properties.status.enum[]? | select(. == $s)' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
            if [[ -z "$enum_match" ]]; then
                _ev_warn "unknown status: $mc_status"
                return 1
            fi
        fi
    fi

    # skill_eval_finding: dimension + verdict enum checks (observer harness,
    # x-57a5) - same chokepoint rationale as mission_complete/human_touch above.
    if [[ "$type" == "skill_eval_finding" ]]; then
        local dim verdict enum_match
        dim=$(jq -r '.data.dimension // empty' <<<"$payload" 2>/dev/null)
        if [[ -n "$dim" ]]; then
            enum_match=$(jq -r --arg d "$dim" '.event_types[] | select(.name == "skill_eval_finding") | .data.properties.dimension.enum[]? | select(. == $d)' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
            if [[ -z "$enum_match" ]]; then
                _ev_warn "unknown dimension: $dim"
                return 1
            fi
        fi
        verdict=$(jq -r '.data.verdict // empty' <<<"$payload" 2>/dev/null)
        if [[ -n "$verdict" ]]; then
            enum_match=$(jq -r --arg v "$verdict" '.event_types[] | select(.name == "skill_eval_finding") | .data.properties.verdict.enum[]? | select(. == $v)' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
            if [[ -z "$enum_match" ]]; then
                _ev_warn "unknown verdict: $verdict"
                return 1
            fi
        fi
    fi

    # review_attestation: verdict enum check (x-e703 trust-core gate event) -
    # mirrors the Python validator so a producer typo fails loud in both.
    if [[ "$type" == "review_attestation" ]]; then
        local ra_verdict enum_match
        ra_verdict=$(jq -r '.data.verdict // empty' <<<"$payload" 2>/dev/null)
        if [[ -n "$ra_verdict" ]]; then
            enum_match=$(jq -r --arg v "$ra_verdict" '.event_types[] | select(.name == "review_attestation") | .data.properties.verdict.enum[]? | select(. == $v)' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
            if [[ -z "$enum_match" ]]; then
                _ev_warn "unknown verdict: $ra_verdict"
                return 1
            fi
        fi
    fi

    # gate_escape: reason enum check (x-f894 autonomy-debt counter) - mirrors
    # the Python validator so a producer typo fails loud in both. reason is a
    # required field (caught above if absent); this rejects a present-but-bad
    # value so it is never a silent bucket in the retro ranking.
    if [[ "$type" == "gate_escape" ]]; then
        local ge_reason enum_match
        ge_reason=$(jq -r '.data.reason // empty' <<<"$payload" 2>/dev/null || true)
        if [[ -n "$ge_reason" ]]; then
            enum_match=$(jq -r --arg r "$ge_reason" '.event_types[] | select(.name == "gate_escape") | .data.properties.reason.enum[]? | select(. == $r)' "$EVENTS_SCHEMA_CACHE" 2>/dev/null || true)
            if [[ -z "$enum_match" ]]; then
                _ev_warn "unknown reason: $ge_reason"
                return 1
            fi
        fi
    fi

    # Size cap: encode data and check bytes.
    local max_bytes data_size
    max_bytes=$(jq -r '.limits.max_data_bytes // 65536' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
    # `jq -c .data` gives compact JSON; -n removes trailing newline so wc -c
    # counts only payload bytes.
    data_size=$(jq -cn --argjson p "$payload" '$p.data' | tr -d '\n' | wc -c | tr -d ' ')
    if (( data_size > max_bytes )); then
        _ev_warn "event data exceeds max_data_bytes (got $data_size, limit $max_bytes)"
        return 1
    fi

    return 0
}

# Best-effort cleanup so cache files don't accumulate in /tmp.
trap 'rm -f "$EVENTS_SCHEMA_CACHE" 2>/dev/null || true' EXIT
