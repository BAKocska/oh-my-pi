from __future__ import annotations

import dataclasses

import omp
from omp import Budget, Faulted, Ok, Payload, SpillBudget


@dataclasses.dataclass(frozen=True, slots=True)
class OutputArgs:
    """Structured output captured by the demonstration device."""

    label: str
    details: tuple[str, ...]
    error: str | None = None
    superseded: bool = False


@dataclasses.dataclass(frozen=True, slots=True)
class OutputPayload(Payload):
    """Complete durable output retained independently of its prompt projection."""

    label: str
    details: tuple[str, ...]
    error: str | None
    superseded: bool

    def useless(self) -> bool:
        """Mark a superseded projection as safe for compaction to drop."""
        return self.superseded


@dataclasses.dataclass(frozen=True, slots=True)
class OutputFault(omp.Fault):
    """Typed failure for an invalid demonstration output."""

    detail: str


class OutputBudgetDevice:
    """Retain full typed output while projecting only budgeted model text."""

    Payload = OutputPayload
    Fault = OutputFault
    __spill__ = SpillBudget(inline_limit=1024, media_type="application/json", always=True)

    async def __call__(
        self, args: OutputArgs, ctx: omp.Context
    ) -> OutputPayload | OutputFault:
        """Return the structured truth without truncating its details or error."""
        del ctx
        if not args.label.strip():
            return OutputFault("label must not be empty")
        return OutputPayload(args.label, args.details, args.error, args.superseded)

    def prompt(self, view: object, caps: omp.PromptCaps) -> list[object]:
        """Build a terse model view within maximum_text_bytes using Budget."""
        out = Budget(caps)
        match view:
            case Ok(payload):
                state = "superseded" if payload.superseded else "current"
                if not out.push(f"{payload.label}: {state}; {len(payload.details)} detail line(s)\n"):
                    return out.finish()
                if payload.error is not None:
                    out.push(f"error: {payload.error}\n")
                return out.finish()
            case Faulted(fault):
                out.push(f"output rejected: {fault.detail}\n")
                return out.finish()
            case _:
                raise TypeError("output_budget prompt received an unsupported call outcome")


@omp.renderer("output_budget", family="v", rev=1)
def render_output(view: object, ctx: omp.ui.RenderCtx) -> omp.ui.Tml:
    """Render the retained typed verdict, expanding details without re-derivation."""
    match view.verdict:
        case Ok(payload):
            head = omp.ui.tml(
                "<row><text fg=accent>{label}</text><text fg=muted> · {count} detail line(s)</text></row>",
                label=payload.label,
                count=len(payload.details),
            )
            if ctx.collapsed:
                return head
            detail_text = "\n".join(payload.details) or "(no details)"
            error = (
                omp.ui.tml("<row><text fg=err>error: {detail}</text></row>", detail=payload.error)
                if payload.error is not None
                else omp.ui.Tml.raw("")
            )
            return omp.ui.tml("{head}<pre>{details}</pre>{error}", head=head, details=detail_text, error=error)
        case Faulted(fault):
            return omp.ui.tml("<row><text fg=err>{detail}</text></row>", detail=fault.detail)
        case _:
            return omp.ui.tml("<row><text fg=muted>output pending</text></row>")


_output_budget = omp.device("output_budget", family="v", rev=1)(OutputBudgetDevice())
