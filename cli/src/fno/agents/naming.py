"""Thin binding to the fno-agents binary's name verbs (vocabulary owner:
crates/fno-agents/src/naming.rs). Argv marshalling and result parsing only."""

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
    raw = json.loads(_run("name-codes", ["--json"]))
    return (frozenset(raw["sources"]), frozenset(raw["verbs"]), dict(raw["word_codes"]),
            tuple((r["site"], r["source"], r["verb"]) for r in raw["provenance"]))

def dispatch_sources():
    return _codes()[0]

def dispatch_verbs():
    return _codes()[1]

def provenance_rows():
    return _codes()[3]

def slug_component(raw, cap=SLUG_CAP):
    return re.sub(r"-+", "-", re.sub(r"[^a-z0-9-]", "-", (raw or "").lower())).strip("-")[:cap].rstrip("-")

def _flags(*pairs):
    out = []
    for flag, value in pairs:
        if (value or "").strip():
            out += [flag, value or ""]
    return out

def agent_name(prefix, node_id, *, slug=None, qualifier=None, discriminator=None):
    if not (prefix or "").strip() and not (node_id or "").strip():
        raise AgentNameError("agent name needs at least a prefix or a node id")
    return _mint(prefix, node_id, *_flags(("--slug", slug), ("--qualifier", qualifier),
                                          ("--discriminator", discriminator)))

def verb_code_for(word):
    v = (word or "").strip().removeprefix("/fno:").removeprefix("$fno:").lstrip("/") or "target"
    code = _codes()[2].get(v)
    if not code:
        raise AgentNameError(f"unknown dispatch verb {word!r}")
    return code

def dispatch_agent_name(source, verb, identity, *, slug=None, qualifier=None, discriminator=None):
    if (verb or "").strip() not in dispatch_verbs():
        raise AgentNameError(f"unknown dispatch verb {verb!r}")
    return _mint(*(_opt_pos(source, "--source")), "--verb", verb, identity,
                 *_flags(("--slug", slug), ("--qualifier", qualifier), ("--discriminator", discriminator)))

def bridge_name(prefix, node_id, *, slug=None, qualifier=None, discriminator=None,
                source=None, verb=None):
    if verb or source:
        if prefix:
            raise BridgeUsageError("pass the legacy prefix form or --source/--verb, not both")
        if not verb:
            raise BridgeUsageError("--source requires --verb")
        return _mint(*(_opt_pos(source, "--source")), "--verb",
                     verb if verb in dispatch_verbs() else verb_code_for(verb), node_id,
                     *_flags(("--slug", slug), ("--qualifier", qualifier), ("--discriminator", discriminator)))
    if not prefix:
        raise BridgeUsageError("a prefix or --verb is required")
    return agent_name(prefix, node_id, slug=slug, qualifier=qualifier, discriminator=discriminator)

def _opt_pos(value, flag):
    return [flag, value] if (value or "").strip() else []

def parse_many(names):
    if not names:
        return []
    out = []
    for line in _run("name-parse", [], "\n".join(names)).splitlines():
        row = json.loads(line)
        out.append(None if row.get("verb") is None else DispatchName(
            row["name"], row.get("source"), row["verb"], row.get("node"), row.get("tail") or ""))
    return out

def parse_dispatch_agent_name(name):
    return parse_many([name])[0] if name else None

def legacy_verb_code(name):
    if not name:
        return None
    return "t" if name.startswith("target-") else ("th" if name.startswith("think-") else None)
