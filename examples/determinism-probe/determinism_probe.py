from __future__ import annotations

import dataclasses
import importlib.util
import json
import sys
from enum import Enum
from pathlib import Path
from typing import Any, Callable

import omp


@dataclasses.dataclass(slots=True)
class ProbeArgs:
    seed: str = "alpha"


@dataclasses.dataclass(frozen=True, slots=True)
class ContractResult:
    contract: str
    stable_under_identical: bool
    changed_under_perturbation: bool
    lint_fires: bool
    available: bool = True
    detail: str = ""


@dataclasses.dataclass(frozen=True, slots=True)
class ProbeReport(omp.Payload):
    checks: list[ContractResult]
    findings: list[str]


@dataclasses.dataclass(frozen=True, slots=True)
class ProbeFault(omp.Fault):
    detail: str


class ProbeKind(Enum):
    PRIMARY = "primary"


@dataclasses.dataclass(frozen=True, slots=True)
class CodecSample:
    label: str
    kind: ProbeKind
    count: int
    enabled: bool
    ratio: float
    raw: bytes
    items: list[int]
    passthrough: Any
    opaque: object


@omp.entry_kind(
    "examples.determinism-probe.sample", rev="v.1", display=False, spill=False
)
@dataclasses.dataclass(frozen=True, slots=True)
class ProbeEntry:
    sequence: int
    label: str
    tags: list[str]


_VOLATILE_COUNTER = 0


@omp.prompt_slot("guidance", priority=-100, cls=omp.SlotClass.STABLE)
def stable_probe_prompt(ctx: omp.PromptContext) -> str:
    """Render only immutable prompt input, with an observable perturbation surface."""

    return f"determinism-probe session={ctx.session_id} cwd={ctx.cwd}"


@omp.prompt_slot("status", priority=-100, cls=omp.SlotClass.VOLATILE)
def deliberately_volatile_prompt(ctx: omp.PromptContext) -> str:
    """Violate purity deliberately so the two-render lint must reject this slot."""

    del ctx
    global _VOLATILE_COUNTER
    _VOLATILE_COUNTER += 1
    return f"deliberately-volatile-render={_VOLATILE_COUNTER}"


def _json_bytes(value: object) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")


def _part_bytes(parts: object) -> bytes:
    rows: list[dict[str, object]] = []
    for part in parts if isinstance(parts, list) else [parts]:
        if isinstance(part, omp.TextPart):
            rows.append({"kind": "text", "text": part.text})
        elif isinstance(part, omp.JsonPart):
            rows.append({"kind": "json", "json": part.json.decode("utf-8")})
        elif isinstance(part, omp.BlobPart):
            rows.append({"kind": "blob", "blob": repr(part.blob), "alt": part.alt})
        else:
            rows.append({"kind": type(part).__name__, "value": repr(part)})
    return _json_bytes(rows)


def _value_bytes(value: object) -> bytes:
    if isinstance(value, bytes):
        return value
    if isinstance(value, str):
        return value.encode("utf-8")
    if isinstance(value, omp.LiftedCall):
        return value.raw_args + b"\0" + value.verdict
    if isinstance(value, list):
        return _part_bytes(value)
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return _json_bytes(dataclasses.asdict(value))
    return repr(value).encode("utf-8")


def _check(
    contract: str,
    operation: Callable[[object], object],
    original: object,
    perturbed: object,
) -> ContractResult:
    first = _value_bytes(operation(original))
    second = _value_bytes(operation(original))
    changed = _value_bytes(operation(perturbed))
    if first != second:
        raise AssertionError(f"{contract}: identical input produced different bytes")
    if first == changed:
        raise AssertionError(f"{contract}: the chosen perturbation was not observable")
    return ContractResult(contract, True, True, False)


def _checked_prompt(
    renderer: Callable[[omp.PromptContext], str | None], ctx: omp.PromptContext
) -> str | None:
    """Apply the documented two-render volatile-prompt lint at the Python boundary."""

    first = renderer(ctx)
    second = renderer(ctx)
    if _value_bytes(first) != _value_bytes(second):
        raise omp.prompts.VolatilePrompt(
            f"prompt slot {ctx.slot!r} changed across identical renders"
        )
    return first


def _prompt_context(*, session_id: str = "session-a", cwd: str = "/workspace") -> omp.PromptContext:
    return omp.PromptContext(
        session_id=session_id,
        model="probe-model",
        provider="probe-provider",
        context_window=32_768,
        epoch=7,
        cwd=cwd,
        roots=("/workspace",),
        vcs_branch="main",
        vcs_commit="0123456789abcdef",
        is_subagent=False,
        agent_kind=None,
        slot="guidance",
        cls=omp.SlotClass.STABLE,
        budget_bytes=4_096,
    )


def _project_report(
    view: omp.Ok[ProbeReport] | omp.Faulted[ProbeFault], caps: omp.PromptCaps
) -> list[omp.Part]:
    if isinstance(view, omp.Faulted):
        text = f"determinism probe failed: {view.fault.detail}"
    else:
        passed = sum(
            check.available
            and check.stable_under_identical
            and check.changed_under_perturbation
            for check in view.payload.checks
        )
        text = f"determinism probe: {passed}/{len(view.payload.checks)} executable contracts passed"
    return [omp.Part.text(text)] if caps.fits(text) else []


def _lift_probe(from_rev: omp.Rev, call: omp.RecordedCall) -> omp.LiftedCall | None:
    if from_rev != omp.Rev("det", 0):
        return None
    try:
        old = json.loads(call.raw_args)
    except (TypeError, UnicodeDecodeError, json.JSONDecodeError):
        return None
    if not isinstance(old, dict) or not isinstance(old.get("label"), str):
        return None
    return omp.LiftedCall.of({"seed": old["label"]}, json.loads(call.verdict))


def _catalog_bytes(definitions: object) -> bytes:
    rows: list[dict[str, object]] = []
    for definition in definitions if isinstance(definitions, tuple) else ():
        docs = definition.docs
        rows.append(
            {
                "docs": None if docs is None else str(docs),
                "family": definition.family,
                "name": definition.name,
                "rev": definition.rev,
                "summary": definition.summary,
            }
        )
    rows.sort(key=lambda row: (str(row["name"]), str(row["family"]), int(row["rev"])))
    return _json_bytes(rows)


def _local_checks() -> tuple[list[ContractResult], list[str]]:
    checks: list[ContractResult] = []
    findings: list[str] = []

    stable_ctx = _prompt_context()
    checks.append(
        _check(
            "prompt slot bytes",
            lambda value: _checked_prompt(stable_probe_prompt, value),
            stable_ctx,
            dataclasses.replace(stable_ctx, cwd="/workspace/changed"),
        )
    )

    volatile_ctx = dataclasses.replace(
        stable_ctx, slot="status", cls=omp.SlotClass.VOLATILE
    )
    try:
        _checked_prompt(deliberately_volatile_prompt, volatile_ctx)
    except omp.prompts.VolatilePrompt:
        checks.append(
            ContractResult(
                "volatile prompt lint",
                False,
                False,
                True,
                detail="omp.prompts.VolatilePrompt raised on the mismatched pair",
            )
        )
    else:
        raise AssertionError("volatile prompt lint accepted a changing contribution")

    caps = omp.PromptCaps(
        maximum_parts=2,
        maximum_text_bytes=512,
        media=False,
        dialect=omp.Dialect.NATIVE,
        model_class=omp.ModelClass.STANDARD,
    )
    report = ProbeReport([], [])
    changed_report = ProbeReport(
        [ContractResult("perturbed", True, True, False)], []
    )
    checks.append(
        _check(
            "verdict projection bytes",
            lambda value: _project_report(omp.Ok(value), caps),
            report,
            changed_report,
        )
    )

    recorded = omp.RecordedCall(
        identity=omp.ToolIdentity("determinism_probe", omp.Rev("det", 0)),
        raw_args=b'{"label":"alpha"}',
        verdict=b'{"ok":true}',
    )
    perturbed_call = dataclasses.replace(recorded, raw_args=b'{"label":"beta"}')
    checks.append(
        _check(
            "Device.lift idempotence",
            lambda value: _lift_probe(omp.Rev("det", 0), value),
            recorded,
            perturbed_call,
        )
    )

    checks.append(
        _check(
            "LiftedCall.of canonical bytes",
            lambda value: omp.LiftedCall.of(value, {"accepted": True}),
            ProbeArgs("alpha"),
            ProbeArgs("beta"),
        )
    )

    if not callable(getattr(omp, "dumps", None)) or not callable(getattr(omp, "loads", None)):
        checks.append(
            ContractResult(
                "omp.dumps/omp.loads canonical codec",
                False,
                False,
                False,
                available=False,
                detail="one or both documented public codec symbols are absent",
            )
        )
        findings.append(
            "omp.dumps and omp.loads are absent, so canonical argument serialization and typed round-trip cannot be exercised"
        )
        checks.append(
            ContractResult(
                "entry-kind encode/decode round-trip",
                False,
                False,
                False,
                available=False,
                detail="the documented public codec required for round-trip is absent",
            )
        )
    else:
        codec = getattr(omp, "dumps")
        decoder = getattr(omp, "loads")
        sample = CodecSample(
            label="café",
            kind=ProbeKind.PRIMARY,
            count=7,
            enabled=True,
            ratio=1.5,
            raw=b"\x00\xff",
            items=[1, 2],
            passthrough={"z": 2, "a": 1},
            opaque=["x", None],
        )
        encoded_sample = codec(sample)
        expected_sample = (
            b'{"count":7,"enabled":true,"items":[1,2],"kind":"primary",'
            b'"label":"caf\xc3\xa9","opaque":["x",null],'
            b'"passthrough":{"a":1,"z":2},"ratio":1.5,'
            b'"raw":{"$bytes":"AP8="}}'
        )
        if encoded_sample != expected_sample:
            raise AssertionError("omp.dumps did not produce canonical compact UTF-8")
        if decoder(encoded_sample, CodecSample) != sample:
            raise AssertionError("canonical codec did not round-trip reachable shapes")
        try:
            codec(float("nan"))
        except TypeError:
            pass
        else:
            raise AssertionError("omp.dumps accepted a non-finite number")
        for malformed, shape in (
            (b'{"seed":7}', ProbeArgs),
            (encoded_sample + b" ", CodecSample),
        ):
            try:
                decoder(malformed, shape)
            except omp.VerdictShapeError:
                pass
            else:
                raise AssertionError("omp.loads accepted a shape mismatch or trailing data")
        checks.append(
            _check(
                "omp.dumps/omp.loads canonical codec",
                lambda value: codec(value),
                sample,
                dataclasses.replace(sample, label="changed"),
            )
        )
        entry = ProbeEntry(7, "alpha", ["a", "b"])
        encoded = codec(entry)
        if decoder(encoded, ProbeEntry) != entry:
            raise AssertionError("entry-kind codec did not round-trip")
        changed_entry = dataclasses.replace(entry, label="beta")
        checks.append(
            _check("entry-kind encode round-trip", codec, entry, changed_entry)
        )

    if not isinstance(getattr(omp, "VolatilePrompt", None), type):
        findings.append(
            "omp.VolatilePrompt is absent; only omp.prompts.VolatilePrompt is exported"
        )

    from omp._registry import registry

    definitions = registry.snapshot().device_definitions
    if not definitions:
        raise AssertionError("catalog probe has no declared device rows")
    checks.append(
        _check(
            "catalog/docs rendering",
            _catalog_bytes,
            definitions,
            definitions[:-1],
        )
    )
    return checks, findings


def _load_example(directory: str, module_name: str) -> object:
    path = Path(__file__).resolve().parents[1] / directory / f"{module_name}.py"
    import_name = f"_determinism_probe_{module_name}"
    spec = importlib.util.spec_from_file_location(import_name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot import sweep target {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[import_name] = module
    spec.loader.exec_module(module)
    return module


def _corpus_checks() -> list[ContractResult]:
    checks: list[ContractResult] = []

    canary = _load_example("canary", "canary")
    original_ctx = _prompt_context(session_id="sweep-a")
    checks.append(
        _check(
            "corpus canary.canary_prompt",
            canary.canary_prompt,
            original_ctx,
            dataclasses.replace(original_ctx, session_id="sweep-b"),
        )
    )

    bridge = _load_example("chat-bridge", "chat_bridge")
    checks.append(
        _check(
            "corpus chat_bridge._reply_prompt",
            bridge._reply_prompt,
            (bridge.Reply("r1", "alpha"),),
            (bridge.Reply("r1", "beta"),),
        )
    )

    dialect = _load_example("edit-dialect", "edit_dialect")
    dialect_call = omp.RecordedCall(
        identity=omp.ToolIdentity("edit", omp.Rev("hl", 1)),
        raw_args=b'{"input":"[a#A1B2]\\nCUT 1.=1"}',
        verdict=b'{"before":"1","after":"2"}',
    )
    checks.append(
        _check(
            "corpus edit_dialect._DialectProjection.lift",
            lambda value: dialect._DialectProjection.lift(omp.Rev("hl", 1), value),
            dialect_call,
            dataclasses.replace(
                dialect_call,
                raw_args=b'{"input":"[b#C3D4]\\nCUT 1.=1"}',
            ),
        )
    )
    dialect_caps = omp.PromptCaps(
        maximum_parts=2,
        maximum_text_bytes=1_024,
        media=False,
        dialect=omp.Dialect.HASHLINE,
        model_class=omp.ModelClass.STANDARD,
    )
    original_payload = dialect.EditApplied("1", "2", False, False, ["1-2"])
    changed_payload = dataclasses.replace(original_payload, after="3")
    checks.append(
        _check(
            "corpus edit_dialect._DialectProjection.prompt",
            lambda value: dialect._DialectProjection.prompt(omp.Ok(value), dialect_caps),
            original_payload,
            changed_payload,
        )
    )
    return checks


def run_probe(*, sweep_corpus: bool = False) -> ProbeReport:
    checks, findings = _local_checks()
    if sweep_corpus:
        checks.extend(_corpus_checks())
    return ProbeReport(checks, findings)


def smoke() -> ProbeReport:
    report = run_probe(sweep_corpus=True)
    for check in report.checks:
        if not check.available:
            continue
        if check.contract == "volatile prompt lint":
            assert check.lint_fires
            continue
        assert check.stable_under_identical
        assert check.changed_under_perturbation
    return report


@omp.device(
    "determinism_probe",
    family="det",
    rev=1,
    summary="Run repeat/perturb byte-stability checks over pure extension contracts.",
)
async def determinism_probe(args: ProbeArgs, ctx: omp.Context) -> ProbeReport:
    del args, ctx
    return run_probe(sweep_corpus=False)


determinism_probe.prompt = _project_report
determinism_probe.lift = _lift_probe
