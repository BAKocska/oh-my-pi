"""Shared Python-surface errors that perform no work at import time."""

from _omp import EnvUnavailable


class NotWiredError(EnvUnavailable):
    """A frozen Python API has no installed host dispatch arm yet."""
