"""Typed, awaitable Environment DATA-plane surface.

Importing this module only constructs immutable values and namespace objects.  A
host installs the scoped backend after its DATA handshake; no import opens a
file, socket, process, or event loop.
"""

from __future__ import annotations

import asyncio
import contextvars
import inspect
import json
from collections.abc import AsyncIterator, Iterable, Mapping
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from types import MappingProxyType
from typing import Any

from _omp import (
    BlobRef,
    ClientPath,
    Duration,
    EnvPath,
    EnvUnavailable,
    PlacementError,
    _read_bytes_blocking,
)

from . import EffectsNotAuthorized, Fault, OmpError, StaleGeneration
from ._errors import NotWiredError
from .placement import Restart


class Capability(StrEnum):
    """A manifest-facing capability enforced by the Environment."""

    DOC_READ = "env.doc.read"
    DOC_WRITE = "env.doc.write"
    FS_READ = "env.fs.read"
    FS_WRITE = "env.fs.write"
    EXEC = "env.exec"
    PROCESS = "env.process"
    BLOB = "env.blob"
    SEARCH = "env.search"
    LSP = "env.lsp"
    NET = "env.net"
    WORKSPACE_SNAPSHOT = "env.workspace.snapshot"
    WORKTREE = "env.worktree"


@dataclass(frozen=True, slots=True)
class _EnvironmentFault(Fault):
    kind: str
    message: str
    capability: str | None = None


class EnvError(OmpError):
    """Base for attempted Environment operations that returned a typed fault."""

    def __init__(
        self,
        message: str,
        *,
        fault: Fault | None = None,
        capability: Capability | None = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.capability = capability
        self.fault = fault if fault is not None else _EnvironmentFault(
            "env", message, capability.value if capability is not None else None
        )


class Denied(EnvError):
    """The scoped connection or sandbox denied the operation."""


class DirectFilesystemDenied(PermissionError, OmpError):
    """The trusted direct-filesystem escape was not declared and granted."""


class QuotaExceeded(EnvError):
    """A DATA-plane hard quota is exhausted."""

    def __init__(self, message: str, *, quota: str, limit: int, fault: Fault | None = None) -> None:
        super().__init__(message, fault=fault)
        self.quota = quota
        self.limit = limit


class NotFound(EnvError):
    """The requested Environment resource does not exist."""


class AlreadyExists(EnvError):
    """A destination exists and replacement was forbidden."""

class Conflict(EnvError):
    """A revisioned mutation could not be rebased."""

    def __init__(
        self,
        message: str,
        *,
        expected: Any = None,
        current: Any = None,
        ranges: Iterable[Any] = (),
        fault: Fault | None = None,
    ) -> None:
        super().__init__(message, fault=fault)
        self.expected = expected
        self.current = current
        self.ranges = tuple(ranges)


@dataclass(frozen=True, slots=True)
class EditConflictFault(Fault):
    """Durable conflict payload carrying both revisions and collided ranges."""

    expected: Revision
    current: Revision
    ranges: tuple[tuple[int, int], ...]

    def __post_init__(self) -> None:
        object.__setattr__(self, "ranges", tuple(self.ranges))


class Stale(EnvError):
    """A retained revision or host generation is stale."""


class PreconditionFailed(EnvError):
    """A non-revision precondition failed."""


class Unsupported(EnvError):
    """The Environment cannot implement this operation."""


class Invalid(EnvError):
    """An Environment operation argument is malformed."""


class Cancelled(EnvError):
    """The Environment cancelled an in-flight operation."""


class TimedOut(EnvError):
    """The invocation deadline elapsed while the operation was in flight."""


class Io(EnvError):
    """The Environment filesystem returned an unclassified I/O error."""

    def __init__(self, message: str, *, errno: int | None = None, fault: Fault | None = None) -> None:
        super().__init__(message, fault=fault)
        self.errno = errno


class Disconnected(EnvError):
    """The DATA transport closed permanently."""


class StreamLost(EnvError):
    """A correlated event stream lost continuity."""

    def __init__(self, message: str, *, skipped: int, reason: str, fault: Fault | None = None) -> None:
        super().__init__(message, fault=fault)
        self.skipped = skipped
        self.reason = reason


class Partial(EnvError):
    """A transaction failed after at least one edit became durable."""

    def __init__(
        self,
        message: str,
        *,
        committed: Iterable[Any],
        failed_index: int,
        fault: Fault | None = None,
    ) -> None:
        super().__init__(message, fault=fault)
        self.committed = tuple(committed)
        self.failed_index = failed_index


@dataclass(frozen=True, slots=True)
class EnvInfo:
    """Identity and capabilities cached from the Environment handshake."""

    workspace_id: bytes
    root: EnvPath
    server_epoch: bytes
    server_version: str
    server_build: str
    schema_rev: int
    capabilities: frozenset[Capability]
    remote: bool


@dataclass(frozen=True, slots=True)
class BlobStat:
    """Presence and stored size of a content digest."""

    present: bool
    size: int


class Follow(StrEnum):
    """Symbolic-link traversal policy for workspace walks."""

    NEVER = "never"
    ROOTS = "roots"
    ALWAYS = "always"


class Rank(StrEnum):
    """Workspace result ranking mode."""

    NONE = "none"
    PATH = "path"


class FileKind(StrEnum):
    """Discriminate the host-reported kind of a filesystem entry."""

    REGULAR_FILE = "regular_file"
    DIRECTORY = "directory"
    SYMLINK = "symlink"
    OTHER = "other"


@dataclass(frozen=True, slots=True)
class PathMeta:
    """Describe one filesystem entry without reading its contents."""

    path: EnvPath
    kind: FileKind
    byte_length: int
    read_only: bool | None = None
    executable: bool | None = None
    modified: float | None = None
    accessed: float | None = None
    created: float | None = None


@dataclass(frozen=True, slots=True)
class DirEntry:
    """One immediate directory child and its unfollowed metadata."""

    name: str
    meta: PathMeta


@dataclass(frozen=True, slots=True)
class SymlinkTarget:
    """Resolved lexical target of a symbolic-link entry."""

    target: EnvPath
    relative: bool


class LinkKind(StrEnum):
    """Host-facing symbolic-link target hint."""

    FILE = "file"
    DIRECTORY = "directory"


@dataclass(frozen=True, slots=True)
class CopyResult:
    """Receipt for a filesystem copy."""

    meta: PathMeta
    bytes_copied: int


@dataclass(frozen=True, slots=True)
class WorktreeInfo:
    """Describe the Environment worktree containing the current workspace."""

    id: str
    root: EnvPath
    base: str
    generation: int


async def worktree() -> WorktreeInfo | None:
    """Return current worktree topology through the capability-gated host arm."""
    raise NotWiredError("omp.env.worktree")


@dataclass(frozen=True, slots=True)
class Entry:
    """One workspace walk result."""

    path: EnvPath
    kind: str
    size: int | None = None
    mtime_ms: float | None = None
    depth: int = 0


@dataclass(frozen=True, slots=True)
class Match:
    """One workspace content-search match."""

    path: EnvPath
    line: int
    byte_offset: int
    line_bytes: bytes

@dataclass(frozen=True, slots=True)
class Revision:
    """An immutable content revision pinned by a document lease."""

    sequence: int
    content_hash: bytes

    @property
    def hex(self) -> str:
        """Return the lowercase content hash for logging."""
        return self.content_hash.hex()


@dataclass(frozen=True, slots=True)
class Edit:
    """A byte-range replacement in a base revision's coordinate space."""

    start: int
    end: int
    replacement: bytes


@dataclass(frozen=True, slots=True)
class EditResult:
    """The committed result of a revisioned document mutation."""

    revision: Revision
    previous: Revision
    rebased: bool
    formatted: bool
    changed_ranges: tuple[tuple[int, int], ...]
    previous_path: EnvPath | None


@dataclass(frozen=True, slots=True)
class EditPlan:
    """A resolved document mutation that has not been committed."""

    revision: Revision
    edits: tuple[Edit, ...]
    preview: str
    first_changed_line: int | None
    warnings: tuple[str, ...]


class OnStale(StrEnum):
    """Policy for a mutation whose base revision is no longer current."""

    FAIL = "fail"
    REBASE = "rebase"
    REPLACE = "replace"


class Format(StrEnum):
    """Language-server formatting policy for a document mutation."""

    OFF = "off"
    BEST_EFFORT = "best_effort"
    REQUIRED = "required"


class Overwrite(StrEnum):
    """Destination replacement policy for document and filesystem operations."""

    FAIL = "fail"
    REPLACE_FILE = "replace_file"
    REPLACE_EMPTY_DIR = "replace_empty_dir"


class Presence(StrEnum):
    """Whether a revisioned document path currently exists."""

    PRESENT = "present"
    MISSING = "missing"


class Kind(StrEnum):
    """Content kind of a pinned document revision."""

    TEXT = "text"
    BINARY = "binary"


class SummaryRender(StrEnum):
    """Rendering dialect for structural summaries."""

    HASHLINE = "hashline"
    NUMBERED = "numbered"
    PLAIN = "plain"


class SummaryReason(StrEnum):
    """Machine-readable reason a structural summary was unavailable."""

    BINARY = "binary"
    MISSING_DOCUMENT = "missing_document"
    TOO_LARGE = "too_large"
    TOO_MANY_LINES = "too_many_lines"
    BELOW_MINIMUM_LINES = "below_minimum_lines"
    PROSE_DISABLED = "prose_disabled"
    UNSUPPORTED_LANGUAGE = "unsupported_language"
    EMPTY = "empty"
    SYNTAX_ERROR = "syntax_error"
    NO_ELISIONS = "no_elisions"
    PARSER_FAILURE = "parser_failure"


@dataclass(frozen=True, slots=True)
class SummaryOptions:
    """Caller-controlled structural-summary thresholds and rendering."""

    min_body_lines: int = 2
    min_comment_lines: int = 4
    unfold_until_lines: int = 0
    unfold_limit_lines: int = 0
    prose: bool = False
    min_total_lines: int = 0
    render: SummaryRender = SummaryRender.HASHLINE
    language: str | None = None

    def __post_init__(self) -> None:
        for name in (
            "min_body_lines",
            "min_comment_lines",
            "unfold_until_lines",
            "unfold_limit_lines",
            "min_total_lines",
        ):
            value = getattr(self, name)
            if type(value) is not int:
                raise TypeError(f"{name} must be an int")
            if value < 0:
                raise ValueError(f"{name} must be non-negative")
        if not isinstance(self.prose, bool):
            raise TypeError("prose must be a bool")
        if not isinstance(self.render, SummaryRender):
            raise TypeError("render must be an omp.env.SummaryRender")
        if self.language is not None and (
            not isinstance(self.language, str) or not self.language
        ):
            raise ValueError("language must be a non-empty str or None")


@dataclass(frozen=True, slots=True)
class SummarySegment:
    """One kept or elided one-based inclusive summary range."""

    kept: bool
    start_line: int
    end_line: int
    text: str | None

    def __post_init__(self) -> None:
        if type(self.start_line) is not int or type(self.end_line) is not int:
            raise TypeError("summary segment coordinates must be ints")
        if self.start_line < 1 or self.end_line < self.start_line:
            raise ValueError("summary segment coordinates must be one-based and ordered")
        if self.kept and self.text is None:
            raise ValueError("a kept summary segment must carry text")
        if not self.kept and self.text is not None:
            raise ValueError("an elided summary segment cannot carry text")


@dataclass(frozen=True, slots=True)
class Summary:
    """Successful bounded structural summary."""

    language: str
    parsed: bool
    elided: bool
    total_lines: int
    segments: tuple[SummarySegment, ...]
    text: str
    display_text: str
    elided_ranges: tuple[tuple[int, int], ...]
    elided_lines: int

    def __post_init__(self) -> None:
        object.__setattr__(self, "segments", tuple(self.segments))
        object.__setattr__(
            self, "elided_ranges", tuple(tuple(bounds) for bounds in self.elided_ranges)
        )


@dataclass(frozen=True, slots=True)
class SummaryUnavailable:
    """Machine-readable structural-summary refusal."""

    reason: SummaryReason
    total_lines: int
    language: str
    parsed: bool


class DocEventKind(StrEnum):
    """Kind of committed or externally observed document change."""

    COMMITTED = "committed"
    EXTERNAL_CREATED = "external_created"
    EXTERNAL_MODIFIED = "external_modified"
    EXTERNAL_DELETED = "external_deleted"
    EXTERNAL_RENAMED = "external_renamed"
    WATCH_RESCANNED = "watch_rescanned"


@dataclass(frozen=True, slots=True)
class DocEvent:
    """One ordered change observed by a document lease."""

    sequence: int
    kind: DocEventKind
    revision: Revision
    previous_revision: Revision
    txn_id: bytes | None = None
    invalidated_txn_ids: tuple[bytes, ...] = ()
    previous_path: EnvPath | None = None


_binding: contextvars.ContextVar[tuple[Any, EnvInfo] | None] = contextvars.ContextVar(
    "omp_env_binding", default=None
)


@dataclass(frozen=True, slots=True)
class DirectFilesystemGrant:
    """Durable provenance for the exceptional direct-filesystem capability."""

    extension_id: str
    publisher: str
    capability_digest: str
    grant_id: str
    granted_at: str
    generation: int


_direct_filesystem_binding: contextvars.ContextVar[
    tuple[Any, DirectFilesystemGrant] | None
] = contextvars.ContextVar("omp_direct_filesystem_binding", default=None)


def _install_direct_filesystem_backend(
    backend: Any, grant: DirectFilesystemGrant | None
) -> None:
    """Install the distinct trusted escape backend after Core grant admission."""

    _direct_filesystem_binding.set(None if grant is None else (backend, grant))


class DirectFilesystem:
    """Declared trusted path escape, deliberately separate from Environment."""

    async def request(
        self,
        operation: str,
        path: str | Path,
        *,
        data: bytes | None = None,
    ) -> object:
        """Perform one audited direct operation through the escape CONTROL arm."""

        binding = _direct_filesystem_binding.get()
        if binding is None:
            raise DirectFilesystemDenied(
                "trusted.direct-filesystem is not declared and durably granted"
            )
        backend, grant = binding
        path = Path(path)
        if not path.is_absolute():
            raise ValueError("direct-filesystem paths must be absolute")
        if operation not in {"read", "write", "stat", "list", "mkdir", "remove"}:
            raise ValueError("unsupported direct-filesystem operation")
        if data is not None and (type(data) is not bytes or len(data) > 1_048_576):
            raise ValueError("direct-filesystem payload exceeds 1 MiB")
        request = getattr(backend, "direct_filesystem_request", None)
        if request is None:
            raise NotWiredError("omp.env.direct_filesystem")
        arguments = {
            "operation": operation,
            "path": str(path),
            "data": data,
            "grant": grant,
        }
        result = request(arguments)
        if inspect.isawaitable(result):
            result = await result
        return result

    def grant(self) -> DirectFilesystemGrant:
        """Return immutable durable grant provenance without performing I/O."""

        binding = _direct_filesystem_binding.get()
        if binding is None:
            raise DirectFilesystemDenied(
                "trusted.direct-filesystem is not declared and durably granted"
            )
        return binding[1]


direct_filesystem = DirectFilesystem()
"""The explicitly exceptional filesystem capability; never an Environment alias."""


def _install_backend(backend: Any, environment_info: EnvInfo) -> None:
    """Install one invocation-scoped backend in the active Python context."""
    _binding.set((backend, environment_info))


def _snapshot_backend() -> Any:
    binding = _binding.get()
    if binding is None:
        raise EnvUnavailable("no Environment DATA client is installed at this placement")
    return binding[0]


def info() -> EnvInfo:
    """Return the immutable Environment handshake receipt without I/O."""
    binding = _binding.get()
    if binding is None:
        raise EnvUnavailable("no Environment DATA client is installed at this placement")
    return binding[1]


def has(*caps: Capability) -> bool:
    """Return whether every capability is granted on this scoped connection."""
    binding = _binding.get()
    return binding is not None and all(cap in binding[1].capabilities for cap in caps)

def require(*caps: Capability) -> None:
    """Raise :class:`Denied` for the first missing capability."""

    for cap in caps:
        if not has(cap):
            raise Denied(f"Environment capability {cap.value!r} was not granted", capability=cap)

def _env_path(value: EnvPath, argument: str = "path") -> EnvPath:
    if not isinstance(value, EnvPath):
        if isinstance(value, ClientPath):
            raise TypeError(f"{argument} is a ClientPath; Environment APIs only accept EnvPath")
        raise TypeError(f"{argument} must be omp.EnvPath, not {type(value).__name__}")
    return value


async def _request(operation: str, /, **arguments: Any) -> Any:
    backend = _snapshot_backend()
    request = backend.request
    if inspect.iscoroutinefunction(request):
        return await request(operation, arguments)
    result = await asyncio.to_thread(request, operation, arguments)
    if inspect.isawaitable(result):
        return await result
    return result


_STREAM_END = object()


def _next_stream_item(iterator: Any) -> Any:
    return next(iterator, _STREAM_END)


async def _stream(operation: str, /, **arguments: Any) -> AsyncIterator[Any]:
    backend = _snapshot_backend()
    stream = backend.stream
    if inspect.iscoroutinefunction(stream):
        source = await stream(operation, arguments)
    else:
        source = await asyncio.to_thread(stream, operation, arguments)
    if hasattr(source, "__aiter__"):
        async for item in source:
            yield item
        return
    iterator = iter(source)
    while True:
        item = await asyncio.to_thread(_next_stream_item, iterator)
        if item is _STREAM_END:
            return
        yield item


async def _read_bytes(path: EnvPath) -> bytes:
    path = _env_path(path)
    binding = _binding.get()
    backend = None if binding is None else binding[0]
    if backend is None:
        value = await asyncio.to_thread(_read_bytes_blocking, path)
    else:
        value = await _request("omp.env.docs.read_bytes", path=path)
    if type(value) is not bytes:
        raise TypeError("Environment read_bytes backend must return bytes")
    return value


async def _read_text(path: EnvPath, encoding: str = "utf-8") -> str:
    return (await _read_bytes(path)).decode(encoding)


def _local_path(path: EnvPath) -> Path:
    backend = _snapshot_backend()
    local_path = getattr(backend, "local_path", None)
    if local_path is None:
        raise PlacementError("this body is not colocated with the Environment filesystem")
    value = local_path(_env_path(path))
    if value is None:
        raise PlacementError("the active sandbox scope does not cover this Environment path")
    return Path(value)


class Doc:
    """A revisioned document lease owned by the Environment."""

    __slots__ = ("_lease", "path", "revision", "uri")

    def __init__(
        self, lease: Any, path: EnvPath, revision: Revision | None = None
    ) -> None:
        self._lease = lease
        self.path = path
        self.revision: Revision | None = revision
        self.uri = path.uri

    async def read_bytes(self, *, revision: Any = None) -> bytes:
        """Read this lease at its head or an explicitly pinned revision."""
        return await _request("omp.env.docs.Doc.read_bytes", lease=self._lease, revision=revision)

    async def read(self, *, revision: Any = None, encoding: str = "utf-8") -> str:
        """Read and decode this lease."""
        return (await self.read_bytes(revision=revision)).decode(encoding)

    async def lines(self, start: int, end: int, *, revision: Any = None) -> list[str]:
        """Read a zero-based half-open line range."""
        return await _request(
            "omp.env.docs.Doc.lines", lease=self._lease, start=start, end=end, revision=revision
        )

    async def dry_run(
        self, ops: Any, *, format: Format = Format.OFF
    ) -> EditPlan:
        """Resolve a document mutation without committing it."""
        return await _request(
            "omp.env.docs.Doc.dry_run",
            lease=self._lease,
            ops=ops,
            format=format,
        )

    async def edit(self, edits: Iterable[Any], **options: Any) -> Any:
        """Commit ordered, non-overlapping edits against this lease."""
        return await _request("omp.env.docs.Doc.edit", lease=self._lease, edits=tuple(edits), **options)

    async def write(self, data: str | bytes, **options: Any) -> Any:
        """Replace the document contents revisionally."""
        return await _request("omp.env.docs.Doc.write", lease=self._lease, data=data, **options)

    async def hashline(self, patch: str, **options: Any) -> Any:
        """Apply one hashline patch through the document actor."""
        return await _request("omp.env.docs.Doc.hashline", lease=self._lease, patch=patch, **options)

    async def summary(
        self, options: SummaryOptions | None = None
    ) -> Summary | SummaryUnavailable:
        """Return a bounded structural summary at the current revision."""
        if options is not None and not isinstance(options, SummaryOptions):
            raise TypeError("options must be an omp.env.SummaryOptions or None")
        result = await _request(
            "omp.env.docs.Doc.summary", lease=self._lease, options=options
        )
        if isinstance(result, (Summary, SummaryUnavailable)):
            return result
        if not isinstance(result, Mapping):
            raise TypeError("summary backend returned an invalid value")
        values = dict(result)
        if "reason" in values:
            reason = values["reason"]
            if not isinstance(reason, SummaryReason):
                values["reason"] = SummaryReason(reason)
            return SummaryUnavailable(**values)
        values["segments"] = tuple(
            segment if isinstance(segment, SummarySegment) else SummarySegment(**segment)
            for segment in values["segments"]
        )
        return Summary(**values)

    async def refresh(self) -> Any:
        """Refresh and return the current committed revision."""
        self.revision = await _request("omp.env.docs.Doc.refresh", lease=self._lease)
        return self.revision

    async def close(self) -> None:
        """Close the lease idempotently."""
        lease, self._lease = self._lease, None
        if lease is not None:
            await _request("omp.env.docs.Doc.close", lease=lease)

    def events(self) -> AsyncIterator[DocEvent]:
        """Yield ordered document events until close or stream loss."""
        return _stream("omp.env.docs.Doc.events", lease=self._lease)

    async def __aenter__(self) -> Doc:
        return self

    async def __aexit__(self, *_exc: Any) -> None:
        await self.close()


class Txn:
    """Invocation-scoped ordered document transaction."""

    __slots__ = ("id", "_operations", "_committed")

    def __init__(self, txn_id: bytes | None = None) -> None:
        self.id = txn_id
        self._operations: list[tuple[str, dict[str, Any]]] = []
        self._committed = False

    def edit(self, doc: Doc, ops: Iterable[Any], **options: Any) -> None:
        """Queue a revisioned edit."""
        self._operations.append(("edit", {"lease": doc._lease, "ops": tuple(ops), **options}))

    def create(self, path: EnvPath, content: str | bytes, **options: Any) -> None:
        """Queue a document creation."""
        self._operations.append(
            ("create", {"path": _env_path(path), "content": content, **options})
        )

    def write(self, doc: Doc, content: str | bytes, **options: Any) -> None:
        """Queue a whole-document replacement."""
        self._operations.append(
            ("write", {"lease": doc._lease, "content": content, **options})
        )

    def move(self, doc: Doc, destination: EnvPath, **options: Any) -> None:
        """Queue a document move."""
        self._operations.append(
            ("move", {"lease": doc._lease, "destination": _env_path(destination), **options})
        )

    def delete(self, doc: Doc) -> None:
        """Queue a document deletion."""
        self._operations.append(("delete", {"lease": doc._lease}))

    async def commit(self) -> Any:
        """Commit once and return the retained terminal transaction outcome."""
        if self._committed:
            raise PreconditionFailed("a Txn handle can commit only once")
        self._committed = True
        return await _request(
            "omp.env.Txn.commit", txn_id=self.id, operations=tuple(self._operations)
        )

    async def __aenter__(self) -> Txn:
        return self

    async def __aexit__(self, exc_type: Any, _exc: Any, _tb: Any) -> None:
        if exc_type is None and not self._committed:
            await self.commit()


class _Docs:
    """Revisioned document lease operations."""

    async def open(self, path: EnvPath, *, language: str | None = None, create: bool = False) -> Doc:
        """Open a document and return a server-owned lease."""
        path = _env_path(path)
        result = await _request("omp.env.docs.open", path=path, language=language, create=create)
        if isinstance(result, Doc):
            return result
        if not isinstance(result, Mapping) or "lease" not in result:
            raise TypeError("document backend returned an invalid open receipt")
        return Doc(result["lease"], path, result.get("revision"))

    def transaction(self, *, txn_id: bytes | None = None) -> Txn:
        """Create an atomic document transaction handle."""
        return Txn(txn_id)


def _as_path_meta(value: Any) -> PathMeta:
    if isinstance(value, PathMeta):
        return value
    if isinstance(value, Mapping):
        values = dict(value)
        kind = values.get("kind")
        if not isinstance(kind, FileKind):
            values["kind"] = FileKind(kind)
        return PathMeta(**values)
    raise TypeError("filesystem backend returned invalid path metadata")


class _Fs:
    """Raw metadata and namespace operations over typed Environment paths."""

    async def stat(self, path: EnvPath) -> PathMeta:
        """Stat a path while following symbolic links."""
        return _as_path_meta(
            await _request("omp.env.fs.stat", path=_env_path(path))
        )

    async def lstat(self, path: EnvPath) -> PathMeta:
        """Stat a path without following its final symbolic link."""
        return _as_path_meta(
            await _request("omp.env.fs.lstat", path=_env_path(path))
        )

    async def list_dir(
        self, path: EnvPath, *, follow: bool = False
    ) -> list[DirEntry]:
        """List immediate children with unfollowed metadata by default."""
        values = await _request(
            "omp.env.fs.list_dir", path=_env_path(path), follow=follow
        )
        return [
            value
            if isinstance(value, DirEntry)
            else DirEntry(value["name"], _as_path_meta(value["meta"]))
            for value in values
        ]

    async def read_link(self, path: EnvPath) -> SymlinkTarget:
        """Read a symbolic-link target and whether its on-disk form was relative."""
        value = await _request("omp.env.fs.read_link", path=_env_path(path))
        if isinstance(value, SymlinkTarget):
            return value
        if isinstance(value, Mapping):
            return SymlinkTarget(**value)
        raise TypeError("filesystem backend returned an invalid symlink target")

    async def canonicalize(self, path: EnvPath) -> EnvPath:
        """Resolve a path in the Environment namespace."""
        value = await _request("omp.env.fs.canonicalize", path=_env_path(path))
        return _env_path(value, "canonical path")

    async def mkdir(
        self, path: EnvPath, *, parents: bool = False, exist_ok: bool = False
    ) -> PathMeta:
        """Create a directory and return its metadata."""
        return _as_path_meta(
            await _request(
                "omp.env.fs.mkdir",
                path=_env_path(path),
                parents=parents,
                exist_ok=exist_ok,
            )
        )

    async def remove(
        self,
        path: EnvPath,
        *,
        recursive: bool = False,
        revision: Revision | None = None,
    ) -> None:
        """Remove a path, optionally fenced by a document revision."""
        await _request(
            "omp.env.fs.remove",
            path=_env_path(path),
            recursive=recursive,
            revision=revision,
        )

    async def rename(
        self,
        src: EnvPath,
        dest: EnvPath,
        *,
        overwrite: Overwrite = Overwrite.FAIL,
        src_revision: Revision | None = None,
        dest_revision: Revision | None = None,
    ) -> PathMeta:
        """Rename a path inside the Environment namespace."""
        if not isinstance(overwrite, Overwrite):
            raise TypeError("overwrite must be an omp.env.Overwrite")
        return _as_path_meta(
            await _request(
                "omp.env.fs.rename",
                src=_env_path(src, "src"),
                dest=_env_path(dest, "dest"),
                overwrite=overwrite,
                src_revision=src_revision,
                dest_revision=dest_revision,
            )
        )

    async def copy(
        self,
        src: EnvPath,
        dest: EnvPath,
        *,
        follow: bool = True,
        overwrite: Overwrite = Overwrite.FAIL,
        dest_revision: Revision | None = None,
    ) -> CopyResult:
        """Copy one non-directory entry."""
        if not isinstance(overwrite, Overwrite):
            raise TypeError("overwrite must be an omp.env.Overwrite")
        value = await _request(
            "omp.env.fs.copy",
            src=_env_path(src, "src"),
            dest=_env_path(dest, "dest"),
            follow=follow,
            overwrite=overwrite,
            dest_revision=dest_revision,
        )
        if isinstance(value, CopyResult):
            return value
        if isinstance(value, Mapping):
            return CopyResult(_as_path_meta(value["meta"]), value["bytes_copied"])
        raise TypeError("filesystem backend returned an invalid copy receipt")

    async def symlink(
        self,
        target: EnvPath,
        link: EnvPath,
        *,
        kind: LinkKind = LinkKind.FILE,
        relative: bool = False,
        overwrite: Overwrite = Overwrite.FAIL,
    ) -> PathMeta:
        """Create a symbolic link without ambient path conversion."""
        if not isinstance(kind, LinkKind):
            raise TypeError("kind must be an omp.env.LinkKind")
        if not isinstance(overwrite, Overwrite):
            raise TypeError("overwrite must be an omp.env.Overwrite")
        return _as_path_meta(
            await _request(
                "omp.env.fs.symlink",
                target=_env_path(target, "target"),
                link=_env_path(link, "link"),
                kind=kind,
                relative=relative,
                overwrite=overwrite,
            )
        )

    async def hard_link(
        self,
        src: EnvPath,
        link: EnvPath,
        *,
        follow: bool = False,
        overwrite: Overwrite = Overwrite.FAIL,
    ) -> PathMeta:
        """Create a hard link without ambient path conversion."""
        if not isinstance(overwrite, Overwrite):
            raise TypeError("overwrite must be an omp.env.Overwrite")
        return _as_path_meta(
            await _request(
                "omp.env.fs.hard_link",
                src=_env_path(src, "src"),
                link=_env_path(link, "link"),
                follow=follow,
                overwrite=overwrite,
            )
        )

    async def chmod(
        self,
        path: EnvPath,
        *,
        read_only: bool | None = None,
        executable: bool | None = None,
        follow: bool = True,
        revision: Revision | None = None,
    ) -> PathMeta:
        """Update portable permission properties."""
        return _as_path_meta(
            await _request(
                "omp.env.fs.chmod",
                path=_env_path(path),
                read_only=read_only,
                executable=executable,
                follow=follow,
                revision=revision,
            )
        )


class SyncKind(StrEnum):
    """Negotiated LSP text-document synchronization mode."""

    NONE = "none"
    FULL = "full"
    INCREMENTAL = "incremental"


@dataclass(frozen=True, slots=True)
class SyncPolicy:
    """Resolved synchronization behavior for one document-server binding."""

    change: SyncKind
    open_close: bool
    will_save: bool
    will_save_wait_until: bool
    save: bool
    save_include_text: bool
    position_encoding: str

    def __post_init__(self) -> None:
        if not isinstance(self.change, SyncKind):
            raise TypeError("change must be an omp.env.SyncKind")
        if not self.position_encoding:
            raise ValueError("position_encoding must be non-empty")


@dataclass(frozen=True, slots=True)
class LspBinding:
    """One language server bound to a document."""

    server_id: bytes
    name: str
    sync: SyncPolicy
    capabilities: dict[str, Any]

    def __post_init__(self) -> None:
        if type(self.server_id) is not bytes:
            raise TypeError("server_id must be bytes")
        if not isinstance(self.sync, SyncPolicy):
            raise TypeError("sync must be an omp.env.SyncPolicy")
        object.__setattr__(self, "capabilities", dict(self.capabilities))


class LspStale(StrEnum):
    """Policy for a request whose pinned document revision moved."""

    FAIL = "fail"
    RETRY_HEAD = "retry_head"


class LspBindingEventKind(StrEnum):
    """Kind of LSP server binding transition."""

    READY = "ready"
    POLICY_CHANGED = "policy_changed"
    RESTARTED = "restarted"
    STOPPED = "stopped"


@dataclass(frozen=True, slots=True)
class LspEvent:
    """One server notification with its authoritative revision when known."""

    server_id: bytes
    method: str
    params: Any
    path: str | None
    revision: Revision | None


@dataclass(frozen=True, slots=True)
class LspBindingEvent:
    """One connection-wide server binding transition."""

    kind: LspBindingEventKind
    binding: LspBinding
    path: str | None


class LspFailure(EnvError):
    """JSON-RPC error returned by a selected language server."""

    def __init__(
        self,
        code: int,
        message: str,
        data: Any = None,
        *,
        fault: Fault | None = None,
    ) -> None:
        if type(code) is not int:
            raise TypeError("LspFailure code must be an int")
        super().__init__(message, fault=fault)
        self.code = code
        self.data = data


def _as_revision(value: Any) -> Revision | None:
    if value is None or isinstance(value, Revision):
        return value
    if isinstance(value, Mapping):
        return Revision(**value)
    raise TypeError("LSP backend returned an invalid revision")


def _as_sync_policy(value: Any) -> SyncPolicy:
    if isinstance(value, SyncPolicy):
        return value
    if isinstance(value, Mapping):
        values = dict(value)
        change = values.get("change")
        if not isinstance(change, SyncKind):
            values["change"] = SyncKind(change)
        return SyncPolicy(**values)
    raise TypeError("LSP backend returned an invalid sync policy")


def _as_lsp_binding(value: Any) -> LspBinding:
    if isinstance(value, LspBinding):
        return value
    if isinstance(value, Mapping):
        values = dict(value)
        values["sync"] = _as_sync_policy(values.pop("sync_policy", values.get("sync")))
        return LspBinding(**values)
    raise TypeError("LSP backend returned an invalid binding")


_lsp_last_revision: contextvars.ContextVar[Revision | None] = contextvars.ContextVar(
    "omp_env_lsp_last_revision", default=None
)


class _Lsp:
    """Revision-aware language-server multiplexing."""

    @property
    def last_revision(self) -> Revision | None:
        """Return the authoritative revision used by the latest request in this context."""
        return _lsp_last_revision.get()

    async def bindings(self, path: EnvPath) -> list[LspBinding]:
        """Return servers currently bound to a path."""
        values = await _request("omp.env.lsp.bindings", path=_env_path(path))
        return [_as_lsp_binding(value) for value in values]

    async def request(
        self,
        server: bytes,
        method: str,
        params: Any,
        *,
        doc: Doc | None = None,
        on_stale: LspStale = LspStale.RETRY_HEAD,
        timeout: Duration | None = None,
    ) -> Any:
        """Issue a revision-aware LSP request and retain the revision actually used."""
        if type(server) is not bytes:
            raise TypeError("server must be an LspBinding.server_id bytes value")
        if not isinstance(method, str) or not method:
            raise ValueError("method must be a non-empty str")
        if doc is not None and not isinstance(doc, Doc):
            raise TypeError("doc must be an omp.env.Doc or None")
        if not isinstance(on_stale, LspStale):
            raise TypeError("on_stale must be an omp.env.LspStale")
        result = await _request(
            "omp.env.lsp.request",
            server=server,
            method=method,
            params=params,
            doc=doc,
            on_stale=on_stale,
            timeout=timeout,
        )
        if isinstance(result, Mapping) and "revision" in result:
            revision = _as_revision(result["revision"])
            _lsp_last_revision.set(revision)
            error = result.get("error")
            if error is not None:
                if not isinstance(error, Mapping):
                    raise TypeError("LSP backend returned an invalid error")
                raise LspFailure(
                    error["code"], error["message"], error.get("data")
                )
            if "result" not in result:
                raise TypeError("LSP backend response omitted its result")
            return result["result"]
        _lsp_last_revision.set(None)
        return result

    async def notify(self, server: bytes, method: str, params: Any) -> None:
        """Issue an LSP notification."""
        if type(server) is not bytes:
            raise TypeError("server must be an LspBinding.server_id bytes value")
        if not isinstance(method, str) or not method:
            raise ValueError("method must be a non-empty str")
        await _request("omp.env.lsp.notify", server=server, method=method, params=params)

    async def _events(self) -> AsyncIterator[LspEvent | LspBindingEvent]:
        async for value in _stream("omp.env.lsp.events"):
            if isinstance(value, (LspEvent, LspBindingEvent)):
                yield value
                continue
            if not isinstance(value, Mapping):
                raise TypeError("LSP backend returned an invalid event")
            values = dict(value)
            if "method" in values:
                values["revision"] = _as_revision(values.get("revision"))
                yield LspEvent(**values)
            else:
                kind = values.get("kind")
                if not isinstance(kind, LspBindingEventKind):
                    values["kind"] = LspBindingEventKind(kind)
                values["binding"] = _as_lsp_binding(values["binding"])
                yield LspBindingEvent(**values)

    def events(self) -> AsyncIterator[LspEvent | LspBindingEvent]:
        """Yield typed LSP registry and server events."""
        return self._events()


class Run:
    """Guarded command handle and ordered async event stream."""

    __slots__ = ("id",)

    def __init__(self, run_id: bytes) -> None:
        self.id = run_id

    def __aiter__(self) -> AsyncIterator[Output | Exit]:
        return _run_events(self.id)

    async def wait(self) -> Completed:
        """Drain output and return the terminal completion receipt."""
        return _as_completed(await _request("omp.env.Run.wait", run=self.id))

    async def stdin(self, data: bytes) -> None:
        """Write bytes to stdin or the PTY master."""
        await _request("omp.env.Run.stdin", run=self.id, data=data)

    async def eof(self) -> None:
        """Close command stdin."""
        await _request("omp.env.Run.eof", run=self.id)

    async def signal(self, signal: str) -> None:
        """Signal the Environment-owned process group."""
        await _request("omp.env.Run.signal", run=self.id, signal=signal)

    async def resize(self, rows: int, columns: int) -> None:
        """Resize the command PTY."""
        await _request("omp.env.Run.resize", run=self.id, rows=rows, columns=columns)

    def cancel(self) -> None:
        """Request non-blocking structural command teardown."""
        _snapshot_backend().cancel_run(self.id)

    async def detach(self, name: str) -> None:
        """Relinquish the guard to an Environment-owned named job."""
        await _request("omp.env.Run.detach", run=self.id, name=name)


class Session:
    """Persistent server-owned shell session."""

    __slots__ = ("id", "cwd", "_closed")

    def __init__(self, session_id: bytes, cwd: EnvPath) -> None:
        self.id = session_id
        self.cwd = cwd
        self._closed = False

    async def run(self, script: str, **options: Any) -> Run:
        """Start one serialized command in this session."""
        result = await _request(
            "omp.env.Session.run", session=self.id, script=script, **options
        )
        if isinstance(result, Run):
            return result
        return Run(result["id"])

    async def close(self) -> None:
        """Close this session idempotently."""
        if not self._closed:
            self._closed = True
            await _request("omp.env.Session.close", session=self.id)

    async def __aenter__(self) -> Session:
        return self

    async def __aexit__(self, *_exc: Any) -> None:
        await self.close()


class Channel(StrEnum):
    """Output channel emitted by an Environment-owned process."""

    STDOUT = "stdout"
    STDERR = "stderr"
    PTY = "pty"


@dataclass(frozen=True, slots=True)
class Output:
    """One ordered output frame from an Environment-owned command."""

    channel: Channel
    data: bytes
    sequence: int


@dataclass(frozen=True, slots=True)
class Exit:
    """Terminal event for an Environment-owned command."""

    status: Completed


class Outcome(StrEnum):
    """Terminal outcome of an Environment-owned command."""

    EXITED = "exited"
    FAILED = "failed"
    TIMEOUT = "timeout"
    CANCELLED = "cancelled"
    DENIED = "denied"


@dataclass(frozen=True, slots=True)
class Completed:
    """Bounded terminal receipt for an Environment-owned command."""

    outcome: Outcome
    exit_code: int | None
    signal: str
    wall: Duration
    output: bytes
    artifact: BlobRef | None
    aborted: bool

    def text(self, channel: Channel | None = None) -> str:
        """Decode the bounded output lossily."""
        del channel
        return self.output.decode("utf-8", errors="replace")


def _as_completed(value: Any) -> Completed:
    if isinstance(value, Completed):
        return value
    if isinstance(value, Mapping):
        values = dict(value)
        outcome = values.get("outcome")
        if not isinstance(outcome, Outcome):
            values["outcome"] = Outcome(outcome)
        return Completed(**values)
    raise TypeError("exec backend returned an invalid completion receipt")


async def _run_events(run_id: bytes) -> AsyncIterator[Output | Exit]:
    async for value in _stream("omp.env.Run.events", run=run_id):
        if isinstance(value, (Output, Exit)):
            yield value
        elif isinstance(value, Mapping) and "status" in value:
            yield Exit(_as_completed(value["status"]))
        elif isinstance(value, Mapping):
            values = dict(value)
            channel = values.get("channel")
            if not isinstance(channel, Channel):
                values["channel"] = Channel(channel)
            yield Output(**values)
        else:
            raise TypeError("exec backend returned an invalid run event")


@dataclass(frozen=True, slots=True)
class RestartPolicy:
    """Automatic restart policy for a named process."""

    policy: Restart
    delay: Duration = Duration("500ms")
    max_restarts: int | None = None


@dataclass(frozen=True, slots=True)
class ReadyLog:
    """Readiness probe matching a regular expression against combined output."""

    pattern: str
    timeout: Duration = Duration("30s")


@dataclass(frozen=True, slots=True)
class ReadyTcp:
    """Readiness probe connecting to a TCP endpoint."""

    port: int
    host: str = "127.0.0.1"
    timeout: Duration = Duration("30s")


@dataclass(frozen=True, slots=True)
class ReadyPing:
    """Readiness probe requiring a matching toolhost Pong frame."""

    nonce: int = 1
    timeout: Duration = Duration("30s")


@dataclass(frozen=True, slots=True, init=False)
class ReadyAll:
    """Readiness group requiring every supplied probe to pass."""

    probes: tuple[ReadyLog | ReadyTcp | ReadyPing, ...]

    def __init__(self, *probes: ReadyLog | ReadyTcp | ReadyPing) -> None:
        for probe in probes:
            if not isinstance(probe, (ReadyLog, ReadyTcp, ReadyPing)):
                raise TypeError(
                    "ReadyAll probes must be ReadyLog, ReadyTcp, or ReadyPing values"
                )
        object.__setattr__(self, "probes", probes)


Ready = ReadyLog | ReadyTcp | ReadyPing | ReadyAll


class ProcState(StrEnum):
    """Observable state of one named-process generation."""

    STARTING = "starting"
    READY = "ready"
    RUNNING = "running"
    EXITED = "exited"
    STOPPED = "stopped"
    FAILED = "failed"


@dataclass(frozen=True, slots=True)
class ProcessInfo:
    """Immutable snapshot of one named-process generation."""

    name: str
    generation: int
    state: ProcState
    status: Completed


@dataclass(frozen=True, slots=True)
class ProcessOutput:
    """One ordered output frame from a named-process generation."""

    generation: int
    channel: Channel
    data: bytes
    sequence: int


class Lifecycle(StrEnum):
    """Named-process lifecycle target used by wait operations."""

    READY = "ready"
    EXIT = "exit"


@dataclass(frozen=True, slots=True)
class HttpResponse:
    """Immutable response returned by scoped Environment HTTP egress.

    ``final_url`` identifies the URL that produced this response after any
    permitted redirect hops.
    """

    status: int
    headers: Mapping[str, str]
    body: bytes
    final_url: str

    def __post_init__(self) -> None:
        object.__setattr__(self, "headers", MappingProxyType(dict(self.headers)))
        if type(self.body) is not bytes:
            raise TypeError("HttpResponse.body must be bytes")
        if type(self.final_url) is not str:
            raise TypeError("HttpResponse.final_url must be a str")

    def json(self) -> Any:
        """Decode the response body as JSON."""
        return json.loads(self.body)


async def _http_request(
    public_name: str,
    operation: str,
    method: str,
    url: str,
    *,
    body: bytes,
    headers: Mapping[str, str],
    timeout: Duration | None,
    redirects: int,
) -> HttpResponse:
    if type(redirects) is not int:
        raise TypeError("redirects must be an int")
    if not 0 <= redirects <= 10:
        raise ValueError("redirects must be between 0 and 10")
    if _binding.get() is None:
        raise NotWiredError(public_name)
    if type(body) is not bytes:
        raise TypeError("HTTP request body must be bytes")
    result = await _request(
        operation,
        method=method,
        url=url,
        body=body,
        headers=headers,
        timeout=timeout,
        redirects=redirects,
    )
    return result if isinstance(result, HttpResponse) else HttpResponse(**result)


async def http_get(
    url: str,
    *,
    timeout: Duration | None = None,
    headers: Mapping[str, str] = MappingProxyType({}),
    redirects: int = 10,
) -> HttpResponse:
    """Request one URL with GET through scoped Environment HTTP egress.

    ``redirects`` is the maximum number of redirect hops, from 0 through 10.
    Zero returns the first redirect response without following it.
    """
    return await _http_request(
        "omp.env.http_get",
        "omp.env.http.get",
        "GET",
        url,
        body=b"",
        headers=headers,
        timeout=timeout,
        redirects=redirects,
    )


async def http_post(
    url: str,
    *,
    body: bytes = b"",
    headers: Mapping[str, str] = MappingProxyType({}),
    timeout: Duration | None = None,
    redirects: int = 10,
) -> HttpResponse:
    """Request one URL with POST through scoped Environment HTTP egress.

    ``redirects`` is the maximum number of redirect hops, from 0 through 10.
    Zero returns the first redirect response without following it.
    """
    return await _http_request(
        "omp.env.http_post",
        "omp.env.http.post",
        "POST",
        url,
        body=body,
        headers=headers,
        timeout=timeout,
        redirects=redirects,
    )


async def http_put(
    url: str,
    *,
    body: bytes = b"",
    headers: Mapping[str, str] = MappingProxyType({}),
    timeout: Duration | None = None,
    redirects: int = 10,
) -> HttpResponse:
    """Request one URL with PUT through scoped Environment HTTP egress.

    ``redirects`` is the maximum number of redirect hops, from 0 through 10.
    Zero returns the first redirect response without following it.
    """
    return await _http_request(
        "omp.env.http_put",
        "omp.env.http.put",
        "PUT",
        url,
        body=body,
        headers=headers,
        timeout=timeout,
        redirects=redirects,
    )


@dataclass(frozen=True, slots=True)
class Pty:
    """Configure terminal dimensions and emulation for an exec process."""

    rows: int = 24
    columns: int = 80
    terminal: str = "xterm-256color"


class Process:
    """Stable named-process generation handle."""

    __slots__ = ("name", "generation")

    def __init__(self, name: str, generation: int) -> None:
        self.name = name
        self.generation = generation

    @property
    def endpoint(self) -> str:
        """Return the generation-fenced loopback or Unix endpoint."""
        raise NotWiredError("omp.env.Process.endpoint")


    async def info(self) -> ProcessInfo:
        """Return the current generation snapshot."""
        return await _request(
            "omp.env.Process.info", name=self.name, generation=self.generation
        )

    def output(self, *, after: int = 0) -> AsyncIterator[ProcessOutput]:
        """Yield retained and live ordered process output."""
        return _stream(
            "omp.env.Process.output",
            name=self.name,
            generation=self.generation,
            after=after,
        )

    def states(self) -> AsyncIterator[ProcessInfo]:
        """Yield named-process lifecycle transitions."""
        return _stream(
            "omp.env.Process.states", name=self.name, generation=self.generation
        )

    async def send(self, data: bytes) -> None:
        """Send bytes to process stdin."""
        await _request(
            "omp.env.Process.send",
            name=self.name,
            generation=self.generation,
            data=data,
        )

    async def send_secret(self, name: str, value: str) -> None:
        """Inject a scoped secret without exposing it through argv or environment."""
        await _request(
            "omp.env.Process.send_secret",
            name=self.name,
            generation=self.generation,
            secret_name=name,
            value=value,
        )

    async def signal(self, signal: str) -> None:
        """Signal the Environment-owned process group."""
        await _request(
            "omp.env.Process.signal",
            name=self.name,
            generation=self.generation,
            signal=signal,
        )

    async def stop(self, **options: Any) -> ProcessInfo:
        """Stop the process tree and return its terminal state."""
        return await _request(
            "omp.env.Process.stop",
            name=self.name,
            generation=self.generation,
            **options,
        )

    async def restart(self) -> Process:
        """Restart from the retained launch spec and return the next generation."""
        if _binding.get() is None:
            raise NotWiredError("omp.env.Process.restart")
        result = await _request(
            "omp.env.Process.restart", name=self.name, generation=self.generation
        )
        return (
            result
            if isinstance(result, Process)
            else Process(result["name"], result["generation"])
        )


class BlobWriter:
    """Incremental Environment blob upload handle."""

    __slots__ = ("_upload", "_committed")

    def __init__(self, upload: Any) -> None:
        self._upload = upload
        self._committed = False

    async def write(self, chunk: bytes) -> None:
        """Append one ordered byte chunk."""
        await _request("omp.env.BlobWriter.write", upload=self._upload, chunk=chunk)

    async def commit(self) -> BlobRef:
        """Commit staged chunks and return their content identity."""
        self._committed = True
        return await _request("omp.env.BlobWriter.commit", upload=self._upload)

    def abort(self) -> None:
        """Abandon staged chunks without making content visible."""
        _snapshot_backend().abort_blob(self._upload)

    async def __aenter__(self) -> BlobWriter:
        return self

    async def __aexit__(self, exc_type: Any, _exc: Any, _tb: Any) -> None:
        if exc_type is not None or not self._committed:
            self.abort()


class _Sh:
    """Guarded Environment command execution."""

    def session(self, **options: Any) -> Session:
        """Create a persistent, server-owned exec session handle."""
        cwd = options.get("cwd")
        if cwd is not None:
            options["cwd"] = _env_path(cwd, "cwd")
        result = _snapshot_backend().session(options)
        if isinstance(result, Session):
            return result
        return Session(result["id"], result["cwd"])

    async def run(self, script: str, **options: Any) -> Completed:
        """Run a command and collect its bounded completion receipt."""
        if not isinstance(script, str) or not script:
            raise ValueError("script must be a non-empty str")
        cwd = options.get("cwd")
        if cwd is not None:
            options["cwd"] = _env_path(cwd, "cwd")
        return _as_completed(
            await _request("omp.env.sh.run", script=script, **options)
        )

    def parse(self, script: str) -> Any:
        """Parse a script without executing it or performing I/O."""
        backend = _snapshot_backend()
        return backend.parse_script(script)


class _Proc:
    """Server-owned named process operations."""

    async def start(self, name: str, script: str, **options: Any) -> Process:
        """Start a named process."""
        cwd = options.get("cwd")
        if cwd is not None:
            options["cwd"] = _env_path(cwd, "cwd")
        result = await _request("omp.env.proc.start", name=name, script=script, **options)
        return result if isinstance(result, Process) else Process(result["name"], result["generation"])

    async def adopt(self, name: str) -> Process | None:
        """Adopt a live named process if present."""
        result = await _request("omp.env.proc.adopt", name=name)
        if result is None or isinstance(result, Process):
            return result
        return Process(result["name"], result["generation"])

    async def ensure(
        self,
        name: str,
        script: str,
        *,
        cwd: EnvPath | None = None,
        env: Mapping[str, str] | None = None,
        pty: Pty | None = None,
        restart: RestartPolicy | None = None,
        ready: Ready | None = None,
    ) -> Process:
        """Adopt a matching process or start it atomically."""
        if cwd is not None:
            cwd = _env_path(cwd, "cwd")
        if restart is not None and not isinstance(restart, RestartPolicy):
            raise TypeError("restart must be an omp.env.RestartPolicy or None")
        if ready is not None and not isinstance(
            ready, (ReadyLog, ReadyTcp, ReadyPing, ReadyAll)
        ):
            raise TypeError("ready must be an omp.env.Ready value or None")
        result = await _request(
            "omp.env.proc.ensure",
            name=name,
            script=script,
            cwd=cwd,
            env=env,
            pty=pty,
            restart=restart,
            ready=ready,
        )
        return (
            result
            if isinstance(result, Process)
            else Process(result["name"], result["generation"])
        )

    async def list(self) -> list[ProcessInfo]:
        """List named processes visible to this connection."""
        return await _request("omp.env.proc.list")


class _Blobs:
    """Streaming content-addressed Environment storage."""

    async def put(self, data: Any) -> BlobRef:
        """Store bytes or an iterable/async iterable of byte chunks."""
        return await _request("omp.env.blobs.put", data=data)

    def writer(self) -> BlobWriter:
        """Create an incremental blob upload context manager."""
        result = _snapshot_backend().blob_writer()
        return result if isinstance(result, BlobWriter) else BlobWriter(result)

    async def get(self, ref: BlobRef, *, offset: int = 0, length: int | None = None) -> bytes:
        """Fetch a blob or byte range as one bytes object."""
        value = await _request("omp.env.blobs.get", ref=ref, offset=offset, length=length)
        if type(value) is not bytes:
            raise TypeError("blob backend must return bytes")
        return value

    def stream(
        self, ref: BlobRef, *, offset: int = 0, length: int | None = None
    ) -> AsyncIterator[bytes]:
        """Stream a blob without materializing the full payload."""
        return _stream("omp.env.blobs.stream", ref=ref, offset=offset, length=length)

    async def stat(self, ref: BlobRef) -> BlobStat:
        """Return blob presence and stored size."""
        result = await _request("omp.env.blobs.stat", ref=ref)
        return result if isinstance(result, BlobStat) else BlobStat(**result)

    async def delete(self, ref: BlobRef) -> bool:
        """Delete a blob and report whether it existed."""
        return await _request("omp.env.blobs.delete", ref=ref)


class _Find:
    """Cached, gitignore-aware workspace walking and search."""

    async def files(self, **options: Any) -> list[Entry]:
        """Return bounded workspace entries."""
        root = options.get("root")
        if root is not None:
            options["root"] = _env_path(root, "root")
        return await _request("omp.env.find.files", **options)

    def walk(self, **options: Any) -> AsyncIterator[Entry]:
        """Stream workspace entries lazily."""
        root = options.get("root")
        if root is not None:
            options["root"] = _env_path(root, "root")
        return _stream("omp.env.find.walk", **options)

    async def grep(self, pattern: str | bytes, **options: Any) -> list[Match]:
        """Search workspace contents under the server-side walker."""
        root = options.get("root")
        if root is not None:
            options["root"] = _env_path(root, "root")
        return await _request("omp.env.find.grep", pattern=pattern, **options)


docs = _Docs()
fs = _Fs()
lsp = _Lsp()
sh = _Sh()
proc = _Proc()
blobs = _Blobs()
find = _Find()


__all__ = (
    "AlreadyExists",
    "BlobStat",
    "BlobWriter",
    "Channel",
    "Completed",
    "Cancelled",
    "Capability",
    "Conflict",
    "CopyResult",
    "DirEntry",
    "DocEvent",
    "DocEventKind",
    "Denied",
    "DirectFilesystem",
    "DirectFilesystemDenied",
    "DirectFilesystemGrant",
    "Disconnected",
    "Doc",
    "EffectsNotAuthorized",
    "Entry",
    "EnvError",
    "EnvInfo",
    "Edit",
    "EditConflictFault",
    "EditPlan",
    "EditResult",
    "Exit",
    "FileKind",
    "Follow",
    "Format",
    "HttpResponse",
    "Invalid",
    "Io",
    "Kind",
    "LinkKind",
    "LspBinding",
    "LspBindingEvent",
    "LspBindingEventKind",
    "LspEvent",
    "LspFailure",
    "LspStale",
    "Match",
    "Lifecycle",
    "OnStale",
    "Overwrite",
    "PathMeta",
    "NotFound",
    "Partial",
    "Presence",
    "Outcome",
    "Output",
    "Process",
    "ProcessInfo",
    "ProcessOutput",
    "ProcState",
    "Pty",
    "PreconditionFailed",
    "QuotaExceeded",
    "Rank",
    "Revision",
    "Ready",
    "ReadyAll",
    "ReadyLog",
    "ReadyPing",
    "ReadyTcp",
    "RestartPolicy",
    "Run",
    "Session",
    "Stale",
    "StaleGeneration",
    "StreamLost",
    "Summary",
    "SummaryOptions",
    "SummaryReason",
    "SummaryRender",
    "SummarySegment",
    "SummaryUnavailable",
    "SymlinkTarget",
    "SyncKind",
    "SyncPolicy",
    "TimedOut",
    "Unsupported",
    "Txn",
    "WorktreeInfo",
    "blobs",
    "direct_filesystem",
    "docs",
    "find",
    "fs",
    "has",
    "http_get",
    "http_post",
    "http_put",
    "info",
    "lsp",
    "proc",
    "require",
    "sh",
    "worktree",
)
