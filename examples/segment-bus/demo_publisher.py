from __future__ import annotations

from typing import Protocol, cast

import omp

from segment_bus import PublishReceipt, PublishRequest, Segment


class _SegmentsClient(Protocol):
    async def publish(self, request: PublishRequest) -> PublishReceipt: ...


@omp.hook("extension_activate")
async def publish_demo(payload: object, ctx: omp.Context) -> None:
    """Exercise the granted consumer path with one small demo segment."""

    del payload, ctx
    client = cast(
        _SegmentsClient,
        await omp.services.connect("segments.publish", rev=1),
    )
    await client.publish(
        PublishRequest(
            (
                Segment(
                    key="demo",
                    text="service linked",
                    priority=40,
                    tone="accent",
                ),
            )
        )
    )
