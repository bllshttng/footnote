"""Which backlog verbs does the external tracker backend refuse or mark?

The sets live in the sibling ``_verb_classification.txt`` (package data, one
verb per line under a ``[section]`` header), so a new verb is one data line
and this package stops growing per verb. This module is the reader that turns
the file into the three frozensets the classifier in ``graph/cli.py`` walks.
A malformed section or a verb before any section fails the import.
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
