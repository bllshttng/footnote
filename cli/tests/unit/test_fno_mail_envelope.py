"""Parity tests for the Python ``<fno_mail>`` renderer against the Rust source of
truth (``crates/fno-agents/src/claude_drive.rs`` G1 tests). The two renderers
produce the same wire bytes, so a delivered message is identical whether it went
out via the claude control.sock inject (Rust) or the codex/gemini/relay paths
(Python). If G1's locked format ever changes, both sides fail together."""
from __future__ import annotations

from fno.mail.envelope import fno_mail_open, wrap_fno_mail


def test_open_tag_matches_rust_fno_mail_open():
    # Mirrors Rust `fno_mail_open_is_lowercase_quoted_attrs`: lowercase tag,
    # key="value" double-quoted attrs, `from` is the short 8-hex, node when present.
    assert (
        fno_mail_open(
            from_="7d1f8bdc", harness="claude-code", model="opus-4.8", node="x-26df"
        )
        == '<fno_mail from="7d1f8bdc" harness="claude-code" model="opus-4.8" node="x-26df">'
    )


def test_open_tag_omits_node_includes_directed_to():
    # node omitted for a node-less sender; `to` included when directed at a peer.
    assert (
        fno_mail_open(
            from_="7d1f8bdc", harness="claude-code", model="opus-4.8", to="ee99ff00"
        )
        == '<fno_mail from="7d1f8bdc" harness="claude-code" model="opus-4.8" to="ee99ff00">'
    )


def test_wrap_is_paired_envelope_matching_rust():
    # Mirrors Rust `wrap_fno_mail_is_a_paired_envelope`: open tag, newline, body,
    # newline, close tag.
    assert (
        wrap_fno_mail(
            "ship it",
            from_="7d1f8bdc",
            harness="claude-code",
            model="opus-4.8",
            node="x-26df",
        )
        == '<fno_mail from="7d1f8bdc" harness="claude-code" model="opus-4.8" node="x-26df">\nship it\n</fno_mail>'
    )


def test_wrap_preserves_multiline_body():
    # A multiline body rides inside the paired tag intact (the control.sock JSON
    # carries it as one `text` field; not subject to the relay single-line rule).
    body = "line one\nline two"
    wrapped = wrap_fno_mail(
        body, from_="aaaa1111", harness="codex", model="gpt-5.5"
    )
    assert wrapped == f'<fno_mail from="aaaa1111" harness="codex" model="gpt-5.5">\n{body}\n</fno_mail>'
    assert wrapped.startswith("<fno_mail ")
    assert wrapped.endswith("</fno_mail>")
