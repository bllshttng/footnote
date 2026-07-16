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
#                                 [--allow-merge|--no-merge] [--max N] [--dry-run] [--here]
#                                 [--permission-mode <mode>] [--route provider/model]
#   dispatch-node.sh --all-ready  [--flags "..."] [--allow-merge|--no-merge] [--max N] [--dry-run] [--here]
#                                 [--permission-mode <mode>] [--route provider/model]
#
# --allow-merge / --no-merge: per-run merge posture override (x-4391). Neither
#   flag => posture from config.dispatch.auto_merge (default false = no-merge).
#   An explicit flag wins the config default.
#
# --route provider/model: per-dispatch explicit model route (x-b0b4), forwarded
#   to every worker spawn. Fails CLOSED in the spawn (unknown/non-anthropic/
#   keyless refuses -> the node stays dispatchable). Wins over the build lane.
#   A CLAUDE worker carries --role build (the build lane is a fail-safe no-op
#   until `fno route set build ...` opts in). Non-claude workers do NOT: the
#   build/route lane is claude-specific, and a role-bearing spawn is classified
#   Python-owned by the runtime, which rejects opencode/agy (x-567d / codex P1).
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
# x-4391 tri-state: "" = unset (resolve from config.dispatch.auto_merge after arg
# parse); 1 = allow merge (--allow-merge); 0 = no-merge (--no-merge). Once
# resolved it is always 0/1, so the downstream `-eq 0`/`-eq 1` checks are total.
ALLOW_MERGE=""
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
    --no-merge)   ALLOW_MERGE=0; shift ;;
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

# x-4391: merge posture is resolved PER NODE (in the loop) so a batch spanning
# projects reads each node's own config.dispatch.auto_merge from that node's cwd
# (codex P2). The global $ALLOW_MERGE tri-state here carries ONLY an explicit
# --allow-merge/--no-merge flag (applies to every node); "" = no flag = resolve
# config per node below. resolve_node_posture prints 1 (allow) or 0 (no-merge)
# for a given node cwd: an explicit flag wins; else config.dispatch.auto_merge
# read from THAT cwd. `fno config get` prints a Python bool (`True`/`False`) and
# has no cwd flag, so cd in a subshell then lowercase before the exact-`true`
# compare; any error / non-true output (stale fno, absent config, gone cwd)
# degrades to no-merge (Locked Decision 6: never grant merge on error).
resolve_node_posture() {
  local node_cfg_cwd="$1"
  if [[ -n "$ALLOW_MERGE" ]]; then printf '%s' "$ALLOW_MERGE"; return; fi
  local am
  am="$( ( [[ -n "$node_cfg_cwd" ]] && cd "$node_cfg_cwd" 2>/dev/null; fno config get dispatch.auto_merge 2>/dev/null ) | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]' || true)"
  [[ "$am" == "true" ]] && printf '1' || printf '0'
}

# x-4391: remove a single standalone `no-merge` token from a /target-family
# command under allow posture. The resolver builtin (_AUTONOMOUS_COMMAND) and
# config.dispatch.command can bake `no-merge` into the resolved command, so
# skipping injection alone would leave it live and make auto_merge=true silently
# dead. Space-delimited replacement (AC1-EDGE: never a substring - a pathological
# id like `no-merger-x` is untouched); guarded to /target|$fno:target so a
# non-/target command or a prose brief's text is never mangled.
strip_no_merge() {
  local cmd="$1"
  case "$cmd" in
    "/target "*|'$fno:target '*) : ;;
    *) printf '%s' "$cmd"; return ;;
  esac
  local padded=" $cmd "
  padded="${padded/ no-merge / }"            # first standalone token only
  padded="${padded#"${padded%%[![:space:]]*}"}"   # ltrim the pad
  padded="${padded%"${padded##*[![:space:]]}"}"   # rtrim the pad
  printf '%s' "$padded"
}

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
# jq `//` treats "" as truthy, so filter empties with select() before the
# fallback (repo rule) - a resolver that ever returned "" must read as absent.
DISPATCH_PROVIDER="$(printf '%s' "$resolve_json" | jq -r '.harness | select(. != null and . != "")' 2>/dev/null)"
DISPATCH_SUBSTRATE="$(printf '%s' "$resolve_json" | jq -r '.substrate | select(. != null and . != "")' 2>/dev/null)"
# Per-harness command TEMPLATE (x-567d): a native skill invocation where one is
# verified (claude `/target`, codex `$fno:target`, agy `/target`) or a prose
# brief (opencode/gemini, which have no footnote slash surface). `{id}` is
# substituted per node below. claude keeps its local tgt_cmd builder (FLAGS /
# --allow-merge), so a missing template only matters for the non-claude lanes.
DISPATCH_COMMAND="$(printf '%s' "$resolve_json" | jq -r '.command | select(. != null and . != "")' 2>/dev/null)"
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
  # node's worker spawns on the tier model too - bash never resolves. Scope the
  # pick to the RESOLVED dispatch provider (x-567d), not a hardcoded `claude`: a
  # tier that resolves to a model of a DIFFERENT harness is dropped to the
  # provider default rather than passed as an invalid `<provider> --model
  # <foreign>` (the cross-harness mismatch obs 100675 named). Empty output (no
  # pin/tier, cross-harness pick, or any resolve error) -> zero args.
  if [[ -z "$model_pin" ]]; then
    model_pin="$(fno target resolve-model "$id" --provider "$DISPATCH_PROVIDER" 2>/dev/null | head -1 | tr -d '[:space:]' || true)"
  fi
  model_args=()
  [[ -n "$model_pin" ]] && model_args=("--model" "$model_pin")

  # x-dfa4: forward the permission mode to the worker spawn (same array
  # discipline as model_args - avoids word-splitting at the trust boundary).
  # CLAUDE-ONLY on the autonomous lane (codex review P2): `fno agents spawn`
  # REJECTS --permission-mode for a non-claude provider on a non-pane substrate
  # (cli.py), so forwarding it to a codex/gemini/agy/opencode headless worker
  # fails the whole dispatch. Non-claude workers get their bypass from the
  # resolved permission_bypass caps, not this flag. Empty => byte-identical.
  perm_args=()
  perm_hint=""  # dry-run preview mirror of perm_args (claude-gated, codex review P2)
  if [[ -n "$PERMISSION_MODE" && "$DISPATCH_PROVIDER" == "claude" ]]; then
    perm_args=("--permission-mode" "$PERMISSION_MODE")
    perm_hint="--permission-mode $PERMISSION_MODE "
  fi

  # x-b0b4: a claude worker rides the build lane. The build/route lane is
  # claude-SPECIFIC (model routing over the claude subscription), and the runtime
  # classifies any role-/route-bearing spawn as Python-owned - but Python's
  # dispatchable set is claude/codex/gemini only, so a role-bearing opencode/agy
  # spawn exits "unknown provider" BEFORE reaching its Rust headless dispatcher
  # (x-567d / codex P1). Gate both on claude: non-claude spawns carry neither, so
  # they reach their native dispatch path. Empty arrays need the bash-3.2 `+`
  # guard at expansion (below) - do NOT expand a bare "${role_args[@]+"${role_args[@]}"}".
  role_args=()
  route_args=()
  role_hint=""  # dry-run preview mirror of role_args (safe for the empty case)
  if [[ "$DISPATCH_PROVIDER" == "claude" ]]; then
    role_args=("--role" "build")
    role_hint="--role build "
    [[ -n "$ROUTE" ]] && route_args=("--route" "$ROUTE")
  fi
  # x-9f75: group pane workers by project - pass --squad <node.project> so
  # same-project dispatches converge into one workspace (create-if-absent lives
  # server-side in run_pane). --squad is pane-only (the CLI rejects it for
  # bg/headless), so gate on the substrate; a bg worker is a detached thread
  # with no tab to group. Best-effort: an empty project just omits the flag.
  squad_args=()
  squad_hint=""
  if [[ "$DISPATCH_SUBSTRATE" == "pane" ]]; then
    # `.project` is a Rust String (serialized "" when unset, not null), and jq's
    # `//` treats "" as truthy, so filter empties explicitly before the fallback.
    node_project="$(printf '%s' "$node_json" | jq -r '.project | select(. != "") // empty' 2>/dev/null)"
    if [[ -n "$node_project" ]]; then
      squad_args=("--squad" "$node_project")
      squad_hint="--squad $node_project "
    fi
  fi
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
  if [[ "$DISPATCH_PROVIDER" != "claude" ]]; then
    # Non-claude carries no build/route lane (gated above); the receipt reflects
    # the native provider, not a claude route.
    route_val="$DISPATCH_PROVIDER"
  elif [[ -n "$ROUTE" ]]; then
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

  # x-4391: per-node merge posture, read from THIS node's project cwd so a batch
  # spanning repos honors each project's config.dispatch.auto_merge (codex P2). An
  # explicit flag (in $ALLOW_MERGE) wins for every node; else config-per-node.
  node_allow_merge="$(resolve_node_posture "$(printf '%s' "$node_json" | jq -r '._resolved_cwd // .cwd // empty' 2>/dev/null)")"

  # ---- Build the worker command + resolve the launch cwd ----
  # Command precedence, single source = `fno dispatch resolve`:
  #   node dispatch_verb / dispatch_brief (US3, x-f78d)  >  per-harness builtin (x-567d)
  # A node verb/brief goes through the resolver (validates the verb allowlist,
  # caps the brief at 8 KB, emits TARGET_BRIEF); no override -> claude builds its
  # native /target locally (--flags / --allow-merge, byte-identical) and every
  # OTHER harness uses the per-harness command the initial resolve chose (codex
  # `$fno:target`, agy `/target`, opencode/gemini prose brief). no-merge is a
  # launcher flag for a /target-family command; a prose brief carries it in prose.
  # select(. != "") before // empty: jq's // treats "" as truthy (repo idiom).
  dispatch_verb="$(printf '%s' "$node_json" | jq -r '.dispatch_verb | select(. != "") // empty' 2>/dev/null)"
  dispatch_brief="$(printf '%s' "$node_json" | jq -r '.dispatch_brief | select(. != "") // empty' 2>/dev/null)"
  TARGET_BRIEF_ENV=""
  if [[ -n "$dispatch_verb" || -n "$dispatch_brief" ]]; then
    # --harness so the resolver normalizes the verb per-harness (x-a5e4): a
    # `/target` verb resolves to `$fno:target {id}` on codex, `/target {id}` on
    # claude/agy, a prose brief on gemini/opencode. no-merge is injected below.
    resolve_args=(dispatch resolve --node "$id" --harness "$DISPATCH_PROVIDER" -J)
    [[ -n "$dispatch_verb" ]] && resolve_args+=(--verb "$dispatch_verb")
    [[ -n "$dispatch_brief" ]] && resolve_args+=(--brief "$dispatch_brief")
    resolved_json="$(fno "${resolve_args[@]}" 2>/dev/null)"; resolve_rc=$?
    if [[ "$resolve_rc" -ne 0 ]] || ! printf '%s' "$resolved_json" | jq -e '.command' >/dev/null 2>&1; then
      # Refused verb / oversized brief (or a stale fno without the flags): fail
      # closed and leave the node re-dispatchable, never launch a wrong command.
      fno claim release "$res_key" --holder "$res_holder" >/dev/null 2>&1 || true
      echo "failed $id reason=\"dispatch resolve refused verb/brief (rc=$resolve_rc); node not dispatched\""
      n_failed=$((n_failed + 1))
      continue
    fi
    tgt_cmd="$(printf '%s' "$resolved_json" | jq -r '.command')"
    TARGET_BRIEF_ENV="$(printf '%s' "$resolved_json" | jq -r '.env.TARGET_BRIEF | select(. != "") // empty')"
    # /target-family command (claude `/target`, codex `$fno:target`): thread
    # --flags + the no-merge default. Both invoke the same target skill, so both
    # must get no-merge - else a codex node with dispatch_verb=/target resolves to
    # `$fno:target <id>` WITHOUT no-merge and a configured auto-merge could merge
    # it (codex review P1). A prose brief carries "do not merge" in its prose.
    tgt_prefix=""
    [[ "$tgt_cmd" == "/target "* ]] && tgt_prefix="/target "
    [[ "$tgt_cmd" == '$fno:target '* ]] && tgt_prefix='$fno:target '
    if [[ -n "$tgt_prefix" ]]; then
      rest="${tgt_cmd#"$tgt_prefix"}"
      inject=""
      [[ -n "$FLAGS" ]] && inject="$FLAGS "
      if [[ "$node_allow_merge" -eq 0 && " $FLAGS " != *" no-merge "* && " $rest " != *" no-merge "* ]]; then
        inject="${inject}no-merge "
      elif [[ "$node_allow_merge" -eq 1 ]]; then
        # allow posture: strip a resolver-/template-baked no-merge from rest. A
        # no-merge in --flags is per-run explicit (rung 1) and rides in $inject,
        # untouched. strip on the bare rest, then re-add the prefix.
        rest="$(strip_no_merge "${tgt_prefix}${rest}")"
        rest="${rest#"$tgt_prefix"}"
      fi
      tgt_cmd="${tgt_prefix}${inject}${rest}"
    fi
  elif [[ "$DISPATCH_PROVIDER" == "claude" ]]; then
    # claude native /target, built locally: /target [FLAGS] [no-merge] <id>
    # (Locked Decision 4; --allow-merge opts out). Byte-identical to before.
    tgt_cmd="/target"
    [[ -n "$FLAGS" ]] && tgt_cmd="$tgt_cmd $FLAGS"
    if [[ "$node_allow_merge" -eq 0 && " $FLAGS " != *" no-merge "* ]]; then
      tgt_cmd="$tgt_cmd no-merge"
    fi
    tgt_cmd="$tgt_cmd $id"
  elif [[ -n "$DISPATCH_COMMAND" ]]; then
    # non-claude per-harness builtin (x-567d), {id} substituted (codex
    # `$fno:target`, agy `/target`, opencode/gemini prose brief).
    tgt_cmd="${DISPATCH_COMMAND//\{id\}/$id}"
    # x-4391: the builtin template bakes no-merge (_AUTONOMOUS_COMMAND); under
    # allow posture strip it from a /target-family command (a prose brief is
    # guarded out by strip_no_merge's prefix check).
    [[ "$node_allow_merge" -eq 1 ]] && tgt_cmd="$(strip_no_merge "$tgt_cmd")"
  else
    fno claim release "$res_key" --holder "$res_holder" >/dev/null 2>&1 || true
    echo "failed $id reason=\"resolver returned no command for harness '$DISPATCH_PROVIDER'; update fno or set config.dispatch.command\""
    n_failed=$((n_failed + 1))
    continue
  fi

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
    echo "launched $id name=$agent_name session=DRY-RUN cwd=${dry_cwd} hint=\"would run: fno agents spawn --provider $DISPATCH_PROVIDER --substrate $DISPATCH_SUBSTRATE ${cwd_hint}${squad_hint}${role_hint}${ROUTE:+--route $ROUTE }${model_pin:+--model $model_pin }${perm_hint}$agent_name '$tgt_cmd'\"${TARGET_BRIEF_ENV:+ brief=set} route=${route_val}"
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
  # Carry the US3 brief to the worker via env (inherited by claude --bg), never
  # on the command line. Exported unconditionally (empty when the node has no
  # brief) so a prior loop iteration's brief can never leak into a later node.
  export TARGET_BRIEF="$TARGET_BRIEF_ENV"
  spawn_err_file="$(mktemp 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}/dispatch-node-$$.err")"
  # Three explicit branches (NOT an optional-flag array): bash 3.2 (macOS)
  # errors on `"${arr[@]}"` for an empty array under `set -u`. node cwd ->
  # --cwd; no node cwd + default -> ensure a conductor worktree and pass --cwd
  # it (deterministic isolation, x-73ca), falling back to --fresh on any ensure
  # failure (empty $wt) so the dispatch is never blocked; --here -> inherit.
  launch_cwd="${node_cwd:-$(pwd)}"
  if [[ -n "$node_cwd" ]]; then
    spawn_out="$(fno agents spawn --provider "$DISPATCH_PROVIDER" --substrate "$DISPATCH_SUBSTRATE" --cwd "$node_cwd" "${squad_args[@]+"${squad_args[@]}"}" "${role_args[@]+"${role_args[@]}"}" "${route_args[@]+"${route_args[@]}"}" "${model_args[@]+"${model_args[@]}"}" "${perm_args[@]+"${perm_args[@]}"}" "$agent_name" "$tgt_cmd" 2>"$spawn_err_file")"; spawn_rc=$?
  elif [[ "$HERE" -eq 0 ]]; then
    wt=""
    # DISPATCH_PROVIDER is the RESOLVED harness (.harness from dispatch resolve),
    # so forward it as --harness: a claude bg dispatch lands harness-native at
    # <repo>/.claude/worktrees/, a non-native harness degrades to external.
    [[ -n "$CANONICAL_ROOT" ]] && wt="$(fno worktree ensure --repo "$CANONICAL_ROOT" --name "$agent_name" --harness "$DISPATCH_PROVIDER" 2>/dev/null)"
    if [[ -n "$wt" ]]; then
      # policy=never returns the repo root: launch in place, but SKIP setup - it
      # links shared state INTO the canonical checkout (Locked Decision 4: guard
      # worktree-only side effects on path == repo root). Compare physical paths
      # (ensure prints the resolved root; CANONICAL_ROOT is not phys-resolved).
      _wt_phys="$(cd "$wt" 2>/dev/null && pwd -P || printf '%s' "$wt")"
      _root_phys="$(cd "$CANONICAL_ROOT" 2>/dev/null && pwd -P || printf '%s' "$CANONICAL_ROOT")"
      if [[ "$_wt_phys" != "$_root_phys" ]]; then
        # Link gitignored shared state into the new worktree (footnote-ecosystem
        # only; absent -> skip). Caller-side because the verb is package code and
        # may not shell out to a repo-root script (shellout-drift gate).
        _wt_setup="$CANONICAL_ROOT/scripts/setup/setup-worktree.sh"
        [[ -f "$_wt_setup" ]] && CANONICAL="$CANONICAL_ROOT" WORKTREE="$wt" bash "$_wt_setup" >/dev/null 2>&1
      fi
      spawn_out="$(fno agents spawn --provider "$DISPATCH_PROVIDER" --substrate "$DISPATCH_SUBSTRATE" --cwd "$wt" "${squad_args[@]+"${squad_args[@]}"}" "${role_args[@]+"${role_args[@]}"}" "${route_args[@]+"${route_args[@]}"}" "${model_args[@]+"${model_args[@]}"}" "${perm_args[@]+"${perm_args[@]}"}" "$agent_name" "$tgt_cmd" 2>"$spawn_err_file")"; spawn_rc=$?
      launch_cwd="$wt"
    else
      spawn_out="$(fno agents spawn --provider "$DISPATCH_PROVIDER" --substrate "$DISPATCH_SUBSTRATE" --fresh "${squad_args[@]+"${squad_args[@]}"}" "${role_args[@]+"${role_args[@]}"}" "${route_args[@]+"${route_args[@]}"}" "${model_args[@]+"${model_args[@]}"}" "${perm_args[@]+"${perm_args[@]}"}" "$agent_name" "$tgt_cmd" 2>"$spawn_err_file")"; spawn_rc=$?
      # --fresh lands the worker in canonical main; report that real path (not a
      # space-containing label) so the cwd= field stays machine-parseable.
      launch_cwd="${CANONICAL_ROOT:-$(pwd)}"
    fi
  else
    spawn_out="$(fno agents spawn --provider "$DISPATCH_PROVIDER" --substrate "$DISPATCH_SUBSTRATE" "${squad_args[@]+"${squad_args[@]}"}" "${role_args[@]+"${role_args[@]}"}" "${route_args[@]+"${route_args[@]}"}" "${model_args[@]+"${model_args[@]}"}" "${perm_args[@]+"${perm_args[@]}"}" "$agent_name" "$tgt_cmd" 2>"$spawn_err_file")"; spawn_rc=$?
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
    sid="$(printf '%s\n' "$spawn_out" | grep -F '"short_id"' | head -1 | jq -r '.short_id | select(. != null and . != "")' 2>/dev/null)"
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
