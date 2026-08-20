from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Literal
from urllib.parse import urlencode

import omp


RemoteState = Literal["missing", "pending", "allow", "deny"]
UnreachableMode = Literal["ask", "deny", "allow"]


@dataclass(frozen=True, slots=True)
class RemoteReply:
    """Represent the remote service's current answer for one ticket."""

    state: RemoteState
    reason: str | None = None


class RemoteUnavailable(RuntimeError):
    """Report that the configured approval service could not answer."""


def _webhook(ctx: omp.Context) -> str:
    value = str(ctx.settings.get("webhook", "")).strip()
    if not value.startswith(("http://", "https://")):
        raise RemoteUnavailable("settings.webhook must be an absolute HTTP URL")
    return value


def _mode(ctx: omp.Context) -> UnreachableMode:
    value = str(ctx.settings.get("unreachable", "ask"))
    return value if value in {"ask", "deny", "allow"} else "deny"


def _query_url(ctx: omp.Context, action: str, **values: str) -> str:
    base = _webhook(ctx)
    separator = "&" if "?" in base else "?"
    return f"{base}{separator}{urlencode({'action': action, **values})}"


async def _request(url: str) -> omp.env.HttpResponse:
    return await omp.env.http_get(url, timeout=omp.Duration("10s"))


def _decode(response: omp.env.HttpResponse) -> RemoteReply:
    if response.status == 404:
        return RemoteReply("missing")
    if response.status < 200 or response.status >= 300:
        raise RemoteUnavailable(f"approval service returned HTTP {response.status}")
    try:
        body = response.json()
        state = body["state"]
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        raise RemoteUnavailable("approval service returned an invalid response") from error
    if state not in {"missing", "pending", "allow", "deny"}:
        raise RemoteUnavailable(f"approval service returned unknown state {state!r}")
    reason = body.get("reason")
    return RemoteReply(state, None if reason is None else str(reason))


def _ticket_payload(ticket: omp.ApprovalTicket) -> str:
    return json.dumps(
        {
            "ticket_id": ticket.ticket_id,
            "invocation_id": ticket.invocation_id,
            "created_at": ticket.created_at,
            "reasons": [
                {
                    "title": reason.title,
                    "body": reason.body,
                    "subject": reason.subject,
                    "kind": reason.kind.value,
                    "scopes": [scope.value for scope in reason.scopes],
                    "evidence": list(reason.evidence),
                }
                for reason in ticket.reasons
            ],
        },
        separators=(",", ":"),
        sort_keys=True,
    )


async def remote_status(ticket_id: str, ctx: omp.Context) -> RemoteReply:
    """Read the remote service's durable state for one Core ticket."""

    try:
        response = await _request(
            _query_url(ctx, "status", ticket_id=ticket_id)
        )
    except Exception as error:
        if isinstance(error, RemoteUnavailable):
            raise
        raise RemoteUnavailable("approval service is unreachable") from error
    return _decode(response)


async def forward_once(
    ticket: omp.ApprovalTicket, ctx: omp.Context
) -> RemoteReply:
    """Offer a ticket only when its remote idempotency key is absent."""

    status = await remote_status(ticket.ticket_id, ctx)
    if status.state != "missing":
        return status
    try:
        response = await _request(
            _query_url(
                ctx,
                "offer",
                ticket_id=ticket.ticket_id,
                payload=_ticket_payload(ticket),
            )
        )
    except Exception as error:
        if isinstance(error, RemoteUnavailable):
            raise
        raise RemoteUnavailable("approval service is unreachable") from error
    offered = _decode(response)
    if offered.state == "missing":
        raise RemoteUnavailable("approval service did not retain the offered ticket")
    return offered


def remote_decision(
    ticket: omp.ApprovalTicket, reply: RemoteReply
) -> omp.ApprovalDecision | None:
    """Translate a completed remote answer without inventing a pending answer."""

    if reply.state not in {"allow", "deny"}:
        return None
    return omp.ApprovalDecision(
        approved=reply.state == "allow",
        scope=omp.PolicyScope.ONCE,
        source=omp.ApprovalSource.EXTERNAL,
        decided_by="remote-approve",
        reason=reply.reason,
        audited=False,
    )


def unreachable_decision(
    ticket: omp.ApprovalTicket,
    mode: UnreachableMode,
    *,
    allow_fail_open: bool,
) -> omp.ApprovalDecision | None:
    """Map an unavailable service to local fallback, denial, or gated read-only allow."""

    if mode == "ask":
        return None
    approved = mode == "allow" and allow_fail_open and all(
        reason.kind is omp.ApprovalKind.READ for reason in ticket.reasons
    )
    if mode == "allow" and not approved:
        reason = "fail-open requires allow_fail_open=true and read-only reasons"
    else:
        reason = "remote approval service is unreachable"
    return omp.ApprovalDecision(
        approved=approved,
        scope=omp.PolicyScope.ONCE,
        source=omp.ApprovalSource.UNAVAILABLE,
        decided_by="remote-approve",
        reason=reason,
        audited=approved,
    )


async def _fallback(
    ticket: omp.ApprovalTicket, ctx: omp.Context
) -> omp.ApprovalDecision | None:
    return unreachable_decision(
        ticket,
        _mode(ctx),
        allow_fail_open=ctx.settings.get("allow_fail_open") is True,
    )


@omp.approver(
    "remote-approve",
    kinds=tuple(omp.ApprovalKind),
    timeout=omp.Duration("5m"),
    unreachable=omp.Unreachable.ESCALATE_LOCAL,
)
async def remote_approver(
    ticket: omp.ApprovalTicket, ctx: omp.Context
) -> omp.ApprovalDecision | None:
    """Idempotently offer a Core-owned ticket without creating or awaiting one."""

    try:
        reply = await forward_once(ticket, ctx)
    except RemoteUnavailable:
        return await _fallback(ticket, ctx)
    return remote_decision(ticket, reply)


async def reconcile_pending(ctx: omp.Context) -> None:
    """Poll remote answers and resolve only still-pending Core tickets."""

    for ticket in await omp.policy.pending():
        try:
            reply = await remote_status(ticket.ticket_id, ctx)
            decision = remote_decision(ticket, reply)
            if reply.state == "missing":
                decision = remote_decision(ticket, await forward_once(ticket, ctx))
        except RemoteUnavailable:
            decision = await _fallback(ticket, ctx)
        if decision is not None:
            await omp.policy.decide(ticket.ticket_id, decision)


@omp.hook("extension_activate", phase=omp.HookPhase.OBSERVE)
async def activate_remote_approval(
    event: omp.ExtensionActivateEvent, ctx: omp.Context
) -> None:
    """Upsert polling and reconcile tickets re-offered after activation."""

    del event
    interval = omp.Duration(str(ctx.settings.get("poll_interval", "30s")))
    await omp.agents.schedule(
        "remote-approve-poll",
        omp.agents.Every(interval),
        omp.agents.Inject(mode=omp.agents.DeliveryMode.NEXT_TURN, visible=False),
        scope=omp.agents.ScheduleScope.SESSION,
        missed=omp.agents.MissedRunPolicy.COALESCE,
        overlap="skip",
    )
    await reconcile_pending(ctx)


@omp.hook("turn_start", phase=omp.HookPhase.OBSERVE)
async def poll_remote_approval(
    event: omp.TurnStartEvent, ctx: omp.Context
) -> None:
    """Poll pending tickets when an invisible Every firing wakes the agent."""

    del event
    await reconcile_pending(ctx)
