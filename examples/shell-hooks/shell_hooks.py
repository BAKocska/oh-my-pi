from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, fields, is_dataclass
from datetime import date, datetime
from enum import Enum
from itertools import islice
from typing import Any

import omp


_MAX_PAYLOAD_BYTES = 64 * 1024
_MAX_STRING_CHARS = 4 * 1024
_MAX_COLLECTION_ITEMS = 64
_SUPPORTED_EVENTS = (
    "SessionStart",
    "SessionEnd",
    "PreCompact",
    "PostCompact",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "UserPromptSubmit",
    "Stop",
)
_GATEABLE_EVENTS = frozenset({"SessionStart", "PreToolUse", "UserPromptSubmit"})


class ShellHooksConfigError(omp.OmpError):
    """Report one invalid shell-hook setting with machine-readable coordinates."""

    def __init__(self, event: str, field: str, detail: str) -> None:
        self.code = "invalid_shell_hook_config"
        self.event = event
        self.field = field
        self.detail = detail
        super().__init__(f"{self.code}: {event}.{field}: {detail}")


@dataclass(frozen=True, slots=True)
class CommandHook:
    """Hold one validated Claude Code event command."""

    command: str
    timeout: omp.Duration
    on_failure: str


def load_config(settings: Mapping[str, object]) -> Mapping[str, CommandHook]:
    """Validate and normalize the event-to-command settings map."""

    configured: dict[str, CommandHook] = {}
    for event, raw in settings.items():
        if event not in _SUPPORTED_EVENTS:
            raise ShellHooksConfigError(str(event), "event", "unknown Claude Code hook name")
        if not isinstance(raw, Mapping):
            raise ShellHooksConfigError(event, "entry", "expected a table")

        command = raw.get("command")
        if not isinstance(command, str) or not command.strip():
            raise ShellHooksConfigError(event, "command", "expected a non-empty string")

        timeout_value = raw.get("timeout", "5s")
        if not isinstance(timeout_value, str):
            raise ShellHooksConfigError(event, "timeout", "expected a duration string")
        try:
            timeout = omp.Duration(timeout_value)
        except (TypeError, ValueError) as error:
            raise ShellHooksConfigError(event, "timeout", str(error)) from error
        if timeout.seconds <= 0:
            raise ShellHooksConfigError(event, "timeout", "must be greater than zero")

        on_failure = raw.get("on_failure", "continue")
        if on_failure not in {"continue", "deny"}:
            raise ShellHooksConfigError(
                event, "on_failure", "expected 'continue' or 'deny'"
            )
        if on_failure == "deny" and event not in _GATEABLE_EVENTS:
            raise ShellHooksConfigError(
                event,
                "on_failure",
                "this mapped omp event is observation-only; use 'continue'",
            )
        configured[event] = CommandHook(command.strip(), timeout, on_failure)
    return configured


def _json_value(value: object, depth: int = 0) -> object:
    if depth >= 6:
        return str(value)[:_MAX_STRING_CHARS]
    if value is None or isinstance(value, bool | int | float):
        return value
    if isinstance(value, str):
        return value[:_MAX_STRING_CHARS]
    if isinstance(value, bytes):
        return value[:_MAX_STRING_CHARS].decode("utf-8", errors="replace")
    if isinstance(value, Enum):
        return _json_value(value.value, depth + 1)
    if isinstance(value, datetime | date):
        return value.isoformat()
    if is_dataclass(value) and not isinstance(value, type):
        return {
            field.name: _json_value(getattr(value, field.name), depth + 1)
            for field in fields(value)
        }
    if isinstance(value, Mapping):
        return {
            str(key)[:_MAX_STRING_CHARS]: _json_value(item, depth + 1)
            for key, item in islice(value.items(), _MAX_COLLECTION_ITEMS)
        }
    if isinstance(value, Sequence) and not isinstance(value, str | bytes | bytearray):
        return [
            _json_value(item, depth + 1)
            for item in value[:_MAX_COLLECTION_ITEMS]
        ]
    return str(value)[:_MAX_STRING_CHARS]


def _target_name(target: object) -> str:
    server = getattr(target, "server", None)
    tool = getattr(target, "tool", None)
    if server is not None and tool is not None:
        return f"mcp__{server}__{tool}"
    return str(getattr(target, "name", type(target).__name__))


def _payload(event_name: str, event: object, ctx: omp.Context) -> bytes:
    session_id = str(getattr(event, "session_id", ctx.session))
    event_data = _json_value(event)
    payload: dict[str, object] = {
        "session_id": session_id,
        "hook_event_name": event_name,
        "omp_event_name": ctx.event,
        "omp_event": event_data,
    }
    cwd = getattr(event, "cwd", None)
    if cwd is not None:
        payload["cwd"] = str(cwd)

    if event_name == "SessionStart":
        payload["source"] = "resume" if getattr(event, "resumed", False) else "startup"
    elif event_name == "SessionEnd":
        payload["reason"] = "other"
        payload["omp_reason"] = _json_value(getattr(event, "reason", None))
    elif event_name == "PreCompact":
        payload["trigger"] = str(getattr(event, "reason", "auto"))
        payload["custom_instructions"] = getattr(event, "custom_instructions", None)
    elif event_name in {"PreToolUse", "PostToolUse", "PostToolUseFailure"}:
        target = getattr(event, "target", None)
        payload["tool_name"] = _target_name(target)
        payload["tool_use_id"] = str(getattr(event, "call_id", ""))
        tool_input = getattr(event, "args", None)
        if tool_input is None:
            tool_input = getattr(target, "args", {})
        payload["tool_input"] = _json_value(tool_input)
        if event_name == "PostToolUse":
            payload["tool_response"] = _json_value(getattr(event, "payload", None))
        elif event_name == "PostToolUseFailure":
            failure = getattr(event, "fault", None) or getattr(event, "abort", None)
            payload["error"] = _json_value(failure)
    elif event_name == "UserPromptSubmit":
        payload["prompt"] = str(getattr(event, "text", ""))[:_MAX_STRING_CHARS]
    elif event_name == "Stop":
        payload["stop_hook_active"] = False
        payload["error"] = getattr(event, "error", None)

    encoded = json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8")
    if len(encoded) > _MAX_PAYLOAD_BYTES:
        payload = {
            "session_id": session_id,
            "hook_event_name": event_name,
            "omp_event_name": ctx.event,
            "payload_truncated": True,
            "original_bytes": len(encoded),
        }
        encoded = json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8")
    return encoded + b"\n"


def _clamped_timeout(configured: omp.Duration, ctx: omp.Context) -> omp.Duration:
    remaining = ctx.deadline_in()
    if remaining is None or configured.seconds <= remaining.seconds:
        return configured
    return omp.Duration(seconds=remaining.seconds)


async def _execute(
    config: CommandHook, event_name: str, event: object, ctx: omp.Context
) -> omp.env.Completed:
    async with omp.env.sh.session() as session:
        run = await session.run(config.command, timeout=_clamped_timeout(config.timeout, ctx))
        await run.write(_payload(event_name, event, ctx))
        await run.eof()
        return await run.wait()


def _failed(result: omp.env.Completed) -> bool:
    return result.outcome is not omp.env.Outcome.EXITED or result.exit_code != 0


def _failure_reason(event_name: str, result: omp.env.Completed) -> str:
    detail = result.text().strip()[:512]
    status = result.outcome.value
    if result.exit_code is not None:
        status = f"{status} ({result.exit_code})"
    suffix = f": {detail}" if detail else ""
    return f"{event_name} shell hook failed: {status}{suffix}"


async def _observe(event_name: str, event: object, ctx: omp.Context) -> None:
    config = load_config(ctx.settings).get(event_name)
    if config is None or config.on_failure == "deny":
        return
    await _execute(config, event_name, event, ctx)


async def _gate(event_name: str, event: object, ctx: omp.Context) -> omp.HookDecision:
    config = load_config(ctx.settings).get(event_name)
    if config is None or config.on_failure != "deny":
        return omp.Defer()
    result = await _execute(config, event_name, event, ctx)
    if _failed(result):
        return omp.Deny(_failure_reason(event_name, result), code="shell_hook_failed")
    return omp.Defer()


@omp.hook("extension_activate", phase=omp.HookPhase.OBSERVE)
async def _activate(event: omp.ExtensionActivateEvent, ctx: omp.Context) -> None:
    del event
    load_config(ctx.settings)


@omp.hook("session_start", phase=omp.HookPhase.OBSERVE)
async def _observe_session_start(event: omp.SessionStartEvent, ctx: omp.Context) -> None:
    await _observe("SessionStart", event, ctx)


@omp.hook(
    "session_start", phase=omp.HookPhase.PRECHECK, on_failure=omp.OnFailure.DENY
)
async def _gate_session_start(
    event: omp.SessionStartEvent, ctx: omp.Context
) -> omp.HookDecision:
    return await _gate("SessionStart", event, ctx)


@omp.hook("session_shutdown", phase=omp.HookPhase.OBSERVE)
async def _observe_session_end(
    event: omp.SessionShutdownEvent, ctx: omp.Context
) -> None:
    await _observe("SessionEnd", event, ctx)


@omp.hook("compaction")
async def _observe_pre_compact(event: omp.CompactionEvent, ctx: omp.Context) -> None:
    await _observe("PreCompact", event, ctx)


@omp.hook("compaction_done", phase=omp.HookPhase.OBSERVE)
async def _observe_post_compact(event: omp.CompactionOutcome, ctx: omp.Context) -> None:
    await _observe("PostCompact", event, ctx)


@omp.hook("tool_call", phase=omp.HookPhase.OBSERVE)
async def _observe_pre_tool_use(event: omp.ToolCallEvent, ctx: omp.Context) -> None:
    await _observe("PreToolUse", event, ctx)


@omp.hook("tool_call", phase=omp.HookPhase.PRECHECK, on_failure=omp.OnFailure.DENY)
async def _gate_pre_tool_use(
    event: omp.ToolCallEvent, ctx: omp.Context
) -> omp.HookDecision:
    return await _gate("PreToolUse", event, ctx)


@omp.hook("tool_result", phase=omp.HookPhase.OBSERVE)
async def _observe_post_tool_use(event: omp.ToolResultEvent, ctx: omp.Context) -> None:
    if event.outcome is omp.OutcomeKind.OK:
        await _observe("PostToolUse", event, ctx)
    else:
        await _observe("PostToolUseFailure", event, ctx)


@omp.hook("user_input", phase=omp.HookPhase.OBSERVE)
async def _observe_user_prompt(event: omp.UserInputEvent, ctx: omp.Context) -> None:
    await _observe("UserPromptSubmit", event, ctx)


@omp.hook("user_input", phase=omp.HookPhase.PRECHECK, on_failure=omp.OnFailure.DENY)
async def _gate_user_prompt(
    event: omp.UserInputEvent, ctx: omp.Context
) -> omp.HookDecision:
    return await _gate("UserPromptSubmit", event, ctx)


@omp.hook("agent_end", phase=omp.HookPhase.OBSERVE)
async def _observe_stop(event: omp.AgentEndEvent, ctx: omp.Context) -> None:
    await _observe("Stop", event, ctx)
