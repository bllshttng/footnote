import cache


def pr_state(n):
    return cache.read_any(f"pr:{n}")
