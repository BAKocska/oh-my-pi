from __future__ import annotations

import json
import subprocess
from collections.abc import AsyncIterator, Iterator, Mapping
from dataclasses import dataclass
from typing import Any

import omp


@dataclass(frozen=True, slots=True)
class ComputerResult(omp.Payload):
    """Carry one desktop result and an optional blob-backed image part."""

    action: str
    data: dict[str, object]
    image: omp.BlobPart | None = None


@dataclass(frozen=True, slots=True)
class ComputerFault(omp.Fault):
    """Describe a native-driver rejection without losing its action."""

    action: str
    detail: str


class _NativeDriver:
    __slots__ = ("_process",)

    def __init__(self) -> None:
        self._process = subprocess.Popen(
            ("computer-driver", "--stdio"),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            bufsize=0,
        )

    def frames(self, action: str, arguments: Mapping[str, object]) -> Iterator[Mapping[str, object]]:
        process = self._process
        if process.poll() is not None:
            raise RuntimeError(f"computer-driver exited with status {process.returncode}")
        if process.stdin is None or process.stdout is None:
            raise RuntimeError("computer-driver stdio is unavailable")
        request = json.dumps(
            {"action": action, "arguments": dict(arguments)},
            separators=(",", ":"),
        ).encode("utf-8")
        process.stdin.write(request + b"\n")
        process.stdin.flush()
        while True:
            line = process.stdout.readline()
            if not line:
                raise RuntimeError("computer-driver closed stdout")
            decoded = json.loads(line)
            if not isinstance(decoded, Mapping):
                raise RuntimeError("computer-driver emitted a non-object frame")
            frame = dict(decoded)
            if frame.get("kind") == "image":
                size = frame.get("bytes")
                if not isinstance(size, int) or size < 0:
                    raise RuntimeError("computer-driver emitted an invalid image length")
                frame["data"] = self._read_exact(size)
            yield frame
            if frame.get("kind") in {"done", "error", "image"}:
                return

    def _read_exact(self, size: int) -> bytes:
        stdout = self._process.stdout
        if stdout is None:
            raise RuntimeError("computer-driver stdout is unavailable")
        buffer = bytearray(size)
        view = memoryview(buffer)
        offset = 0
        while offset < size:
            read = stdout.readinto(view[offset:])
            if not read:
                raise RuntimeError("computer-driver truncated an image frame")
            offset += read
        separator = stdout.read(1)
        if separator != b"\n":
            raise RuntimeError("computer-driver image frame omitted its delimiter")
        return bytes(buffer)


_DRIVER: _NativeDriver | None = None


def _boot_driver() -> None:
    global _DRIVER
    _DRIVER = _NativeDriver()


def _driver() -> _NativeDriver:
    if _DRIVER is None:
        raise RuntimeError("computer-driver worker boot did not complete")
    return _DRIVER


def _value(frame: Mapping[str, object], action: str) -> ComputerResult | ComputerFault:
    if frame.get("kind") == "error":
        return ComputerFault(action, str(frame.get("detail", "native driver failed")))
    result = frame.get("result", {})
    if not isinstance(result, Mapping):
        return ComputerFault(action, "native driver returned a non-object result")
    return ComputerResult(action, dict(result))


async def _one_shot(
    action: str, args: Mapping[str, object]
) -> ComputerResult | ComputerFault:
    for frame in _driver().frames(action, args):
        if frame.get("kind") != "update":
            return _value(frame, action)
    return ComputerFault(action, "native driver ended without a result")


async def _interaction(
    action: str, args: Mapping[str, object]
) -> AsyncIterator[object]:
    for frame in _driver().frames(action, args):
        kind = frame.get("kind")
        if kind == "update":
            update = frame.get("update", {})
            if not isinstance(update, Mapping):
                yield omp.Done(ComputerFault(action, "native driver returned a malformed update"))
                return
            yield omp.Update(dict(update))
            continue
        yield omp.Done(_value(frame, action))
        return
    yield omp.Done(ComputerFault(action, "native driver ended without a terminal frame"))


async def _screenshot(args: Mapping[str, object], ctx: omp.Context) -> ComputerResult | ComputerFault:
    del ctx
    for frame in _driver().frames("screenshot", args):
        if frame.get("kind") == "error":
            return ComputerFault("screenshot", str(frame.get("detail", "capture failed")))
        if frame.get("kind") != "image":
            continue
        image = frame.get("data")
        media_type = frame.get("media_type", "image/png")
        if not isinstance(image, bytes) or not isinstance(media_type, str):
            return ComputerFault("screenshot", "native driver returned a malformed image")
        part = omp.Part.blob(
            omp.Spill(image, media_type=media_type),
            "Desktop screenshot",
        )
        return ComputerResult("screenshot", {"media_type": media_type}, part)
    return ComputerFault("screenshot", "native driver ended without an image")


async def _click(args: Mapping[str, object], ctx: omp.Context) -> AsyncIterator[object]:
    del ctx
    async for event in _interaction("click", args):
        yield event


async def _type(args: Mapping[str, object], ctx: omp.Context) -> AsyncIterator[object]:
    del ctx
    async for event in _interaction("type", args):
        yield event


async def _scroll(args: Mapping[str, object], ctx: omp.Context) -> AsyncIterator[object]:
    del ctx
    async for event in _interaction("scroll", args):
        yield event


async def _window_list(args: Mapping[str, object], ctx: omp.Context) -> ComputerResult | ComputerFault:
    del ctx
    return await _one_shot("window/list", args)


async def _window_focus(args: Mapping[str, object], ctx: omp.Context) -> AsyncIterator[object]:
    del ctx
    async for event in _interaction("window/focus", args):
        yield event


async def _window_move(args: Mapping[str, object], ctx: omp.Context) -> AsyncIterator[object]:
    del ctx
    async for event in _interaction("window/move", args):
        yield event


async def _window_resize(args: Mapping[str, object], ctx: omp.Context) -> AsyncIterator[object]:
    del ctx
    async for event in _interaction("window/resize", args):
        yield event


async def _window_close(args: Mapping[str, object], ctx: omp.Context) -> AsyncIterator[object]:
    del ctx
    async for event in _interaction("window/close", args):
        yield event


def _object_schema(properties: Mapping[str, object], required: tuple[str, ...] = ()) -> dict[str, object]:
    return {
        "type": "object",
        "properties": dict(properties),
        "required": list(required),
        "additionalProperties": False,
    }


_NUMBER = {"type": "number"}
_INTEGER = {"type": "integer"}
_STRING = {"type": "string"}
_SCREENSHOT_SCHEMA = _object_schema(
    {
        "display": _INTEGER,
        "window_id": _STRING,
        "format": {"type": "string", "enum": ["png", "jpeg"]},
    }
)
_CLICK_SCHEMA = _object_schema(
    {"x": _NUMBER, "y": _NUMBER, "button": {"type": "string", "enum": ["left", "middle", "right"]}, "count": _INTEGER},
    ("x", "y"),
)
_TYPE_SCHEMA = _object_schema({"text": _STRING, "interval_ms": _INTEGER}, ("text",))
_SCROLL_SCHEMA = _object_schema(
    {"x": _NUMBER, "y": _NUMBER, "delta_x": _NUMBER, "delta_y": _NUMBER},
    ("delta_y",),
)
_WINDOW_ID_SCHEMA = _object_schema({"window_id": _STRING}, ("window_id",))
_WINDOW_MOVE_SCHEMA = _object_schema(
    {"window_id": _STRING, "x": _INTEGER, "y": _INTEGER},
    ("window_id", "x", "y"),
)
_WINDOW_RESIZE_SCHEMA = _object_schema(
    {"window_id": _STRING, "width": _INTEGER, "height": _INTEGER},
    ("window_id", "width", "height"),
)


_COMPUTER = omp.devices.parent("computer", family="cu", rev=1, place="worker:driver")

omp.workers.declare(
    omp.WorkerSpec(
        name="driver",
        site=omp.Site.LOCAL,
        boot=_boot_driver,
        idle_ttl=omp.Duration("10m"),
        restart=omp.Restart.ON_FAILURE,
        max_concurrency=1,
    )
)


@omp.hook("extension_activate")
async def extension_activate(event: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    """Mount the fixed desktop-control subtree when the extension activates."""

    del event, ctx
    await _COMPUTER.mount_many(
        omp.MountSpec("screenshot", _screenshot, _SCREENSHOT_SCHEMA, "Capture a display or window as blob-backed image media."),
        omp.MountSpec("click", _click, _CLICK_SCHEMA, "Click a desktop coordinate and stream driver progress."),
        omp.MountSpec("type", _type, _TYPE_SCHEMA, "Type text and stream driver progress."),
        omp.MountSpec("scroll", _scroll, _SCROLL_SCHEMA, "Scroll at an optional coordinate and stream driver progress."),
        omp.MountSpec("window/list", _window_list, _object_schema({}), "List visible desktop windows."),
        omp.MountSpec("window/focus", _window_focus, _WINDOW_ID_SCHEMA, "Focus a window and stream driver progress."),
        omp.MountSpec("window/move", _window_move, _WINDOW_MOVE_SCHEMA, "Move a window and stream driver progress."),
        omp.MountSpec("window/resize", _window_resize, _WINDOW_RESIZE_SCHEMA, "Resize a window and stream driver progress."),
        omp.MountSpec("window/close", _window_close, _WINDOW_ID_SCHEMA, "Close a window and stream driver progress."),
    )


@omp.renderer("computer", family="cu", rev=1)
def render_computer(
    view: omp.View[object, ComputerResult, ComputerFault], ctx: omp.ui.RenderCtx
) -> omp.ui.Tml | None:
    """Render screenshots as terminal image parts and other results as compact JSON."""

    del ctx
    if view.verdict is None:
        return omp.ui.tml("<row>{icon} operating desktop</row>", icon=omp.ui.icon("monitor"))
    match view.verdict:
        case omp.Ok(ComputerResult(image=image)) if image is not None:
            return omp.ui.image(image.blob, trim=True)
        case omp.Ok(ComputerResult(action=action, data=data)):
            return omp.ui.md(f"**{action}**\n\n```json\n{json.dumps(data, sort_keys=True)}\n```")
        case omp.Faulted(ComputerFault(action=action, detail=detail)):
            return omp.ui.md(f"**{action} failed:** {detail}")
        case _:
            return None
