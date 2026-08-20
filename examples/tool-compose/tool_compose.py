from __future__ import annotations

import ast
import json
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import Literal

import omp


_MAX_SCRIPT_BYTES = 8 * 1024
_Status = Literal["ok", "denied", "faulted"]
_ComposeStatus = Literal["completed", "partial"]
_Invoke = Callable[[str, Mapping[str, object], omp.Context], Awaitable[object]]


@dataclass(frozen=True, slots=True)
class ComposeArgs:
    """A bounded sequence of explicit device calls written as a small script."""

    script: str


@dataclass(frozen=True, slots=True)
class StepRecord:
    """The typed settlement record for one independently gated device invocation."""

    index: int
    device: str
    args: Mapping[str, object]
    status: _Status
    result: object | None = None
    reason: str | None = None
    code: str | None = None
    decision_id: str | None = None


@dataclass(frozen=True, slots=True)
class ComposeResult:
    """The completed or partial trail of a composed script."""

    status: _ComposeStatus
    steps: tuple[StepRecord, ...]
    result: object | None = None


@dataclass(frozen=True, slots=True)
class _Step:
    binding: str | None
    device: str
    args: ast.Dict


def _call_node(statement: ast.stmt) -> tuple[str | None, ast.Call]:
    binding: str | None
    expression: ast.expr
    if isinstance(statement, ast.Assign):
        if len(statement.targets) != 1 or not isinstance(statement.targets[0], ast.Name):
            raise ValueError("each assignment must bind one simple name")
        binding = statement.targets[0].id
        expression = statement.value
    elif isinstance(statement, ast.Expr):
        binding = None
        expression = statement.value
    else:
        raise ValueError("script statements must be calls or simple assignments")

    if isinstance(expression, ast.Await):
        expression = expression.value
    if not isinstance(expression, ast.Call):
        raise ValueError("each statement must call call(device, args)")
    if not isinstance(expression.func, ast.Name) or expression.func.id != "call":
        raise ValueError("call is the only script operation")
    return binding, expression


def _parse_args(call: ast.Call) -> ast.Dict:
    if len(call.args) > 2:
        raise ValueError("call accepts a device path and at most one argument object")
    keys: list[ast.expr | None] = []
    values: list[ast.expr] = []
    if len(call.args) == 2:
        supplied = call.args[1]
        if not isinstance(supplied, ast.Dict):
            raise ValueError("the positional call arguments must be an object literal")
        keys.extend(supplied.keys)
        values.extend(supplied.values)
    for keyword in call.keywords:
        if keyword.arg is None:
            raise ValueError("expanded keyword arguments are not supported")
        keys.append(ast.Constant(keyword.arg))
        values.append(keyword.value)
    names = [key.value for key in keys if isinstance(key, ast.Constant)]
    if len(names) != len(keys) or any(not isinstance(name, str) for name in names):
        raise ValueError("call argument keys must be literal strings")
    if len(set(names)) != len(names):
        raise ValueError("call argument keys must be unique")
    return ast.Dict(keys=keys, values=values)


def _parse_script(script: str, max_steps: int) -> tuple[_Step, ...]:
    if not isinstance(script, str) or not script.strip():
        raise ValueError("script must be a non-empty string")
    if len(script.encode()) > _MAX_SCRIPT_BYTES:
        raise ValueError(f"script exceeds {_MAX_SCRIPT_BYTES} bytes")
    try:
        module = ast.parse(script, mode="exec")
    except SyntaxError as error:
        raise ValueError(f"invalid compose script: {error.msg}") from error
    if not module.body:
        raise ValueError("script must contain at least one call")
    if len(module.body) > max_steps:
        raise ValueError(f"script has {len(module.body)} steps; maximum is {max_steps}")

    steps: list[_Step] = []
    bindings: set[str] = set()
    for statement in module.body:
        binding, call = _call_node(statement)
        if not call.args or not isinstance(call.args[0], ast.Constant):
            raise ValueError("the device path must be a literal string")
        device = call.args[0].value
        if not isinstance(device, str) or not device:
            raise ValueError("the device path must be a non-empty literal string")
        if binding is not None:
            if binding == "call" or binding in bindings:
                raise ValueError(f"invalid or duplicate binding {binding!r}")
            bindings.add(binding)
        steps.append(_Step(binding, device, _parse_args(call)))
    return tuple(steps)


def _decode_value(node: ast.expr, bindings: Mapping[str, object], depth: int = 0) -> object:
    if depth > 16:
        raise ValueError("script value nesting exceeds 16 levels")
    if isinstance(node, ast.Constant) and isinstance(node.value, (str, int, float, bool, type(None))):
        return node.value
    if isinstance(node, ast.Name):
        if node.id not in bindings:
            raise ValueError(f"binding {node.id!r} is not available")
        return bindings[node.id]
    if isinstance(node, (ast.List, ast.Tuple)):
        values = tuple(_decode_value(item, bindings, depth + 1) for item in node.elts)
        return list(values) if isinstance(node, ast.List) else values
    if isinstance(node, ast.Dict):
        result: dict[str, object] = {}
        for key_node, value_node in zip(node.keys, node.values, strict=True):
            if not isinstance(key_node, ast.Constant) or not isinstance(key_node.value, str):
                raise ValueError("object keys must be literal strings")
            result[key_node.value] = _decode_value(value_node, bindings, depth + 1)
        return result
    raise ValueError("values may contain only literals, containers, and prior bindings")


def _allowed_devices(settings: Mapping[str, object]) -> frozenset[str]:
    encoded = settings.get("allowed_devices", "[]")
    if isinstance(encoded, str):
        try:
            decoded = json.loads(encoded)
        except json.JSONDecodeError as error:
            raise ValueError("allowed_devices must be a JSON array of exact device paths") from error
    else:
        decoded = encoded
    if not isinstance(decoded, Sequence) or isinstance(decoded, (str, bytes)):
        raise ValueError("allowed_devices must be a JSON array of exact device paths")
    if any(not isinstance(path, str) or not path for path in decoded):
        raise ValueError("allowed_devices entries must be non-empty strings")
    if len(set(decoded)) != len(decoded):
        raise ValueError("allowed_devices entries must be unique")
    return frozenset(decoded)


def _max_steps(settings: Mapping[str, object]) -> int:
    value = settings.get("max_steps", 8)
    if not isinstance(value, int) or isinstance(value, bool) or not 1 <= value <= 32:
        raise ValueError("max_steps must be an integer from 1 through 32")
    return value


def _catalog_paths() -> frozenset[str]:
    return frozenset(str(row.path) for row in omp.devices.list(mounted_only=True))


async def _invoke_dyn(
    device: str, args: Mapping[str, object], ctx: omp.Context
) -> object:
    del device, args, ctx
    raise omp.NotWiredError("omp.dyn.invoke")


async def _execute(
    args: ComposeArgs,
    ctx: omp.Context,
    *,
    invoke: _Invoke = _invoke_dyn,
    catalog: frozenset[str] | None = None,
) -> ComposeResult:
    allowed = _allowed_devices(ctx.settings)
    steps = _parse_script(args.script, _max_steps(ctx.settings))
    visible = _catalog_paths() if catalog is None else catalog
    for step in steps:
        if step.device not in allowed:
            raise PermissionError(f"device {step.device!r} is not in allowed_devices")
        if step.device not in visible:
            raise LookupError(f"allowed device {step.device!r} is not in the live catalog")

    trail: list[StepRecord] = []
    bindings: dict[str, object] = {}
    for index, step in enumerate(steps, 1):
        decoded = _decode_value(step.args, bindings)
        if not isinstance(decoded, Mapping):
            raise TypeError("decoded step arguments must be an object")
        call_args = dict(decoded)
        try:
            result = await invoke(step.device, call_args, ctx)
        except omp.PolicyDenied as error:
            trail.append(
                StepRecord(
                    index=index,
                    device=step.device,
                    args=call_args,
                    status="denied",
                    reason=error.reason,
                    code=error.code,
                    decision_id=error.decision_id,
                )
            )
            return ComposeResult(status="partial", steps=tuple(trail))
        except omp.NotWiredError:
            raise
        except Exception as error:
            trail.append(
                StepRecord(
                    index=index,
                    device=step.device,
                    args=call_args,
                    status="faulted",
                    reason=str(error),
                )
            )
            return ComposeResult(status="partial", steps=tuple(trail))
        trail.append(
            StepRecord(
                index=index,
                device=step.device,
                args=call_args,
                status="ok",
                result=result,
            )
        )
        if step.binding is not None:
            bindings[step.binding] = result

    return ComposeResult(
        status="completed",
        steps=tuple(trail),
        result=trail[-1].result,
    )


@omp.device(
    "compose",
    family="workflow",
    rev=1,
    place="worker:compose",
    schema=ComposeArgs,
    summary="Run a bounded script whose device calls are independently admitted.",
)
async def compose(args: ComposeArgs, ctx: omp.Context) -> ComposeResult:
    """Run a bounded, allowlisted sequence through the ordinary device dispatcher."""

    return await _execute(args, ctx)
