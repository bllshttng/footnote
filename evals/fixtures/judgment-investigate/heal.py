from cache import read_any


def needs_heal(n):
    return read_any(f"pr:{n}") == "conflicting"
