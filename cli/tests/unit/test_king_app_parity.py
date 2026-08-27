"""`fno king` forwards to `fno agents king`, so that app is the only door.

A verb registered on `king_app` alone is reachable from nowhere. That is not a
theoretical gap: `manifest-path` was left off, and `hooks/target-stop-hook.sh`
calls `fno king manifest-path` on every stop. The missing verb exited 2, which
the hook reads as "resolver cannot answer" rather than "no manifest names this
session", so it burned its retry budget and then ran with the king gate off.

`board` is the one verb that is absent on purpose, because the board lives at
`fno inbox board`. Both facts are pinned here so the next reader does not close
the first gap by reopening the second.
"""

from fno.king.cli import agents_king_app, king_app


def _by_name(app):
    return {command.name: command for command in app.registered_commands}


def test_the_stop_hooks_resolver_verb_is_reachable():
    assert "manifest-path" in _by_name(agents_king_app)


def test_the_agents_door_omits_only_board():
    missing = _by_name(king_app).keys() - _by_name(agents_king_app).keys()
    assert missing == {"board"}


def test_hidden_verbs_stay_hidden_on_the_agents_door():
    agents = _by_name(agents_king_app)
    for name, command in _by_name(king_app).items():
        if name in agents:
            assert agents[name].hidden == command.hidden, name
