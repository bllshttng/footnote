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
    # The binary reads empty-everything as usage (exit 2); in-process producers
    # keep the naming-error contract, so this one refusal stays local.
    if not (prefix or "").strip() and not (node_id or "").strip():
        raise AgentNameError("an agent name needs a prefix or a node id; both were empty")
    return _mint(prefix, node_id, *_flags(("--slug", slug), ("--qualifier", qualifier),
                                          ("--discriminator", discriminator)))

def verb_code_for(word):
    v = (word or "").strip().removeprefix("/fno:").removeprefix("$fno:").lstrip("/") or "target"
    code = _codes()[2].get(v)
    if not code:
        raise AgentNameError(f"unknown dispatch verb {word!r}")
    return code

def dispatch_agent_name(source, verb, identity, *, slug=None, qualifier=None, discriminator=None):
    # An identity longer than the runtime contract can never fit: refuse in
    # process, before any subprocess is spent on a guaranteed refusal.
    if len((identity or "").strip()) > MAX_LEN:
        raise AgentNameError(
            f"required agent-name identity is {len((identity or '').strip())} chars, "
            f"over the {MAX_LEN}-char runtime limit"
        )
    if (verb or "").strip() not in dispatch_verbs():
        raise AgentNameError(f"unknown dispatch verb {verb!r}")
    return _mint(*(_opt_pos(source, "--source")), "--verb", verb, identity,
                 *_flags(("--slug", slug), ("--qualifier", qualifier), ("--discriminator", discriminator)))

def bridge_name(prefix, node_id, *, slug=None, qualifier=None, discriminator=None,
                source=None, verb=None):
    # Usage refusals (both forms, missing verb/prefix) are the binary's texts:
    # _mint maps its exit 2 to BridgeUsageError, exit 3 to AgentNameError. A
    # missing --verb is forwarded as absent so the binary names the refusal.
    if verb or source:
        args = list(_opt_pos(source, "--source"))
        if verb:
            args += ["--verb", verb if verb in dispatch_verbs() else verb_code_for(verb)]
        # A positional prefix rides too: the binary refuses the both-forms pair.
        pos = [prefix, node_id] if (prefix or "").strip() else [node_id]
        return _mint(*args, *pos,
                     *_flags(("--slug", slug), ("--qualifier", qualifier), ("--discriminator", discriminator)))
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
