"""`fno do target start` carries every denominator flag its own refusal names.

`start` composes `init`, so a plan-less code node makes init print the
absent-denominator refusal through start's own stdout. That refusal says
"re-run with --deliverables N". Doing exactly that used to fail with
`Error: No such option: --deliverables`, because the option lived on `init`
alone. A receipt naming a flag its own verb rejects is a dead end.
"""
from __future__ import annotations

import inspect
import re

from fno import target_cli
from fno.target.denominator import absent_denominator_refusal

_NODE = {"id": "x-1", "domain": "code", "title": "fix one thing", "details": ""}


def _flags_named_in(message: str) -> set[str]:
    return set(re.findall(r"--[a-z][a-z-]+", message))


def test_start_accepts_every_flag_the_denominator_refusal_names():
    message = absent_denominator_refusal(
        node=_NODE, plan_path=None, deliverables=None
    )
    assert message is not None
    named = _flags_named_in(message)
    assert "--deliverables" in named

    params = set(inspect.signature(target_cli.start).parameters)
    for flag in named:
        assert flag.removeprefix("--").replace("-", "_") in params, (
            f"{flag} is named by the refusal a caller reads out of "
            f"`fno do target start`, but start does not accept it"
        )


def test_start_forwards_the_declared_count_to_init(monkeypatch):
    """The flag must reach init's argv, not just parse. A parsed-and-dropped
    flag reads as accepted and still stamps no denominator."""
    seen: list[list[str]] = []

    class _Done(Exception):
        pass

    def _fake_run(cmd, *a, **kw):
        seen.append(list(cmd))
        raise _Done

    monkeypatch.setattr(target_cli.subprocess, "run", _fake_run)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_prepare_codex_native_branch", lambda *a: "main")
    monkeypatch.setattr(
        target_cli, "_manifest_node_id", lambda *a, **k: None
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda *a, **k: (0, "")
    )
    monkeypatch.setattr(
        target_cli, "_resolve_node_model", lambda *a, **k: (None, "none")
    )

    try:
        target_cli._start_codex_native(
            canonical=target_cli.Path("/repo"),
            cwd=target_cli.Path("/repo/wt"),
            node="x-1",
            plan_path=None,
            size=None,
            model=None,
            harness=None,
            beastmode=False,
            no_merge=False,
            deliverables=4,
        )
    except _Done:
        pass

    assert seen, "init was never invoked"
    assert "--deliverables" in seen[0]
    assert seen[0][seen[0].index("--deliverables") + 1] == "4"
