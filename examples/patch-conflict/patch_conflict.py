"""Conformance probe for conflicting context patches and compaction epochs."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable

import omp


@dataclass(frozen=True, slots=True)
class _Projected:
    id: str
    parts: tuple[omp.TextPart, ...]


@dataclass(frozen=True, slots=True)
class ProbeObservation:
    """One boundary result produced by the stub projection host."""

    case: str
    observed: str


@dataclass(frozen=True, slots=True)
class _Resolved:
    origin: str
    operation: object
    ids: tuple[str, ...]


class _ProjectionStub:
    """Small agent-side oracle that validates a view before materializing its plan."""

    def __init__(
        self,
        view: omp.ContextView,
        *,
        epoch: int | None = None,
        live_ids: Iterable[str] | None = None,
    ) -> None:
        self.epoch = view.epoch if epoch is None else epoch
        self._refs = {message.id: message for message in view.messages}
        self._live_ids = set(self._refs) if live_ids is None else set(live_ids)
        self.items = [
            _Projected(message.id, (omp.Part.text(message.preview),))
            for message in view.messages
        ]
        self.diagnostics: list[str] = []
        self._mutating = False

    def snapshot(self) -> tuple[tuple[str, tuple[omp.TextPart, ...]], ...]:
        """Return a comparable snapshot of the working projection."""

        return tuple((item.id, item.parts) for item in self.items)

    def _drop(self, origin: str, operation: object, reason: str) -> None:
        if self._mutating:
            raise AssertionError("validation happened after projection mutation began")
        self.diagnostics.append(
            f"PatchRejected:{origin}:{type(operation).__name__}:{reason}"
        )

    @staticmethod
    def _operations(patch: omp.ContextPatch) -> Iterable[object]:
        # This ordering is only a stable traversal. Overlap resolution is by handler
        # order: the touched-id set makes operation class order immaterial.
        yield from patch.prune
        yield from patch.drop_parts
        yield from patch.replace
        yield from patch.insert
        yield from patch.reorder

    @staticmethod
    def _ids(operation: object) -> tuple[str, ...]:
        if isinstance(operation, (omp.Prune, omp.DropParts, omp.Replace, omp.Reorder)):
            return operation.ids
        return ()

    def apply(
        self,
        view: omp.ContextView,
        contributions: tuple[tuple[str, omp.ContextPatch], ...],
    ) -> tuple[str, ...]:
        """Validate all operations, then materialize accepted operations once."""

        before = self.snapshot()
        if view.epoch != self.epoch:
            if self.snapshot() != before:
                raise AssertionError("stale epoch mutated the projection")
            raise omp.ContextGone(
                f"projection epoch {view.epoch} is stale; current epoch is {self.epoch}"
            )

        accepted: list[_Resolved] = []
        touched: set[str] = set()
        for origin, patch in contributions:
            for operation in self._operations(patch):
                ids = self._ids(operation)
                if len(ids) != len(set(ids)):
                    self._drop(origin, operation, "duplicate id")
                    continue

                # Prune explicitly ignores unknown ids in docs/py/08. Other target
                # ids are structurally required and are rejected by preflight.
                if isinstance(operation, omp.Prune):
                    known_ids = tuple(id_ for id_ in ids if id_ in self._refs)
                    for id_ in ids:
                        if id_ not in self._refs:
                            self.diagnostics.append(
                                f"Ignored:{origin}:Prune:unknown id {id_}"
                            )
                    ids = known_ids
                elif isinstance(operation, omp.Insert):
                    anchor = operation.anchor
                    if anchor.id is not None and anchor.id not in self._refs:
                        self._drop(origin, operation, f"unknown anchor {anchor.id}")
                        continue
                elif any(id_ not in self._refs for id_ in ids):
                    self._drop(origin, operation, "unknown required id")
                    continue

                if any(id_ not in self._live_ids for id_ in ids):
                    self._drop(origin, operation, "id left live chain")
                    continue
                if any(self._refs[id_].pinned for id_ in ids):
                    self._drop(origin, operation, "pinned target")
                    continue
                if isinstance(operation, omp.Reorder):
                    if operation.before not in self._refs:
                        self._drop(origin, operation, "unknown before id")
                        continue
                    if operation.before not in self._live_ids:
                        self._drop(origin, operation, "before id left live chain")
                        continue
                overlap = touched.intersection(ids)
                if overlap:
                    self._drop(
                        origin,
                        operation,
                        f"conflict with earlier handler on {','.join(sorted(overlap))}",
                    )
                    continue
                touched.update(ids)
                accepted.append(_Resolved(origin, operation, ids))

        if self.snapshot() != before:
            raise AssertionError("preflight mutated the projection")

        self._mutating = True
        for index, resolved in enumerate(accepted):
            operation = resolved.operation
            if isinstance(operation, omp.Prune):
                self.items = [item for item in self.items if item.id not in resolved.ids]
            elif isinstance(operation, omp.DropParts):
                self.items = [
                    _Projected(item.id, ()) if item.id in resolved.ids else item
                    for item in self.items
                ]
            elif isinstance(operation, omp.Replace):
                positions = [
                    position
                    for position, item in enumerate(self.items)
                    if item.id in resolved.ids
                ]
                if not positions:
                    continue
                position = min(positions)
                self.items = [item for item in self.items if item.id not in resolved.ids]
                self.items.insert(
                    position,
                    _Projected(
                        f"synthetic:{resolved.origin}:{index}",
                        tuple(operation.parts),
                    ),
                )
            elif isinstance(operation, omp.Reorder):
                moving = [item for item in self.items if item.id in resolved.ids]
                remaining = [item for item in self.items if item.id not in resolved.ids]
                before = next(
                    position
                    for position, item in enumerate(remaining)
                    if item.id == operation.before
                )
                self.items = [*remaining[:before], *moving, *remaining[before:]]
            elif isinstance(operation, omp.Insert):
                # No matrix row uses Insert; accepting it still proves preflight is
                # independent of mutation without pretending to assign a durable id.
                continue
        self._mutating = False
        return tuple(item.id for item in self.items)


@omp.hook("thread_projection")
async def winning_projection(
    view: omp.ContextView, ctx: omp.Context
) -> omp.ContextPatch:
    """Contribute the earlier replace/prune operations for the conflict rows."""

    del view, ctx
    return omp.ContextPatch(
        prune=[omp.Prune(ids=("move",), reason="earlier prune wins")],
        replace=[
            omp.Replace(
                ids=("shared",),
                parts=(omp.Part.text("replacement"),),
                label="earlier replacement wins",
            )
        ],
        note="earlier deterministic handler",
    )


async def losing_projection(
    view: omp.ContextView, ctx: omp.Context
) -> omp.ContextPatch:
    """Contribute later operations that overlap the earlier handler's ids."""

    del view, ctx
    return omp.ContextPatch(
        drop_parts=[omp.DropParts(ids=("shared",), reason="later overlap")],
        reorder=[omp.Reorder(ids=("move",), before="tail")],
        note="later deterministic handler",
    )


def _message(
    id_: str,
    *,
    pinned: bool = False,
    kind: omp.MessageKind = omp.MessageKind.USER,
) -> omp.MessageRef:
    return omp.MessageRef(
        id=id_,
        event=1,
        seq=1,
        kind=kind,
        role="user",
        turn_id="turn-1",
        created_at_ms=0,
        tokens=1,
        byte_len=len(id_),
        part_count=1,
        media_count=0,
        tool=None,
        is_error=False,
        useless=False,
        pinned=pinned,
        elided=False,
        superseded_by=None,
        artifacts=(),
        preview=id_,
    )


def stub_view() -> omp.ContextView:
    """Build the pinned, overlapping, and stale-id fixture used by the smoke."""

    usage = omp.ContextUsage(
        total_tokens=6,
        context_window=128,
        reserve_tokens=8,
        usable_tokens=120,
        fraction=0.05,
        prompt_head_tokens=1,
        device_catalog_tokens=0,
        message_tokens=5,
        catalog_notice_tokens=0,
        media_tokens=0,
        compaction_epoch=7,
        threshold_fraction=0.8,
        in_flight=True,
    )
    return omp.ContextView(
        session_id="session-1",
        turn_id="turn-1",
        model="stub",
        provider="stub",
        epoch=7,
        messages=(
            _message("pinned", pinned=True),
            _message("shared", kind=omp.MessageKind.TOOL_RESULT),
            _message("move"),
            _message("safe"),
            _message("stale"),
            _message("tail"),
        ),
        usage=usage,
        prompt_hash="00",
        reset_event=None,
    )


async def smoke(ctx: omp.Context) -> tuple[ProbeObservation, ...]:
    """Drive every README matrix row against a deterministic stub host."""

    view = stub_view()
    winner = await winning_projection(view, ctx)
    loser = await losing_projection(view, ctx)
    conflict_host = _ProjectionStub(view)
    conflict_ids = conflict_host.apply(
        view, (("winner", winner), ("loser", loser))
    )
    conflict_drops = tuple(
        row for row in conflict_host.diagnostics if "conflict with earlier handler" in row
    )
    assert len(conflict_drops) == 2
    assert "shared" not in conflict_ids and "move" not in conflict_ids
    assert any(id_.startswith("synthetic:winner:") for id_ in conflict_ids)

    validation_host = _ProjectionStub(
        view,
        live_ids=(set(conflict_ids) | {"pinned", "safe", "tail"}) - {"stale"},
    )
    validation_before = validation_host.snapshot()
    validation_ids = validation_host.apply(
        view,
        (
            (
                "mixed",
                omp.ContextPatch(
                    prune=[
                        omp.Prune(ids=("pinned",), reason="must refuse pin"),
                        omp.Prune(ids=("safe",), reason="valid sibling"),
                        omp.Prune(ids=("unknown",), reason="documented ignore"),
                    ],
                    drop_parts=[omp.DropParts(ids=("stale",), reason="stale id")],
                    replace=[
                        omp.Replace(
                            ids=("missing",),
                            parts=(omp.Part.text("never applied"),),
                        )
                    ],
                    reorder=[omp.Reorder(ids=("tail", "tail"), before="shared")],
                ),
            ),
        ),
    )
    assert "pinned" in validation_ids
    assert "safe" not in validation_ids
    assert any("pinned target" in row for row in validation_host.diagnostics)
    assert any("unknown required id" in row for row in validation_host.diagnostics)
    assert any("duplicate id" in row for row in validation_host.diagnostics)
    assert any("id left live chain" in row for row in validation_host.diagnostics)
    assert any(row.startswith("Ignored:mixed:Prune") for row in validation_host.diagnostics)
    # Only the valid sibling may distinguish the final result from the original.
    expected_validation = tuple(row for row in validation_before if row[0] != "safe")
    assert validation_host.snapshot() == expected_validation

    epoch_host = _ProjectionStub(view, epoch=view.epoch + 1)
    epoch_before = epoch_host.snapshot()
    try:
        epoch_host.apply(
            view,
            (("late", omp.ContextPatch(prune=[omp.Prune(ids=("safe",))])),),
        )
    except omp.ContextGone:
        pass
    else:
        raise AssertionError("stale projection epoch was silently accepted")
    assert epoch_host.snapshot() == epoch_before

    return (
        ProbeObservation("overlapping handlers", "later conflicting ops dropped"),
        ProbeObservation("stale/nonexistent/duplicate ids", "rejected before mutation"),
        ProbeObservation("unknown prune id", "ignored by the documented exception"),
        ProbeObservation("pinned target", "refused; valid sibling still applied"),
        ProbeObservation("reorder plus prune", "earlier prune won; reorder dropped"),
        ProbeObservation("drop-parts plus replace", "earlier replace won; drop dropped"),
        ProbeObservation("compaction race", "ContextGone; projection unchanged"),
    )
