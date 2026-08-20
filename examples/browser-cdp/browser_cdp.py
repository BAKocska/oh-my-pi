from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import os
import shlex
import struct
from collections import deque
from collections.abc import AsyncIterator, Mapping
from dataclasses import dataclass
from typing import Annotated, Any
from urllib.parse import quote, urlsplit

import omp


_PROCESS_NAME = "examples.browser-cdp.browser"
_CHILD_PATHS = ("open", "eval", "snapshot", "screenshot", "close")
_WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
_INLINE_JSON_LIMIT = 64 * 1024
_PROCESS: omp.env.Process | None = None
_TARGET: _Target | None = None
_AVAILABLE = False
_WATCHERS: set[asyncio.Task[None]] = set()


@dataclass(frozen=True, slots=True)
class OpenArgs:
    """Describe a page navigation and its load deadline."""

    url: Annotated[str, omp.Field("Absolute URL to open.", example="https://example.com")]
    timeout_ms: Annotated[
        int,
        omp.Field("Maximum navigation wait in milliseconds.", example="30000"),
    ] = 30_000


@dataclass(frozen=True, slots=True)
class EvalArgs:
    """Describe JavaScript evaluated in the active page."""

    expression: Annotated[
        str,
        omp.Field("JavaScript expression to evaluate.", example="document.title"),
    ]
    await_promise: bool = True


@dataclass(frozen=True, slots=True)
class SnapshotArgs:
    """Bound an accessibility-tree snapshot of the active page."""

    max_nodes: Annotated[
        int,
        omp.Field("Maximum accessibility nodes to return.", example="500"),
    ] = 500


@dataclass(frozen=True, slots=True)
class ScreenshotArgs:
    """Describe a screenshot of the active page."""

    format: Annotated[
        str,
        omp.Field("Image encoding: png, jpeg, or webp.", example="png"),
    ] = "png"
    quality: Annotated[
        int | None,
        omp.Field("Lossy image quality from 0 through 100.", example="90"),
    ] = None
    full_page: bool = False


@dataclass(frozen=True, slots=True)
class CloseArgs:
    """Select the active page for closure."""

    target_id: str | None = None


@dataclass(frozen=True, slots=True)
class BrowserResult(omp.Payload):
    """Carry one structured Chrome DevTools Protocol result."""

    action: str
    target_id: str | None
    data: dict[str, object]
    artifact: omp.BlobPart | None = None


@dataclass(frozen=True, slots=True)
class BrowserScreenshot(omp.Payload):
    """Carry a media-typed, blob-backed page screenshot."""

    target_id: str
    media_type: str
    image: omp.BlobPart


@dataclass(frozen=True, slots=True)
class BrowserFault(omp.Fault):
    """Describe a bounded browser or CDP failure."""

    action: str
    detail: str


@dataclass(frozen=True, slots=True)
class _Target:
    target_id: str
    websocket_url: str
    url: str


class _CdpError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class _Endpoint:
    host: str
    port: int
    base_path: str

    @classmethod
    def parse(cls, value: str) -> _Endpoint:
        candidate = value if "://" in value else f"http://{value}"
        parsed = urlsplit(candidate)
        if parsed.scheme not in {"http", "tcp"} or parsed.hostname is None or parsed.port is None:
            raise _CdpError(f"unsupported browser process endpoint {value!r}")
        return cls(parsed.hostname, parsed.port, parsed.path.rstrip("/"))

    def path(self, suffix: str) -> str:
        return f"{self.base_path}{suffix}" or "/"


class _WebSocket:
    __slots__ = ("_reader", "_writer", "_fragment", "_fragment_opcode")

    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        self._reader = reader
        self._writer = writer
        self._fragment = bytearray()
        self._fragment_opcode: int | None = None

    @classmethod
    async def connect(cls, url: str) -> _WebSocket:
        parsed = urlsplit(url)
        if parsed.scheme != "ws" or parsed.hostname is None:
            raise _CdpError(f"unsupported CDP websocket URL {url!r}")
        port = parsed.port or 80
        reader, writer = await asyncio.open_connection(parsed.hostname, port)
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        path = parsed.path or "/"
        if parsed.query:
            path = f"{path}?{parsed.query}"
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {parsed.hostname}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        writer.write(request.encode("ascii"))
        await writer.drain()
        status, headers = await _read_headers(reader)
        expected = base64.b64encode(hashlib.sha1(f"{key}{_WS_GUID}".encode("ascii")).digest()).decode("ascii")
        if status != 101 or headers.get("sec-websocket-accept") != expected:
            writer.close()
            await writer.wait_closed()
            raise _CdpError(f"CDP websocket upgrade failed with HTTP {status}")
        return cls(reader, writer)

    async def send_json(self, value: Mapping[str, object]) -> None:
        await self._send_frame(0x1, json.dumps(value, separators=(",", ":")).encode("utf-8"))

    async def receive_json(self) -> Mapping[str, object]:
        while True:
            opcode, final, payload = await self._read_frame()
            if opcode == 0x8:
                raise _CdpError("CDP websocket closed")
            if opcode == 0x9:
                await self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:
                continue
            if opcode in {0x1, 0x2}:
                self._fragment.clear()
                self._fragment.extend(payload)
                self._fragment_opcode = opcode
            elif opcode == 0x0 and self._fragment_opcode is not None:
                self._fragment.extend(payload)
            else:
                raise _CdpError(f"unsupported websocket opcode {opcode}")
            if not final:
                continue
            if self._fragment_opcode != 0x1:
                raise _CdpError("CDP websocket returned non-text data")
            try:
                decoded = json.loads(self._fragment)
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                raise _CdpError("CDP websocket returned invalid JSON") from error
            self._fragment.clear()
            self._fragment_opcode = None
            if not isinstance(decoded, Mapping):
                raise _CdpError("CDP websocket returned a non-object message")
            return decoded

    async def close(self) -> None:
        if not self._writer.is_closing():
            try:
                await self._send_frame(0x8, struct.pack("!H", 1000))
            except (ConnectionError, asyncio.IncompleteReadError):
                pass
            self._writer.close()
            await self._writer.wait_closed()

    async def _send_frame(self, opcode: int, payload: bytes) -> None:
        size = len(payload)
        header = bytearray((0x80 | opcode,))
        if size < 126:
            header.append(0x80 | size)
        elif size < 65_536:
            header.append(0x80 | 126)
            header.extend(struct.pack("!H", size))
        else:
            header.append(0x80 | 127)
            header.extend(struct.pack("!Q", size))
        mask = os.urandom(4)
        header.extend(mask)
        masked = bytes(byte ^ mask[index & 3] for index, byte in enumerate(payload))
        self._writer.write(header + masked)
        await self._writer.drain()

    async def _read_frame(self) -> tuple[int, bool, bytes]:
        first, second = await self._reader.readexactly(2)
        final = bool(first & 0x80)
        opcode = first & 0x0F
        size = second & 0x7F
        if size == 126:
            size = struct.unpack("!H", await self._reader.readexactly(2))[0]
        elif size == 127:
            size = struct.unpack("!Q", await self._reader.readexactly(8))[0]
        mask = await self._reader.readexactly(4) if second & 0x80 else None
        payload = await self._reader.readexactly(size)
        if mask is not None:
            payload = bytes(byte ^ mask[index & 3] for index, byte in enumerate(payload))
        return opcode, final, payload


class _CdpSession:
    __slots__ = ("_socket", "_next_id", "_events")

    def __init__(self, socket: _WebSocket) -> None:
        self._socket = socket
        self._next_id = 1
        self._events: deque[Mapping[str, object]] = deque()

    @classmethod
    async def connect(cls, url: str) -> _CdpSession:
        return cls(await _WebSocket.connect(url))

    async def __aenter__(self) -> _CdpSession:
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self._socket.close()

    async def command(self, method: str, params: Mapping[str, object] | None = None) -> Mapping[str, object]:
        request_id = self._next_id
        self._next_id += 1
        request: dict[str, object] = {"id": request_id, "method": method}
        if params:
            request["params"] = dict(params)
        await self._socket.send_json(request)
        while True:
            message = await self._socket.receive_json()
            if message.get("id") != request_id:
                if isinstance(message.get("method"), str):
                    self._events.append(message)
                continue
            error = message.get("error")
            if isinstance(error, Mapping):
                raise _CdpError(str(error.get("message", f"CDP {method} failed")))
            result = message.get("result", {})
            if not isinstance(result, Mapping):
                raise _CdpError(f"CDP {method} returned a non-object result")
            return result

    async def next_event(self, timeout: float) -> Mapping[str, object]:
        if self._events:
            return self._events.popleft()
        async with asyncio.timeout(timeout):
            while True:
                message = await self._socket.receive_json()
                if isinstance(message.get("method"), str):
                    return message


async def _read_headers(reader: asyncio.StreamReader) -> tuple[int, dict[str, str]]:
    status_line = (await reader.readline()).decode("ascii", errors="replace").rstrip("\r\n")
    parts = status_line.split(" ", 2)
    if len(parts) < 2 or not parts[1].isdigit():
        raise _CdpError("browser endpoint returned an invalid HTTP status line")
    headers: dict[str, str] = {}
    while True:
        line = await reader.readline()
        if line in {b"\r\n", b"\n", b""}:
            return int(parts[1]), headers
        key, separator, value = line.decode("iso-8859-1").partition(":")
        if not separator:
            raise _CdpError("browser endpoint returned a malformed HTTP header")
        headers[key.strip().lower()] = value.strip()


async def _http(endpoint: _Endpoint, method: str, path: str) -> tuple[int, bytes]:
    reader, writer = await asyncio.open_connection(endpoint.host, endpoint.port)
    writer.write(
        (
            f"{method} {path} HTTP/1.1\r\n"
            f"Host: {endpoint.host}:{endpoint.port}\r\n"
            "Connection: close\r\n"
            "Content-Length: 0\r\n\r\n"
        ).encode("ascii")
    )
    await writer.drain()
    try:
        status, headers = await _read_headers(reader)
        length = headers.get("content-length")
        body = await reader.readexactly(int(length)) if length is not None else await reader.read()
    finally:
        writer.close()
        await writer.wait_closed()
    return status, body


async def _http_json(endpoint: _Endpoint, method: str, path: str) -> Mapping[str, object] | list[object]:
    status, body = await _http(endpoint, method, path)
    if not 200 <= status < 300:
        raise _CdpError(f"browser endpoint returned HTTP {status}")
    try:
        decoded = json.loads(body)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise _CdpError("browser endpoint returned invalid JSON") from error
    if not isinstance(decoded, (Mapping, list)):
        raise _CdpError("browser endpoint returned an unsupported JSON value")
    return decoded


def _endpoint() -> _Endpoint:
    if _PROCESS is None or not _AVAILABLE:
        raise _CdpError("browser process is unavailable")
    return _Endpoint.parse(_PROCESS.endpoint)


def _target_from(value: object) -> _Target | None:
    if not isinstance(value, Mapping):
        return None
    target_id = value.get("id")
    websocket_url = value.get("webSocketDebuggerUrl")
    url = value.get("url", "")
    if not isinstance(target_id, str) or not isinstance(websocket_url, str):
        return None
    return _Target(target_id, websocket_url, url if isinstance(url, str) else "")


async def _active_target() -> _Target:
    global _TARGET
    endpoint = _endpoint()
    rows = await _http_json(endpoint, "GET", endpoint.path("/json/list"))
    if not isinstance(rows, list):
        raise _CdpError("browser target list was not an array")
    targets = tuple(target for row in rows if (target := _target_from(row)) is not None)
    if _TARGET is not None:
        match = next((target for target in targets if target.target_id == _TARGET.target_id), None)
        if match is not None:
            _TARGET = match
            return match
    if not targets:
        raise _CdpError("browser has no open page target")
    _TARGET = targets[0]
    return targets[0]


def _fault(action: str, error: Exception) -> BrowserFault:
    return BrowserFault(action, str(error) or type(error).__name__)

def _result(
    action: str,
    target_id: str | None,
    data: dict[str, object],
) -> BrowserResult:
    encoded = json.dumps(
        data,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    if len(encoded) <= _INLINE_JSON_LIMIT:
        return BrowserResult(action, target_id, data)
    artifact = omp.Part.blob(
        omp.Spill(encoded, media_type="application/json"),
        f"Complete browser {action} result",
    )
    return BrowserResult(
        action,
        target_id,
        {"spilled": True, "bytes": len(encoded)},
        artifact,
    )



async def _browser_root(args: dict[str, object], ctx: omp.Context) -> BrowserResult:
    """Report the supervised browser process and active page state."""
    del args, ctx
    target_id = None if _TARGET is None else _TARGET.target_id
    generation = None if _PROCESS is None else _PROCESS.generation
    return BrowserResult("status", target_id, {"available": _AVAILABLE, "generation": generation})


browser = omp.device(
    "browser",
    family="cdp",
    rev=1,
    place="host",
    summary="Navigate, inspect, evaluate, capture, and close pages through supervised CDP.",
    available=lambda: omp.Availability(_AVAILABLE, None if _AVAILABLE else "browser process is down"),
    effects=omp.Effects(exec=omp.ExecEffects(network=True)),
    tier=omp.Tier.WRITE,
)(_browser_root)


@browser.subtool("open")
async def browser_open(args: OpenArgs, ctx: omp.Context) -> AsyncIterator[object]:
    """Open a URL and stream navigation lifecycle updates until load."""
    del ctx
    global _TARGET
    if args.timeout_ms < 1 or args.timeout_ms > 300_000:
        yield omp.Done(BrowserFault("open", "timeout_ms must be between 1 and 300000"))
        return
    try:
        endpoint = _endpoint()
        yield omp.Update(stage="creating_target", url=args.url)
        created = await _http_json(
            endpoint,
            "PUT",
            endpoint.path(f"/json/new?{quote(args.url, safe='')}")
        )
        target = _target_from(created)
        if target is None:
            raise _CdpError("browser did not return a debuggable page target")
        _TARGET = target
        yield omp.Update(stage="target_created", target_id=target.target_id)
        async with await _CdpSession.connect(target.websocket_url) as cdp:
            await cdp.command("Page.enable")
            await cdp.command("Page.setLifecycleEventsEnabled", {"enabled": True})
            yield omp.Update(stage="navigating", target_id=target.target_id)
            navigation = await cdp.command("Page.navigate", {"url": args.url})
            error_text = navigation.get("errorText")
            if isinstance(error_text, str) and error_text:
                raise _CdpError(error_text)
            loop = asyncio.get_running_loop()
            deadline = loop.time() + args.timeout_ms / 1000
            while True:
                remaining = deadline - loop.time()
                if remaining <= 0:
                    raise TimeoutError("navigation load deadline exceeded")
                event = await cdp.next_event(remaining)
                method = event.get("method")
                params = event.get("params", {})
                if method == "Page.loadEventFired":
                    yield omp.Update(stage="loaded", target_id=target.target_id)
                    break
                if method == "Page.lifecycleEvent" and isinstance(params, Mapping):
                    name = params.get("name")
                    if isinstance(name, str):
                        yield omp.Update(stage="loading", event=name, target_id=target.target_id)
        yield omp.Done(
            BrowserResult(
                "open",
                target.target_id,
                {"url": args.url, "frame_id": navigation.get("frameId")},
            )
        )
    except (OSError, ValueError, asyncio.TimeoutError, _CdpError) as error:
        yield omp.Done(_fault("open", error))


@browser.subtool("eval")
async def browser_eval(args: EvalArgs, ctx: omp.Context) -> BrowserResult | BrowserFault:
    """Evaluate JavaScript in the active page and return its value."""
    del ctx
    try:
        target = await _active_target()
        async with await _CdpSession.connect(target.websocket_url) as cdp:
            response = await cdp.command(
                "Runtime.evaluate",
                {
                    "expression": args.expression,
                    "awaitPromise": args.await_promise,
                    "returnByValue": True,
                    "userGesture": True,
                },
            )
        exception = response.get("exceptionDetails")
        if isinstance(exception, Mapping):
            text = exception.get("text", "JavaScript evaluation failed")
            return BrowserFault("eval", str(text))
        remote = response.get("result", {})
        if not isinstance(remote, Mapping):
            raise _CdpError("Runtime.evaluate returned a malformed result")
        value = remote.get("value", remote.get("description"))
        return _result(
            "eval",
            target.target_id,
            {"type": remote.get("type"), "subtype": remote.get("subtype"), "value": value},
        )
    except (OSError, ValueError, asyncio.TimeoutError, _CdpError) as error:
        return _fault("eval", error)


@browser.subtool("snapshot")
async def browser_snapshot(args: SnapshotArgs, ctx: omp.Context) -> BrowserResult | BrowserFault:
    """Return a bounded accessibility-tree snapshot of the active page."""
    del ctx
    if args.max_nodes < 1 or args.max_nodes > 5_000:
        return BrowserFault("snapshot", "max_nodes must be between 1 and 5000")
    try:
        target = await _active_target()
        async with await _CdpSession.connect(target.websocket_url) as cdp:
            response = await cdp.command("Accessibility.getFullAXTree")
        raw_nodes = response.get("nodes", ())
        if not isinstance(raw_nodes, list):
            raise _CdpError("Accessibility.getFullAXTree returned malformed nodes")
        nodes = [dict(node) for node in raw_nodes[: args.max_nodes] if isinstance(node, Mapping)]
        return _result(
            "snapshot",
            target.target_id,
            {"nodes": nodes, "total_nodes": len(raw_nodes), "truncated": len(raw_nodes) > len(nodes)},
        )
    except (OSError, ValueError, asyncio.TimeoutError, _CdpError) as error:
        return _fault("snapshot", error)


@browser.subtool("screenshot")
async def browser_screenshot(args: ScreenshotArgs, ctx: omp.Context) -> BrowserScreenshot | BrowserFault:
    """Capture the active page as a media-typed blob part."""
    del ctx
    image_format = args.format.lower()
    if image_format not in {"png", "jpeg", "webp"}:
        return BrowserFault("screenshot", "format must be png, jpeg, or webp")
    if args.quality is not None and not 0 <= args.quality <= 100:
        return BrowserFault("screenshot", "quality must be between 0 and 100")
    try:
        target = await _active_target()
        params: dict[str, object] = {
            "format": image_format,
            "fromSurface": True,
            "captureBeyondViewport": args.full_page,
        }
        if args.quality is not None and image_format != "png":
            params["quality"] = args.quality
        async with await _CdpSession.connect(target.websocket_url) as cdp:
            response = await cdp.command("Page.captureScreenshot", params)
        encoded = response.get("data")
        if not isinstance(encoded, str):
            raise _CdpError("Page.captureScreenshot omitted image data")
        try:
            image = base64.b64decode(encoded, validate=True)
        except ValueError as error:
            raise _CdpError("Page.captureScreenshot returned invalid base64") from error
        media_type = f"image/{image_format}"
        part = omp.Part.blob(
            omp.Spill(image, media_type=media_type),
            f"Browser screenshot of {target.url or target.target_id}",
        )
        return BrowserScreenshot(target.target_id, media_type, part)
    except (OSError, ValueError, asyncio.TimeoutError, _CdpError) as error:
        return _fault("screenshot", error)


@browser.subtool("close")
async def browser_close(args: CloseArgs, ctx: omp.Context) -> BrowserResult | BrowserFault:
    """Close the active page target without killing its supervised browser."""
    del ctx
    global _TARGET
    try:
        target = await _active_target()
        target_id = args.target_id or target.target_id
        endpoint = _endpoint()
        status, body = await _http(endpoint, "GET", endpoint.path(f"/json/close/{quote(target_id, safe='')}"))
        if not 200 <= status < 300:
            raise _CdpError(f"browser close returned HTTP {status}")
        if _TARGET is not None and _TARGET.target_id == target_id:
            _TARGET = None
        return BrowserResult("close", target_id, {"closed": True, "message": body.decode("utf-8", errors="replace")})
    except (OSError, ValueError, asyncio.TimeoutError, _CdpError) as error:
        return _fault("close", error)


async def _publish_availability(mounted: bool, reason: str | None = None) -> None:
    global _AVAILABLE
    _AVAILABLE = mounted
    paths = ("browser", *(f"browser/{child}" for child in _CHILD_PATHS))
    await omp.devices.set_availability(
        *(omp.AvailabilityDelta(path, mounted, None if mounted else reason) for path in paths)
    )


def _state_name(value: object) -> str:
    raw = value.get("state", value) if isinstance(value, Mapping) else getattr(value, "state", value)
    raw = getattr(raw, "value", raw)
    return str(raw).lower()


async def _watch_process(process: omp.env.Process) -> None:
    global _PROCESS
    try:
        async for state in process.states():
            name = _state_name(state)
            if name in {"ready", "running"}:
                generation = state.get("generation") if isinstance(state, Mapping) else getattr(state, "generation", None)
                if isinstance(generation, int) and generation != process.generation:
                    process = omp.env.Process(process.name, generation)
                    _PROCESS = process
                await _publish_availability(True)
            elif name in {"exited", "stopped", "failed"}:
                await _publish_availability(False, f"browser process is {name}")
        await _publish_availability(False, "browser process state stream ended")
    except Exception as error:
        await _publish_availability(False, f"browser process watch failed: {error}")


def _browser_script(executable: str, port: int, headless: bool) -> str:
    argv = [
        executable,
        "--remote-debugging-address=127.0.0.1",
        f"--remote-debugging-port={port}",
        "--user-data-dir=${TMPDIR:-/tmp}/omp-browser-cdp",
        "--no-first-run",
        "--no-default-browser-check",
    ]
    if headless:
        argv.append("--headless=new")
    argv.append("about:blank")
    rendered = shlex.join(argv)
    return rendered.replace("'--user-data-dir=${TMPDIR:-/tmp}/omp-browser-cdp'", "--user-data-dir=${TMPDIR:-/tmp}/omp-browser-cdp")


@omp.hook("extension_activate")
async def activate_browser(event: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    """Ensure the named browser process and publish its child availability atomically."""
    del event
    global _PROCESS
    omp.env.require(omp.env.Capability.PROCESS)
    executable = ctx.settings.get("executable", "chromium")
    debug_port = ctx.settings.get("debug_port", 9222)
    headless = ctx.settings.get("headless", True)
    if not isinstance(executable, str) or not executable:
        raise ValueError("browser executable setting must be a non-empty string")
    if isinstance(debug_port, bool) or not isinstance(debug_port, int) or not 1024 <= debug_port <= 65535:
        raise ValueError("browser debug_port setting must be between 1024 and 65535")
    if not isinstance(headless, bool):
        raise ValueError("browser headless setting must be bool")
    process = await omp.env.proc.ensure(
        _PROCESS_NAME,
        _browser_script(executable, debug_port, headless),
        restart=omp.env.RestartPolicy(omp.Restart.ON_FAILURE),
        ready=omp.env.ReadyTcp(debug_port, timeout=omp.Duration("30s")),
    )
    _PROCESS = process
    await _publish_availability(True)
    watcher = asyncio.create_task(_watch_process(process), name="browser-cdp:process-state")
    _WATCHERS.add(watcher)
    watcher.add_done_callback(_WATCHERS.discard)
