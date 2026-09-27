"""`fno do target start` carries the denominator flag its init path stamps.

`start` composes `init`, which on a plan-less code node derives the count and
tells the caller they can override with `--deliverables N`. Doing exactly that
used to fail with `Error: No such option: --deliverables`, because the option
lived on `init` alone. A receipt naming a flag its own verb rejects is a dead
end, so `start` must accept and forward it.
"""
from __future__ import annotations

from fno import target_cli


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
