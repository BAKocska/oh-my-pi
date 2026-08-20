from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum

import omp


@dataclass(frozen=True, slots=True)
class _ReadUpdate:
    """One typed read progress update."""

    phase: str


@dataclass(frozen=True, slots=True)
class _ReadTextPart:
    """One typed UTF-8 read result part."""

    text: str


@dataclass(frozen=True, slots=True)
class _ReadBlobPart:
    """One typed binary read result part."""

    blob: omp.BlobRef
    alt: str


@dataclass(frozen=True, slots=True)
class _ReadPayload(omp.Payload):
    """The typed durable payload of read revision 1."""

    parts: tuple[_ReadTextPart | _ReadBlobPart, ...]


class _OutputChannel(StrEnum):
    """A typed bash output channel."""

    STDOUT = "stdout"
    STDERR = "stderr"
    PTY = "pty"


@dataclass(frozen=True, slots=True)
class _BashUpdate:
    """One typed ordered output update from bash revision 1."""

    channel: _OutputChannel
    data: bytes
    sequence: int


@dataclass(frozen=True, slots=True)
class _BashFrame:
    """One typed output frame retained in a bash verdict."""

    channel: _OutputChannel
    data: bytes
    sequence: int


@dataclass(frozen=True, slots=True)
class _BashStatus:
    """The typed terminal process status from bash revision 1."""

    outcome: str
    exit_code: int | None
    signal: str | None
    wall_clock_ms: int
    spilled_output: omp.BlobRef | None = None
    aborted: bool = False
    effects_unknown: bool = False


@dataclass(frozen=True, slots=True)
class _BashPayload(omp.Payload):
    """The typed durable payload of bash revision 1."""

    session_id: bytes
    exec_id: bytes
    command: str
    transcript: tuple[_BashFrame, ...]
    adjustments: tuple[object, ...]
    status: _BashStatus


@dataclass(frozen=True, slots=True)
class _BashFoldState:
    """Pre-computed byte totals for a bash output stream."""

    stdout: int = 0
    stderr: int = 0
    pty: int = 0

    @property
    def total(self) -> int:
        """Return the total retained output bytes."""

        return self.stdout + self.stderr + self.pty


@dataclass(frozen=True, slots=True)
class _GrepContext:
    """One typed context line adjacent to a grep match."""

    line_number: int
    line: str


@dataclass(frozen=True, slots=True)
class _GrepMatch:
    """One typed grep match retained for rendering."""

    line_number: int
    line: str
    truncated: bool = False
    context_before: tuple[_GrepContext, ...] = ()
    context_after: tuple[_GrepContext, ...] = ()


@dataclass(frozen=True, slots=True)
class _GrepFile:
    """One typed file group in a grep payload."""

    path: str
    source_key: str
    snapshot_tag: str | None
    matches: tuple[_GrepMatch, ...]


@dataclass(frozen=True, slots=True)
class _GrepPayload(omp.Payload):
    """The typed durable payload of grep revision 1."""

    files: tuple[_GrepFile, ...]
    total_files: int
    total_files_lower_bound: bool
    multi_scope: bool
    skip: int
    file_limit_reached: bool
    per_file_limit_reached: bool
    notes: tuple[str, ...]
    projected_text: str
    output_blob: omp.BlobRef | None
    output_shown_lines: int
    output_total_lines: int


_EMPTY_BASH_STATE = _BashFoldState()


def _byte_label(size: int) -> str:
    """Format a byte count without locale or external state."""

    if size < 1_024:
        return f"{size} B"
    if size < 1_048_576:
        return f"{size / 1_024:.1f} KiB"
    return f"{size / 1_048_576:.1f} MiB"


def _table(headers: tuple[str, ...], rows: tuple[tuple[str, ...], ...]) -> omp.ui.Tml:
    """Build a compact TML table from already-computed cells."""

    empty = omp.ui.Tml.raw("")
    head = omp.ui.join(
        (
            omp.ui.tml("<td><text fg=muted bold>{value}</text></td>", value=value)
            for value in headers
        ),
        sep=empty,
    )
    body = omp.ui.join(
        (
            omp.ui.tml(
                "<tr>{cells}</tr>",
                cells=omp.ui.join(
                    (
                        omp.ui.tml("<td truncate>{value}</td>", value=value)
                        for value in row
                    ),
                    sep=empty,
                ),
            )
            for row in rows
        ),
        sep=empty,
    )
    return omp.ui.tml("<table gap=2><tr>{head}</tr>{body}</table>", head=head, body=body)


def _read_part_bytes(part: _ReadTextPart | _ReadBlobPart) -> int:
    """Read a typed part's authoritative byte count."""

    match part:
        case _ReadTextPart(text=text):
            return len(text.encode("utf-8"))
        case _ReadBlobPart(blob=blob):
            return blob.size


def _read_rows(parts: tuple[_ReadTextPart | _ReadBlobPart, ...]) -> tuple[tuple[str, ...], ...]:
    """Pre-compute stable table cells for read result parts."""

    rows: list[tuple[str, ...]] = []
    for number, part in enumerate(parts, 1):
        match part:
            case _ReadTextPart(text=text):
                rows.append((str(number), "text", _byte_label(len(text.encode("utf-8"))), f"{text.count(chr(10)) + bool(text)} line(s)"))
            case _ReadBlobPart(blob=blob, alt=alt):
                rows.append((str(number), "blob", _byte_label(blob.size), alt))
    return tuple(rows)


@omp.renderer("read", family="", rev=1)
def render_read(
    view: omp.View[_ReadUpdate, _ReadPayload, omp.Fault], ctx: omp.ui.RenderCtx
) -> omp.ui.Tml | None:
    """Render read revision 1 from typed parts without parsing projected prose."""

    if view.verdict is None:
        phase = view.updates[-1].phase if view.updates else "reading"
        return omp.ui.tml("<row>{icon}<text> {phase}</text></row>", icon=omp.ui.icon("file"), phase=phase)
    match view.verdict:
        case omp.Ok(_ReadPayload(parts=parts)):
            total = sum(_read_part_bytes(part) for part in parts)
            header = omp.ui.tml(
                "<row>{icon}<text bold> read</text><spacer/><text fg=info>{count}</text></row>",
                icon=omp.ui.icon("file"),
                count=_byte_label(total),
            )
            if ctx.collapsed or not parts:
                return header
            return omp.ui.tml(
                "<col gap=1>{header}{table}</col>",
                header=header,
                table=_table(("#", "kind", "bytes", "detail"), _read_rows(parts)),
            )
        case _:
            return None


def _reduce_bash(state: object | None, update: object) -> _BashFoldState:
    """Fold bash output bytes once so every view remains constant-time."""

    current = state if isinstance(state, _BashFoldState) else _EMPTY_BASH_STATE
    if not isinstance(update, _BashUpdate):
        return current
    size = len(update.data)
    match update.channel:
        case _OutputChannel.STDOUT:
            return _BashFoldState(current.stdout + size, current.stderr, current.pty)
        case _OutputChannel.STDERR:
            return _BashFoldState(current.stdout, current.stderr + size, current.pty)
        case _OutputChannel.PTY:
            return _BashFoldState(current.stdout, current.stderr, current.pty + size)
        case _:
            return current


def _exit_badge(status: _BashStatus) -> omp.ui.Tml:
    """Build an exit badge from the typed terminal status."""

    if status.exit_code is not None:
        label = f"exit {status.exit_code}"
        color = "ok" if status.exit_code == 0 else "err"
    else:
        label = status.outcome
        color = "warn" if status.outcome in {"timeout", "cancelled"} else "err"
    return omp.ui.tml("<text fg={color} bold>[{label}]</text>", color=color, label=label)


@omp.renderer("bash", family="", rev=1, reduce=_reduce_bash)
def render_bash(
    view: omp.View[_BashUpdate, _BashPayload, omp.Fault], ctx: omp.ui.RenderCtx
) -> omp.ui.Tml | None:
    """Render bash revision 1 from typed status and pre-folded output counts."""

    state = view.state if isinstance(view.state, _BashFoldState) else _EMPTY_BASH_STATE
    if view.verdict is None:
        return omp.ui.tml(
            "<row>{icon}<text> running</text><spacer/><text fg=muted>{bytes}</text></row>",
            icon=omp.ui.icon("terminal"),
            bytes=_byte_label(state.total),
        )
    match view.verdict:
        case omp.Ok(_BashPayload(command=command, status=status)):
            spilled = status.spilled_output.size if status.spilled_output is not None else 0
            total = state.total + spilled
            header = omp.ui.tml(
                "<row>{icon}<text bold> bash</text> {badge}<spacer/><text fg=info>{bytes}</text></row>",
                icon=omp.ui.icon("terminal"),
                badge=_exit_badge(status),
                bytes=_byte_label(total),
            )
            if ctx.collapsed:
                return header
            rows = (
                ("command", command),
                ("stdout", _byte_label(state.stdout)),
                ("stderr", _byte_label(state.stderr)),
                ("pty", _byte_label(state.pty)),
                ("spilled", _byte_label(spilled)),
                ("wall", f"{status.wall_clock_ms} ms"),
            )
            return omp.ui.tml(
                "<col gap=1>{header}{table}</col>",
                header=header,
                table=_table(("field", "value"), rows),
            )
        case _:
            return None


def _grep_file_bytes(file: _GrepFile) -> int:
    """Count UTF-8 bytes retained in a typed grep file group."""

    return sum(len(match.line.encode("utf-8")) for match in file.matches)


def _grep_rows(files: tuple[_GrepFile, ...]) -> tuple[tuple[str, ...], ...]:
    """Pre-compute stable table cells for grep file groups."""

    return tuple(
        (file.path, str(len(file.matches)), _byte_label(_grep_file_bytes(file))) for file in files
    )


@omp.renderer("grep", family="", rev=1)
def render_grep(
    view: omp.View[object, _GrepPayload, omp.Fault], ctx: omp.ui.RenderCtx
) -> omp.ui.Tml | None:
    """Render grep revision 1 from typed groups and output accounting fields."""

    if view.verdict is None:
        return omp.ui.tml("<row>{icon}<text> searching</text></row>", icon=omp.ui.icon("search"))
    match view.verdict:
        case omp.Ok(
            _GrepPayload(
                files=files,
                total_files=total_files,
                total_files_lower_bound=lower_bound,
                projected_text=projected_text,
                output_blob=output_blob,
                output_total_lines=output_total_lines,
            )
        ):
            total_matches = sum(len(file.matches) for file in files)
            output_bytes = output_blob.size if output_blob is not None else len(projected_text.encode("utf-8"))
            qualifier = "+" if lower_bound else ""
            header = omp.ui.tml(
                "<row>{icon}<text bold> grep</text><spacer/><text fg=info>{matches} match(es) · {files} file(s) · {bytes}</text></row>",
                icon=omp.ui.icon("search"),
                matches=total_matches,
                files=f"{total_files}{qualifier}",
                bytes=_byte_label(output_bytes),
            )
            if ctx.collapsed or not files:
                return header
            rows = _grep_rows(files) + (("total output", f"{output_total_lines} line(s)", _byte_label(output_bytes)),)
            return omp.ui.tml(
                "<col gap=1>{header}{table}</col>",
                header=header,
                table=_table(("file", "matches", "bytes"), rows),
            )
        case _:
            return None
