"""The dispatch merge-grant decision, in one place."""


def auto_merge_grant(settings: object | None) -> bool:
    """Return whether ``settings`` grants merge to dispatched workers."""
    try:
        return getattr(getattr(settings, "auto_merge", None), "grant", None) == "dispatch"
    except Exception:  # noqa: BLE001 - malformed settings must never grant
        return False
