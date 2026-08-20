from __future__ import annotations

import omp
from omp import Duration
from omp import (
    Api,
    AuthMode,
    AuthSpec,
    CacheRetention,
    ChatCaps,
    CompatFlags,
    ContextSpec,
    Cost,
    CredentialSource,
    Effort,
    ManagementSpec,
    Modality,
    ModelSpec,
    OAuthFlow,
    OAuthSpec,
    Operation,
    PrincipalResolution,
    ProviderSpec,
    RouteSpec,
    ThinkingMode,
    ThinkingSpec,
    TokenPlacement,
    ToolCaps,
    ToolFeature,
    ToolSchemaFlavor,
)

_KIMI_AUTH = AuthSpec(
    mode=AuthMode.BEARER,
    header="authorization",
    prefix="Bearer ",
    sources=(CredentialSource.stored(), CredentialSource.env("KIMI_API_KEY")),
    oauth=OAuthSpec(
        client_id="kimi-code-cli",
        token_url="https://api.moonshot.cn/oauth/token",
        flow=OAuthFlow.device_code(
            "https://api.moonshot.cn/oauth/device/code",
            interval=Duration("5s"),
        ),
        scopes=("code",),
        placement=TokenPlacement.header("authorization", "Bearer "),
        principal=PrincipalResolution.access_token_claims("sub", "uid"),
    ),
)

_KIMI_SPEC = ProviderSpec(
    id="kimi-code",
    name="Kimi Code",
    management=ManagementSpec(
        operations=frozenset({Operation.AUTH, Operation.USAGE}),
        refresh=True,
    ),
    routes=(
        RouteSpec(
            id="anthropic",
            base_url="https://api.moonshot.cn/anthropic",
            api=Api.ANTHROPIC_MESSAGES,
            auth=_KIMI_AUTH,
            compat=CompatFlags(schema_flavor=ToolSchemaFlavor.MOONSHOT_MFJS),
        ),
    ),
    models=(
        ModelSpec(
            id="kimi-k2-turbo",
            display_name="Kimi K2 Turbo",
            family="kimi-k2",
            routes=("anthropic",),
            operations=frozenset({Operation.CHAT, Operation.COUNT_TOKENS}),
            context_window=262_144,
            max_output_tokens=32_768,
            input_modalities=frozenset({Modality.TEXT, Modality.IMAGE}),
            thinking=ThinkingSpec(
                mode=ThinkingMode.EFFORT,
                efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH),
                default=Effort.MEDIUM,
            ),
            cost=Cost(input="0.60", output="2.50", cache_read="0.15"),
            context=ContextSpec.prefix_cache(
                retention=frozenset({CacheRetention.SESSION})
            ),
            chat=ChatCaps(
                tools=ToolCaps(
                    features=frozenset(
                        {ToolFeature.PARALLEL, ToolFeature.NAMED_CHOICE}
                    ),
                    maximum_tools=128,
                ),
                input_modalities=frozenset({Modality.TEXT, Modality.IMAGE}),
            ),
        ),
    ),
)


@omp.provider(_KIMI_SPEC)
class KimiCode:
    """Declare Kimi Code inference routes, authentication, and model facts."""
