"""fno executor - the in-package executor resolution modules.

``_locked`` (locked-decision parser) and ``_surface`` (frontend-surface
inference) are the SINGLE source of truth. They are stdlib-only and are
invoked as ``python3 -m fno.executor._locked`` / ``_surface`` from
in-clone bash hooks (infer-has-ui, resolve-plan-executor, the
frontend-craft gate harness), so they must stay importable without
typer. The former ``fno executor`` CLI verb was removed by the verb
audit; the refusal at that spelling lives in ``fno.tombstones``.
"""
