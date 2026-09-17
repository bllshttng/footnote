"""AC5 (x-8975): per-lane native bundles driven through the REAL slot lane.

The slot resolver is the real Rust binary; capacity and inventory are pinned,
settings are fixtures. Every assertion lands on a POSITIVE marker: the bundle
tokens in the argv, or the named rung in the receipt. Bundle resolution is
unverified by construction - fno reads no effective harness config - and the
receipt says so, which is the AC5-CONFLICT honest marker.
"""

from __future__ import annotations

import io
from types import SimpleNamespace

import pytest

from fno.agents.spawn_defaults import inject_spawn_defaults
from fno.rust_binary import find_dev_binary

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


def _settings(profiles: dict) -> SimpleNamespace:
    def _block(**cfg):
        return SimpleNamespace(
            provider=cfg.get("provider", ""),
            model=cfg.get("model", ""),
            effort=cfg.get("effort", ""),
            substrate=cfg.get("substrate", ""),
            permission_mode=cfg.get("permission_mode", ""),
            route=cfg.get("route", ""),
            account=cfg.get("account", ""),
            pane_group=cfg.get("pane_group", ""),
            lanes=cfg.get("lanes", []),
            on_exhausted=cfg.get("on_exhausted", "refuse"),
            by_difficulty={},
            on_low="prefer_healthy",
            on_unknown="allow",
            harness=cfg.get("harness", {}),
        )

    return SimpleNamespace(
        agents=SimpleNamespace(
            defaults=_block(),
            profiles={verb: _block(**cfg) for verb, cfg in profiles.items()},
            max_lanes={},
        ),
        routing=SimpleNamespace(models=[]),
        model_routing=None,
    )


def _pin_capacity(monkeypatch, claude=None, codex=None, extra=None, active=None):
    """Pin the capacity readings the verb judges lanes with.

    The Python capacity read was deleted (x-1c38): the verb computes it from
    the runtime-state file, so a hermetic one rides in through env instead of
    a monkeypatched Python function. claude/codex pin one account record each
    (`cl-a` for claude, `cx-a` for codex); None leaves the harness with no
    record, which reads unknown. `extra` adds per-account readings as
    {harness: {account: state}} (a dict value may carry resets_at). `active`
    writes identity stamps as {harness: account}. Returns (config, state)
    paths so a test can move capacity mid-flight.
    """
    import json
    import os
    import tempfile
    import time as _time

    d = tempfile.mkdtemp(prefix="fno-cap-")
    records = []
    for harness, spec in (("claude", claude), ("codex", codex)):
        if spec is not None:
            records.append((f"{'cl' if harness == 'claude' else 'cx'}-a", harness, spec))
    for harness, accounts in (extra or {}).items():
        for account, spec in accounts.items():
            records.append((account, harness, spec))
    cfg = os.path.join(d, "config.toml")
    with open(cfg, "w") as f:
        f.write(f"state_dir = '{d}'\n")
        for account, harness, _spec in records:
            f.write(f'[[accounts.records]]\nid = "{account}"\nharness = "{harness}"\n')
    now = _time.time()

    def row(spec) -> dict:
        if isinstance(spec, dict):
            state, resets = spec.get("state", "ok"), spec.get("resets_at")
        else:
            state, resets = spec, None
        pct = {"ok": 5.0, "low": 95.0}.get(state, 100.0)
        return {
            "probed_at": now,
            "partial": False,
            "windows": [{"label": "daily", "used_pct": pct, "resets_at": resets}],
        }

    state = os.path.join(d, "state.json")
    with open(state, "w") as f:
        f.write(json.dumps({"usage": {a: row(spec) for a, _h, spec in records}}))
    for harness, account in (active or {}).items():
        os.makedirs(os.path.join(d, "providers"), exist_ok=True)
        with open(os.path.join(d, "providers", f".active-{harness}"), "w") as f:
            f.write(account)
    monkeypatch.setenv("FNO_CONFIG", cfg)
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", state)
    return cfg, state


@requires_rust
def test_lane_bundle_reaches_the_argv_behind_the_fence(monkeypatch):
    """AC5-BUNDLE: a lane's exact argv-vector lands behind the -- fence, the
    receipt names the lane rung it came from, and resolution is labelled
    unverified rather than claimed as evidence (AC5-CONFLICT)."""
    _pin_capacity(monkeypatch)
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_settings({
            "target": {
                "lanes": [
                    {
                        "provider": "codex",
                        "model": "gpt-5.6-sol",
                        "args": ["--profile", "sol"],
                    },
                ],
            },
        }),
        stderr=err,
        env={},
    )
    i = out.index("--")
    assert out[i + 1 : i + 3] == ["--profile", "sol"]
    assert "agents.profiles.target.lanes[0].args" in err.getvalue()
    assert "unverified" in err.getvalue()


@requires_rust
def test_sibling_lane_selection_carries_that_lanes_bundle(monkeypatch):
    """AC5-BUNDLE: the first lane's account is exhausted, the sibling is
    selected, and the BUNDLE that reaches the argv is the sibling's - not the
    skipped lane's and not a concatenation of both."""
    _pin_capacity(monkeypatch, claude="ok", extra={"claude": {"acct-a": "exhausted"}})
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_settings({
            "target": {
                "lanes": [
                    {
                        "provider": "claude",
                        "model": "claude-sonnet-5",
                        "account": "acct-a",
                        "args": ["--settings", "a.json"],
                    },
                    {
                        "provider": "claude",
                        "model": "claude-opus-5",
                        "account": "acct-b",
                        "args": ["--settings", "b.json"],
                    },
                ],
            },
        }),
        stderr=err,
        env={},
    )
    assert out[out.index("--model") + 1] == "claude-opus-5"
    i = out.index("--")
    assert out[i + 1 : i + 3] == ["--settings", "b.json"]
    assert "a.json" not in out
    assert "agents.profiles.target.lanes[1].args" in err.getvalue()


@requires_rust
def test_typed_fence_displaces_the_lane_bundle_by_name(monkeypatch):
    """AC5-BUNDLE displacement half: a fence the caller typed selects their
    complete bundle; the configured one is named in the skip line."""
    _pin_capacity(monkeypatch)
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1", "--", "--profile", "mine"],
        settings=_settings({
            "target": {
                "lanes": [
                    {
                        "provider": "codex",
                        "model": "gpt-5.6-sol",
                        "args": ["--profile", "sol"],
                    },
                ],
            },
        }),
        stderr=err,
        env={},
    )
    assert out.count("--") == 1
    assert "harness args skipped" in err.getvalue()
    assert "agents.profiles.target.lanes[0].args" in err.getvalue()


@requires_rust
def test_bundle_leaves_account_attribution_intact(monkeypatch):
    """AC5-POSTURE: the bundle rides passthrough; the lane's account pin is
    still composed and attributed, and the bundle carries no account rewrite."""
    _pin_capacity(monkeypatch)
    err = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_settings({
            "target": {
                "lanes": [
                    {
                        "provider": "claude",
                        "model": "claude-sonnet-5",
                        "account": "acct-a",
                        "args": ["--settings", "a.json"],
                    },
                ],
            },
        }),
        stderr=err,
        env={},
    )
    assert out[out.index("--account") + 1] == "acct-a"
    i = out.index("--")
    assert out[i + 1 : i + 3] == ["--settings", "a.json"]
