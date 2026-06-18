#!/bin/sh
# fno.sh - the no-prerequisite install channel for fno (ab-f49b54c1).
#
# Served at https://fno.sh and run as:
#
#     curl -fsSL fno.sh | sh
#
# The only thing the user must already have is curl (or wget) to fetch this
# script. uv provides Python, so the script provisions everything else:
#
#   1. ensure uv is present (chain to Astral's installer when absent, reuse an
#      existing uv when present, resolve uv at its known path despite the piped-
#      script PATH gap),
#   2. `uv tool install fno` (the published PyPI platform wheel: the Python CLI
#      plus the three fno-agents* binaries, bundled),
#   3. verify the installed package is THIS project's before declaring success,
#   4. print success + the version verified, and when uv's tool bin is not yet
#      on PATH, run `uv tool update-shell` to add it to the right shell profile
#      (idempotent; opt out with FNO_NO_MODIFY_PATH to get a manual hint only).
#
# This is the shell entry to the same bootstrap core the cargo channel
# (ab-4040eee8) uses via a Rust shim: ensure uv -> uv tool install fno ->
# verify-ours -> done. One mechanism, two entries.
#
# POSIX sh ONLY (runs unmodified under dash and macOS /bin/sh): no bashisms, no
# arrays, no `local`, no `set -o pipefail`. Fully non-interactive - it is its own
# stdin under `curl | sh`, so it can never `read` a prompt. Every configuration
# is an env var (FNO_VERSION, FNO_INSTALL_WHEEL, FNO_INSTALL_DIR,
# FNO_NO_MODIFY_PATH).
#
# Env knobs:
#   FNO_VERSION        pin an exact release (`FNO_VERSION=1.2.3` -> install fno==1.2.3)
#   FNO_INSTALL_WHEEL  install from a local wheel / any uv spec instead of by-name
#                      `fno` (used by the clean-machine smoke before the PyPI publish)
#   FNO_INSTALL_DIR    forwarded to uv as UV_TOOL_BIN_DIR for the tool-bin location
#   FNO_NO_MODIFY_PATH set to any non-empty value to skip the `uv tool update-shell`
#                      profile edit and print a manual PATH hint instead (for people
#                      who manage their own dotfiles)
set -eu

# --- output helpers --------------------------------------------------------
# Progress + info go to stderr so a `curl | sh` caller's stdout stays clean and
# every failure path is detectable (message on stderr, non-zero exit).
say()  { printf 'fno: %s\n' "$*" >&2; }
die()  { printf 'fno: %s\n' "$*" >&2; exit "${2:-1}"; }

have() { command -v "$1" >/dev/null 2>&1; }

# Strip ANSI CSI escapes (ESC[ ... <final @..~>) and surrounding whitespace from
# $1, echoing the result. The ESC byte is built with printf rather than written
# as `\033` in the sed expression: BSD sed (macOS) does NOT interpret `\033` as
# ESC, so a literal-escape pattern would silently fail to strip on a Mac.
strip_ansi() {
	_esc=$(printf '\033')
	printf '%s' "$1" | sed -e "s/${_esc}\[[0-9;]*[@-~]//g" -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

# Fetch $1 to stdout over HTTPS, preferring curl, falling back to wget. Used for
# the Astral uv installer (the outer one-liner is curl by definition, but the
# inner fetch degrades to wget so a curl-less-but-wget box still works).
FNO_FETCH_TO_STDOUT=
fetch_pipe_cmd() {
	if have curl; then
		FNO_FETCH_TO_STDOUT='curl -LsSf'
	elif have wget; then
		FNO_FETCH_TO_STDOUT='wget -qO-'
	else
		die "neither curl nor wget is available to fetch the uv installer; install one (or install uv from https://docs.astral.sh/uv/) and re-run."
	fi
}

# --- uv discovery + install ------------------------------------------------
# Locate a usable uv: prefer one on PATH, else the well-known dirs Astral's
# installer uses. Sets FNO_UV to `uv` (on PATH) or an absolute path; returns 1
# when none is found. Mirrors the cargo shim's find_uv.
FNO_UV=
find_uv() {
	if have uv; then
		FNO_UV=uv
		return 0
	fi
	for cand in \
		"${HOME:-}/.local/bin/uv" \
		"${XDG_BIN_HOME:-}/uv" \
		"${HOME:-}/.cargo/bin/uv"; do
		if [ -x "$cand" ]; then
			FNO_UV="$cand"
			return 0
		fi
	done
	return 1
}

# Ensure uv is available, chaining to Astral's standalone installer when absent.
# After install, re-resolve uv at its known path: Astral's installer edits shell
# profiles, which do NOT take effect inside this piped script, so a bare `uv`
# would 127 on the next step (AC2-EDGE). Logs which path was taken (AC2-UI).
ensure_uv() {
	if find_uv; then
		say "using existing uv ($FNO_UV)"
		return 0
	fi
	say "uv not found - installing the standalone uv via Astral (one time)..."
	fetch_pipe_cmd
	# Download the installer to a variable FIRST, then run it - never `fetch | sh`
	# directly: a POSIX pipeline's status is its LAST command (`sh`), which exits
	# 0 on empty stdin even when curl/wget failed (there is no pipefail to lean
	# on), so a piped form would mask a download failure and surface later as a
	# misleading `uv: command not found` (AC2-ERR). $FNO_FETCH_TO_STDOUT is an
	# intentional multi-word command (e.g. `curl -LsSf`), so it stays unquoted.
	# shellcheck disable=SC2086
	if ! _installer=$($FNO_FETCH_TO_STDOUT https://astral.sh/uv/install.sh) || [ -z "$_installer" ]; then
		die "could not download the uv installer (astral.sh unreachable or the downloader failed). Install uv from https://docs.astral.sh/uv/ and re-run."
	fi
	if ! printf '%s\n' "$_installer" | sh; then
		die "the uv installer failed. Install uv from https://docs.astral.sh/uv/ and re-run."
	fi
	if ! find_uv; then
		die "uv was installed but is not resolvable; add \$HOME/.local/bin to PATH and re-run, or install uv from https://docs.astral.sh/uv/."
	fi
	say "installed uv ($FNO_UV)"
}

# --- install source resolution ---------------------------------------------
# Choose the `uv tool install` source. FNO_INSTALL_WHEEL (a local wheel / any uv
# spec) wins so the channel is testable before the PyPI publish; otherwise the
# by-name PyPI package `fno`, optionally pinned by FNO_VERSION. Sets FNO_SOURCE.
FNO_SOURCE=
resolve_source() {
	if [ -n "${FNO_INSTALL_WHEEL:-}" ]; then
		FNO_SOURCE="$FNO_INSTALL_WHEEL"
		return 0
	fi
	# FNO_VERSION, when PROVIDED (even empty), must be a real version - a malformed
	# pin fails loudly rather than silently installing latest (AC5-EDGE).
	if [ "${FNO_VERSION+x}" = x ]; then
		case "$FNO_VERSION" in
			"")
				die "FNO_VERSION is set but empty; unset it to install the latest, or set an exact version like FNO_VERSION=1.2.3." ;;
			*[!0-9A-Za-z.+_-]*)
				die "FNO_VERSION='$FNO_VERSION' is not a valid version; use an exact version like FNO_VERSION=1.2.3." ;;
			[0-9]*)
				FNO_SOURCE="fno==$FNO_VERSION"
				return 0 ;;
			*)
				die "FNO_VERSION='$FNO_VERSION' must start with a digit (e.g. 1.2.3)." ;;
		esac
	fi
	FNO_SOURCE=fno
}

# --- resolution: the wheel fno + its tool venv python ----------------------
# Resolve the wheel fno console script + its venv python inside uv's tool dir:
# <uv tool dir>/fno/bin/{fno,python}. Sets FNO_REAL + FNO_VENV_PY; returns 1 when
# uv is absent or the tool dir is unreadable (the caller then provisions).
# uv colorizes `tool dir` on a TTY; we capture via a pipe and pass NO_COLOR, but
# strip any stray CSI escape defensively so it can never corrupt the path.
FNO_REAL=
FNO_VENV_PY=
FNO_TOOL_BIN=
resolve_real() {
	find_uv || return 1
	_dir=$(NO_COLOR=1 UV_NO_COLOR=1 "$FNO_UV" tool dir 2>/dev/null) || return 1
	_dir=$(strip_ansi "$_dir")
	[ -n "$_dir" ] || return 1
	FNO_REAL="$_dir/fno/bin/fno"
	FNO_VENV_PY="$_dir/fno/bin/python"
	# The tool-BIN dir (where uv exposes `fno` and the fno-agents* binaries for
	# PATH) is distinct from the venv bin above. Ask uv directly; fall back to the
	# configured / default location if this uv predates `tool dir --bin`.
	_bin=$(NO_COLOR=1 UV_NO_COLOR=1 "$FNO_UV" tool dir --bin 2>/dev/null) || _bin=
	FNO_TOOL_BIN=$(strip_ansi "$_bin")
	[ -n "$FNO_TOOL_BIN" ] || FNO_TOOL_BIN="${UV_TOOL_BIN_DIR:-${HOME:-}/.local/bin}"
	return 0
}

# --- identity verification (AC5): never report success for a foreign fno -----
# Predicate: returns 0 when the installed fno is THIS project's package, else 1
# with FNO_VERIFY_REASON set. Never exits, so callers decide the response: the
# idempotency check treats a non-ours / broken install as "(re)install needed",
# while the post-install path turns a failure into a hard abort (a foreign
# by-name PyPI package must never be reported as a successful install, AC5-ERR).
# Probes the tool venv's own python via importlib.metadata, keying on a signal
# the project owns (the author), not the bare binary name `fno` which a squatter
# could also publish. Mirrors the cargo shim's verify_ours / decide_identity.
FNO_VERIFIED_VERSION=
FNO_VERIFY_REASON=
verify_ours() {
	FNO_VERIFY_REASON=
	if [ ! -x "$FNO_VENV_PY" ]; then
		FNO_VERIFY_REASON="its tool venv python is missing"
		return 1
	fi
	# Fall back to Author-email when Author is absent: a PEP 621 author with an
	# email makes the build backend emit only `Author-email: Jason Noah Choi <...>`
	# and drop the bare Author field. The owner's name travels in both, so the
	# substring check still holds.
	_probe='import importlib.metadata as m
md = m.metadata("fno")
print(md["Name"])
print(md.get("Author") or md.get("Author-email") or "")
print(md["Version"])'
	if ! _out=$("$FNO_VENV_PY" -c "$_probe" 2>/dev/null); then
		FNO_VERIFY_REASON="no readable package metadata"
		return 1
	fi
	_name=$(printf '%s\n' "$_out" | sed -n '1p')
	_author=$(printf '%s\n' "$_out" | sed -n '2p')
	_version=$(printf '%s\n' "$_out" | sed -n '3p')
	# name must be `fno` (case-insensitive)...
	case "$_name" in
		fno|FNO|Fno) : ;;
		*) FNO_VERIFY_REASON="name=$_name"; return 1 ;;
	esac
	# ...AND authored by this project's owner.
	case "$_author" in
		*"Jason Noah Choi"*) : ;;
		*) FNO_VERIFY_REASON="author=$_author"; return 1 ;;
	esac
	FNO_VERIFIED_VERSION="$_version"
	return 0
}

# --- success report --------------------------------------------------------
# Report the verified version (AC5-UI) and, when uv's tool bin is not on PATH,
# make a later `fno`/`fno-agents` call resolvable rather than a bare 127 (AC3-UI).
report_success() {
	say "verified fno $FNO_VERIFIED_VERSION (this project's package)."
	# Check the DIRECTORY against PATH, not `have fno`: a pre-existing fno earlier
	# on PATH would otherwise suppress the fix, yet `fno` would run that other
	# binary instead of the one just verified here (codex P2).
	case ":${PATH:-}:" in
		*":$FNO_TOOL_BIN:"*)
			say "done. run 'fno --help' to get started."
			return 0
			;;
	esac
	# Not on PATH. Prefer to fix it the way uv itself does: `uv tool update-shell`
	# detects the shell (zsh/bash/fish), edits the right profile, and is
	# idempotent - no hand-rolled "which rc file / is it a login shell / is the
	# line already there" detection here, which is exactly the cross-shell
	# bug-farm uv maintains so we don't have to. FNO_NO_MODIFY_PATH opts out for
	# people who manage their own dotfiles; they get the manual export hint.
	if [ -n "${FNO_NO_MODIFY_PATH:-}" ]; then
		say "installed, but $FNO_TOOL_BIN is not on your PATH yet (FNO_NO_MODIFY_PATH set)."
		say "add it for this and future shells, e.g.:"
		say "    export PATH=\"$FNO_TOOL_BIN:\$PATH\""
		say "done. run 'fno --help' once $FNO_TOOL_BIN is on PATH."
		return 0
	fi
	# Degrade to the manual hint if this uv predates `tool update-shell` or the
	# profile edit fails for any reason - never leave the user with no PATH fix.
	if "$FNO_UV" tool update-shell >/dev/null 2>&1; then
		_shell_name=$(basename "${SHELL:-sh}")
		say "added $FNO_TOOL_BIN to your $_shell_name profile (via 'uv tool update-shell')."
		say "restart your shell, or run this to use fno now:"
		say "    export PATH=\"$FNO_TOOL_BIN:\$PATH\""
	else
		say "installed, but $FNO_TOOL_BIN is not on your PATH yet."
		say "add it for this and future shells, e.g.:"
		say "    export PATH=\"$FNO_TOOL_BIN:\$PATH\""
	fi
	say "done. run 'fno --help' to get started."
}

# --- main ------------------------------------------------------------------
main() {
	# footnote is not supported on native Windows yet (the agent runtime uses
	# POSIX flock + Unix-domain sockets; a named-pipe port is a separate spec).
	# Catch MSYS/MinGW/Cygwin shells early with a clear message instead of a
	# confusing `uv tool install fno` "no matching distribution" failure.
	case "$(uname -s 2>/dev/null)" in
		MINGW* | MSYS* | CYGWIN* | Windows_NT)
			die "footnote is not supported on native Windows yet. Please run under WSL2 (a Linux environment)."
			;;
	esac

	resolve_source

	# Honor FNO_INSTALL_DIR by handing it to uv as the tool-bin location.
	if [ -n "${FNO_INSTALL_DIR:-}" ]; then
		UV_TOOL_BIN_DIR="$FNO_INSTALL_DIR"
		export UV_TOOL_BIN_DIR
	fi

	# Idempotency: if fno is already provisioned (this run, or by pip/cargo) and
	# verifies as ours, no-op success - never a redundant heavy reinstall
	# (AC4-HP, AC4-EDGE). An explicit FNO_VERSION / FNO_INSTALL_WHEEL is a request
	# to (re)install that exact source, so it skips the no-op and provisions.
	if [ -z "${FNO_INSTALL_WHEEL:-}" ] && [ "${FNO_VERSION+x}" != x ]; then
		if resolve_real && [ -x "$FNO_REAL" ] && verify_ours; then
			say "fno is already installed and verified - nothing to do."
			report_success
			return 0
		fi
	fi

	ensure_uv

	# Progress line BEFORE the slow provision so the first run never looks like a
	# hang (AC1-UI). --force repairs a half-built / stale tool venv rather than
	# failing with "already installed" (AC4-FR); we only reach here when no usable
	# verified install was found, so --force never clobbers a healthy one.
	say "provisioning the fno CLI via uv (one time, may take a few seconds)..."
	if ! "$FNO_UV" tool install --force "$FNO_SOURCE"; then
		die "\`uv tool install $FNO_SOURCE\` failed (network/PyPI unreachable, disk full, or a bad version). Check the error above and retry, or run it manually."
	fi

	if ! resolve_real || [ ! -x "$FNO_REAL" ]; then
		die "provisioned the wheel but could not locate the installed fno; try 'uv tool install fno' manually."
	fi
	# A foreign by-name PyPI `fno`, or unreadable metadata, must abort here -
	# never report success for a package that is not ours (AC5-ERR).
	verify_ours || die "the installed fno is not this project's package ($FNO_VERIFY_REASON); refusing to report success for a foreign fno."
	report_success
}

main "$@"
