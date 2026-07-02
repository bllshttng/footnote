"""Characterization / parity safety net for the executor-parser internalization
(ab-58645f63).

The two bash parsers (``scripts/lib/infer-task-executor.sh`` and
``scripts/lib/parse-locked-executor.sh``) were deleted and their logic moved
into ``fno.executor._surface`` / ``fno.executor._locked``. Mis-routing a
frontend changeset to ``do`` (or a backend one to ``impeccable``) is a real
behavioral bug, so this test pulls BOTH pre-delete scripts straight out of git
history, runs them and the new Python modules over a shared fixture corpus, and
asserts byte-for-byte identical output. A green run proves zero routing drift.

If git history for the scripts is unavailable (e.g. a shallow clone after the
delete commit has aged out), the parity assertions are skipped but the Python
modules are still exercised against their expected values so coverage never
silently disappears.
"""
from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

import pytest

from fno.executor import _locked, _surface

# The scripts were deleted on this branch; their last content lives in git
# history. We resolve them relative to the package source so the test runs from
# any cwd.
_REPO_ROOT = Path(__file__).resolve().parents[3]
_INFER_REL = "scripts/lib/infer-task-executor.sh"
_PARSE_REL = "scripts/lib/parse-locked-executor.sh"


def _git_show(rel: str) -> str | None:
    """Return the file content from the most recent commit that still had it.

    Tries HEAD first (in case the delete hasn't been committed yet on this
    branch), then walks back to the last commit that contained the path.
    Returns None if git or the blob is unavailable.
    """
    if shutil.which("git") is None:
        return None
    # First: does HEAD still have it (pre-commit working state)?
    head = subprocess.run(
        ["git", "show", f"HEAD:{rel}"],
        cwd=_REPO_ROOT,
        capture_output=True,
        text=True,
    )
    if head.returncode == 0 and head.stdout:
        return head.stdout
    # Otherwise: last commit that touched/contained the path.
    rev = subprocess.run(
        ["git", "rev-list", "-1", "HEAD", "--", rel],
        cwd=_REPO_ROOT,
        capture_output=True,
        text=True,
    )
    sha = rev.stdout.strip()
    if rev.returncode != 0 or not sha:
        return None
    blob = subprocess.run(
        ["git", "show", f"{sha}:{rel}"],
        cwd=_REPO_ROOT,
        capture_output=True,
        text=True,
    )
    if blob.returncode == 0 and blob.stdout:
        return blob.stdout
    return None


@pytest.fixture(scope="module")
def infer_script(tmp_path_factory) -> Path | None:
    content = _git_show(_INFER_REL)
    if content is None:
        return None
    p = tmp_path_factory.mktemp("bash") / "infer-task-executor.sh"
    p.write_text(content)
    return p


@pytest.fixture(scope="module")
def parse_script(tmp_path_factory) -> Path | None:
    content = _git_show(_PARSE_REL)
    if content is None:
        return None
    p = tmp_path_factory.mktemp("bash") / "parse-locked-executor.sh"
    p.write_text(content)
    return p


def _run_bash(script: Path, stdin: str) -> str:
    return subprocess.run(
        ["bash", str(script)],
        input=stdin,
        capture_output=True,
        text=True,
    ).stdout


# ---------------------------------------------------------------------------
# Surface inference (_surface) fixture corpus
# ---------------------------------------------------------------------------

# (stdin, expected impeccable|do). Each entry is a newline-joined file list.
_SURFACE_FIXTURES: list[tuple[str, str]] = [
    # --- locked frontend matches -> impeccable ---
    ("src/components/Foo.tsx", "impeccable"),
    ("src/Widget.jsx", "impeccable"),
    ("app/page.tsx", "impeccable"),            # App Router via .tsx arm
    ("app/layout.tsx", "impeccable"),
    ("app/routes/api.ts", "impeccable"),       # nested routes/
    ("src/components/Bar.ts", "impeccable"),
    ("packages/ui/components/Button.ts", "impeccable"),
    ("components/Header.ts", "impeccable"),     # root-level components/
    ("routes/api.ts", "impeccable"),            # root-level routes/
    ("src/routes/api.ts", "impeccable"),
    ("pkg/routes/x.go", "impeccable"),
    ("src/styles/main.css", "impeccable"),
    ("src/styles/themes/dark.scss", "impeccable"),
    ("packages/web/src/styles/main.css", "impeccable"),
    # --- backend / non-matching -> do ---
    ("app/main.py", "do"),                      # Python module root, NOT frontend
    ("app/models/user.py", "do"),
    ("app/tasks/celery.py", "do"),
    ("cli/src/fno/loop.py", "do"),
    ("scripts/lib/common.sh", "do"),
    ("pkg/api/server.go", "do"),
    (".fno/settings.yaml", "do"),
    ("docs/readme.md", "do"),
    ("src/utils/format.ts", "do"),
    ("src/Widget.vue", "do"),                   # .vue is NOT a locked surface
    ("src/Widget.svelte", "do"),
    # --- edge: empty / blank ---
    ("", "do"),
    ("\n\n", "do"),
    # --- mixed lists: any frontend wins ---
    ("a.py\nb.py\nc.tsx", "impeccable"),
    ("a.py\nb.go", "do"),
    ("cli/src/loop.py\nsrc/components/Foo.tsx\n", "impeccable"),
    ("src/utils/format.ts\ncli/src/loop.py\n", "do"),
    # --- no trailing newline ---
    ("src/components/Foo.tsx", "impeccable"),
    ("cli/src/loop.py", "do"),
    # --- a directory literally named with a trailing slash and nothing after ---
    # (shell `components/*` matches because `*` matches the empty string;
    # this is a real bash edge the Python port must reproduce exactly)
    ("components/", "impeccable"),
    ("src/routes/", "impeccable"),
    ("src/styles/", "impeccable"),
    ("app/", "do"),
]


@pytest.mark.parametrize("stdin,expected", _SURFACE_FIXTURES)
def test_surface_python_matches_expected(stdin: str, expected: str) -> None:
    """The Python module routes each fixture to the documented value."""
    paths = [ln for ln in stdin.split("\n") if ln != ""]
    matched = _surface.any_frontend_surface(paths)
    assert ("impeccable" if matched else "do") == expected


@pytest.mark.parametrize("stdin,expected", _SURFACE_FIXTURES)
def test_surface_parity_vs_bash(stdin: str, expected: str, infer_script) -> None:
    """Byte-for-byte: the deleted bash CLI and the Python CLI agree."""
    if infer_script is None:
        pytest.skip("bash infer-task-executor.sh unavailable from git history")
    bash_out = _run_bash(infer_script, stdin)
    py = subprocess.run(
        [sys.executable, "-m", "fno.executor._surface"],
        input=stdin,
        capture_output=True,
        text=True,
        cwd=_REPO_ROOT,
        env={**_module_env()},
    )
    assert py.stdout == bash_out, f"stdin={stdin!r} py={py.stdout!r} bash={bash_out!r}"
    # And both agree with the documented expectation.
    assert bash_out.strip() == expected


def test_surface_has_ui_flag() -> None:
    """--has-ui echoes true/false (the infer-has-ui.sh contract)."""
    assert _surface.any_frontend_surface(["src/components/Foo.tsx"]) is True
    assert _surface.any_frontend_surface(["app/main.py"]) is False
    # CLI form
    for stdin, expected in [("src/components/Foo.tsx\n", "true"), ("app/main.py\n", "false"), ("", "false")]:
        py = subprocess.run(
            [sys.executable, "-m", "fno.executor._surface", "--has-ui"],
            input=stdin,
            capture_output=True,
            text=True,
            cwd=_REPO_ROOT,
            env={**_module_env()},
        )
        assert py.stdout.strip() == expected


# ---------------------------------------------------------------------------
# Locked-decision parser (_locked) fixture corpus
# ---------------------------------------------------------------------------

_LOCKED_FIXTURES: list[tuple[str, str]] = [
    # canonical values
    (
        "## Locked Decisions (DO NOT revisit)\n\n"
        "1. **State machine**: explicit per-screen.\n"
        "2. **Executor routing**: plan-level `executor: impeccable` (auto-detected).\n"
        "   Rationale: frontend-only feature.\n",
        "impeccable",
    ),
    ("## Locked Decisions\n\n1. **Executor routing**: plan-level `executor: do` (cli-flag).\n", "do"),
    ("## Locked Decisions\n\n1. **Executor routing**: plan-level `executor: mixed` with per-task overrides.\n", "mixed"),
    # no entry -> empty
    ("## Locked Decisions\n\n1. **Auth model**: cookie-based.\n2. **State machine**: redux.\n", ""),
    ("# A design doc with no Locked Decisions section at all.\n\nJust prose.\n", ""),
    ("", ""),
    # formatting variation
    ("## Locked Decisions\n\n1. **Executor Routing:** plan-level `executor: impeccable`\n", "impeccable"),
    ("## Locked Decisions\n\n1. Executor routing: plan-level `executor: do`\n", "do"),
    ("## Locked Decisions\n\n1. **Executor routing**:    plan-level    `executor:   impeccable`   (auto-detected).\n", "impeccable"),
    # tab-only blank line between entries (must flush the buffer)
    (
        "## Locked Decisions\n\n"
        "1. **Executor routing**: plan-level `executor: do` (auto-detected).\n"
        "\t\n"
        "2. **Other thing**: foo.\n"
        "3. **Executor routing**: plan-level `executor: impeccable` (user-confirmed).\n",
        "impeccable",
    ),
    # mixed casing
    ("## Locked Decisions\n\n1. **executor ROUTING**: plan-level `Executor: Impeccable`\n", "impeccable"),
    # missing provenance suffix
    ("## Locked Decisions\n\n1. **Executor routing**: plan-level `executor: mixed`\n", "mixed"),
    # multiple entries: last wins
    (
        "## Locked Decisions\n\n"
        "1. **Executor routing**: plan-level `executor: do` (auto-detected).\n"
        "2. **Other thing**: foo.\n"
        "3. **Executor routing**: plan-level `executor: impeccable` (user-confirmed).\n",
        "impeccable",
    ),
    # unknown values rejected
    ("## Locked Decisions\n\n1. **Executor routing**: plan-level `executor: garbage`.\n", ""),
    ("## Locked Decisions\n\n1. **Executor routing**: plan-level `executor: archer`.\n", ""),
    # section scoping: executor mention OUTSIDE Locked Decisions must NOT match
    (
        "# Some Doc\n\n"
        "The operator dispatches an `executor: impeccable` in some cases.\n\n"
        "## Locked Decisions\n\n"
        "1. **Auth model**: cookie-based.\n",
        "",
    ),
    # heading-terminated section: a later ## stops the scan
    (
        "## Locked Decisions\n\n"
        "1. **Executor routing**: plan-level `executor: do`\n\n"
        "## Architecture\n\n"
        "We also discuss `executor: impeccable` here in prose.\n",
        "do",
    ),
    # multi-line continuation buffer
    (
        "## Locked Decisions\n\n"
        "1. **Executor routing**:\n"
        "   plan-level `executor: impeccable`\n"
        "   because it is a frontend feature.\n",
        "impeccable",
    ),
]


def _module_env() -> dict:
    import os

    env = dict(os.environ)
    src = str(_REPO_ROOT / "cli" / "src")
    env["PYTHONPATH"] = src + (os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")
    return env


@pytest.mark.parametrize("doc,expected", _LOCKED_FIXTURES)
def test_locked_python_matches_expected(doc: str, expected: str) -> None:
    assert _locked.parse_locked_executor(doc) == expected


@pytest.mark.parametrize("doc,expected", _LOCKED_FIXTURES)
def test_locked_parity_vs_bash(doc: str, expected: str, parse_script) -> None:
    """Byte-for-byte: the deleted bash parser and the Python CLI agree."""
    if parse_script is None:
        pytest.skip("bash parse-locked-executor.sh unavailable from git history")
    bash_out = _run_bash(parse_script, doc)
    py = subprocess.run(
        [sys.executable, "-m", "fno.executor._locked"],
        input=doc,
        capture_output=True,
        text=True,
        cwd=_REPO_ROOT,
        env={**_module_env()},
    )
    assert py.stdout == bash_out, f"doc={doc!r} py={py.stdout!r} bash={bash_out!r}"
    assert bash_out.strip() == expected


# ── x-571f: parse_locked_model (sibling of parse_locked_executor) ──────────────

def test_parse_locked_model_extracts_from_locked_decisions() -> None:
    doc = (
        "# Plan\n\n## Architecture\nWe discuss the model resolver here.\n\n"
        "## Locked Decisions\n1. **Model**: `fable` (user-confirmed)\n"
        "2. Executor: do\n"
    )
    assert _locked.parse_locked_model(doc) == "fable"


def test_parse_locked_model_ignores_prose_outside_section() -> None:
    # A bare `model:` outside Locked Decisions is prose, not a lock.
    doc = "## Overview\nmodel: fable\n\n## Locked Decisions\nExecutor: do\n"
    assert _locked.parse_locked_model(doc) == ""


def test_parse_locked_model_last_wins_and_rejects_overlong() -> None:
    doc = "## Locked Decisions\nModel: opus\nModel: sonnet\n"
    assert _locked.parse_locked_model(doc) == "sonnet"
    over = "## Locked Decisions\nModel: " + "x" * 65 + "\n"
    assert _locked.parse_locked_model(over) == ""


def test_parse_locked_model_rejects_multitoken_and_metachars() -> None:
    # codex review PR #150: a spaced value must be REJECTED, not truncated to the
    # first token (which would transcribe a wrong-but-valid pin onto the node).
    assert _locked.parse_locked_model("## Locked Decisions\nModel: opus 4.8\n") == ""
    # A glob/shell metacharacter is out of the shell-safe charset -> rejected.
    assert _locked.parse_locked_model("## Locked Decisions\nModel: fo*\n") == ""


def test_parse_locked_model_tolerates_bold_styles() -> None:
    # Both bold conventions must parse to the bare token (gemini review PR #150):
    # key-only bold and key+colon bold.
    assert _locked.parse_locked_model("## Locked Decisions\n**Model**: fable\n") == "fable"
    assert _locked.parse_locked_model("## Locked Decisions\n**Model:** fable\n") == "fable"
    assert _locked.parse_locked_model("## Locked Decisions\n1. **Model:** fable\n") == "fable"
    # A metacharacter value is still rejected (the closing-bold consumption does
    # not eat a value's own '*').
    assert _locked.parse_locked_model("## Locked Decisions\n**Model:** fo*\n") == ""


def test_parse_locked_model_tolerates_crlf() -> None:
    # A CRLF-checked-out plan must not leave a trailing \r on the value (gemini
    # review PR #150) that would silently reject a valid pin.
    assert _locked.parse_locked_model("## Locked Decisions\r\nModel: fable\r\n") == "fable"
    assert _locked.parse_locked_model("## Locked Decisions\r\nModel: fable (user-confirmed)\r\n") == "fable"


def test_parse_locked_model_strips_provenance_suffix() -> None:
    # A trailing (provenance) suffix mirrors the executor lock and is dropped.
    doc = "## Locked Decisions\nModel: fable (user-confirmed)\n"
    assert _locked.parse_locked_model(doc) == "fable"
    # A full provider-model id with dots/slashes/dashes passes verbatim.
    assert _locked.parse_locked_model("## Locked Decisions\nModel: claude-opus-4-8\n") == "claude-opus-4-8"


def test_locked_model_cli_key_flag_and_backward_compat() -> None:
    doc = "## Locked Decisions\nModel: fable\n"
    model_out = subprocess.run(
        [sys.executable, "-m", "fno.executor._locked", "--key", "model"],
        input=doc, capture_output=True, text=True, cwd=_REPO_ROOT, env={**_module_env()},
    )
    assert model_out.stdout.strip() == "fable"
    # No flag -> the executor parser (backward compatible): a Model line is
    # invisible to it, so it emits nothing. This proves --key routes the parser
    # rather than the default silently changing.
    exec_out = subprocess.run(
        [sys.executable, "-m", "fno.executor._locked"],
        input=doc, capture_output=True, text=True, cwd=_REPO_ROOT, env={**_module_env()},
    )
    assert exec_out.stdout.strip() == ""
