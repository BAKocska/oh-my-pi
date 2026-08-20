"""Drop stale tool-result projections without discarding their typed verdicts."""

from __future__ import annotations

import omp

from omp import DropParts  

_HEADROOM_FRACTION = 0.08
_KEEP_RECENT_TURNS = 4
_MIN_RESULT_BYTES = 4_096
_MAX_DROP_OPS = 16


@omp.hook("thread_projection")
async def prune_stale_parts(
    view: omp.ContextView, ctx: omp.Context
) -> omp.ContextPatch | None:
    """Drop eligible historical result parts until the projection regains headroom."""

    del ctx
    target_fraction = max(0.0, view.usage.threshold_fraction - _HEADROOM_FRACTION)
    target_tokens = int(view.usage.usable_tokens * target_fraction)
    tokens_to_reclaim = max(0, view.usage.total_tokens - target_tokens)
    if tokens_to_reclaim == 0:
        return None

    turn_ids = [turn_id for turn_id, _messages in view.by_turn() if turn_id]
    protected_turns = frozenset(turn_ids[-_KEEP_RECENT_TURNS:])
    patch = omp.ContextPatch(note="drop stale useless tool-result parts")
    reclaimed = 0

    for message in view.messages:
        if len(patch.drop_parts) >= _MAX_DROP_OPS or reclaimed >= tokens_to_reclaim:
            break
        if (
            message.kind is not omp.MessageKind.TOOL_RESULT
            or message.turn_id in protected_turns
            or message.pinned
            or message.elided
            or not message.useless
            or message.part_count == 0
            or message.byte_len < _MIN_RESULT_BYTES
        ):
            continue

        patch.drop_parts.append(
            DropParts(
                ids=(message.id,),
                reason="historical useless result exceeds the projection budget",
            )
        )
        reclaimed += message.tokens

    return patch if not patch.is_empty() else None
