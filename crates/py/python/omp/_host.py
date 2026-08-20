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
from typing import Any, BinaryIO

from _omp import FrameTooLarge, HostDisconnected, _interrupt, _thread_id

_MAX_FRAME_BYTES = 4 * 1024 * 1024
_MAX_PENDING = 1024
_effects: contextvars.ContextVar[dict[str, Any] | None] = contextvars.ContextVar("omp_effects", default=None)
_reentrancy: contextvars.ContextVar[int] = contextvars.ContextVar("omp_control_depth", default=0)

@dataclass(frozen=True, slots=True)
class _Frame:
    """One decoded CONTROL envelope."""
    kind: str
    correlation: int | None
    body: dict[str, Any]

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
    __slots__ = ("_fd", "_pending", "_next_id", "_mailbox", "_stdout", "_stderr", "_tasks")
    def __init__(self, fd: int) -> None:
        self._fd = fd
        self._pending: dict[int, asyncio.Future[Any]] = {}
        self._next_id = 1
        self._mailbox: deque[dict[str, Any]] = deque()
        self._stdout: Any = None
        self._stderr: Any = None
        self._tasks: dict[str, tuple[asyncio.Task[Any], int]] = {}
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
        if size > _MAX_FRAME_BYTES:
            raise FrameTooLarge(f"CONTROL frame is {size} bytes (limit {_MAX_FRAME_BYTES})")
        return self._decode(self._read_exact(size))
    def _write(self, value: dict[str, Any]) -> None:
        raw = json.dumps(value, separators=(",", ":")).encode()
        if len(raw) > _MAX_FRAME_BYTES:
            raise FrameTooLarge(f"CONTROL frame is {len(raw)} bytes (limit {_MAX_FRAME_BYTES})")
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
            task, thread_id = self._tasks.get(str(frame.body["invocation"]), (None, 0))
            if task is not None:
                task.cancel()
                _interrupt(thread_id)
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

    def track_dispatch(self, invocation: str, task: asyncio.Task[Any]) -> None:
        """Record the asyncio task and Python thread for cancellation escalation."""
        self._tasks[invocation] = (task, _thread_id())

    def settle_dispatch(self, invocation: str) -> None:
        """Forget a settled invocation's cancellation target."""
        self._tasks.pop(invocation, None)

def bootstrap(fd: int | None = None) -> Host:
    """Open the inherited CONTROL descriptor and install its explicit bridge."""
    if fd is None:
        fd = int(os.environ["OMP_EXT_CONTROL_FD"])
    host = Host(fd)
    host.install_capture()
    from . import _install_control_backend
    _install_control_backend(host)
    return host
