from __future__ import annotations

import asyncio
import re
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from typing import Literal, Mapping

import omp


_PROCESS_PREFIX = "examples-unified-exec."
_MAX_POLL_BYTES = 64 * 1024
_MAX_POLL_FRAMES = 128
_QUIET_SECONDS = 0.05
_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}\Z")
_KEYS: Mapping[str, bytes] = {
    "ENTER": b"\r",
    "TAB": b"\t",
    "ESCAPE": b"\x1b",
    "BACKSPACE": b"\x7f",
    "UP": b"\x1b[A",
    "DOWN": b"\x1b[B",
    "RIGHT": b"\x1b[C",
    "LEFT": b"\x1b[D",
    "HOME": b"\x1b[H",
    "END": b"\x1b[F",
    "DELETE": b"\x1b[3~",
    "CTRL_C": b"\x03",
    "CTRL_D": b"\x04",
    "CTRL_Z": b"\x1a",
}


@dataclass(frozen=True, slots=True)
class SessionArgs:
    """Select and configure one supervised interactive-session operation."""

    op: Literal["open", "poll", "stdin", "signal", "close"]
    name: str
    script: str | None = None
    cursor: int = 0
    text: str | None = None
    keys: list[str] = field(default_factory=list)
    signal: str | None = None
    rows: int = 24
    columns: int = 80
    terminal: str = "xterm-256color"
    ready_pattern: str | None = None
    ready_timeout: str = "30s"
    max_bytes: int = 16 * 1024


@dataclass(frozen=True, slots=True)
class SessionResult:
    """Bounded output and state returned by an interactive-session operation."""

    op: str
    name: str
    generation: int | None
    state: str
    cursor: int
    output: str = ""
    output_bytes: int = 0
    artifact: omp.BlobRef | None = None


def _process_name(name: str) -> str:
    """Validate a caller-facing name and place it in this extension's namespace."""

    if not _NAME.fullmatch(name):
        raise ValueError("name must be 1-64 letters, digits, dots, underscores, or hyphens")
    return f"{_PROCESS_PREFIX}{name}"


def _key_bytes(keys: list[str]) -> bytes:
    """Encode documented terminal key names for the named-process send surface."""

    encoded = bytearray()
    for key in keys:
        try:
            encoded.extend(_KEYS[key.upper()])
        except KeyError as error:
            names = ", ".join(_KEYS)
            raise ValueError(f"unknown key {key!r}; expected one of: {names}") from error
    return bytes(encoded)


def _state(value: object) -> str:
    """Project a process-state enum or wire mapping to stable text."""

    if isinstance(value, Mapping):
        state = value.get("state", "unknown")
    else:
        state = getattr(value, "state", "unknown")
    return str(getattr(state, "value", state))


async def _next_frame(
    frames: AsyncIterator[omp.env.ProcessOutput], timeout: float
) -> omp.env.ProcessOutput | None:
    """Read one immediately available frame, ending a poll after a short quiet period."""

    try:
        async with asyncio.timeout(timeout):
            return await anext(frames)
    except (StopAsyncIteration, TimeoutError):
        return None


async def _poll(
    process: omp.env.Process, *, after: int, max_bytes: int
) -> tuple[int, str, int, omp.BlobRef | None]:
    """Collect a bounded cursor window and stream oversized output into blob storage."""

    if after < 0:
        raise ValueError("cursor must be non-negative")
    if not 1 <= max_bytes <= _MAX_POLL_BYTES:
        raise ValueError(f"max_bytes must be between 1 and {_MAX_POLL_BYTES}")

    cursor = after
    total = 0
    preview = bytearray()
    writer = None
    artifact = None
    frames = process.output(after=after)

    try:
        for _ in range(_MAX_POLL_FRAMES):
            frame = await _next_frame(frames, _QUIET_SECONDS)
            if frame is None:
                break
            cursor = max(cursor, frame.sequence)
            chunk = frame.data
            total += len(chunk)
            if len(preview) < max_bytes:
                preview.extend(chunk[: max_bytes - len(preview)])
            if total > max_bytes and writer is None:
                writer = omp.env.blobs.writer()
                await writer.write(bytes(preview))
                already_written = len(preview)
                if already_written < total:
                    await writer.write(chunk[len(chunk) - (total - already_written) :])
            elif writer is not None:
                await writer.write(chunk)
        if writer is not None:
            artifact = await writer.commit()
    finally:
        if writer is not None and artifact is None:
            writer.abort()
        closer = getattr(frames, "aclose", None)
        if closer is not None:
            await closer()

    return cursor, preview.decode("utf-8", errors="replace"), total, artifact


async def _live(name: str) -> omp.env.Process:
    """Adopt a live Environment-owned session or fail without restarting it."""

    process = await omp.env.proc.adopt(_process_name(name))
    if process is None:
        raise ValueError(f"session {name!r} is not open")
    return process


@omp.device("session", family="unified-exec", rev=1, place="host")
async def session(args: SessionArgs, ctx: omp.Context) -> SessionResult:
    """Open, poll, drive, signal, or close one Environment-owned PTY process."""

    del ctx
    omp.env.require(omp.env.Capability.PROCESS)

    if args.op == "open":
        if not args.script or not args.script.strip():
            raise ValueError("open requires a non-empty script")
        if args.rows <= 0 or args.columns <= 0 or not args.terminal:
            raise ValueError("PTY rows, columns, and terminal must be positive/non-empty")
        process = await omp.env.proc.ensure(
            _process_name(args.name),
            args.script,
            pty={
                "rows": args.rows,
                "columns": args.columns,
                "terminal": args.terminal,
            },
            ready=(
                omp.env.ReadyLog(
                    args.ready_pattern,
                    timeout=omp.Duration(args.ready_timeout),
                )
                if args.ready_pattern
                else None
            ),
            restart=omp.env.RestartPolicy(omp.Restart.NO),
        )
        info = await process.info()
        return SessionResult(
            op=args.op,
            name=args.name,
            generation=process.generation,
            state=_state(info),
            cursor=args.cursor,
        )

    if args.op == "poll":
        process = await _live(args.name)
        cursor, output, output_bytes, artifact = await _poll(
            process, after=args.cursor, max_bytes=args.max_bytes
        )
        info = await process.info()
        return SessionResult(
            op=args.op,
            name=args.name,
            generation=process.generation,
            state=_state(info),
            cursor=cursor,
            output=output,
            output_bytes=output_bytes,
            artifact=artifact,
        )

    if args.op == "stdin":
        process = await _live(args.name)
        data = (args.text or "").encode() + _key_bytes(args.keys)
        if not data:
            raise ValueError("stdin requires text or at least one key")
        await process.send(data)
        info = await process.info()
        return SessionResult(args.op, args.name, process.generation, _state(info), args.cursor)

    if args.op == "signal":
        if not args.signal:
            raise ValueError("signal requires a signal name")
        process = await _live(args.name)
        await process.signal(args.signal.upper())
        info = await process.info()
        return SessionResult(args.op, args.name, process.generation, _state(info), args.cursor)

    if args.op == "close":
        process = await omp.env.proc.adopt(_process_name(args.name))
        if process is None:
            return SessionResult(args.op, args.name, None, "closed", args.cursor)
        info = await process.stop()
        return SessionResult(args.op, args.name, process.generation, _state(info), args.cursor)

    raise ValueError(f"unsupported session operation: {args.op!r}")
