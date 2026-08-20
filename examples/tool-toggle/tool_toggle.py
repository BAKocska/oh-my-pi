from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable

import omp
from omp import ui


_SESSION = omp.StateScope.SESSION


class CatalogContractError(RuntimeError):
    """Report catalog metadata missing from the frozen Python surface."""


@omp.entry_kind("examples.tool-toggle.selection", rev="v.1", display=False)
@dataclass(frozen=True, slots=True)
class ToolSelection:
    """Persist the complete enabled-path selection for one session."""

    enabled: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class CatalogRow:
    """Normalize the catalog fields consumed by the picker."""

    path: str
    summary: str
    mounted: bool
    slotted: bool
    schema_bytes: int
    schema_tokens: int
    provenance: omp.Provenance


def _catalog_rows() -> tuple[CatalogRow, ...]:
    """Read and validate the session's complete frozen device catalog."""

    listing = getattr(omp.devices, "list", None)
    if not callable(listing):
        raise CatalogContractError("frozen omp.devices.list is unavailable")

    rows: list[CatalogRow] = []
    for item in listing(mounted_only=False):
        provenance = getattr(item, "provenance", None)
        slotted = getattr(item, "slotted", None)
        schema_bytes = getattr(item, "schema_bytes", None)
        schema_tokens = getattr(item, "schema_tokens", None)
        if not isinstance(provenance, omp.Provenance):
            raise CatalogContractError(
                f"{item.path}: catalog row lacks omp.DeviceInfo.provenance"
            )
        if not isinstance(slotted, bool):
            raise CatalogContractError(
                f"{item.path}: catalog row lacks omp.DeviceInfo.slotted"
            )
        if not isinstance(schema_bytes, int) or schema_bytes < 0:
            raise CatalogContractError(
                f"{item.path}: catalog row lacks omp.DeviceInfo.schema_bytes"
            )
        if not isinstance(schema_tokens, int) or schema_tokens < 0:
            raise CatalogContractError(
                f"{item.path}: catalog row lacks omp.DeviceInfo.schema_tokens"
            )
        rows.append(
            CatalogRow(
                path=str(item.path),
                summary=item.summary or "No summary supplied.",
                mounted=item.mounted,
                slotted=slotted,
                schema_bytes=schema_bytes,
                schema_tokens=schema_tokens,
                provenance=provenance,
            )
        )
    return tuple(rows)


def _provenance_group(provenance: omp.Provenance) -> str:
    """Render all seven structurally stamped provenance fields as a group key."""

    digest = provenance.artifact_digest
    return (
        f"{provenance.publisher} / {provenance.extension_id}"
        f" · {provenance.version} · {digest} · {provenance.layer}"
        f" · {provenance.tier} · generation {provenance.generation}"
    )


def _context_cost(row: CatalogRow) -> str:
    """Price schema slots while making schema-free devices visibly free."""

    if not row.slotted:
        return "0 B · 0 tokens · dyn device"
    return f"{row.schema_bytes} B · {row.schema_tokens} tokens · schema slot"


def _matches_focus(row: CatalogRow, focus: str) -> bool:
    """Match a focus query against path and every provenance field."""

    if not focus:
        return True
    provenance = row.provenance
    values = (
        row.path,
        provenance.publisher,
        provenance.extension_id,
        provenance.version,
        provenance.artifact_digest,
        provenance.layer,
        provenance.tier,
        str(provenance.generation),
    )
    needle = focus.casefold()
    return any(needle in value.casefold() for value in values)


def _picker_items(rows: Iterable[CatalogRow], focus: str = "") -> tuple[ui.SelectItem, ...]:
    """Build grouped picker rows with explicit context cost."""

    return tuple(
        ui.SelectItem(
            value=row.path,
            label=row.path,
            desc=f"{_context_cost(row)} — {row.summary}",
            cells=(_context_cost(row),),
            group=_provenance_group(row.provenance),
        )
        for row in rows
        if _matches_focus(row, focus)
    )


async def _saved_selection() -> ToolSelection | None:
    """Read the latest session-scoped selection snapshot."""

    record = await omp.state.latest(ToolSelection, scope=_SESSION)
    value = None if record is None else record.value
    return value if isinstance(value, ToolSelection) else None


async def _apply_selection(
    rows: Iterable[CatalogRow], enabled: Iterable[str], *, persist: bool
) -> tuple[omp.AvailabilityDelta, ...]:
    """Apply one batched availability transition and optionally persist it."""

    selected = frozenset(enabled)
    ordered_rows = tuple(rows)
    known = frozenset(row.path for row in ordered_rows)
    unknown = selected - known
    if unknown:
        raise ValueError(f"unknown catalog paths: {', '.join(sorted(unknown))}")

    deltas = tuple(
        omp.AvailabilityDelta(
            path=row.path,
            mounted=row.path in selected,
            reason=None if row.path in selected else "disabled by tool-toggle",
        )
        for row in ordered_rows
        if row.mounted != (row.path in selected)
    )
    if deltas:
        await omp.devices.set_availability(*deltas)
    if persist:
        await omp.state.append(
            ToolSelection(enabled=tuple(sorted(selected))), scope=_SESSION
        )
    return deltas


@omp.command(
    "tool-toggle",
    description="Focus, group, and toggle extension tools with visible context cost",
    args=(ui.Arg("focus", "Filter by path or provenance", usage="[query]"),),
    hint="[path, publisher, extension, version, digest, layer, tier, or generation]",
)
async def tool_toggle(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Open the grouped tool picker and commit one availability batch."""

    del ctx
    focus = " ".join(inv.argv).strip()
    rows = _catalog_rows()
    visible = tuple(row for row in rows if _matches_focus(row, focus))
    if not visible:
        return ui.Consumed(ui.text(f"No catalog rows match {focus!r}."))

    saved = await _saved_selection()
    known_paths = frozenset(row.path for row in rows)
    checked = (
        tuple(row.path for row in rows if row.mounted)
        if saved is None
        else tuple(path for path in saved.enabled if path in known_paths)
    )
    outcome = await ui.multi_select(
        "Extension tools",
        _picker_items(visible),
        checked=tuple(path for path in checked if path in {row.path for row in visible}),
        options=ui.DialogOptions(
            help="Space toggles. Devices cost zero schema tokens; slot tools do not.",
            overlay=ui.OverlayOptions(
                width=ui.Pct(86), max_height=ui.Pct(82), fill_height=True
            ),
            context=ui.text(
                "Availability is committed as one TurnBoundary batch. The resulting "
                "catalog notice lives in messages; its catalog_notice token split is "
                "not yet settled."
            ),
        ),
    )
    if not outcome:
        return ui.Consumed()

    visible_paths = frozenset(row.path for row in visible)
    selected = (frozenset(checked) - visible_paths) | frozenset(outcome.values)
    deltas = await _apply_selection(rows, selected, persist=True)
    return ui.Consumed(
        ui.text(f"Updated {len(deltas)} tool availabilities as one turn-boundary batch.")
    )


@omp.hook("extension_activate")
async def restore_selection(payload: object, ctx: omp.Context) -> None:
    """Restore a saved selection when this extension activates in a session."""

    del payload, ctx
    saved = await _saved_selection()
    if saved is not None:
        rows = _catalog_rows()
        known_paths = frozenset(row.path for row in rows)
        await _apply_selection(
            rows,
            (path for path in saved.enabled if path in known_paths),
            persist=False,
        )
