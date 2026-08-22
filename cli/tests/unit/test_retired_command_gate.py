"""The retired-command gate's verdicts, exercised on a synthetic tree.

The gate decides whether a caller-facing string shows a ruling-retired
command in RUNNABLE form. Its verdicts are cheap to state and easy to get
wrong, so each one gets a case here against a repo built for the purpose
rather than against the real tree, whose contents move under every sweep.

The negative cases matter as much as the positive ones. A gate never observed
failing has not been tested, and all three of this gate's controls exist to refuse
a pass that only means "the instrument never matched anything".
"""
from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[3]
GATE = REPO_ROOT / "scripts" / "ci" / "check-retired-command-strings.sh"
REGISTRY = REPO_ROOT / "scripts" / "ci" / "retired-commands.txt"
CANARY = REPO_ROOT / "scripts" / "ci" / "fixtures" / "retired-command-canary.sh"


def _git(repo: Path, *args: str) -> None:
    subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """A minimal tree the gate can scan, carrying the real gate and registry."""
    for sub in (
        "scripts/ci/fixtures", "cli/src/fno", "crates/fno-agents/src",
        "skills/k", "docs", "hooks", "agents", "commands",
    ):
        (tmp_path / sub).mkdir(parents=True, exist_ok=True)
    shutil.copy(GATE, tmp_path / "scripts/ci/check-retired-command-strings.sh")
    shutil.copy(REGISTRY, tmp_path / "scripts/ci/retired-commands.txt")
    shutil.copy(CANARY, tmp_path / "scripts/ci/fixtures/retired-command-canary.sh")
    (tmp_path / "scripts/ci/retired-ok-paths.txt").write_text("# none\n")

    # Every surface must hold at least one tracked file of its extension, or
    # the per-surface control fires before any verdict is reached.
    (tmp_path / "cli/src/fno/a.py").write_text("x = 1\n")
    (tmp_path / "crates/fno-agents/src/a.rs").write_text("fn a() {}\n")
    (tmp_path / "skills/k/a.md").write_text("prose\n")
    (tmp_path / "docs/a.md").write_text("prose\n")
    (tmp_path / "hooks/a.sh").write_text("true\n")
    (tmp_path / "agents/a.md").write_text("prose\n")
    (tmp_path / "commands/a.md").write_text("prose\n")

    _git(tmp_path, "init", "-q", "-b", "main")
    _git(tmp_path, "add", "-A")
    _git(tmp_path, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init")
    return tmp_path


def _run(repo: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", "scripts/ci/check-retired-command-strings.sh"],
        cwd=repo,
        capture_output=True,
        text=True,
    )


def _add(repo: Path, rel: str, body: str) -> None:
    (repo / rel).write_text(body)
    _git(repo, "add", "-A")


def test_clean_tree_passes_and_reports_what_it_inspected(repo: Path) -> None:
    result = _run(repo)
    assert result.returncode == 0, result.stderr
    # A count, not silence: a clean scan must read differently from an empty one.
    assert "inspected 1 runnable-form site(s)" in result.stdout


def test_runnable_form_in_a_python_string_fails(repo: Path) -> None:
    _add(repo, "cli/src/fno/new.py", 'HELP = "Clean up with claude rm <short_id>."\n')
    result = _run(repo)
    assert result.returncode == 1
    assert "cli/src/fno/new.py" in result.stderr


def test_failure_message_names_command_replacement_and_ruling(repo: Path) -> None:
    _add(repo, "cli/src/fno/new.py", 'HELP = "Clean up with claude rm <short_id>."\n')
    result = _run(repo)
    assert "claude rm" in result.stderr
    assert "fno agents rm" in result.stderr
    assert "d-1900e419" in result.stderr
    # The escape is named at the refusal, not left to a doc page: a gate whose
    # way out is undocumented teaches the reader to override it.
    assert "retired-ok:" in result.stderr


def test_marker_on_the_same_line_clears_the_site(repo: Path) -> None:
    _add(
        repo,
        "cli/src/fno/new.py",
        'MSG = "claude rm <short_id> exited 1"  # retired-ok: reports a failed shellout.\n',
    )
    assert _run(repo).returncode == 0


def test_marker_on_the_line_above_clears_the_site(repo: Path) -> None:
    _add(
        repo,
        "cli/src/fno/new.py",
        '# retired-ok: reports a failed shellout.\nMSG = "claude rm <short_id> exited 1"\n',
    )
    assert _run(repo).returncode == 0


def test_marker_two_lines_above_does_not_clear_the_site(repo: Path) -> None:
    """The window is deliberately one line: a marker must sit at its site."""
    _add(
        repo,
        "cli/src/fno/new.py",
        '# retired-ok: reports a failed shellout.\n# padding\nMSG = "claude rm <short_id>"\n',
    )
    assert _run(repo).returncode == 1


def test_bare_form_is_out_of_scope(repo: Path) -> None:
    """"claude rm failed to start" names the command and cannot be run."""
    _add(repo, "cli/src/fno/new.py", 'MSG = "claude rm failed to start"\n')
    assert _run(repo).returncode == 0


def test_rust_doc_comment_is_never_reported(repo: Path) -> None:
    _add(
        repo,
        "crates/fno-agents/src/new.rs",
        "/// Shells out to `claude rm <short_id>` on teardown.\nfn f() {}\n",
    )
    assert _run(repo).returncode == 0


def test_rust_runtime_string_is_reported(repo: Path) -> None:
    _add(
        repo,
        "crates/fno-agents/src/new.rs",
        'fn f() { eprintln!("tear it down with claude rm {short}"); }\n',
    )
    assert _run(repo).returncode == 1


def test_markdown_prose_is_reported(repo: Path) -> None:
    _add(repo, "skills/k/new.md", "Run `claude stop <short_id>` first.\n")
    assert _run(repo).returncode == 1


def test_malformed_registry_line_names_its_line_number(repo: Path) -> None:
    """A silently skipped entry is a retired command the gate stops seeking."""
    reg = repo / "scripts/ci/retired-commands.txt"
    reg.write_text("# header\nclaude rm|missing the ruling field\n")
    _git(repo, "add", "-A")
    result = _run(repo)
    assert result.returncode == 1
    assert ":2:" in result.stderr and "malformed" in result.stderr


def test_a_surface_that_resolves_to_nothing_fails(repo: Path) -> None:
    """The control that the first draft of this gate lacked.

    A pathspec matching zero files fails silently, and a whole-scan control
    cannot see it while another surface still returns hits.
    """
    shutil.rmtree(repo / "crates")
    _git(repo, "add", "-A")
    result = _run(repo)
    assert result.returncode == 1
    assert "surface crates holds no tracked .rs files" in result.stderr


def test_losing_the_canary_fixture_is_refused_as_vacuous(repo: Path) -> None:
    """The target control asserts on the FIXTURE, not on production text.

    That is deliberate. A real cleanup may legitimately take the tree to zero
    marked sites, and a control keyed on production text cannot tell that from
    a marker spelling that drifted - it would answer a successful cleanup with
    a permanent red whose only exits are to re-add a forbidden string or to
    delete the gate.
    """
    (repo / "scripts/ci/fixtures/retired-command-canary.sh").unlink()
    _git(repo, "add", "-A")
    result = _run(repo)
    assert result.returncode == 1
    assert "vacuous" in result.stderr


def test_the_canary_line_is_a_real_hit_when_unmarked(repo: Path) -> None:
    """A canary that the pattern cannot match would certify nothing."""
    canary = repo / "scripts/ci/fixtures/retired-command-canary.sh"
    canary.write_text(canary.read_text().replace("# retired-ok:", "# note:"))
    _git(repo, "add", "-A")
    result = _run(repo)
    assert result.returncode == 1
    assert "retired-command-canary.sh" in result.stderr


def test_a_concrete_short_id_is_reported(repo: Path) -> None:
    """`claude rm 7c5dcf5d` is strictly more copyable than a placeholder."""
    _add(repo, "docs/new.md", "You are responsible for the manual `claude rm 7c5dcf5d`.\n")
    result = _run(repo)
    assert result.returncode == 1
    assert "docs/new.md" in result.stderr


def test_a_wrapped_format_string_is_reported(repo: Path) -> None:
    """The shape that carried a live instruction past the first draft.

    The command ends one line and its argument starts the next, so no
    argument token ever appears on the matched line.
    """
    _add(
        repo,
        "cli/src/fno/new.py",
        'sys.stderr.write(\n    f"Orphan supervisor: claude rm "\n    f"{short_id} to clean later.\\n"\n)\n',
    )
    result = _run(repo)
    assert result.returncode == 1
    assert "cli/src/fno/new.py" in result.stderr


def test_docs_are_scanned(repo: Path) -> None:
    """The operator guide is the most caller-facing surface there is."""
    _add(repo, "docs/new.md", "Run `claude stop <short_id>` by hand first.\n")
    assert _run(repo).returncode == 1


def test_a_shell_comment_is_never_reported(repo: Path) -> None:
    _add(repo, "hooks/new.sh", "# reaps with claude rm <short_id> internally\ntrue\n")
    assert _run(repo).returncode == 0


def test_a_path_on_the_allowlist_clears_its_sites(repo: Path) -> None:
    """The exemption for a file that cannot afford an in-file marker.

    A byte-budgeted file is re-read on a schedule, so a marker in it is paid
    every time. The brief that forced this was 36 bytes over its budget with
    the marker and exactly at main's size without it.
    """
    _add(repo, "docs/budgeted.md", "Never run a bare `claude rm <id>`.\n")
    assert _run(repo).returncode == 1
    (repo / "scripts/ci/retired-ok-paths.txt").write_text(
        "docs/budgeted.md|byte-budgeted, so an in-file marker is paid per read\n"
    )
    _git(repo, "add", "-A")
    assert _run(repo).returncode == 0


def test_the_success_line_names_every_path_it_cleared(repo: Path) -> None:
    """A whole-file exemption shown only as a count reads like no exemption."""
    _add(repo, "docs/budgeted.md", "Never run a bare `claude rm <id>`.\n")
    (repo / "scripts/ci/retired-ok-paths.txt").write_text(
        "docs/budgeted.md|byte-budgeted, so an in-file marker is paid per read\n"
    )
    _git(repo, "add", "-A")
    result = _run(repo)
    assert result.returncode == 0
    assert "1 declared by path" in result.stdout
    assert "docs/budgeted.md" in result.stdout


def test_a_stale_allowlist_entry_fails(repo: Path) -> None:
    """An exemption for a path that no longer exists silently widens the gate."""
    (repo / "scripts/ci/retired-ok-paths.txt").write_text(
        "docs/deleted.md|it moved and nobody updated this line\n"
    )
    _git(repo, "add", "-A")
    result = _run(repo)
    assert result.returncode == 1
    assert "does not exist" in result.stderr


def test_a_malformed_allowlist_line_names_its_line_number(repo: Path) -> None:
    (repo / "scripts/ci/retired-ok-paths.txt").write_text(
        "# header\ndocs/budgeted.md\n"
    )
    _git(repo, "add", "-A")
    result = _run(repo)
    assert result.returncode == 1
    assert ":2:" in result.stderr and "malformed" in result.stderr
