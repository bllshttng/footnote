"""Unit tests for fno.test_cmd smoke discovery.

discover_shell_harnesses() walks the owned shell-harness trees and returns the
standalone harnesses, so a new tests/hooks/test_x.sh runs with zero registry
edits (the two-test-trees lesson: a green subset is not proof everything ran).
It excludes libraries (scripts/lib/), the orchestrated cli/tests/smoke/
subtree, files without a shell shebang, and source-only helpers.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from fno.test_cmd import (
    _STRUCTURAL_STEPS,
    changed_snapshot,
    discover_shell_harnesses,
    select_changed,
)


def _write(path: Path, body: str, *, executable: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body)
    if executable:
        path.chmod(0o755)


def test_discovers_new_harness_and_excludes_non_harnesses(tmp_path: Path) -> None:
    root = tmp_path

    # AC4-EDGE: a brand-new harness dropped into an owned tree is discovered
    # with no registry edit (the consolidation's core win).
    _write(root / "tests/zzz-probe.sh", "#!/usr/bin/env bash\necho probe\n", executable=True)
    _write(root / "tests/hooks/test_new_probe.sh",
           "#!/usr/bin/env bash\necho hi\n", executable=True)
    # A harness in scripts/tests/ and one under skills/*/tests/.
    _write(root / "scripts/tests/test_one.sh",
           "#!/usr/bin/env bash\necho one\n", executable=True)
    _write(root / "skills/agent/tests/test_one.sh",
           "#!/usr/bin/env bash\necho skill\n", executable=True)

    # A source-only helper: a sibling sources it -> helper, not a harness.
    _write(root / "tests/hooks/test_helper.sh",
           "#!/usr/bin/env bash\n# sourced by a sibling, never standalone\n",
           executable=True)
    _write(root / "tests/hooks/test_uses_helper.sh",
           "#!/usr/bin/env bash\nsource ./test_helper.sh\necho ok\n", executable=True)
    # A helper sourced via the conventional variable form ($SCRIPT_DIR/...):
    # the resolver falls back to the basename under the sourcer's dir.
    _write(root / "tests/hooks/test_var_helper.sh",
           "#!/usr/bin/env bash\n# sourced via $SCRIPT_DIR\n", executable=True)
    _write(root / "tests/hooks/test_uses_var_helper.sh",
           '#!/usr/bin/env bash\nSCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"\n'
           'source "$SCRIPT_DIR/test_var_helper.sh"\necho ok\n', executable=True)

    # Excluded classes.
    _write(root / "scripts/lib/libthing.sh",       # library, out of owned scope
           "#!/usr/bin/env bash\necho lib\n", executable=True)
    _write(root / "cli/tests/smoke/test_smoke_one.sh",  # orchestrated subtree
           "#!/usr/bin/env bash\necho smoke\n", executable=True)
    _write(root / "tests/hooks/not_shell.sh",       # no shell shebang
           "#!/usr/bin/env python3\nprint('no')\n", executable=True)

    found = set(discover_shell_harnesses(root))

    assert "tests/zzz-probe.sh" in found                  # AC4-EDGE
    assert "tests/hooks/test_new_probe.sh" in found
    assert "scripts/tests/test_one.sh" in found
    assert "skills/agent/tests/test_one.sh" in found
    assert "tests/hooks/test_uses_helper.sh" in found     # real harness that sources a helper
    assert "tests/hooks/test_helper.sh" not in found      # source-only helper excluded
    assert "tests/hooks/test_uses_var_helper.sh" in found  # real harness, variable source ref
    assert "tests/hooks/test_var_helper.sh" not in found   # $SCRIPT_DIR-sourced helper excluded
    assert "scripts/lib/libthing.sh" not in found         # library excluded
    assert "cli/tests/smoke/test_smoke_one.sh" not in found  # orchestrated subtree excluded
    assert "tests/hooks/not_shell.sh" not in found        # non-shell shebang excluded


# --- changed-surface selection (fno test smoke --changed) -------------------


def _repo(root: Path) -> None:
    """Minimal shape select_changed() reads: a cli/tests tree + owned harnesses."""
    _write(root / "cli/src/fno/widget.py", "x = 1\n")
    _write(root / "cli/tests/unit/test_widget.py", "def test_x(): pass\n")
    _write(root / "cli/tests/unit/test_test_cmd.py", "def test_y(): pass\n")
    _write(root / "tests/lib/helper.sh", "#!/usr/bin/env bash\n:\n", executable=True)
    for name in ("test_a", "test_b"):
        _write(root / f"tests/lib/{name}.sh",
               '#!/usr/bin/env bash\nsource "$(dirname "$0")/helper.sh"\n:\n',
               executable=True)


def test_python_source_selects_conventional_test_and_names_the_rule(tmp_path: Path) -> None:
    """AC2: the receipt names the selected test AND the rule that selected it."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["cli/src/fno/widget.py"])
    assert [(s["rule"], s["target"]) for s in sel] == [
        ("python-source-stem", "cli/tests/unit/test_widget.py")
    ]
    assert unmapped == []


def test_changed_test_file_selects_itself(tmp_path: Path) -> None:
    """AC1: a changed test file is its own selection (no inference needed)."""
    _repo(tmp_path)
    sel, _ = select_changed(tmp_path, ["cli/tests/unit/test_widget.py"])
    assert sel == [{"rule": "test-file-self", "path": "cli/tests/unit/test_widget.py",
                    "kind": "pytest", "target": "cli/tests/unit/test_widget.py"}]


def test_shell_helper_selects_both_owning_harnesses_once(tmp_path: Path) -> None:
    """AC3: reverse source-reference rule, each owner exactly once."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["tests/lib/helper.sh"])
    assert sorted(s["target"] for s in sel) == ["tests/lib/test_a.sh", "tests/lib/test_b.sh"]
    assert {s["rule"] for s in sel} == {"shell-helper-reverse"}
    assert unmapped == []


def test_unknown_path_is_unmapped_and_selects_nothing(tmp_path: Path) -> None:
    """AC4: an unknown path is visible as unmapped, never silently green."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["docs/some-note.md"])
    assert sel == []
    assert unmapped == ["docs/some-note.md"]


def test_python_source_without_a_conventional_test_is_unmapped(tmp_path: Path) -> None:
    """AC4: best-effort inference that finds nothing admits it."""
    _repo(tmp_path)
    _write(tmp_path / "cli/src/fno/lonely.py", "x = 1\n")
    sel, unmapped = select_changed(tmp_path, ["cli/src/fno/lonely.py"])
    assert sel == []
    assert unmapped == ["cli/src/fno/lonely.py"]


def test_infra_change_selects_the_selector_contract_tests(tmp_path: Path) -> None:
    """Rule 6: a change to the runner itself falls back to its contract tests."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["cli/src/fno/test_cmd.py"])
    assert [s["rule"] for s in sel] == ["infra-broad"]
    assert sel[0]["target"] == "cli/tests/unit/test_test_cmd.py"
    assert unmapped == []


def test_duplicate_selection_is_emitted_once(tmp_path: Path) -> None:
    """Two changed paths that map to one test select it once, not twice."""
    _repo(tmp_path)
    sel, _ = select_changed(
        tmp_path, ["cli/src/fno/widget.py", "cli/tests/unit/test_widget.py"])
    assert len(sel) == 1


def _git_repo(root: Path) -> None:
    for args in (("init", "-q"), ("config", "user.email", "t@t.t"),
                 ("config", "user.name", "t")):
        subprocess.run(["git", "-C", str(root), *args], check=True)


def test_snapshot_diffs_explicit_base_and_head(tmp_path: Path) -> None:
    """The CI path diffs the given revisions, not a mutable remote ref."""
    _git_repo(tmp_path)
    _write(tmp_path / "a.txt", "one\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-qm", "base"], check=True)
    base = subprocess.run(["git", "-C", str(tmp_path), "rev-parse", "HEAD"],
                          capture_output=True, text=True, check=True).stdout.strip()
    _write(tmp_path / "b.txt", "two\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-qm", "head"], check=True)

    paths, reason = changed_snapshot(tmp_path, base, "HEAD")
    assert reason == ""
    assert paths == ["b.txt"]


def test_snapshot_local_mode_falls_back_to_origin_master(tmp_path: Path) -> None:
    """A master-default repo sizes the local diff from origin/master."""
    _git_repo(tmp_path)
    _write(tmp_path / "a.txt", "one\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-qm", "base"], check=True)
    base = subprocess.run(["git", "-C", str(tmp_path), "rev-parse", "HEAD"],
                          capture_output=True, text=True, check=True).stdout.strip()
    subprocess.run(["git", "-C", str(tmp_path), "update-ref",
                    "refs/remotes/origin/master", base], check=True)
    _write(tmp_path / "b.txt", "two\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-qm", "head"], check=True)

    paths, reason = changed_snapshot(tmp_path)
    assert reason == ""
    assert paths == ["b.txt"]


def test_snapshot_local_mode_names_the_merge_base_when_both_refs_are_absent(
    tmp_path: Path,
) -> None:
    """No origin/main and no origin/master: the reason names the merge-base
    failure, not a downstream `git diff <stderr> failed` (stderr from a failed
    probe must not leak into the base variable)."""
    _git_repo(tmp_path)
    _write(tmp_path / "a.txt", "one\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-qm", "base"], check=True)

    paths, reason = changed_snapshot(tmp_path)
    assert paths == []
    assert "cannot resolve merge-base" in reason


def test_snapshot_fails_closed_on_an_unresolvable_base(tmp_path: Path) -> None:
    """AC5: a missing base is an explicit unevaluated result, not an empty diff."""
    _git_repo(tmp_path)
    _write(tmp_path / "a.txt", "one\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(tmp_path), "commit", "-qm", "base"], check=True)

    paths, reason = changed_snapshot(tmp_path, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "HEAD")
    assert paths == []
    assert "does not resolve" in reason


def test_snapshot_refuses_a_half_specified_range(tmp_path: Path) -> None:
    """AC5: --base without --head cannot silently fall back to local mode."""
    _git_repo(tmp_path)
    paths, reason = changed_snapshot(tmp_path, "HEAD", "")
    assert paths == []
    assert "both --base and --head" in reason


def test_python_source_selects_the_suffixed_test_family(tmp_path: Path) -> None:
    """The repo's real convention: claims.py is covered by test_claims_*.py."""
    _repo(tmp_path)
    _write(tmp_path / "cli/src/fno/claims.py", "x = 1\n")
    _write(tmp_path / "cli/tests/unit/test_claims_core.py", "def test_a(): pass\n")
    _write(tmp_path / "cli/tests/unit/test_claims_io.py", "def test_b(): pass\n")
    sel, unmapped = select_changed(tmp_path, ["cli/src/fno/claims.py"])
    assert sorted(s["target"] for s in sel) == [
        "cli/tests/unit/test_claims_core.py", "cli/tests/unit/test_claims_io.py"]
    assert unmapped == []


def test_orchestrated_subtree_maps_to_its_runner_step(tmp_path: Path) -> None:
    """cli/tests/smoke/ is excluded from discovery; its runner step owns it."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["cli/tests/smoke/test_thing.sh"])
    assert [(s["rule"], s["target"]) for s in sel] == [
        ("registry-step", "Smoke tests")]
    assert unmapped == []


def test_deleted_test_file_is_not_handed_to_pytest(tmp_path: Path) -> None:
    """A delete shows up in the diff; running the missing file would just error."""
    _repo(tmp_path)
    sel, unmapped = select_changed(tmp_path, ["cli/tests/unit/test_deleted.py"])
    assert sel == []
    assert unmapped == ["cli/tests/unit/test_deleted.py"]


def test_deleted_source_still_selects_its_surviving_tests(tmp_path: Path) -> None:
    """Deleting widget.py must still run test_widget.py, which outlives it."""
    _repo(tmp_path)
    (tmp_path / "cli/src/fno/widget.py").unlink()
    sel, unmapped = select_changed(tmp_path, ["cli/src/fno/widget.py"])
    assert [s["target"] for s in sel] == ["cli/tests/unit/test_widget.py"]
    assert unmapped == []


def test_root_level_test_maps_to_the_registry_step_that_runs_it(tmp_path: Path) -> None:
    """Root tests/*.py are owned by a step's python3 invocation, not by /tests/."""
    from fno.test_cmd import _STRUCTURAL_STEPS
    owned = [c for _, _, c in _STRUCTURAL_STEPS if "tests/metrics/" in c]
    assert owned, "expected a registry step invoking a root tests/metrics file"
    sel, unmapped = select_changed(
        tmp_path, ["tests/metrics/test_session_cost_dedup.py"])
    assert [s["rule"] for s in sel] == ["registry-step"]
    assert unmapped == []


def test_journey_harness_carries_its_build_prerequisite(tmp_path: Path) -> None:
    """A harness needing the debug binary must not run without the build step.

    Without it the harness exits 77 (a SKIP), which the packet counts as a
    failure - a false red instead of early feedback.
    """
    from fno.test_cmd import _RUST_BUILD_STEP, _changed_steps
    _repo(tmp_path)
    _write(tmp_path / "tests/hooks/test_journey.sh",
           '#!/usr/bin/env bash\nB="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents"\n'
           '[[ -x "$B" ]] || exit 77\n', executable=True)
    sel, _ = select_changed(tmp_path, ["tests/hooks/test_journey.sh"])
    names = [s[0] for s in _changed_steps(tmp_path, sel)]
    assert names[0] == _RUST_BUILD_STEP, names
    assert "tests/hooks/test_journey.sh" in names


def test_plain_harness_does_not_drag_in_the_rust_build(tmp_path: Path) -> None:
    """The prerequisite is conditional; a plain harness pays nothing for it."""
    from fno.test_cmd import _RUST_BUILD_STEP, _changed_steps
    _repo(tmp_path)
    sel, _ = select_changed(tmp_path, ["tests/lib/test_a.sh"])
    assert _RUST_BUILD_STEP not in [s[0] for s in _changed_steps(tmp_path, sel)]


def test_parent_dir_source_ref_resolves_to_the_real_helper(tmp_path: Path) -> None:
    """`source "$SCRIPT_DIR/../lib/config.sh"` is how shared helpers are used."""
    _repo(tmp_path)
    _write(tmp_path / "scripts/lib/shared.sh", "#!/usr/bin/env bash\n:\n")
    _write(tmp_path / "scripts/tests/test_uses_shared.sh",
           '#!/usr/bin/env bash\nSCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"\n'
           'source "$SCRIPT_DIR/../lib/shared.sh"\n', executable=True)
    sel, unmapped = select_changed(tmp_path, ["scripts/lib/shared.sh"])
    assert [(s["rule"], s["target"]) for s in sel] == [
        ("shell-helper-reverse", "scripts/tests/test_uses_shared.sh")]
    assert unmapped == []


def test_quarantined_harnesses_are_never_selected(tmp_path: Path) -> None:
    """The full gate refuses to run these; the packet must not either.

    Selecting one turns an ordinary edit into a red packet that aborts
    preflight BEFORE the real gate, on a test CI would never have run.
    """
    import fno.test_cmd as tc
    _repo(tmp_path)
    _write(tmp_path / "scripts/lib/shared.sh", "#!/usr/bin/env bash\n:\n")
    for name in ("test_rotten.sh", "test_healthy.sh"):
        _write(tmp_path / f"scripts/tests/{name}",
               '#!/usr/bin/env bash\nSCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"\n'
               'source "$SCRIPT_DIR/../lib/shared.sh"\n', executable=True)
    original = tc._DISCOVERY_DEFERRED
    tc._DISCOVERY_DEFERRED = frozenset({"scripts/tests/test_rotten.sh"})
    try:
        sel, _ = select_changed(tmp_path, ["scripts/lib/shared.sh"])
    finally:
        tc._DISCOVERY_DEFERRED = original
    targets = [s["target"] for s in sel]
    assert "scripts/tests/test_healthy.sh" in targets
    assert "scripts/tests/test_rotten.sh" not in targets


def test_changed_pytest_step_is_preceded_by_the_binary_scrub() -> None:
    """The packet must skip @requires_rust exactly as the full gate does."""
    import inspect

    import fno.test_cmd as tc
    src = inspect.getsource(tc._run_changed)
    assert "Pytest (changed subset" in src and "target/debug/fno-agents" in src


def test_prereq_codes_are_per_mode(tmp_path: Path) -> None:
    """22 is changed-only; the full run keeps its documented exit 2.

    A public CLI contract: callers distinguishing setup errors from
    changed-packet non-verdicts must not see one mode's sentinel from the other.
    """
    import inspect

    import fno.test_cmd as tc
    assert "return CHANGED_RC_PREREQ" in inspect.getsource(tc._run_changed)
    full = inspect.getsource(tc._run_smoke)
    assert "CHANGED_RC_PREREQ" not in full
    assert "return 2" in full


def test_empty_flag_values_are_refused(tmp_path: Path) -> None:
    """An unset shell variable is a caller bug, never an instruction.

    `--only=` silently disabled subset selection and ran the FULL suite;
    `--base=` silently fell back to local mode. Both look like the subset the
    caller asked for, which is the whole failure class this mode guards.
    """
    import pytest

    from fno.test_cmd import _parse_smoke_args
    for argv in (["--only="], ["--only"], ["--changed", "--base=", "--head", "HEAD"],
                 ["--changed", "--base", "HEAD", "--head="], ["--changed", "--head", ""],
                 # A following option is a forgotten value, not the value:
                 # `--only --retry-failed` otherwise became an only-glob of
                 # "--retry-failed" and skipped the mutual-exclusion check.
                 ["--only", "--retry-failed"], ["--only", "--changed"],
                 ["--changed", "--base", "--head", "HEAD"]):
        with pytest.raises(ValueError, match="needs a value"):
            _parse_smoke_args(argv)
    # Real values still parse.
    opts = _parse_smoke_args(["--changed", "--base", "HEAD~1", "--head", "HEAD"])
    assert opts["changed"] and opts["base"] == "HEAD~1" and opts["head"] == "HEAD"


def test_pytest_smoke_caps_auto_workers() -> None:
    command = next(
        command
        for name, _cwd, command in _STRUCTURAL_STEPS
        if name == "Pytest (unit + integration)"
    )

    assert "-n auto" in command
    assert "--maxprocesses=4" in command


def test_census_green_is_failure_red_slow_killed_pass_empty_is_usage_error(
    tmp_path: Path, monkeypatch
) -> None:
    """The inverted contract the feature exists to enforce: a green-fast deferred
    harness is FAILURE (exit 1); RED / SLOW / killed all pass (exit 0); an empty
    or unparseable set exits 2. Pinned so it cannot silently invert."""
    import fno.test_cmd as tc

    # GREEN: rc 0, fast, not killed -> exit 1.
    monkeypatch.setattr(tc, "_census_entries", lambda: ["g.sh"])
    monkeypatch.setattr(tc, "_run_bounded", lambda *a: (0, 5.0, False))
    assert tc._run_census_deferred([]) == 1

    # RED + SLOW(fast-but-over-tranche) + killed -> all non-green -> exit 0.
    canned = iter([(1, 5.0, False), (0, 999.0, False), (0, 999.0, True)])
    monkeypatch.setattr(tc, "_census_entries", lambda: ["r.sh", "s.sh", "k.sh"])
    monkeypatch.setattr(tc, "_run_bounded", lambda *a: next(canned))
    assert tc._run_census_deferred([]) == 0

    # Empty / unparseable set -> exit 2.
    monkeypatch.setattr(tc, "_census_entries", lambda: [])
    assert tc._run_census_deferred([]) == 2


def test_census_entries_override_dedups_sorts_and_skips_comments(
    tmp_path: Path, monkeypatch
) -> None:
    import fno.test_cmd as tc

    f = tmp_path / "deferred.txt"
    f.write_text("# comment\n\ntests/b.sh\n tests/a.sh \ntests/b.sh\n")
    monkeypatch.setenv("CENSUS_DEFERRED_FILE", str(f))
    try:
        assert tc._census_entries() == ["tests/a.sh", "tests/b.sh"]
    finally:
        monkeypatch.delenv("CENSUS_DEFERRED_FILE", raising=False)


def test_smoke_discovered_steps_excludes_deferred(tmp_path: Path) -> None:
    """The full-gate consumer (_run_smoke) subtracts _DISCOVERY_DEFERRED too, not
    just select_changed; a dropped clause here would let a drained harness
    re-enter the full gate while every test still passed."""
    import fno.test_cmd as tc

    _write(tmp_path / "tests/healthy.sh", "#!/usr/bin/env bash\n:\n", executable=True)
    _write(tmp_path / "tests/rotten.sh", "#!/usr/bin/env bash\n:\n", executable=True)
    original = tc._DISCOVERY_DEFERRED
    tc._DISCOVERY_DEFERRED = frozenset({"tests/rotten.sh"})
    try:
        rels = [s[0] for s in tc._smoke_discovered_steps(tmp_path, referenced=set())]
    finally:
        tc._DISCOVERY_DEFERRED = original
    assert "tests/healthy.sh" in rels
    assert "tests/rotten.sh" not in rels


def test_run_smoke_routes_through_the_discovered_steps_helper() -> None:
    """_run_smoke builds its discovered steps via _smoke_discovered_steps (the
    helper test_smoke_discovered_steps_excludes_deferred covers), so a regression
    that re-inlines the comprehension and drops the _DISCOVERY_DEFERRED clause
    cannot pass while the full gate silently re-admits a drained harness."""
    import inspect

    import fno.test_cmd as tc

    assert "_smoke_discovered_steps" in inspect.getsource(tc._run_smoke)
