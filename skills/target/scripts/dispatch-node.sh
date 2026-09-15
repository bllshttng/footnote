#!/usr/bin/env bash
# dispatch-node.sh - Dispatch ready backlog node(s) as fresh detached `/target`
# workers, fire-and-forget, with a per-node outcome line. The launch is ONE
# command per node: `fno agents spawn --node <id> --substrate thread` (x-3873).
# The spawn door renders the seed from the node's declared verb and brief chain,
# picks the lane from the grid while the model axis is free, ensures the launch
# worktree, and takes the family-2 guard against double-dispatch. This script
# passes the node and only what the human typed - nothing else.
#
# Self-contained skill script. External deps: `fno` (backlog, agents) + jq.
# See internal/fno/plans/2026-06-02-target-plan-mode-phase2.md (US5/US6).
#
# Usage:
#   dispatch-node.sh <node-id...> [--max N] [--dry-run] [--here]
#                                 [--permission-mode <mode>] [--route provider/model]
#   dispatch-node.sh --all-ready  [--max N] [--dry-run] [--here]
#                                 [--permission-mode <mode>] [--route provider/model]
#
# Merge posture (x-3873): config.auto_merge.grant decides at target init, the
# same way it does for every advance worker. There is no per-run flag. The
# per-run override is a typed message on the spawn itself:
#   fno agents spawn --node <id> '/fno:target --no-merge <id>'
# A typed /target or /blueprint must agree with the verb the node derives
# (x-2c0d); on disagreement the spawn refuses before any lane is spent, so
# drop the verb from the payload and let --node supply it.
#
# --route provider/model: per-dispatch explicit model route (x-b0b4), forwarded
#   to every worker spawn only when typed. Fails CLOSED in the spawn.
#
# --here / --in-place: keep the worker in the dispatcher's cwd instead of the
#   door's worktree-ensure default.
#
# Per-node outcome lines (stdout; one per node; NEVER silent):
#   launched         <node> name=<agent> session=<sid> hint="fno agents logs <agent>"
#   already-running  <node> reason="live target worker holds node:<id> (<holder>)"
#   skipped-contested <node> reason="suspect claim (respawned worker); advancing" (x-ba4b)
#   parked           <node> reason="blocked|deferred|<status> (not up-next)"
#   skipped-done     <node> reason="already done|superseded"
#   failed           <node> reason="<why>"
#   deferred-cap     <node> reason="--max <N> reached"
#   degraded-name    <node> reason="canonical naming unavailable (rc=<n>); ..."
#                    (advisory; the canonical name owner was unreachable and the
#                     fallback assembly named this worker. Precedes that node's
#                     real outcome line - launched, or failed if the fallback
#                     name itself breaks the 64-char runtime contract.)
# Summary (last line):
#   summary: launched=<n> parked=<n> already=<n> skipped=<n> done=<n> failed=<n> capped=<n>[ nothing-up-next]
#
# Invariants (Failure Modes section of the plan):
#   - The launch is the door (fno agents spawn), the one launch verb. The
#     substrate is the detached thread lane. NEVER
#     `--bare`/`-p` (those force the API-credit pool and strip skills/hooks).
#   - A failed dispatch is surfaced and leaves the node `ready`/re-dispatchable;
#     never reports a launch that did not happen; never silently swallows.
#   - Fire-and-forget: this script NEVER writes/clears the caller's
#     .fno/target-state.md. The planning session is untouched.
#   - Under --all-ready, `ready` nodes and plan-less `idea` nodes (Rung.NONE,
#     cold-dispatchable per x-e24a) dispatch; a linked idea stub (plan_path set,
#     Rung.IDEA) and `design` are parked. An EXPLICITLY-NAMED node also dispatches
#     when idea-status (the triage pile; there is no distinct `triage` status) -
#     naming it is the human's vet, the worker runs think->blueprint->do;
#     blocked/deferred are always parked.

set -uo pipefail

# ---- deps -------------------------------------------------------------------
command -v fno >/dev/null 2>&1 || { echo "failed: - reason=\"fno not on PATH\"" >&2; echo "summary: launched=0 parked=0 already=0 skipped=0 done=0 failed=1 capped=0"; exit 1; }
command -v jq  >/dev/null 2>&1 || { echo "failed: - reason=\"jq not on PATH\""  >&2; echo "summary: launched=0 parked=0 already=0 skipped=0 done=0 failed=1 capped=0"; exit 1; }

# ---- arg parse --------------------------------------------------------------
NODES=()
ALL_READY=0
MAX=0          # 0 => no cap (quota is the throttle; do not invent a hard cap)
DRY_RUN=0
HERE=0         # 1 => keep the worker in the dispatcher's cwd (opt out of ensure)
PERMISSION_MODE=""  # forwarded as --permission-mode only when the human typed it
ROUTE=""       # x-b0b4: per-dispatch explicit provider,model route (fail-closed)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --all-ready)  ALL_READY=1; shift ;;
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

# ---- per-node dispatch ------------------------------------------------------
n_launched=0; n_parked=0; n_already=0; n_skipped=0
n_wedged=0; n_done=0; n_failed=0; n_capped=0

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

  status="$(printf '%s' "$node_json" | jq -r '.status // "unknown"')"
  plan_path="$(printf '%s' "$node_json" | jq -r '.plan_path // ""')"

  case "$status" in
    done)
      echo "skipped-done $id reason=\"node already done\""
      n_done=$((n_done + 1))
      continue ;;
    superseded)
      echo "skipped-done $id reason=\"node superseded\""
      n_done=$((n_done + 1))
      continue ;;
    ready|in_progress|claimed)
      # ready => dispatchable. in_progress (legacy: claimed) => a worker may
      # already hold it; the live-claim check below reports already-running, or
      # (stale claim => dead worker) falls through to re-dispatch as recovery.
      # Both spellings are accepted: a graph row persisted before the
      # claimed -> in_progress rename still reads the old token until its next
      # mutation recomputes it.
      : ;;
    design|idea)
      # Explicitly naming a pre-ready node (the triage pile is idea-status;
      # `design` is a linked-but-unblueprinted doc) IS the human's vet: dispatch
      # it and let the /target worker run the phases the rung still needs -
      # think->blueprint->do from `idea`, blueprint->do from `design`.
      # Under --all-ready a plan-less idea (Rung.NONE) is cold-dispatchable
      # (x-e24a) and drains like ready work; a linked idea stub (plan_path set,
      # Rung.IDEA) or a `design` doc still needs warm inline-fill, so park it.
      if [[ "$ALL_READY" -eq 1 && ( "$status" != "idea" || -n "$plan_path" ) ]]; then
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

  # ---- Read-only early receipt; cmd_spawn owns the real guard (x-5c08) ----
  # Dry-run and legacy claimed-node parking need a verdict before the spawn
  # branch. A real ready dispatch passes --node to `fno agents spawn`, whose one
  # birth choke point reruns this family-2 decision with side effects and takes
  # dispatch:<id>; no reservation is taken on this shell rung.
  # Fail CLOSED: a stale `fno` without the verb (or any non-clean/unparseable
  # verdict) leaves the node `ready` and launches nothing.
  # spawn-guard is a Python-only verb (no Rust client impl). Pin the call to the
  # Python runtime so an operator with FNO_AGENTS_RUNTIME=rust exported does not
  # route it to the Rust binary (which lacks it -> 127 -> the guard fails closed
  # and bg-dispatch breaks). The inline override is scoped to this command; the
  # real `fno agents spawn` below routes normally. The default (unset) runtime
  # already keeps spawn-guard Python.
  res_key="dispatch:$id"; res_holder="dispatch-node:$$"
  node_guard_cwd="$(printf '%s' "$node_json" | jq -r '._resolved_cwd // .cwd // empty' 2>/dev/null)"
  guard_cwd_args=()
  [[ -n "$node_guard_cwd" ]] && guard_cwd_args=("--cwd" "$node_guard_cwd")
  if [[ "$DRY_RUN" -eq 1 || "$status" == "claimed" ]]; then
    guard_out="$(FNO_AGENTS_RUNTIME=python fno agents spawn-guard "$id" --holder "$res_holder" --no-reserve --json ${guard_cwd_args[@]+"${guard_cwd_args[@]}"} 2>/dev/null)"; guard_rc=$?
  else
    guard_out='{"verdict":"dispatchable"}'; guard_rc=0
  fi
  # grep the JSON object line first (defense in depth vs any stderr/banner that
  # could leak onto stdout), then parse the verdict.
  guard_json="$(printf '%s\n' "$guard_out" | grep -F '"verdict"' | head -1)"
  verdict="$(printf '%s' "$guard_json" | jq -r '.verdict // empty' 2>/dev/null)"
  # NO untried-wedge rewrite here, unlike spawn.sh, and the asymmetry is the
  # point. This probe runs only under --dry-run or for a `claimed` node. A dry
  # run must preview, never launch, and a `claimed` node is parked for manual
  # recovery by the arm below. Every other node skips the probe entirely and
  # goes straight to the real spawn, which is where the recovery already
  # happens. A rewrite would be dead code wearing a safety comment.
  case "$verdict" in
    already-running)
      reason="$(printf '%s' "$guard_json" | jq -r '.reason // empty' 2>/dev/null)"
      if [[ "$reason" == "live-claim" ]]; then
        holder="$(printf '%s' "$guard_json" | jq -r '.holder // "unknown"' 2>/dev/null)"
        echo "already-running $id reason=\"live target worker holds node:$id ($holder)\""
        n_already=$((n_already + 1))
      elif [[ "$reason" == "unproven-claim" ]]; then
        # The claim is held, and that is ALL that was measured. Saying "live
        # target worker" here asserted a worker nothing had tested, at the exact
        # moment a reader decides whether to staff the node. Same skip, honest
        # sentence.
        holder="$(printf '%s' "$guard_json" | jq -r '.holder // "unknown"' 2>/dev/null)"
        echo "already-running $id reason=\"node:$id is held by $holder but no target init took that claim; no worker has reached target init\""
        n_already=$((n_already + 1))
      elif [[ "$reason" == "worker-row" ]]; then
        # The worker ROW is the occupant, not the claim. The old text named the
        # claim, sent the reader hunting a release remedy for a claim nobody
        # holds, and hid the one actionable field: who is on the node.
        worker="$(printf '%s' "$guard_json" | jq -r '.worker // empty' 2>/dev/null)"
        if [[ "$(printf '%s' "$guard_json" | jq -r '.worker_unmeasured // empty' 2>/dev/null)" == "true" ]]; then
          echo "already-running $id reason=\"worker row ${worker:-unmeasured} is on node:$id with liveness never measured; peek it, read fno agents claim status node:$id, and stop it if its run is finished\""
        else
          echo "already-running $id reason=\"worker row ${worker:-unmeasured} is on node:$id; peek it, and stop it if its run is finished\""
        fi
        n_already=$((n_already + 1))
      elif [[ "$reason" == "suspect-claim" ]]; then
        # x-ba4b: TTL-unexpired dead-pid claim (a respawned worker). Contested
        # liveness degrades to SKIP, never steal and never park the lane -
        # advance to the next unblocked ready node.
        holder="$(printf '%s' "$guard_json" | jq -r '.holder // "unknown"' 2>/dev/null || true)"
        # NO remedy read here, and no n_wedged. This arm sees only the PROBE's
        # verdict, and a probe never carries one: the remedy is force-release
        # advice that is honest only after a recovery has been tried, so the
        # guard withholds it under --no-reserve. Reading `.remedy` here found
        # the empty string every time, so the counter could not increment and
        # the wedge exit below was unreachable from this arm. The wedge that
        # matters is counted at the post-spawn refusal, which is the only place
        # a tried-and-failed recovery can be observed.
        echo "skipped-contested $id reason=\"suspect claim on node:$id ($holder); respawned worker, advancing\""
        n_skipped=$((n_skipped + 1))
      else
        # x-a7ab 1.2 / x-b44e: a peer dispatcher holds dispatch:<id> (reservation-
        # held, or won the visibility barrier). Mirror spawn.sh's receipt so the
        # /target bg and /agent spawn dispatch paths never disagree: the loser
        # carries skipped: duplicate-claim, not a generic racing-launch line.
        echo "already-running $id reason=\"skipped: duplicate-claim (peer dispatcher holds $res_key)\""
        n_already=$((n_already + 1))
      fi
      continue ;;
    corrupted)
      # The worker's init-side `fno agents claim acquire` cannot reclaim a corrupted
      # claim, so launching would run WITHOUT the node:<id> mutex and leave the
      # corrupt lock in place (external review P2). Fail closed; an operator
      # force-releases/repairs it before re-dispatch.
      echo "failed $id reason=\"node:$id claim is corrupted; force-release or repair before dispatching\""
      n_failed=$((n_failed + 1))
      continue ;;
    refused)
      reason="$(printf '%s' "$guard_json" | jq -r '.reason // "dispatch-refused"' 2>/dev/null)"
      holder="$(printf '%s' "$guard_json" | jq -r '.holder // "unknown"' 2>/dev/null)"
      echo "skipped $id reason=\"$reason by family-2 guard (prior holder=$holder); no worker launched\""
      n_skipped=$((n_skipped + 1))
      continue ;;
    dispatchable)
      if [[ "$status" == "claimed" ]]; then
        # status: claimed but node:<id> claim not live. Do NOT auto-recover via
        # dispatch (external review P2): the worker init may see a stale legacy
        # graph session_id, refuse to record graph_node_id, run anyway, then be
        # unable to clear the legacy graph claim on exit - leaving the node stuck
        # claimed/hidden. Park for manual recovery (--no-reserve was used above,
        # so there is no reservation to release).
        echo "parked $id reason=\"claimed but node:$id claim not live; needs manual recovery (legacy graph claim may be stuck)\""
        n_parked=$((n_parked + 1))
        continue
      fi
      # status == ready reaches the real shared guard inside cmd_spawn. A stale
      # predecessor is reclaimed there; a failure limit or racing reservation
      # refuses before substrate fan-out.
      : ;;
    *)
      # verdict=error, OR empty/unparseable (a stale fno WITHOUT the verb prints
      # Typer "No such command" + exits non-zero; or a probe crash): fail CLOSED.
      detail="$(printf '%s' "$guard_json" | jq -r '.detail // empty' 2>/dev/null)"
      echo "failed $id reason=\"${detail:-spawn-guard unavailable (rc=$guard_rc); not dispatching to avoid a double-launch}\""
      n_failed=$((n_failed + 1))
      continue ;;
  esac

  # Provenance-carrying name (x-84b2): [<source>-]<verb-code>-<node>-<slug>,
  # minted through the canonical owner (x-3218), which sanitizes the slug AND
  # budgets the assembled name against the runtime's 64-char limit. No --verb:
  # the seed (and so the verb) is rendered by the spawn door from the node.
  # FNO_AGENTS_RUNTIME=python pins the Python dispatch: an ambient `=rust` routes
  # EVERY `fno agents` verb to the binary, which has no `name` port. Exit 3 (not
  # 2) is the naming refusal - Click spends 2 on usage errors including "no such
  # command", so an `fno` too old to know this verb would otherwise read as
  # "unrepresentable" and refuse the whole fleet.
  # Streams are merged so a refusal's cause survives; the name is read as the
  # LAST line of the capture and that line alone must match the runtime
  # contract (a live config notice on stderr reproduced a false refusal on
  # 2026-09-03 when the WHOLE capture was matched).
  node_slug="$(printf '%s' "$node_json" | jq -r '.slug // .title // empty' 2>/dev/null)"
  # --verb t: the bridge refuses a verb-less mint (it usage-refused every
  # dispatch here until 2026-09-14, so every name came from the fallback
  # assembly below). FNO_AGENTS_NAME_MODEL: the route's model when pinned, so
  # the name carries the model tag that makes a misroute visible at a glance
  # (x-57fe); an env var, not a flag - the flag registry refuses Python flag
  # growth (x-72fc).
  name_args=("$id" --verb t --slug "$node_slug")
  name_env=(FNO_AGENTS_RUNTIME=python)
  [[ -n "$ROUTE" ]] && name_env+=(FNO_AGENTS_NAME_MODEL="${ROUTE##*/}")
  name_out="$(env "${name_env[@]}" fno agents name "${name_args[@]}" 2>&1)"
  name_rc=$?
  name_last="${name_out##*$'\n'}"
  agent_name=""
  [[ "$name_rc" -eq 0 && "$name_last" =~ ^[A-Za-z0-9_-]{1,64}$ ]] && agent_name="$name_last"
  if [[ "$name_rc" -eq 3 ]]; then
    # Exit 3 covers every refusal cause (unknown source/verb, over-budget
    # identity, invalid characters), so relay the real message. Newlines and
    # double quotes are squeezed out first: this line has a documented grammar
    # other tools parse, and the message embeds a repr of the node id.
    name_msg="$(printf '%s' "${name_out:-agent name cannot be represented}" | tr '\n"' '  ')"
    echo "failed $id reason=\"$name_msg\""
    n_failed=$((n_failed + 1))
    continue
  elif [[ "$name_rc" -ne 0 || -z "$agent_name" ]]; then
    # Degraded: fno unreachable or too old for this verb. Keep a fallback
    # assembly, and say so - an invisible degrade means the whole fleet can be
    # named by the fallback with nothing in the receipt to show it. The
    # fallback mirrors the canonical shape (t-<hex>[-<slug>]) minus the model;
    # the vocabulary is never re-implemented here.
    # rc=0 here means the owner ran but its output was unusable (noise on the
    # merged stream), which is a different story from an unreachable owner - say
    # which, or the receipt reads as "unavailable (rc=0)" and puzzles the reader.
    if [[ "$name_rc" -eq 0 ]]; then
      name_why="canonical naming returned an unusable name"
    else
      name_why="canonical naming unavailable (rc=$name_rc)"
    fi
    echo "degraded-name $id reason=\"$name_why; using the fallback assembly\""
    node_slug="$(printf '%s' "$node_slug" | tr '[:upper:]' '[:lower:]' | tr -c 'a-z0-9-' '-' \
      | sed -E 's/-+/-/g; s/^-+//; s/-+$//' | cut -c1-12 | sed -E 's/-+$//')"
    node_hex="${id##*-}"
    if [[ -n "$node_slug" ]]; then
      agent_name="t-${node_hex}-${node_slug}"
    else
      agent_name="t-${node_hex}"
    fi
    # The fallback is uncapped, and nothing downstream enforces 64 here: this
    # spawn passes --node, which forces the Python path (_NAME_MAX_LEN = 128),
    # so the daemon's 64-char validator is never reached. Refuse rather than
    # launch a worker under a name the runtime contract does not allow.
    if [[ "${#agent_name}" -gt 64 ]]; then
      echo "failed $id reason=\"fallback name is ${#agent_name} chars, over the 64-char runtime limit\""
      n_failed=$((n_failed + 1))
      continue
    fi
  fi

  # ---- Preview / launch ----
  # One command per node (x-3873 change 4): the door renders the seed, picks
  # the lane while the model axis is free, ensures the worktree, and takes the
  # family-2 guard. Only what the human typed rides beside --node: --here,
  # --route, --permission-mode. Worktree isolation is the door's (change 1), so
  # no ensure runs here and the receipt does not claim a landing directory.
  typed_args=()
  [[ "$HERE" -eq 1 ]] && typed_args+=(--here)
  [[ -n "$ROUTE" ]] && typed_args+=(--route "$ROUTE")
  [[ -n "$PERMISSION_MODE" ]] && typed_args+=(--permission-mode "$PERMISSION_MODE")

  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "launched $id name=$agent_name session=DRY-RUN hint=\"would run: fno agents spawn --node $id --substrate thread --name $agent_name${typed_args:+ ${typed_args[*]}}\""
    n_launched=$((n_launched + 1))
    continue
  fi

  # ---- Guard 3: the agents registry (safe now, under the reservation) ----
  # A LIVE same-name agent means a worker is already up (its node:<id> claim not
  # yet caught above); report already-running. A dead row is removed so `ask`
  # creates fresh rather than resuming it.
  # Capture the probe exit code AND require a parseable {agents:[...]}: a
  # crashed/garbled `fno agents list` (daemon down, stale install printing a
  # Typer error) must fail CLOSED (refuse), never collapse to an empty
  # existing_status and fall through to a double-launch in the boot window
  # (parity with spawn.sh Guard 3, cv-dddd8ae5; sigma silent-failure-hunter).
  agents_json="$(fno agents list 2>/dev/null)"; list_rc=$?
  if [[ "$list_rc" -ne 0 ]] || ! printf '%s' "$agents_json" | jq -e 'has("agents")' >/dev/null 2>&1; then
    echo "failed $id reason=\"agents-list probe failed (rc=$list_rc); not dispatching to avoid a double-launch\""
    n_failed=$((n_failed + 1))
    continue
  fi
  existing_status="$(printf '%s' "$agents_json" \
    | jq -r --arg n "$agent_name" '.agents[]? | select(.name==$n) | .status' 2>/dev/null | head -1)"
  if [[ "$existing_status" == "live" ]]; then
    echo "already-running $id reason=\"a live agent $agent_name already exists (worker booting/running)\""
    n_already=$((n_already + 1))
    continue
  elif [[ -n "$existing_status" ]]; then
    fno agents rm "$agent_name" >/dev/null 2>&1 || true
  fi

  # ---- Dispatch, fire-and-forget ----
  # The detached thread lane: for claude this is the `claude --bg` thread
  # (x-2c27) - it runs the node to completion unattended and shows in
  # `claude agents`. NOT an owned-PTY pane (a fire-and-forget dispatch must not
  # stall at a placement prompt) and NEVER --bare/-p (the API-credit pool).
  # stderr goes to a temp file, NOT 2>&1: a stderr warning must never pollute
  # the JSON receipt parse below (house rule; gemini review PR #457).
  spawn_err_file="$(mktemp 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}/dispatch-node-$$.err")"
  spawn_out="$(fno agents spawn --node "$id" --substrate thread --name "$agent_name" \
    ${typed_args[@]+"${typed_args[@]}"} 2>"$spawn_err_file")"; spawn_rc=$?
  spawn_err="$(cat "$spawn_err_file" 2>/dev/null)"; rm -f "$spawn_err_file"
  if [[ "$spawn_rc" -ne 0 ]]; then
    # The shared cmd_spawn guard reports a machine prefix before any substrate
    # launches. Preserve its exact family-2 outcome in this caller's receipt.
    if printf '%s' "$spawn_err" | grep -qF "node dispatch refused:"; then
      guard_verdict="$(printf '%s' "$spawn_err" | sed -n 's/.* verdict=\([^ ]*\).*/\1/p;q')"
      guard_reason="$(printf '%s' "$spawn_err" | sed -n 's/.* reason=\([^ ;]*\).*/\1/p;q')"
      case "$guard_reason" in
        live-claim)
          echo "already-running $id reason=\"$guard_reason by shared family-2 guard; no worker launched\""
          n_already=$((n_already + 1))
          continue ;;
        unproven-claim)
          # This arm carries the ordinary dispatch. The probe above runs only
          # under --dry-run or for a `claimed` node, so a ready node reaches
          # the guard verdict HERE and nowhere else. Without this arm the
          # reason falls past the esac to the generic failure handler, turning
          # a benign skip into `failed` and a non-zero exit for the batch.
          echo "already-running $id reason=\"node:$id is held but no target init took that claim; no worker has reached target init\""
          n_already=$((n_already + 1))
          continue ;;
        worker-row)
          guard_worker="$(printf '%s' "$spawn_err" | sed -n 's/.* worker=\([^ ;]*\).*/\1/p;q')"
          if printf '%s' "$spawn_err" | grep -qF 'worker_unmeasured=true'; then
            echo "already-running $id reason=\"worker row ${guard_worker:-unmeasured} is on node:$id with liveness never measured; peek it, read fno agents claim status node:$id, and stop it if its run is finished\""
          else
            echo "already-running $id reason=\"worker row ${guard_worker:-unmeasured} is on node:$id; peek it, and stop it if its run is finished\""
          fi
          n_already=$((n_already + 1))
          continue ;;
        reservation-held|duplicate-claim)
          echo "already-running $id reason=\"skipped: duplicate-claim (peer dispatcher holds dispatch:$id); no worker launched\""
          n_already=$((n_already + 1))
          continue ;;
        suspect-claim)
          echo "skipped-contested $id reason=\"suspect-claim by shared family-2 guard; no worker launched\""
          # From THIS refusal's stderr, not the probe JSON above: that variable
          # was assigned once, before the spawn, and by here always reads
          # `dispatchable`, so its `.remedy` was always empty and n_wedged could
          # never increment.
          remedy="$(printf '%s' "$spawn_err" | grep -E '^  (Clear it|Override): ' || true)"
          if [[ -n "$remedy" ]]; then
            echo "$remedy"
            n_wedged=$((n_wedged + 1))
          fi
          n_skipped=$((n_skipped + 1))
          continue ;;
        auto-deferred)
          echo "skipped $id reason=\"auto-deferred by shared family-2 guard; no worker launched\""
          n_skipped=$((n_skipped + 1))
          continue ;;
        defer-failed)
          echo "failed $id reason=\"defer-failed by shared family-2 guard; no worker launched\""
          n_failed=$((n_failed + 1))
          continue ;;
      esac
      if [[ "$guard_verdict" == "corrupted" ]]; then
        echo "failed $id reason=\"node:$id claim is corrupted; force-release or repair before dispatching\""
        n_failed=$((n_failed + 1))
        continue
      fi
    fi
    # Surface the failure. cmd_spawn owns and releases the family-2 claims on
    # every pre-launch or spawn error. A name collision (exit 2, "already
    # exists") means a worker beat us in the registry-check window: report
    # already-running, not failed.
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

  # The thread receipt is a compact JSON line naming the session. Parse
  # claude's short_id or a session_id carrier; a receipt with neither still
  # had exit 0 - the clean exit IS the launch proof, so label it `thread`.
  sid="$(printf '%s\n' "$spawn_out" | grep -F '"short_id"' | head -1 \
    | jq -r '.short_id | select(. != null and . != "")' 2>/dev/null)"
  [[ -z "$sid" ]] && sid="$(printf '%s\n' "$spawn_out" | grep -F '"session_id"' | head -1 \
    | jq -r '.session_id | select(. != null and . != "")' 2>/dev/null)"
  [[ -z "$sid" ]] && sid="thread"
  # Launched. The worker now owns node:<id>, which guards later dispatches.
  echo "launched $id name=$agent_name session=$sid hint=\"fno agents logs $agent_name\""
  n_launched=$((n_launched + 1))
done

echo "summary: launched=$n_launched parked=$n_parked already=$n_already skipped=$n_skipped done=$n_done failed=$n_failed capped=$n_capped wedged=$n_wedged"
# Exit non-zero only when nothing launched AND at least one hard failure, so a
# caller can detect a total dispatch failure while a mixed batch still exits 0.
if [[ "$n_launched" -eq 0 && "$n_failed" -gt 0 ]]; then
  exit 1
fi
# A WEDGE fails only a SINGLE-node invocation. A batch sweep must not fail
# because one node of twenty is wedged - it did the other nineteen - but
# `dispatch-node.sh <one-id>` that launched nothing because that node is wedged
# has no other work to report, and exiting 0 tells its caller the launch worked
# (x-05be shape 3). The remedy line is already on stdout above.
if [[ "$n_wedged" -gt 0 && "${#NODES[@]}" -eq 1 && "$n_launched" -eq 0 ]]; then
  exit 1
fi
exit 0
