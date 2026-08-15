"""One-shot LLM-as-a-function subprocess boundary."""

from __future__ import annotations

import json
import os
import subprocess
from collections.abc import Callable
from typing import Any, Optional


_REAL_RUN = subprocess.run


class LLMCallRefused(RuntimeError):
    """A real model call was refused by the test/CI safety boundary."""


def llm_call(
    prompt: str,
    *,
    schema: Optional[dict[str, Any]] = None,
    system_prompt: Optional[str] = None,
    model: Optional[str] = None,
    timeout: float = 300,
    check: bool = False,
    bare_when_api_key: bool = False,
    prompt_as_arg: bool = False,
    runner: Optional[Callable[..., subprocess.CompletedProcess[str]]] = None,
) -> subprocess.CompletedProcess[str]:
    """Run one bounded, tool-less Claude call through the shared seam.

    ``FNO_LLM_STUB`` is the one subprocess-level integration seam. Unit tests
    can instead inject ``runner`` or patch ``subprocess.run``. A real model call
    is refused under pytest/CI so a missing test double never spends credentials.
    """
    run = subprocess.run if runner is None else runner
    stub = (os.environ.get("FNO_LLM_STUB") or "").strip()
    in_pytest = os.environ.get("PYTEST_CURRENT_TEST") is not None
    in_ci = os.environ.get("CI", "").lower() in ("true", "1", "yes")
    if not stub and run is _REAL_RUN and (in_pytest or in_ci):
        raise LLMCallRefused(
            "FNO_LLM_STUB not configured; refusing real claude -p in tests"
        )

    if stub:
        cmd = [stub]
    else:
        cmd = ["claude", "-p"]
        if bare_when_api_key and os.environ.get("ANTHROPIC_API_KEY"):
            cmd.append("--bare")
        cmd += ["--output-format", "json"]
        if schema is not None:
            cmd += ["--json-schema", json.dumps(schema)]
        if system_prompt:
            cmd += ["--append-system-prompt", system_prompt]
        if model:
            cmd += ["--model", model]
        if prompt_as_arg:
            # Behind `--` like every other claude seed: a leading-flag prompt
            # must be the positional, not a claude flag.
            cmd += ["--", prompt]

    prompt_is_argv = prompt_as_arg and not stub
    return run(
        cmd,
        input=None if prompt_is_argv else prompt,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=check,
    )
