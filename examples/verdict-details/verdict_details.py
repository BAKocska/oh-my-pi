from __future__ import annotations

import dataclasses
import re
from collections import Counter

import omp
from omp import Budget, Faulted, Ok, Payload, SpillBudget, View


_WORD = re.compile(r"[^\W_]+(?:['’-][^\W_]+)*", re.UNICODE)


@dataclasses.dataclass(frozen=True, slots=True)
class AnalyzeArgs:
    """Text and an optional label to analyze."""

    text: str
    label: str = "input"


@dataclasses.dataclass(frozen=True, slots=True)
class TermCount:
    """One normalized term and its complete occurrence count."""

    term: str
    count: int


@dataclasses.dataclass(frozen=True, slots=True)
class AnalyzePayload(Payload):
    """Complete durable analysis retained independently of its projections."""

    label: str
    source_text: str
    utf8_bytes: int
    characters: int
    lines: int
    words: int
    unique_terms: int
    longest_line_characters: int
    terms: list[TermCount]


@dataclasses.dataclass(frozen=True, slots=True)
class AnalyzeFault(omp.Fault):
    """Typed reason an analysis request could not be completed."""

    detail: str


def _summary(payload: AnalyzePayload, *, compact: bool) -> str:
    if compact:
        return (
            f"{payload.label}: {payload.lines} lines · {payload.words} words · "
            f"{payload.utf8_bytes} bytes"
        )
    return (
        f"{payload.label}: {payload.lines} lines · {payload.words} words · "
        f"{payload.unique_terms} unique terms · {payload.characters} characters · "
        f"{payload.utf8_bytes} UTF-8 bytes · longest line "
        f"{payload.longest_line_characters} characters"
    )


def _analyze(args: AnalyzeArgs) -> AnalyzePayload | AnalyzeFault:
    if not args.text.strip():
        return AnalyzeFault("text must contain at least one non-whitespace character")

    label = args.label.strip() or "input"
    lines = args.text.splitlines()
    normalized_terms = [match.group(0).casefold() for match in _WORD.finditer(args.text)]
    counts = Counter(normalized_terms)
    terms = [
        TermCount(term, count)
        for term, count in sorted(counts.items(), key=lambda item: (-item[1], item[0]))
    ]
    return AnalyzePayload(
        label=label,
        source_text=args.text,
        utf8_bytes=len(args.text.encode("utf-8")),
        characters=len(args.text),
        lines=len(lines),
        words=len(normalized_terms),
        unique_terms=len(counts),
        longest_line_characters=max(map(len, lines), default=0),
        terms=terms,
    )


class Analyze:
    """Analyze text while keeping complete details outside model-visible parts."""

    Payload = AnalyzePayload
    Fault = AnalyzeFault
    __spill__ = SpillBudget(inline_limit=4 * 1024)

    async def __call__(
        self, args: AnalyzeArgs, ctx: omp.Context
    ) -> AnalyzePayload | AnalyzeFault:
        """Return complete typed details and let the host journal or spill them."""

        del ctx
        return _analyze(args)

    def prompt(
        self,
        view: Ok[AnalyzePayload] | Faulted[AnalyzeFault],
        caps: omp.PromptCaps,
    ) -> list[omp.TextPart | omp.JsonPart | omp.BlobPart]:
        """Reproject one retained verdict as a single byte-budgeted line."""

        out = Budget(caps)
        match view:
            case Ok(payload):
                full = _summary(payload, compact=False)
                projected = full if caps.fits(full) else _summary(payload, compact=True)
                out.push(projected)
            case Faulted(fault):
                out.push(f"analysis rejected: {fault.detail}")
            case _:
                raise TypeError("analyze prompt received an unsupported call outcome")
        return out.finish()


analyze = omp.device(
    "analyze",
    family="v",
    rev=1,
    place="host",
    summary="Analyze text while retaining complete details outside model context.",
)(Analyze())


@omp.renderer("analyze", family="v", rev=1)
def render_analyze(
    view: View[object, AnalyzePayload, AnalyzeFault], ctx: omp.ui.RenderCtx
) -> omp.ui.Tml | None:
    """Render a one-line fold or an expanded view of the retained details."""

    if view.verdict is None:
        return omp.ui.tml("<row>{icon} analyzing text</row>", icon=omp.ui.icon("search"))
    match view.verdict:
        case Ok(payload):
            summary = _summary(payload, compact=False)
            if ctx.collapsed:
                return omp.ui.tml("<row>{summary}</row>", summary=summary)
            term_counts = "\n".join(
                f"{term.term}: {term.count}" for term in payload.terms
            ) or "(no terms)"
            return omp.ui.tml(
                "<box title='Analysis'><row>{summary}</row>"
                "<box title='Source'>{source}</box>"
                "<box title='Term counts'>{terms}</box></box>",
                summary=summary,
                source=omp.ui.text(payload.source_text),
                terms=omp.ui.text(term_counts),
            )
        case Faulted(fault):
            return omp.ui.tml("<row>{icon} {detail}</row>", icon=omp.ui.icon("error"), detail=fault.detail)
        case _:
            return None
