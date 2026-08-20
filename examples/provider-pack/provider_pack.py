from __future__ import annotations

import omp
from omp import (
    Api,
    AuthMode,
    AuthSpec,
    CacheRetention,
    Cap,
    ChatCaps,
    CompatFlags,
    ContextSpec,
    Cost,
    CredentialSource,
    DiscoveryDefaults,
    DiscoveryKind,
    DiscoverySpec,
    Effort,
    ManagementSpec,
    Modality,
    ModelSpec,
    Operation,
    PromptCacheCaps,
    ProviderSpec,
    ReasoningCaps,
    Role,
    RouteSpec,
    ServiceTier,
    ThinkingMode,
    ThinkingSpec,
    ToolCaps,
    ToolFeature,
    ToolSchemaFlavor,
)


def _validate_pack(specs: tuple[ProviderSpec, ...]) -> tuple[ProviderSpec, ...]:
    """Reject ambiguous provider or model identifiers before registration."""
    providers: set[str] = set()
    models: dict[str, str] = {}
    for spec in specs:
        if spec.id in providers:
            raise ValueError(f"duplicate provider id in provider pack: {spec.id}")
        providers.add(spec.id)
        for model in spec.models:
            owner = models.get(model.id)
            if owner is not None:
                raise ValueError(
                    f"duplicate model id in provider pack: {model.id} "
                    f"({owner}, {spec.id})"
                )
            models[model.id] = spec.id
    return specs


_MOONSHOT_AUTH = AuthSpec(
    mode=AuthMode.BEARER,
    header="authorization",
    prefix="Bearer ",
    sources=(
        CredentialSource.stored(),
        CredentialSource.env("MOONSHOT_API_KEY"),
    ),
)

_MOONSHOT_TOOLS = ToolCaps(
    features=frozenset(
        {
            ToolFeature.PARALLEL,
            ToolFeature.NAMED_CHOICE,
            ToolFeature.REQUIRED_CHOICE,
        }
    ),
)

_MOONSHOT_CACHE = PromptCacheCaps(
    retention=frozenset({CacheRetention.STANDARD}),
    minimum_prefix_tokens=1_024,
)

_MOONSHOT_SPEC = ProviderSpec(
    id="moonshot",
    name="Moonshot AI",
    routes=(
        RouteSpec(
            id="global",
            base_url="https://api.moonshot.ai/v1",
            api=Api.OPENAI_CHAT,
            auth=_MOONSHOT_AUTH,
            compat=CompatFlags(schema_flavor=ToolSchemaFlavor.MOONSHOT_MFJS),
        ),
    ),
    models=(
        ModelSpec(
            id="kimi-k2.6",
            display_name="Kimi K2.6",
            routes=("global",),
            family="kimi-k2.6",
            context_window=262_144,
            max_output_tokens=32_768,
            input_modalities=frozenset({Modality.TEXT, Modality.IMAGE}),
            thinking=ThinkingSpec(
                mode=ThinkingMode.EFFORT,
                efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH),
                default=Effort.MEDIUM,
                supports_display=True,
            ),
            cost=Cost(
                input="0.95",
                output="4.00",
                cache_read="0.16",
                cache_write="0.95",
            ),
            compat=CompatFlags(schema_flavor=ToolSchemaFlavor.MOONSHOT_MFJS),
            context=ContextSpec.prefix_cache(
                retention=frozenset({CacheRetention.STANDARD}),
                min_prefix_tokens=1_024,
            ),
            chat=ChatCaps(
                roles=frozenset(
                    {Role.SYSTEM, Role.USER, Role.ASSISTANT, Role.TOOL}
                ),
                mid_session_roles=frozenset(
                    {Role.USER, Role.ASSISTANT, Role.TOOL}
                ),
                tools=_MOONSHOT_TOOLS,
                structured_output=frozenset({"json_object", "json_schema"}),
                reasoning=ReasoningCaps(
                    features=frozenset({"display", "effort"}),
                    efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH),
                ),
                input_modalities=frozenset({Modality.TEXT, Modality.IMAGE}),
                prompt_caching=_MOONSHOT_CACHE,
                service_tiers=(
                    ServiceTier(name="standard", priority=0),
                    ServiceTier(name="priority", priority=20),
                ),
                sampling=frozenset({"temperature", "top_p", "stop"}),
            ),
        ),
        ModelSpec(
            id="kimi-k2.5",
            display_name="Kimi K2.5",
            routes=("global",),
            family="kimi-k2.5",
            context_window=262_144,
            max_output_tokens=32_768,
            input_modalities=frozenset({Modality.TEXT, Modality.IMAGE}),
            thinking=ThinkingSpec(
                mode=ThinkingMode.EFFORT,
                efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH),
                default=Effort.MEDIUM,
            ),
            cost=Cost(
                input="0.60",
                output="3.00",
                cache_read="0.10",
                cache_write="0.60",
            ),
        ),
    ),
)


_ZAI_AUTH = AuthSpec(
    mode=AuthMode.API_KEY,
    header="x-api-key",
    prefix="",
    sources=(
        CredentialSource.stored(),
        CredentialSource.env("ZAI_GLM_API_KEY"),
    ),
)

_ZAI_SPEC = ProviderSpec(
    id="zai-glm",
    name="Z.ai GLM",
    routes=(
        RouteSpec(
            id="anthropic",
            base_url="https://api.z.ai/api/anthropic",
            api=Api.ANTHROPIC_MESSAGES,
            auth=_ZAI_AUTH,
            compat=CompatFlags(schema_flavor=ToolSchemaFlavor.ANTHROPIC),
        ),
    ),
    models=(
        ModelSpec(
            id="glm-5.1",
            display_name="GLM 5.1",
            routes=("anthropic",),
            family="glm-5",
            context_window=200_000,
            max_output_tokens=128_000,
            input_modalities=frozenset({Modality.TEXT, Modality.IMAGE}),
            thinking=ThinkingSpec(
                mode=ThinkingMode.BUDGET,
                efforts=(
                    Effort.MINIMAL,
                    Effort.LOW,
                    Effort.MEDIUM,
                    Effort.HIGH,
                    Effort.XHIGH,
                ),
                default=Effort.HIGH,
                supports_display=True,
            ),
            cost=Cost(input="1.00", output="3.20"),
            compat=CompatFlags(schema_flavor=ToolSchemaFlavor.ANTHROPIC),
            chat=ChatCaps(
                roles=frozenset(
                    {Role.SYSTEM, Role.USER, Role.ASSISTANT, Role.TOOL}
                ),
                mid_session_roles=frozenset(
                    {Role.USER, Role.ASSISTANT, Role.TOOL}
                ),
                tools=ToolCaps(
                    features=frozenset(
                        {
                            ToolFeature.PARALLEL,
                            ToolFeature.NAMED_CHOICE,
                            ToolFeature.REQUIRED_CHOICE,
                        }
                    )
                ),
                structured_output=frozenset({"json_object"}),
                reasoning=ReasoningCaps(
                    features=frozenset({"display", "budget"}),
                    efforts=(
                        Effort.MINIMAL,
                        Effort.LOW,
                        Effort.MEDIUM,
                        Effort.HIGH,
                        Effort.XHIGH,
                    ),
                ),
                input_modalities=frozenset({Modality.TEXT, Modality.IMAGE}),
                service_tiers=Cap.UNSUPPORTED,
            ),
        ),
        ModelSpec(
            id="glm-4.7-flash",
            display_name="GLM 4.7 Flash",
            routes=("anthropic",),
            family="glm-4.7-flash",
            context_window=200_000,
            max_output_tokens=128_000,
            input_modalities=frozenset({Modality.TEXT, Modality.IMAGE}),
            thinking=ThinkingSpec(
                mode=ThinkingMode.BUDGET,
                efforts=(Effort.MINIMAL, Effort.LOW, Effort.MEDIUM, Effort.HIGH),
                default=Effort.MEDIUM,
            ),
            cost=Cost.free(),
        ),
    ),
)


_ALIBABA_CODING_AUTH = AuthSpec(
    mode=AuthMode.BEARER,
    header="authorization",
    prefix="Bearer ",
    sources=(
        CredentialSource.stored(),
        CredentialSource.env("ALIBABA_CODING_API_KEY"),
    ),
)

_ALIBABA_TOKEN_AUTH = AuthSpec(
    mode=AuthMode.BEARER,
    header="authorization",
    prefix="Bearer ",
    sources=(
        CredentialSource.stored(),
        CredentialSource.env("ALIBABA_TOKEN_PLAN_API_KEY"),
    ),
)

_ALIBABA_API_AUTH = AuthSpec(
    mode=AuthMode.BEARER,
    header="authorization",
    prefix="Bearer ",
    sources=(
        CredentialSource.stored(),
        CredentialSource.env("ALIBABA_API_KEY", "DASHSCOPE_API_KEY"),
    ),
)

_ALIBABA_SPEC = ProviderSpec(
    id="alibaba-dashscope",
    name="Alibaba Cloud DashScope",
    routes=(
        RouteSpec(
            id="coding-intl",
            base_url="https://coding-intl.dashscope.aliyuncs.com/v1",
            api=Api.OPENAI_CHAT,
            auth=_ALIBABA_CODING_AUTH,
        ),
        RouteSpec(
            id="coding-cn",
            base_url="https://coding.dashscope.aliyuncs.com/v1",
            api=Api.OPENAI_CHAT,
            auth=_ALIBABA_CODING_AUTH,
        ),
        RouteSpec(
            id="token-intl",
            base_url="https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
            api=Api.OPENAI_CHAT,
            auth=_ALIBABA_TOKEN_AUTH,
        ),
        RouteSpec(
            id="token-cn",
            base_url="https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
            api=Api.OPENAI_CHAT,
            auth=_ALIBABA_TOKEN_AUTH,
        ),
        RouteSpec(
            id="api-intl",
            base_url="https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            api=Api.OPENAI_CHAT,
            auth=_ALIBABA_API_AUTH,
        ),
        RouteSpec(
            id="api-cn",
            base_url="https://dashscope.aliyuncs.com/compatible-mode/v1",
            api=Api.OPENAI_CHAT,
            auth=_ALIBABA_API_AUTH,
        ),
    ),
    models=(
        ModelSpec(
            id="qwen3.8-max-preview",
            display_name="Qwen3.8 Max Preview",
            routes=("token-intl", "token-cn"),
            family="qwen3.8-max",
            context_window=128_000,
            max_output_tokens=8_192,
            thinking=ThinkingSpec(
                mode=ThinkingMode.EFFORT,
                efforts=(Effort.LOW, Effort.HIGH, Effort.XHIGH),
                default=Effort.HIGH,
                supports_display=True,
                requires_effort=True,
            ),
            cost=Cost.free(),
            compat=CompatFlags(schema_flavor=ToolSchemaFlavor.JSON_SCHEMA),
            chat=ChatCaps(
                roles=frozenset(
                    {Role.SYSTEM, Role.USER, Role.ASSISTANT, Role.TOOL}
                ),
                mid_session_roles=frozenset(
                    {Role.USER, Role.ASSISTANT, Role.TOOL}
                ),
                tools=ToolCaps(
                    features=frozenset(
                        {
                            ToolFeature.PARALLEL,
                            ToolFeature.NAMED_CHOICE,
                            ToolFeature.REQUIRED_CHOICE,
                        }
                    )
                ),
                structured_output=frozenset({"json_object", "json_schema"}),
                reasoning=ReasoningCaps(
                    features=frozenset({"display", "effort"}),
                    efforts=(Effort.LOW, Effort.HIGH, Effort.XHIGH),
                ),
                input_modalities=frozenset({Modality.TEXT}),
                service_tiers=(
                    ServiceTier(name="token-plan", priority=10),
                ),
                sampling=frozenset({"temperature", "top_p", "stop"}),
            ),
        ),
        ModelSpec(
            id="qwen3-8b",
            display_name="Qwen3 8B",
            routes=("coding-intl", "coding-cn", "api-intl", "api-cn"),
            family="qwen3",
            context_window=128_000,
            max_output_tokens=8_192,
            cost=Cost.free(),
        ),
        ModelSpec(
            id="qwen3.7-max",
            display_name="Qwen3.7 Max",
            routes=("token-intl", "token-cn", "api-intl", "api-cn"),
            family="qwen3.7-max",
            context_window=128_000,
            max_output_tokens=8_192,
            cost=Cost.free(),
        ),
    ),
)


_OVH_AUTH = AuthSpec(
    mode=AuthMode.BEARER,
    header="authorization",
    prefix="Bearer ",
    sources=(
        CredentialSource.stored(),
        CredentialSource.env("OVH_AI_TOKEN"),
    ),
)

_OVH_DISCOVERY = DiscoverySpec(
    kind=DiscoveryKind.OPENAI_MODELS,
    path="/models",
    label="OVH AI Endpoints models",
    authoritative=False,
)

_OVH_SPEC = ProviderSpec(
    id="ovhai",
    name="OVH AI Endpoints",
    management=ManagementSpec(
        operations=frozenset({Operation.DISCOVER_MODELS}),
    ),
    routes=(
        RouteSpec(
            id="responses",
            base_url="https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
            api=Api.OPENAI_RESPONSES,
            auth=_OVH_AUTH,
            discovery=_OVH_DISCOVERY,
            priority=10,
        ),
        RouteSpec(
            id="chat-completions",
            base_url="https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
            api=Api.OPENAI_CHAT,
            auth=_OVH_AUTH,
        ),
    ),
    discovery_defaults=DiscoveryDefaults(
        routes=("responses",),
        cost=Cost.free(),
        context_window=32_768,
        max_output_tokens=32_768,
    ),
    models=(
        ModelSpec(
            id="gpt-oss-120b",
            display_name="GPT OSS 120B",
            routes=("responses", "chat-completions"),
            family="gpt-oss",
            context_window=131_072,
            max_output_tokens=32_768,
            thinking=ThinkingSpec(
                mode=ThinkingMode.EFFORT,
                efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH),
                default=Effort.MEDIUM,
                supports_display=True,
            ),
            cost=Cost.free(),
            compat=CompatFlags(schema_flavor=ToolSchemaFlavor.JSON_SCHEMA),
            chat=ChatCaps(
                roles=frozenset(
                    {
                        Role.DEVELOPER,
                        Role.USER,
                        Role.ASSISTANT,
                        Role.TOOL,
                    }
                ),
                mid_session_roles=frozenset(
                    {Role.USER, Role.ASSISTANT, Role.TOOL}
                ),
                tools=ToolCaps(
                    features=frozenset(
                        {
                            ToolFeature.PARALLEL,
                            ToolFeature.STRICT_SCHEMA,
                            ToolFeature.NAMED_CHOICE,
                            ToolFeature.REQUIRED_CHOICE,
                        }
                    )
                ),
                structured_output=frozenset({"json_object", "json_schema"}),
                reasoning=ReasoningCaps(
                    features=frozenset({"display", "effort"}),
                    efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH),
                ),
                input_modalities=frozenset({Modality.TEXT}),
                service_tiers=Cap.UNSUPPORTED,
                sampling=frozenset({"temperature", "top_p", "stop"}),
                server_state=Cap.UNSUPPORTED,
            ),
        ),
        ModelSpec(
            id="Qwen3.5-9B",
            display_name="Qwen 3.5 9B",
            routes=("chat-completions",),
            family="qwen3.5",
            context_window=32_768,
            max_output_tokens=32_768,
            input_modalities=frozenset({Modality.TEXT, Modality.IMAGE}),
            thinking=ThinkingSpec(
                mode=ThinkingMode.EFFORT,
                efforts=(Effort.LOW, Effort.MEDIUM, Effort.HIGH),
                default=Effort.MEDIUM,
            ),
            cost=Cost.free(),
        ),
    ),
)


_PACK = _validate_pack(
    (_MOONSHOT_SPEC, _ZAI_SPEC, _ALIBABA_SPEC, _OVH_SPEC)
)


@omp.provider(_PACK[0])
class MoonshotProvider:
    """Declare Moonshot endpoints, authentication, model facts, and pricing."""


@omp.provider(_PACK[1])
class ZaiGlmProvider:
    """Declare Z.ai's Anthropic-compatible GLM catalog."""


@omp.provider(_PACK[2])
class AlibabaDashScopeProvider:
    """Declare Alibaba's regional coding, token-plan, and API routes."""


@omp.provider(_PACK[3])
class OvhAiProvider:
    """Declare OVH AI Endpoints routes and host-owned model discovery."""
