"""Tests for the launchd plist template and render_plist helper.

Task 5.2: AC1-HP (placeholder substitution) + AC2-ERR (shell-special rejection).
"""
import plistlib
from pathlib import Path

import pytest


def test_render_plist_substitutes_placeholders():
    """AC1-HP: render_plist returns a valid plist with all placeholders replaced."""
    from fno.inbox.watch_cli import render_plist

    rendered = render_plist("fno", Path("/tmp/example-repo/abilities"))
    parsed = plistlib.loads(rendered.encode("utf-8"))

    assert parsed["Label"] == "com.fno.watch.fno"
    assert parsed["ProgramArguments"][1] == "fno"
    assert parsed["ProgramArguments"][2] == "/tmp/example-repo/abilities"
    assert parsed["RunAtLoad"] is True
    assert parsed["KeepAlive"] is True
    assert (
        parsed["StandardOutPath"]
        == "/tmp/example-repo/abilities/.fno/abi-watch.out.log"
    )
    assert (
        parsed["StandardErrorPath"]
        == "/tmp/example-repo/abilities/.fno/abi-watch.err.log"
    )


@pytest.mark.parametrize(
    "bad_name",
    [
        "foo;bar",
        "foo$bar",
        "foo bar",
        "foo/bar",
        "foo\\bar",
        "",
    ],
)
def test_render_plist_rejects_shell_special_chars(bad_name):
    """AC2-ERR: render_plist raises ValueError for project names with special chars."""
    from fno.inbox.watch_cli import render_plist

    with pytest.raises(ValueError):
        render_plist(bad_name, Path("/tmp/x"))
