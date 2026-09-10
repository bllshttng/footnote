"""The dispatch verb registry model: an outside skill as a descriptor, not a
bare string. Field for field it mirrors ``ReviewerDescriptor``; the lookup
union drops any key naming a shipped or allowlisted verb."""
from __future__ import annotations

from dataclasses import dataclass
from typing import Literal, Mapping, Optional, Sequence


@dataclass(frozen=True)
class DispatchVerbDescriptor:
    # The unknown-harness default spelling, exactly as the worker must receive
    # it. Rendered verbatim; never fno-namespaced.
    invocation: str
    # Per-harness overrides. Present means this verb exists ONLY on these
    # harnesses; absent means `invocation` is correct everywhere. Same field,
    # same meaning, as ReviewerDescriptor.
    invocations: Optional[Mapping[str, str]] = None
    # `skill` is probed against the harness's skill roots at resolve time.
    requires: Literal["none", "skill"] = "none"
    # Whether the resolver appends the node id.
    takes_node_id: bool = True
    # What a completion proves, weakest last.
    asserts: Literal["pr", "doc", "invocation"] = "invocation"


#: The built-in dispatch verbs. harness_map._DEFAULT_ALLOWED_VERBS mirrors
#: this tuple; that module imports this one lazily and must stay cycle-free.
DEFAULT_DISPATCH_VERBS = ("/target", "/think", "/blueprint")


def canonical_verb_key(key: str) -> str:
    """Leading `/`, `/fno:x` -> `/x`: the resolver's canonical spelling."""
    k = key.strip()
    if k[:1] == "/":
        k = k[1:]
    if k[:4] == "fno:":
        k = k[4:]
    return "/" + k if k else k


def resolvable_verbs(
    registry: Optional[Mapping[str, DispatchVerbDescriptor]] = None,
    allowed: Optional[Sequence[str]] = None,
) -> dict[str, DispatchVerbDescriptor]:
    """The registry, minus shipped spellings. A key canonicalizing to a
    built-in or operator-allowlisted verb is dropped, so a project cannot
    redefine a shipped verb into a weaker descriptor."""
    allowed_canon = {
        canonical_verb_key(v)
        for v in (tuple(allowed) if allowed is not None else DEFAULT_DISPATCH_VERBS)
        if isinstance(v, str) and v.strip()
    }
    return {
        canon: desc
        for key, desc in (registry or {}).items()
        if (canon := canonical_verb_key(str(key))) not in allowed_canon
    }
