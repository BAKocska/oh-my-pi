from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import ui


_SCOPE = omp.StateScope.USER


@omp.entry_kind("examples.prompt-manager.saved", rev="v.1", display=False)
@dataclass(frozen=True, slots=True)
class SavedPrompt:
    """Store one named reusable prompt in the user-scoped append log."""

    name: str
    text: str


def _keep_latest(
    saved: dict[str, SavedPrompt], record: object
) -> dict[str, SavedPrompt]:
    value = getattr(record, "value", None)
    if isinstance(value, SavedPrompt):
        saved[value.name] = value
    return saved


async def _saved_prompts() -> dict[str, SavedPrompt]:
    saved, _watermark = await omp.state.fold(
        SavedPrompt, _keep_latest, {}, scope=_SCOPE
    )
    return saved


async def _complete_saved_names(
    query: ui.ArgQuery, ctx: omp.Context
) -> tuple[ui.CompletionItem, ...]:
    del ctx
    if query.argv != ("use",):
        return ()
    prefix = query.prefix.casefold()
    saved = await _saved_prompts()
    return tuple(
        ui.CompletionItem(
            insert=item.name,
            label=item.name,
            desc=item.text.replace("\n", " ")[:72],
            group="Saved prompts",
        )
        for item in sorted(saved.values(), key=lambda item: item.name.casefold())
        if item.name.casefold().startswith(prefix)
    )


def _usage() -> ui.Consumed:
    return ui.Consumed(
        ui.text("Usage: /prompt save <name> <text> | list | use <name>")
    )


@ui.command(
    "prompt",
    description="Save, list, or insert a reusable prompt",
    args=(
        ui.Arg("save", "Save or replace a prompt", usage="<name> <text>"),
        ui.Arg("list", "List saved prompt names"),
        ui.Arg("use", "Place a saved prompt in the composer", usage="<name>"),
    ),
    hint="save | list | use",
    arg_completions=_complete_saved_names,
)
async def prompt(
    inv: ui.Invocation, ctx: omp.Context
) -> ui.Consumed | ui.Prompt:
    """Save, list, or place a reusable prompt in the composer without submitting."""

    del ctx
    if not inv.argv:
        return _usage()

    operation = inv.argv[0]
    if operation == "save":
        if len(inv.argv) < 3:
            return _usage()
        name = inv.argv[1].strip()
        text = " ".join(inv.argv[2:]).strip()
        if not name or not text:
            return _usage()
        await omp.state.append(SavedPrompt(name=name, text=text), scope=_SCOPE)
        return ui.Consumed(ui.text(f"Saved prompt: {name}"))
    if operation == "list":
        if len(inv.argv) != 1:
            return _usage()
        saved = await _saved_prompts()
        names = sorted(saved, key=str.casefold)
        notice = "Saved prompts: " + ", ".join(names) if names else "No saved prompts."
        return ui.Consumed(ui.text(notice))

    if operation == "use":
        if len(inv.argv) != 2:
            return _usage()
        saved = await _saved_prompts()
        item = saved.get(inv.argv[1])
        if item is None:
            return ui.Consumed(ui.text(f"Unknown saved prompt: {inv.argv[1]}"))
        return ui.Prompt(item.text, submit=False)

    return _usage()
