"""Unit tests for ``detect_project`` in ``fno.graph._intake``.

Specifically locks in the tilde-expansion fix: historical graph entries
with ``cwd: ~/code/me/foo`` must match an absolute ``repo_root()`` of
``/Users/.../code/me/foo``. Without ``os.path.expanduser``, the comparison
silently never matches and triage scoping falls through to global view.
"""
from __future__ import annotations

import os
from unittest.mock import patch

from fno.graph._intake import detect_project


def _entry(project: str, cwd: str) -> dict:
    return {"id": f"ab-{project}001", "project": project, "cwd": cwd}


def test_detect_project_matches_tilde_form_cwd(tmp_path):
    """A graph entry stored with `~/code/me/foo` matches an abs repo_root."""
    fake_root = "/Users/testuser/code/me/foo"
    entries = [_entry("foo", "~/code/me/foo")]

    with patch("fno.graph._intake.repo_root", return_value=fake_root), \
         patch.dict(os.environ, {"HOME": "/Users/testuser"}):
        result = detect_project(entries)

    assert result == "foo", (
        "tilde-form cwd should expanduser-match the absolute repo_root; "
        f"got {result!r}"
    )


def test_detect_project_matches_absolute_form_cwd(tmp_path):
    """A graph entry stored with the absolute path still matches (regression)."""
    fake_root = "/Users/testuser/code/me/foo"
    entries = [_entry("foo", "/Users/testuser/code/me/foo")]

    with patch("fno.graph._intake.repo_root", return_value=fake_root), \
         patch.dict(os.environ, {"HOME": "/Users/testuser"}):
        result = detect_project(entries)

    assert result == "foo"


def test_detect_project_fallback_via_tilde_form_subpath(tmp_path):
    """A tilde-form cwd that is a subpath of repo_root acts as the fallback project."""
    fake_root = "/Users/testuser/code/me/foo"
    # Subpath entry: parent matches but a deeper entry exists
    entries = [_entry("foo-child", "~/code/me/foo/sub")]

    with patch("fno.graph._intake.repo_root", return_value=fake_root), \
         patch.dict(os.environ, {"HOME": "/Users/testuser"}):
        result = detect_project(entries)

    # No exact match, but the tilde-form subpath under the root falls
    # through to the fallback_project branch. Without expanduser this
    # would also fail silently.
    assert result == "foo-child"


def test_detect_project_no_match_returns_none(tmp_path):
    """Unrelated repos return None even with tilde-form cwds in the graph."""
    fake_root = "/Users/testuser/code/me/elsewhere"
    entries = [_entry("foo", "~/code/me/foo")]

    with patch("fno.graph._intake.repo_root", return_value=fake_root), \
         patch.dict(os.environ, {"HOME": "/Users/testuser"}):
        result = detect_project(entries)

    assert result is None


# -- Phase 02: resolution-chain tests for _build_intake_node --


def _build_spec(plan_path: str, **overrides) -> dict:
    spec = {
        "plan_path": plan_path,
        "roadmap_id": None,
        "title": "test",
        "priority": "p2",
        "deps": [],
        "points": None,
    }
    spec.update(overrides)
    return spec


def test_frontmatter_project_beats_cwd(tmp_path, monkeypatch):
    """Frontmatter project takes precedence over git-root-basename inference."""
    from fno.graph._intake import _build_intake_node

    plan = tmp_path / "plan-Z.md"
    plan.write_text(
        "---\nproject: example-pipeline\n---\n# title\n"
    )
    monkeypatch.chdir(tmp_path)
    node = _build_intake_node(_build_spec(str(plan)), [])
    assert node["project"] == "example-pipeline"


def test_cli_project_beats_frontmatter(tmp_path, monkeypatch):
    """``cli_project`` on the spec wins over frontmatter."""
    from fno.graph._intake import _build_intake_node

    plan = tmp_path / "plan-W.md"
    plan.write_text("---\nproject: foo\n---\n")
    monkeypatch.chdir(tmp_path)
    node = _build_intake_node(_build_spec(str(plan), cli_project="bar"), [])
    assert node["project"] == "bar"


def test_frontmatter_cwd_overrides_canonical_root(tmp_path, monkeypatch):
    """Frontmatter cwd field replaces git-root-derived canonical_root."""
    from fno.graph._intake import _build_intake_node

    plan = tmp_path / "plan-V.md"
    plan.write_text(
        "---\nproject: foo\ncwd: /home/user/code/foo\n---\n"
    )
    monkeypatch.chdir(tmp_path)
    node = _build_intake_node(_build_spec(str(plan)), [])
    assert node["cwd"] == "/home/user/code/foo"


def test_frontmatter_cwd_expands_tilde(tmp_path, monkeypatch):
    """``~`` in frontmatter cwd is expanded via os.path.expanduser."""
    from fno.graph._intake import _build_intake_node

    plan = tmp_path / "plan-Vt.md"
    plan.write_text("---\nproject: foo\ncwd: ~/code/foo\n---\n")
    monkeypatch.setenv("HOME", "/Users/testuser")
    monkeypatch.chdir(tmp_path)
    node = _build_intake_node(_build_spec(str(plan)), [])
    assert node["cwd"] == "/Users/testuser/code/foo"


def test_no_frontmatter_falls_through(tmp_path, monkeypatch):
    """Plans with no frontmatter route via the existing chain."""
    from fno.graph._intake import _build_intake_node

    plan = tmp_path / "plan-U.md"
    plan.write_text("# title\nno frontmatter\n")
    monkeypatch.chdir(tmp_path)
    node = _build_intake_node(_build_spec(str(plan)), [])
    assert node["project"] in (None, "")


def test_frontmatter_project_non_string_falls_through(tmp_path, monkeypatch, capsys):
    """If frontmatter project is not a non-empty string, fall through and warn."""
    from fno.graph._intake import _build_intake_node

    plan = tmp_path / "plan-T.md"
    plan.write_text("---\nproject: 123\n---\n")
    monkeypatch.chdir(tmp_path)
    node = _build_intake_node(_build_spec(str(plan)), [])
    err = capsys.readouterr().err
    assert "rejecting non-string project" in err
    assert node["project"] != "123"


def test_frontmatter_cwd_non_string_falls_through(tmp_path, monkeypatch, capsys):
    """If frontmatter cwd is not a non-empty string, fall through and warn."""
    from fno.graph._intake import _build_intake_node

    plan = tmp_path / "plan-Tc.md"
    plan.write_text("---\nproject: foo\ncwd: 42\n---\n")
    monkeypatch.chdir(tmp_path)
    node = _build_intake_node(_build_spec(str(plan)), [])
    err = capsys.readouterr().err
    assert "rejecting non-string cwd" in err
    assert node["cwd"] != "42"


# ---------------------------------------------------------------------------
# Task 1.2: resolve_node_project_and_cwd derives cwd from explicit project
# ---------------------------------------------------------------------------

def _patch_candidates_unit(settings_path):
    from unittest.mock import patch
    return patch(
        "fno.graph._intake._settings_candidate_paths",
        return_value=[settings_path],
    )


def test_resolve_node_explicit_cli_project_derives_cwd_from_workmap(tmp_path, monkeypatch):
    """Site 4: cli_project explicit + no fm_cwd -> cwd from work-map root."""
    import textwrap
    from fno.graph._intake import resolve_node_project_and_cwd
    from unittest.mock import patch

    settings_path = tmp_path / "settings.yaml"
    work_root = str(tmp_path / "fno-root")
    settings_path.write_text(textwrap.dedent(f"""\
        work:
          projects:
            fno:
              path: {work_root}
    """))

    plan = tmp_path / "plan-X.md"
    plan.write_text("---\ntitle: test\n---\n")
    monkeypatch.chdir(tmp_path)

    with _patch_candidates_unit(settings_path):
        project, node_cwd, _ = resolve_node_project_and_cwd(str(plan), "fno", [])

    assert project == "fno"
    assert node_cwd == work_root


def test_resolve_node_fm_project_derives_cwd_from_workmap(tmp_path, monkeypatch):
    """Site 4: fm_project explicit + no fm_cwd -> cwd from work-map root."""
    import textwrap
    from fno.graph._intake import resolve_node_project_and_cwd
    from unittest.mock import patch

    settings_path = tmp_path / "settings.yaml"
    work_root = str(tmp_path / "fm-root")
    settings_path.write_text(textwrap.dedent(f"""\
        work:
          projects:
            fm-proj:
              path: {work_root}
    """))

    plan = tmp_path / "plan-fm.md"
    plan.write_text("---\nproject: fm-proj\n---\n")
    monkeypatch.chdir(tmp_path)

    with _patch_candidates_unit(settings_path):
        project, node_cwd, _ = resolve_node_project_and_cwd(str(plan), None, [])

    assert project == "fm-proj"
    assert node_cwd == work_root


def test_resolve_node_fm_cwd_wins_over_workmap(tmp_path, monkeypatch):
    """Site 4: fm_cwd present -> fm_cwd wins even when project is explicit."""
    import textwrap
    from fno.graph._intake import resolve_node_project_and_cwd
    from unittest.mock import patch

    settings_path = tmp_path / "settings.yaml"
    work_root = str(tmp_path / "fm-root2")
    settings_path.write_text(textwrap.dedent(f"""\
        work:
          projects:
            fm-proj2:
              path: {work_root}
    """))

    plan = tmp_path / "plan-fmcwd.md"
    plan.write_text(
        "---\nproject: fm-proj2\ncwd: /explicit/fm/cwd\n---\n"
    )
    monkeypatch.chdir(tmp_path)

    with _patch_candidates_unit(settings_path):
        _, node_cwd, _ = resolve_node_project_and_cwd(str(plan), None, [])

    assert node_cwd == "/explicit/fm/cwd"


def test_resolve_node_detected_project_cwd_unchanged(tmp_path, monkeypatch):
    """Site 4: project detected from entries (not explicit) -> cwd unchanged (canonical_root)."""
    import textwrap
    from fno.graph._intake import resolve_node_project_and_cwd
    from unittest.mock import patch

    settings_path = tmp_path / "settings.yaml"
    settings_path.write_text(textwrap.dedent("""\
        work:
          projects:
            detected-proj:
              path: /workmap/detected
    """))

    plan = tmp_path / "plan-det.md"
    plan.write_text("---\ntitle: test\n---\n")
    monkeypatch.chdir(tmp_path)

    # entries has a node matching cwd -> project detection path
    fake_root = str(tmp_path)
    entries = [{"id": "ab-00000001", "project": "detected-proj", "cwd": fake_root}]

    with _patch_candidates_unit(settings_path), \
         patch("fno.graph._intake.repo_root", return_value=fake_root), \
         patch("fno.graph._intake.resolve_git_roots", return_value=("detected-proj", fake_root)):
        _, node_cwd, _ = resolve_node_project_and_cwd(str(plan), None, entries)

    # cwd must come from canonical_root, NOT the work-map (detection path is unchanged)
    assert node_cwd == fake_root
