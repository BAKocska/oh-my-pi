from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import (
    Api,
    AuthMode,
    AuthSpec,
    ChatCaps,
    CredentialSource,
    ModelSpec,
    Operation,
    ProviderSpec,
    RouteSpec,
    ToolCaps,
    ToolFeature,
)


_PROVIDER_ID = "google-native-grounding"
_ROUTE_ID = "gemini"
_HOSTED_GROUNDING = frozenset({"search", "web"})
_TOOL_CALLS = ToolCaps(
    features=frozenset(
        {
            ToolFeature.PARALLEL,
            ToolFeature.NAMED_CHOICE,
            ToolFeature.DISABLED_CHOICE,
        }
    )
)

_GROUNDING_MODEL = ModelSpec(
    id="gemini-2.5-pro",
    display_name="Gemini 2.5 Pro with native grounding",
    family="gemini-2.5",
    routes=(_ROUTE_ID,),
    operations=frozenset({Operation.CHAT, Operation.COUNT_TOKENS}),
    chat=ChatCaps(
        tools=_TOOL_CALLS,
        hosted_tools=_HOSTED_GROUNDING,
    ),
)

_NATIVE_GROUNDING_SPEC = ProviderSpec(
    id=_PROVIDER_ID,
    name="Google native grounding",
    routes=(
        RouteSpec(
            id=_ROUTE_ID,
            base_url="https://generativelanguage.googleapis.com/v1beta",
            api=Api.GEMINI,
            auth=AuthSpec(
                mode=AuthMode.API_KEY,
                header="x-goog-api-key",
                prefix="",
                sources=(
                    CredentialSource.stored(),
                    CredentialSource.env("GEMINI_API_KEY", "GOOGLE_API_KEY"),
                ),
            ),
        ),
    ),
    models=(_GROUNDING_MODEL,),
)

omp.provider(_NATIVE_GROUNDING_SPEC)


@dataclass(frozen=True, slots=True)
class Citation:
    """One provider-returned grounding citation."""

    title: str
    url: str
    excerpt: str = ""


@dataclass(frozen=True, slots=True)
class CitationArgs:
    """Grounding citations to retain and present."""

    citations: tuple[Citation, ...]


@dataclass(frozen=True, slots=True)
class CitationPayload(omp.Payload):
    """Typed durable citation set returned by the presentation device."""

    citations: tuple[Citation, ...]


@dataclass(frozen=True, slots=True)
class CitationFault(omp.Fault):
    """Typed reason a citation set could not be presented."""

    detail: str


class GroundingCitations:
    """Retain provider citations without proxying a provider-native tool."""

    Payload = CitationPayload
    Fault = CitationFault

    async def __call__(
        self, args: CitationArgs, ctx: omp.Context
    ) -> CitationPayload | CitationFault:
        """Return the provider's citations as a typed verdict."""

        del ctx
        if not args.citations:
            return CitationFault("the provider returned no grounding citations")
        if any(not citation.title.strip() or not citation.url.strip() for citation in args.citations):
            return CitationFault("every citation requires a non-empty title and URL")
        return CitationPayload(args.citations)

    def prompt(
        self,
        view: omp.Ok[CitationPayload] | omp.Faulted[CitationFault],
        caps: omp.PromptCaps,
    ) -> list[omp.TextPart | omp.JsonPart | omp.BlobPart]:
        """Project a bounded numbered citation list for the model."""

        out = omp.Budget(caps)
        match view:
            case omp.Ok(payload):
                out.push(f"Grounding citations ({len(payload.citations)}):")
                for index, citation in enumerate(payload.citations, 1):
                    if not out.push(f"{index}. {citation.title} — {citation.url}"):
                        break
            case omp.Faulted(fault):
                out.push(f"Grounding citations unavailable: {fault.detail}")
            case _:
                raise TypeError("grounding citation prompt received an unsupported outcome")
        return out.finish()


grounding_citations = omp.device(
    "grounding_citations",
    family="native-grounding",
    rev=1,
    place="host",
    summary="Retain and render citations returned by provider-native grounding.",
)(GroundingCitations())


@omp.renderer("grounding_citations", family="native-grounding", rev=1)
def render_grounding_citations(
    view: omp.View[object, CitationPayload, CitationFault], ctx: omp.ui.RenderCtx
) -> omp.ui.Tml | None:
    """Render provider-returned citations from the typed device verdict."""

    if view.verdict is None:
        return omp.ui.tml(
            "<row>{icon} preparing grounding citations</row>",
            icon=omp.ui.icon("search"),
        )
    match view.verdict:
        case omp.Ok(payload):
            if ctx.collapsed:
                return omp.ui.tml(
                    "<row>{icon} {count} grounding citations</row>",
                    icon=omp.ui.icon("search"),
                    count=len(payload.citations),
                )
            citations = "\n".join(
                f"{index}. {citation.title}\n   {citation.url}"
                + (f"\n   {citation.excerpt}" if citation.excerpt else "")
                for index, citation in enumerate(payload.citations, 1)
            )
            return omp.ui.tml(
                "<box title='Grounding citations'>{citations}</box>",
                citations=omp.ui.text(citations),
            )
        case omp.Faulted(fault):
            return omp.ui.tml(
                "<row>{icon} {detail}</row>",
                icon=omp.ui.icon("error"),
                detail=fault.detail,
            )
        case _:
            return None
