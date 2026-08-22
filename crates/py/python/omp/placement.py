"""Declarative placement and worker API; import is transport-free."""
from __future__ import annotations
import asyncio
import base64
import socket
from collections.abc import Sequence
from dataclasses import dataclass, field
from enum import StrEnum
from types import MappingProxyType
from typing import Any, Callable, Final, Iterable, Mapping, TypeVar

from _omp import Duration, HostDisconnected, PlacementError, StaleGeneration

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
class WorkerUnavailable(PlacementError):
    """The requested named worker is not currently reachable."""
class WorkerEvicted(PlacementError):
    """A worker handle names a draining or retired generation."""
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

def _worker_info(value: object) -> WorkerInfo:
    """Decode one authenticated supervisor observation."""
    if isinstance(value, WorkerInfo):
        return value
    if not isinstance(value, Mapping):
        raise TypeError("worker info response must be an object")
    raw_site = value.get("site")
    if isinstance(raw_site, Site):
        site = raw_site
    elif isinstance(raw_site, Mapping):
        try:
            site = Site(
                SiteKind(raw_site["kind"]),
                raw_site.get("process"),
                raw_site.get("ready"),
            )
        except (KeyError, TypeError, ValueError) as error:
            raise TypeError("worker info response has an invalid site") from error
    else:
        raise TypeError("worker info response must contain a site")
    try:
        return WorkerInfo(
            name=str(value["name"]),
            generation=int(value["generation"]),
            state=WorkerState(value["state"]),
            site=site,
            pid=None if value.get("pid") is None else int(value["pid"]),
            spawned_at_ms=int(value.get("spawned_at_ms", 0)),
            last_call_at_ms=(
                None
                if value.get("last_call_at_ms") is None
                else int(value["last_call_at_ms"])
            ),
            calls=int(value.get("calls", 0)),
            in_flight=int(value.get("in_flight", 0)),
            code_cached=int(value.get("code_cached", 0)),
            enforced=frozenset(str(item) for item in value.get("enforced", ())),
            fault=None if value.get("fault") is None else str(value["fault"]),
        )
    except (KeyError, TypeError, ValueError) as error:
        raise TypeError("worker info response is malformed") from error


@dataclass(frozen=True, slots=True)
class Spill:
    """Marks a result buffer for env-side out-of-band blob storage."""
    value: bytes
    media_type: str = "application/octet-stream"
_T = TypeVar("_T")


class _WorkerSession:
    """Generation-fenced raw worker-session context manager."""
    __slots__ = ("_handle", "_session")

    def __init__(self, handle: "WorkerHandle") -> None:
        self._handle = handle
        self._session: Any = None

    async def __aenter__(self) -> Any:
        endpoint = await workers._admin(
            "session",
            name=self._handle.name,
            generation=self._handle.generation,
        )
        if not isinstance(endpoint, Mapping):
            raise WorkerUnavailable("worker session endpoint must be an object")
        endpoint_generation = endpoint.get("generation")
        if (
            not isinstance(endpoint_generation, int)
            or endpoint_generation != self._handle.generation
        ):
            raise WorkerEvicted(
                f"worker {self._handle.name!r} generation "
                f"{self._handle.generation} has been evicted"
            )
        family = endpoint.get("family")
        address = endpoint.get("address")
        encoded_key = endpoint.get("authkey_base64")
        if family == "unix":
            if not isinstance(address, str) or not address:
                raise WorkerUnavailable("worker session has an invalid Unix address")
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connect_address: Any = address
        elif family == "tcp":
            if (
                not isinstance(address, Sequence)
                or isinstance(address, (str, bytes, bytearray))
                or len(address) != 2
                or not isinstance(address[0], str)
                or not isinstance(address[1], int)
            ):
                raise WorkerUnavailable("worker session has an invalid TCP address")
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            connect_address = (address[0], address[1])
        else:
            raise WorkerUnavailable("worker session has an unknown address family")
        if encoded_key is None:
            authkey = None
        elif isinstance(encoded_key, str):
            try:
                authkey = base64.b64decode(encoded_key, validate=True)
            except ValueError as error:
                sock.close()
                raise WorkerUnavailable("worker session has an invalid auth key") from error
        else:
            sock.close()
            raise WorkerUnavailable("worker session has an invalid auth key")
        try:
            await asyncio.to_thread(sock.connect, connect_address)
            from omp_remote import Session

            self._session = await asyncio.to_thread(Session, sock, authkey)
        except BaseException:
            sock.close()
            raise
        return self._session

    async def __aexit__(self, *_exc: Any) -> None:
        if self._session is not None:
            await asyncio.to_thread(self._session.close)
            self._session = None


class WorkerHandle:
    """Generation-fenced handle for a named worker."""
    __slots__ = ("name", "generation", "site")
    def __init__(self, name: str, generation: int = 0, site: Site = Site.ENV) -> None: self.name, self.generation, self.site = name, generation, site
    async def state(self) -> WorkerState:
        """Return this worker generation's current state."""
        try:
            return (await self.info()).state
        except WorkerEvicted:
            raise
        except (HostDisconnected, NotWiredError, WorkerUnavailable):
            return WorkerState.FAILED

    async def info(self) -> WorkerInfo:
        """Return the full observation of this worker generation."""
        info = await workers._admin(
            "info", name=self.name, generation=self.generation
        )
        if not isinstance(info, WorkerInfo):
            raise TypeError("worker info response must return WorkerInfo")
        return info

    async def call(
        self, function: Callable[..., _T], /, *args: Any, **kwargs: Any
    ) -> _T:
        """Run a remote function against this handle's worker generation."""
        return await workers._call(
            self.name, self.generation, function, args, kwargs
        )
    async def map(self, function: Callable[[_T], Any], values: Iterable[_T], *, concurrency: int | None = None) -> list[Any]:
        """Map calls serially; ``concurrency`` is reserved until the Part 3 supervisor lands."""
        if concurrency is not None and concurrency < 1: raise ValueError("concurrency must be positive")
        return [await self.call(function, value) for value in values]
    async def warm(self) -> None:
        """Ensure this worker generation has reached the ready state."""
        state = await workers._admin(
            "warm", name=self.name, generation=self.generation
        )
        if state in (WorkerState.DRAINING, WorkerState.EVICTED):
            raise WorkerEvicted(
                f"worker {self.name!r} generation {self.generation} has been evicted"
            )

    async def stop(self, *, grace: Duration = Duration("5s")) -> None:
        """Drain and terminate this worker generation."""
        try:
            await workers._admin(
                "stop", name=self.name, generation=self.generation, grace=grace
            )
        except (HostDisconnected, NotWiredError, WorkerEvicted, WorkerUnavailable):
            return

    def session(self) -> _WorkerSession:
        """Borrow one raw session from this worker's connection pool."""
        return _WorkerSession(self)

class _Workers:
    """Worker declaration table and host-authoritative worker namespace."""
    RESULT_SPILL_BYTES: Final[int] = 256 * 1024
    DEFAULT_IDLE_TTL: Final[Duration] = Duration("7m")
    MAX_CONCURRENT_SPAWNS: Final[int] = 4
    __slots__ = ("_spawn_gate",)
    def __init__(self) -> None:
        self._spawn_gate = asyncio.Semaphore(self.MAX_CONCURRENT_SPAWNS)
    def declare(self, spec: WorkerSpec) -> None:
        """Record one worker manifest projection during IMPORT."""
        if not isinstance(spec, WorkerSpec):
            raise TypeError("omp.workers.declare requires a WorkerSpec")
        registry.register_worker(spec.name, spec)
    async def _admin(self, action: str, **kwargs: Any) -> Any:
        from . import _control_backend, _control_request

        operation = f"omp.workers.{action}"
        if _control_backend.get() is None:
            raise NotWiredError(operation)
        arguments = {
            name: value.seconds if isinstance(value, Duration) else value
            for name, value in kwargs.items()
        }
        try:
            result = await _control_request(operation, **arguments)
        except WorkerEvicted:
            raise
        except StaleGeneration as error:
            name = kwargs.get("name", "<unknown>")
            generation = kwargs.get("generation")
            detail = f"worker {name!r}"
            if generation is not None:
                detail += f" generation {generation}"
            raise WorkerEvicted(f"{detail} has been evicted") from error
        if action in ("get", "info", "restart"):
            info = _worker_info(result)
            expected_name = kwargs.get("name")
            expected_generation = kwargs.get("generation")
            if expected_name is not None and info.name != expected_name:
                raise WorkerUnavailable("worker supervisor returned the wrong worker")
            if (
                expected_generation is not None
                and info.generation != expected_generation
            ):
                raise WorkerEvicted(
                    f"worker {info.name!r} generation {expected_generation} has been evicted"
                )
            return info
        if action == "list":
            if not isinstance(result, Sequence) or isinstance(
                result, (str, bytes, bytearray)
            ):
                raise TypeError("worker list response must be a sequence")
            return [_worker_info(item) for item in result]
        if action == "warm" and isinstance(result, str):
            return WorkerState(result)
        return result
    async def get(self, name: str) -> WorkerHandle:
        """Resolve a named worker handle, bounding concurrent cold spawns."""
        if not name:
            raise ValueError("worker name is required")
        async with self._spawn_gate:
            resolved = await self._admin("get", name=name)
        if isinstance(resolved, WorkerInfo):
            if resolved.state in (WorkerState.DRAINING, WorkerState.EVICTED):
                raise WorkerEvicted(
                    f"worker {name!r} generation {resolved.generation} has been evicted"
                )
            if resolved.state is not WorkerState.READY:
                detail = f": {resolved.fault}" if resolved.fault else ""
                raise WorkerUnavailable(f"worker {name!r} is not ready{detail}")
            return WorkerHandle(resolved.name, resolved.generation, resolved.site)
        raise TypeError("worker get response must return WorkerInfo")
    async def list(self) -> list[WorkerInfo]:
        """Return observations for every declared worker generation."""
        try:
            return await self._admin("list")
        except (HostDisconnected, NotWiredError, WorkerUnavailable):
            return []

    async def evict(
        self, name: str, *, grace: Duration = Duration("5s")
    ) -> bool:
        """Drain and terminate the current generation of a named worker."""
        try:
            evicted = await self._admin("evict", name=name, grace=grace)
        except (HostDisconnected, NotWiredError, WorkerUnavailable):
            return False
        if not isinstance(evicted, bool):
            raise TypeError("worker evict response must be bool")
        return evicted

    async def restart(
        self, name: str, *, grace: Duration = Duration("5s")
    ) -> WorkerInfo:
        """Restart a named worker and return its new generation."""
        try:
            return await self._admin("restart", name=name, grace=grace)
        except Exception as error:
            raise WorkerUnavailable(f"worker {name!r} failed") from error
    async def _call(
        self,
        name: str,
        generation: int,
        function: Callable[..., _T],
        args: tuple[Any, ...],
        kwargs: dict[str, Any],
    ) -> _T:
        try:
            handle = WorkerHandle(name, generation)
            async with handle.session() as session:
                return await asyncio.to_thread(session.call, function, *args, **kwargs)
        except (WorkerEvicted, StaleGeneration):
            raise
        except Exception as error:
            raise WorkerUnavailable(f"worker {name!r} failed") from error
workers = _Workers()
worker_state = "worker_state"
MAX_WORKERS = 8
