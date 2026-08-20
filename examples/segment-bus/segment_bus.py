from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import omp
from omp import ui

MAX_SEGMENTS_PER_PUBLISHER = 8
"""Maximum number of live segments accepted from one publisher."""

MAX_BYTES_PER_PUBLISHER = 1024
"""Maximum UTF-8 payload bytes retained for one publisher."""

_STATUS_KEY = "segment-bus"
_STATUS_ORDER = 60
_TONES = frozenset(token.value for token in ui.Token)


@dataclass(frozen=True, slots=True)
class Segment:
    """One publisher-owned status segment and its default internal priority."""

    key: str
    text: str
    priority: int = 100
    tone: Literal[
        "fg",
        "accent",
        "info",
        "ok",
        "warn",
        "err",
        "muted",
        "border",
        "surface",
        "hover",
        "selection",
        "shadow",
        "panel",
        "secondary",
        "contrast",
    ] = "muted"


@dataclass(frozen=True, slots=True)
class PublishRequest:
    """Atomic replacement for all segments owned by the calling extension."""

    segments: tuple[Segment, ...]


@dataclass(frozen=True, slots=True)
class PublishReceipt:
    """Accepted publisher totals after an atomic segment replacement."""

    publisher: str
    count: int
    bytes: int


class PublisherQuotaExceeded(ValueError):
    """Refusal raised before state changes when a publisher exceeds a quota."""

    def __init__(self, publisher: str, quota: str, limit: int, actual: int) -> None:
        super().__init__(
            f"publisher {publisher!r} exceeds {quota} quota: {actual} > {limit}"
        )
        self.publisher = publisher
        self.quota = quota
        self.limit = limit
        self.actual = actual


_segments_by_publisher: dict[str, tuple[Segment, ...]] = {}


def _validate_segment(segment: Segment) -> None:
    if not isinstance(segment, Segment):
        raise TypeError("segments must contain Segment values")
    if not segment.key or segment.key != segment.key.strip():
        raise ValueError("segment key must be non-empty and have no surrounding whitespace")
    if not isinstance(segment.text, str):
        raise TypeError("segment text must be str")
    if isinstance(segment.priority, bool) or not isinstance(segment.priority, int):
        raise TypeError("segment priority must be int")
    if segment.tone not in _TONES:
        raise ValueError(f"unknown semantic tone: {segment.tone!r}")


def _payload_bytes(segments: tuple[Segment, ...]) -> int:
    return sum(
        len(segment.key.encode("utf-8"))
        + len(segment.text.encode("utf-8"))
        + len(segment.tone.encode("ascii"))
        + len(str(segment.priority).encode("ascii"))
        for segment in segments
    )


def _render() -> None:
    ordered = sorted(
        (
            (segment.priority, publisher, segment.key, segment)
            for publisher, segments in _segments_by_publisher.items()
            for segment in segments
        ),
        key=lambda item: item[:3],
    )
    content = ui.join(
        (
            ui.tml(
                "<segment fg={tone}>{label}</segment>",
                tone=segment.tone,
                label=ui.text(segment.text),
            )
            for _, _, _, segment in ordered
        )
    )
    ui.set_status(
        _STATUS_KEY,
        content if ordered else None,
        order=_STATUS_ORDER,
        side=ui.Slot.STATUS_RIGHT,
    )


@omp.service("segments.publish", rev=1)
class SegmentPublisherService:
    """Manifest-gated owner of the shared statusline contribution."""

    async def publish(self, request: PublishRequest) -> PublishReceipt:
        """Replace the caller's bounded segment set and repaint once."""

        publisher = omp.Context.current().extension
        if not publisher:
            raise ValueError("service caller has no extension identity")
        if not isinstance(request, PublishRequest):
            raise TypeError("publish expects PublishRequest")

        segments = request.segments
        if not isinstance(segments, tuple):
            raise TypeError("PublishRequest.segments must be a tuple")
        if len(segments) > MAX_SEGMENTS_PER_PUBLISHER:
            raise PublisherQuotaExceeded(
                publisher,
                "segments",
                MAX_SEGMENTS_PER_PUBLISHER,
                len(segments),
            )

        keys: set[str] = set()
        for segment in segments:
            _validate_segment(segment)
            if segment.key in keys:
                raise ValueError(f"duplicate segment key: {segment.key!r}")
            keys.add(segment.key)

        payload_bytes = _payload_bytes(segments)
        if payload_bytes > MAX_BYTES_PER_PUBLISHER:
            raise PublisherQuotaExceeded(
                publisher,
                "bytes",
                MAX_BYTES_PER_PUBLISHER,
                payload_bytes,
            )

        if segments:
            _segments_by_publisher[publisher] = segments
        else:
            _segments_by_publisher.pop(publisher, None)
        _render()
        return PublishReceipt(publisher, len(segments), payload_bytes)
