"""Switch one supervised llama-server between named flag sets."""

from __future__ import annotations

import shlex
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Protocol

import omp
from omp import ui
from omp.provider import Availability as ProviderAvailability
from omp.provider import DiscoveryKind, DiscoverySpec, TrustDomain

_PROCESS_NAME = "examples.llama-switch.server"
_HOST = "127.0.0.1"
_PORT = 8080
_ENDPOINT = f"http://{_HOST}:{_PORT}/v1"
_RESERVED_FLAGS = frozenset({"--host", "--port"})
_READY = omp.env.ReadyAll(
    omp.env.ReadyLog(
        r"(?i)(server is listening|listening on|http server)",
        timeout=omp.Duration("60s"),
    ),
    omp.env.ReadyTcp(
        _PORT,
        host=_HOST,
        timeout=omp.Duration("60s"),
    ),
)


@dataclass(frozen=True, slots=True)
class ServerConfig:
    """A named, validated llama-server argument vector."""

    name: str
    flags: tuple[str, ...]


class GenerationFrame(Protocol):
    """A process state or output frame carrying its supervisor generation."""

    generation: int


def _provider_spec(available: bool) -> omp.ProviderSpec:
    """Build the local provider declaration for one availability state."""

    return omp.ProviderSpec(
        id="llama-local",
        name="Local llama-server",
        management=omp.ManagementSpec(
            operations=frozenset({omp.Operation.DISCOVER_MODELS}),
        ),
        routes=(
            omp.RouteSpec(
                id="local",
                base_url=_ENDPOINT,
                api=omp.Api.OPENAI_CHAT,
                trust=TrustDomain.loopback(),
                discovery=DiscoverySpec(
                    kind=DiscoveryKind.OPENAI_MODELS,
                    path="/models",
                    label="Local llama-server",
                    authoritative=False,
                ),
            ),
        ),
        models=(
            omp.ModelSpec(
                id="llama-local",
                display_name="Active local llama model",
                routes=("local",),
                cost=omp.Cost.free(),
                availability=(
                    ProviderAvailability.AVAILABLE
                    if available
                    else ProviderAvailability.DISABLED
                ),
            ),
        ),
    )


_LLAMA_PROVIDER = omp.provider(_provider_spec(False))


@_LLAMA_PROVIDER
class LlamaLocalProvider:
    """Declare the supervised server as an OpenAI-compatible local provider."""


def _configs(ctx: omp.Context) -> tuple[dict[str, ServerConfig], str]:
    """Decode named llama-server flag sets from extension settings."""

    raw_configs = ctx.settings.get("configs")
    if not isinstance(raw_configs, Mapping) or not raw_configs:
        raise ValueError("settings.configs must be a non-empty table of flag arrays")

    configs: dict[str, ServerConfig] = {}
    for raw_name, raw_flags in raw_configs.items():
        if not isinstance(raw_name, str) or not raw_name.strip():
            raise ValueError("config names must be non-empty strings")
        name = raw_name.strip()
        if name in configs:
            raise ValueError(f"duplicate config name: {name}")
        if not isinstance(raw_flags, Sequence) or isinstance(
            raw_flags, (str, bytes, bytearray)
        ):
            raise ValueError(f"config {name!r} must be an array of flags")
        if not raw_flags or any(not isinstance(flag, str) or not flag for flag in raw_flags):
            raise ValueError(f"config {name!r} contains an empty or non-string flag")
        flags = tuple(raw_flags)
        if not any(flag in {"--model", "-m"} for flag in flags):
            raise ValueError(f"config {name!r} must select a model with --model or -m")
        collision = next((flag for flag in flags if flag in _RESERVED_FLAGS), None)
        if collision is not None:
            raise ValueError(
                f"config {name!r} may not override reserved endpoint flag {collision}"
            )
        configs[name] = ServerConfig(name, flags)

    default = ctx.settings.get("default")
    if not isinstance(default, str) or default not in configs:
        raise ValueError("settings.default must name one configured flag set")
    return configs, default


def _script(config: ServerConfig) -> str:
    """Render one shell-engine-safe llama-server launch script."""

    argv = ("llama-server", *config.flags, "--host", _HOST, "--port", str(_PORT))
    return " ".join(shlex.quote(argument) for argument in argv)


def _require_current(frame: GenerationFrame, generation: int) -> None:
    """Reject a state or output frame emitted by an obsolete process generation."""

    if frame.generation != generation:
        raise omp.StaleGeneration(
            f"llama-server frame generation {frame.generation} is stale; current is {generation}"
        )


async def _set_available(available: bool) -> None:
    """Atomically publish llama-server provider availability."""

    await _LLAMA_PROVIDER.replace(_provider_spec(available))


async def _publish_state(info: omp.env.ProcessInfo, generation: int) -> None:
    """Fence a process transition before applying it to provider availability."""

    _require_current(info, generation)
    await _set_available(info.state in {omp.env.ProcState.READY, omp.env.ProcState.RUNNING})


async def _ensure(config: ServerConfig) -> omp.env.Process:
    """Adopt or start the named server and publish its observed ready state."""

    process = await omp.env.proc.ensure(
        _PROCESS_NAME,
        _script(config),
        restart=omp.env.RestartPolicy(policy=omp.Restart.NO),
        ready=_READY,
    )
    await _publish_state(await process.info(), process.generation)
    return process


async def _switch(config: ServerConfig) -> omp.env.Process:
    """Stop the current generation and ensure the selected argv as the next one."""

    await _set_available(False)
    current = await omp.env.proc.adopt(_PROCESS_NAME)
    if current is not None:
        stopped = await current.stop(grace=omp.Duration("5s"))
        await _publish_state(stopped, current.generation)
    return await _ensure(config)


async def _complete_configs(
    query: ui.ArgQuery, ctx: omp.Context
) -> tuple[ui.CompletionItem, ...]:
    """Complete configured llama-server flag-set names."""

    if query.argv:
        return ()
    try:
        configs, _ = _configs(ctx)
    except ValueError:
        return ()
    prefix = query.prefix.casefold()
    return tuple(
        ui.CompletionItem(
            insert=name,
            label=name,
            desc="llama-server flag set",
            group="llama-server configs",
        )
        for name in sorted(configs, key=str.casefold)
        if name.casefold().startswith(prefix)
    )


@omp.command(
    "llama",
    description="Restart llama-server with a named flag set",
    args=(ui.Arg("config", "Configured llama-server flag set", usage="<config>"),),
    hint="<config>",
    arg_completions=_complete_configs,
)
async def llama(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Restart llama-server with exactly one configured argument vector."""

    if len(inv.argv) != 1:
        return ui.Consumed(ui.text("Usage: /llama <config>"))
    try:
        configs, _ = _configs(ctx)
    except ValueError as error:
        return ui.Consumed(ui.text(f"Invalid llama-switch settings: {error}"))
    config = configs.get(inv.argv[0])
    if config is None:
        return ui.Consumed(ui.text(f"Unknown llama-server config: {inv.argv[0]}"))

    process = await _switch(config)
    return ui.Consumed(
        ui.text(
            f"llama-server is ready with config {config.name} "
            f"(generation {process.generation})"
        )
    )


@omp.hook("extension_activate")
async def activate(payload: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    """Adopt or start the default supervised llama-server generation."""

    del payload
    configs, default = _configs(ctx)
    await _ensure(configs[default])
