"""Explicit CONTROL bootstrap and bounded framed transport.

Importing this module is inert. ``bootstrap`` is the sole operation that reads the
inherited CONTROL descriptor.
"""
from __future__ import annotations

import asyncio
import contextvars
import json
import os
import struct
import sys
from collections import deque
from dataclasses import dataclass
from threading import Lock, Timer
from typing import Any, BinaryIO

from _omp import HostDisconnected, _interrupt, _thread_id

from . import _scope
from ._errors import FrameTooLarge
from .limits import CANCEL_GRACE, MAX_FRAME_BYTES, MAX_PENDING_EFFECTS

_MAX_PENDING = MAX_PENDING_EFFECTS
_effects: contextvars.ContextVar[dict[str, Any] | None] = contextvars.ContextVar("omp_effects", default=None)
_reentrancy: contextvars.ContextVar[int] = contextvars.ContextVar("omp_control_depth", default=0)

@dataclass(frozen=True, slots=True)
class _Frame:
    """One decoded CONTROL envelope."""
    kind: str
    correlation: int | None
    body: dict[str, Any]


@dataclass(slots=True)
class _Dispatch:
    """One live invocation and its pending cancellation escalation."""

    task: asyncio.Task[Any]
    thread_id: int
    scope: _scope.Scope | None
    escalation: Timer | None = None
    cancel_started: bool = False


class _Capture:
    """Line-buffered stream forwarding output as structured CONTROL logs."""
    __slots__ = ("_host", "_stream", "_buffer")
    def __init__(self, host: "Host", stream: str) -> None:
        self._host, self._stream, self._buffer = host, stream, ""
    def write(self, text: str) -> int:
        self._buffer += text
        while "\n" in self._buffer:
            line, self._buffer = self._buffer.split("\n", 1)
            self._host.log(self._stream, line)
        return len(text)
    def flush(self) -> None:
        if self._buffer:
            self._host.log(self._stream, self._buffer)
            self._buffer = ""

class Host:
    """Correlation-aware, reentrant CONTROL codec on one inherited descriptor."""
    __slots__ = (
        "_fd",
        "_lock",
        "_pending",
        "_next_id",
        "_mailbox",
        "_stdout",
        "_stderr",
        "_tasks",
    )
    def __init__(self, fd: int) -> None:
        self._fd = fd
        self._lock = Lock()
        self._pending: dict[int, asyncio.Future[Any]] = {}
        self._next_id = 1
        self._mailbox: deque[dict[str, Any]] = deque()
        self._stdout: Any = None
        self._stderr: Any = None
        self._tasks: dict[str, _Dispatch] = {}
    @staticmethod
    def _decode(raw: bytes) -> _Frame:
        try:
            value = json.loads(raw)
            return _Frame(str(value["kind"]), value.get("correlation"), dict(value.get("body", {})))
        except (TypeError, ValueError, KeyError) as error:
            raise HostDisconnected("invalid CONTROL frame") from error
    def _read_exact(self, count: int) -> bytes:
        chunks = bytearray()
        while len(chunks) < count:
            chunk = os.read(self._fd, count - len(chunks))
            if not chunk:
                raise HostDisconnected("CONTROL channel disconnected")
            chunks.extend(chunk)
        return bytes(chunks)
    def _read_frame(self) -> _Frame:
        header = self._read_exact(4)
        size = struct.unpack("!I", header)[0]
        if size > MAX_FRAME_BYTES:
            raise FrameTooLarge(size, MAX_FRAME_BYTES)
        return self._decode(self._read_exact(size))
    def _write(self, value: dict[str, Any]) -> None:
        raw = json.dumps(value, separators=(",", ":")).encode()
        if len(raw) > MAX_FRAME_BYTES:
            raise FrameTooLarge(len(raw), MAX_FRAME_BYTES)
        os.write(self._fd, struct.pack("!I", len(raw)) + raw)
    def effect(self, effect: dict[str, Any]) -> None:
        """Write one already-encoded, non-correlated UI effect frame."""
        self._write({"kind": "UiEffect", "body": effect})
    def log(
        self,
        stream: object,
        text: str,
        fields: dict[str, Any] | None = None,
    ) -> None:
        """Emit captured text or a structured context log as one Log frame."""
        if fields is None:
            body: dict[str, Any] = {"stream": stream, "text": text}
        else:
            body = {
                "level": str(getattr(stream, "value", stream)),
                "message": text,
                "fields": fields,
            }
        self._write({"kind": "Log", "body": body})
    def install_capture(self) -> None:
        """Capture child stdout and stderr without making prints protocol errors."""
        self._stdout, self._stderr = sys.stdout, sys.stderr
        sys.stdout, sys.stderr = _Capture(self, "stdout"), _Capture(self, "stderr")
    def poll(self) -> None:
        """Receive one frame and resolve a correlated request or queue an effect."""
        frame = self._read_frame()
        if frame.kind == "CancelDispatch":
            invocation = str(frame.body["invocation"])
            with self._lock:
                dispatch = self._tasks.get(invocation)
                if (
                    dispatch is None
                    or dispatch.cancel_started
                    or dispatch.task.done()
                ):
                    return
                dispatch.cancel_started = True
                timer = Timer(
                    CANCEL_GRACE.seconds,
                    self._interrupt_dispatch,
                    (invocation, dispatch),
                )
                timer.daemon = True
                dispatch.escalation = timer
                first_cancel = (
                    dispatch.scope is not None
                    and _scope._request_cancel(dispatch.scope)
                )
            loop = dispatch.task.get_loop()
            loop.call_soon_threadsafe(dispatch.task.cancel)
            if first_cancel:
                loop.call_soon_threadsafe(
                    _scope._fire_cancel_callbacks,
                    dispatch.scope,
                    lambda error: self._log_cancel_callback_error(
                        invocation, error
                    ),
                )
            timer.start()
            return
        if frame.kind == "Effect":
            self._mailbox.append(frame.body)
            return
        if frame.correlation is not None and (future := self._pending.pop(frame.correlation, None)) is not None:
            if not future.done(): future.set_result(frame.body)
    async def request(self, operation: str, arguments: dict[str, Any]) -> Any:
        """Send one request while explicitly tracking nested CONTROL reentrancy."""
        if len(self._pending) >= _MAX_PENDING:
            raise HostDisconnected("too many pending CONTROL requests")
        correlation, self._next_id = self._next_id, self._next_id + 1
        future = asyncio.get_running_loop().create_future()
        self._pending[correlation] = future
        token = _reentrancy.set(_reentrancy.get() + 1)
        try:
            self._write({"kind": "Request", "correlation": correlation, "body": {"operation": operation, "arguments": arguments}})
            while not future.done():
                await asyncio.to_thread(self.poll)
            return future.result()
        finally:
            _reentrancy.reset(token)
            self._pending.pop(correlation, None)
    def take_effect(self) -> dict[str, Any] | None:
        """Return the next host-delivered effect without reentering the codec."""
        return self._mailbox.popleft() if self._mailbox else None

    def _log_cancel_callback_error(
        self,
        invocation: str,
        error: BaseException,
    ) -> None:
        """Report a guarded cancellation callback failure without breaking CONTROL."""
        try:
            self.log(
                "error",
                "cancellation callback failed",
                {
                    "invocation": invocation,
                    "error": repr(error),
                },
            )
        except BaseException:
            pass

    def _interrupt_dispatch(
        self,
        invocation: str,
        dispatch: _Dispatch,
    ) -> None:
        """Interrupt a dispatch only when it remains live after the grace."""
        with self._lock:
            if (
                self._tasks.get(invocation) is not dispatch
                or dispatch.task.done()
            ):
                return
            dispatch.escalation = None
            # Python owns stages 1-2 inside the interpreter; Rust owns stage 3.
            _interrupt(dispatch.thread_id)

    def _settle_if_current(
        self,
        invocation: str,
        dispatch: _Dispatch,
    ) -> None:
        """Forget a dispatch only if it is still the invocation's live target."""
        with self._lock:
            if self._tasks.get(invocation) is not dispatch:
                return
            self._tasks.pop(invocation)
        if dispatch.escalation is not None:
            dispatch.escalation.cancel()

    def track_dispatch(
        self,
        invocation: str,
        task: asyncio.Task[Any],
        scope: _scope.Scope | None = None,
    ) -> None:
        """Record the task, thread, and authority scope for cancellation."""
        if scope is None:
            try:
                scope = _scope.current()
            except RuntimeError:
                pass
        dispatch = _Dispatch(task, _thread_id(), scope)
        with self._lock:
            previous = self._tasks.get(invocation)
            self._tasks[invocation] = dispatch
        if previous is not None and previous.escalation is not None:
            previous.escalation.cancel()
        task.add_done_callback(
            lambda _task: self._settle_if_current(invocation, dispatch)
        )

    def settle_dispatch(self, invocation: str) -> None:
        """Forget a settled invocation and cancel its pending escalation."""
        with self._lock:
            dispatch = self._tasks.pop(invocation, None)
        if dispatch is not None and dispatch.escalation is not None:
            dispatch.escalation.cancel()

def bootstrap(fd: int | None = None) -> Host:
    """Open the inherited CONTROL descriptor and install its explicit bridge."""
    if fd is None:
        fd = int(os.environ["OMP_EXT_CONTROL_FD"])
    host = Host(fd)
    host.install_capture()
    from . import _install_control_backend
    _install_control_backend(host)
    return host
