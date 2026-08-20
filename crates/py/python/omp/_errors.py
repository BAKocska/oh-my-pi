"""Shared Python-surface errors that perform no work at import time."""

from __future__ import annotations

from _omp import EnvUnavailable, OmpError


class ManifestError(OmpError):
    """Report one malformed extension-manifest field."""

    def __init__(self, path: str, key: str, detail: str) -> None:
        self.path = path
        self.key = key
        self.detail = detail
        super().__init__(self._message())

    def _message(self) -> str:
        return f"manifest {self.path!r} key {self.key!r}: {self.detail}"


class ApiLevelError(ManifestError):
    """Report an unsupported requested omp API level."""

    def __init__(self, requested: int, supported: frozenset[int]) -> None:
        self.requested = requested
        self.supported = supported
        super().__init__("<manifest>", "omp_api", "unsupported API level")

    def _message(self) -> str:
        return (
            f"requested omp API level {self.requested!r} is unsupported; "
            f"supported levels: {sorted(self.supported)!r}"
        )


class DeclarationLimit(ManifestError):
    """Report that import produced more declarations than the host permits."""

    def __init__(self, count: int, limit: int) -> None:
        self.count = count
        self.limit = limit
        super().__init__(
            "<manifest>",
            "declarations",
            f"declaration count {count} exceeds limit {limit}",
        )


class CapabilityError(OmpError):
    """Report one required capability that was not granted."""

    def __init__(self, capability: object) -> None:
        self.capability = capability
        super().__init__(self._message())

    def _message(self) -> str:
        return f"required capability {self.capability!r} was not granted"


class TrustError(CapabilityError):
    """Report that the active trust tier is below the required tier."""

    def __init__(self, required: object, actual: object) -> None:
        self.required = required
        self.actual = actual
        super().__init__(required)

    def _message(self) -> str:
        return f"trust tier {self.required!r} required; actual tier is {self.actual!r}"


class DuplicateRegistration(OmpError):
    """Report a declaration collision and its incumbent holder."""

    def __init__(self, name: str, holder: str) -> None:
        self.name = name
        self.holder = holder
        super().__init__(f"declaration {name!r} is already held by {holder!r}")


class DeclarationSealed(OmpError):
    """Report a declaration attempted after the registry froze."""

    def __init__(self, name: str) -> None:
        self.name = name
        super().__init__(f"declaration {name!r} was attempted after registry freeze")


class EffectsNotAuthorized(OmpError):
    """Report an operation attempted before its invocation authorized effects."""

    def __init__(self, invocation: str, spec: object) -> None:
        self.invocation = invocation
        self.spec = spec
        super().__init__(
            f"invocation {invocation!r} has not authorized operation {spec!r}"
        )


class DeadlineExceeded(OmpError):
    """Report a deadline that elapsed before an operation could start."""

    def __init__(self, deadline: object) -> None:
        self.deadline = deadline
        super().__init__(f"deadline {deadline!r} elapsed before operation start")


class FrameTooLarge(OmpError):
    """Report an encoded frame that exceeds the transport bound."""

    def __init__(self, actual: int, limit: int) -> None:
        self.actual = actual
        self.limit = limit
        super().__init__(f"encoded frame is {actual} bytes; limit is {limit}")


class ExtensionError(OmpError):
    """An extension declaration or runtime surface failed."""


class SpecError(ExtensionError):
    """A provider declaration failed validation."""


class NotWiredError(EnvUnavailable):
    """A frozen Python API has no installed host dispatch arm yet."""
