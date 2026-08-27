#!/usr/bin/env bash
# x-d285 repro: route and account binding survive every Claude re-entry.
#
# Builds a scratch world with NO real credentials - a fake `claude` binary on
# PATH, an isolated account record, a route file carrying a sentinel secret -
# then walks the operator path end to end:
#
#   1. the additive fork SessionStart (both ids land on one row),
#   2. every re-entry resolution (attach, resume, recover) naming BOTH the
#      account config dir and the validated route settings,
#   3. explicit two-id selection on recover,
#   4. the selected fork id re-entering through the fake provider,
#   5. a sweep proving the sentinel secret appears NOWHERE outside the 0600
#      route file itself - not in stdout, stderr, the registry, or events.
#
# The pane/mux gesture doors are covered by the cargo matrix
# (`cargo test --manifest-path crates/fno/Cargo.toml reentry`); this script
# proves the operator-facing doors with the worktree's own binaries.
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
scratch="$(mktemp -d /tmp/fno-route-repro.XXXXXX)"
trap 'rm -rf "$scratch"' EXIT

PRIMARY="e6f78b98-e594-47ed-ad81-84f8a78b8bb7"
FORK="f00baa11-2222-4333-8444-555566667777"
SECRET="repro-route-secret-do-not-leak"

mkdir -p "$scratch/work" "$scratch/acct/.claude" "$scratch/.fno/agents" "$scratch/bin" "$scratch/out"
printf '{}' > "$scratch/acct/.claude/.credentials.json"

fail() {
  echo "repro-route-survival: FAIL: $*" >&2
  exit 1
}

# The fake provider: records its argv and the namespace it ran under.
cat > "$scratch/bin/claude" <<EOF
#!/usr/bin/env bash
printf 'call argv: %s\n' "\$*" >> "$scratch/out/claude-calls.log"
printf 'call CLAUDE_CONFIG_DIR: %s\n' "\${CLAUDE_CONFIG_DIR:-}" >> "$scratch/out/claude-calls.log"
exit 0
EOF
chmod +x "$scratch/bin/claude"

# The account resolver shellout targets `fno` on PATH; point it at THIS
# checkout's CLI (a deployed stale binary lacks --print-binding).
cat > "$scratch/bin/fno" <<EOF
#!/usr/bin/env bash
exec uv run --project "$root_dir/cli" fno-py "\$@"
EOF
chmod +x "$scratch/bin/fno"
export PATH="$scratch/bin:$PATH"

# One config pins both runtimes to the scratch home: state_dir (Python paths)
# and the account record; FNO_AGENTS_HOME pins the Rust registry home to the
# same file.
cat > "$scratch/.fno/config.toml" <<EOF
[config]
state_dir = "$scratch/.fno/"

[config.providers]
records = [
  { id = "makers", name = "Makers", harness = "claude", auth = "api_key", env = { ANTHROPIC_API_KEY = "repro-placeholder" }, config_dir = "$scratch/acct/.claude", priority = 10 },
]
EOF
export FNO_CONFIG="$scratch/.fno/config.toml"
export FNO_AGENTS_HOME="$scratch/.fno/agents"

# The recorded route: a live credential value inside a 0600-style artifact.
# The sentinel is what the leak sweep hunts.
cat > "$scratch/route.json" <<EOF
{"env": {"ANTHROPIC_BASE_URL": "https://zai.example/v1", "ANTHROPIC_AUTH_TOKEN": "$SECRET", "FNO_ROUTE_PROVIDER": "zai"}}
EOF

# The routed worker row exactly as the spawn seams write it.
cat > "$scratch/.fno/agents/registry.json" <<EOF
{"schema_version": 20, "agents": [{
  "name": "repro-router",
  "harness": "claude",
  "provider": "zai",
  "cwd": "$scratch/work",
  "log_path": null,
  "short_id": "e6f78b98",
  "harness_session_id": "$PRIMARY",
  "launch_account": "makers",
  "related_session_id": null,
  "route_settings_path": "$scratch/route.json",
  "status": "live",
  "created_at": "2026-08-27T00:00:00Z"
}]}
EOF

fno_agents() {
  cargo run --quiet --manifest-path "$root_dir/crates/fno-agents/Cargo.toml" \
    --bin fno-agents -- "$@"
}

printf '%s\n' 'repro 1/5: the additive fork SessionStart lands both ids on one row'
uv run --project "$root_dir/cli" python -m fno.agents.register_session \
  --harness claude --session-id "$FORK" --cwd "$scratch/work" \
  --agent-self repro-router > "$scratch/out/sessionstart.out" 2> "$scratch/out/sessionstart.err"
reg="$scratch/.fno/agents/registry.json"
grep -q "\"harness_session_id\": \"$PRIMARY\"" "$reg" || fail "the primary id was replaced"
grep -q "\"related_session_id\": \"$FORK\"" "$reg" || fail "the fork id did not land on the row"

printf '%s\n' 'repro 2/5: attach and resume resolutions carry account and route together'
fno_agents reentry-plan repro-router --transition attach > "$scratch/out/attach.json" 2> "$scratch/out/attach.err" \
  || fail "attach plan refused: $(cat "$scratch/out/attach.err")"
grep -q '"claude"' "$scratch/out/attach.json" || fail "attach plan has no provider invocation"
grep -q '"attach"' "$scratch/out/attach.json" || fail "attach plan is not an attach"
grep -q 'e6f78b98' "$scratch/out/attach.json" || fail "attach plan lost the transport key"
grep -q "$scratch/acct/.claude" "$scratch/out/attach.json" || fail "attach plan lost the account namespace"
grep -q "$scratch/route.json" "$scratch/out/attach.json" || fail "attach plan lost the route settings"

fno_agents reentry-plan repro-router --transition resume > "$scratch/out/resume.json" 2> "$scratch/out/resume.err" \
  || fail "resume plan refused: $(cat "$scratch/out/resume.err")"
grep -q '\-\-resume' "$scratch/out/resume.json" || fail "resume plan is not a resume"
grep -q "$PRIMARY" "$scratch/out/resume.json" || fail "resume plan lost the session id"
grep -q "$scratch/acct/.claude" "$scratch/out/resume.json" || fail "resume plan lost the account namespace"
grep -q "$scratch/route.json" "$scratch/out/resume.json" || fail "resume plan lost the route settings"

printf '%s\n' 'repro 3/5: recover refuses two ids until one is named'
if fno_agents recover repro-router > "$scratch/out/recover-bare.out" 2> "$scratch/out/recover-bare.err"; then
  fail "bare recover on a two-id row must refuse"
fi
grep -q "$PRIMARY" "$scratch/out/recover-bare.err" || fail "the refusal must name the primary id"
grep -q "$FORK" "$scratch/out/recover-bare.err" || fail "the refusal must name the fork id"

printf '%s\n' 'repro 4/5: either selected id recovers under the recorded binding'
for id in "$PRIMARY" "$FORK"; do
  fno_agents recover repro-router --session "$id" --print-command \
    > "$scratch/out/recover-print-$id.out" 2> "$scratch/out/recover-print-$id.err" \
    || fail "recover --print-command for $id refused: $(cat "$scratch/out/recover-print-$id.err")"
  grep -q -- "--resume $id" "$scratch/out/recover-print-$id.out" || fail "print form lost the selected id $id"
  grep -q "CLAUDE_CONFIG_DIR=$scratch/acct/.claude" "$scratch/out/recover-print-$id.out" \
    || fail "print form lost the account namespace for $id"
  grep -q -- "--settings $scratch/route.json" "$scratch/out/recover-print-$id.out" \
    || fail "print form lost the route settings for $id"
done

# The fork id re-enters through the fake provider: the exec path IS the
# operator's pane replacement for a row with no mux destination.
fno_agents recover repro-router --session "$FORK" \
  > "$scratch/out/recover-exec.out" 2> "$scratch/out/recover-exec.err" \
  || fail "recover exec failed: $(cat "$scratch/out/recover-exec.err")"
grep -q -- "--resume $FORK" "$scratch/out/claude-calls.log" || fail "the provider saw the wrong session id"
grep -q -- "--settings $scratch/route.json" "$scratch/out/claude-calls.log" || fail "the provider saw no route settings"
grep -q "CLAUDE_CONFIG_DIR: $scratch/acct/.claude" "$scratch/out/claude-calls.log" \
  || fail "the provider ran outside the recorded namespace"

printf '%s\n' 'repro 5/5: the sentinel secret stays inside the route file'
# Every artifact the run produced, except the route file itself, must be
# secret-free. An absence here is only trusted because steps 1-4 positively
# proved the machinery ran (each grep above was a positive marker).
leaks="$(grep -rl "$SECRET" "$scratch/out" "$scratch/.fno" 2>/dev/null | grep -v '^$' || true)"
if [[ -n "$leaks" ]]; then
  fail "the route secret leaked into: $leaks"
fi

printf '%s\n' 'repro-route-survival: PASS - both ids on one row, every argv carries account + route, either id recovers, no secret outside the route file'
