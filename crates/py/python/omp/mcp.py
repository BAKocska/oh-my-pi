"""Declarative Model Context Protocol server mounting.

Importing this module performs no I/O.  Transport, authentication, and mount
values are immutable declarations; the Environment-owned MCP client performs
all process, network, credential, and protocol work after CONTROL dispatch.
"""

from __future__ import annotations

import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from enum import StrEnum
from types import MappingProxyType
from urllib.parse import urlsplit

from _omp import Duration

from ._errors import SpecError
from .devices import Device, Precedence
from .placement import Place, Restart
from .policy import Tier


_EMPTY_MAP: Mapping[str, str] = MappingProxyType({})
_DEVICE_SEGMENT = re.compile(r"[a-z][a-z0-9_]{0,63}\Z")
_HEADER_NAME = re.compile(r"[!#$%&'*+.^_`|~0-9A-Za-z-]+\Z")


class McpTransportKind(StrEnum):
    """Discriminate the three supported MCP wire transports."""

    STDIO = "stdio"
    HTTP = "http"
    SSE = "sse"


class McpAuthKind(StrEnum):
    """Discriminate declared MCP credential requirements."""

    OAUTH = "oauth"
    API_KEY = "api_key"
    NONE = "none"


class McpServerState(StrEnum):
    """Describe an Environment-owned MCP connection's lifecycle state."""

    DISCONNECTED = "disconnected"
    CONNECTING = "connecting"
    CONNECTED = "connected"
    RECONNECTING = "reconnecting"
    FAILED = "failed"


def _non_empty(value: object, field_name: str) -> str:
    if not isinstance(value, str) or not value:
        raise SpecError(f"{field_name} must be a non-empty string")
    if "\x00" in value or "\r" in value or "\n" in value:
        raise SpecError(f"{field_name} must not contain NUL, CR, or LF")
    return value


def _server_name(value: object, field_name: str = "McpMount.server") -> str:
    name = _non_empty(value, field_name)
    if _DEVICE_SEGMENT.fullmatch(name) is None:
        raise SpecError(
            f"{field_name} must be a lowercase device segment of at most 64 characters"
        )
    return name


def _string_tuple(value: object, field_name: str, *, unique: bool = False) -> tuple[str, ...]:
    if isinstance(value, (str, bytes)) or not isinstance(value, Sequence):
        raise SpecError(f"{field_name} must be a sequence of strings")
    result = tuple(_non_empty(item, f"{field_name} item") for item in value)
    if unique and len(set(result)) != len(result):
        raise SpecError(f"{field_name} must not contain duplicates")
    return result


def _string_map(
    value: object,
    field_name: str,
    *,
    header_names: bool = False,
    allow_empty_values: bool = False,
) -> Mapping[str, str]:
    if not isinstance(value, Mapping):
        raise SpecError(f"{field_name} must be a mapping of strings to strings")
    copied: dict[str, str] = {}
    folded: set[str] = set()
    for raw_key, raw_value in value.items():
        key = _non_empty(raw_key, f"{field_name} key")
        if not isinstance(raw_value, str):
            raise SpecError(f"{field_name}[{key!r}] must be a string")
        if "\x00" in raw_value or "\r" in raw_value or "\n" in raw_value:
            raise SpecError(
                f"{field_name}[{key!r}] must not contain NUL, CR, or LF"
            )
        if not raw_value and not allow_empty_values:
            raise SpecError(f"{field_name}[{key!r}] must be a non-empty string")
        item = raw_value
        if header_names:
            if _HEADER_NAME.fullmatch(key) is None:
                raise SpecError(f"{field_name} contains invalid HTTP header name {key!r}")
            normalized = key.casefold()
            if normalized in folded:
                raise SpecError(f"{field_name} contains duplicate header name {key!r}")
            folded.add(normalized)
        copied[key] = item
    return MappingProxyType(copied)


def _remote_url(value: object, field_name: str) -> str:
    url = _non_empty(value, field_name)
    try:
        parsed = urlsplit(url)
        port = parsed.port
    except ValueError as error:
        raise SpecError(f"{field_name} is not a valid HTTP URL: {error}") from error
    del port
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        raise SpecError(f"{field_name} must be an absolute http or https URL")
    if parsed.username is not None or parsed.password is not None:
        raise SpecError(f"{field_name} must not contain embedded credentials")
    if parsed.fragment:
        raise SpecError(f"{field_name} must not contain a fragment")
    return url


@dataclass(frozen=True, slots=True)
class Stdio:
    """Declare an Environment-owned MCP child using newline-delimited stdio."""

    command: str
    args: tuple[str, ...] = ()
    env: Mapping[str, str] | None = None
    cwd: str | None = None
    kind: McpTransportKind = field(default=McpTransportKind.STDIO, init=False)

    def __post_init__(self) -> None:
        """Validate and snapshot the child-process declaration."""

        object.__setattr__(self, "command", _non_empty(self.command, "Stdio.command"))
        object.__setattr__(self, "args", _string_tuple(self.args, "Stdio.args"))
        if self.env is not None:
            object.__setattr__(
                self,
                "env",
                _string_map(self.env, "Stdio.env", allow_empty_values=True),
            )
        if self.cwd is not None:
            object.__setattr__(self, "cwd", _non_empty(self.cwd, "Stdio.cwd"))


@dataclass(frozen=True, slots=True)
class Http:
    """Declare a streamable-HTTP MCP endpoint."""

    url: str
    headers: Mapping[str, str] | None = None
    kind: McpTransportKind = field(default=McpTransportKind.HTTP, init=False)

    def __post_init__(self) -> None:
        """Validate and snapshot the remote endpoint declaration."""

        object.__setattr__(self, "url", _remote_url(self.url, "Http.url"))
        if self.headers is not None:
            object.__setattr__(
                self,
                "headers",
                _string_map(
                    self.headers,
                    "Http.headers",
                    header_names=True,
                    allow_empty_values=True,
                ),
            )


@dataclass(frozen=True, slots=True)
class Sse:
    """Declare a legacy HTTP-plus-SSE MCP endpoint."""

    url: str
    headers: Mapping[str, str] | None = None
    kind: McpTransportKind = field(default=McpTransportKind.SSE, init=False)

    def __post_init__(self) -> None:
        """Validate and snapshot the remote endpoint declaration."""

        object.__setattr__(self, "url", _remote_url(self.url, "Sse.url"))
        if self.headers is not None:
            object.__setattr__(
                self,
                "headers",
                _string_map(
                    self.headers,
                    "Sse.headers",
                    header_names=True,
                    allow_empty_values=True,
                ),
            )


McpTransport = Stdio | Http | Sse
"""One validated MCP transport declaration."""


@dataclass(frozen=True, slots=True)
class McpAuth:
    """Declare an MCP server's credential requirement without credential values."""

    kind: McpAuthKind
    scopes: tuple[str, ...] = ()
    name: str | None = None

    def __post_init__(self) -> None:
        """Refuse malformed or internally inconsistent authentication shapes."""

        if not isinstance(self.kind, McpAuthKind):
            raise SpecError("McpAuth.kind must be a McpAuthKind")
        scopes = _string_tuple(self.scopes, "McpAuth.scopes", unique=True)
        object.__setattr__(self, "scopes", scopes)
        if self.name is not None:
            object.__setattr__(self, "name", _non_empty(self.name, "McpAuth.name"))
        if self.kind is McpAuthKind.OAUTH:
            if self.name is not None:
                raise SpecError("OAuth MCP auth cannot declare an API-key name")
        elif self.kind is McpAuthKind.API_KEY:
            if self.name is None:
                raise SpecError("API-key MCP auth requires a credential name")
            if scopes:
                raise SpecError("API-key MCP auth cannot declare OAuth scopes")
        elif self.name is not None or scopes:
            raise SpecError("unauthenticated MCP auth cannot declare a name or scopes")

    @classmethod
    def oauth(cls, *, scopes: Sequence[str] = ()) -> "McpAuth":
        """Require an Environment-managed OAuth credential with optional scopes."""

        return cls(
            McpAuthKind.OAUTH,
            _string_tuple(scopes, "McpAuth.scopes", unique=True),
        )

    @classmethod
    def api_key(cls, *, name: str) -> "McpAuth":
        """Require the named Environment-managed API-key credential."""

        return cls(McpAuthKind.API_KEY, name=name)

    @classmethod
    def none(cls) -> "McpAuth":
        """Declare that the MCP server requires no authentication."""

        return cls(McpAuthKind.NONE)


@dataclass(frozen=True, slots=True)
class McpMount:
    """Declare one MCP server and its endpoint-to-device projection."""

    server: str
    transport: McpTransport
    auth: McpAuth = field(default_factory=McpAuth.none)
    include: tuple[str, ...] = ("*",)
    exclude: tuple[str, ...] = ()
    rename: Mapping[str, str] = field(default_factory=lambda: _EMPTY_MAP)
    docs: Mapping[str, str] = field(default_factory=lambda: _EMPTY_MAP)
    precedence: Precedence = Precedence.DEFAULT
    tier: Tier = Tier.WRITE
    timeout: Duration = Duration("30s")
    restart: Restart = Restart.ON_FAILURE

    def __post_init__(self) -> None:
        """Validate and snapshot all locally decidable mount semantics."""

        object.__setattr__(self, "server", _server_name(self.server))
        if not isinstance(self.transport, (Stdio, Http, Sse)):
            raise SpecError("McpMount.transport must be Stdio, Http, or Sse")
        if not isinstance(self.auth, McpAuth):
            raise SpecError("McpMount.auth must be McpAuth")
        include = _string_tuple(self.include, "McpMount.include", unique=True)
        exclude = _string_tuple(self.exclude, "McpMount.exclude", unique=True)
        if not include:
            raise SpecError("McpMount.include must contain at least one endpoint glob")
        if any("\x00" in pattern or "/" in pattern for pattern in (*include, *exclude)):
            raise SpecError("McpMount endpoint globs must not contain NUL or '/'")
        object.__setattr__(self, "include", include)
        object.__setattr__(self, "exclude", exclude)

        rename = _string_map(self.rename, "McpMount.rename")
        targets: set[str] = set()
        for endpoint, target in rename.items():
            if "/" in endpoint:
                raise SpecError("McpMount.rename endpoint names must not contain '/'")
            _server_name(target, f"McpMount.rename[{endpoint!r}]")
            if target in targets:
                raise SpecError(f"McpMount.rename contains duplicate target {target!r}")
            targets.add(target)
        object.__setattr__(self, "rename", rename)
        object.__setattr__(
            self,
            "docs",
            _string_map(self.docs, "McpMount.docs", allow_empty_values=True),
        )

        if not isinstance(self.precedence, Precedence):
            raise SpecError("McpMount.precedence must be omp.Precedence")
        if not isinstance(self.tier, Tier):
            raise SpecError("McpMount.tier must be omp.Tier")
        if not isinstance(self.timeout, Duration):
            raise SpecError("McpMount.timeout must be omp.Duration")
        if self.timeout < Duration("0s"):
            raise SpecError("McpMount.timeout must not be negative")
        if not isinstance(self.restart, Restart):
            try:
                object.__setattr__(self, "restart", Restart(self.restart))
            except (TypeError, ValueError) as error:
                raise SpecError(
                    "McpMount.restart must be 'no', 'on-failure', or 'always'"
                ) from error

        if self.auth.kind is not McpAuthKind.NONE and isinstance(
            self.transport, (Http, Sse)
        ):
            headers = self.transport.headers or _EMPTY_MAP
            if any(name.casefold() == "authorization" for name in headers):
                raise SpecError(
                    "authenticated MCP mounts cannot also declare an Authorization header"
                )


@dataclass(frozen=True, slots=True)
class McpResource:
    """Describe one MCP resource or resource template discovered by the host."""

    uri: str
    name: str
    media_type: str | None = None
    template: bool = False

    def __post_init__(self) -> None:
        """Validate host-projected resource metadata."""

        object.__setattr__(self, "uri", _non_empty(self.uri, "McpResource.uri"))
        object.__setattr__(self, "name", _non_empty(self.name, "McpResource.name"))
        if self.media_type is not None:
            object.__setattr__(
                self,
                "media_type",
                _non_empty(self.media_type, "McpResource.media_type"),
            )
        if not isinstance(self.template, bool):
            raise SpecError("McpResource.template must be bool")


@dataclass(frozen=True, slots=True)
class McpServer:
    """Snapshot one Environment-owned MCP server connection."""

    name: str
    state: McpServerState
    protocol_version: str | None = None
    instructions: str | None = None
    endpoints: tuple[str, ...] = ()
    resources: tuple[McpResource, ...] = ()
    prompts: tuple[str, ...] = ()
    last_error: str | None = None

    def __post_init__(self) -> None:
        """Validate and freeze host-projected connection state."""

        object.__setattr__(self, "name", _server_name(self.name, "McpServer.name"))
        if not isinstance(self.state, McpServerState):
            try:
                object.__setattr__(self, "state", McpServerState(self.state))
            except (TypeError, ValueError) as error:
                raise SpecError("McpServer.state is not a known MCP server state") from error
        for field_name in ("protocol_version", "instructions", "last_error"):
            value = getattr(self, field_name)
            if value is not None:
                object.__setattr__(
                    self,
                    field_name,
                    _non_empty(value, f"McpServer.{field_name}"),
                )
        object.__setattr__(
            self,
            "endpoints",
            _string_tuple(self.endpoints, "McpServer.endpoints", unique=True),
        )
        if isinstance(self.resources, (str, bytes)) or not isinstance(
            self.resources, Sequence
        ):
            raise SpecError("McpServer.resources must be a sequence of McpResource values")
        resources = tuple(self.resources)
        if any(not isinstance(resource, McpResource) for resource in resources):
            raise SpecError("McpServer.resources must contain only McpResource values")
        object.__setattr__(self, "resources", resources)
        object.__setattr__(
            self,
            "prompts",
            _string_tuple(self.prompts, "McpServer.prompts", unique=True),
        )


def _transport_payload(transport: McpTransport) -> dict[str, object]:
    if isinstance(transport, Stdio):
        return {
            "type": transport.kind.value,
            "command": transport.command,
            "args": list(transport.args),
            "env": dict(transport.env or _EMPTY_MAP),
            "cwd": transport.cwd,
        }
    return {
        "type": transport.kind.value,
        "url": transport.url,
        "headers": dict(transport.headers or _EMPTY_MAP),
    }


def _mount_payload(spec: McpMount) -> dict[str, object]:
    return {
        "server": spec.server,
        "transport": _transport_payload(spec.transport),
        "auth": {
            "kind": spec.auth.kind.value,
            "scopes": list(spec.auth.scopes),
            "name": spec.auth.name,
        },
        "include": list(spec.include),
        "exclude": list(spec.exclude),
        "rename": dict(spec.rename),
        "docs": dict(spec.docs),
        "precedence": int(spec.precedence),
        "tier": spec.tier.value,
        "timeout": str(spec.timeout),
        "restart": spec.restart.value,
    }


def _mounted_device(server: str, row: Mapping[str, object]) -> Device:
    name = _non_empty(row.get("name"), "mcp.mount device name")
    family = _non_empty(row.get("family"), "mcp.mount device family")
    rev = row.get("rev")
    definition = row.get("definition")
    if not isinstance(rev, int) or rev < 0:
        raise SpecError("mcp.mount device revision must be a non-negative integer")
    if not isinstance(definition, Mapping):
        raise SpecError("mcp.mount device definition must be a mapping")
    original_name = _non_empty(definition.get("name"), "mcp.mount MCP tool name")
    schema = definition.get("inputSchema")
    if schema is not None and not isinstance(schema, dict):
        raise SpecError("mcp.mount tool inputSchema must be a mapping")
    documentation = row.get("documentation")
    if documentation is not None and not isinstance(documentation, str):
        raise SpecError("mcp.mount device documentation must be a string")

    async def body(**arguments: object) -> object:
        from . import _control_request

        return await _control_request(
            "omp.mcp.invoke",
            server=server,
            tool=original_name,
            arguments=arguments,
        )

    device = Device(
        name=name,
        family=family,
        rev=rev,
        place=Place.ENV,
        precedence=int(Precedence.DEFAULT),
        replaces=None,
        schema=schema,
        docs=documentation,
        summary=definition.get("description")
        if isinstance(definition.get("description"), str)
        else None,
        body=body,
    )
    device.mounted = True
    return device


async def mount(spec: McpMount) -> tuple[Device, ...]:
    """Mount a validated MCP server through the Environment-owned CONTROL arm."""

    if not isinstance(spec, McpMount):
        raise SpecError("mcp.mount requires an McpMount declaration")
    from . import _control_request

    result = await _control_request("omp.mcp.mount", spec=_mount_payload(spec))
    if not isinstance(result, Mapping) or not isinstance(result.get("devices"), Sequence):
        raise SpecError("omp.mcp.mount returned an invalid device catalog")
    return tuple(_mounted_device(spec.server, row) for row in result["devices"])


async def unmount(server: str) -> None:
    """Unmount every device from one MCP server and release its connection."""

    server = _server_name(server, "mcp.unmount server")
    from . import _control_request

    await _control_request("omp.mcp.unmount", server=server)


async def servers() -> tuple[McpServer, ...]:
    """Read MCP connection state through the Environment-owned CONTROL arm."""

    from . import _control_request

    result = await _control_request("omp.mcp.servers")
    if not isinstance(result, Mapping) or not isinstance(result.get("servers"), Sequence):
        raise SpecError("omp.mcp.servers returned an invalid inventory")
    states = {
        0: McpServerState.DISCONNECTED,
        1: McpServerState.DISCONNECTED,
        2: McpServerState.CONNECTING,
        3: McpServerState.CONNECTED,
        4: McpServerState.RECONNECTING,
        5: McpServerState.FAILED,
    }
    inventory: list[McpServer] = []
    for row in result["servers"]:
        if not isinstance(row, Mapping):
            raise SpecError("omp.mcp.servers returned an invalid server row")
        raw_state = row.get("state")
        if raw_state not in states:
            raise SpecError("omp.mcp.servers returned an unknown lifecycle state")
        inventory.append(
            McpServer(
                name=row.get("name"),
                state=states[raw_state],
                endpoints=tuple(row.get("endpoints", ())),
                last_error=row.get("last_error"),
            )
        )
    return tuple(inventory)


__all__ = (
    "Http",
    "McpAuth",
    "McpAuthKind",
    "McpMount",
    "McpResource",
    "McpServer",
    "McpServerState",
    "McpTransport",
    "McpTransportKind",
    "Sse",
    "Stdio",
    "mount",
    "servers",
    "unmount",
)
