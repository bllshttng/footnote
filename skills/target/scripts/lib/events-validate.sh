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
EVENTS_SCHEMA_CACHE="${EVENTS_SCHEMA_CACHE:-${TMPDIR:-/tmp}/events-schema-${BASHPID:-$$}-${RANDOM:-0}.cache}"

_ev_warn() { printf '%s\n' "$*" >&2; }

_ev_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        printf '%s' "$1" | sha256sum | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        printf '%s' "$1" | shasum -a 256 | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        printf '%s' "$1" | openssl dgst -sha256 | awk '{print $NF}'
    else
        return 2
    fi
}

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

    # Claim records have two truthful PID shapes. A null PID needs the
    # positive marker and a TTL expiry; an integer PID must not carry the
    # marker. This mirrors Python events.validate so a missing instrument
    # cannot read as an intentional PID-unavailable claim.
    if [[ "$type" == claim_* ]] && jq -e '.data | has("pid")' <<<"$payload" >/dev/null 2>&1; then
        local pid_kind pid_unavailable marker_type expiry
        pid_kind=$(jq -r 'if .data.pid == null then "null" else "value" end' <<<"$payload" 2>/dev/null)
        pid_unavailable=$(jq -r 'if ((.data | has("pid_unavailable")) and (.data.pid_unavailable | type == "boolean") and .data.pid_unavailable) then "true" else "false" end' <<<"$payload" 2>/dev/null)
        marker_type=$(jq -r 'if ((.data | has("pid_unavailable"))) then (.data.pid_unavailable | type) else "absent" end' <<<"$payload" 2>/dev/null)
        expiry=$(jq -r 'if ((.data | has("expires_at")) and (.data.expires_at != null)) then "present" else "absent" end' <<<"$payload" 2>/dev/null)
        if [[ "$marker_type" != "absent" && "$marker_type" != "boolean" ]]; then
            _ev_warn "event type $type pid_unavailable must be boolean"
            return 1
        fi
        if [[ "$pid_kind" == "null" && ("$pid_unavailable" != "true" || "$expiry" != "present") ]]; then
            _ev_warn "event type $type null pid requires pid_unavailable=true and expires_at"
            return 1
        fi
        if [[ "$pid_kind" != "null" && "$pid_unavailable" == "true" ]]; then
            _ev_warn "event type $type pid_unavailable=true requires pid=null"
            return 1
        fi
    fi

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

    if [[ "$type" == "context_snapshot" ]]; then
        if [[ "$src" != "hook" && "$src" != "test" ]]; then
            _ev_warn "context_snapshot source must be hook or test"
            return 1
        fi
        local context_ok observed_hashes declared_hashes joined_hashes
        local expected_hash actual_hash
        context_ok=$(jq -r '
            .data as $d
            | (if ($d | has("measurement_errors"))
                then $d.measurement_errors else [] end) as $errors
            | (($d.session_id | type == "string")
                and (($d.session_id | gsub("\\s"; "") | length) > 0)) as $session_ok
            | ($d.harness == "claude" or $d.harness == "codex"
                or $d.harness == "gemini") as $harness_ok
            | ($d.entry_state == "startup" or $d.entry_state == "resume"
                or $d.entry_state == "clear"
                or $d.entry_state == "post_compact") as $entry_state_ok
            | ($d.source_manifest | type == "array"
                and all(.[]; type == "object")) as $manifest_ok
            | ($errors | type == "array"
                and all(.[]; type == "string")) as $errors_ok
            | [$d.source_manifest[] | select(.status == "observed")] as $observed
            | ($observed | all(.[];
                (.bytes | type == "number")
                and (.bytes | floor == .)
                and (.bytes >= 0 and .bytes <= 1.7976931348623157e308)
                and (.content_hash | type == "string"))) as $observed_ok
            | ($observed | map(.bytes) | add // 0) as $bytes
            | ($observed | map(.content_hash)) as $hashes
            | (($d.measurement_complete == true)
                == (($d.source_manifest | length) > 0
                    and ($observed | length) == ($d.source_manifest | length)
                    and ($errors | length) == 0)) as $complete_ok
            | $session_ok and $harness_ok and $entry_state_ok
                and $manifest_ok and $errors_ok and $observed_ok
                and ($d.context_bytes | type == "number")
                and ($d.context_bytes | floor == .)
                and ($d.estimated_tokens | type == "number")
                and ($d.estimated_tokens | floor == .)
                and $d.context_bytes == $bytes
                and $d.estimated_tokens == (($bytes + 3) / 4 | floor)
                and $d.source_hashes == $hashes
                and ($d.measurement_complete | type == "boolean")
                and $complete_ok
        ' <<<"$payload" 2>/dev/null || true)
        if [[ "$context_ok" != "true" ]]; then
            _ev_warn "context_snapshot derived fields disagree with manifest"
            return 1
        fi
        observed_hashes=$(jq -c '
            [.data.source_manifest[]
                | select(.status == "observed") | .content_hash]
        ' <<<"$payload" 2>/dev/null || true)
        declared_hashes=$(jq -c '.data.source_hashes' <<<"$payload" 2>/dev/null || true)
        if [[ "$observed_hashes" != "$declared_hashes" ]]; then
            _ev_warn "context_snapshot source_hashes disagree with source_manifest"
            return 1
        fi
        joined_hashes=$(jq -r '.data.source_hashes | join("\n")' <<<"$payload" 2>/dev/null)
        if [[ "$observed_hashes" == "[]" ]]; then
            expected_hash="null"
        elif ! expected_hash="$(_ev_sha256 "$joined_hashes")"; then
            _ev_warn "context_snapshot validation requires a SHA-256 command"
            return 2
        fi
        actual_hash=$(jq -r '
            if .data.context_hash == null then "null" else .data.context_hash end
        ' <<<"$payload" 2>/dev/null)
        if [[ "$actual_hash" != "$expected_hash" ]]; then
            _ev_warn "context_snapshot context_hash disagrees with source_hashes"
            return 1
        fi
    fi

    if [[ "$type" == "verification_receipt" ]]; then
        local receipt_ok
        receipt_ok=$(jq -r --arg src "$src" --slurpfile schema "$EVENTS_SCHEMA_CACHE" '
            def leap_year($year):
                ($year % 4 == 0)
                and (($year % 100 != 0) or ($year % 400 == 0));
            def days_in_month($year; $month):
                if $month == 2 then
                    if leap_year($year) then 29 else 28 end
                elif ([4, 6, 9, 11] | index($month)) != null then 30
                else 31
                end;
            def utc_order_key:
                if type != "string" then null
                else (
                    capture("^(?<year>[0-9]{4})-(?<month>[0-9]{2})-(?<day>[0-9]{2})T(?<hour>[0-9]{2}):(?<minute>[0-9]{2}):(?<second>[0-9]{2})(?:\\.(?<frac>[0-9]{1,6}))?(?:Z|\\+00:00)$")?
                    | if . == null then null
                      else (. as $parts
                        | ($parts.year | tonumber) as $year
                        | ($parts.month | tonumber) as $month
                        | ($parts.day | tonumber) as $day
                        | ($parts.hour | tonumber) as $hour
                        | ($parts.minute | tonumber) as $minute
                        | ($parts.second | tonumber) as $second
                        | if $year < 1
                            or $month < 1 or $month > 12
                            or $day < 1 or $day > days_in_month($year; $month)
                            or $hour > 23 or $minute > 59 or $second > 59
                          then null
                          else ($parts.year + $parts.month + $parts.day
                            + $parts.hour + $parts.minute + $parts.second
                            + ((($parts.frac // "") + "000000")[0:6]))
                          end)
                      end
                ) end;
            .data as $d
            | (.ts | utc_order_key) as $envelope_ts
            | ($schema[0].event_types[]
                | select(.name == "verification_receipt")
                | .data.properties) as $p
            | ($d.started_at | utc_order_key) as $started
            | ($d.finished_at | utc_order_key) as $finished
            | (($schema[0].event_types[] | select(.name == "verification_receipt") | .sources) as $sources
                | ($sources | index($src)) != null)
                and ($envelope_ts != null)
                and (($d.candidate_sha | type == "string")
                and ($d.candidate_sha | test("^[0-9A-Fa-f]{40}$")))
                and ($d.command | type == "array" and length > 0 and length <= 4096
                    and all(.[]; type == "string" and utf8bytelength > 0 and utf8bytelength <= 4096))
                and ($d.environment | type == "object"
                    and all(["host", "platform", "runner"][];
                        . as $f | ($d.environment[$f] | type == "string" and test("[^[:space:]]"))))
                and ($d.scope | type == "array" and length > 0 and length <= 128
                    and all(.[]; type == "string" and utf8bytelength > 0 and utf8bytelength <= 512))
                and ($started != null and $finished != null and $finished >= $started)
                and ($p.mode.enum | index($d.mode) != null)
                and ($p.result.enum | index($d.result) != null)
                and ($d.producer | type == "object"
                    and all(["kind", "id"][];
                        . as $f | ($d.producer[$f] | type == "string" and test("[^[:space:]]"))))
                and ($d.generation | type == "number" and floor == .
                    and . >= 1 and . <= 9007199254740991)
                and ($d.steps_expected | type == "number" and floor == . and . >= 0)
                and ($d.steps_executed | type == "number" and floor == . and . >= 0)
                and ($d.steps_executed <= $d.steps_expected)
                and ($d.steps_expected == ($d.scope | length))
                and (($d.mode != "full" or $d.result != "passed")
                    or ($d.steps_expected > 0 and $d.steps_executed == $d.steps_expected))
                and ($d.mode != "void" or $d.result != "passed")
        ' <<<"$payload" 2>/dev/null || true)
        if [[ "$receipt_ok" != "true" ]]; then
            _ev_warn "verification_receipt fields are malformed or contradictory"
            return 1
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

    # mail_escalation: reason enum check - mirrors the Python validator so a
    # producer typo fails loud in both. reason drives the overlay evidence text
    # and the question-vs-attended-miss split; a bad value must not land.
    if [[ "$type" == "mail_escalation" ]]; then
        local me_reason enum_match
        me_reason=$(jq -r '.data.reason // empty' <<<"$payload" 2>/dev/null || true)
        if [[ -n "$me_reason" ]]; then
            enum_match=$(jq -r --arg r "$me_reason" '.event_types[] | select(.name == "mail_escalation") | .data.properties.reason.enum[]? | select(. == $r)' "$EVENTS_SCHEMA_CACHE" 2>/dev/null || true)
            if [[ -z "$enum_match" ]]; then
                _ev_warn "unknown reason: $me_reason"
                return 1
            fi
        fi
    fi

    # post_merge_dispatch_receipt: phase + route enum checks (x-a35a) - mirrors
    # the Python validator so a producer typo fails loud in both. phase drives
    # the reserved-before-accepted lifecycle forensics rely on; route is the
    # single decision function's verdict. Both required (absence caught above);
    # has() (not `// empty`) rejects a falsey null/false value too - jq `//`
    # treats false/null as empty, which would otherwise slip it past as present.
    if [[ "$type" == "post_merge_dispatch_receipt" ]]; then
        local pm_phase pm_route enum_match
        if jq -e '.data | has("phase")' <<<"$payload" >/dev/null 2>&1; then
            pm_phase=$(jq -r '.data.phase | tostring' <<<"$payload" 2>/dev/null || true)
            enum_match=$(jq -r --arg v "$pm_phase" '.event_types[] | select(.name == "post_merge_dispatch_receipt") | .data.properties.phase.enum[]? | select(. == $v)' "$EVENTS_SCHEMA_CACHE" 2>/dev/null || true)
            if [[ -z "$enum_match" ]]; then
                _ev_warn "unknown phase: $pm_phase"
                return 1
            fi
        fi
        if jq -e '.data | has("route")' <<<"$payload" >/dev/null 2>&1; then
            pm_route=$(jq -r '.data.route | tostring' <<<"$payload" 2>/dev/null || true)
            enum_match=$(jq -r --arg v "$pm_route" '.event_types[] | select(.name == "post_merge_dispatch_receipt") | .data.properties.route.enum[]? | select(. == $v)' "$EVENTS_SCHEMA_CACHE" 2>/dev/null || true)
            if [[ -z "$enum_match" ]]; then
                _ev_warn "unknown route: $pm_route"
                return 1
            fi
        fi
    fi

    # Size cap: encode data and check bytes.
    local max_bytes data_size data_size_encoding
    max_bytes=$(jq -r '.limits.max_data_bytes // 65536' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
    data_size_encoding=$(jq -r '.limits.data_size_encoding // empty' "$EVENTS_SCHEMA_CACHE" 2>/dev/null)
    if [[ "$data_size_encoding" != "compact-json-ascii-v1" ]]; then
        _ev_warn "unsupported limits.data_size_encoding: ${data_size_encoding:-missing}"
        return 1
    fi
    # `jq -ac .data` gives compact ASCII JSON; -n removes the trailing newline
    # so wc -c counts only payload bytes.
    data_size=$(jq -acn --argjson p "$payload" '$p.data' | tr -d '\n' | wc -c | tr -d ' ')
    if (( data_size > max_bytes )); then
        _ev_warn "event data exceeds max_data_bytes (got $data_size, limit $max_bytes)"
        return 1
    fi

    return 0
}

# Best-effort cleanup so cache files don't accumulate in /tmp.
trap 'rm -f "$EVENTS_SCHEMA_CACHE" 2>/dev/null || true' EXIT
