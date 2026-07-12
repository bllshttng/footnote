"""Cross-language coupling for the maintainer dev-source pin (x-88b9).

The Rust bootstrap re-provisions from a checkout via ``config.dev.source`` read
DIRECTLY from ``~/.fno/config.toml`` (it runs when ``fno`` itself is broken, so
it cannot shell ``fno config get``). That read hard-codes the ``[dev].source``
key string. If the Pydantic model's key drifts from what the bootstrap reads,
recovery silently breaks. These tests pin BOTH sides so a rename can't slip.
"""
from __future__ import annotations

from pathlib import Path

from fno.config import DevBlock, SettingsModel

_BOOTSTRAP = (
    Path(__file__).resolve().parents[3] / "crates" / "fno" / "src" / "bootstrap.rs"
)


def test_model_exposes_dev_source_defaulting_empty() -> None:
    # Python side: config.dev.source exists and defaults to unset (PyPI path).
    assert DevBlock().source == ""
    assert SettingsModel().dev.source == ""


def test_bootstrap_reads_the_same_dev_source_key() -> None:
    # Rust side: parse_dev_source must key off [dev].source. Assert on the
    # literal table+key the parser uses so a rename here fails this test.
    src = _BOOTSTRAP.read_text()
    assert 'get("dev")' in src, "bootstrap no longer reads the [dev] table"
    assert 'get("source")' in src, "bootstrap no longer reads the .source key"
