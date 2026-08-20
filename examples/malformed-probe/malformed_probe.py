"""Probe malformed values at extension-owned decode boundaries."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Annotated, Awaitable, Callable

import omp


@dataclass(frozen=True, slots=True)
class MalformedProbeArgs:
    """Select whether to include source-audit-only boundaries in the report."""

    include_unreachable: bool = True


@dataclass(frozen=True, slots=True)
class Observation:
    """Record one boundary's stable, human-auditable disposition."""

    boundary: str
    malformed_input: str
    disposition: str
    exception: str | None
    message: str
    names_rule: bool


@dataclass(frozen=True, slots=True)
class MalformedProbeReport(omp.Payload):
    """Return every locally observable refusal and frozen-surface absence."""

    observations: tuple[Observation, ...]
    findings: tuple[str, ...]


@omp.params
class _PathParams:
    path: Annotated[
        str,
        omp.Field(
            alias=("file_path", "filename"),
            example="src/main.py",
        ),
    ]


@omp.params
class _LimitParams:
    limit: Annotated[
        int,
        omp.Field(coerce=(omp.Coerce.INTEGER,), example="42"),
    ]


class _FinalizationBackend:
    """Replay one host finalization result through the public cursor boundary."""

    def __init__(self, response: object) -> None:
        self.response = response

    async def request(
        self, operation: str, arguments: dict[str, object]
    ) -> object:
        if operation != "omp.params.args" or "invocation_id" not in arguments:
            raise AssertionError(f"unexpected finalizer request {operation!r}")
        return self.response

    def effect(self, effect: dict[str, object]) -> None:
        raise AssertionError(f"unexpected finalizer effect {effect!r}")


def _exception_name(error: BaseException) -> str:
    return f"{type(error).__module__}.{type(error).__qualname__}"


async def _call_async(
    boundary: str,
    malformed_input: str,
    operation: Callable[[], Awaitable[object]],
    *,
    rule_tokens: tuple[str, ...] = (),
) -> Observation:
    try:
        value = await operation()
    except Exception as error:  # the exception type is the result under test
        message = str(error)
        return Observation(
            boundary,
            malformed_input,
            "refused",
            _exception_name(error),
            message,
            all(token.lower() in message.lower() for token in rule_tokens),
        )
    return Observation(
        boundary,
        malformed_input,
        "accepted",
        None,
        repr(value),
        not rule_tokens,
    )


def _call(
    boundary: str,
    malformed_input: str,
    operation: Callable[[], object],
    *,
    rule_tokens: tuple[str, ...] = (),
) -> Observation:
    try:
        value = operation()
    except Exception as error:  # the exception type is the result under test
        message = str(error)
        return Observation(
            boundary,
            malformed_input,
            "refused",
            _exception_name(error),
            message,
            all(token.lower() in message.lower() for token in rule_tokens),
        )
    return Observation(
        boundary,
        malformed_input,
        "accepted",
        None,
        repr(value),
        not rule_tokens,
    )


def _surface_absence(
    boundary: str, malformed_input: str, symbols: tuple[str, ...]
) -> Observation:
    missing = tuple(symbol for symbol in symbols if not hasattr(omp, symbol))
    if not missing:
        return Observation(
            boundary,
            malformed_input,
            "host-only",
            None,
            "decoder types are exported but require a host-owned invocation feed",
            True,
        )
    return Observation(
        boundary,
        malformed_input,
        "unreachable",
        None,
        "frozen surface does not export " + ", ".join(missing),
        False,
    )


def _reserved_parameter_observation(name: str) -> Observation:
    """Exercise the same metadata extractor used by device registration without mutation."""

    from omp._registry import _extract_arg_specs

    if name == "do_":
        @dataclass(frozen=True, slots=True)
        class Reserved:
            do_: Annotated[str, omp.Field()]
    else:
        @dataclass(frozen=True, slots=True)
        class Reserved:
            future_: Annotated[str, omp.Field()]

    try:
        specs = _extract_arg_specs(lambda args, ctx: None, Reserved)
    except Exception as error:
        message = str(error)
        return Observation(
            f"device declaration reserved {name}",
            name,
            "refused",
            _exception_name(error),
            message,
            name in message or "reserved" in message.lower(),
        )
    return Observation(
        f"device declaration reserved {name}",
        name,
        "accepted",
        None,
        f"derived argument paths: {tuple(spec.path for spec in specs)!r}",
        False,
    )


def _duration_observation() -> Observation:
    if omp.Duration.__module__ != "_omp":
        return Observation(
            "Duration",
            "not-a-duration",
            "host-only",
            None,
            "the example harness supplies an inert native Duration stub",
            True,
        )
    return _call(
        "Duration",
        "not-a-duration",
        lambda: omp.Duration("not-a-duration"),
        rule_tokens=("duration",),
    )


def _url_call(
    boundary: str,
    malformed_input: str,
    operation: Callable[[], object],
) -> Observation:
    if not hasattr(omp.urls.Scheme, "UNKNOWN"):
        return Observation(
            boundary,
            malformed_input,
            "host-only",
            None,
            "the example harness supplies a FILE-only native URL vocabulary",
            True,
        )
    return _call(boundary, malformed_input, operation)


async def _finalization_observation(
    boundary: str,
    malformed_input: str,
    response: object,
    shape: type[object],
    *,
    rule_tokens: tuple[str, ...] = (),
) -> Observation:
    previous_backend = omp._control_backend.get()
    open_phase = omp.InvocationPhase.OPEN
    if not isinstance(open_phase, omp.InvocationPhase):
        open_phase = omp.InvocationPhase()
    cursor = omp.IncomingParams(
        name="malformed_probe",
        rev=omp.Rev("conformance", 1),
        invocation_id=boundary,
        phase=open_phase,
        shape=shape,
    )
    omp._install_control_backend(_FinalizationBackend(response))
    try:
        async def finalize() -> object:
            value = await cursor.args()
            return value, tuple(cursor.repairs())

        return await _call_async(
            boundary,
            malformed_input,
            finalize,
            rule_tokens=rule_tokens,
        )
    finally:
        omp._install_control_backend(previous_backend)


async def _run_probe(
    *, include_unreachable: bool = True
) -> MalformedProbeReport:
    """Run every pure/local boundary and replay typed host finalization results."""

    observations: list[Observation] = []

    # TML: structural errors are local; degradation and control stripping finish in the renderer.
    observations.extend(
        (
            _call(
                "TML structure",
                "é<row></bad>",
                lambda: omp.ui.Tml.raw("é<row></bad>"),
                rule_tokens=("closing tag",),
            ),
            _call(
                "TML unknown tag",
                "<future-panel><text>x</text></future-panel>",
                lambda: omp.ui.Tml.raw(
                    "<future-panel><text>x</text></future-panel>"
                ).source,
            ),
            _call(
                "TML raw controls",
                r"<text>bad\x00\x1b[2A</text>",
                lambda: omp.ui.Tml.raw("<text>bad\x00\x1b[2A</text>").source,
            ),
            _call(
                "TML text controls",
                r"bad\x00\x1b[2A",
                lambda: omp.ui.text("bad\x00\x1b[2A").source,
            ),
            _call(
                "TML depth",
                "65 nested <x> elements",
                lambda: omp.ui.Tml.raw("<x>" * 65 + "</x>" * 65).source,
                rule_tokens=("depth",),
            ),
            _call(
                "TML bytes",
                "262145 UTF-8 bytes",
                lambda: omp.ui.Tml.raw("x" * 262_145).source,
                rule_tokens=("bytes",),
            ),
            _call(
                "TML source type",
                r"b'<text>\xff</text>'",
                lambda: omp.ui.Tml.raw(b"<text>\xff</text>"),
                rule_tokens=("str",),
            ),
        )
    )

    # Selectors and URLs are pure and require no host connection.
    observations.extend(
        (
            _call(
                "selector inverted range",
                "5-3",
                lambda: omp.urls.parse_selector("5-3"),
                rule_tokens=("end", "start"),
            ),
            _call(
                "selector zero line",
                "0",
                lambda: omp.urls.parse_selector("0"),
                rule_tokens=("1-indexed",),
            ),
            _call(
                "selector negative line",
                "-1",
                lambda: omp.urls.parse_selector("-1"),
                rule_tokens=("selector",),
            ),
            _url_call(
                "URL unknown scheme",
                "future+transport://resource",
                lambda: omp.urls.parse("future+transport://resource").scheme,
            ),
            _url_call(
                "URL encoded selector delimiter",
                "file://notes%3A5",
                lambda: omp.urls.parse("file://notes%3A5"),
            ),
            _url_call(
                "URL malformed percent triplet",
                "file://notes%ZZ",
                lambda: omp.urls.parse("file://notes%ZZ"),
            ),
        )
    )

    # Public shortcut construction validates before registering a handler.
    observations.extend(
        (
            _call(
                "shortcut empty key",
                "ctrl+",
                lambda: omp.shortcut("ctrl+", action_id="malformed.empty"),
                rule_tokens=("shortcut chord",),
            ),
            _call(
                "shortcut duplicate modifier",
                "ctrl+ctrl+x",
                lambda: omp.shortcut(
                    "ctrl+ctrl+x", action_id="malformed.duplicate"
                ),
                rule_tokens=("shortcut chord",),
            ),
            _call(
                "shortcut unknown modifier",
                "hyper+x",
                lambda: omp.shortcut("hyper+x", action_id="malformed.unknown"),
                rule_tokens=("shortcut chord",),
            ),
        )
    )

    observations.extend(
        (
            _duration_observation(),
            _reserved_parameter_observation("do_"),
            _reserved_parameter_observation("future_"),
        )
    )

    if include_unreachable:
        finalization_cases = (
            (
                "duplicate canonical key",
                '{"path":"a","path":"b"}',
                {
                    "issue": {
                        "path": ("path",),
                        "expected": "exactly one canonical value",
                        "kind": "ambiguous",
                        "example": '{"path":"a"}',
                    }
                },
                _PathParams,
                ("ambiguous",),
            ),
            (
                "canonical plus alias",
                '{"path":"a","file_path":"b"}',
                {
                    "issue": {
                        "path": ("path",),
                        "expected": "exactly one canonical or alias value",
                        "kind": "ambiguous",
                    }
                },
                _PathParams,
                ("ambiguous",),
            ),
            (
                "two aliases",
                '{"file_path":"a","filename":"b"}',
                {
                    "issue": {
                        "path": ("path",),
                        "expected": "exactly one canonical or alias value",
                        "kind": "ambiguous",
                    }
                },
                _PathParams,
                ("ambiguous",),
            ),
            (
                "undeclared top-level key",
                '{"path":"a","extra":1}',
                {
                    "value": {
                        "path": "a",
                        "extra": 1,
                    }
                },
                dict,
                (),
            ),
            (
                "coercible wrong type",
                '{"limit":"42"}',
                {
                    "value": {"limit": 42},
                    "repairs": (
                        {
                            "path": ("limit",),
                            "kind": "coercion",
                            "detail": "converted string to integer",
                        },
                    ),
                },
                _LimitParams,
                (),
            ),
            (
                "non-coercible wrong type",
                '{"limit":[]}',
                {
                    "issue": {
                        "path": ("limit",),
                        "expected": "integer",
                        "kind": "type_mismatch",
                        "example": "42",
                        "found": "array",
                    }
                },
                _LimitParams,
                ("type_mismatch",),
            ),
        )
        for label, value, response, shape, tokens in finalization_cases:
            observations.append(
                await _finalization_observation(
                    "device args: " + label,
                    value,
                    response,
                    shape,
                    rule_tokens=tokens,
                )
            )

        observations.append(
            _surface_absence(
                "journal undeclared kind",
                "UndeclaredEntry()",
                ("UnknownEntryKind",),
            )
        )
        observations.append(
            _call(
                "journal canonical JSON",
                '{"count":"wrong"}',
                lambda: omp.journal.decode(b'{"count":"wrong"}'),
            )
        )

    findings: list[str] = []
    by_boundary = {row.boundary: row for row in observations}
    if by_boundary["TML depth"].disposition != "refused":
        findings.append("TML depth 65 is not refused above TML_MAX_DEPTH=64")
    if by_boundary["TML bytes"].disposition != "refused":
        findings.append("TML source above TML_MAX_BYTES is not refused")
    structure = by_boundary["TML structure"]
    if structure.exception == "omp.ui.TmlError" and getattr(
        _capture_tml_error(), "at", None
    ) == 6:
        findings.append("TmlError.at counts Unicode code points, not documented UTF-8 bytes")
    if any(
        by_boundary[f"device declaration reserved {name}"].disposition == "accepted"
        for name in ("do_", "future_")
    ):
        findings.append("reserved parameter names survive device schema derivation")
    if include_unreachable and (
        not hasattr(omp, "IncomingParams")
        or any(
            by_boundary["device args: " + label].disposition
            != (
                "accepted"
                if label in {"undeclared top-level key", "coercible wrong type"}
                else "refused"
            )
            for label in (
                "duplicate canonical key",
                "canonical plus alias",
                "two aliases",
                "undeclared top-level key",
                "coercible wrong type",
                "non-coercible wrong type",
            )
        )
    ):
        findings.append("the argument finalizer surface does not preserve typed outcomes")
    if include_unreachable and (
        not hasattr(omp.journal, "decode")
        or not hasattr(omp, "UnknownEntryKind")
        or not hasattr(omp, "EntryUndecodable")
    ):
        findings.append("the documented strict journal decode/refusal surface is not exported")

    return MalformedProbeReport(tuple(observations), tuple(findings))


def _capture_tml_error() -> BaseException | None:
    try:
        omp.ui.Tml.raw("é<row></bad>")
    except Exception as error:
        return error
    return None


def run_probe(*, include_unreachable: bool = True) -> MalformedProbeReport:
    """Run the async cursor checks from ordinary extension and smoke callers."""

    return asyncio.run(_run_probe(include_unreachable=include_unreachable))


def smoke() -> MalformedProbeReport:
    """Assert every locally reachable row and return the full evidence report."""

    report = run_probe()
    rows = {row.boundary: row for row in report.observations}

    assert rows["TML structure"].exception == "omp.ui.TmlError"
    assert rows["TML unknown tag"].disposition == "accepted"
    assert rows["TML raw controls"].disposition == "accepted"
    assert "\x00" not in omp.ui.text("bad\x00\x1b[2A").source
    assert rows["TML depth"].exception == "omp.ui.TmlError"
    assert rows["TML depth"].names_rule
    assert rows["TML bytes"].exception == "omp.ui.TmlError"
    assert rows["TML bytes"].names_rule
    assert getattr(_capture_tml_error(), "at", None) == 7
    assert rows["TML source type"].exception == "builtins.TypeError"

    for boundary in (
        "selector inverted range",
        "selector zero line",
        "selector negative line",
    ):
        assert rows[boundary].exception == "omp.urls.SelectorError"
        assert rows[boundary].names_rule

    if rows["URL unknown scheme"].disposition != "host-only":
        assert str(rows["URL unknown scheme"].message).endswith("UNKNOWN: 'unknown'>")
        assert rows["URL encoded selector delimiter"].disposition == "accepted"
        assert rows["URL malformed percent triplet"].disposition == "accepted"
    else:
        assert all(
            rows[boundary].disposition == "host-only"
            for boundary in (
                "URL unknown scheme",
                "URL encoded selector delimiter",
                "URL malformed percent triplet",
            )
        )

    for boundary in (
        "shortcut empty key",
        "shortcut duplicate modifier",
        "shortcut unknown modifier",
    ):
        assert rows[boundary].exception == "omp.ui.ShortcutError"
        assert rows[boundary].names_rule

    for name in ("do_", "future_"):
        row = rows[f"device declaration reserved {name}"]
        assert row.exception == "omp.devices.SchemaError"
        assert name in row.message
        assert "reserved-name rule" in row.message

    for label in (
        "duplicate canonical key",
        "canonical plus alias",
        "two aliases",
    ):
        row = rows["device args: " + label]
        assert row.exception == "omp.params.ArgFault"
        assert "ambiguous" in row.message
    extra = rows["device args: undeclared top-level key"]
    assert extra.disposition == "accepted"
    assert "'extra': 1" in extra.message
    repaired = rows["device args: coercible wrong type"]
    assert repaired.disposition == "accepted"
    assert "limit=42" in repaired.message
    assert "RepairKind.COERCION" in repaired.message
    mismatch = rows["device args: non-coercible wrong type"]
    assert mismatch.exception == "omp.params.ArgFault"
    assert "type_mismatch" in mismatch.message
    assert "example: 42" in mismatch.message

    assert rows["journal undeclared kind"].disposition == "host-only"
    assert rows["journal canonical JSON"].disposition == "accepted"
    assert rows["journal canonical JSON"].message == "{'count': 'wrong'}"
    assert not report.findings

    return report


@omp.device(
    "malformed_probe",
    family="conformance",
    rev=1,
    place="host",
    schema=MalformedProbeArgs,
    summary="Probe malformed values at every extension-owned decode boundary.",
)
async def malformed_probe(
    args: MalformedProbeArgs, ctx: omp.Context
) -> MalformedProbeReport:
    """Run the local probe without mutating host or journal state."""

    del ctx
    return await _run_probe(include_unreachable=args.include_unreachable)
