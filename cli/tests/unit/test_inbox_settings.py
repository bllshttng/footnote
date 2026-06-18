"""Tests for fno.inbox.settings - WatchSettings reader and surfaces map."""
import pytest


def _write_settings(tmp_path, body: str) -> None:
    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir(parents=True, exist_ok=True)
    (abilities_dir / "settings.yaml").write_text(body, encoding="utf-8")


def test_watch_settings_default(tmp_path):
    from fno.inbox.settings import read_watch_settings
    s = read_watch_settings(tmp_path)
    assert s.enabled is False
    assert s.notify_on_send == "question_only"


def test_notify_on_send_valid_values(tmp_path):
    """AC1-HP: valid policy strings are accepted as-is."""
    from fno.inbox.settings import read_watch_settings, WatchSettings

    for valid in ("question_only", "all", "off"):
        abilities_dir = tmp_path / ".fno"
        abilities_dir.mkdir(parents=True, exist_ok=True)
        (abilities_dir / "settings.yaml").write_text(
            f"config:\n  inbox:\n    watch:\n      enabled: true\n      notify_on_send: {valid}\n",
            encoding="utf-8",
        )
        s = read_watch_settings(tmp_path)
        assert s.notify_on_send == valid, f"expected {valid!r}, got {s.notify_on_send!r}"


def test_notify_on_send_typo_fails_closed(tmp_path, capsys):
    """AC1-ERR: unknown notify_on_send value (e.g. 'always') falls back to 'off' (fail-closed).

    Caught by sigma-review HIGH (type-design-analyzer).
    """
    from fno.inbox.settings import read_watch_settings

    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir(parents=True, exist_ok=True)
    (abilities_dir / "settings.yaml").write_text(
        "config:\n  inbox:\n    watch:\n      enabled: true\n      notify_on_send: always\n",
        encoding="utf-8",
    )

    s = read_watch_settings(tmp_path)

    # Must fail closed: 'always' is not a valid policy, so 'off' is used
    assert s.notify_on_send == "off", (
        f"Expected fail-closed 'off', got {s.notify_on_send!r}"
    )

    # A warning must have been written to stderr
    captured = capsys.readouterr()
    assert "always" in captured.err, (
        f"Expected warning mentioning typo value 'always' in stderr, got: {captured.err!r}"
    )


# ---------------------------------------------------------------------------
# read_peer_surfaces
# ---------------------------------------------------------------------------


def test_peers_surfaces_round_trip(tmp_path):
    """AC1-HP: surfaces map round-trips per peer."""
    from fno.inbox.settings import read_peer_surfaces

    _write_settings(
        tmp_path,
        """\
config:
  inbox:
    peers:
      acme-web:
        surfaces: [api-client, ui, design-tokens]
      acme-backend:
        surfaces: [api-server, schema, etl, domain-data]
      acme-docs:
        surfaces: [user-docs, api-reference]
""",
    )

    peers = read_peer_surfaces(tmp_path)

    assert peers == {
        "acme-web": ["api-client", "ui", "design-tokens"],
        "acme-backend": ["api-server", "schema", "etl", "domain-data"],
        "acme-docs": ["user-docs", "api-reference"],
    }


def test_peers_surfaces_missing_block_returns_empty_dict(tmp_path, capsys):
    """AC2-ERR: missing block returns empty dict, no warning."""
    from fno.inbox.settings import read_peer_surfaces

    # No settings.yaml at all.
    peers = read_peer_surfaces(tmp_path)
    assert peers == {}
    captured = capsys.readouterr()
    assert captured.err == "", f"Expected silent missing-block, got stderr: {captured.err!r}"

    # settings.yaml without the block.
    _write_settings(tmp_path, "config:\n  inbox:\n    watch:\n      enabled: true\n")
    peers = read_peer_surfaces(tmp_path)
    assert peers == {}
    captured = capsys.readouterr()
    assert captured.err == "", f"Expected silent missing-block, got stderr: {captured.err!r}"


def test_peers_surfaces_skips_malformed_entries(tmp_path, capsys):
    """A peer entry without a list-typed `surfaces` value is dropped, not crashed on.

    Sigma-review HIGH (silent-failure-hunter): the drop is right, but it
    must be observable. Typo'd entries warn to stderr; missing-key entries
    stay silent (config bug vs not-yet-configured).
    """
    from fno.inbox.settings import read_peer_surfaces

    _write_settings(
        tmp_path,
        """\
config:
  inbox:
    peers:
      acme-web:
        surfaces: [api-client]
      typo-peer:
        surfaces: api-server  # string, not list - dropped
      empty-peer: {}            # no surfaces key - dropped
""",
    )

    peers = read_peer_surfaces(tmp_path)
    assert peers == {"acme-web": ["api-client"]}

    captured = capsys.readouterr()
    # Typo'd entry must warn loudly (config bug)...
    assert "typo-peer" in captured.err, f"typo-peer drop should warn; stderr was: {captured.err!r}"
    # ...but the empty entry stays silent (just unconfigured).
    assert "empty-peer" not in captured.err, (
        f"empty-peer (missing key) should not warn; stderr was: {captured.err!r}"
    )


def test_inbox_settings_handles_null_config_block(tmp_path):
    """gemini-code-assist HIGH on PR #214: a settings.yaml file with a bare
    ``config:`` key (no children) parses to ``{"config": None}``. The earlier
    chained ``data.get("config", {}).get("inbox", {})`` chain crashed with
    ``AttributeError: 'NoneType' object has no attribute 'get'`` because
    ``dict.get`` only returns the default when the key is missing, not when
    its value is null. The fix type-checks each level. This regression test
    pins the safe behavior.
    """
    from fno.inbox.settings import (
        read_peer_surfaces,
        read_surface_patterns,
        read_watch_settings,
    )

    # Bare `config:` key with null value.
    _write_settings(tmp_path, "config:\n")

    # All three readers must return safe defaults, not raise.
    assert read_peer_surfaces(tmp_path) == {}
    assert read_surface_patterns(tmp_path) == {}
    watch = read_watch_settings(tmp_path)
    assert watch.enabled is False

    # Same for `config.inbox: null`.
    _write_settings(tmp_path, "config:\n  inbox:\n")
    assert read_peer_surfaces(tmp_path) == {}
    assert read_surface_patterns(tmp_path) == {}


def test_inbox_settings_warns_on_unreadable_yaml(tmp_path, capsys):
    """Sigma-review HIGH (silent-failure-hunter): YAML parse errors must
    surface a warning so a typo on line 200 is not indistinguishable from
    "no peers configured".
    """
    from fno.inbox.settings import read_peer_surfaces

    _write_settings(tmp_path, "config:\n  inbox:\n    peers:\n      bad: [unclosed\n")

    peers = read_peer_surfaces(tmp_path)
    assert peers == {}
    captured = capsys.readouterr()
    assert "malformed YAML" in captured.err, (
        f"YAML parse failure should print a warning; stderr was: {captured.err!r}"
    )


# ---------------------------------------------------------------------------
# read_surface_patterns
# ---------------------------------------------------------------------------


def test_peers_surface_patterns_round_trip(tmp_path):
    """AC1-HP: surface_patterns map round-trips per surface name."""
    from fno.inbox.settings import read_surface_patterns

    _write_settings(
        tmp_path,
        """\
config:
  inbox:
    surface_patterns:
      api-client: ["src/api/**", "src/lib/api-client/**"]
      api-server: ["api/routes/**", "api/handlers/**"]
      schema: ["migrations/**", "src/db/schema/**"]
""",
    )

    patterns = read_surface_patterns(tmp_path)
    assert patterns == {
        "api-client": ["src/api/**", "src/lib/api-client/**"],
        "api-server": ["api/routes/**", "api/handlers/**"],
        "schema": ["migrations/**", "src/db/schema/**"],
    }


def test_peers_surface_patterns_missing_block_returns_empty(tmp_path, capsys):
    """AC2-ERR: missing surface_patterns block returns {} silently."""
    from fno.inbox.settings import read_surface_patterns

    patterns = read_surface_patterns(tmp_path)
    assert patterns == {}
    captured = capsys.readouterr()
    assert captured.err == ""
