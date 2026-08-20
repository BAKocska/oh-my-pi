"""Typed, awaitable Environment DATA-plane surface.

Importing this module only constructs immutable values and namespace objects.  A
host installs the scoped backend after its DATA handshake; no import opens a
file, socket, process, or event loop.
"""

from __future__ import annotations

import asyncio
import inspect
import contextvars
from collections.abc import AsyncIterator, Iterable, Mapping
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from types import MappingProxyType
from _omp import (
    BlobRef,
    ClientPath,
    EnvPath,
    EnvUnavailable,
    PlacementError,
    _read_bytes_blocking,
)

from . import EffectsNotAuthorized, Fault, OmpError, StaleGeneration


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


_binding: contextvars.ContextVar[tuple[Any, EnvInfo] | None] = contextvars.ContextVar(
    "omp_env_binding", default=None
)


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

    def __init__(self, lease: Any, path: EnvPath, revision: Any = None) -> None:
        self._lease = lease
        self.path = path
        self.revision = revision
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

    async def edit(self, edits: Iterable[Any], **options: Any) -> Any:
        """Commit ordered, non-overlapping edits against this lease."""
        return await _request("omp.env.docs.Doc.edit", lease=self._lease, edits=tuple(edits), **options)

    async def write(self, data: str | bytes, **options: Any) -> Any:
        """Replace the document contents revisionally."""
        return await _request("omp.env.docs.Doc.write", lease=self._lease, data=data, **options)

    async def hashline(self, patch: str, **options: Any) -> Any:
        """Apply one hashline patch through the document actor."""
        return await _request("omp.env.docs.Doc.hashline", lease=self._lease, patch=patch, **options)

    async def summary(self, options: Any = None) -> Any:
        """Return a bounded structural summary at the current revision."""
        return await _request(
            "omp.env.docs.Doc.summary", lease=self._lease, options=options
        )

    async def refresh(self) -> Any:
        """Refresh and return the current committed revision."""
        self.revision = await _request("omp.env.docs.Doc.refresh", lease=self._lease)
        return self.revision

    async def close(self) -> None:
        """Close the lease idempotently."""
        lease, self._lease = self._lease, None
        if lease is not None:
            await _request("omp.env.docs.Doc.close", lease=lease)

    def events(self) -> AsyncIterator[Any]:
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


class _Fs:
    """Raw metadata and namespace operations over typed Environment paths."""

    async def stat(self, path: EnvPath) -> Any:
        """Stat a path while following symbolic links."""
        return await _request("omp.env.fs.stat", path=_env_path(path))

    async def lstat(self, path: EnvPath) -> Any:
        """Stat a path without following its final symbolic link."""
        return await _request("omp.env.fs.lstat", path=_env_path(path))

    async def list_dir(self, path: EnvPath) -> list[Any]:
        """List one directory."""
        return await _request("omp.env.fs.list_dir", path=_env_path(path))

    async def read_link(self, path: EnvPath) -> EnvPath:
        """Read a symbolic-link target as an Environment path."""
        return await _request("omp.env.fs.read_link", path=_env_path(path))

    async def canonicalize(self, path: EnvPath) -> EnvPath:
        """Resolve a path in the Environment namespace."""
        return await _request("omp.env.fs.canonicalize", path=_env_path(path))

    async def mkdir(self, path: EnvPath, *, parents: bool = False, exist_ok: bool = False) -> None:
        """Create a directory."""
        await _request(
            "omp.env.fs.mkdir", path=_env_path(path), parents=parents, exist_ok=exist_ok
        )

    async def remove(self, path: EnvPath, *, recursive: bool = False) -> None:
        """Remove a path."""
        await _request("omp.env.fs.remove", path=_env_path(path), recursive=recursive)

    async def rename(self, source: EnvPath, destination: EnvPath, **options: Any) -> Any:
        """Rename a path inside the Environment namespace."""
        return await _request(
            "omp.env.fs.rename",
            source=_env_path(source, "source"),
            destination=_env_path(destination, "destination"),
            **options,
        )

    async def copy(self, source: EnvPath, destination: EnvPath, **options: Any) -> Any:
        """Copy a path inside the Environment namespace."""
        return await _request(
            "omp.env.fs.copy",
            source=_env_path(source, "source"),
            destination=_env_path(destination, "destination"),
            **options,
        )

    async def symlink(self, target: EnvPath, link: EnvPath) -> None:
        """Create a symbolic link without ambient path conversion."""
        await _request(
            "omp.env.fs.symlink",
            target=_env_path(target, "target"),
            link=_env_path(link, "link"),
        )

    async def hard_link(self, target: EnvPath, link: EnvPath) -> None:
        """Create a hard link without ambient path conversion."""
        await _request(
            "omp.env.fs.hard_link",
            target=_env_path(target, "target"),
            link=_env_path(link, "link"),
        )

    async def chmod(self, path: EnvPath, permissions: Any) -> None:
        """Set Environment-owned path permissions."""
        await _request(
            "omp.env.fs.chmod", path=_env_path(path), permissions=permissions
        )


class _Lsp:
    """Revision-aware language-server multiplexing."""

    async def bindings(self, path: EnvPath) -> list[Any]:
        """Return servers currently bound to a path."""
        return await _request("omp.env.lsp.bindings", path=_env_path(path))

    async def request(self, server: object, method: str, params: Any, **options: Any) -> Any:
        """Issue a revision-aware LSP request."""
        return await _request(
            "omp.env.lsp.request", server=server, method=method, params=params, **options
        )

    async def notify(self, server: object, method: str, params: Any) -> None:
        """Issue an LSP notification."""
        await _request("omp.env.lsp.notify", server=server, method=method, params=params)

    def events(self) -> AsyncIterator[Any]:
        """Yield LSP registry and server events."""
        return _stream("omp.env.lsp.events")


class Run:
    """Guarded command handle and ordered async event stream."""

    __slots__ = ("id",)

    def __init__(self, run_id: bytes) -> None:
        self.id = run_id

    def __aiter__(self) -> AsyncIterator[Any]:
        return _stream("omp.env.Run.events", run=self.id)

    async def wait(self) -> Any:
        """Drain output and return the terminal completion receipt."""
        return await _request("omp.env.Run.wait", run=self.id)

    async def write(self, data: bytes) -> None:
        """Write bytes to stdin or the PTY master."""
        await _request("omp.env.Run.write", run=self.id, data=data)

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


class Process:
    """Stable named-process generation handle."""

    __slots__ = ("name", "generation")

    def __init__(self, name: str, generation: int) -> None:
        self.name = name
        self.generation = generation

    async def info(self) -> Any:
        """Return the current generation snapshot."""
        return await _request("omp.env.Process.info", name=self.name)

    def output(self, *, after: int = 0) -> AsyncIterator[Any]:
        """Yield retained and live ordered process output."""
        return _stream("omp.env.Process.output", name=self.name, after=after)

    def states(self) -> AsyncIterator[Any]:
        """Yield named-process lifecycle transitions."""
        return _stream("omp.env.Process.states", name=self.name)

    async def send(self, data: bytes) -> None:
        """Send bytes to process stdin."""
        await _request("omp.env.Process.send", name=self.name, data=data)

    async def signal(self, signal: str) -> None:
        """Signal the Environment-owned process group."""
        await _request("omp.env.Process.signal", name=self.name, signal=signal)

    async def stop(self, **options: Any) -> Any:
        """Stop the process tree and return its terminal state."""
        return await _request("omp.env.Process.stop", name=self.name, **options)


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

    async def run(self, script: str, **options: Any) -> Any:
        """Run a command and collect its bounded completion receipt."""
        cwd = options.get("cwd")
        if cwd is not None:
            options["cwd"] = _env_path(cwd, "cwd")
        return await _request("omp.env.sh.run", script=script, **options)

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

    async def ensure(self, name: str, script: str, **options: Any) -> Process:
        """Adopt a matching process or start it atomically."""
        cwd = options.get("cwd")
        if cwd is not None:
            options["cwd"] = _env_path(cwd, "cwd")
        result = await _request("omp.env.proc.ensure", name=name, script=script, **options)
        return result if isinstance(result, Process) else Process(result["name"], result["generation"])

    async def list(self) -> list[Any]:
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
    "Cancelled",
    "Capability",
    "Conflict",
    "Denied",
    "Disconnected",
    "Doc",
    "EffectsNotAuthorized",
    "Entry",
    "EnvError",
    "EnvInfo",
    "Follow",
    "Invalid",
    "Io",
    "Match",
    "NotFound",
    "Partial",
    "Process",
    "PreconditionFailed",
    "QuotaExceeded",
    "Rank",
    "Run",
    "Session",
    "Stale",
    "StaleGeneration",
    "StreamLost",
    "TimedOut",
    "Unsupported",
    "Txn",
    "blobs",
    "docs",
    "find",
    "fs",
    "has",
    "info",
    "lsp",
    "proc",
    "require",
    "sh",
)
