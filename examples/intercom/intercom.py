from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import Literal

from omp import Context, Duration, device, service
from omp.agents import DeliveryMode
# GAP: not in frozen layer (docs/py/12-agents.md §Inter-session messaging).
from omp.agents import AgentRef, Message, inbox, peers as list_peers, send, wait_for


@dataclass(frozen=True, slots=True)
class DeliveryView:
    """One destination and its core broker receipt."""

    to: str
    receipt: str


@dataclass(frozen=True, slots=True)
class MessageView:
    """Stable projection of one canonical peer-message journal item."""

    id: str
    from_: str
    to: str
    text: str
    mode: str
    reply_to: str | None
    sent_ms: int
    session_id: str


@dataclass(frozen=True, slots=True)
class IntercomArgs:
    """Arguments for sending, draining, or waiting for peer messages."""

    op: Literal["send", "inbox", "wait"]
    to: list[str] = field(default_factory=list)
    text: str | None = None
    mode: Literal["aside", "steer", "next_turn"] = "aside"
    reply_to: str | None = None
    sender: str | None = None
    peek: bool = False
    limit: int | None = None
    timeout: str = "60s"


@dataclass(frozen=True, slots=True)
class IntercomResult:
    """Receipts or messages returned by one intercom operation."""

    receipts: list[DeliveryView] = field(default_factory=list)
    messages: list[MessageView] = field(default_factory=list)


@dataclass(frozen=True, slots=True)
class NotifyRequest:
    """Typed service request for non-blocking notification fan-out."""

    recipients: list[str]
    text: str
    mode: Literal["aside", "steer", "next_turn"] = "aside"
    reply_to: str | None = None


@dataclass(frozen=True, slots=True)
class NotifyResult:
    """Per-destination receipts from a notification fan-out."""

    receipts: list[DeliveryView]


@dataclass(frozen=True, slots=True)
class PeersArgs:
    """Scope of the core-owned peer roster to list."""

    scope: Literal["session", "project"] = "session"


@dataclass(frozen=True, slots=True)
class PeerView:
    """Compact addressable row folded from an agent roster entry."""

    id: str
    name: str
    kind: str
    status: str
    agent: str
    parent: str | None
    depth: int
    activity: str
    last_activity_ms: int
    output_url: str
    transcript_url: str


@dataclass(frozen=True, slots=True)
class PeersResult:
    """Ordered roster returned by the core broker."""

    peers: list[PeerView]


def _message_view(message: Message) -> MessageView:
    return MessageView(
        id=message.id,
        from_=message.from_,
        to=message.to,
        text=message.text,
        mode=message.mode.value,
        reply_to=message.reply_to,
        sent_ms=message.sent_ms,
        session_id=message.session_id,
    )


def _fold_roster(roster: list[AgentRef]) -> list[PeerView]:
    return [
        PeerView(
            id=peer.id,
            name=peer.name,
            kind=peer.kind.value,
            status=peer.status.value,
            agent=peer.agent,
            parent=peer.parent,
            depth=peer.depth,
            activity=peer.activity,
            last_activity_ms=peer.last_activity_ms,
            output_url=str(peer.output_url),
            transcript_url=str(peer.transcript_url),
        )
        for peer in roster
    ]


async def _send_many(
    recipients: list[str],
    text: str,
    *,
    mode: DeliveryMode,
    reply_to: str | None,
) -> list[DeliveryView]:
    if not recipients:
        raise ValueError("at least one recipient is required")
    if not text.strip():
        raise ValueError("message text must not be empty")

    receipts = await asyncio.gather(
        *(
            send(to, text, mode=mode, reply_to=reply_to)
            for to in recipients
        )
    )
    return [
        DeliveryView(to=to, receipt=receipt.value)
        for to, receipt in zip(recipients, receipts, strict=True)
    ]


@service("examples.intercom.notify", rev=1)
class IntercomService:
    """Manifest-gated notification service for sibling extensions."""

    async def notify(self, request: NotifyRequest) -> NotifyResult:
        """Fan a notification out through the core-owned agent broker."""

        return NotifyResult(
            await _send_many(
                request.recipients,
                request.text,
                mode=DeliveryMode(request.mode),
                reply_to=request.reply_to,
            )
        )


@device("intercom", family="intercom", rev=1, place="host")
async def intercom(args: IntercomArgs, ctx: Context) -> IntercomResult:
    """Send, drain, or liveness-wait for cross-session peer messages."""

    del ctx
    if args.op == "send":
        if args.text is None:
            raise ValueError("text is required for send")
        return IntercomResult(
            receipts=await _send_many(
                args.to,
                args.text,
                mode=DeliveryMode(args.mode),
                reply_to=args.reply_to,
            )
        )

    if args.op == "inbox":
        messages = await inbox(peek=args.peek, limit=args.limit)
        return IntercomResult(messages=[_message_view(message) for message in messages])

    message = await wait_for(
        sender=args.sender,
        reply_to=args.reply_to,
        timeout=Duration(args.timeout),
    )
    return IntercomResult(messages=[] if message is None else [_message_view(message)])


@device("peers", family="intercom", rev=1, place="host")
async def peers(args: PeersArgs, ctx: Context) -> PeersResult:
    """List the core-owned peer roster without extension-local discovery."""

    del ctx
    return PeersResult(_fold_roster(await list_peers(scope=args.scope)))
