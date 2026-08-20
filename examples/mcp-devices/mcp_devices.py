from __future__ import annotations

import asyncio
import json
import re
import shlex
from collections.abc import AsyncIterator, Mapping
from dataclasses import dataclass
from typing import Any

import omp
from omp import Context
from omp import ExtensionActivateEvent, devices, hook


_PROTOCOL_VERSION = "2025-06-18"
_CLIENTS: dict[str, _JsonRpcClient] = {}
_WATCHERS: set[asyncio.Task[None]] = set()
_MCP_DEVICES = devices.parent("mcp", family="mcp", rev=1, place="host")


@dataclass(frozen=True, slots=True)
class _ServerConfig:
    name: str
    command: str
    args: tuple[str, ...]
    env: Mapping[str, str]


@dataclass(frozen=True, slots=True)
class _McpTool:
    name: str
    description: str
    input_schema: Mapping[str, object]


class _JsonRpcClient:
    def __init__(self, process: omp.env.Process) -> None:
        self.process = process
        self.available = False
        self._next_id = 1
        self._lock = asyncio.Lock()
        self._output: AsyncIterator[Any] = process.output()
        self._buffer = bytearray()

    async def initialize(self) -> tuple[_McpTool, ...]:
        async with self._lock:
            await self._request_unlocked(
                "initialize",
                {
                    "protocolVersion": _PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "omp-mcp-devices", "version": "0.1.0"},
                },
            )
            await self._notify_unlocked("notifications/initialized", {})
            tools = await self._list_tools_unlocked()
            self.available = True
            return tools

    async def call(self, tool: str, arguments: Mapping[str, object]) -> object:
        if not self.available:
            raise RuntimeError(f"MCP server {self.process.name!r} is unavailable")
        async with self._lock:
            return await self._request_unlocked(
                "tools/call", {"name": tool, "arguments": dict(arguments)}
            )

    def mark_unavailable(self) -> None:
        self.available = False

    async def _list_tools_unlocked(self) -> tuple[_McpTool, ...]:
        found: list[_McpTool] = []
        cursor: str | None = None
        while True:
            params: dict[str, object] = {}
            if cursor is not None:
                params["cursor"] = cursor
            result = await self._request_unlocked("tools/list", params)
            if not isinstance(result, Mapping):
                raise RuntimeError("MCP tools/list returned a non-object result")
            raw_tools = result.get("tools", ())
            if not isinstance(raw_tools, list):
                raise RuntimeError("MCP tools/list returned a non-list tools field")
            for raw in raw_tools:
                if not isinstance(raw, Mapping) or not isinstance(raw.get("name"), str):
                    raise RuntimeError("MCP tools/list returned a malformed tool")
                schema = raw.get("inputSchema", {"type": "object"})
                if not isinstance(schema, Mapping):
                    raise RuntimeError("MCP tool inputSchema must be an object")
                description = raw.get("description", "")
                found.append(
                    _McpTool(
                        name=raw["name"],
                        description=description if isinstance(description, str) else "",
                        input_schema=dict(schema),
                    )
                )
            next_cursor = result.get("nextCursor")
            if not isinstance(next_cursor, str) or not next_cursor:
                return tuple(found)
            cursor = next_cursor

    async def _request_unlocked(self, method: str, params: Mapping[str, object]) -> object:
        request_id = self._next_id
        self._next_id += 1
        await self.process.send(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": dict(params),
                },
                separators=(",", ":"),
            ).encode("utf-8")
            + b"\n"
        )
        while True:
            message = await self._read_message()
            if message.get("id") != request_id:
                continue
            error = message.get("error")
            if error is not None:
                raise RuntimeError(f"MCP {method} failed: {error!r}")
            return message.get("result")

    async def _notify_unlocked(self, method: str, params: Mapping[str, object]) -> None:
        await self.process.send(
            json.dumps(
                {"jsonrpc": "2.0", "method": method, "params": dict(params)},
                separators=(",", ":"),
            ).encode("utf-8")
            + b"\n"
        )

    async def _read_message(self) -> Mapping[str, object]:
        while True:
            framed = self._take_frame()
            if framed is not None:
                value = json.loads(framed)
                if not isinstance(value, Mapping):
                    raise RuntimeError("MCP server emitted a non-object JSON-RPC message")
                return value
            event = await anext(self._output)
            channel = _field(event, "channel", "stdout")
            channel_name = str(_field(channel, "value", channel)).lower()
            if channel_name not in {"stdout", "out", "1"}:
                continue
            data = _field(event, "data", b"")
            if isinstance(data, str):
                data = data.encode("utf-8")
            if not isinstance(data, bytes):
                raise RuntimeError("named-process output did not contain bytes")
            self._buffer.extend(data)

    def _take_frame(self) -> bytes | None:
        while self._buffer.startswith((b"\r\n", b"\n")):
            del self._buffer[: 2 if self._buffer.startswith(b"\r\n") else 1]
        if self._buffer.lower().startswith(b"content-length:"):
            marker = self._buffer.find(b"\r\n\r\n")
            if marker < 0:
                return None
            header = bytes(self._buffer[:marker]).decode("ascii")
            fields = dict(
                line.split(":", 1) for line in header.split("\r\n") if ":" in line
            )
            length_text = next(
                (value for key, value in fields.items() if key.lower() == "content-length"),
                None,
            )
            if length_text is None:
                raise RuntimeError("MCP frame omitted Content-Length")
            length = int(length_text.strip())
            end = marker + 4 + length
            if len(self._buffer) < end:
                return None
            payload = bytes(self._buffer[marker + 4 : end])
            del self._buffer[:end]
            return payload
        newline = self._buffer.find(b"\n")
        if newline < 0:
            return None
        payload = bytes(self._buffer[:newline]).rstrip(b"\r")
        del self._buffer[: newline + 1]
        return payload or None


def _load_configs(value: object) -> tuple[_ServerConfig, ...]:
    if not isinstance(value, str):
        raise TypeError("the servers setting must be a JSON object encoded as a string")
    decoded = json.loads(value)
    if not isinstance(decoded, Mapping):
        raise ValueError("the servers setting must decode to an object")
    configs: list[_ServerConfig] = []
    for name, raw in decoded.items():
        if not isinstance(name, str) or not isinstance(raw, Mapping):
            raise ValueError("each MCP server must be a named object")
        if raw.get("disabled", False):
            continue
        command = raw.get("command")
        args = raw.get("args", ())
        process_env = raw.get("env", {})
        if not isinstance(command, str) or not command:
            raise ValueError(f"MCP server {name!r} requires a command")
        if not isinstance(args, list) or not all(isinstance(arg, str) for arg in args):
            raise ValueError(f"MCP server {name!r} args must be a string list")
        if not isinstance(process_env, Mapping) or not all(
            isinstance(key, str) and isinstance(item, str)
            for key, item in process_env.items()
        ):
            raise ValueError(f"MCP server {name!r} env must map strings to strings")
        configs.append(_ServerConfig(name, command, tuple(args), dict(process_env)))
    return tuple(configs)


def _field(value: object, name: str, default: object = None) -> object:
    if isinstance(value, Mapping):
        return value.get(name, default)
    return getattr(value, name, default)


def _segment(value: str) -> str:
    segment = re.sub(r"[^a-z0-9_]", "_", value.lower()).strip("_")
    if not segment:
        raise ValueError(f"MCP name {value!r} has no usable device-path characters")
    if segment[0].isdigit():
        segment = f"mcp_{segment}"
    return segment[:64]


def _device_body(client: _JsonRpcClient, tool: _McpTool):
    async def invoke(args: dict[str, object], ctx: Context) -> object:
        """Call one mounted MCP endpoint with its decoded arguments."""
        del ctx
        return await client.call(tool.name, args)

    invoke.__name__ = _segment(tool.name)
    invoke.__doc__ = tool.description or f"Call the MCP tool {tool.name}."
    return invoke


async def _set_available(
    paths: tuple[str, ...], mounted: bool, reason: str | None = None
) -> None:
    await devices.set_availability(
        *(omp.AvailabilityDelta(path, mounted, reason) for path in paths)
    )


async def _watch_server(client: _JsonRpcClient, paths: tuple[str, ...]) -> None:
    async for state in client.process.states():
        raw_state = _field(state, "state", state)
        state_name = str(_field(raw_state, "value", raw_state)).lower()
        if state_name in {"ready", "running"}:
            if not client.available:
                try:
                    await client.initialize()
                except Exception as error:
                    await _set_available(
                        paths, False, f"MCP reconnect failed: {error}"
                    )
                    continue
            await _set_available(paths, True)
        elif state_name in {"exited", "stopped", "failed"}:
            client.mark_unavailable()
            await _set_available(paths, False, f"MCP process is {state_name}")


@hook("extension_activate")
async def extension_activate(event: ExtensionActivateEvent, ctx: Context) -> None:
    """Mount configured stdio MCP tools when this extension activates."""
    del event
    omp.env.require(omp.env.Capability.PROCESS)
    configs = _load_configs(ctx.settings.get("servers", "{}"))
    for config in configs:
        server = _segment(config.name)
        process = await omp.env.proc.ensure(
            f"mcp_devices.{server}",
            shlex.join((config.command, *config.args)),
            env=dict(config.env),
            restart="on-failure",
        )
        client = _JsonRpcClient(process)
        tools = await client.initialize()
        mounts: list[omp.MountSpec] = []
        paths: list[str] = []
        seen: set[str] = set()
        for tool in tools:
            leaf = _segment(tool.name)
            if leaf in seen:
                raise ValueError(
                    f"MCP server {config.name!r} has colliding device names after normalization"
                )
            seen.add(leaf)
            subpath = f"{server}/{leaf}"
            mounts.append(
                omp.MountSpec(
                    subpath=subpath,
                    body=_device_body(client, tool),
                    schema=tool.input_schema,
                    summary=tool.description or f"Call the MCP tool {tool.name}.",
                    docs=tool.description or None,
                )
            )
            paths.append(_MCP_DEVICES.path(subpath))
        await _MCP_DEVICES.mount_many(*mounts)
        _CLIENTS[server] = client
        watcher = asyncio.create_task(
            _watch_server(client, tuple(paths)), name=f"mcp-devices:{server}"
        )
        _WATCHERS.add(watcher)
        watcher.add_done_callback(_WATCHERS.discard)
