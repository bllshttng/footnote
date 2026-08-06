"""Verb-surface ratchet tests - `fno lint verb-ratchet`.

Covers the ratchet directions (AC2), the both-binaries invariant (AC3), the
fail-closed Rust-front reach (AC4), and the conflict-message shape (AC9), plus
the usage-string parser and the advertised-subset guard that catches a Rust
addition the usage string would otherwise hide.
"""
from __future__ import annotations

import shutil
from pathlib import Path

import pytest

from fno import lint_verb_ratchet as vr

_FNO_FRONT = shutil.which("fno")


# --------------------------------------------------------------------------- #
# Real-surface invariants (AC3: both binaries) - skipped where the Rust front
# is unreachable, since the whole point of AC4 is that it cannot be faked.
# --------------------------------------------------------------------------- #
@pytest.mark.skipif(_FNO_FRONT is None, reason="Rust front `fno` not on PATH")
def test_rust_surface_includes_mux_and_version():
    rust = vr.enumerate_rust_leaves()
    assert "version" in rust
    assert "mux pane ls" in rust
    assert "mux attach" in rust
    # the hidden verbs the usage string omits are still present (constant-only)
    assert "mux tab ls" in rust
    assert "mux layout get" in rust


def test_python_surface_recurses_to_real_leaves():
    py = vr.enumerate_python_leaves()
    # a nested group leaf (recursion works, not just top-level names)
    assert "backlog done" in py
    # a top-level single command
    assert "whoami" in py
    # eager inline commands are counted
    assert "help" in py
    # a true alias is deduped (graph shares backlog's import target)
    assert not any(x == "graph" or x.startswith("graph ") for x in py)


# --------------------------------------------------------------------------- #
# Usage-string parser
# --------------------------------------------------------------------------- #
KNOWN_USAGE = (
    "usage: fno [--session <name>] | fno version [--json] | fno mux server "
    "[--session <name>] | fno mux ls [--json] | fno mux attach <name> | "
    "fno mux kill-server [<name>] [--json] | fno mux shell-init <zsh|bash> "
    "[--json] | fno mux doctor [--json] | fno mux serve --web [--session <name>] "
    "[--bind <addr>] [--port <n>] | fno mux pane ls|read|run|send|wait|kill|"
    "claim|release ... | fno mux block pipe --from <pane> --to <pane> [--block "
    "last|<seq>] [--json] [--force] | fno mux workspace prune [--dry-run] "
    "[--include-named] [--json]"
)


def test_parse_mux_usage_fans_out_pane_alternatives():
    leaves = vr._parse_mux_usage(KNOWN_USAGE)
    assert "version" in leaves
    assert "mux server" in leaves
    assert "mux serve" in leaves
    assert "mux block pipe" in leaves
    assert "mux workspace prune" in leaves
    # the pipe-expanded pane family: all eight, no literal "|"
    for pane in ("ls", "read", "run", "send", "wait", "kill", "claim", "release"):
        assert f"mux pane {pane}" in leaves
    assert "mux shell-init" in leaves  # <zsh|bash> is positional, not a path token
    assert "|" not in "".join(leaves)


# --------------------------------------------------------------------------- #
# Fail-closed Rust-front reach (AC4)
# --------------------------------------------------------------------------- #
def _fake_run(responses):
    """Build a subprocess.run replacement keyed by the first positional token."""
    class _R:
        def __init__(self, returncode, stdout, stderr):
            self.returncode = returncode
            self.stdout = stdout
            self.stderr = stderr

    def _run(argv, *a, **k):
        # argv[0] is the binary; argv[1] is the verb ("version" / "mux")
        verb = argv[1] if len(argv) > 1 else ""
        return _R(*responses[verb])
    return _run


def test_fail_closed_when_rust_front_missing(monkeypatch):
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: None)
    with pytest.raises(vr.VerbRatchetError, match="not on PATH"):
        vr.enumerate_rust_leaves()


def test_fail_closed_when_version_not_rust_front(monkeypatch):
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: Path("/fake/fno"))
    # version exits nonzero -> unreachable
    monkeypatch.setattr(
        vr.subprocess, "run",
        _fake_run({"version": (1, "", "No such command"), "mux": (2, "", KNOWN_USAGE)}),
    )
    with pytest.raises(vr.VerbRatchetError, match="unreachable"):
        vr.enumerate_rust_leaves()


def test_fail_closed_when_version_has_no_rev(monkeypatch):
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: Path("/fake/fno"))
    # a python shim that answers version with non-Rust output -> no git_rev
    monkeypatch.setattr(
        vr.subprocess, "run",
        _fake_run({"version": (0, "fno 0.3.1\n", ""), "mux": (2, "", KNOWN_USAGE)}),
    )
    with pytest.raises(vr.VerbRatchetError, match="no git_rev"):
        vr.enumerate_rust_leaves()


def test_fail_closed_when_stale_front_lacks_mux(monkeypatch):
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: Path("/fake/fno"))
    # a stale front: version answers, but it predates mux (no "mux pane")
    monkeypatch.setattr(
        vr.subprocess, "run",
        _fake_run({
            "version": (0, '{"git_rev":"abc","package":"0.1.0"}', ""),
            "mux": (2, "", "usage: fno [args]"),  # no mux pane anchor
        }),
    )
    with pytest.raises(vr.VerbRatchetError, match="does not own mux"):
        vr.enumerate_rust_leaves()


def test_fail_closed_when_rust_advertises_unknown_verb(monkeypatch):
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: Path("/fake/fno"))
    usage = KNOWN_USAGE.replace("mux block pipe", "mux frobnicate flag")  # new advertised verb
    monkeypatch.setattr(
        vr.subprocess, "run",
        _fake_run({
            "version": (0, '{"git_rev":"abc","package":"0.3.1"}', ""),
            "mux": (2, "", usage),
        }),
    )
    with pytest.raises(vr.VerbRatchetError, match="not listed in RUST_FRONT_VERBS"):
        vr.enumerate_rust_leaves()


def test_fail_closed_when_rust_drops_an_advertised_verb(monkeypatch):
    # Reverse check: a non-hidden constant verb that vanishes from the usage
    # (Rust match arm + usage line both removed, constant left stale) must fail
    # closed - the guard-on-one-of-N-paths gap the module warns about.
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: Path("/fake/fno"))
    usage = KNOWN_USAGE.replace(
        "fno mux serve --web [--session <name>] [--bind <addr>] [--port <n>] | ", ""
    )
    monkeypatch.setattr(
        vr.subprocess, "run",
        _fake_run({
            "version": (0, '{"git_rev":"abc","package":"0.3.1"}', ""),
            "mux": (2, "", usage),
        }),
    )
    with pytest.raises(vr.VerbRatchetError, match="no longer shows"):
        vr.enumerate_rust_leaves()


def test_fail_closed_on_subprocess_timeout(monkeypatch):
    # A hung Rust front raises TimeoutExpired; it must surface as a NAMED
    # fail-closed error, not an uncaught traceback (AC4: named error).
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: Path("/fake/fno"))

    def raise_timeout(argv, *a, **k):
        raise vr.subprocess.TimeoutExpired(cmd=argv, timeout=20)

    monkeypatch.setattr(vr.subprocess, "run", raise_timeout)
    with pytest.raises(vr.VerbRatchetError, match="unreachable"):
        vr.enumerate_rust_leaves()


def test_fail_closed_on_missing_executable(monkeypatch):
    # The binary vanished between `which` and exec -> FileNotFoundError ->
    # named fail-closed, not a traceback.
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: Path("/fake/fno"))
    monkeypatch.setattr(vr.subprocess, "run", lambda *a, **k: (_ for _ in ()).throw(FileNotFoundError(2, "gone")))
    with pytest.raises(vr.VerbRatchetError, match="unreachable"):
        vr.enumerate_rust_leaves()


# --------------------------------------------------------------------------- #
# Ratchet directions (AC2) and conflict message (AC9)
# --------------------------------------------------------------------------- #
_LIVE = ["backlog done", "help", "mux pane ls", "version", "whoami"]


def _check_with(monkeypatch, tmp_path, baseline_text):
    monkeypatch.setattr(vr, "enumerate_all_leaves", lambda: list(_LIVE))
    monkeypatch.setattr(vr, "baseline_path", lambda: tmp_path / "verb-baseline.txt")
    (tmp_path / "verb-baseline.txt").write_text(baseline_text, encoding="utf-8")
    return vr.check()


def test_check_ok_when_baseline_matches_live(monkeypatch, tmp_path):
    report = _check_with(monkeypatch, tmp_path, vr.generate(_LIVE))
    assert report.ok is True
    assert "ok (5 leaves)" in report.message


def test_check_fails_naming_added_verb(monkeypatch, tmp_path):
    # baseline is missing "version" -> live has a verb the baseline lacks (AC2 add)
    stale = [v for v in _LIVE if v != "version"]
    report = _check_with(monkeypatch, tmp_path, vr.generate(stale))
    assert report.ok is False
    assert "version" in report.message
    assert "Added" in report.message
    # AC9: the conflict-is-the-review-moment message is present on a failure
    assert "merge conflict" in report.message


def test_check_fails_naming_removed_verb(monkeypatch, tmp_path):
    # baseline lists a verb live no longer has (AC2 remove without baseline update)
    extra = _LIVE + ["backlog ghost"]
    report = _check_with(monkeypatch, tmp_path, vr.generate(extra))
    assert report.ok is False
    assert "backlog ghost" in report.message
    assert "Removed" in report.message


def test_check_passes_when_removal_updates_baseline(monkeypatch, tmp_path):
    # AC2: a removal WITH the baseline updated -> live and baseline agree -> ok
    shrunk = [v for v in _LIVE if v != "whoami"]
    monkeypatch.setattr(vr, "enumerate_all_leaves", lambda: list(shrunk))
    monkeypatch.setattr(vr, "baseline_path", lambda: tmp_path / "verb-baseline.txt")
    (tmp_path / "verb-baseline.txt").write_text(vr.generate(shrunk), encoding="utf-8")
    assert vr.check().ok is True


# --------------------------------------------------------------------------- #
# Round-trip
# --------------------------------------------------------------------------- #
def test_generate_parse_baseline_roundtrip():
    text = vr.generate(["zeta", "alpha", "mux pane ls"])
    assert vr.parse_baseline(text) == ["zeta", "alpha", "mux pane ls"]
    # comments and blanks are ignored
    noisy = "# header\n\nalpha\n  # mid\nbeta\n"
    assert vr.parse_baseline(noisy) == ["alpha", "beta"]
