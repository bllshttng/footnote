"""Footnote promise-tag reader plugin for Hermes Agent.

Install by symlinking this directory into ~/.hermes/plugins/ and enabling it
per docs/SETUP-HERMES.md. After each model reply (the ``post_llm_call`` hook)
the plugin writes .fno/target-promise.signal with the inner content of the
last <promise>...</promise> tag in the reply.

See docs/harnesses/promise-sentinel.md for the protocol.
"""

import os

from .reader import on_response

__all__ = ["on_response", "register"]


def _post_llm_call(**kwargs):
    text = kwargs.get("assistant_response")
    if isinstance(text, str) and text:
        on_response(text, os.getcwd())


def register(ctx):
    ctx.register_hook("post_llm_call", _post_llm_call)
