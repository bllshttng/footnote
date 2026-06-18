"""fno target subpackage: blast-radius router (x-518f).

Houses the deterministic blast-radius classifier (`blast.py`) that the
`fno target blast-check` verb and the `/target` init size-modulation consume.
Kept off the LOC-ratchet path (the manifest covers cli/src/fno/loop.py and
cli/src/fno/gates/, not this package), so the classifier can grow freely.
"""
