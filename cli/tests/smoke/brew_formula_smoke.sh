#!/usr/bin/env bash
# ab-d59d219a US5: clean-machine smoke for the `brew install fno` channel.
#
# Two tiers, both run against the REPO copy of the formula
# (scripts/install/homebrew/fno.rb), which is byte-identical to the tap copy:
#
#   STRUCTURAL (always, no wheel/brew install needed):
#     1. `ruby -c` parses the committed formula.
#     2. The launch-critical elements are present: Language::Python::Virtualenv,
#        depends_on python, the EXPLICIT `bin.install_symlink ... fno-agents*`
#        (the shared_scripts symlink, Locked Decision 4), the three-binary
#        `test do` asserts, and the license.
#     3. `brew style` passes modulo the tap-context-only Sorbet/frozen-string
#        cops (a loose .rb outside a tap trips those; a real tap formula does
#        not carry Sorbet sigils). Full `brew audit --strict --new fno` needs the
#        published PyPI url + generated resources - an operator LAUNCH step.
#
#   INSTALL (only with a binary-complete wheel + brew, gated on CI or
#   FNO_BREW_SMOKE_INSTALL=1 so a local run never mutates the dev's brew):
#     4. A concrete local-wheel formula (url=file://<wheel>, REAL sha256, the
#        same symlink + test logic) installs via `brew install` (AC1-HP/AC5-HP).
#     5. All three fno-agents* binaries resolve under the keg bin (US2/AC2-HP):
#        proof the explicit symlink surfaces the wheel's shared_scripts.
#     6. `brew test fno` passes (AC1-UI/AC5-UI) and `fno --version` runs (no 127).
#     7. `brew uninstall fno` removes the keg cleanly (US3/AC3-UI): `fno` no
#        longer resolves to the brew copy, no orphaned keg left linked.
#
# Per-check pass/fail; exits non-zero on any miss (AC5-ERR/UI) so a broken
# formula fails the release rather than going silently green.
#
# Usage: brew_formula_smoke.sh <path-to-binary-complete-wheel>
#   The wheel is required for the INSTALL tier; the STRUCTURAL tier ignores it.
#   The by-name `brew install <owner>/fno/fno` + the full `--new` audit go green
#   only AFTER the PyPI publish + tap creation (foundation launch #jc); this
#   smoke proves the formula + the symlink mechanism now.
set -uo pipefail

WHEEL="${1:-}"

# Repo root from this script's location (cli/tests/smoke/ -> repo root).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
FORMULA="$REPO_ROOT/scripts/install/homebrew/fno.rb"
[ -f "$FORMULA" ] || { echo "FAIL[input] formula not found at $FORMULA"; exit 1; }

fail=0
pass() { printf 'PASS[%s] %s\n' "$1" "$2"; }
miss() { printf 'FAIL[%s] %s\n' "$1" "$2"; fail=1; }
run_capture() { OUT="$("$@" 2>&1)"; RC=$?; return "$RC"; }

# ---------------------------------------------------------------------------
# STRUCTURAL tier (always)
# ---------------------------------------------------------------------------

# --- check 1: the committed formula parses ---
if run_capture ruby -c "$FORMULA"; then
  pass "parse" "ruby -c: formula is valid Ruby"
else
  miss "parse" "ruby -c failed: $(printf '%s' "$OUT" | tail -1)"
fi

# --- check 2: launch-critical elements present ---
need_grep() { # <label> <regex> <human>
  if grep -Eq "$2" "$FORMULA"; then pass "$1" "$3"; else miss "$1" "missing: $3"; fi
}
need_grep "venv"        '"-m", "venv", libexec' "builds the venv from the python@3.13 dependency"
need_grep "python-dep"  'depends_on +"python@3\.' "declares depends_on python@3.x (brew provides Python)"
need_grep "nounzip"     'using: :nounzip' "keeps the wheel a file (url :nounzip)"
need_grep "wheel-file"  'pip".*install.*Dir\["\*\.whl"\]' \
  "pip-installs the wheel FILE, not virtualenv_install_with_resources' unpacked dir"
need_grep "symlink"     'bin\.install_symlink +Dir\[libexec/"bin/fno-agents\*"\]' \
  "explicitly symlinks the shared_scripts binaries (Locked Decision 4)"
need_grep "test-binaries" 'fno-agents.+fno-agents-daemon.+fno-agents-worker' \
  "test block asserts all three binaries"
need_grep "license"     'license +"Apache-2\.0"' "declares the license"

# --- check 3: brew style (ignore tap-context-only Sorbet/frozen/PyPiUrls cops) ---
if command -v brew >/dev/null 2>&1; then
  STYLE="$(brew style "$FORMULA" 2>&1 || true)"
  # Drop the cops a loose-file-outside-a-tap trips but a real tap formula never
  # needs: Sorbet sigils, frozen-string-literal, and the PyPiUrls cop crash on
  # the placeholder url. Anything else (ordering, layout, lint) is a real miss.
  REAL="$(printf '%s' "$STYLE" | grep -E ': [CWE]: ' \
            | grep -Ev 'Sorbet/|FrozenStringLiteralComment|PyPiUrls' || true)"
  if [ -z "$REAL" ]; then
    pass "style" "no structural style offenses (Sorbet/frozen/PyPiUrls are tap-context noise)"
  else
    miss "style" "structural style offenses: $(printf '%s' "$REAL" | head -2)"
  fi
else
  pass "style" "brew absent - skipped (structural style is a brew-host check)"
fi

# ---------------------------------------------------------------------------
# INSTALL tier (binary-complete wheel + brew, CI or opt-in only)
# ---------------------------------------------------------------------------

install_gate() { [ "${CI:-}" = "true" ] || [ "${FNO_BREW_SMOKE_INSTALL:-}" = "1" ]; }

if ! install_gate; then
  echo "--- INSTALL tier skipped (set FNO_BREW_SMOKE_INSTALL=1 or run under CI; it mutates brew) ---"
elif ! command -v brew >/dev/null 2>&1; then
  echo "--- INSTALL tier skipped: brew not on PATH ---"
elif brew list --formula fno >/dev/null 2>&1; then
  # `fno` IS the tool this channel installs. The INSTALL tier owns the bare `fno`
  # keg (install + cleanup uninstall), so refuse to run if a real `fno` is
  # already installed - never clobber a developer's actual install. A clean host
  # (incl. CI runners) has nothing here, so the tier runs. Reaching the `else`
  # below thus guarantees no pre-existing `fno`, making the trap's uninstall safe.
  echo "--- INSTALL tier skipped: an 'fno' keg is already installed; refusing to mutate the real install ---"
elif [ -z "$WHEEL" ] || [ ! -f "$WHEEL" ]; then
  miss "install-input" "INSTALL tier gated ON but no binary-complete wheel given (arg 1): '${WHEEL:-}'"
else
  WHEEL="$(cd "$(dirname "$WHEEL")" && pwd)/$(basename "$WHEEL")"
  SHA="$(shasum -a 256 "$WHEEL" | awk '{print $1}')"

  # Modern brew only installs formulae from a tap, so stage the concrete
  # local-wheel formula into a throwaway tap (this also exercises the real
  # `brew install <owner>/fno/fno` tap path the launched channel uses).
  TAP="local/fnosmoke"
  brew untap "$TAP" >/dev/null 2>&1 || true
  if ! run_capture brew tap-new --no-git "$TAP"; then
    miss "install-setup" "brew tap-new failed: $(printf '%s' "$OUT" | tail -2)"
  fi
  TAP_DIR="$(brew --repository "$TAP" 2>/dev/null)"
  trap 'brew uninstall --force fno >/dev/null 2>&1 || true; brew untap '"$TAP"' >/dev/null 2>&1 || true' EXIT
  mkdir -p "$TAP_DIR/Formula"
  SMOKE_FORMULA="$TAP_DIR/Formula/fno.rb"

  # Concrete local-wheel formula: real file:// url + REAL sha256 (so brew's
  # fetch + hash verify runs - the AC1-ERR path). It uses the SAME install
  # MECHANISM as the committed formula: url's :nounzip keeps the wheel a file, a
  # venv is built from the python@3.13 dep, and pip installs the wheel FILE from
  # buildpath (Dir["*.whl"]) - so this smoke actually exercises the launched path
  # (deps resolve from PyPI via the own-tap network, same as the committed one).
  cat > "$SMOKE_FORMULA" <<RUBY
class Fno < Formula
  desc "Autonomous delivery pipeline CLI (footnote) - smoke"
  homepage "https://github.com/bllshttng/footnote"
  url "file://$WHEEL", using: :nounzip
  sha256 "$SHA"
  license "Apache-2.0"

  depends_on :macos
  depends_on "python@3.13"

  def install
    system Formula["python@3.13"].opt_bin/"python3.13", "-m", "venv", libexec
    system libexec/"bin/pip", "install", "--disable-pip-version-check", Dir["*.whl"].first
    bin.install_symlink libexec/"bin/fno-py"
    bin.install_symlink Dir[libexec/"bin/fno-agents*"]
  end

  test do
    assert_match "fno", shell_output("#{bin}/fno-py --version")
    %w[fno-agents fno-agents-daemon fno-agents-worker].each do |b|
      assert_predicate bin/b, :executable?, "#{b} missing from the keg bin"
    end
  end
end
RUBY

  # --- check 4: install (no pre-existing fno - guaranteed by the guard above) ---
  run_capture brew install --build-from-source "$TAP/fno"
  if [ "$RC" -eq 0 ]; then
    pass "install" "brew install of the local-wheel formula succeeded (rc=0)"
  else
    miss "install" "rc=$RC out: $(printf '%s' "$OUT" | tail -4)"
  fi

  KEGBIN="$(brew --prefix 2>/dev/null)/bin"

  # --- check 5: all three binaries resolve under the keg bin ---
  for b in fno-agents fno-agents-daemon fno-agents-worker; do
    if [ -x "$KEGBIN/$b" ]; then pass "binary:$b" "present + executable on the keg bin"
    else miss "binary:$b" "absent (the explicit symlink must surface the wheel's shared_scripts)"; fi
  done

  # --- check 6: fno-py --version runs (no 127) + brew test passes ---
  # The keg bin carries `fno-py` (the Python CLI console script); the `fno` mux
  # front door is a separate binary, not shipped by the brew/py-wheel channel yet.
  if [ -x "$KEGBIN/fno-py" ]; then
    run_capture "$KEGBIN/fno-py" --version
    if [ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -qi 'fno'; then
      pass "version" "fno-py --version runs from the keg bin (rc=0)"
    else
      miss "version" "rc=$RC out: $(printf '%s' "$OUT" | tail -1)"
    fi
  else
    miss "version" "fno-py not on the keg bin at $KEGBIN/fno-py"
  fi
  run_capture brew test "$TAP/fno"
  if [ "$RC" -eq 0 ]; then
    pass "brew-test" "brew test fno passed (version + three binaries)"
  else
    miss "brew-test" "rc=$RC out: $(printf '%s' "$OUT" | tail -4)"
  fi

  # --- check 7: uninstall is clean (US3/AC3-UI) ---
  run_capture brew uninstall --force fno
  # -e follows symlinks (a broken/orphaned symlink reads as absent), so also
  # check -L to catch a symlink brew left behind pointing at a removed keg.
  if [ "$RC" -eq 0 ] \
     && [ ! -e "$KEGBIN/fno-py" ] && [ ! -L "$KEGBIN/fno-py" ] \
     && [ ! -e "$KEGBIN/fno-agents" ] && [ ! -L "$KEGBIN/fno-agents" ]; then
    pass "uninstall" "brew uninstall removed the keg + symlinks cleanly"
  else
    miss "uninstall" "rc=$RC; keg bin still has fno/fno-agents after uninstall"
  fi
fi

echo "---"
if [ "$fail" -ne 0 ]; then echo "brew smoke: FAILED"; exit 1; fi
echo "brew smoke: all checks passed"
