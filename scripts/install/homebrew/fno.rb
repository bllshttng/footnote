# Homebrew formula for the `fno` CLI - the brew install channel (ab-d59d219a).
#
# This is the canonical source of the formula. It is copied verbatim into the
# own tap repo (github.com/<owner>/homebrew-fno) so `brew install <owner>/fno/fno`
# resolves it. Keeping the source here lets CI parse + audit it and lets the
# brew clean-machine smoke (cli/tests/smoke/brew_formula_smoke.sh) prove the
# install + symlink mechanism against a freshly built local wheel.
#
# The "py-wheel flywheel" (shared with the cargo + fno.sh channels): `fno` is the
# Python Typer CLI and only `fno-agents{,-daemon,-worker}` are Rust, so the
# formula installs the published PyPI *platform wheel* (which already bundles all
# three binaries as wheel `shared_scripts`) into a brew-managed venv. brew
# provides Python (depends_on "python@3.13"); the wheel is the single artifact
# source. brew owns the venv + symlinks, so `brew uninstall`/`brew upgrade` are
# clean (Locked Decisions 2 + 3).
#
# LAUNCH GATE (do not ship the tap copy until both are done):
#   1. The real `fno` package is published to PyPI (over the reserved 0.0.0
#      placeholder) - the shared gate with cargo + fno.sh.
#   2. `url` + `sha256` below are filled with the published per-arch wheel URLs
#      and hashes (kept in lockstep on every release bump).
# Until then the placeholders below are intentional - the brew smoke exercises a
# concrete local-wheel formula with the SAME install mechanism (see AC4/AC5).
#
# Deps: an own-tap formula installs with network, so `pip install` resolves the
# Python deps from PyPI at install time. (Vendoring deps as offline `resource`
# blocks via `brew update-python-resources fno` is a future hardening only a
# homebrew-core submission would require - out of scope for the own tap.)
class Fno < Formula
  desc "Autonomous delivery pipeline CLI (footnote)"
  homepage "https://github.com/bllshttng/footnote"
  license "Apache-2.0"

  # Only macOS wheels are declared below; declaring the requirement keeps a
  # Linuxbrew user from a confusing "no url" error (Linux rides a future wheel).
  depends_on :macos
  depends_on "python@3.13"

  # Per-arch platform wheels: the wheel carries native Rust binaries, so the URL
  # must match the host arch. The release publishes both macosx arm64 + x86_64
  # wheels; brew selects per arch. url + sha256 are filled at first release (see
  # LAUNCH GATE above); the 64-zero sha256 is a deliberate placeholder.
  #
  # `using: :nounzip` keeps the wheel a FILE: a .whl is a zip, and an unpacked
  # wheel dir is not pip-installable (no build backend), so the install step
  # below pip-installs the wheel file directly rather than the unpacked tree.
  on_macos do
    on_arm do
      url "https://files.pythonhosted.org/packages/source/f/fno/fno-VERSION-py3-none-macosx_11_0_arm64.whl", using: :nounzip
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://files.pythonhosted.org/packages/source/f/fno/fno-VERSION-py3-none-macosx_10_12_x86_64.whl", using: :nounzip
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    # Build the venv from the python@3.13 dependency (never the host python,
    # which may be older on a clean machine).
    system Formula["python@3.13"].opt_bin/"python3.13", "-m", "venv", libexec

    # The wheel is a FILE in buildpath (url's :nounzip - an unpacked wheel dir is
    # not pip-installable), so pip-install the wheel file directly. pip resolves
    # the Python deps from PyPI (own-tap network). This is the mechanism the brew
    # smoke exercises end to end.
    system libexec/"bin/pip", "install", "--disable-pip-version-check", Dir["*.whl"].first

    # The `fno` console_script plus the three Rust binaries (which ride in the
    # wheel as `shared_scripts`) all land in the venv bin; pip links none of them
    # into the keg bin, so symlink them explicitly. The fno-agents* symlink is
    # the load-bearing step (Locked Decision 4): the CLI invokes the binaries by
    # name on PATH. Arch-agnostic via libexec.
    bin.install_symlink libexec/"bin/fno"
    bin.install_symlink Dir[libexec/"bin/fno-agents*"]
  end

  test do
    # The CLI runs from the keg bin.
    assert_match "fno", shell_output("#{bin}/fno --version")

    # All three binaries must be present + executable on the keg bin, or a
    # daemon/loop verb would 127 at runtime. Fail the test on any miss
    # (no silent success for a half-installed CLI).
    %w[fno-agents fno-agents-daemon fno-agents-worker].each do |b|
      assert_predicate bin/b, :executable?, "#{b} missing from the keg bin"
    end
  end
end
