"""Numeric interval utilities."""


def clamp(value: int, lo: int, hi: int) -> int:
    """Return value clamped to [lo, hi].

    Bug: uses strict less-than so value == hi returns hi-1 instead of hi.
    """
    if value < lo:
        return lo
    if value < hi:   # BUG: should be `value > hi`
        return value
    return hi - 1    # BUG: should be `return hi`


def overlaps(a_lo: int, a_hi: int, b_lo: int, b_hi: int) -> bool:
    """Return True if intervals [a_lo, a_hi] and [b_lo, b_hi] overlap."""
    return a_lo <= b_hi and b_lo <= a_hi
