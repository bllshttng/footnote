"""Abilities promise-tag reader plugin for hermes-agent.

Install by symlinking this directory into ~/.hermes/plugins/ per
docs/SETUP-HERMES.md. The plugin writes .fno/target-promise.signal
with the inner content of the last <promise>...</promise> tag that
appears in each assistant response.

See docs/providers/promise-sentinel.md for the protocol.
"""

from .reader import on_response

__all__ = ["on_response"]
