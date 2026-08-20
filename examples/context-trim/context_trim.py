"""Keep context headroom with bounded patches over stable thread item ids."""

from __future__ import annotations

import omp

_HEADROOM_FRACTION = 0.10
_SUPERSEDED_DISTANCE = 12
_GIANT_DISTANCE = 20
_GIANT_TOKENS = 2_048
_MAX_OPS_BELOW_THRESHOLD = 8
_MAX_OPS_AT_THRESHOLD = 16


@omp.hook("thread_projection")
async def trim_context(
    view: omp.ContextView, ctx: omp.Context
) -> omp.ContextPatch | None:
    """Prune or elide old tool outputs until the configured context headroom is restored."""
    del ctx
    target_fraction = max(0.0, view.usage.threshold_fraction - _HEADROOM_FRACTION)
    target_tokens = int(view.usage.usable_tokens * target_fraction)
    tokens_to_reclaim = max(0, view.usage.total_tokens - target_tokens)
    if tokens_to_reclaim == 0:
        return None

    max_ops = (
        _MAX_OPS_AT_THRESHOLD
        if view.usage.fraction >= view.usage.threshold_fraction
        else _MAX_OPS_BELOW_THRESHOLD
    )
    positions = {message.id: index for index, message in enumerate(view.messages)}
    last_index = len(view.messages) - 1
    reclaimed = 0
    patch = omp.ContextPatch(note="restore context headroom from old tool outputs")

    pruned_ids: set[str] = set()
    for index, message in enumerate(view.messages):
        if len(patch.prune) >= max_ops or reclaimed >= tokens_to_reclaim:
            break
        if message.kind is not omp.MessageKind.TOOL_RESULT or message.pinned:
            continue

        superseding_index = positions.get(message.superseded_by or "")
        if (
            last_index - index < _SUPERSEDED_DISTANCE
            or superseding_index is None
            or superseding_index <= index
        ):
            continue
        patch.prune.append(
            omp.Prune(
                ids=(message.id,),
                reason=f"superseded by stable item {message.superseded_by}",
            )
        )
        pruned_ids.add(message.id)
        reclaimed += message.tokens

    for index, message in enumerate(view.messages):
        if len(patch.prune) + len(patch.replace) >= max_ops:
            break
        if reclaimed >= tokens_to_reclaim:
            break
        if (
            message.kind is not omp.MessageKind.TOOL_RESULT
            or message.pinned
            or message.id in pruned_ids
            or last_index - index < _GIANT_DISTANCE
            or message.tokens < _GIANT_TOKENS
            or message.elided
            or not message.artifacts
        ):
            continue

        artifact = message.artifacts[0]
        tool = str(message.tool) if message.tool is not None else "unknown"
        marker = (
            "[headroom-elided type=tool_result "
            f"tool={tool} artifact={artifact} original_tokens={message.tokens}]"
        )
        patch.replace.append(
            omp.Replace(
                ids=(message.id,),
                parts=(omp.Part.text(marker),),
                role="user",
                label=f"headroom: {tool} output",
            )
        )
        reclaimed += message.tokens

    return patch if not patch.is_empty() else None
