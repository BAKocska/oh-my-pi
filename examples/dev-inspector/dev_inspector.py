"""Read-only prompt and model-request inspection overlays."""

from __future__ import annotations

from collections.abc import Mapping, Sequence

import omp
from omp import telemetry, ui


_RECENT_REQUESTS = 12
_SLOT_BANDS: Mapping[str, omp.SlotClass] = {
    "conventions": omp.SlotClass.FROZEN,
    "role": omp.SlotClass.FROZEN,
    "runtime": omp.SlotClass.FROZEN,
    "tools": omp.SlotClass.STABLE,
    "policy": omp.SlotClass.STABLE,
    "workflow": omp.SlotClass.FROZEN,
    "skills": omp.SlotClass.STABLE,
    "rules": omp.SlotClass.STABLE,
    "guidance": omp.SlotClass.STABLE,
    "workspace": omp.SlotClass.STABLE,
    "memory": omp.SlotClass.EPOCHAL,
    "standing": omp.SlotClass.EPOCHAL,
    "recall": omp.SlotClass.VOLATILE,
    "status": omp.SlotClass.VOLATILE,
    "delivery": omp.SlotClass.FROZEN,
}
_REDACTION_NOTICE = (
    "Request and response content is redacted. Grant telemetry.capture_content "
    "explicitly to expose content-bearing telemetry fields."
)


def _slot_name(slot_id: str) -> str:
    """Return the catalog slot prefix of an assembler fingerprint key."""

    for separator in ("/", ":"):
        if separator in slot_id:
            return slot_id.split(separator, 1)[0]
    return slot_id


def _prompt_view(request: telemetry.ModelRequest | None) -> ui.Tml:
    """Render assembler-owned prompt fingerprints without recomputing hashes."""

    if request is None:
        return ui.tml(
            "<box title='Prompt inspector' border=round pad='1 2'>"
            "<callout fg=muted>No settled model requests are available yet.</callout>"
            "</box>"
        )
    changed = frozenset(request.prompt.changed)
    rows = tuple(
        ui.tml(
            "<tr><td><text>{slot_id}</text></td><td><text>{bytes}</text></td>"
            "<td><text>{band}</text></td><td><text>{changed}</text></td></tr>",
            slot_id=slot_id,
            bytes="unavailable",
            band=(
                _SLOT_BANDS[_slot_name(slot_id)].value
                if _slot_name(slot_id) in _SLOT_BANDS
                else "unknown"
            ),
            changed="yes" if slot_id in changed else "no",
        )
        for slot_id in request.prompt.slots
    )
    body = rows or (
        ui.tml(
            "<tr><td><text fg=muted>No cacheable prompt slots.</text></td>"
            "<td><text>—</text></td><td><text>—</text></td><td><text>—</text></td></tr>"
        ),
    )
    return ui.tml(
        "<box title='Prompt inspector' border=round pad='1 2'><col gap=1>"
        "<text fg=muted>Assembler fingerprint {digest}; stable prefix {stable} bytes.</text>"
        "<table gap=2><tr><td><text bold>Slot id</text></td>"
        "<td><text bold>Bytes</text></td><td><text bold>Band</text></td>"
        "<td><text bold>Changed</text></td></tr>{rows}</table>"
        "<callout fg=warn>Per-slot byte sizes are unavailable on the frozen fingerprint surface.</callout>"
        "</col></box>",
        digest=request.prompt.digest,
        stable=str(request.prompt.prefix_stable_bytes),
        rows=body,
    )


def _request_view(
    requests: Sequence[telemetry.ModelRequest], *, content_granted: bool
) -> ui.Tml:
    """Render recent settled requests while enforcing the content grant boundary."""

    rows = tuple(
        ui.tml(
            "<tr><td><text>{model}</text></td><td><text>{tokens}</text></td>"
            "<td><text>{cache}</text></td><td><text>{degraded}</text></td></tr>",
            model=request.served_model,
            tokens=f"{request.usage.total:,}",
            cache=f"{request.usage.cache_hit_rate:.0%}",
            degraded="unavailable",
        )
        for request in requests
    )
    body = rows or (
        ui.tml(
            "<tr><td><text fg=muted>No settled model requests.</text></td>"
            "<td><text>—</text></td><td><text>—</text></td><td><text>—</text></td></tr>"
        ),
    )
    if content_granted:
        detail = tuple(
            ui.tml(
                "<text>{model}: Tokens.detail={detail}; args_raw/response content unavailable</text>",
                model=request.served_model,
                detail=str(dict(request.usage.detail)),
            )
            for request in requests
        ) or (ui.text("No content-bearing telemetry rows."),)
        content = ui.tml(
            "<callout fg=warn>Content grant active. The frozen ModelRequest exposes Tokens.detail, "
            "but no args_raw or response-content field.</callout><col gap=1>{detail}</col>",
            detail=detail,
        )
    else:
        content = ui.tml("<callout fg=warn>{notice}</callout>", notice=_REDACTION_NOTICE)
    return ui.tml(
        "<box title='Request inspector' border=round pad='1 2'><col gap=1>"
        "<table gap=2><tr><td><text bold>Model</text></td>"
        "<td><text bold>Tokens</text></td><td><text bold>Cache hit</text></td>"
        "<td><text bold>Degradations</text></td></tr>{rows}</table>{content}"
        "</col></box>",
        rows=body,
        content=content,
    )


async def _recent_requests(session: str) -> tuple[telemetry.ModelRequest, ...]:
    """Query recent model requests from the current session only."""

    result = await telemetry.query(
        telemetry.Query(
            match=(telemetry.Step(kinds=(telemetry.Kind.MODEL_REQUEST,), name="request"),),
            same_turn=False,
            scope=telemetry.Scope.SELF,
            sessions=(session,),
            order_by=("-seq",),
            limit=_RECENT_REQUESTS,
        )
    )
    return tuple(
        event
        for row in result.rows
        for event in row.events
        if isinstance(event, telemetry.ModelRequest)
    )


@omp.command(
    "inspect",
    description="Inspect prompt fingerprints or recent model requests",
    args=(ui.Arg("view", "Overlay view", usage="prompt | request"),),
    hint="prompt | request",
)
async def inspect(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed | None:
    """Open one read-only developer-inspection overlay."""

    view = inv.argv[0] if inv.argv else "prompt"
    if view not in {"prompt", "request"} or len(inv.argv) > 1:
        return ui.Consumed(ui.text("Usage: /inspect [prompt|request]"))
    if not ctx.has_ui:
        return ui.Consumed(ui.text("The inspector requires an interactive UI."))

    requests = await _recent_requests(ctx.session)
    content = (
        _prompt_view(requests[0] if requests else None)
        if view == "prompt"
        else _request_view(
            requests, content_granted="telemetry.capture_content" in ctx.caps
        )
    )
    async with await ui.overlay(
        content, ui.OverlayOptions(width=ui.Pct(92), max_height=ui.Pct(86))
    ) as overlay:
        await overlay.wait()
    return None
