from __future__ import annotations

from dataclasses import dataclass
from hashlib import blake2s
from shlex import quote

import omp
from omp import ui

_CONTEXT_LINES = 3
_DIFF_PREVIEW_BYTES = 200_000
_STATUS_KEY = "dirty-files"


@dataclass(frozen=True, slots=True)
class _Change:
    status: str
    path: str
    original_path: str | None = None

    @property
    def label(self) -> str:
        """Return a readable label without losing spaces or rename direction."""
        if self.original_path is None:
            return f"{self.status}  {self.path}"
        return f"{self.status}  {self.original_path} → {self.path}"


class _GitError(RuntimeError):
    pass


_last_footer_hash: bytes | None = None


def _decode_path(path: bytes) -> str:
    """Decode a Git path for terminal display and safe shell quoting."""
    return path.decode("utf-8", errors="replace")


def _parse_porcelain(output: bytes) -> tuple[_Change, ...]:
    """Parse porcelain-v1 -z records, including the extra rename/copy path."""
    fields = output.split(b"\0")
    changes: list[_Change] = []
    index = 0
    while index < len(fields):
        record = fields[index]
        index += 1
        if not record:
            continue
        if len(record) < 4 or record[2:3] != b" ":
            raise ValueError("malformed git status --porcelain -z record")

        status = record[:2].decode("ascii", errors="replace")
        path = _decode_path(record[3:])
        original_path: str | None = None
        if "R" in status or "C" in status:
            if index >= len(fields) or not fields[index]:
                raise ValueError("rename/copy record is missing its original path")
            original_path = _decode_path(fields[index])
            index += 1
        changes.append(_Change(status, path, original_path))
    return tuple(changes)


async def _working_tree() -> tuple[_Change, ...]:
    """Read one explicit snapshot of the working tree from the Environment."""
    done = await omp.env.sh.run(
        "git status --porcelain=v1 -z --untracked-files=all",
        cwd=omp.env.info().root,
        timeout=omp.Duration("15s"),
    )
    if done.exit_code != 0:
        detail = done.text().strip() or "git status failed"
        raise _GitError(detail)
    try:
        return _parse_porcelain(done.output)
    except ValueError as error:
        raise _GitError(str(error)) from error


def _paint_footer(count: int | None) -> None:
    """Coalesce the keyed footer contribution by its rendered state hash."""
    global _last_footer_hash

    state = b"unavailable" if count is None else str(count).encode("ascii")
    state_hash = blake2s(state, digest_size=16).digest()
    if state_hash == _last_footer_hash:
        return
    if count is None:
        ui.set_status(_STATUS_KEY, None)
    else:
        ui.set_status(
            _STATUS_KEY,
            ui.tml(
                "<segment fg={tone}>{icon}{count}</segment>",
                tone=ui.Token.MUTED if count == 0 else ui.Token.WARN,
                icon=ui.icon("branch"),
                count=ui.text(f"{count} changed"),
            ),
            order=60,
            side=ui.Slot.STATUS_RIGHT,
        )
    _last_footer_hash = state_hash


async def _refresh() -> tuple[_Change, ...]:
    """Refresh the snapshot and its footer only when explicitly invoked."""
    try:
        changes = await _working_tree()
    except _GitError:
        _paint_footer(None)
        raise
    _paint_footer(len(changes))
    return changes


async def _diff(change: _Change, context: int = _CONTEXT_LINES) -> tuple[str, bool]:
    """Collect a bounded unified diff for one safely quoted porcelain path."""
    path = quote(change.path)
    pathspec = " ".join(
        quote(item)
        for item in (
            (change.path,)
            if change.original_path is None
            else (change.original_path, change.path)
        )
    )
    if change.status == "??":
        script = (
            f"git diff --no-index --no-ext-diff --unified={context} -- /dev/null {path} "
            "|| test $? -eq 1"
        )
    else:
        script = (
            "base=HEAD; git rev-parse --verify HEAD >/dev/null 2>&1 || "
            "base=$(git hash-object -t tree /dev/null); "
            f"git diff --no-ext-diff --unified={context} $base -- {pathspec}"
        )
    done = await omp.env.sh.run(
        script,
        cwd=omp.env.info().root,
        timeout=omp.Duration("30s"),
    )
    if done.exit_code != 0:
        raise _GitError(done.text().strip() or f"git diff failed for {change.path}")

    truncated = done.artifact is not None or len(done.output) > _DIFF_PREVIEW_BYTES
    if done.artifact is not None:
        output = await omp.env.blobs.get(done.artifact, length=_DIFF_PREVIEW_BYTES)
    else:
        output = done.output[:_DIFF_PREVIEW_BYTES]
    return output.decode("utf-8", errors="replace"), truncated


def _diff_tml(body: str, *, context: int = _CONTEXT_LINES) -> ui.Tml:
    """Render a newline-preserving unified body through frozen diff markup."""
    if not 0 <= context <= 65_535:
        raise ValueError("diff context must fit an unsigned 16-bit value")
    cleaned = "".join(
        character
        for character in body
        if character in {"\n", "\t"} or ord(character) >= 32
    )
    escaped = cleaned.replace("\\", "\\\\").replace("<", "\\<")
    return ui.Tml.raw(f"<diff id=preview context={context}>{escaped}</diff>")


def _overlay_tml(
    changes: tuple[_Change, ...], selected: int | None, body: str, truncated: bool
) -> ui.Tml:
    """Build the retained two-pane working-tree overlay."""
    options = ui.join(
        (
            ui.tml(
                "<option value={value}>{label}</option>",
                value=f"f{index}",
                label=ui.text(change.label),
            )
            for index, change in enumerate(changes)
        )
    )
    if changes:
        selected_value = f"f{selected if selected is not None else 0}"
        picker = ui.tml(
            "<select id=files filter h=22 value={selected}>{options}</select>",
            selected=selected_value,
            options=options,
        )
    else:
        picker = ui.tml("<text fg=muted>Working tree clean.</text>")

    note = (
        ui.tml("<text fg=warn>Preview truncated; full output is retained as an Environment blob.</text>")
        if truncated
        else ui.tml("")
    )
    empty = (
        ui.tml("<text fg=muted>No textual diff for this entry.</text>")
        if changes and not body
        else ui.tml("")
    )
    return ui.tml(
        "<box title='Working tree changes' border=round pad=1>"
        "<col gap=1>"
        "<row gap=2>"
        "<box title='Files' border=round w=36>{picker}</box>"
        "<box title='Unified diff' border=round grow>"
        "<col>{note}{empty}<scroll h=22>{diff}</scroll></col>"
        "</box>"
        "</row>"
        "<row justify=end gap=1>"
        "<button id=refresh label=Refresh/><button id=close label=Close cancel/>"
        "</row>"
        "</col>"
        "</box>",
        picker=picker,
        note=note,
        empty=empty,
        diff=_diff_tml(body),
    )


def _selected_index(value: str | None, size: int) -> int | None:
    """Decode an opaque picker value without trusting it as a path."""
    if value is None or not value.startswith("f"):
        return None
    try:
        index = int(value[1:])
    except ValueError:
        return None
    return index if 0 <= index < size else None


async def _preview(change: _Change) -> tuple[str, bool]:
    """Turn diff failures into a visible, non-fabricated preview body."""
    try:
        return await _diff(change)
    except _GitError as error:
        return f"diff unavailable: {error}", False


async def _show_changes(ctx: omp.Context) -> ui.Consumed | None:
    """Own the interactive overlay and respond only to watched interactions."""
    try:
        changes = await _refresh()
    except _GitError as error:
        return ui.Consumed(ui.tml("<callout fg=err>{message}</callout>", message=str(error)))

    if not ctx.has_ui:
        return ui.Consumed(ui.text(f"{len(changes)} changed files; interactive UI unavailable."))

    selected = 0 if changes else None
    body, truncated = await _preview(changes[0]) if changes else ("", False)
    async with await ui.overlay(
        _overlay_tml(changes, selected, body, truncated),
        ui.OverlayOptions(width=ui.Pct(92), max_height=ui.Pct(88)),
        watch=("files", "refresh"),
    ) as overlay:
        async for event in overlay.events():
            if event.kind in {ui.EventKind.HIGHLIGHTED, ui.EventKind.CHANGED} and event.id == "files":
                next_selected = _selected_index(event.value, len(changes))
                if next_selected is None or next_selected == selected:
                    continue
                selected = next_selected
                body, truncated = await _preview(changes[selected])
                overlay.set(_overlay_tml(changes, selected, body, truncated))
            elif event.kind is ui.EventKind.PRESSED and event.id == "refresh":
                previous = changes[selected].path if selected is not None and changes else None
                try:
                    changes = await _refresh()
                except _GitError as error:
                    overlay.set(
                        ui.tml("<callout fg=err>{message}</callout>", message=str(error))
                    )
                    continue
                selected = next(
                    (index for index, change in enumerate(changes) if change.path == previous),
                    0 if changes else None,
                )
                body, truncated = (
                    await _preview(changes[selected]) if selected is not None else ("", False)
                )
                overlay.set(_overlay_tml(changes, selected, body, truncated))
            elif event.kind in {ui.EventKind.CANCEL, ui.EventKind.SUBMIT}:
                break
    return None


@omp.hook("extension_activate")
async def seed_footer(payload: object, ctx: omp.Context) -> None:
    """Seed the dirty-file segment once when the extension activates."""
    del payload, ctx
    try:
        await _refresh()
    except _GitError:
        pass


@omp.shortcut(
    "alt+shift+g",
    action_id="show-git-changes",
    description="Open working-tree changes",
)
async def show_git_changes(action: ui.Action, ctx: omp.Context) -> None:
    """Open the working-tree overlay from its declared shortcut."""
    del action
    result = await _show_changes(ctx)
    if result is not None and result.notice is not None:
        ui.notify(result.notice, level=ui.Level.ERROR)


@omp.command("changes", description="Browse changed files and unified diffs")
async def changes_command(inv: ui.Invocation, ctx: omp.Context) -> ui.Consumed | None:
    """Open the working-tree overlay from `/changes`."""
    del inv
    return await _show_changes(ctx)
