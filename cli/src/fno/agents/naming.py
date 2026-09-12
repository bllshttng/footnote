"""Thin argv/parsing binding to the fno-agents binary's name verbs (owner: naming.rs)."""

import json
import os
import re
import subprocess
from collections import namedtuple
from functools import lru_cache

MAX_LEN = 64
SLUG_CAP = 30

class AgentNameError(ValueError):
    pass

class BridgeUsageError(ValueError):
    pass

DispatchName = namedtuple("DispatchName", "name source verb node tail")

def _run(verb, args, stdin=None):
    from fno import rust_binary

    binary = rust_binary.resolve_binary()
    if binary is None:
        raise AgentNameError("no fno-agents binary found; run `fno doctor update`")
    proc = subprocess.run(
        [str(binary), verb, *args], input=stdin, capture_output=True, text=True,
        timeout=30, env={**os.environ, "FNO_AGENTS_RUNTIME": "rust"},
    )
    if proc.returncode:
        msg = (proc.stderr or "binary failed").strip().removeprefix("error: ")
        if verb != "name-mint" or proc.returncode == 3:
            raise AgentNameError(msg)
        raise BridgeUsageError(msg)
    return proc.stdout

def _mint(*args):
    lines = _run("name-mint", list(args)).strip().splitlines()
    if not lines or not lines[-1]:
        raise AgentNameError("name mint produced no name")
    return lines[-1]

@lru_cache(maxsize=1)
def _codes():
    # A garbage payload (an rc-0 stub, a banner) reads as the stale-binary refusal.
    try:
        raw = json.loads(_run("name-codes", ["--json"]))
    except json.JSONDecodeError as exc:
        raise AgentNameError(f"name-codes payload unparsable: the fno-agents binary is stale ({exc})")
    return (frozenset(raw["sources"]), frozenset(raw["verbs"]), dict(raw["word_codes"]))

def dispatch_sources():
    return _codes()[0]

def dispatch_verbs():
    return _codes()[1]

def slug_component(raw, cap=SLUG_CAP):
    return re.sub(r"-+", "-", re.sub(r"[^a-z0-9-]", "-", (raw or "").lower())).strip("-")[:cap].rstrip("-")

def _flags(*pairs):
    out = []
    for flag, value in pairs:
        if (value or "").strip():
            out += [flag, value or ""]
    return out

def mint_or_none(source, verb, identity, **kwargs):
    """The mint for degradable seams: a stale/missing binary reads as None."""
    try:
        return dispatch_agent_name(source, verb, identity, **kwargs)
    except (AgentNameError, BridgeUsageError):
        return None

def parse_node_ids(names):
    """Batch node extraction: one name-parse subprocess for the whole list."""
    keys = list(names)
    try:
        rows = parse_many([name or "" for name in keys]) if keys else []
    except AgentNameError:
        return {name: None for name in keys}
    return {name: (row.node if row else None) for name, row in zip(keys, rows)}

def dispatch_agent_name(source, verb, identity, *, slug=None, qualifier=None, discriminator=None):
    # Pure argv assembly: the binary owns the vocabulary, the budget, and every
    # refusal (exit 3 naming, exit 2 usage), including word -> verb-code mapping.
    return _mint(*(_opt_pos(source, "--source")), "--verb", verb, identity,
                 *_flags(("--slug", slug), ("--qualifier", qualifier), ("--discriminator", discriminator)))

def _opt_pos(value, flag):
    return [flag, value] if (value or "").strip() else []

def parse_many(names):
    if not names:
        return []
    out = []
    for line in _run("name-parse", [], "\n".join(names)).splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            # A garbage row reads as the stale-binary refusal callers guard.
            raise AgentNameError("name-parse produced an unparsable row: the fno-agents binary is stale")
        out.append(None if row.get("verb") is None else DispatchName(
            row["name"], row.get("source"), row["verb"], row.get("node"), row.get("tail") or ""))
    return out

def parse_dispatch_agent_name(name):
    return parse_many([name])[0] if name else None
