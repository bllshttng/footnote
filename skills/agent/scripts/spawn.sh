#!/usr/bin/env bash
# spawn.sh - the honest-receipt core of /fno:agent (spawn verb).
#
# Runs the GENUINE `fno agents spawn|host <name> "<message>" --harness <h>`
# launch and reports ONLY a real captured receipt (short-id or one-shot reply). This is the cardinal guard (AC5-FR):
# the motivating session was a non-execution that *looked* like a command and
# was reported as success. Here the success/FAILED decision is deterministic
# shell, not model judgment: no 8-hex short-id in the real stdout => FAILED with
# the real stderr, never a fabricated uuid.
#
# The SKILL.md owns the CONFIRM gate; spawn.sh runs only after a yes. It still
# re-checks for a live duplicate right before the spawn (atomic, closing the
# window between the SKILL's read-only pre-check and here).
#
# Self-contained skill script. External deps: bash + `fno` (agents, claim) + jq.
#
# Usage:
#   spawn.sh --name <n> --harness <h> --message "<msg>" [--node <ab-XXXX>] [--cwd <dir>]
#
# Outcome (one line on stdout; NEVER silent, NEVER a fabricated uuid):
#   result=launched short_id=<hex> name=<n> hint="fno agents logs <hex>" trace="fno agents trace <n>"
#   result=launched short_id=<name> name=<n> pane="<session>:<pane_id>" hint="fno mux attach <session>"
#   result=already-running name=<n> reason="<why>"
#   result=failed reason="<real captured error>"
# Exit: 0 launched | 0 already-running | 1 failed.

set -uo pipefail

NAME=""
PROVIDER=""
MESSAGE=""
NODE=""
CWD=""
SELF=""                # caller's own claim holder (target_claim_holder). Lets
                       # the collision pre-check tell a self-held claim (route to
                       # the sanctioned handoff) from a foreign one (refuse).
# Routing inputs (codex/gemini first-class dispatch, ab-417ab20f). Defaults keep
# the legacy claude path equivalent: exec + build -> claude resolves to `spawn`
# (Group 1 ab-8b3e4fe0: ask never creates), so an old caller that passes none
# of these still launches a persistent claude bg peer.
MODE="exec"            # exec | interactive  (-i routes codex/gemini -> host)
MODEL=""               # exact model name, forwarded as `spawn --model` (each
                       # provider's own --model). Empty = provider default.
EFFORT=""              # reasoning effort forwarded as `spawn --effort`.
PAYLOAD_MODE="build"   # build (node-id /target) | seed | handoff | passthrough
SUBSTRATE=""           # x-2c27: ""|pane|bg|headless. bg -> claude --bg thread
                       # (JSON receipt); headless -> one-shot (reply receipt).
YOLO=0                 # 1 appends --yolo to the spawn/host argv
PERMISSION_MODE=""     # x-dfa4: forwarded as --permission-mode to the spawn verb
ROLE=""                # x-d2fe: forwarded as --role to the spawn verb (model routing)
TIMEOUT=""             # forwarded as --timeout to the spawn verb (per-spawn seconds)
FRESH=0                # 1 appends --fresh (canonical-root cwd) to the spawn argv
HERE=0                 # 1 appends --here (opt out of --fresh) to the spawn argv
ADD_DIR=""             # x-b6e2: forwarded as --add-dir to the spawn verb
AGENT=""               # x-b6e2: forwarded as --agent to the spawn verb
TOOLS=""               # x-b6e2: forwarded as --tools to the spawn verb
DENY_TOOLS=""          # x-b6e2: forwarded as --deny-tools to the spawn verb

# Dispatcher reservation state (Guard 2). Initialized up-front so fail() can
# reference it safely under set -u even before the reservation is acquired.
RES_KEY=""
RES_HOLDER="dispatch-skill:$$"
RES_HELD=0
release_reservation() {
  [[ "$RES_HELD" -eq 1 && -n "$RES_KEY" ]] \
    && fno claim release "$RES_KEY" --holder "$RES_HOLDER" >/dev/null 2>&1
  RES_HELD=0
  return 0
}

# fail() is only reached on non-launch paths, so always release any reservation
# we hold (keeps the node re-dispatchable). A successful launch never calls fail.
fail() { release_reservation; printf 'result=failed reason="%s"\n' "$1"; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name)         NAME="${2:-}"; shift 2 ;;
    # --harness is the fno-CLI name for this axis (the binary to launch);
    # --provider is the older spelling this script shipped with.
    --harness|--provider) PROVIDER="${2:-}"; shift 2 ;;
    --message)      MESSAGE="${2:-}"; shift 2 ;;
    --node)         NODE="${2:-}"; shift 2 ;;
    --self)         SELF="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --cwd)          CWD="${2:-}"; shift 2 ;;
    --mode)         MODE="${2:-}"; shift 2 ;;
    --model)        MODEL="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --effort)       EFFORT="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --payload-mode) PAYLOAD_MODE="${2:-}"; shift 2 ;;
    --substrate)    SUBSTRATE="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    -Y|--yolo)      YOLO=1; shift ;;
    --permission-mode) PERMISSION_MODE="${2:-}"; shift 2 ;;
    -r|--role)      ROLE="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    -t|--timeout)   TIMEOUT="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --add-dir)      ADD_DIR="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --agent)        AGENT="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --tools)        TOOLS="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    --deny-tools)   DENY_TOOLS="${2:-}"; [[ $# -ge 2 ]] && shift 2 || shift ;;
    # Pass-through cwd flags: forwarded verbatim to `fno agents spawn`. x-85fe
    # inverted the runtime default -- a spawn with NO cwd source now lands on
    # canonical, so this script defaults NOTHING and behavior follows the runtime:
    # --here keeps the caller cwd, --fresh is an accepted no-op alias. A code
    # payload still auto-isolates to a fresh worktree (maybe_auto_worktree sets
    # CWD, forwarded as an explicit --cwd that wins).
    --fresh)        FRESH=1; shift ;;
    --here|--in-place) HERE=1; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done

command -v fno >/dev/null 2>&1 || fail "fno not on PATH"
command -v jq  >/dev/null 2>&1 || fail "jq not on PATH"
[[ -n "$NAME" ]]     || fail "missing --name"
# x-de9d US8: --provider may be omitted when config.agents.defaults.provider is
# set; adopt the config default here so verb selection and the receipt check
# both have a concrete provider (the seam then sees it as an explicit flag with
# the same value). With neither flag nor config default, fail early with today's
# message (epic Open Question 4).
if [[ -z "$PROVIDER" ]]; then
  PROVIDER="$(fno config get agents.defaults.provider 2>/dev/null | tr -d '[:space:]' || true)"
  [[ -n "$PROVIDER" ]] || fail "missing --provider"
  # AC5-FR: a config-sourced provider must never be invisible. The seam's own
  # notice will not fire (it sees this as an explicit --provider), so echo it here.
  printf 'spawn.sh: provider from config.agents.defaults.provider=%s\n' "$PROVIDER" >&2
fi

# ---- Verb selection (Locked Decisions 1 + 2; Group 1 ab-8b3e4fe0) -------
# Creation is always spawn/host. claude routes to `spawn` (client-side `claude
# --bg --name`, subscription lane, JSON receipt); a codex/gemini build/seed
# routes to `spawn` (exec, autonomous) by default and `host` (interactive,
# human-driven) under `-i`. A one-shot exchange is the `headless` substrate
# (x-cbb0: subsumes the retired `ask` verb) -> `spawn --substrate headless`
# (reply on stdout). A codex/gemini passthrough never reaches here (normalize.sh
# refuses it); a claude passthrough runs the slash command via `spawn`.
if [[ "$PROVIDER" == "claude" ]]; then
  VERB="spawn"
elif [[ "$MODE" == "interactive" ]]; then
  VERB="host"
else
  VERB="spawn"
fi

# x-2c27: an explicit --substrate (bg|headless) always selects the spawn verb
# (never host). `headless` yields a one-shot reply receipt; `bg` (and pane/
# default) yield the JSON short-id receipt. REPLY drives the receipt-family
# branch below.
REPLY=0
if [[ -n "$SUBSTRATE" ]]; then
  VERB="spawn"
  [[ "$SUBSTRATE" == "headless" ]] && REPLY=1
fi

# A non-host launch needs a task; only a bare interactive host may be idle
# (AC2-EDGE). Reject an empty exec payload before any billed launch.
if [[ -z "$MESSAGE" && "$VERB" != "host" ]]; then
  fail "missing --message (only a host/interactive launch may have an empty task)"
fi

sanitize() { printf '%s' "$1" | tr '\n\r' '  ' | sed 's/"/'"'"'/g' | cut -c1-300; }

# ---- Guards 1+2 via the shared spawn-guard verb (x-73cc) -----------------
# The race-critical node:<id> claim probe (Guard 1) + create-only dispatch:<id>
# reservation (Guard 2) live in `fno agents spawn-guard` so this path and the
# /target bg path (skills/target/scripts/dispatch-node.sh) can never drift on
# the part that matters. Only a NODE dispatch is guarded; a free-text seed /
# handoff launch (no --node) skips the guard exactly as before. The verb does
# Guard 1 then Guard 2 in one process: a `dispatchable` verdict means it has
# acquired dispatch:<node> for $RES_HOLDER (released on every non-launch path
# via fail(); left to TTL-expire on a launch). Fail CLOSED: a stale `fno`
# without the verb (or any non-clean/unparseable verdict) refuses to launch.
if [[ -n "$NODE" ]]; then
  RES_KEY="dispatch:$NODE"
  # Pin to the Python runtime: spawn-guard is a Python-only verb, so an operator
  # with FNO_AGENTS_RUNTIME=rust exported would otherwise route it to the Rust
  # binary (which lacks it -> 127 -> fail-closed, breaking node dispatch). The
  # inline override is scoped to this call only; the real spawn below routes
  # normally (codex P2 parity with dispatch-node.sh).
  guard_out="$(FNO_AGENTS_RUNTIME=python fno agents spawn-guard "$NODE" --holder "$RES_HOLDER" --ttl 3m --json 2>/dev/null)"; guard_rc=$?
  guard_json="$(printf '%s\n' "$guard_out" | grep -F '"verdict"' | head -1)"
  verdict="$(printf '%s' "$guard_json" | jq -r '.verdict // empty' 2>/dev/null)"
  case "$verdict" in
    already-running)
      reason="$(printf '%s' "$guard_json" | jq -r '.reason // empty' 2>/dev/null)"
      if [[ "$reason" == "live-claim" ]]; then
        holder="$(printf '%s' "$guard_json" | jq -r '.holder // "unknown"' 2>/dev/null)"
        # Self-handoff: the live claim is the CALLER's own (holder == --self).
        # Distinguish it from a foreign collision, but do NOT spawn and do NOT
        # release the claim here. A node claim can be released ONLY by the two
        # sanctioned sites (handoff.sh / `fno backlog unclaim`, holder-verified);
        # a helper subprocess release is a locked-down authority violation
        # (ab-588326a7). And a bg spawn cannot emit the `delegated` event a clean
        # takeover needs (it does not control the successor's session id), so
        # merely proceeding would spawn a worker that is born contested while the
        # caller still holds a live claim. The honest move is to route the caller
        # to the sanctioned handoff rather than reassign from the wrong layer.
        if [[ -n "$SELF" && "$holder" == "$SELF" ]]; then
          printf 'result=self-handoff name=%s reason="you already hold node:%s (%s); /agent cannot reassign it from here. Hand off via /target self-handoff (archives state, emits the delegated event, releases the claim atomically), or run '"'"'fno backlog unclaim %s'"'"' then re-dispatch."\n' "$NAME" "$NODE" "$holder" "$NODE"
        else
          printf 'result=already-running name=%s reason="live worker holds node:%s (%s)"\n' "$NAME" "$NODE" "$holder"
        fi
      else
        case "$reason" in
          reservation-held|duplicate-claim)
            # x-a7ab 1.2 / x-b44e: a peer dispatcher won the visibility barrier
            # or already holds the dispatch:<id> reservation. Exactly one worker
            # launches; the loser's receipt carries skipped: duplicate-claim so a
            # loop/human sees the dedup, not a generic already-running.
            printf 'result=already-running name=%s reason="skipped: duplicate-claim (peer dispatcher holds %s)"\n' "$NAME" "$RES_KEY" ;;
          *)
            printf 'result=already-running name=%s reason="a peer dispatcher holds %s (racing launch)"\n' "$NAME" "$RES_KEY" ;;
        esac
      fi
      exit 0 ;;
    corrupted)
      fail "node:$NODE claim is corrupted; force-release or repair before dispatching" ;;
    dispatchable)
      RES_HELD=1 ;;
    *)
      # verdict=error, OR empty/unparseable (a stale fno WITHOUT the verb prints
      # Typer "No such command" + exits non-zero; or a probe crash): fail CLOSED.
      detail="$(printf '%s' "$guard_json" | jq -r '.detail // empty' 2>/dev/null)"
      fail "${detail:-spawn-guard unavailable (rc=$guard_rc); not dispatching to avoid a double-launch}" ;;
  esac
fi

# ---- Guard 3: the agents registry ---------------------------------------
# Any NON-terminal status means a worker is present - report already-running
# rather than `fno agents rm`-ing it (codex P2: ready/idle/busy are drive-
# eligible workers, not just `live`; removing one then respawning double-launches).
# Only a clearly-dead row is removed so `spawn` creates fresh instead of
# colliding with it. An unknown/unexpected status is treated as present (fail-safe).
# Capture the probe output AND its exit code separately so a crashed/truncated
# `fno agents list` (daemon down, jq parse error) fails CLOSED instead of
# collapsing to "no agent row" and double-launching. For a free-form / --name-only
# dispatch (no --node) this is the ONLY duplicate guard, so a swallowed probe
# error here is the highest-value gap (silent-failure-hunter on PR #433, cv-dddd8ae5).
# Mirrors the claim-probe fail-closed pattern above.
agents_json="$(fno agents list 2>/dev/null)"; list_rc=$?
if [[ "$list_rc" -ne 0 ]]; then
  fail "agents-list probe failed (rc=$list_rc); not dispatching to avoid a double-launch"
fi
if ! printf '%s' "$agents_json" | jq -e 'has("agents")' >/dev/null 2>&1; then
  fail "agents-list probe returned no parseable {agents:[...]}; not dispatching to avoid a double-launch"
fi
existing_status="$(printf '%s' "$agents_json" \
  | jq -r --arg n "$NAME" '.agents[]? | select(.name==$n) | .status' 2>/dev/null | head -1)"
case "$existing_status" in
  "")
    : ;;  # no agent row -> proceed to spawn
  exited|orphaned|dead|stopped|failed|terminated|killed)
    fno agents rm "$NAME" >/dev/null 2>&1 || true ;;  # dead -> clear, then spawn fresh
  *)
    # live|ready|idle|busy|spawning|restarting|<unknown> -> worker present.
    release_reservation
    printf 'result=already-running name=%s reason="an agent named %s exists (status=%s)"\n' "$NAME" "$NAME" "$existing_status"
    exit 0 ;;
esac

# ---- Auto-worktree for code-implementing payloads (x-9c4c) --------------
# A bg /target|/do|/fix launched into a repo's MAIN checkout lands on the
# canonical (often protected) branch and relies on the soft skill instruction
# "a bg /target self-creates its worktree before building." Do it
# deterministically here instead: create ~/conductor/workspaces/<repo>/<name>
# on a fresh feature branch and launch THERE, so the worker is born isolated
# (location verdict ok from line one) regardless of whether the cwd came from
# -P, a node's _resolved_cwd, or the caller sitting on canonical main.
#
# Scope: code payloads only -- a born-with-why /think dispatch writes a design
# doc, not commits, so it stays in repo root (failure mode 1). Fail-safe: any
# error keeps the original cwd so the prior self-worktree path still applies;
# the launch is NEVER blocked (failure mode 2). The <repo>/<name> path is
# deterministic per dispatch, so a re-spawn of the same node reuses it rather
# than colliding (failure mode 3).
AUTO_WT=""
# A payload writes code (and so wants isolation) when it is a node-id build
# dispatch OR an explicit /target|/do|/fix passthrough. Keying off PAYLOAD_MODE --
# not only the message prefix -- is load-bearing: an opencode/codex node build
# reaches here as `/fno:target ab-xxxx` / `$fno:target ab-xxxx`, not a literal
# /target, and those workers have NO location gate, so a prefix-only check would
# leave them editing a protected main checkout -- the exact harm this guards
# against. The passthrough arm matches the SAME per-harness namespacing: an
# explicit `/target`|`/do`|`/fix` reaches here verbatim on claude/agy but as
# `/fno:target` (opencode) / `$fno:target` (codex), so all three renderings must
# isolate. seed (verbatim conversational pane) and handoff (a doc continuation)
# and a non-code claude slash command (/think writes a design doc) are NOT code
# payloads.
is_code_payload() {
  case "$PAYLOAD_MODE" in
    build) return 0 ;;  # node-id dispatch: /target|/fno:target|$fno:target <id>
    passthrough)        # explicit slash command; isolate only code verbs
      case "$MESSAGE" in
        /target|/target\ *|/do|/do\ *|/fix|/fix\ *) return 0 ;;
        /fno:target|/fno:target\ *|/fno:do|/fno:do\ *|/fno:fix|/fno:fix\ *) return 0 ;;
        '$fno:target'|'$fno:target '*|'$fno:do'|'$fno:do '*|'$fno:fix'|'$fno:fix '*) return 0 ;;
        *) return 1 ;;
      esac ;;
    *) return 1 ;;      # seed | handoff
  esac
}
# A claude `/target <node>` worker isolates ITSELF at cold-start (`fno target
# start` -> `fno worktree ensure` -> the harness `EnterWorktree` tool), which
# moves the session's cwd while leaving its PROJECT at the launch dir. Pre-
# creating here instead binds the project to the worktree: claude keys
# ~/.claude/projects/ off the launch cwd with no rename hook, so every such spawn
# mints a throwaway project dir holding one transcript, orphaned once the
# worktree is reaped. Both paths reach the same `fno worktree ensure` and so
# honor the per-project policy identically; what differs is the project binding
# (and the branch name, spawn-derived vs `/target`'s own).
#
# Gated on claude because `EnterWorktree` is a Claude Code harness tool: a
# codex/opencode worker can run `fno target start` but cannot move its session
# into the result (a `cd` dies with the shell), so it still needs pre-creation --
# and its transcripts never land in ~/.claude/projects/ anyway. `/do` and `/fix`
# refuse on a protected branch rather than isolating, so they keep it everywhere.
#
# Both a NODE and a literal `/target` message are required, and neither is
# incidental. The cold-start that does the isolating is `fno target start
# <node>`, so a free-text `/target ship the thing` has nothing to resolve --
# unattended it hits the location-refusal backstop and aborts. And PAYLOAD_MODE
# defaults to `build`, so a caller that omits --payload-mode while passing a
# prose task would otherwise be assumed to run `/target` when it never will.
# Delegate only when the worker demonstrably receives a node-backed /target.
self_isolating_payload() {
  [[ "$PROVIDER" == "claude" ]] || return 1
  [[ -n "$NODE" ]] || return 1
  case "$PAYLOAD_MODE" in build|passthrough) ;; *) return 1 ;; esac
  case "$MESSAGE" in
    /target|/target\ *) return 0 ;;
    *) return 1 ;;
  esac
}
maybe_auto_worktree() {
  is_code_payload || return 0
  command -v git >/dev/null 2>&1 || return 0
  local base="${CWD:-$PWD}"
  [[ -d "$base" ]] || return 0
  local top
  top="$(git -C "$base" rev-parse --show-toplevel 2>/dev/null)" || return 0
  [[ -n "$top" ]] || return 0
  # Delegate to the worker's own cold-start. Launch at the repo root (not the
  # caller's cwd, which may be a subdir that would slug its own project dir).
  if self_isolating_payload; then
    CWD="$top"
    printf 'auto-worktree: delegated to the worker cold-start (launching at %s)\n' "$top" >&2
    return 0
  fi
  # The git/worktree mechanism (main-checkout-only gate, idempotent reuse,
  # stray-dir non-clobber, origin/main base, best-effort setup-worktree.sh)
  # lives in the `fno worktree ensure` verb (x-73ca) so all three code-dispatch
  # paths share ONE implementation. On any failure it prints nothing on stdout,
  # so $wt is empty and we launch in the original CWD -- isolation is best-effort
  # and never blocks the spawn. (NOTE: ensure bases the branch on origin/main,
  # which also retires the stale-base phantom-deletion bug here.)
  # $PROVIDER is already a harness kind - VALID_PROVIDERS (claude|codex|gemini|agy|
  # opencode, mirroring the Rust KNOWN_PROVIDERS) IS the harness set; glm/z.ai is a
  # model route layered on --harness claude, and ccm/ccr are claude account config
  # dirs, so neither reaches here as a bareword provider. Forward it directly as
  # --harness: ensure lands a claude payload harness-native at <repo>/.claude/
  # worktrees/, degrades any non-claude (or unexpected) harness to the external base.
  local wt
  wt="$(fno worktree ensure --repo "$top" --name "$NAME" --harness "$PROVIDER" 2>/dev/null)"
  if [[ -n "$wt" ]]; then
    CWD="$wt"; AUTO_WT="$wt"
    # policy=never launches in place: ensure prints the repo main checkout itself.
    # Skip every worktree-only side effect - setup-worktree.sh would link shared
    # state INTO the canonical checkout (Locked Decision 4: guard on path == root).
    # Compare PHYSICAL paths: ensure prints the resolved root while $top is the raw
    # show-toplevel, so a symlinked root (macOS /tmp -> /private/tmp) would defeat a
    # bare string match and run setup on canonical.
    local wt_phys top_phys
    wt_phys="$(cd "$wt" 2>/dev/null && pwd -P || printf '%s' "$wt")"
    top_phys="$(cd "$top" 2>/dev/null && pwd -P || printf '%s' "$top")"
    if [[ "$wt_phys" != "$top_phys" ]]; then
      # Link gitignored shared state (footnote-ecosystem only; absent -> skip).
      # This stays caller-side: the verb is package code and may not shell out to
      # a repo-root script (shellout-drift gate); a skill script may.
      local setup="$top/scripts/setup/setup-worktree.sh"
      [[ -f "$setup" ]] && CANONICAL="$top" WORKTREE="$wt" bash "$setup" >/dev/null 2>&1
      printf 'auto-worktree: %s\n' "$wt" >&2
    else
      AUTO_WT=""  # in place: no worktree was created, so nothing to reap later
      printf 'auto-worktree: policy=never, launching in place (%s)\n' "$wt" >&2
    fi
  fi
}
maybe_auto_worktree   # self-gating: no-op unless code payload + main checkout

# ---- Spawn (subscription lane only) -------------------------------------
# Run the GENUINE verb. claude `spawn` builds `claude --bg --name <name> <msg>`
# client-side (Group 1 ab-8b3e4fe0 moved the create off `ask`); codex/gemini
# `spawn`/`host` are daemon-managed PTY workers (Locked Decision 1) and
# `spawn --once` is the ephemeral one-shot. Name is POSITIONAL (Locked
# Decision 8). Never default to claude `-p`/`--bare` (x-2c27, amended from
# "never -p"): `pane`/`bg` use owned-PTY / `claude --bg`, never `-p`; `-p` is
# reachable only via the explicit `--substrate headless` verb (which the Rust
# client, not this script, translates to `claude -p`). --yolo is appended only
# when the user explicitly passed it (normalize.sh strips it for claude). A bare interactive
# host omits the message positional (a valid idle session). The cmd array
# always carries at least `agents <verb> --harness <h> --name <n>`, so
# "${cmd[@]}" is never an empty expansion (bash 3.2 set -u safe).
#
# stdout and stderr are captured SEPARATELY (via a temp file) so the receipt
# parse only ever sees stdout: a stderr warning (incl. the --once teardown
# receipt) can never be mistaken for a short-id, and the failure reason still
# carries the real stderr.
cmd=(agents "$VERB" --harness "$PROVIDER")
[[ -n "$CWD" ]] && cmd+=(--cwd "$CWD")
[[ "$FRESH" -eq 1 ]] && cmd+=(--fresh)
[[ "$HERE" -eq 1 ]] && cmd+=(--here)
[[ "$YOLO" -eq 1 ]] && cmd+=(--yolo)
[[ -n "$MODEL" ]] && cmd+=(--model "$MODEL")
[[ -n "$EFFORT" ]] && cmd+=(--effort "$EFFORT")
[[ -n "$PERMISSION_MODE" ]] && cmd+=(--permission-mode "$PERMISSION_MODE")
[[ -n "$ROLE" ]] && cmd+=(--role "$ROLE")
[[ -n "$TIMEOUT" ]] && cmd+=(--timeout "$TIMEOUT")
[[ -n "$ADD_DIR" ]] && cmd+=(--add-dir "$ADD_DIR")
[[ -n "$AGENT" ]] && cmd+=(--agent "$AGENT")
[[ -n "$TOOLS" ]] && cmd+=(--tools "$TOOLS")
[[ -n "$DENY_TOOLS" ]] && cmd+=(--deny-tools "$DENY_TOOLS")
# x-2c27: an explicit substrate emits --substrate (the canonical selector); the
# `headless` value carries the one-shot lane the retired `ask` verb used to.
[[ -n "$SUBSTRATE" ]] && cmd+=(--substrate "$SUBSTRATE")
# x-84a8: forward the node so a node-driven pane spawn exports FNO_NODE/SLUG/PLAN
# provenance (the verb resolves slug/plan from the graph). Ad-hoc spawns have no
# --node and export nothing new. Harmless on bg/headless (the verb ignores it).
[[ -n "$NODE" ]] && cmd+=(--node "$NODE")
cmd+=(--name "$NAME")
[[ -n "$MESSAGE" ]] && cmd+=("$MESSAGE")

err_file="$(mktemp 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}/agents-spawn-$$.err")"
# EXIT trap guarantees cleanup even if interrupted (SIGINT) mid-spawn; the manual
# rm below covers the normal path so the file is gone before any later use.
trap 'rm -f "$err_file"' EXIT
spawn_out="$(fno "${cmd[@]}" 2>"$err_file")"; spawn_rc=$?
spawn_err="$(cat "$err_file" 2>/dev/null)"; rm -f "$err_file"

if [[ "$spawn_rc" -ne 0 ]]; then
  fail "dispatch failed (rc=$spawn_rc): $(sanitize "${spawn_err:-$spawn_out}")"
fi

# ---- Honest receipt (the cardinal guard) --------------------------------
# Receipt family is keyed by the VERB/mode the skill ran (never by sniffing
# the output - Locked Decision 3):
#   --substrate headless -> a CLIENT-SIDE one-shot (`codex exec` / `gemini -p`,
#                          the lane the retired `ask` verb used, x-cbb0):
#                          stdout is the model REPLY verbatim, NOT a short-id
#                          (the teardown receipt rides stderr). Success = rc==0
#                          (checked above) AND a non-empty reply; the reply IS
#                          the deliverable. Empty reply -> FAILED, never faked.
#   spawn|host           -> stdout is JSON carrying {"short_id",...} (pretty
#                          multi-line from the daemon; one compact line from
#                          the client-side claude spawn). Parse `.short_id`
#                          with jq, validate whole-string 8-hex.
#                          Empty/missing/non-8-hex (even on exit 0) is FAILED.
if [[ "$REPLY" -eq 1 ]]; then
  # one-shot reply receipt (spawn --once / --substrate headless). Trim
  # whitespace for the empty check only; the full reply is relayed verbatim.
  reply_trimmed="${spawn_out//[$' \t\r\n']/}"
  if [[ -z "$reply_trimmed" ]]; then
    fail "empty reply (one-shot returned no content on exit 0): $(sanitize "${spawn_err:-(no stderr)}")"
  fi
  # The reply IS the deliverable (no lasting peer for a one-shot). Outcome line
  # first, then the full reply verbatim for the skill to preview.
  printf 'result=replied name=%s reply_chars=%s hint="fno agents logs %s"\n' \
    "$NAME" "${#spawn_out}" "$NAME"
  printf '%s\n' "$spawn_out"
  exit 0
else
  short_id="$(printf '%s' "$spawn_out" | jq -r '.short_id // empty' 2>/dev/null)"
  PANE_SESSION=""; PANE_ID=""   # set below only for a matched Python mux-pane receipt
  # Python-authored pane rows have no worker socket, so their genuine receipt
  # carries an empty short_id plus the addressable registry name and concrete
  # mux coordinates. Accept that handle only when every identity field matches
  # this launch; a partial/mismatched empty-id receipt still fails closed below.
  case "$SUBSTRATE" in
    ""|pane)
      if [[ -z "$short_id" ]]; then
        receipt_name="$(printf '%s' "$spawn_out" | jq -r '.name // empty' 2>/dev/null)"
        receipt_provider="$(printf '%s' "$spawn_out" | jq -r '.provider // empty' 2>/dev/null)"
        receipt_status="$(printf '%s' "$spawn_out" | jq -r '.status // empty' 2>/dev/null)"
        mux_session="$(printf '%s' "$spawn_out" | jq -r '.mux_session // empty' 2>/dev/null)"
        pane_id="$(printf '%s' "$spawn_out" | jq -r '.pane_id // empty' 2>/dev/null)"
        if [[ "$receipt_name" == "$NAME" && "$receipt_provider" == "$PROVIDER" \
           && "$receipt_status" == "live" && -n "$mux_session" && -n "$pane_id" ]]; then
          short_id="$receipt_name"
          # Remember the mux coordinates so the report points at `fno mux attach`:
          # a pane row has no log_path, so the `fno agents logs` hint misses.
          PANE_SESSION="$mux_session"; PANE_ID="$pane_id"
        fi
      fi
      ;;
  esac
  # WHOLE-string match (not `grep -qx`, which matches ANY line): a multi-line
  # `.short_id` value - e.g. `{"short_id":"junk\ndeadbeef"}` or a banner leaking
  # into the value - must NOT pass on one of its lines being valid. `[[ =~ ]]`
  # anchors `^...$` to the whole string, so any embedded newline or stray byte
  # fails (parity with the one-shot path's single-line requirement). bash 3.2 safe.
  #
  # The valid SHAPE depends on the substrate (x-61b7). Only `bg`/`headless` return
  # a real 8-hex session-id prefix (client-side `claude --bg` / one-shot). The
  # default/`pane` owned-PTY lane is addressed by an identifier-shaped registry
  # handle: Rust derives a non-empty name-slug short_id; Python supplies the
  # verified receipt name above because mux panes own no worker socket. The
  # 8-hex rule wrongly rejects both shapes, so accept a single-line identifier
  # there (empty/torn receipts still fail - the cardinal guard remains intact).
  case "$SUBSTRATE" in
    bg|headless) short_id_shape='^[0-9a-f]{8}$' ;;
    # 64, not 40: the pane handle is the derived agent name (<verb>-<node-id>-<slug>),
    # which normalize builds up to ~50 chars (verb + id + a 32-char slug). A 40-cap
    # rejected a real long-slug codex pane launch as FAILED (name 43 > 40).
    *)           short_id_shape='^[A-Za-z0-9_-]{1,64}$' ;;
  esac
  if [[ ! "$short_id" =~ $short_id_shape ]]; then
    fail "no valid short-id receipt ($VERB JSON .short_id empty/malformed for substrate '${SUBSTRATE:-pane}'): $(sanitize "${spawn_out:-$spawn_err}")"
  fi
fi

# ---- Report (mode-aware) ------------------------------------------------
# host is interactive: STAGED, not running yet - the user drives it later.
# Plain spawn may be autonomous work or a seeded interactive pane; report the
# payload intent rather than labeling every non-host launch as exec work.
if [[ "$VERB" == "host" ]]; then
  printf 'result=launched short_id=%s name=%s mode=interactive staged="not running yet" drive="fno agents grid %s"\n' \
    "$short_id" "$NAME" "$NAME"
else
  # Surface the auto-worktree cwd so an isolated launch is never silent. Quote
  # the value (like hint/trace): a path with spaces must not split the receipt's
  # space-separated key=value fields.
  wt_field=""; [[ -n "$AUTO_WT" ]] && wt_field=" cwd=\"$AUTO_WT\""
  report_mode="exec"
  [[ "$PAYLOAD_MODE" == "handoff" ]] && report_mode="spawn"
  [[ "$PAYLOAD_MODE" == "seed" ]] && report_mode="seed"
  if [[ -n "$PANE_SESSION" ]]; then
    # Pane worker: observe via mux, not `fno agents logs` (a pane row has no
    # log_path). Surface the coordinates and quote the ref (a session name may
    # contain a space); shell-quote the session in the hint so `fno mux attach
    # <session>` stays copy-pasteable.
    printf 'result=launched short_id=%s name=%s mode=%s%s pane="%s:%s" hint="fno mux attach %s"\n' \
      "$short_id" "$NAME" "$report_mode" "$wt_field" "$PANE_SESSION" "$PANE_ID" "$(printf '%q' "$PANE_SESSION")"
  else
    # Address the hint by short_id on bg/headless, where it is the session-id
    # prefix: registration can silently fail (the receipt only validates the
    # short_id's shape, never that a row landed), and a session-shaped token
    # heals from the harness store on a registry miss while a bare name never
    # can. `trace` stays name-based - it has no heal, so a short there would
    # trade one refusal for another. Other substrates keep the name: their
    # short_id is a name-slug, not session-shaped, so it gains nothing.
    hint_token="$NAME"
    case "$SUBSTRATE" in bg|headless) hint_token="$short_id" ;; esac
    printf 'result=launched short_id=%s name=%s mode=%s%s hint="fno agents logs %s" trace="fno agents trace %s"\n' \
      "$short_id" "$NAME" "$report_mode" "$wt_field" "$hint_token" "$NAME"
  fi
fi
exit 0
