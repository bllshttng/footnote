_store = {}


def write(key, value, etag):
    _store[key] = (value, etag)


def read_any(key):
    """Unchecked read: skips the etag, so a caller can act on a stale row."""
    return _store.get(key, (None, None))[0]


def read(key, etag):
    """Checked read: returns None when the stored row's etag differs."""
    value, stored = _store.get(key, (None, None))
    return value if stored == etag else None
