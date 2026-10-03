#!/usr/bin/env bash
# tests/smoke/test_postinstall_clobber_guard.sh
#
# Behavior test for the x-7b2e clobber guard in .claude-plugin/postinstall.sh:
# with a FAKE `uv` on PATH whose tool env carries a receipt naming a foreign
# source, the installer must refuse (exit 3) before any install step, naming
# the foreign source and the FNO_INSTALL_REPLACE override. The refusal fires
# before uv's install arms, so no install ever runs. A second case puts a live
# process beside the fake env and expects the live-process refusal.
#
# PATH is $FAKEBIN:/usr/bin:/bin: no real fno (the idempotent-complete skip
# must not take), no real pip (the fallback must hit the stub, never install).
# Run: bash cli/tests/smoke/test_postinstall_clobber_guard.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
POSTINSTALL="$REPO_ROOT/.claude-plugin/postinstall.sh"

fails=0
die() { echo "FAIL: $*" >&2; fails=$((fails + 1)); }

WORK="$(mktemp -d -t fno-clobber-XXXXXX)"
trap 'rm -rf "$WORK"; [[ -n "${DECOY:-}" ]] && kill "$DECOY" 2>/dev/null' EXIT

FAKEBIN="$WORK/bin"
mkdir -p "$FAKEBIN"
TOOLDIR="$WORK/tools"
ENVDIR="$TOOLDIR/fno"
mkdir -p "$ENVDIR/lib/python3.11/site-packages/fno-0.4.0.dist-info"

# fake uv: answers tool dir/list with the fake env, refuses every install call.
cat >"$FAKEBIN/uv" <<FAKE
#!/usr/bin/env bash
case "\$1 \$2" in
  "tool dir") echo "$TOOLDIR" ;;
  "tool list") echo "fno 0.4.0" ;;
  *) echo "fake uv: install refused by test stub" >&2; exit 42 ;;
esac
FAKE
chmod +x "$FAKEBIN/uv"
for pip in pip pip3; do
  printf '#!/usr/bin/env bash\necho "fake pip: refused by test stub" >&2\nexit 42\n' >"$FAKEBIN/$pip"
  chmod +x "$FAKEBIN/$pip"
done
TESTPATH="$FAKEBIN:/usr/bin:/bin"

# --- Case 1: foreign receipt -> refuse exit 3, name the source and the escape.
# The receipt names a source different from this tree's cli/.
printf '{"dir": "/somewhere-else/cli", "url": "file:///somewhere-else/cli/"}\n' \
  >"$ENVDIR/lib/python3.11/site-packages/fno-0.4.0.dist-info/direct_url.json"

out="$(PATH="$TESTPATH" bash "$POSTINSTALL" 2>&1)"
rc=$?
grep -q "refusing to reinstall" <<<"$out" || die "guard must refuse, got: $out"
grep -q "/somewhere-else/cli" <<<"$out" || die "refusal must name the foreign source, got: $out"
grep -q "FNO_INSTALL_REPLACE=1" <<<"$out" || die "refusal must name the override, got: $out"
[[ "$rc" == 3 ]] || die "foreign-receipt refusal must exit 3, got rc=$rc"
echo "PASS foreign receipt -> exit 3 with named source and override"

# --- Case 2: no receipt (a registry install) and no live process -> the guard
# passes and the script reaches the install arms, which the stub refuses (42).
rm "$ENVDIR/lib/python3.11/site-packages/fno-0.4.0.dist-info/direct_url.json"
out="$(PATH="$TESTPATH" bash "$POSTINSTALL" 2>&1)"
rc=$?
grep -q "fake uv: install refused by test stub" <<<"$out" || die "guard must pass a receipt-less env and reach the install arms, got: $out"
[[ "$rc" == 1 ]] || die "stub-refused install must fall through to exit 1, got rc=$rc"
echo "PASS receipt-less env -> guard passes, install arms reached"

# --- Case 3: a live process beside the env -> refuse, name it.
cat >"$ENVDIR/fno-py" <<'FAKE'
#!/usr/bin/env bash
sleep 30
FAKE
chmod +x "$ENVDIR/fno-py"
"$ENVDIR/fno-py" >/dev/null 2>&1 &
DECOY=$!
sleep 0.5
out="$(PATH="$TESTPATH" bash "$POSTINSTALL" 2>&1)"
rc=$?
kill "$DECOY" 2>/dev/null
wait "$DECOY" 2>/dev/null
DECOY=""
grep -q "running from the existing fno tool env" <<<"$out" || die "live process must refuse the reinstall, got: $out"
grep -q "$ENVDIR/fno-py" <<<"$out" || die "refusal must name the live process line, got: $out"
[[ "$rc" == 3 ]] || die "live-process refusal must exit 3, got rc=$rc"
echo "PASS live process beside the env -> exit 3 naming the process"

echo "fails=$fails"
exit "$fails"
