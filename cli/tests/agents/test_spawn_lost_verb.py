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
