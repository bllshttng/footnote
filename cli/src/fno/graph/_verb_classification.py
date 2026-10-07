"""Which backlog verbs does the external tracker backend refuse or mark?

Sets live in the sibling ``_verb_classification.txt`` (one verb per line under
a ``[section]`` header): a new verb is one data line. This module reads the
file into three frozensets and stamps them onto the live registry.
"""

from pathlib import Path

_SECTIONS = ("tracker", "footnote", "no-grain")


def _load() -> "dict[str, frozenset[str]]":
    path = Path(__file__).resolve().parent / "_verb_classification.txt"
    sets: "dict[str, set[str]]" = {name: set() for name in _SECTIONS}
    current = ""
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            current = line[1:-1]
            if current not in sets:
                raise ValueError(f"{path.name}: unknown section [{current}]")
            continue
        if not current:
            raise ValueError(f"{path.name}: verb before any [section]")
        sets[current].add(line)
    return {name: frozenset(rows) for name, rows in sets.items()}


_loaded = _load()

_TRACKER_OWNED_VERBS = _loaded["tracker"]
_FOOTNOTE_OWNED_VERBS = _loaded["footnote"]
_NO_GRAIN_ON_EXTERNAL_BACKEND = _loaded["no-grain"]

_classified = False


def classify_backlog_verbs() -> None:
    """Stamp every live registry verb with its classification, once.

    Not at import: note_cli imports graph.cli to register `note`, so an
    import-time run races that decorator (the original shard 7 failure).
    """
    global _classified
    if _classified:
        return
    import functools

    from fno.graph import cli as graph_cli

    seen: set[str] = set()
    for group, app in graph_cli.iter_backlog_registry():
        for info in app.registered_commands:
            name = info.name or ""
            label = f"{group} {name}" if group else name
            seen.add(label)
            callback = info.callback
            if callback is None:
                raise RuntimeError(f"backlog verb {label!r} has no callback")
            if label in _TRACKER_OWNED_VERBS:
                @functools.wraps(callback)
                def _guarded(*args, _orig=callback, _label=label, **kwargs):
                    graph_cli._refuse_tracker_owned_on_external_backend(_label)
                    return _orig(*args, **kwargs)
                setattr(_guarded, "_fno_tracker_owned", True)
                info.callback = _guarded
            elif label in _FOOTNOTE_OWNED_VERBS:
                setattr(callback, "_fno_footnote_owned", True)
            else:
                raise RuntimeError(
                    f"unclassified backlog verb {label!r}: classify it in "
                    "_verb_classification.txt so the census holds"
                )
    unknown = (_TRACKER_OWNED_VERBS | _FOOTNOTE_OWNED_VERBS) - seen
    if unknown:
        raise RuntimeError(
            f"classified verbs missing from the live registry: {sorted(unknown)}"
        )
    _classified = True
