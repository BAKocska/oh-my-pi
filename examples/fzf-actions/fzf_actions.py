"""Native candidate pickers backed by Environment commands and typed UI actions."""

from __future__ import annotations

from collections.abc import Mapping, Sequence

import omp
from omp import ui

_MAX_CANDIDATE_BYTES = 131_072
_MAX_CANDIDATES = 500
_ACTIONS = frozenset(
    ("insert-into-composer", "run-command", "open-overlay-preview")
)


def _notice(message: str) -> ui.Consumed:
    """Return a durable command notice."""

    return ui.Consumed(ui.text(message))


def _picker(settings: Mapping[str, object], name: str) -> Mapping[str, object] | None:
    """Resolve one named picker from extension settings."""

    pickers = settings.get("pickers")
    if not isinstance(pickers, Mapping):
        return None
    value = pickers.get(name)
    return value if isinstance(value, Mapping) else None


def _candidate_items(output: bytes) -> tuple[ui.SelectItem, ...]:
    """Decode a bounded tabular candidate stream into aligned select rows."""

    text = output[:_MAX_CANDIDATE_BYTES].decode("utf-8", errors="replace")
    lines = (line.rstrip("\r") for line in text.splitlines())
    return tuple(
        ui.SelectItem(value=line, cells=tuple(line.split("\t")))
        for line in lines
        if line
    )[:_MAX_CANDIDATES]


def _action_argv(config: Mapping[str, object], selection: str) -> tuple[str, ...]:
    """Substitute a selection into configured argv values without making shell text."""

    command = config.get("command")
    if (
        not isinstance(command, Sequence)
        or isinstance(command, (str, bytes, bytearray))
        or not command
        or not all(isinstance(part, str) and part for part in command)
    ):
        raise ValueError("run-command requires a non-empty string argv in 'command'")
    if not any("{}" in part for part in command):
        raise ValueError("run-command argv must contain a {} placeholder")
    argv = tuple(part.replace("{}", selection) for part in command)
    if any("\0" in part for part in argv):
        raise ValueError("command arguments may not contain NUL")
    return argv


async def _run_argv(argv: tuple[str, ...]) -> omp.env.Completed:
    """Run argv through the Environment without interpolating values into shell source."""

    names = tuple(f"OMP_FZF_ARG_{index}" for index in range(len(argv)))
    script = "exec " + " ".join(f'"${name}"' for name in names)
    environment = dict(zip(names, argv, strict=True))
    return await omp.env.sh.run(
        script,
        env=environment,
        timeout=omp.Duration("2m"),
    )


async def _preview(selection: str) -> None:
    """Show the selected candidate in a modal retained overlay."""

    content = ui.tml(
        "<panel title=\"Selection preview\"><md>{selection}</md>"
        "<button cancel>Close</button></panel>",
        selection=selection,
    )
    async with await ui.overlay(content) as overlay:
        await overlay.wait()


@ui.command(
    "pick",
    description="Choose a candidate from a configured picker",
    args=(ui.Arg("name", "Configured picker name", usage="<name>"),),
    hint="<name>",
)
async def pick(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed | None:
    """Run a named candidate source, select one row, and apply its declared action."""

    if len(inv.argv) != 1:
        return _notice("Usage: /pick <name>")
    name = inv.argv[0]
    config = _picker(ctx.settings, name)
    if config is None:
        return _notice(f"Unknown picker: {name}")

    candidates_cmd = config.get("candidates_cmd")
    action = config.get("action")
    if not isinstance(candidates_cmd, str) or not candidates_cmd.strip():
        return _notice(f"Picker {name!r} has no candidates command")
    if action not in _ACTIONS:
        return _notice(f"Picker {name!r} has an invalid action")

    completed = await omp.env.sh.run(
        candidates_cmd,
        timeout=omp.Duration("30s"),
    )
    if completed.outcome is not omp.env.Outcome.EXITED or completed.exit_code != 0:
        return _notice(f"Candidate command failed for picker: {name}")

    items = _candidate_items(completed.output)
    if not items:
        return _notice(f"Picker {name!r} returned no candidates")
    selected = await ui.select(
        f"Pick: {name}",
        items,
        options=ui.DialogOptions(
            help="Type to filter · Enter to choose · Esc to cancel"
        ),
    )
    if selected.cancelled or selected.value is None:
        return None

    if action == "insert-into-composer":
        ui.paste_to_editor(selected.value)
        return None
    if action == "open-overlay-preview":
        await _preview(selected.value)
        return None

    try:
        argv = _action_argv(config, selected.value)
    except ValueError as error:
        return _notice(f"Picker {name!r}: {error}")
    result = await _run_argv(argv)
    if result.outcome is not omp.env.Outcome.EXITED or result.exit_code != 0:
        return _notice(f"Action command failed for picker: {name}")
    return None
