from __future__ import annotations

import omp
from omp import (
    AccountScope,
    Api,
    AuthMode,
    AuthSpec,
    CacheRetention,
    ChatCaps,
    ContextSpec,
    Cost,
    Credential,
    CredentialKind,
    CredentialSource,
    Duration,
    Effort,
    ManagementSpec,
    Modality,
    ModelSpec,
    Operation,
    PromptCacheCaps,
    ProviderSpec,
    RefreshRequest,
    RouteSpec,
    Secret,
    ThinkingMode,
    ThinkingSpec,
    ToolCaps,
    ToolFeature,
)

_PROVIDER_ID = "anthropic-vertex"
_GCP_SCOPE = "https://www.googleapis.com/auth/cloud-platform"
_TOKEN_TTL = Duration("45m")
_REGIONS = ("global", "us-east5", "europe-west1", "asia-southeast1")

_VERTEX_AUTH = AuthSpec(
    mode=AuthMode.GCP_ADC,
    header="authorization",
    prefix="Bearer ",
    scopes=(_GCP_SCOPE,),
    account_scope=AccountScope.REGION,
    sources=(CredentialSource.stored(),),
)


def _endpoint(region: str) -> str:
    host = "aiplatform.googleapis.com" if region == "global" else f"{region}-aiplatform.googleapis.com"
    return (
        f"https://{host}/v1/projects/{{project}}/locations/{region}"
        "/publishers/anthropic/models"
    )


def _route(region: str) -> RouteSpec:
    return RouteSpec(
        id=region,
        base_url=_endpoint(region),
        api=Api.ANTHROPIC_MESSAGES,
        auth=_VERTEX_AUTH,
        region=region,
    )


_ALL_REGIONS = _REGIONS
_NON_APAC_REGIONS = _REGIONS[:3]
_CHAT_OPERATIONS = frozenset({Operation.CHAT, Operation.COUNT_TOKENS})
_INPUTS = frozenset({Modality.TEXT, Modality.IMAGE, Modality.DOCUMENT})
_TOOLS = ToolCaps(
    features=frozenset(
        {ToolFeature.PARALLEL, ToolFeature.NAMED_CHOICE, ToolFeature.REQUIRED_CHOICE}
    )
)
_CACHE = PromptCacheCaps(
    retention=frozenset({CacheRetention.SHORT}),
    max_breakpoints=4,
)
_CONTEXT = ContextSpec.prefix_cache(
    retention=frozenset({CacheRetention.SHORT}),
    max_breakpoints=4,
)

_VERTEX_SPEC = ProviderSpec(
    id=_PROVIDER_ID,
    name="Anthropic on Google Cloud Vertex AI",
    management=ManagementSpec(
        operations=frozenset({Operation.AUTH}),
        refresh=True,
    ),
    routes=tuple(_route(region) for region in _REGIONS),
    models=(
        ModelSpec(
            id="claude-opus-4-6",
            display_name="Claude Opus 4.6 on Vertex AI",
            family="claude-opus-4-6",
            routes=_ALL_REGIONS,
            wire_ids={region: "claude-opus-4-6" for region in _ALL_REGIONS},
            operations=_CHAT_OPERATIONS,
            context_window=1_000_000,
            max_output_tokens=128_000,
            input_modalities=_INPUTS,
            thinking=ThinkingSpec(
                mode=ThinkingMode.ANTHROPIC_ADAPTIVE,
                efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH, Effort.MAX),
                default=Effort.HIGH,
            ),
            cost=Cost(input="5", output="25", cache_read="0.50", cache_write="6.25"),
            context=_CONTEXT,
            chat=ChatCaps(tools=_TOOLS, input_modalities=_INPUTS, prompt_caching=_CACHE),
        ),
        ModelSpec(
            id="claude-sonnet-4-6",
            display_name="Claude Sonnet 4.6 on Vertex AI",
            family="claude-sonnet-4-6",
            routes=_ALL_REGIONS,
            wire_ids={region: "claude-sonnet-4-6" for region in _ALL_REGIONS},
            operations=_CHAT_OPERATIONS,
            context_window=1_000_000,
            max_output_tokens=128_000,
            input_modalities=_INPUTS,
            thinking=ThinkingSpec(
                mode=ThinkingMode.ANTHROPIC_ADAPTIVE,
                efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH, Effort.MAX),
                default=Effort.HIGH,
            ),
            cost=Cost(input="3", output="15", cache_read="0.30", cache_write="3.75"),
            context=_CONTEXT,
            chat=ChatCaps(tools=_TOOLS, input_modalities=_INPUTS, prompt_caching=_CACHE),
        ),
        ModelSpec(
            id="claude-haiku-4-5",
            display_name="Claude Haiku 4.5 on Vertex AI",
            family="claude-haiku-4-5",
            routes=_NON_APAC_REGIONS,
            wire_ids={region: "claude-haiku-4-5" for region in _NON_APAC_REGIONS},
            operations=_CHAT_OPERATIONS,
            context_window=200_000,
            max_output_tokens=64_000,
            input_modalities=_INPUTS,
            thinking=ThinkingSpec(
                mode=ThinkingMode.BUDGET,
                efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH),
                default=Effort.MEDIUM,
                budgets={Effort.LOW: 2_048, Effort.MEDIUM: 8_192, Effort.HIGH: 32_768},
            ),
            cost=Cost(input="1", output="5", cache_read="0.10", cache_write="1.25"),
            context=_CONTEXT,
            chat=ChatCaps(tools=_TOOLS, input_modalities=_INPUTS, prompt_caching=_CACHE),
        ),
    ),
)


@omp.provider(_VERTEX_SPEC)
class AnthropicVertex:
    """Declare region-scoped Anthropic routes on Google Cloud Vertex AI."""

    @omp.hook("provider_refresh")
    async def refresh(self, req: RefreshRequest, ctx: omp.Context) -> Credential:
        """Mint one short-lived regional GCP bearer credential off the request path."""

        del ctx
        region = req.props.get("region")
        if not isinstance(region, str) or region not in _REGIONS:
            raise ValueError("Vertex credential refresh requires a declared region")
        scoped = await omp.creds.mint_scoped(
            f"gcp-access-token:{region}",
            ttl=_TOKEN_TTL,
            provider=req.provider,
        )
        return Credential(
            kind=CredentialKind.BEARER,
            secret=Secret(scoped.token),
            expires_at_ms=scoped.expires_at_ms,
            identity=req.identity,
            props=req.props,
        )
