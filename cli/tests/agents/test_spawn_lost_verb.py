"""A spawn payload whose `$fno:` prefix the calling shell ate is refused."""
from __future__ import annotations

import os
import shutil
import subprocess

import pytest

from fno.agents import harness_map
from fno.agents.harness_map import lost_verb_refusal

VERBS = frozenset({"target", "blueprint", "think", "review", "execute", "fix", "pr", "tdd", "law"})


@pytest.fixture(autouse=True)
def _roster(monkeypatch):
    monkeypatch.setattr(harness_map, "footnote_verbs", lambda: VERBS)


def _expand(shell: str, payload: str) -> str:
    """What `shell` hands fno for a double-quoted `payload`, with no `fno` var set."""
    argv = [shell, "-f", "-c"] if shell == "zsh" else [shell, "--norc", "-c"]
    script = f'printf "%s" "{payload}"'
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
    return subprocess.run(argv + [script], env=env, capture_output=True, text=True, check=True).stdout


@pytest.mark.parametrize("shell", ["bash", "zsh"])
@pytest.mark.parametrize(
    "payload",
    ["$fno:target x-1", "do a $fno:blueprint x-1", "$fno:think x-1", "$fno:review high", "$fno:execute plan.md", "$fno:fix x-1", "$fno:pr check 7"],
)
def test_a_real_shell_mangling_is_refused(shell, payload):
    if shutil.which(shell) is None:
        pytest.skip(f"{shell} not installed")
    mangled = _expand(shell, payload)
    assert "$fno:" not in mangled, f"control: {shell} did not expand {payload!r}"
    reason = lost_verb_refusal(mangled)
    assert reason is not None, f"{shell} turned {payload!r} into {mangled!r}"
    assert "Single-quote the payload" in reason


def test_the_refusal_names_the_verb_and_the_quoting_fix():
    reason = lost_verb_refusal("arget x-1")
    assert "'arget'" in reason
    assert "'$fno:target ...'" in reason


@pytest.mark.parametrize(
    "payload",
    [
        "$fno:target x-1",
        "/fno:target x-1",
        "/target x-1",
        "do a $fno:blueprint x-1",
        "fix the login bug",
        "use dd to image the disk",
        "note: target the review queue",
        "",
    ],
)
def test_an_intact_payload_passes(payload):
    assert lost_verb_refusal(payload) is None


def test_the_target_family_is_caught_with_an_empty_roster(monkeypatch):
    monkeypatch.setattr(harness_map, "footnote_verbs", lambda: frozenset())
    assert lost_verb_refusal("arget x-1") is not None
    assert lost_verb_refusal(":blueprint x-1") is not None


def test_the_seam_refuses_an_eaten_verb_before_the_rust_route(capsys):
    """The Rust client execs before cmd_spawn on a thread spawn (auto mode plus
    an installed binary), so the judgment has to sit at the make_context seam."""
    from fno.agents import rust_runtime

    with pytest.raises(SystemExit) as exc:
        rust_runtime._refuse_lost_verb_payload(
            ["spawn", "--name", "eaten-r", "-H", "codex", "arget x-1", "--substrate", "thread"]
        )
    assert exc.value.code == 2
    assert "Single-quote the payload" in capsys.readouterr().err


def test_the_seam_leaves_an_intact_payload_alone():
    from fno.agents import rust_runtime

    rust_runtime._refuse_lost_verb_payload(
        ["spawn", "--name", "w", "-H", "codex", "$fno:pr check 7", "--substrate", "thread"]
    )


def test_make_context_refuses_before_the_rust_route(monkeypatch):
    from typer.testing import CliRunner

    from fno.agents import rust_runtime
    from fno.agents.cli import agents_app

    monkeypatch.setenv("FNO_AGENTS_RUNTIME", "rust")
    routed = []
    monkeypatch.setattr(rust_runtime, "route_to_rust", lambda args, **k: routed.append(list(args)))
    res = CliRunner().invoke(
        agents_app,
        ["spawn", "--name", "eaten-m", "-H", "codex", "arget x-1", "--substrate", "thread"],
    )
    assert res.exit_code == 2, res.output
    assert "Single-quote the payload" in res.output
    assert routed == []
