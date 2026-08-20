"""Generate one concise title after the session's first settled turn."""

from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import agents
from omp import context
from omp import ui

_MAX_SOURCE_CHARS = 4_000
_MAX_TITLE_CHARS = 72
_SCOPE = omp.StateScope.SESSION


@omp.entry_kind("examples.session-titler.attempt", rev="v.1", display=False)
@dataclass(frozen=True, slots=True)
class TitleAttempt:
    """Record the session's sole title-generation attempt and its outcome."""

    outcome: str
    title: str = ""
    failure: str = ""


async def _attempted() -> bool:
    """Return whether this session already consumed its one title attempt."""

    return await omp.state.latest(TitleAttempt, scope=_SCOPE) is not None


def _first_user_prompt(view: context.ContextView) -> str:
    """Select a bounded preview of the first projected user message."""

    for message in view.messages:
        if message.kind is context.MessageKind.USER:
            return message.preview[:_MAX_SOURCE_CHARS]
    return ""


def _clean_title(text: str) -> str:
    """Collapse model formatting and bound the title before the UI sanitizes it."""

    title = " ".join(text.split()).strip(" `\"'")
    if len(title) <= _MAX_TITLE_CHARS:
        return title
    return title[: _MAX_TITLE_CHARS - 1].rstrip() + "…"

async def _record(outcome: str, *, title: str = "", failure: str = "") -> None:
    """Append the durable result of the session's one title attempt."""

    attempt = TitleAttempt(outcome=outcome, title=title, failure=failure)
    await omp.state.append(
        attempt,
        scope=_SCOPE,
        idempotency_key=f"session-titler:{outcome}",
    )


@omp.hook("turn_end", phase=omp.HookPhase.OBSERVE, timeout=omp.Duration("6s"))
async def title_first_turn(event: omp.TurnEndEvent, ctx: omp.Context) -> None:
    """Name the session after its first non-tool-use turn settles."""

    del ctx
    if event.stop is omp.StopReason.TOOL_USE or await _attempted():
        return

    view = await context.view()
    source = _first_user_prompt(view)
    if not source:
        await _record("failed", failure="first user message is unavailable")
        return

    # Claim the once-per-session attempt before inference. A host restart or timeout
    # must not spend on a second title request.
    await _record("started")
    try:
        async with context.lane():
            result = await agents.completion(
                {"first_message": source},
                role="smol",
                system=(
                    "Write a specific session title of at most 72 characters. "
                    "Return only the title, without quotes, Markdown, or a trailing period."
                ),
                default="",
                scope="session",
                max_output_tokens=24,
                deadline=omp.Duration("4s"),
                labels={"feature": "session-title"},
            )
    except Exception as error:
        await _record("failed", failure=f"{type(error).__name__}: {error}")
        return

    if result.fell_back:
        await _record("failed", failure=str(result.fault or "completion failed"))
        return

    title = _clean_title(result.text)
    if not title:
        await _record("failed", failure="completion returned an empty title")
        return

    ui.set_title(title)
    await _record("titled", title=title)
