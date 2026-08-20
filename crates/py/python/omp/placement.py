"""Declarative placement and worker API; import is transport-free."""
from __future__ import annotations
from dataclasses import dataclass, field
from enum import StrEnum
from types import MappingProxyType
from typing import Any, Callable, Iterable, Mapping, TypeVar

from _omp import Duration

from ._registry import registry
from ._errors import NotWiredError

class PlaceKind(StrEnum):
    """Execution locality for a decorated device."""
    HOST = "host"; ENV = "env"; WORKER = "worker"
class SiteKind(StrEnum):
    """Site selected for a named worker."""
    ENV = "env"; LOCAL = "local"; ATTACHED = "attached"
class Restart(StrEnum):
    """Named worker restart policy."""
    NO = "no"; ON_FAILURE = "on-failure"; ALWAYS = "always"
class WorkerState(StrEnum):
    """Observable named-worker lifecycle state."""
    SPAWNING = "spawning"; BOOTING = "booting"; READY = "ready"; DRAINING = "draining"; EVICTED = "evicted"; FAILED = "failed"
class PlacementError(RuntimeError):
    """A placement declaration or execution cannot be honored."""
class WorkerUnavailable(PlacementError):
    """The requested named worker is not currently reachable."""
class ShipError(PlacementError):
    """Code shipping to a worker failed."""
class BoundaryError(PlacementError):
    """A value or capability cannot cross the placement boundary."""

@dataclass(frozen=True, slots=True)
class Place:
    """Parsed ``place=`` decorator value."""
    kind: PlaceKind
    name: str | None = None
    @classmethod
    def worker(cls, name: str) -> "Place":
        """Create a named-worker placement."""
        if not name or any(not (c.isalnum() or c in "._-") for c in name): raise ValueError("invalid worker name")
        return cls(PlaceKind.WORKER, name)
    @classmethod
    def parse(cls, value: str | "Place") -> "Place":
        """Parse host, env, or worker:name placement."""
        if isinstance(value, cls): return value
        if value == "host": return cls.HOST
        if value == "env": return cls.ENV
        if isinstance(value, str) and value.startswith("worker:"): return cls.worker(value[7:])
        raise PlacementError(f"invalid placement {value!r}")
    def __str__(self) -> str: return self.kind.value if self.name is None else f"worker:{self.name}"

Place.HOST = Place(PlaceKind.HOST)  # type: ignore[attr-defined]
Place.ENV = Place(PlaceKind.ENV)  # type: ignore[attr-defined]

@dataclass(frozen=True, slots=True)
class Site:
    """Worker process site declaration."""
    kind: SiteKind
    process: str | None = None
    ready: Any = None
    @classmethod
    def attached(cls, process: str, *, ready: Any = None) -> "Site":
        """Create an attached named-process site."""
        if not process: raise ValueError("attached process is required")
        return cls(SiteKind.ATTACHED, process, ready)
Site.ENV = Site(SiteKind.ENV)  # type: ignore[attr-defined]
Site.LOCAL = Site(SiteKind.LOCAL)  # type: ignore[attr-defined]

@dataclass(frozen=True, slots=True)
class WorkerResources:
    """Requested worker limits, projected to Rust ``WorkerLimits``."""
    memory_bytes: int | None = None
    cpu_shares: float | None = None
    open_files: int | None = None
    wall_clock: Duration | None = None


@dataclass(frozen=True, slots=True)
class WorkerSpec:
    """Manifest-declared persistent worker; detached is intentionally absent."""
    name: str
    site: Site = Site.ENV
    boot: Any = None
    idle_ttl: Duration = Duration("7m")
    max_concurrency: int = 1
    max_calls: int | None = None
    restart: Restart = Restart.NO
    resources: WorkerResources = field(default_factory=WorkerResources)
    cwd: Any = None
    env_delta: Mapping[str, str | None] = field(
        default_factory=lambda: MappingProxyType({})
    )
    readonly: bool = False
    unmanaged: bool = False
    warm: bool = False

    def __post_init__(self) -> None:
        if not self.name:
            raise ValueError("worker name is required")
        if self.max_concurrency < 1:
            raise ValueError("worker max_concurrency must be positive")
        if self.max_calls is not None and self.max_calls < 1:
            raise ValueError("worker max_calls must be positive")
        object.__setattr__(self, "env_delta", MappingProxyType(dict(self.env_delta)))
@dataclass(frozen=True, slots=True)
class WorkerInfo:
    """A generation-fenced worker observation."""
    name: str; generation: int; state: WorkerState; site: Site
    pid: int | None = None; spawned_at_ms: int = 0; last_call_at_ms: int | None = None
    calls: int = 0; in_flight: int = 0; code_cached: int = 0; enforced: frozenset[str] = frozenset(); fault: str | None = None

@dataclass(frozen=True, slots=True)
class Spill:
    """Marks a result buffer for env-side out-of-band blob storage."""
    value: bytes
    media_type: str = "application/octet-stream"
    def __post_init__(self) -> None:
        if workers._unmanaged: raise BoundaryError("Spill is unavailable on unmanaged workers")
_T = TypeVar("_T")


class _WorkerSession:
    """Unwired raw worker-session context manager."""

    async def __aenter__(self) -> Any:
        raise NotWiredError("omp.workers.WorkerHandle.session")

    async def __aexit__(self, *_exc: Any) -> None:
        return None


class WorkerHandle:
    """Generation-fenced handle for a named worker."""
    __slots__ = ("name", "generation", "site")
    def __init__(self, name: str, generation: int = 0, site: Site = Site.ENV) -> None: self.name, self.generation, self.site = name, generation, site
    async def state(self) -> WorkerState:
        """Return this worker generation's current state."""
        raise NotWiredError("omp.workers.WorkerHandle.state")

    async def info(self) -> WorkerInfo:
        """Return the full observation of this worker generation."""
        raise NotWiredError("omp.workers.WorkerHandle.info")

    async def call(self, function: Callable[..., _T], /, *args: Any, **kwargs: Any) -> _T: return await workers._call(self.name, function, args, kwargs)
    async def map(self, function: Callable[[_T], Any], values: Iterable[_T], *, concurrency: int | None = None) -> list[Any]:
        """Map calls serially; ``concurrency`` is reserved until the Part 3 supervisor lands."""
        if concurrency is not None and concurrency < 1: raise ValueError("concurrency must be positive")
        return [await self.call(function, value) for value in values]
    async def warm(self) -> None:
        """Ensure this worker generation has reached the ready state."""
        raise NotWiredError("omp.workers.WorkerHandle.warm")

    async def stop(self, *, grace: Duration = Duration("5s")) -> None:
        """Drain and terminate this worker generation."""
        del grace
        raise NotWiredError("omp.workers.WorkerHandle.stop")

    def session(self) -> _WorkerSession:
        """Borrow one raw session from this worker's connection pool."""
        return _WorkerSession()

class _Workers:
    """Worker declaration table and host-installed WorkerOp DATA registry."""
    RESULT_SPILL_BYTES = 256 * 1024
    DEFAULT_IDLE_TTL = Duration("7m")
    __slots__ = ("_transport", "_unmanaged")
    def __init__(self) -> None: self._transport: Any = None; self._unmanaged = False
    def declare(self, spec: WorkerSpec) -> None:
        """Record one worker manifest projection during IMPORT."""
        if not isinstance(spec, WorkerSpec):
            raise TypeError("omp.workers.declare requires a WorkerSpec")
        registry.register_worker(spec.name, spec)
    def install(self, transport: Any, *, unmanaged: bool = False) -> None: self._transport, self._unmanaged = transport, unmanaged
    async def get(self, name: str) -> WorkerHandle:
        """Resolve a named worker handle through the asynchronous worker surface."""
        return WorkerHandle(name)
    async def list(self) -> list[WorkerInfo]:
        """Return observations for every declared worker generation."""
        if self._transport is None:
            raise NotWiredError("omp.workers.list")
        try:
            return await self._transport.worker_admin("list")
        except Exception:
            return []

    async def evict(
        self, name: str, *, grace: Duration = Duration("5s")
    ) -> bool:
        """Drain and terminate the current generation of a named worker."""
        if self._transport is None:
            raise NotWiredError("omp.workers.evict")
        try:
            return await self._transport.worker_admin(
                "evict", name=name, grace=grace
            )
        except Exception:
            return False

    async def restart(
        self, name: str, *, grace: Duration = Duration("5s")
    ) -> WorkerInfo:
        """Restart a named worker and return its new generation."""
        if self._transport is None:
            raise NotWiredError("omp.workers.restart")
        try:
            return await self._transport.worker_admin(
                "restart", name=name, grace=grace
            )
        except Exception as error:
            raise WorkerUnavailable(f"worker {name!r} failed") from error
    async def _call(self, name: str, function: Callable[..., _T], args: tuple[Any, ...], kwargs: dict[str, Any]) -> _T:
        if self._transport is None:
            from omp_remote import RemoteTraceback
            raise WorkerUnavailable(f"worker {name!r} is unavailable") from RemoteTraceback(f"worker {name!r} has not been provisioned")
        try: return await self._transport.worker_op(name, function, args, kwargs)
        except Exception as error: raise WorkerUnavailable(f"worker {name!r} failed") from error
workers = _Workers()
worker_state = "worker_state"
MAX_WORKERS = 8
