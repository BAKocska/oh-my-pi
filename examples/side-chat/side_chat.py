from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import ui


_SESSION = omp.StateScope.SESSION
_NEW_THREAD = "__new__"
_TAIL_LINES = 18


@omp.entry_kind("examples.side-chat.threads", rev="v.1", display=False)
@dataclass(frozen=True, slots=True)
class SideThreadIds:
    """Persist only stable child-session ids for the current main session."""

    ids: tuple[str, ...]


def _tail(text: str, lines: int = _TAIL_LINES) -> str:
    end = len(text)
    while end and text[end - 1] in "\r\n":
        end -= 1
    start = end
    for _ in range(lines):
        newline = text.rfind("\n", 0, start)
        if newline < 0:
            start = 0
            break
        start = newline
    return text[start + 1 : end] if start else text[:end]


async def _thread_ids() -> tuple[str, ...]:
    record = await omp.state.latest(SideThreadIds, scope=_SESSION)
    value = getattr(record, "value", record)
    if not isinstance(value, SideThreadIds):
        return ()
    return tuple(thread_id for thread_id in value.ids if isinstance(thread_id, str))


async def _remember(thread_id: str) -> tuple[str, ...]:
    ids = await _thread_ids()
    if thread_id in ids:
        return ids
    updated = (*ids, thread_id)
    await omp.state.append(SideThreadIds(updated), scope=_SESSION)
    return updated


async def _spawn(question: str) -> tuple[str, tuple[str, ...]]:
    handle = await omp.agents.spawn(
        omp.agents.SubagentSpec(
            task=(
                "You are a side conversation beside the user's main session. "
                "Answer the question directly, keep this journal available for follow-ups, "
                "and do not modify the workspace unless the user explicitly asks.\n\n"
                f"Question: {question}"
            ),
            background=True,
            max_depth=1,
            labels={"purpose": "side-chat"},
        )
    )
    return handle.session_id, await _remember(handle.session_id)


async def _transcript_tail(thread_id: str) -> str:
    try:
        handle = await omp.agents.get(thread_id)
        transcript = await handle.transcript_url.with_selector("raw").read()
    except omp.agents.AgentGone as gone:
        transcript = await omp.HistoryUrl(str(gone.transcript_url)).with_selector("raw").read()
    return _tail(str(transcript)) or "The side thread has not written a message yet."


def _overlay_content(
    ids: tuple[str, ...], selected: str, transcript: str, status: str = ""
) -> ui.Tml:
    options = [
        ui.tml(
            '<option value="{value}">{label}</option>',
            value=_NEW_THREAD,
            label=ui.text("New side thread"),
        )
    ]
    options.extend(
        ui.tml(
            '<option value="{value}">{label}</option>',
            value=thread_id,
            label=ui.text(thread_id),
        )
        for thread_id in ids
    )
    return ui.tml(
        '<box title="Side chat" border="rounded"><col gap=1>'
        '<select id="thread" label="Thread" value="{selected}" h=6>{options}</select>'
        '<scroll h=12><text wrap>{transcript}</text></scroll>'
        '<input id="message" placeholder="Ask or follow up; leave empty to refresh"/>'
        '<text fg="muted">{status}</text>'
        '<row gap=1><button submit accent>Send</button><button cancel>Close</button></row>'
        '</col></box>',
        selected=selected,
        options=ui.join(options),
        transcript=ui.text(transcript),
        status=ui.text(status),
    )


async def _open_overlay(ids: tuple[str, ...], selected: str) -> None:
    transcript = (
        await _transcript_tail(selected)
        if selected != _NEW_THREAD
        else "Enter a question to start an independent side transcript."
    )
    status = ""
    while True:
        content = _overlay_content(ids, selected, transcript, status)
        async with await ui.overlay(
            content,
            ui.OverlayOptions(width=ui.Pct(70), max_height=ui.Pct(85)),
        ) as overlay:
            outcome = await overlay.wait()
            if outcome.cancelled:
                return
            values = await overlay.values()

        candidate = values.get("thread", selected)
        selected = candidate if isinstance(candidate, str) and candidate else selected
        message_value = values.get("message", "")
        message = message_value.strip() if isinstance(message_value, str) else ""

        if selected == _NEW_THREAD:
            if not message:
                transcript = "Enter a question to start an independent side transcript."
                status = "A new side thread needs a question."
                continue
            selected, ids = await _spawn(message)
            transcript = await _transcript_tail(selected)
            status = f"Started {selected}; its settlement will arrive at a turn boundary."
            continue

        if message:
            receipt = await omp.agents.send(selected, message)
            status = f"Follow-up {receipt.value} to {selected}."
        else:
            status = f"Refreshed {selected}."
        transcript = await _transcript_tail(selected)


@omp.command(
    "btw",
    description="Open parallel side conversations without changing the main transcript",
    args=(ui.Arg("question", "Start a new side thread", usage="[question ...]"),),
    hint="[question ...]",
)
async def btw(invocation: ui.Invocation, ctx: omp.Context) -> ui.Consumed:
    """Open the side-chat modal, optionally starting it with one question."""

    if not ctx.has_ui:
        return ui.Consumed(ui.text("/btw requires an attached interactive UI."))

    ids = await _thread_ids()
    selected = ids[-1] if ids else _NEW_THREAD
    question = " ".join(invocation.argv).strip()
    if question:
        selected, ids = await _spawn(question)
    await _open_overlay(ids, selected)
    return ui.Consumed()
