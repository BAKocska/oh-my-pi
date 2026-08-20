"""Shared Python-surface errors that perform no work at import time."""

from _omp import EnvUnavailable, OmpError


class ExtensionError(OmpError):
    """An extension declaration or runtime surface failed."""


class NotWiredError(EnvUnavailable):
    """A frozen Python API has no installed host dispatch arm yet."""
