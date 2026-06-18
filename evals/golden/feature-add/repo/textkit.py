"""Simple string utilities."""


def reverse(text: str) -> str:
    """Return the string reversed."""
    return text[::-1]


def truncate(text: str, max_len: int) -> str:
    """Return text truncated to max_len characters."""
    if len(text) <= max_len:
        return text
    return text[:max_len]
