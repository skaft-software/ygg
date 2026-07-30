"""Harbor integration for reproducible Ygg evaluations."""

__all__ = [
    "Ygg",
    "YggAgentError",
    "YggBenchmarkTimeoutError",
    "YggProviderError",
    "YggSetupError",
]


def __getattr__(name: str):
    """Load Harbor-dependent classes only when the adapter is requested."""

    if name in __all__:
        from .ygg_agent import (
            Ygg,
            YggAgentError,
            YggBenchmarkTimeoutError,
            YggProviderError,
            YggSetupError,
        )

        return {
            "Ygg": Ygg,
            "YggAgentError": YggAgentError,
            "YggBenchmarkTimeoutError": YggBenchmarkTimeoutError,
            "YggProviderError": YggProviderError,
            "YggSetupError": YggSetupError,
        }[name]
    raise AttributeError(name)
