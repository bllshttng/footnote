"""Provider rotation substrate - public surface for Phase 01.

Re-exports the types and functions callers need so they can do:
    from fno.adapters.providers import ProviderRecord, load_providers
instead of reaching into sub-modules.
"""
from fno.adapters.providers.model import (
    ProviderConfigError,
    ProviderNotFoundError,
    ProviderRecord,
    ProviderStagingError,
    ProviderUnavailableError,
    ProvidersConfig,
)
from fno.adapters.providers.loader import (
    load_providers,
    save_providers,
)

__all__ = [
    "ProviderRecord",
    "ProvidersConfig",
    "ProviderConfigError",
    "ProviderStagingError",
    "ProviderNotFoundError",
    "ProviderUnavailableError",
    "load_providers",
    "save_providers",
]
