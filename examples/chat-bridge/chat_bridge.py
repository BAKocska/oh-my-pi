from __future__ import annotations

import json
from dataclasses import dataclass
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit

import omp


_MAX_SUMMARY_BYTES = 1_024
_MAX_REPLY_BYTES = 4_096
_MAX_RESPONSE_BYTES = 64 * 1_024
_MAX_REPLIES_PER_POLL = 16
_PROVIDER = "chat-bridge"
_SCOPE = omp.StateScope.SESSION
_NO_REPLY = "<chat-bridge:no-reply>"


@omp.entry_kind("examples.chat_bridge.reply_seen", rev="v.1")
@dataclass(frozen=True, slots=True)
class ReplySeen:
    """Record one webhook reply id already admitted into this session."""

    reply_id: str


@dataclass(frozen=True, slots=True)
class Reply:
    """Carry one validated webhook reply into prompt construction."""

    id: str
    text: str


def _clip_utf8(text: str, limit: int) -> str:
    raw = text.encode("utf-8")
    if len(raw) <= limit:
        return text
    if limit <= 3:
        return "." * limit
    return raw[: limit - 3].decode("utf-8", errors="ignore") + "..."


def _settled_summary(event: omp.AgentEndEvent, session_id: str) -> str:
    summary = event.summary
    fields = (
        f"session={_clip_utf8(session_id, 128)}",
        f"submission={_clip_utf8(event.submission_id, 128)}",
        f"turns={summary.committed_turns}",
        f"stop={summary.stop.value if summary.stop is not None else 'none'}",
        f"interrupted={str(summary.interrupted).lower()}",
        f"continued={str(event.continued).lower()}",
    )
    if event.error:
        fields += (f"error={_clip_utf8(event.error, 384)}",)
    return _clip_utf8("agent settled: " + "; ".join(fields), _MAX_SUMMARY_BYTES)


def _with_query(url: str, name: str, value: str) -> str:
    parts = urlsplit(url)
    query = parse_qsl(parts.query, keep_blank_values=True)
    query.append((name, value))
    return urlunsplit((parts.scheme, parts.netloc, parts.path, urlencode(query), parts.fragment))


def _parse_replies(body: bytes) -> tuple[Reply, ...]:
    if len(body) > _MAX_RESPONSE_BYTES:
        raise ValueError("reply response exceeds 64 KiB")
    payload = json.loads(body)
    rows = payload.get("replies", ()) if isinstance(payload, dict) else payload
    if not isinstance(rows, list):
        raise ValueError("reply response must be a list or an object containing 'replies'")

    replies: list[Reply] = []
    for row in rows:
        if not isinstance(row, dict) or "id" not in row or "text" not in row:
            continue
        reply_id = _clip_utf8(str(row["id"]).strip(), 128)
        text = _clip_utf8(str(row["text"]).strip(), _MAX_REPLY_BYTES)
        if reply_id and text:
            replies.append(Reply(reply_id, text))
        if len(replies) == _MAX_REPLIES_PER_POLL:
            break
    return tuple(replies)


def _unseen(replies: tuple[Reply, ...], seen: set[str]) -> tuple[Reply, ...]:
    selected: list[Reply] = []
    for reply in replies:
        if reply.id in seen:
            continue
        seen.add(reply.id)
        selected.append(reply)
    return tuple(selected)


def _reply_prompt(replies: tuple[Reply, ...]) -> str:
    joined = "\n\n".join(
        f"[Webhook reply {reply.id}]\n{reply.text}" for reply in replies
    )
    return _clip_utf8(joined, _MAX_REPLY_BYTES)


async def _headers() -> dict[str, str]:
    token = await omp.creds.mint_scoped(
        "webhook", ttl=omp.Duration("2m"), provider=_PROVIDER
    )
    return {"authorization": f"Bearer {token.token}", "accept": "application/json"}


async def _seen_reply_ids() -> set[str]:
    records = await omp.state.entries(ReplySeen, scope=_SCOPE)
    return {record.value.reply_id for record in records}


async def _fetch_unseen(url: str) -> tuple[Reply, ...]:
    response = await omp.env.http_get(
        url,
        timeout=omp.Duration("10s"),
        headers=await _headers(),
    )
    if response.status < 200 or response.status >= 300:
        raise ValueError(f"reply endpoint returned HTTP {response.status}")
    return _unseen(_parse_replies(response.body), await _seen_reply_ids())


async def _mark_seen(replies: tuple[Reply, ...]) -> None:
    for reply in replies:
        await omp.state.append(
            ReplySeen(reply.id),
            scope=_SCOPE,
            idempotency_key=f"chat-bridge-reply:{reply.id}",
        )


@omp.hook("session_start", phase=omp.HookPhase.OBSERVE)
async def register_reply_poll(
    event: omp.SessionStartEvent, ctx: omp.Context
) -> None:
    """Upsert the session's durable webhook reply poll schedule."""

    del event
    interval = omp.Duration(str(ctx.settings.get("poll_interval", "60s")))
    await omp.agents.schedule(
        "chat_bridge_poll",
        omp.agents.Every(interval=interval, jitter=omp.Duration("5s")),
        omp.agents.Inject(mode=omp.agents.DeliveryMode.NEXT_TURN, visible=True),
        scope=omp.agents.ScheduleScope.SESSION,
        missed=omp.agents.MissedRunPolicy.COALESCE,
        overlap="skip",
    )


@omp.hook("before_agent_start", phase=omp.HookPhase.TRANSFORM)
async def receive_scheduled_replies(
    event: omp.BeforeAgentStartEvent, ctx: omp.Context
) -> omp.Modify | None:
    """Replace a scheduled injection with the next deduplicated webhook reply batch."""

    if event.source is not omp.InputSource.SCHEDULE:
        return None
    replies_url = str(ctx.settings.get("replies_url", "")).strip()
    if not replies_url:
        return omp.Modify(patch={"text": _NO_REPLY})
    replies = await _fetch_unseen(replies_url)
    if not replies:
        return omp.Modify(patch={"text": _NO_REPLY})
    await _mark_seen(replies)
    return omp.Modify(patch={"text": _reply_prompt(replies)})

@omp.hook("before_agent_start", phase=omp.HookPhase.REVIEW)
async def suppress_empty_poll(
    event: omp.BeforeAgentStartEvent, ctx: omp.Context
) -> omp.Deny | None:
    """Suppress a scheduled poll that found no reply to inject."""

    del ctx
    if event.source is omp.InputSource.SCHEDULE and event.text == _NO_REPLY:
        return omp.Deny("webhook poll found no new replies", code="chat_bridge_empty")
    return None


@omp.hook("agent_end", phase=omp.HookPhase.OBSERVE)
async def notify_agent_settled(event: omp.AgentEndEvent, ctx: omp.Context) -> None:
    """Send one bounded completion summary to the configured webhook endpoint."""

    webhook_url = str(ctx.settings.get("webhook_url", "")).strip()
    if not webhook_url:
        return
    summary = _settled_summary(event, ctx.session)
    response = await omp.env.http_get(
        _with_query(webhook_url, "summary", summary),
        timeout=omp.Duration("10s"),
        headers=await _headers(),
    )
    if response.status < 200 or response.status >= 300:
        raise ValueError(f"webhook endpoint returned HTTP {response.status}")
