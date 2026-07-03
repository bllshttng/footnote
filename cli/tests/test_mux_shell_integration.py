"""config.mux.shell_integration coercion (OSC 133 auto-inject knob)."""
from __future__ import annotations

from fno.config import ConfigBlock, MuxBlock


def test_yaml_unquoted_off_boolean_coerces_to_off():
    """YAML 1.1 parses unquoted `off` as boolean False; it must still disable.

    This is the load-bearing case: `shell_integration: off` (no quotes) reaches
    the validator as `False`, and a naive `isinstance(v, str)` check would
    silently re-enable it.
    """
    assert MuxBlock.model_validate({"shell_integration": False}).shell_integration == "off"


def test_string_off_coerces_to_off():
    assert MuxBlock.model_validate({"shell_integration": "off"}).shell_integration == "off"
    assert MuxBlock.model_validate({"shell_integration": " off "}).shell_integration == "off"


def test_anything_else_is_mux_panes():
    for v in ("mux-panes", "garbage", True, 42, None, ""):
        assert MuxBlock.model_validate({"shell_integration": v}).shell_integration == "mux-panes"


def test_default_is_on():
    assert MuxBlock().shell_integration == "mux-panes"


def test_nonmapping_mux_block_degrades_to_defaults():
    """`mux: off` (a scalar) must not raise out of the whole settings load."""
    cb = ConfigBlock.model_validate({"mux": 42})
    assert cb.mux.shell_integration == "mux-panes"
    cb2 = ConfigBlock.model_validate({"mux": "off"})
    assert cb2.mux.shell_integration == "mux-panes"
