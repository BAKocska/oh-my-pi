"""Typed artifact references and host-backed artifact operations."""

from __future__ import annotations

from collections.abc import AsyncIterator, Sequence
from dataclasses import dataclass
from typing import Any, Protocol, cast

from _omp import ArtifactUrl, BlobRef, EnvPath, OmpError

from ._errors import NotWiredError
from ._verdicts import ArtifactLifetime, ArtifactRef
from .journal import EntryId


class ArtifactError(OmpError):
    """Base error for artifact storage and retention operations."""


class ArtifactNotFound(ArtifactError):
    """An artifact or adopted blob is absent or not visible."""


class ArtifactCorrupt(ArtifactError):
    """Stored artifact metadata disagrees with its durable reference."""


class ArtifactNotText(ArtifactError):
    """A text read was requested for a non-text artifact."""


class ArtifactReader(Protocol):
    """Asynchronously read and seek through artifact bytes."""

    async def read(self, n: int = -1) -> bytes:
        """Read at most ``n`` bytes, or all remaining bytes when negative."""
        ...

    async def seek(self, offset: int) -> int:
        """Seek to an absolute byte offset and return the new position."""
        ...

    def __aiter__(self) -> AsyncIterator[bytes]:
        """Iterate the artifact as bounded byte chunks."""
        ...


class ArtifactWriter(Protocol):
    """Atomically stream bytes into a newly minted artifact."""

    @property
    def ref(self) -> ArtifactRef:
        """Return the minted reference after the writer closes successfully."""
        ...

    async def write(self, chunk: bytes | str) -> None:
        """Append one bytes or text chunk to the staged artifact."""
        ...

    async def __aenter__(self) -> ArtifactWriter:
        """Enter the streaming write transaction."""
        ...

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: object | None,
    ) -> bool | None:
        """Commit on success or discard the staged artifact on failure."""
        ...


@dataclass(frozen=True, slots=True)
class ArtifactStat:
    """Describe one artifact without reading its stored bytes."""

    ref: ArtifactRef
    url: ArtifactUrl
    media_type: str
    byte_len: int
    description: str | None
    lifetime: ArtifactLifetime
    created_ms: int
    source: str
    reachable_from: Sequence[EntryId]
    lines: int | None


async def _request(operation: str, /, **arguments: object) -> Any:
    from . import _control_backend, _control_request

    if _control_backend.get() is None:
        raise NotWiredError(operation)
    return await _control_request(operation, **arguments)


async def put(
    data: bytes | str | EnvPath,
    *,
    media_type: str,
    description: str | None = None,
    lifetime: ArtifactLifetime = ArtifactLifetime.SESSION,
) -> ArtifactRef:
    """Store bytes or text and return its session-addressable reference."""

    return cast(
        ArtifactRef,
        await _request(
            "omp.artifacts.put",
            data=data,
            media_type=media_type,
            description=description,
            lifetime=lifetime,
        ),
    )


async def open_write(
    *,
    media_type: str,
    description: str | None = None,
    lifetime: ArtifactLifetime = ArtifactLifetime.SESSION,
) -> ArtifactWriter:
    """Open an atomic streaming artifact writer through the host."""

    return cast(
        ArtifactWriter,
        await _request(
            "omp.artifacts.open_write",
            media_type=media_type,
            description=description,
            lifetime=lifetime,
        ),
    )


async def adopt(
    blob: BlobRef,
    *,
    media_type: str | None = None,
    description: str | None = None,
    lifetime: ArtifactLifetime = ArtifactLifetime.SESSION,
) -> ArtifactRef:
    """Promote an Environment blob into the artifact namespace."""

    return cast(
        ArtifactRef,
        await _request(
            "omp.artifacts.adopt",
            blob=blob,
            media_type=media_type,
            description=description,
            lifetime=lifetime,
        ),
    )


async def get(ref: ArtifactRef) -> bytes:
    """Read and verify an artifact's complete byte contents."""

    return cast(bytes, await _request("omp.artifacts.get", ref=ref))


async def open(ref: ArtifactRef) -> ArtifactReader:
    """Open a streaming byte reader for an artifact."""

    return cast(ArtifactReader, await _request("omp.artifacts.open", ref=ref))


async def read(ref: ArtifactRef, selector: str | None = None) -> str:
    """Read a text artifact using the shared selector grammar."""

    return cast(str, await _request("omp.artifacts.read", ref=ref, selector=selector))


async def stat(ref: ArtifactRef) -> ArtifactStat:
    """Read artifact metadata without fetching its bytes."""

    return cast(ArtifactStat, await _request("omp.artifacts.stat", ref=ref))


async def list(
    *, session: str | None = None, mine: bool = True, limit: int = 200
) -> Sequence[ArtifactStat]:
    """List artifacts reachable from a session journal."""

    return cast(
        Sequence[ArtifactStat],
        await _request(
            "omp.artifacts.list", session=session, mine=mine, limit=limit
        ),
    )


async def pin(ref: ArtifactRef, lifetime: ArtifactLifetime) -> None:
    """Raise an artifact's minimum retention promise."""

    await _request("omp.artifacts.pin", ref=ref, lifetime=lifetime)


def url(ref: ArtifactRef) -> ArtifactUrl:
    """Return the typed ``artifact://`` address for a reference."""

    if not isinstance(ref, ArtifactRef):
        raise TypeError("artifacts.url requires an ArtifactRef")
    return ref.url


__all__ = (
    "ArtifactCorrupt",
    "ArtifactError",
    "ArtifactNotFound",
    "ArtifactNotText",
    "ArtifactReader",
    "ArtifactStat",
    "ArtifactWriter",
    "adopt",
    "get",
    "list",
    "open",
    "open_write",
    "pin",
    "put",
    "read",
    "stat",
    "url",
)
