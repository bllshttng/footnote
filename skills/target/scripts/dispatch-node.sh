#!/usr/bin/env bash
# dispatch-node.sh - Dispatch ready backlog node(s) as fresh `claude --bg` /target
# workers, fire-and-forget, with a per-node outcome line and a node:<id> claim
# guard against double-dispatch. The fresh bg process IS the planning session's
# "clear": a new process is the only real context reset, so the planning session
# persists while dispatched workers run do->review->ship on their own.
#
# Self-contained skill script. External deps: `fno` (backlog, claim, agents) + jq.
# See internal/fno/plans/2026-06-02-target-plan-mode-phase2.md (US5/US6).
#
# Usage:
#   dispatch-node.sh <node-id...> [--flags "<extra /target flags>"]
#                                 [--allow-merge] [--max N] [--dry-run] [--here]
#                                 [--permission-mode <mode>] [--route provider/model]
#   dispatch-node.sh --all-ready  [--flags "..."] [--allow-merge] [--max N] [--dry-run] [--here]
#                                 [--permission-mode <mode>] [--route provider/model]
#
# --route provider/model: per-dispatch explicit model route (x-b0b4), forwarded
#   to every worker spawn. Fails CLOSED in the spawn (unknown/non-anthropic/
#   keyless refuses -> the node stays dispatchable). Wins over the build lane.
#   Every worker also carries --role build unconditionally: the build lane is a
#   fail-safe no-op until `fno route set build ...` opts in, so this is
#   byte-identical to today until configured.
#
# --here / --in-place: keep a worker without a recorded node cwd in the
#   dispatcher's cwd. Default (no node cwd) is --fresh: start from canonical
#   main so a dispatch from a linked worktree does not inherit that worktree.
#
# Per-node outcome lines (stdout; one per node; NEVER silent):
#   launched         <node> name=<agent> session=<sid> cwd=<path> hint="fno agents logs <agent>" route=<provider/model|primary>
#   already-running  <node> reason="live target worker holds node:<id> (<holder>)"
#   skipped-contested <node> reason="suspect claim (respawned worker); advancing" (x-ba4b)
#   parked           <node> reason="blocked|deferred|<status> (not up-next)"
#   skipped-done     <node> reason="already done|superseded"
#   failed           <node> reason="<why>"
#   deferred-cap     <node> reason="--max <N> reached"
# Summary (last line):
#   summary: launched=<n> parked=<n> already=<n> skipped=<n> done=<n> failed=<n> capped=<n>[ nothing-up-next]
#
# Invariants (Failure Modes section of the plan):
#   - Provider + substrate come from `fno dispatch resolve` (the x-4d85
#     harness-capability map), NOT a hardcode (x-567d). claude resolves to `bg`
#     (the DETACHED `claude --bg` thread, x-2c27: auto-worktrees, runs unattended,
#     shows in `claude agents`); every other harness resolves to `headless` (a
#     one-shot that runs to completion) with a loud fallback event. The default
#     is never `pane` (x-3ab8's owned-PTY default would stall a fire-and-forget
#     dispatch at a placement prompt). NEVER `--bare`/`-p` for the bg lane (those
#     force the API-credit pool and strip skills/hooks); `bg` is the subscription
#     `claude --bg` lane. An unresolvable harness hard-fails loudly (AC2-ERR),
#     never a silent claude default.
#   - A failed dispatch is surfaced and leaves the node `ready`/re-dispatchable;
#     never reports a launch that did not happen; never silently swallows.
#   - Fire-and-forget: this script NEVER writes/clears the caller's
#     .fno/target-state.md. The planning session is untouched.
#   - Under --all-ready only `ready` nodes dispatch. An EXPLICITLY-NAMED node
#     also dispatches when idea-status (the triage pile; there is no distinct
#     `triage` status) - naming it is the human's vet, the worker runs
#     think->blueprint->do; blocked/deferred are always parked.

set -uo pipefail

# ---- deps -------------------------------------------------------------------
command -v fno >/dev/null 2>&1 || { echo "failed: - reason=\"fno not on PATH\"" >&2; echo "summary: launched=0 parked=0 already=0 skipped=0 done=0 failed=1 capped=0"; exit 1; }
command -v jq  >/dev/null 2>&1 || { echo "failed: - reason=\"jq not on PATH\""  >&2; echo "summary: launched=0 parked=0 already=0 skipped=0 done=0 failed=1 capped=0"; exit 1; }

# Canonical MAIN checkout for deterministic --fresh isolation (x-73ca). The
# git-common-dir's parent is the main checkout even when the dispatcher runs
# from a linked worktree; `fno worktree ensure --repo <this>` then creates the
# worker's conductor worktree off origin/main. Empty when not in a git repo
# (the --fresh arm falls back to the Rust runtime's own --fresh resolution).
CANONICAL_ROOT=""
_gcd_raw="$(git rev-parse --git-common-dir 2>/dev/null)"
if [[ -n "$_gcd_raw" ]]; then            # guard so we never `cd ""` (a no-op that
  _gcd="$(cd "$_gcd_raw" 2>/dev/null && pwd -P)"   # would falsely set a non-git cwd)
  [[ -n "$_gcd" ]] && CANONICAL_ROOT="$(dirname "$_gcd")"
fi

# ---- arg parse --------------------------------------------------------------
NODES=()
ALL_READY=0
FLAGS=""
ALLOW_MERGE=0
MAX=0          # 0 => no cap (quota is the throttle; do not invent a hard cap)
DRY_RUN=0
HERE=0         # 1 => keep the worker in the dispatcher's cwd (opt out of --fresh)
PERMISSION_MODE=""  # x-dfa4: forwarded as --permission-mode to each worker spawn
ROUTE=""       # x-b0b4: per-dispatch explicit provider,model route (fail-closed)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --all-ready)  ALL_READY=1; shift ;;
    --flags)      FLAGS="${2:-}"; shift 2 ;;
    --allow-merge) ALLOW_MERGE=1; shift ;;
    --max)        MAX="${2:-0}"; shift 2 ;;
    --dry-run)    DRY_RUN=1; shift ;;
    --here|--in-place) HERE=1; shift ;;
    --permission-mode) PERMISSION_MODE="${2:-}"; shift 2 ;;
    --route)      [[ $# -ge 2 ]] || { echo "failed: --route reason=\"requires a provider/model value\"" >&2; echo "summary: launched=0 parked=0 already=0 skipped=0 done=0 failed=1 capped=0"; exit 2; }; ROUTE="$2"; shift 2 ;;
    --) shift; while [[ $# -gt 0 ]]; do NODES+=("$1"); shift; done ;;
    -*) echo "failed: $1 reason=\"unknown flag\"" >&2; exit 2 ;;
    *)  NODES+=("$1"); shift ;;
  esac
done

# x-dfa4: config default for autonomous dispatch (config.agents.spawn_permission_mode).
# An explicit --permission-mode flag wins; empty + config-unset = unchanged. A
# stale `fno` that rejects the unmodeled key degrades to empty (fail-safe).
if [[ -z "$PERMISSION_MODE" ]]; then
  PERMISSION_MODE="$(fno config get agents.spawn_permission_mode 2>/dev/null | tr -d '[:space:]' || true)"
fi

# ---- resolve the node set ---------------------------------------------------
if [[ "$ALL_READY" -eq 1 ]]; then
  # Project-scoped ready, non-deferred nodes (megawalk selection semantics:
  # `ready` excludes deferred + blocked by default). Surface the cost so the
  # operator knows ~Mx subscription quota burns while these run concurrently.
  # Capture the enumeration exit code so a transient `fno backlog ready` failure
  # is surfaced, NOT silently reported as "nothing-up-next" (an empty backlog).
  ready_json="$(fno backlog ready 2>/dev/null)"; ready_rc=$?
  if [[ "$ready_rc" -ne 0 ]]; then
    echo "failed --all-ready reason=\"fno backlog ready exited $ready_rc; not treating as an empty backlog\""
    echo "summary: launched=0 parked=0 already=0 skipped=0 done=0 failed=1 capped=0"
    exit 1
  fi
  # bash 3.2 (macOS) has no `mapfile`. Capture the ids into a var, then iterate
  # via a here-string - a process-substitution loop source (`done < <(...)`)
  # masks jq's exit status inside a subshell (external review HIGH). An empty
  # here-string yields a single empty read that the guard below skips.
  ready_ids="$(printf '%s' "$ready_json" | jq -r '.[].id' 2>/dev/null)"
  while IFS= read -r _id; do
    [[ -n "$_id" ]] && NODES+=("$_id")
  done <<< "$ready_ids"
fi

if [[ "${#NODES[@]}" -eq 0 ]]; then
  echo "summary: launched=0 parked=0 already=0 skipped=0 done=0 failed=0 capped=0 nothing-up-next"
  exit 0
fi

if [[ "$ALL_READY" -eq 1 ]]; then
  echo "dispatching up to ${#NODES[@]} worker(s) (~${#NODES[@]}x subscription quota while active; quota is the throttle)" >&2
fi

# ---- resolve provider + substrate from the harness-capability map (x-567d) ---
# Provider + substrate are NO LONGER hardcoded claude/bg. `fno dispatch resolve`
# reads config.dispatch.harness and returns the autonomous substrate: claude->bg
# (today's path, byte-identical) or codex/gemini/agy/opencode->headless. Three
# outcomes, all LOUD, never a silent claude default:
#   - substrate=bg      : the detached `claude --bg` thread (unchanged).
#   - substrate=headless: a one-shot that runs to completion; a fallback event is
#     emitted so an operator sees the downgrade (epic AC1-EDGE).
#   - resolve fails      : an unknown/misconfigured harness has NO autonomous
#     substrate -> hard fail naming config.dispatch.harness; every node stays
#     ready, nothing launches, a failure event is recorded (epic AC2-ERR).
# `fno dispatch resolve` / `fno event emit` are top-level Python verbs (not in the
# `agents` group), so they are immune to FNO_AGENTS_RUNTIME=rust - no pin needed.
resolve_json="$(fno dispatch resolve --json 2>/dev/null)"; resolve_rc=$?
DISPATCH_PROVIDER="$(printf '%s' "$resolve_json" | jq -r '.harness // empty' 2>/dev/null)"
DISPATCH_SUBSTRATE="$(printf '%s' "$resolve_json" | jq -r '.substrate // empty' 2>/dev/null)"
if [[ "$resolve_rc" -ne 0 || -z "$DISPATCH_PROVIDER" || -z "$DISPATCH_SUBSTRATE" ]]; then
  reason="no autonomous substrate resolved (rc=$resolve_rc); set config.dispatch.harness to a harness with one (claude=bg, codex/gemini/agy/opencode=headless)"
  fno event emit -t dispatch_no_autonomous_substrate -s backlog \
    -d "{\"reason\":\"dispatch resolve rc=$resolve_rc\",\"config_key\":\"config.dispatch.harness\"}" >/dev/null 2>&1 || true
  for id in "${NODES[@]}"; do echo "failed $id reason=\"$reason\""; done
  echo "summary: launched=0 parked=0 already=0 skipped=0 done=0 failed=${#NODES[@]} capped=0"
  exit 1
fi
# Loud, once: a non-bg harness dispatches via headless (a one-shot, not detached).
if [[ "$DISPATCH_SUBSTRATE" != "bg" ]]; then
  echo "note: harness '$DISPATCH_PROVIDER' has no bg substrate; dispatching via headless (one-shot runs to completion, not a detached thread)" >&2
  fno event emit -t dispatch_substrate_fallback -s backlog \
    -d "{\"harness\":\"$DISPATCH_PROVIDER\",\"from\":\"bg\",\"to\":\"$DISPATCH_SUBSTRATE\"}" >/dev/null 2>&1 || true
fi

# ---- per-node dispatch ------------------------------------------------------
n_launched=0; n_parked=0; n_already=0; n_skipped=0; n_done=0; n_failed=0; n_capped=0

for id in "${NODES[@]}"; do
  # --max soft cap: once reached, report the remainder rather than dropping silently.
  if [[ "$MAX" -gt 0 && "$n_launched" -ge "$MAX" ]]; then
    echo "deferred-cap $id reason=\"--max $MAX reached\""
    n_capped=$((n_capped + 1))
    continue
  fi

  # Resolve the node. A non-existent / malformed id is a hard failure, never a
  # phantom worker.
  node_json="$(fno backlog get "$id" 2>/dev/null)"
  if [[ -z "$node_json" ]] || ! printf '%s' "$node_json" | jq -e '.id' >/dev/null 2>&1; then
    echo "failed $id reason=\"no such node (or backlog read failed)\""
    n_failed=$((n_failed + 1))
    continue
  fi

  status="$(printf '%s' "$node_json" | jq -r '._status // "unknown"')"

  case "$status" in
    done)
      echo "skipped-done $id reason=\"node already done\""
      n_done=$((n_done + 1))
      continue ;;
    superseded)
      echo "skipped-done $id reason=\"node superseded\""
      n_done=$((n_done + 1))
      continue ;;
    ready|claimed)
      # ready => dispatchable. claimed => a worker may already hold it; the
      # live-claim check below reports already-running, or (stale claim => dead
      # worker) falls through to re-dispatch as recovery.
      : ;;
    idea)
      # Explicitly naming an idea node (the triage pile is idea-status; there is
      # no distinct `triage` status) IS the human's vet: dispatch it and let the
      # /target worker run think->blueprint->do. --all-ready stays ready-only
      # (its enumeration never yields idea; this guard makes that fail-safe).
      if [[ "$ALL_READY" -eq 1 ]]; then
        echo "parked $id reason=\"$status (not up-next)\""
        n_parked=$((n_parked + 1))
        continue
      fi
      : ;;
    *)
      # blocked / deferred / unknown => pre-planned future work.
      echo "parked $id reason=\"$status (not up-next)\""
      n_parked=$((n_parked + 1))
      continue ;;
  esac

  # Open-PR guard (mirrors _has_unmerged_open_pr, cli.py:68): a node that already
  # carries a pr_number but is not yet done is in flight / in review - the PR
  # outlives the builder's PID node:<id> claim once that worker exits.
  # The selection guard inside `fno backlog next`/`ready` already drops these,
  # but the explicit-id path reads `fno backlog get` directly and skips it, so
  # mirror it here: park instead of launching a duplicate. completed_at => done
  # was already handled by the case above; this catches the PR window before close.
  # One jq pass for both fields (tab-separated); read splits them. An empty/
  # failed parse leaves both empty -> falls through to dispatch (prior behavior).
  IFS=$'\t' read -r pr_number completed_at <<< "$(printf '%s' "$node_json" \
    | jq -r '[.pr_number // "", .completed_at // ""] | @tsv' 2>/dev/null)"
  if [[ -n "$pr_number" && -z "$completed_at" ]]; then
    echo "already-running $id reason=\"node carries open PR #$pr_number; not re-dispatching\""
    n_already=$((n_already + 1))
    continue
  fi

  # Provenance-carrying name: target-<full-node-id>-<slug> so the bg thread title
  # reads at a glance which node a /target worker is on (e.g.
  # target-ab-4040eee8-cargo-bootstrapper). The slug is the node's title-derived
  # handle; a node with no slug degrades to target-<full-node-id>.
  node_slug="$(printf '%s' "$node_json" | jq -r '.slug // .title // empty' 2>/dev/null \
    | tr '[:upper:]' '[:lower:]' | tr -c 'a-z0-9-' '-' \
    | sed -E 's/-+/-/g; s/^-+//; s/-+$//' | cut -c1-30 | sed -E 's/-+$//')"
  if [[ -n "$node_slug" ]]; then
    agent_name="target-${id}-${node_slug}"
  else
    agent_name="target-${id}"
  fi

  # x-571f: per-node model pin. Read once from the node JSON we already hold; a
  # non-empty value is applied as `--model <m>` to every spawn branch below (and
  # the dry-run hint). A bash ARRAY (not a string) so the value is quoted at
  # expansion - no globbing/word-splitting even if a pin ever carried a glob char
  # (gemini review PR #150; `fno backlog update --model` already forbids those,
  # this is defense-in-depth). Expanded as `"${model_args[@]+"${model_args[@]}"}"`
  # - the `+` guard keeps an EMPTY array from tripping bash 3.2's set -u unbound
  # error (the same trap the --cwd branches below avoid). Empty pin -> zero args =
  # byte-identical to today; fail-open on a bad read since jq // empty yields "".
  model_pin="$(printf '%s' "$node_json" | jq -r '.model // empty' 2>/dev/null || true)"
  # x-d7a7: no exact `.model` pin? resolve the node's `model_tier` via the single
  # Python projection (`fno target resolve-model` -> route_resolve) so a tiered
  # node's worker spawns on the tier model too - bash never resolves. `--provider
  # claude` scopes the pick to THIS lane (bg is claude-only): a tier that resolves
  # to a codex/gemini model is dropped to the provider default rather than passed
  # as an invalid `claude --model <foreign>`. Empty output (no pin/tier, cross-
  # harness pick, or any resolve error) -> zero args = byte-identical to today.
  if [[ -z "$model_pin" ]]; then
    model_pin="$(fno target resolve-model "$id" --provider claude 2>/dev/null | head -1 | tr -d '[:space:]' || true)"
  fi
  model_args=()
  [[ -n "$model_pin" ]] && model_args=("--model" "$model_pin")

  # x-dfa4: forward the permission mode to every worker spawn (same array
  # discipline as model_args - avoids word-splitting at the trust boundary).
  # Empty => zero args => byte-identical to today.
  perm_args=()
  [[ -n "$PERMISSION_MODE" ]] && perm_args=("--permission-mode" "$PERMISSION_MODE")

  # x-b0b4: every worker rides the build lane unconditionally (same array
  # discipline). resolve_route returns None for an unconfigured `build`, so this
  # is byte-identical to today until `fno route set build ...` opts in - no
  # config read in bash, no conditional. An explicit --route (fail-closed in the
  # spawn) is forwarded per-dispatch and wins over the lane.
  role_args=("--role" "build")
  route_args=()
  [[ -n "$ROUTE" ]] && route_args=("--route" "$ROUTE")
  # Receipt route= token, resolved PER NODE (not once before the loop) so a
  # `route set`/`unset` racing a bulk dispatch is stamped per worker, never
  # inferred from a stale run-start snapshot (codex P2; plan's per-worker
  # provenance invariant). Explicit --route wins. Otherwise the AUTHORITATIVE
  # build-lane predicate is `fno route env build`: it runs the same
  # resolve_route('build') the worker's bg_create uses, so it catches every
  # fall-safe-to-primary reason (model_routing.enabled=false, a keyed but
  # non-anthropic provider) that a target+key table heuristic would miss. env
  # exits 0 only when a real route resolves; `route ls -J` then supplies the
  # provider,model string. A stale `fno` without the verb (or any failure) leaves
  # `primary` - the honest conservative claim (routing not confirmed).
  if [[ -n "$ROUTE" ]]; then
    route_val="$ROUTE"
  else
    route_val="primary"
    if fno route env build >/dev/null 2>&1; then
      _bpm="$(fno route ls -J 2>/dev/null | jq -r '.[] | select(.role=="build") | .provider_model' 2>/dev/null || true)"
      [[ -n "$_bpm" && "$_bpm" != "unconfigured" ]] && route_val="$_bpm"
    fi
  fi

  # ---- Guards 1+2 via the shared spawn-guard verb (x-73cc) ----
  # The race-critical node:<id> claim probe (Guard 1) + create-only dispatch:<id>
  # reservation (Guard 2) live in `fno agents spawn-guard` so this path and
  # /agent spawn (spawn.sh) can never drift on the part that matters. A dry-run,
  # or a node whose _status is `claimed` (the recovery-park policy below), uses
  # --no-reserve so NO reservation is taken; a real ready dispatch reserves.
  # Fail CLOSED: a stale `fno` without the verb (or any non-clean/unparseable
  # verdict) leaves the node `ready` and launches nothing.
  # spawn-guard is a Python-only verb (no Rust client impl). Pin the call to the
  # Python runtime so an operator with FNO_AGENTS_RUNTIME=rust exported does not
  # route it to the Rust binary (which lacks it -> 127 -> the guard fails closed
  # and bg-dispatch breaks). The pre-refactor `fno claim` calls were never in the
  # `agents` group so were immune; this restores that immunity (codex P2). The
  # inline override is scoped to this command; the real `fno agents spawn` below
  # routes normally. The default (unset) runtime already keeps spawn-guard Python.
  res_key="dispatch:$id"; res_holder="dispatch-node:$$"
  if [[ "$DRY_RUN" -eq 1 || "$status" == "claimed" ]]; then
    guard_out="$(FNO_AGENTS_RUNTIME=python fno agents spawn-guard "$id" --holder "$res_holder" --no-reserve --json 2>/dev/null)"; guard_rc=$?
  else
    guard_out="$(FNO_AGENTS_RUNTIME=python fno agents spawn-guard "$id" --holder "$res_holder" --ttl 3m --json 2>/dev/null)"; guard_rc=$?
  fi
  # grep the JSON object line first (defense in depth vs any stderr/banner that
  # could leak onto stdout), then parse the verdict.
  guard_json="$(printf '%s\n' "$guard_out" | grep -F '"verdict"' | head -1)"
  verdict="$(printf '%s' "$guard_json" | jq -r '.verdict // empty' 2>/dev/null)"
  case "$verdict" in
    already-running)
      reason="$(printf '%s' "$guard_json" | jq -r '.reason // empty' 2>/dev/null)"
      if [[ "$reason" == "live-claim" ]]; then
        holder="$(printf '%s' "$guard_json" | jq -r '.holder // "unknown"' 2>/dev/null)"
        echo "already-running $id reason=\"live target worker holds node:$id ($holder)\""
        n_already=$((n_already + 1))
      elif [[ "$reason" == "suspect-claim" ]]; then
        # x-ba4b: TTL-unexpired dead-pid claim (a respawned worker). Contested
        # liveness degrades to SKIP, never steal and never park the lane -
        # advance to the next unblocked ready node.
        holder="$(printf '%s' "$guard_json" | jq -r '.holder // "unknown"' 2>/dev/null || true)"
        echo "skipped-contested $id reason=\"suspect claim on node:$id ($holder); respawned worker, advancing\""
        n_skipped=$((n_skipped + 1))
      else
        echo "already-running $id reason=\"a peer dispatcher holds $res_key (racing launch)\""
        n_already=$((n_already + 1))
      fi
      continue ;;
    corrupted)
      # The worker's init-side `fno claim acquire` cannot reclaim a corrupted
      # claim, so launching would run WITHOUT the node:<id> mutex and leave the
      # corrupt lock in place (external review P2). Fail closed; an operator
      # force-releases/repairs it before re-dispatch.
      echo "failed $id reason=\"node:$id claim is corrupted; force-release or repair before dispatching\""
      n_failed=$((n_failed + 1))
      continue ;;
    dispatchable)
      if [[ "$status" == "claimed" ]]; then
        # _status: claimed but node:<id> claim not live. Do NOT auto-recover via
        # dispatch (external review P2): the worker init may see a stale legacy
        # graph session_id, refuse to record graph_node_id, run anyway, then be
        # unable to clear the legacy graph claim on exit - leaving the node stuck
        # claimed/hidden. Park for manual recovery (--no-reserve was used above,
        # so there is no reservation to release).
        echo "parked $id reason=\"claimed but node:$id claim not live; needs manual recovery (legacy graph claim may be stuck)\""
        n_parked=$((n_parked + 1))
        continue
      fi
      # status == ready AND claim free/stale: dispatchable (stale => recovery,
      # the worker's atomic init-acquire reclaims a dead holder). In the
      # reserving branch dispatch:$id is now held by $res_holder; it is released
      # on any spawn-failure path below and left to TTL-expire on a launch.
      : ;;
    *)
      # verdict=error, OR empty/unparseable (a stale fno WITHOUT the verb prints
      # Typer "No such command" + exits non-zero; or a probe crash): fail CLOSED.
      detail="$(printf '%s' "$guard_json" | jq -r '.detail // empty' 2>/dev/null)"
      echo "failed $id reason=\"${detail:-spawn-guard unavailable (rc=$guard_rc); not dispatching to avoid a double-launch}\""
      n_failed=$((n_failed + 1))
      continue ;;
  esac

  # ---- Build the worker command + resolve the launch cwd ----
  # no-merge is the default for a fire-and-forget worker (Locked Decision 4);
  # --allow-merge opts out.
  tgt_cmd="/target"
  [[ -n "$FLAGS" ]] && tgt_cmd="$tgt_cmd $FLAGS"
  if [[ "$ALLOW_MERGE" -eq 0 && " $FLAGS " != *" no-merge "* ]]; then
    tgt_cmd="$tgt_cmd no-merge"
  fi
  tgt_cmd="$tgt_cmd $id"

  # Launch in the node's _resolved_cwd (work-map root when project mapped;
  # falls back to recorded .cwd against an older installed fno without the
  # field; empty -> caller's cwd). The _resolved_cwd field is derived at
  # read time by `fno backlog get` and never persisted to graph.json.
  node_cwd="$(printf '%s' "$node_json" | jq -r '._resolved_cwd // .cwd // empty' 2>/dev/null)"
  # cwd precedence: an explicit node cwd (work-map root) wins. With no node cwd,
  # default to --fresh so a worker dispatched from a linked worktree starts from
  # canonical main instead of inheriting the dispatcher's worktree (the shared
  # .fno/ collision this guards against). --here/--in-place opts back into
  # caller-cwd inheritance. --fresh is a no-op when the dispatcher is already at
  # canonical (AC5), so it is always safe to pass here. dispatch-node is
  # single-repo target-class by construction, so the cross-project flow (AC4)
  # never reaches this path.
  cwd_hint=""
  dry_cwd="$(pwd)"
  if [[ -n "$node_cwd" ]]; then
    cwd_hint="--cwd $node_cwd "
    dry_cwd="$node_cwd"
  elif [[ "$HERE" -eq 0 ]]; then
    # cwd= must stay a real, space-free path so the receipt is machine-parseable
    # (the conductor worktree path is not known until ensure runs, so preview the
    # canonical root the --fresh fallback would use); the hint carries the intent.
    cwd_hint="--cwd <fno worktree ensure> "
    dry_cwd="${CANONICAL_ROOT:-$(pwd)}"
  fi

  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "launched $id name=$agent_name session=DRY-RUN cwd=${dry_cwd} hint=\"would run: fno agents spawn --provider $DISPATCH_PROVIDER --substrate $DISPATCH_SUBSTRATE ${cwd_hint}--role build ${ROUTE:+--route $ROUTE }${model_pin:+--model $model_pin }${PERMISSION_MODE:+--permission-mode $PERMISSION_MODE }$agent_name '$tgt_cmd'\" route=${route_val}"
    n_launched=$((n_launched + 1))
    continue
  fi

  # ---- Guard 3: the agents registry (safe now, under the reservation) ----
  # A LIVE same-name agent means a worker is already up (its node:<id> claim not
  # yet caught above); release our reservation and report already-running. A
  # dead row is removed so `ask` creates fresh rather than resuming it.
  # Capture the probe exit code AND require a parseable {agents:[...]}: a
  # crashed/garbled `fno agents list` (daemon down, stale install printing a
  # Typer error) must fail CLOSED (release + refuse), never collapse to an empty
  # existing_status and fall through to a double-launch in the boot window
  # (parity with spawn.sh Guard 3, cv-dddd8ae5; sigma silent-failure-hunter).
  agents_json="$(fno agents list 2>/dev/null)"; list_rc=$?
  if [[ "$list_rc" -ne 0 ]] || ! printf '%s' "$agents_json" | jq -e 'has("agents")' >/dev/null 2>&1; then
    fno claim release "$res_key" --holder "$res_holder" >/dev/null 2>&1 || true
    echo "failed $id reason=\"agents-list probe failed (rc=$list_rc); not dispatching to avoid a double-launch\""
    n_failed=$((n_failed + 1))
    continue
  fi
  existing_status="$(printf '%s' "$agents_json" \
    | jq -r --arg n "$agent_name" '.agents[]? | select(.name==$n) | .status' 2>/dev/null | head -1)"
  if [[ "$existing_status" == "live" ]]; then
    fno claim release "$res_key" --holder "$res_holder" >/dev/null 2>&1 || true
    echo "already-running $id reason=\"a live agent $agent_name already exists (worker booting/running)\""
    n_already=$((n_already + 1))
    continue
  elif [[ -n "$existing_status" ]]; then
    fno agents rm "$agent_name" >/dev/null 2>&1 || true
  fi

  # ---- Dispatch, fire-and-forget ----
  # `fno agents spawn --provider "$DISPATCH_PROVIDER" --substrate "$DISPATCH_SUBSTRATE"`.
  # For claude/bg this lands a DETACHED `claude --bg` thread (x-2c27): it
  # auto-worktrees, runs the node to completion unattended, and shows in `claude
  # agents` (attach/peek/reply) - NOT an owned-PTY pane that would stall at a
  # placement prompt (the x-3ab8 default `pane` is the regression `bg` fixes). For
  # a headless fallback it is a one-shot that runs to completion here. Still the
  # subscription lane (NEVER --bare/-p) and still fire-and-forget. The bg receipt
  # parsed below is the claude-spawn JSON; a headless one-shot has no short_id
  # (see the substrate branch there). name is a positional. Three branches keep
  # the optional --cwd off an empty-array path (bash 3.2 set -u safe). stderr goes
  # to a temp file, NOT 2>&1: a stderr warning must never pollute the JSON receipt
  # parse below (house rule; gemini review PR #457).
  spawn_err_file="$(mktemp 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}/dispatch-node-$$.err")"
  # Three explicit branches (NOT an optional-flag array): bash 3.2 (macOS)
  # errors on `"${arr[@]}"` for an empty array under `set -u`. node cwd ->
  # --cwd; no node cwd + default -> ensure a conductor worktree and pass --cwd
  # it (deterministic isolation, x-73ca), falling back to --fresh on any ensure
  # failure (empty $wt) so the dispatch is never blocked; --here -> inherit.
  launch_cwd="${node_cwd:-$(pwd)}"
  if [[ -n "$node_cwd" ]]; then
    spawn_out="$(fno agents spawn --provider "$DISPATCH_PROVIDER" --substrate "$DISPATCH_SUBSTRATE" --cwd "$node_cwd" "${role_args[@]}" "${route_args[@]+"${route_args[@]}"}" "${model_args[@]+"${model_args[@]}"}" "${perm_args[@]+"${perm_args[@]}"}" "$agent_name" "$tgt_cmd" 2>"$spawn_err_file")"; spawn_rc=$?
  elif [[ "$HERE" -eq 0 ]]; then
    wt=""
    [[ -n "$CANONICAL_ROOT" ]] && wt="$(fno worktree ensure --repo "$CANONICAL_ROOT" --name "$agent_name" 2>/dev/null)"
    if [[ -n "$wt" ]]; then
      # Link gitignored shared state into the new worktree (footnote-ecosystem
      # only; absent -> skip). Caller-side because the verb is package code and
      # may not shell out to a repo-root script (shellout-drift gate).
      _wt_setup="$CANONICAL_ROOT/scripts/setup/setup-worktree.sh"
      [[ -f "$_wt_setup" ]] && CANONICAL="$CANONICAL_ROOT" WORKTREE="$wt" bash "$_wt_setup" >/dev/null 2>&1
      spawn_out="$(fno agents spawn --provider "$DISPATCH_PROVIDER" --substrate "$DISPATCH_SUBSTRATE" --cwd "$wt" "${role_args[@]}" "${route_args[@]+"${route_args[@]}"}" "${model_args[@]+"${model_args[@]}"}" "${perm_args[@]+"${perm_args[@]}"}" "$agent_name" "$tgt_cmd" 2>"$spawn_err_file")"; spawn_rc=$?
      launch_cwd="$wt"
    else
      spawn_out="$(fno agents spawn --provider "$DISPATCH_PROVIDER" --substrate "$DISPATCH_SUBSTRATE" --fresh "${role_args[@]}" "${route_args[@]+"${route_args[@]}"}" "${model_args[@]+"${model_args[@]}"}" "${perm_args[@]+"${perm_args[@]}"}" "$agent_name" "$tgt_cmd" 2>"$spawn_err_file")"; spawn_rc=$?
      # --fresh lands the worker in canonical main; report that real path (not a
      # space-containing label) so the cwd= field stays machine-parseable.
      launch_cwd="${CANONICAL_ROOT:-$(pwd)}"
    fi
  else
    spawn_out="$(fno agents spawn --provider "$DISPATCH_PROVIDER" --substrate "$DISPATCH_SUBSTRATE" "${role_args[@]}" "${route_args[@]+"${route_args[@]}"}" "${model_args[@]+"${model_args[@]}"}" "${perm_args[@]+"${perm_args[@]}"}" "$agent_name" "$tgt_cmd" 2>"$spawn_err_file")"; spawn_rc=$?
  fi
  spawn_err="$(cat "$spawn_err_file" 2>/dev/null)"; rm -f "$spawn_err_file"
  if [[ "$spawn_rc" -ne 0 ]]; then
    # Surface the failure; release the reservation so the node is re-dispatchable.
    # A name collision (exit 2, "already exists") means a worker beat us in the
    # registry-check window: report already-running, not failed.
    fno claim release "$res_key" --holder "$res_holder" >/dev/null 2>&1 || true
    if [[ "$spawn_rc" -eq 2 ]] && printf '%s' "$spawn_err" | grep -qF "already exists"; then
      echo "already-running $id reason=\"an agent named $agent_name already exists (spawn collision)\""
      n_already=$((n_already + 1))
      continue
    fi
    reason="$(printf '%s' "${spawn_err:-$spawn_out}" | tr '\n' ' ' | sed 's/"/'"'"'/g' | cut -c1-200)"
    echo "failed $id reason=\"dispatch failed (rc=$spawn_rc): $reason\""
    n_failed=$((n_failed + 1))
    continue
  fi

  # Receipt shape is substrate-dependent (x-567d). bg lands a DETACHED thread and
  # returns a compact JSON receipt with a short_id we require as launch proof:
  #   {"name": "...", "short_id": "<8hex>", "provider": "claude", "status": "live"}
  # headless is a ONE-SHOT that already ran to completion on exit 0 (no detached
  # thread, no short_id) - the clean exit IS the proof, so we skip the short_id
  # requirement and label the session `headless`.
  if [[ "$DISPATCH_SUBSTRATE" == "bg" ]]; then
    # grep the receipt line first as defense in depth. No parseable short id on
    # exit 0 => no launch we can prove; report honestly + release the reservation.
    sid="$(printf '%s\n' "$spawn_out" | grep -F '"short_id"' | head -1 | jq -r '.short_id // empty' 2>/dev/null)"
    if [[ -z "$sid" ]]; then
      fno claim release "$res_key" --holder "$res_holder" >/dev/null 2>&1 || true
      reason="$(printf '%s' "${spawn_out:-$spawn_err}" | tr '\n' ' ' | sed 's/"/'"'"'/g' | cut -c1-200)"
      echo "failed $id reason=\"spawn exit 0 but no short_id receipt: $reason\""
      n_failed=$((n_failed + 1))
      continue
    fi
  else
    sid="headless"
  fi
  # Launched. Leave the reservation to expire by TTL (the worker now owns
  # node:<id>, which guards later dispatches).
  echo "launched $id name=$agent_name session=$sid cwd=${launch_cwd} hint=\"fno agents logs $agent_name\" route=${route_val}"
  n_launched=$((n_launched + 1))
done

echo "summary: launched=$n_launched parked=$n_parked already=$n_already skipped=$n_skipped done=$n_done failed=$n_failed capped=$n_capped"
# Exit non-zero only when nothing launched AND at least one hard failure, so a
# caller can detect a total dispatch failure while a mixed batch still exits 0.
if [[ "$n_launched" -eq 0 && "$n_failed" -gt 0 ]]; then
  exit 1
fi
exit 0
