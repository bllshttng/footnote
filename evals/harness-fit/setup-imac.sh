#!/bin/bash
# Set up an Intel Mac to run the harness-fit study (Amendment 6).
#
#   git clone https://github.com/bllshttng/footnote && cd footnote
#   git checkout feature/x-272d-harness-fit-wave
#   bash evals/harness-fit/setup-imac.sh            # install and check
#   bash evals/harness-fit/setup-imac.sh --check    # check only, change nothing
#
# It installs Docker (OrbStack), Harbor, the harness CLIs at the versions the
# pilot used, fno with this branch's fno-agents, and the z.ai key. It is safe to
# run again. The run workspace is $HARNESS_FIT_WS (default ~/evals-workspace/harness-fit).
# Then start both runs with: bash evals/harness-fit/run/start.sh
set -euo pipefail

CHECK=0
[ "${1:-}" = "--check" ] && CHECK=1
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
WS="${HARNESS_FIT_WS:-$HOME/evals-workspace/harness-fit}"
ENV_FILE="$HOME/.fno/.env"
CLAUDE_VERSION=2.1.286
OPENCODE_VERSION=1.18.33
PI_VERSION=0.84.2
HARBOR_VERSION=0.23.0
fail=0

ok() { printf '  ok    %s\n' "$1"; }
bad() { printf '  FAIL  %s\n' "$1"; fail=1; }
run() { if [ "$CHECK" = 1 ]; then printf '  would %s\n' "$*"; else "$@"; fi; }
have() { command -v "$1" > /dev/null 2>&1; }

echo "== machine"
arch="$(uname -m)"
cores="$(sysctl -n hw.physicalcpu)"
mem_gib=$(( $(sysctl -n hw.memsize) / 1073741824 ))
if [ "$arch" = x86_64 ]; then ok "x86_64: task images run native"; else bad "arch is $arch; the study needs x86_64 (Amendment 6)"; fi
conc=$(( cores / 2 )); [ "$conc" -gt 4 ] && conc=4; [ "$conc" -lt 1 ] && conc=1
ok "$(sysctl -n hw.model), $cores physical cores, $mem_gib GiB; Run 0 runs $conc trials at once"

echo "== repo"
branch="$(git -C "$REPO" rev-parse --abbrev-ref HEAD)"
if [ "$branch" = feature/x-272d-harness-fit-wave ]; then ok "branch $branch"; else bad "on branch $branch; check out feature/x-272d-harness-fit-wave"; fi
if [ "$(git -C "$REPO" rev-parse --is-shallow-repository)" = true ]; then
  run git -C "$REPO" fetch --unshallow origin  # the replay bank checks out old merge commits
fi
ok "history is full (the replay bank needs old merge commits)"

echo "== tools"
have brew || { bad "Homebrew is missing: install it from https://brew.sh, then run this again"; exit 1; }
for f in git node uv; do
  if have "$f"; then ok "$f"; else run brew install "$f"; fi
done
export PATH="$HOME/.cargo/bin:$PATH"
if have cargo; then ok "cargo"; else run brew install rust; fi

echo "== docker"
if ! have docker; then run brew install --cask orbstack; fi
if have orb; then
  run orb config set cpu "$cores"
  run orb config set memory_mib $(( mem_gib * 1024 / 2 ))
  run orb start
fi
if docker info > /dev/null 2>&1; then ok "docker answers ($(docker info --format '{{.Architecture}}'))"; else bad "docker does not answer"; fi

echo "== harbor $HARBOR_VERSION"
if uvx --from "harbor==$HARBOR_VERSION" harbor --version > /dev/null 2>&1; then ok "harbor runs"; else bad "harbor does not run through uvx"; fi

echo "== harness CLIs (Run 1)"
want() { # want <binary> <npm package> <version>
  if have "$1" && "$1" --version 2>/dev/null | grep -q "$3"; then ok "$1 $3"; else run npm install -g "$2@$3"; fi
}
want claude @anthropic-ai/claude-code "$CLAUDE_VERSION"
want opencode opencode-ai "$OPENCODE_VERSION"
want pi @earendil-works/pi-coding-agent "$PI_VERSION"
[ -d /Applications/ZCode.app ] && ok "ZCode.app present" || echo "  note  no ZCode.app: the zcode arm and lane read unavailable"

echo "== the z.ai key"
if [ "$CHECK" = 0 ]; then mkdir -p "$HOME/.fno" && touch "$ENV_FILE" && chmod 600 "$ENV_FILE"; fi
if ! grep -qs '^ZAI_API_KEY=' "$ENV_FILE"; then
  if [ "$CHECK" = 1 ]; then bad "ZAI_API_KEY is not in $ENV_FILE"; else
    read -r -s -p "  z.ai API key (the fleet's coding-plan key): " key; echo
    printf 'ZAI_API_KEY=%s\n' "$key" >> "$ENV_FILE"
  fi
fi
if grep -qs '^ZAI_API_KEY=.' "$ENV_FILE"; then ok "ZAI_API_KEY in $ENV_FILE (0600)"; fi

echo "== fno and this branch's fno-agents (Run 1)"
have fno || run uv tool install fno
bin="$REPO/crates/fno-agents/target/release/fno-agents"
[ -x "$bin" ] || run cargo build --release --manifest-path "$REPO/crates/fno-agents/Cargo.toml"
if [ "$CHECK" = 0 ] && ! grep -q '^FNO_AGENTS_BIN=' "$ENV_FILE"; then printf 'FNO_AGENTS_BIN=%s\n' "$bin" >> "$ENV_FILE"; fi
if [ -x "$bin" ]; then ok "fno-agents built at $bin"; else bad "fno-agents is not built"; fi

echo "== run workspace $WS"
run mkdir -p "$WS/logs" "$WS/runs" "$WS/pi-agent"
if [ "$CHECK" = 0 ]; then
  chmod 700 "$WS"
  # Run 1 config: the machine's own fno config plus the study lanes, read as FNO_CONFIG.
  base="$HOME/.fno/config.toml"
  { [ -f "$base" ] && cat "$base"; cat "$HERE/run/run1-lanes.toml"
    grep -qs 'id = "zai"' "$base" || printf '\n[[accounts.records]]\nid = "zai"\nname = "zai"\nharness = "claude"\nauth = "api_key"\npriority = 100\nroute = "zai/glm-5.3-flash[1m]"\naccount_id = "zai"\n'
  } > "$WS/run1-config.toml"
  chmod 600 "$WS/run1-config.toml"
  # pi reads its z.ai provider from PI_CODING_AGENT_DIR; the key stays an env reference.
  cat > "$WS/pi-agent/models.json" <<'JSON'
{"providers": {"zai-glm": {"baseUrl": "https://api.z.ai/api/coding/paas/v4", "api": "openai-completions",
  "apiKey": "$ZAI_API_KEY", "models": [{"id": "glm-5.3-flash", "name": "GLM-5.3-Flash", "reasoning": true,
  "input": ["text"], "contextWindow": 200000, "maxTokens": 32000,
  "cost": {"input": 0.15, "output": 0.5, "cacheRead": 0.03, "cacheWrite": 0}}]}}}
JSON
  [ -f "$WS/pi-agent/auth.json" ] || echo '{}' > "$WS/pi-agent/auth.json"
  # opencode: the zai-coding-plan key and effort high for glm-5.3-flash.
  ZAI_API_KEY="$(sed -n 's/^ZAI_API_KEY=//p' "$ENV_FILE")" python3 - <<'PY'
import json, os
from pathlib import Path
def merge(path, update):
    p = Path(path).expanduser(); p.parent.mkdir(parents=True, exist_ok=True)
    d = json.loads(p.read_text()) if p.exists() else {}
    update(d); p.write_text(json.dumps(d, indent=2)); p.chmod(0o600)
merge("~/.local/share/opencode/auth.json",
      lambda d: d.__setitem__("zai-coding-plan", {"type": "api", "key": os.environ["ZAI_API_KEY"]}))
merge("~/.local/state/opencode/model.json",
      lambda d: d.setdefault("variant", {}).__setitem__("zai-coding-plan/glm-5.3-flash", "high"))
PY
fi
if [ -f "$WS/run1-config.toml" ]; then ok "run1-config.toml"; else bad "run1-config.toml is missing"; fi

echo
if [ "$fail" = 0 ]; then
  echo "Ready. Start both runs:  bash $REPO/evals/harness-fit/run/start.sh"
  echo "Watch them:              python3 $REPO/evals/harness-fit/run/status.py"
else
  echo "Not ready: fix each FAIL above, then run this again."
fi
exit "$fail"
