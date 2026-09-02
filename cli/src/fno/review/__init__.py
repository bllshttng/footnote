"""The fno review lane's CLI-adjacent helpers.

The sigma panel this package once hosted is REMOVED (x-f324): the bespoke
orchestrator, scorer, cache, runner and artifact surfaces are gone, and a
config still naming sigma is refused at init with the lane named as the
replacement. What remains is what the retained lanes use:

- :mod:`fno.review.findings` - the findings classifier the emit step shares
- :mod:`fno.review.cli` - ``fno do review classify`` / ``resolve-level``
- :mod:`fno.review.invocation` - review invocation records
- :mod:`fno.review.locking` - the review lock the lane holds while it runs
- :mod:`fno.review.policy` - risk classification + assurance resolution
- :mod:`fno.review.provider_resolution` - cross-model capacity substrate
"""
