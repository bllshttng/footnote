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
    if "FNO_AGENTS_FRONT" not in vr.os.environ:
        pytest.skip("built fno-agents front not selected")
    rust = vr.enumerate_rust_leaves()
    assert "version" in rust
    assert "mux" in rust
    assert "fno-agents" in rust
    assert not any(leaf.startswith("mux ") for leaf in rust)


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


def test_python_surface_captures_hidden_opts_on_every_leaf_kind():
    # Plain-function leaves (doctor) and eager commands (review) carry hidden
    # options a raw-function or bare-name enumeration would silently miss.
    py = vr.enumerate_python_leaves()
    doctor = [leaf for leaf in py if leaf == "doctor" or leaf.startswith("doctor ")]
    assert doctor and any("!--context-audit" in leaf for leaf in doctor)
    review = [leaf for leaf in py if leaf == "review" or leaf.startswith("review ")]
    assert review and any("!--sigma-" in leaf for leaf in review)


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


# The usage string is no longer PARSED into the verb set - parsing it was half
# the tautology, since it and the Python constant were written together and
# omitted the same verbs. It survives here only as the `mux pane` reachability
# anchor, which is all `enumerate_rust_leaves` still reads it for. The verb set
# comes from `scan_rust_source` (real dispatchers) cross-checked against
# `probe_rust_families` (the live binary's own refusal), and the behavioural
# proof that the scan reads source lives in test_verb_ratchet_source_scan.py.


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


def test_rust_front_override_beats_path(monkeypatch):
    monkeypatch.setenv("FNO_RUST_FRONT", "/explicit/fno")
    monkeypatch.setattr(vr.shutil, "which", lambda _: "/on/path/fno")
    assert vr._locate_rust_front() == Path("/explicit/fno")


def test_rust_front_override_unset_falls_back_to_path(monkeypatch):
    monkeypatch.delenv("FNO_RUST_FRONT", raising=False)
    monkeypatch.setattr(vr.shutil, "which", lambda _: "/on/path/fno")
    assert vr._locate_rust_front() == Path("/on/path/fno")


def test_fno_agents_front_prefers_worktree_build_before_path(monkeypatch, tmp_path):
    built = tmp_path / "crates" / "fno-agents" / "target" / "debug" / "fno-agents"
    built.parent.mkdir(parents=True)
    built.write_text("binary")
    built.chmod(0o755)
    monkeypatch.delenv("FNO_AGENTS_FRONT", raising=False)
    monkeypatch.setattr(vr, "_repo_root", lambda: tmp_path)
    monkeypatch.setattr(vr.shutil, "which", lambda _: "/installed/fno-agents")

    assert vr._locate_fno_agents_front() == built


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


def _reachable_front(monkeypatch):
    """A Rust front that answers `version` and `mux --help` truthfully."""
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: Path("/fake/fno"))
    monkeypatch.setattr(
        vr.subprocess, "run",
        _fake_run({
            "version": (0, '{"git_rev":"abc","package":"0.3.1"}', ""),
            "mux": (2, "", KNOWN_USAGE),
        }),
    )


def test_fail_closed_when_source_dispatches_an_unadvertised_verb(monkeypatch):
    # The dispatcher grew an arm and its own refusal message was not updated.
    # This is the shape that let five live verbs sit unbaselined behind green.
    _reachable_front(monkeypatch)
    monkeypatch.setattr(vr, "scan_rust_source", lambda *a: (set(), {"pane": {"ls", "brandnew"}}))
    monkeypatch.setattr(vr, "probe_rust_families", lambda *a: {"pane": {"ls"}})
    with pytest.raises(vr.VerbRatchetError, match="does not name: brandnew"):
        vr.enumerate_rust_leaves()


def test_fail_closed_when_binary_advertises_a_verb_the_scan_missed(monkeypatch):
    # The other direction: the front says it takes a verb the source scan could
    # not see. That means the scan is blind to some dispatch shape, and a blind
    # scan silently under-reports the surface - so it must refuse, not shrug.
    _reachable_front(monkeypatch)
    monkeypatch.setattr(vr, "scan_rust_source", lambda *a: (set(), {"pane": {"ls"}}))
    monkeypatch.setattr(vr, "probe_rust_families", lambda *a: {"pane": {"ls", "unseen"}})
    with pytest.raises(vr.VerbRatchetError, match="scan did not find: unseen"):
        vr.enumerate_rust_leaves()


def test_fail_closed_when_the_probe_negative_control_does_not_fire(monkeypatch):
    # If the front does not refuse the bogus probe verb by name, its advertised
    # set cannot be read. An empty set here would read as "this family has no
    # verbs" and pass against a baseline that omits them all.
    monkeypatch.setattr(vr, "_locate_rust_front", lambda: Path("/fake/fno"))
    monkeypatch.setattr(
        vr.subprocess, "run",
        _fake_run({"mux": (0, "", "")}),  # accepts the probe, says nothing
    )
    with pytest.raises(vr.VerbRatchetError, match="negative control did not fire"):
        vr.probe_rust_families(Path("/fake/fno"), {"pane"})


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
    assert "ok (5 leaves, 0 hidden options - fno-py only" in report.message


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


# --------------------------------------------------------------------------- #
# Hidden-option emission + flag ratchet (Wave 1)
# --------------------------------------------------------------------------- #
def test_visible_options_are_not_emitted_only_hidden():
    # a visible option never reaches the baseline; only .hidden ones do
    import click

    @click.command()
    @click.option("--visible", is_flag=True)
    @click.option("--secret", is_flag=True, hidden=True)
    def cmd(visible, secret):
        pass

    assert vr._hidden_option_tokens(cmd) == ["!--secret"]
    # the long form is the token when an option carries a secondary opt
    @click.command()
    @click.option("--self/--no-self", default=False, hidden=True)
    def cmd2(self):
        pass

    assert vr._hidden_option_tokens(cmd2) == ["!--self"]


def test_format_leaf_is_bare_without_hidden_options():
    import click

    @click.command()
    @click.option("--visible", is_flag=True)
    def cmd(visible):
        pass

    assert vr._format_leaf("foo bar", cmd) == "foo bar"


def test_split_leaf_separates_path_and_flags():
    path, flags = vr._split_leaf("mail send !--self !--no-self")
    assert path == "mail send"
    assert flags == frozenset({"!--self", "!--no-self"})
    # a bare leaf carries an empty flag set
    path, flags = vr._split_leaf("agents adopt")
    assert path == "agents adopt"
    assert flags == frozenset()


def test_generate_parse_baseline_roundtrip_with_flags():
    text = vr.generate(["mail send !--self", "alpha", "mux pane ls"])
    assert vr.parse_baseline(text) == ["mail send !--self", "alpha", "mux pane ls"]


_LIVE_F = ["backlog done !--tag", "help", "mux pane ls", "version", "whoami"]
_LIVE_F_BARE = ["backlog done", "help", "mux pane ls", "version", "whoami"]


def _check_live(monkeypatch, tmp_path, live, baseline_text):
    monkeypatch.setattr(vr, "enumerate_all_leaves", lambda: list(live))
    monkeypatch.setattr(vr, "baseline_path", lambda: tmp_path / "verb-baseline.txt")
    (tmp_path / "verb-baseline.txt").write_text(baseline_text, encoding="utf-8")
    return vr.check()


def test_check_ok_message_names_scope_and_hidden_count(monkeypatch, tmp_path):
    report = _check_live(monkeypatch, tmp_path, _LIVE_F, vr.generate(_LIVE_F))
    assert report.ok is True
    assert "fno-py only" in report.message
    assert "verbs are read from source, its flags are not" in report.message
    assert "5 leaves" in report.message
    assert "1 hidden option " in report.message


def test_check_fails_naming_added_hidden_flag(monkeypatch, tmp_path):
    # baseline predates the hidden option; live carries it on an existing verb
    report = _check_live(monkeypatch, tmp_path, _LIVE_F, vr.generate(_LIVE_F_BARE))
    assert report.ok is False
    assert "flag-exception" in report.message
    assert "backlog done !--tag" in report.message
    assert "Added hidden options" in report.message


def test_check_passes_when_hidden_flag_is_baselined(monkeypatch, tmp_path):
    # the flag lives in both -> ok (the PR carried flag-exception + a regen)
    report = _check_live(monkeypatch, tmp_path, _LIVE_F, vr.generate(_LIVE_F))
    assert report.ok is True


def test_check_removed_hidden_flag_needs_no_exception(monkeypatch, tmp_path):
    # live dropped the flag, baseline still has it -> fails naming it, but the
    # message asks only for a regenerate, NOT a flag-exception (removals are free)
    report = _check_live(monkeypatch, tmp_path, _LIVE_F_BARE, vr.generate(_LIVE_F))
    assert report.ok is False
    assert "backlog done !--tag" in report.message
    assert "Removed hidden options" in report.message
    assert "flag-exception" not in report.message


# --------------------------------------------------------------------------- #
# Source/baseline agreement: the Python-side twin of the AC4 Rust reach check.
#
# The enumerator IMPORTS `fno.cli` in this interpreter while the baseline is
# written to resolve_repo_root(). Run as a bare `fno`, those are two different
# trees, and `--update` reported "regenerated ... (N leaves)" over a
# byte-identical file - a success line for work it did not do, which is how a
# new verb reached CI unbaselined.
# --------------------------------------------------------------------------- #
def test_enumeration_refuses_when_imported_package_is_not_this_checkout(monkeypatch, tmp_path):
    """A stale installed package must be a named error, not a confident answer."""
    monkeypatch.setattr(vr, "_repo_root", lambda: tmp_path)
    with pytest.raises(vr.VerbRatchetError) as exc:
        vr.enumerate_python_leaves()
    msg = str(exc.value)
    # Both paths named: the reader cannot act on "mismatch" alone.
    assert "imported:" in msg and "expected:" in msg
    # And it names the command that actually works.
    assert "uv run --project cli fno-py" in msg


def test_enumeration_passes_when_package_is_this_checkout():
    """The ordinary in-repo run is unaffected (this test process IS the source)."""
    leaves = vr.enumerate_python_leaves()
    assert "pr" in leaves
    assert "pr merge" in leaves
    assert "pr base-lineage-check" not in leaves


def test_guard_covers_check_not_only_update(monkeypatch, tmp_path):
    """check() reaches the same enumerator, so it must refuse too.

    A guard on `--update` alone would leave check() comparing one tree's surface
    against another tree's baseline - a guard on one of two reachable paths,
    which is the shape this module exists to catch.
    """
    monkeypatch.setattr(vr, "_repo_root", lambda: tmp_path)
    with pytest.raises(vr.VerbRatchetError):
        vr.check()
