"""Typed artifact references and host-backed artifact operations."""

from __future__ import annotations

from collections.abc import AsyncIterator, Mapping, Sequence
from dataclasses import dataclass
from typing import Any, Protocol

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


def _wire_ref(ref: ArtifactRef) -> dict[str, object]:
    if not isinstance(ref, ArtifactRef):
        raise TypeError("artifact reference must be an ArtifactRef")
    return {
        "id": ref.id,
        "hash": ref.hash,
        "media_type": ref.media_type,
        "byte_len": ref.byte_len,
    }


def _wire_blob(blob: BlobRef) -> dict[str, object]:
    if not isinstance(blob, BlobRef):
        raise TypeError("blob must be a BlobRef")
    return {"hash": blob.hex, "size": blob.size}


def _decode_ref(value: object) -> ArtifactRef:
    if isinstance(value, ArtifactRef):
        return value
    if not isinstance(value, Mapping):
        raise ArtifactCorrupt("artifact reference response must be a mapping")
    try:
        ref = ArtifactRef(
            id=str(value["id"]),
            hash=str(value["hash"]),
            media_type=str(value["media_type"]),
            byte_len=int(value["byte_len"]),
        )
        bytes.fromhex(ref.hash)
    except (KeyError, TypeError, ValueError) as error:
        raise ArtifactCorrupt("artifact reference response is malformed") from error
    if len(ref.hash) != 64 or ref.byte_len < 0:
        raise ArtifactCorrupt("artifact reference response is malformed")
    return ref


def _decode_entry_id(value: object) -> EntryId:
    if isinstance(value, EntryId):
        return value
    if isinstance(value, str):
        return EntryId.parse(value)
    if isinstance(value, Mapping):
        try:
            return EntryId(session=str(value["session"]), index=int(value["index"]))
        except (KeyError, TypeError, ValueError) as error:
            raise ArtifactCorrupt("artifact reachability row is malformed") from error
    raise ArtifactCorrupt("artifact reachability row is malformed")


def _decode_stat(value: object) -> ArtifactStat:
    if isinstance(value, ArtifactStat):
        return value
    if not isinstance(value, Mapping):
        raise ArtifactCorrupt("artifact stat response must be a mapping")
    try:
        ref = _decode_ref(value["ref"])
        raw_url = value.get("url", str(ref.url))
        reachable = value.get("reachable_from", ())
        if not isinstance(reachable, Sequence) or isinstance(
            reachable, (str, bytes, bytearray)
        ):
            raise TypeError("reachable_from must be a sequence")
        return ArtifactStat(
            ref=ref,
            url=raw_url if isinstance(raw_url, ArtifactUrl) else ArtifactUrl(str(raw_url)),
            media_type=str(value.get("media_type", ref.media_type)),
            byte_len=int(value.get("byte_len", ref.byte_len)),
            description=(
                None if value.get("description") is None else str(value["description"])
            ),
            lifetime=ArtifactLifetime(str(value["lifetime"])),
            created_ms=int(value["created_ms"]),
            source=str(value["source"]),
            reachable_from=tuple(_decode_entry_id(row) for row in reachable),
            lines=None if value.get("lines") is None else int(value["lines"]),
        )
    except (KeyError, TypeError, ValueError) as error:
        if isinstance(error, ArtifactCorrupt):
            raise
        raise ArtifactCorrupt("artifact stat response is malformed") from error


def _blob_for(ref: ArtifactRef) -> BlobRef:
    try:
        return BlobRef(bytes.fromhex(ref.hash), ref.byte_len)
    except (TypeError, ValueError) as error:
        raise ArtifactCorrupt("artifact reference has an invalid content identity") from error


async def _request(operation: str, /, **arguments: object) -> Any:
    from . import _control_backend, _control_request

    if _control_backend.get() is None:
        raise NotWiredError(operation)
    return await _control_request(operation, **arguments)


class _DataArtifactReader:
    __slots__ = ("_blob", "_offset", "_size")

    def __init__(self, ref: ArtifactRef) -> None:
        self._blob = _blob_for(ref)
        self._offset = 0
        self._size = ref.byte_len

    async def read(self, n: int = -1) -> bytes:
        if isinstance(n, bool) or not isinstance(n, int):
            raise TypeError("artifact read size must be an integer")
        if n == 0 or self._offset >= self._size:
            return b""
        length = None if n < 0 else min(n, self._size - self._offset)
        from .env import blobs

        chunk = await blobs.get(self._blob, offset=self._offset, length=length)
        self._offset += len(chunk)
        if self._offset > self._size:
            raise ArtifactCorrupt("artifact stream exceeded its durable length")
        return chunk

    async def seek(self, offset: int) -> int:
        if isinstance(offset, bool) or not isinstance(offset, int):
            raise TypeError("artifact offset must be an integer")
        if offset < 0 or offset > self._size:
            raise ValueError("artifact offset is outside the stored byte range")
        self._offset = offset
        return offset

    async def _chunks(self) -> AsyncIterator[bytes]:
        from .env import blobs

        async for chunk in blobs.stream(self._blob, offset=self._offset):
            if not isinstance(chunk, bytes):
                raise ArtifactCorrupt("artifact stream returned a non-bytes chunk")
            self._offset += len(chunk)
            if self._offset > self._size:
                raise ArtifactCorrupt("artifact stream exceeded its durable length")
            yield chunk
        if self._offset != self._size:
            raise ArtifactCorrupt("artifact stream ended before its durable length")

    def __aiter__(self) -> AsyncIterator[bytes]:
        return self._chunks()


class _DataArtifactWriter:
    __slots__ = ("_description", "_lifetime", "_media_type", "_ref", "_writer")

    def __init__(
        self,
        media_type: str,
        description: str | None,
        lifetime: ArtifactLifetime,
    ) -> None:
        self._media_type = media_type
        self._description = description
        self._lifetime = lifetime
        self._ref: ArtifactRef | None = None
        self._writer: object | None = None

    @property
    def ref(self) -> ArtifactRef:
        if self._ref is None:
            raise RuntimeError("artifact writer has not committed")
        return self._ref

    async def write(self, chunk: bytes | str) -> None:
        writer = self._writer
        if writer is None:
            raise RuntimeError("artifact writer is not open")
        if isinstance(chunk, str):
            chunk = chunk.encode("utf-8")
        elif not isinstance(chunk, bytes):
            raise TypeError("artifact chunks must be bytes or str")
        await writer.write(chunk)

    async def __aenter__(self) -> _DataArtifactWriter:
        if self._writer is not None:
            raise RuntimeError("artifact writer cannot be entered twice")
        from .env import blobs

        writer = blobs.writer()
        self._writer = await writer.__aenter__()
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        traceback: object | None,
    ) -> bool | None:
        writer = self._writer
        if writer is None:
            raise RuntimeError("artifact writer is not open")
        if exc_type is not None:
            await writer.__aexit__(exc_type, exc, traceback)
            return None
        try:
            blob = await writer.commit()
            self._ref = await adopt(
                blob,
                media_type=self._media_type,
                description=self._description,
                lifetime=self._lifetime,
            )
        finally:
            await writer.__aexit__(exc_type, exc, traceback)
        return None


async def put(
    data: bytes | str | EnvPath,
    *,
    media_type: str,
    description: str | None = None,
    lifetime: ArtifactLifetime = ArtifactLifetime.SESSION,
) -> ArtifactRef:
    """Store bytes or text and return its session-addressable reference."""

    if isinstance(data, str):
        data = data.encode("utf-8")
    elif not isinstance(data, (bytes, EnvPath)):
        raise TypeError("artifact data must be bytes, str, or EnvPath")
    from .env import blobs

    blob = await blobs.put(data)
    return await adopt(
        blob,
        media_type=media_type,
        description=description,
        lifetime=lifetime,
    )


async def open_write(
    *,
    media_type: str,
    description: str | None = None,
    lifetime: ArtifactLifetime = ArtifactLifetime.SESSION,
) -> ArtifactWriter:
    """Open an atomic streaming artifact writer through the DATA plane."""

    if not isinstance(media_type, str) or not media_type:
        raise ValueError("media_type must be a non-empty string")
    if description is not None and not isinstance(description, str):
        raise TypeError("description must be a string or None")
    if not isinstance(lifetime, ArtifactLifetime):
        raise TypeError("lifetime must be an ArtifactLifetime")
    return _DataArtifactWriter(media_type, description, lifetime)


async def adopt(
    blob: BlobRef,
    *,
    media_type: str | None = None,
    description: str | None = None,
    lifetime: ArtifactLifetime = ArtifactLifetime.SESSION,
) -> ArtifactRef:
    """Promote an Environment blob into the artifact namespace."""

    if media_type is not None and (not isinstance(media_type, str) or not media_type):
        raise ValueError("media_type must be a non-empty string or None")
    if description is not None and not isinstance(description, str):
        raise TypeError("description must be a string or None")
    if not isinstance(lifetime, ArtifactLifetime):
        raise TypeError("lifetime must be an ArtifactLifetime")
    return _decode_ref(
        await _request(
            "omp.artifacts.adopt",
            blob=_wire_blob(blob),
            media_type=media_type,
            description=description,
            lifetime=lifetime.value,
        )
    )


async def _get_checked(ref: ArtifactRef) -> bytes:
    from .env import blobs

    data = await blobs.get(_blob_for(ref))
    if len(data) != ref.byte_len:
        raise ArtifactCorrupt("stored artifact length disagrees with its durable reference")
    return data


async def get(ref: ArtifactRef) -> bytes:
    """Read and verify an artifact's complete byte contents."""

    return await _get_checked((await stat(ref)).ref)


async def open(ref: ArtifactRef) -> ArtifactReader:
    """Open a streaming byte reader for an artifact."""

    return _DataArtifactReader((await stat(ref)).ref)


def _textual(media_type: str) -> bool:
    base = media_type.partition(";")[0].strip().lower()
    return (
        base.startswith("text/")
        or base in {"application/json", "application/xml", "application/yaml"}
        or base.endswith(("+json", "+xml", "+yaml"))
    )


def _select_lines(text: str, selector: str | None) -> str:
    if selector is None:
        return text
    from .urls import parse_selector

    parsed = parse_selector(selector)
    if parsed.conflicts:
        lines = text.splitlines(keepends=True)
        selected: list[str] = []
        start: int | None = None
        for index, line in enumerate(lines):
            if line.startswith("<<<<<<< "):
                start = index
            elif start is not None and line.startswith(">>>>>>> "):
                selected.append("".join(lines[start : index + 1]))
                start = None
        return "".join(selected)
    if not parsed.ranges:
        return text
    lines = text.splitlines(keepends=True)
    selected: list[str] = []
    for first, last in parsed.ranges:
        upper = len(lines) if last is None else min(last, len(lines))
        for index in range(first - 1, upper):
            line = lines[index]
            selected.append(line if parsed.raw else f"{index + 1}|{line}")
    return "".join(selected)


async def read(ref: ArtifactRef, selector: str | None = None) -> str:
    """Read a text artifact using the shared selector grammar."""

    metadata = await stat(ref)
    if not _textual(metadata.media_type):
        raise ArtifactNotText(
            f"artifact media type {metadata.media_type!r} is not textual"
        )
    data = await _get_checked(metadata.ref)
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ArtifactCorrupt("text artifact is not valid UTF-8") from error
    return _select_lines(text, selector)


async def stat(ref: ArtifactRef) -> ArtifactStat:
    """Read artifact metadata without fetching its bytes."""

    return _decode_stat(
        await _request("omp.artifacts.stat", ref=_wire_ref(ref))
    )


async def list(
    *, session: str | None = None, mine: bool = True, limit: int = 200
) -> Sequence[ArtifactStat]:
    """List artifacts reachable from a session journal."""

    rows = await _request(
        "omp.artifacts.list", session=session, mine=mine, limit=limit
    )
    if not isinstance(rows, Sequence) or isinstance(rows, (str, bytes, bytearray)):
        raise ArtifactCorrupt("artifact list response must be a sequence")
    return tuple(_decode_stat(row) for row in rows)


async def pin(ref: ArtifactRef, lifetime: ArtifactLifetime) -> None:
    """Raise an artifact's minimum retention promise."""

    if not isinstance(lifetime, ArtifactLifetime):
        raise TypeError("lifetime must be an ArtifactLifetime")
    await _request(
        "omp.artifacts.pin", ref=_wire_ref(ref), lifetime=lifetime.value
    )


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
