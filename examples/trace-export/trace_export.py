from __future__ import annotations

from dataclasses import fields, is_dataclass
from enum import Enum
from typing import Any, Mapping

import omp
from omp.telemetry import OtlpTarget, ProcessTarget, export

TRACE_KINDS = (
    "turn_start",
    "turn_end",
    "model_request",
    "model_attempt",
    "provider_error",
    "tool_call",
)
"""Turn, model, and tool event kinds forwarded by this extension."""

EVENTS_SEEN = omp.telemetry.counter(
    "trace_export.events",
    unit="{event}",
    description="Trace events observed by examples.trace-export.",
)
"""Count of events mapped by the trace-export subscription."""


@omp.hook("extension_activate")
async def register_export(payload: object, ctx: omp.Context) -> None:
    """Register the settings-selected declarative exporter at activation."""
    del payload
    target_name = str(ctx.settings.get("target", "process"))
    if target_name == "process":
        target = ProcessTarget(
            process=str(ctx.settings.get("process", "bt-trace-daemon")),
            framing="jsonl",
            handshake={
                "type": "initialize",
                "protocol_version": 1,
                "client": {"source": "omp"},
            },
        )
    elif target_name == "otlp":
        target = OtlpTarget(
            endpoint=str(ctx.settings.get("otlp_endpoint", "https://api.braintrust.dev/otel")),
            headers={"authorization": "Bearer ${creds:braintrust}"},
        )
    else:
        raise ValueError("settings.target must be 'process' or 'otlp'")
    export(target, kinds=TRACE_KINDS)


def trace_frame(event: object) -> dict[str, object]:
    """Map a typed telemetry event to a stable, JSON-compatible trace frame."""
    values: Mapping[str, Any]
    if is_dataclass(event) and not isinstance(event, type):
        values = {item.name: getattr(event, item.name) for item in fields(event)}
    elif hasattr(event, "__dict__"):
        values = vars(event)
    else:
        raise TypeError("telemetry events must be dataclass or attribute records")

    def encode(value: object) -> object:
        if isinstance(value, Enum):
            return value.value
        if is_dataclass(value) and not isinstance(value, type):
            return {item.name: encode(getattr(value, item.name)) for item in fields(value)}
        if isinstance(value, Mapping):
            return {str(key): encode(item) for key, item in value.items()}
        if isinstance(value, (tuple, list)):
            return [encode(item) for item in value]
        if value is None or isinstance(value, (str, int, float, bool)):
            return value
        return str(value)

    kind = getattr(event, "kind", type(event).__name__)
    return {
        "kind": kind.value if isinstance(kind, Enum) else str(kind),
        "seq": int(getattr(event, "seq", 0)),
        "event": {key: encode(value) for key, value in values.items()},
    }


@omp.telemetry(
    TRACE_KINDS,
    scope=omp.telemetry.Scope.TREE,
    queue=4096,
    overflow=omp.telemetry.Overflow.DROP_OLDEST,
    replay=True,
    replay_limit=2048,
)
async def map_trace_event(event: object, ctx: omp.Context) -> None:
    """Map replayed and live events without performing process or socket I/O."""
    del ctx
    trace_frame(event)
    EVENTS_SEEN.add(1, kind=str(getattr(event, "kind", type(event).__name__)))
