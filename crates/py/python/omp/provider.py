"""Pure provider-catalog declarations.

This module mirrors the public source vocabulary compiled into
``crates/llm-catalog``.  Importing it only constructs immutable Python values;
provider registration is recorded in the local declaration table and performs
no credential, network, filesystem, CONTROL, or DATA access.
"""
from __future__ import annotations

from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from enum import StrEnum
from types import MappingProxyType
from typing import Any, TypeVar

from ._registry import registry

_T = TypeVar("_T", bound=type)
_EMPTY_MAP: Mapping[Any, Any] = MappingProxyType({})


class Api(StrEnum):
    """Codec selector for one provider route."""

    OPENAI_CHAT = "openai_chat"
    OPENAI_RESPONSES = "openai_responses"
    OPENAI_CODEX = "openai_codex"
    ANTHROPIC_MESSAGES = "anthropic_messages"
    GEMINI = "gemini"
    GOOGLE_CCA = "google_cca"
    BEDROCK = "bedrock"
    OLLAMA = "ollama"
    GITLAB_DUO = "gitlab_duo"
    CURSOR = "cursor"
    DEVIN = "devin"
    OPENAI_EMBEDDING = "openai_embedding"
    OPENAI_MEDIA = "openai_media"
    OPENAI_REALTIME = "openai_realtime"
    SEARCH_EXA = "search_exa"
    SEARCH_TAVILY = "search_tavily"
    SEARCH_KAGI = "search_kagi"
    SEARCH_PERPLEXITY = "search_perplexity"
    SEARCH_PARALLEL = "search_parallel"
    OMP_NATIVE = "omp_native"
    LOCAL = "local"


class AuthMode(StrEnum):
    """Authentication protocol, matching Rust ``AuthSpecKind`` spellings."""

    NONE = "none"
    API_KEY = "api_key"
    BEARER = "bearer"
    OAUTH = "oauth"
    AWS_SIGV4 = "aws_sigv4"
    GCP_ADC = "gcp_adc"
    AZURE_AD = "azure_ad"
    GITHUB_APP = "github_app"
    OMP_SESSION = "omp_session"


class Transport(StrEnum):
    """Request transport matching Rust ``TransportKind`` spellings."""

    HTTP = "http"
    WEBSOCKET = "websocket"
    WEBRTC = "webrtc"
    AWS_EVENT_STREAM = "aws_event_stream"
    CONNECT = "connect"
    LOCAL = "local"


class CodecProfile(StrEnum):
    """Typed codec-construction discriminator."""

    STANDARD = "standard"
    GOOGLE_CCA_GEMINI_CLI = "google-cca-gemini-cli"
    GOOGLE_CCA_ANTIGRAVITY = "google-cca-antigravity"
    APPLE_FM = "apple-fm"


class AccountScope(StrEnum):
    """Boundary at which a principal and its quota are shared."""

    PROVIDER = "provider"
    ROUTE = "route"
    REGION = "region"


class OAuthFlowKind(StrEnum):
    """OAuth authorization-flow discriminator."""

    PKCE = "pkce"
    DEVICE_CODE = "device_code"
    PASTE = "paste"
    CUSTOM = "custom"


class Completion(StrEnum):
    """Completion mechanism for an OAuth authorization-code flow."""

    CALLBACK_URL = "callback_url"
    PASTE_CALLBACK_URL = "paste_callback_url"
    PASTE_CODE = "paste_code"


class RefreshBehavior(StrEnum):
    """OAuth refresh behavior with Rust-compatible stable spellings."""

    UNSUPPORTED = "unsupported"
    TOKEN_ENDPOINT = "token_endpoint"


class Operation(StrEnum):
    """Closed catalog operation vocabulary matching ``OperationKind``."""

    CHAT = "chat"
    COUNT_TOKENS = "count_tokens"
    TOKENIZE = "tokenize"
    DETOKENIZE = "detokenize"
    EMBED = "embed"
    GENERATE_IMAGE = "generate_image"
    GENERATE_VIDEO = "generate_video"
    SPEAK = "speak"
    TRANSCRIBE = "transcribe"
    REALTIME = "realtime"
    SEARCH = "search"
    USAGE = "usage"
    DISCOVER_MODELS = "discover_models"
    AUTH = "auth"
    NATIVE = "native"


class Modality(StrEnum):
    """Canonical media modality vocabulary."""

    TEXT = "text"
    IMAGE = "image"
    AUDIO = "audio"
    VIDEO = "video"
    DOCUMENT = "document"


class ToolFeature(StrEnum):
    """Independent tool-call behaviors from Rust ``ToolFeatureBits``."""

    PARALLEL = "parallel"
    STRICT_SCHEMA = "strict_schema"
    NAMED_CHOICE = "named_choice"
    REQUIRED_CHOICE = "required_choice"
    DISABLED_CHOICE = "disabled_choice"


class ToolSchemaFlavor(StrEnum):
    """Provider-specific tool parameter-schema normalization."""

    JSON_SCHEMA = "json_schema"
    ANTHROPIC = "anthropic"
    GOOGLE = "google"
    MOONSHOT_MFJS = "moonshot_mfjs"
    GRAMMAR = "grammar"
    CCA = "cca"


class CacheRetention(StrEnum):
    """Prompt-cache retention classes from Rust ``CacheRetentionBits``."""

    EPHEMERAL = "ephemeral"
    STANDARD = "standard"
    LONG = "long"


class ThinkingMode(StrEnum):
    """Provider-native reasoning control, matching Rust kebab-case values."""

    EFFORT = "effort"
    BUDGET = "budget"
    GOOGLE_LEVEL = "google-level"
    ANTHROPIC_ADAPTIVE = "anthropic-adaptive"
    ANTHROPIC_BUDGET_EFFORT = "anthropic-budget-effort"


class Effort(StrEnum):
    """Portable ordered reasoning effort vocabulary."""

    OFF = "off"
    MINIMAL = "minimal"
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    XHIGH = "xhigh"
    MAX = "max"


@dataclass(frozen=True, slots=True)
class CredentialSource:
    """One public credential source in exact acquisition order."""

    kind: str
    ordered_names: tuple[str, ...] = ()
    options: Mapping[str, object] = field(default_factory=lambda: _EMPTY_MAP)

    @classmethod
    def env(cls, *names: str) -> "CredentialSource":
        """Read the first populated environment variable from ``names``."""
        if not names or not all(isinstance(name, str) and name for name in names):
            raise ValueError("credential environment names must be non-empty strings")
        return cls("environment", tuple(names))

    @classmethod
    def stored(cls) -> "CredentialSource":
        """Read an encrypted credential from the account store."""
        return cls("stored")

    @classmethod
    def oauth(cls) -> "CredentialSource":
        """Run the OAuth flow linked by the enclosing authentication spec."""
        return cls("oauth")

    @classmethod
    def aws_chain(cls) -> "CredentialSource":
        """Resolve the standard AWS credential chain."""
        return cls("aws_chain")

    @classmethod
    def session(cls) -> "CredentialSource":
        """Acquire an interactive provider session credential."""
        return cls("session")


@dataclass(frozen=True, slots=True)
class TokenPlacement:
    """Placement of a resolved OAuth access token."""

    kind: str
    name: str | None = None
    prefix: str | None = None

    @classmethod
    def header(cls, name: str, prefix: str = "") -> "TokenPlacement":
        """Place the token in a sensitive request header."""
        return cls("header", name, prefix)

    @classmethod
    def query(cls, parameter: str) -> "TokenPlacement":
        """Place the token in a sensitive query parameter."""
        return cls("query", parameter)


@dataclass(frozen=True, slots=True)
class PrincipalResolution:
    """Evidence binding refreshed credentials to a stable principal."""

    kind: str
    values: tuple[str, ...]

    @classmethod
    def id_token_claim(cls, claim: str) -> "PrincipalResolution":
        """Read a verified ID-token claim."""
        return cls("id_token_claim", (claim,))

    @classmethod
    def access_token_claims(cls, *claims: str) -> "PrincipalResolution":
        """Read the first present stable access-token claim."""
        return cls("access_token_claims", tuple(claims))

    @classmethod
    def token_response_field(cls, pointer: str) -> "PrincipalResolution":
        """Read a typed token-response field by JSON Pointer."""
        return cls("token_response_field", (pointer,))

    @classmethod
    def userinfo(cls, url: str, field: str) -> "PrincipalResolution":
        """Fetch a public user-information field."""
        return cls("userinfo_endpoint", (url, field))

    @classmethod
    def static_label(cls, label: str) -> "PrincipalResolution":
        """Use a reviewed static principal label."""
        return cls("static_label", (label,))


@dataclass(frozen=True, slots=True)
class OAuthFlow:
    """Flow-specific public OAuth endpoints and completion behavior."""

    kind: OAuthFlowKind
    url: str
    redirect_uri: str | None = None
    completion: Completion | None = None
    parameters: Mapping[str, str] = field(default_factory=lambda: _EMPTY_MAP)
    max_polls: int | None = None
    interval: object | None = None
    max_interval: object | None = None
    prompt: str | None = None

    @classmethod
    def pkce(
        cls,
        authorize_url: str,
        redirect_uri: str,
        *,
        completion: Completion = Completion.CALLBACK_URL,
        params: Mapping[str, str] = _EMPTY_MAP,
    ) -> "OAuthFlow":
        """Declare an S256 PKCE authorization-code flow."""
        return cls(OAuthFlowKind.PKCE, authorize_url, redirect_uri, completion, MappingProxyType(dict(params)))

    @classmethod
    def device_code(
        cls,
        device_authorization_url: str,
        *,
        max_polls: int = 180,
        interval: object = None,
        max_interval: object = None,
    ) -> "OAuthFlow":
        """Declare an RFC 8628 device authorization flow."""
        return cls(
            OAuthFlowKind.DEVICE_CODE,
            device_authorization_url,
            max_polls=max_polls,
            interval=interval,
            max_interval=max_interval,
        )

    @classmethod
    def paste(cls, authorization_url: str, prompt: str) -> "OAuthFlow":
        """Declare a browser-assisted pasted-input flow."""
        return cls(OAuthFlowKind.PASTE, authorization_url, prompt=prompt)


@dataclass(frozen=True, slots=True)
class OAuthSpec:
    """Public OAuth flow data containing no credential secrets."""

    client_id: str
    token_url: str
    flow: OAuthFlow
    scopes: tuple[str, ...] = ()
    audience: str | None = None
    placement: TokenPlacement = TokenPlacement("header", "authorization", "Bearer ")
    token_params: Mapping[str, str] = field(default_factory=lambda: _EMPTY_MAP)
    refresh: RefreshBehavior = RefreshBehavior.TOKEN_ENDPOINT
    principal: PrincipalResolution | None = None


@dataclass(frozen=True, slots=True)
class AuthSpec:
    """Authentication requirements without credential values."""

    mode: AuthMode
    header: str | None = "authorization"
    prefix: str | None = "Bearer "
    query: str | None = None
    scopes: tuple[str, ...] = ()
    audience: str | None = None
    account_scope: AccountScope = AccountScope.PROVIDER
    sources: tuple[CredentialSource, ...] = (CredentialSource("stored"),)
    oauth: OAuthSpec | None = None
    signing: object | None = None


@dataclass(frozen=True, slots=True)
class ManagementSpec:
    """Provider-level management capabilities."""

    operations: frozenset[Operation] = frozenset()
    multiple_accounts: bool = False
    refresh: bool = False
    principal_quota: bool = False


@dataclass(frozen=True, slots=True)
class CompatFlags:
    """Closed route/model wire-compatibility overrides used by declarations."""

    schema_flavor: ToolSchemaFlavor | None = None


@dataclass(frozen=True, slots=True)
class RouteSpec:
    """One concrete provider endpoint and its codec/auth contract."""

    id: str
    base_url: str
    api: Api
    transport: Transport = Transport.HTTP
    auth: AuthSpec = AuthSpec(AuthMode.NONE, header=None, prefix=None, sources=())
    headers: Mapping[str, str] = field(default_factory=lambda: _EMPTY_MAP)
    region: str | None = None
    discovery: object | None = None
    trust: object | None = None
    limits: object | None = None
    compat: CompatFlags = CompatFlags()
    codec_profile: CodecProfile = CodecProfile.STANDARD
    priority: int | None = None


class Cap(StrEnum):
    """Evidence state for an unsupported or not-yet-known capability axis."""

    UNKNOWN = "unknown"
    UNSUPPORTED = "unsupported"


@dataclass(frozen=True, slots=True)
class ToolCaps:
    """Tool declaration and choice constraints from Rust ``ToolCapabilities``."""

    features: frozenset[ToolFeature] = frozenset()
    maximum_tools: int | None = None


@dataclass(frozen=True, slots=True)
class ReasoningCaps:
    """Reasoning visibility, effort, and token-budget constraints."""

    features: frozenset[str] = frozenset()
    efforts: tuple[Effort, ...] = ()
    minimum_budget_tokens: int | None = None
    maximum_budget_tokens: int | None = None


@dataclass(frozen=True, slots=True)
class PromptCacheCaps:
    """Prompt-cache retention and breakpoint constraints."""

    retention: frozenset[CacheRetention] = frozenset()
    minimum_prefix_tokens: int | None = None
    maximum_breakpoints: int | None = None


@dataclass(frozen=True, slots=True)
class ServiceTier:
    """One provider service tier and its relative scheduling priority."""

    name: str
    priority: int


@dataclass(frozen=True, slots=True)
class ServerStateCaps:
    """Provider-side conversation-state constraints."""

    continuation: bool
    expiry_evidence: bool
    fork_requires_reseed: bool


@dataclass(frozen=True, slots=True)
class LogprobCaps:
    """Token-level log-probability constraints."""

    maximum_top_logprobs: int
    prompt_logprobs: bool


@dataclass(frozen=True, slots=True)
class ChatCaps:
    """Complete chat capability axes, field-for-field with Rust."""

    roles: Cap | frozenset[str] = Cap.UNKNOWN
    mid_session_roles: Cap | frozenset[str] = Cap.UNKNOWN
    tools: Cap | ToolCaps = Cap.UNKNOWN
    structured_output: Cap | frozenset[str] = Cap.UNKNOWN
    grammar: Cap | frozenset[str] = Cap.UNKNOWN
    text_verbosity: Cap | frozenset[str] = Cap.UNKNOWN
    reasoning: Cap | ReasoningCaps = Cap.UNKNOWN
    input_modalities: Cap | frozenset[Modality] = Cap.UNKNOWN
    hosted_tools: Cap | frozenset[str] = Cap.UNKNOWN
    prompt_caching: Cap | PromptCacheCaps = Cap.UNKNOWN
    service_tiers: Cap | tuple[ServiceTier, ...] = Cap.UNKNOWN
    sampling: Cap | frozenset[str] = Cap.UNKNOWN
    safety: Cap | frozenset[str] = Cap.UNKNOWN
    determinism: Cap | frozenset[str] = Cap.UNKNOWN
    server_state: Cap | ServerStateCaps = Cap.UNKNOWN
    logprobs: Cap | LogprobCaps = Cap.UNKNOWN


@dataclass(frozen=True, slots=True)
class ThinkingSpec:
    """Provider-native reasoning controls and ordered effort ladder."""

    mode: ThinkingMode
    efforts: tuple[Effort, ...]
    default: Effort | None = None
    budgets: Mapping[Effort, int] = field(default_factory=lambda: _EMPTY_MAP)
    supports_display: bool | None = None
    suppress_when_off: bool | None = None
    requires_effort: bool | None = None

    def __post_init__(self) -> None:
        order = tuple(Effort)
        if Effort.OFF in self.efforts:
            raise ValueError("ThinkingSpec.efforts must not advertise OFF")
        positions = tuple(order.index(effort) for effort in self.efforts)
        if any(left >= right for left, right in zip(positions, positions[1:])):
            raise ValueError("ThinkingSpec.efforts must be strictly ascending")
        if self.default is not None and self.default not in self.efforts:
            raise ValueError("ThinkingSpec.default must appear in efforts")


@dataclass(frozen=True, slots=True)
class Cost:
    """Exact public price inputs compiled into integer nano-USD components."""

    input: object = 0
    output: object = 0
    cache_read: object = 0
    cache_write: object = 0
    image: object = 0
    video_second: object = 0
    audio_second: object = 0
    char_input: object = 0
    request: object = 0
    tiers: tuple[CostTier, ...] = ()

    @classmethod
    def free(cls) -> "Cost":
        """Return a zero-price schedule."""
        return cls()
@dataclass(frozen=True, slots=True)
class CostTier:
    """Replacement pricing activated above a prompt-token threshold."""

    prompt_tokens_above: int
    cost: Cost




@dataclass(frozen=True, slots=True)
class ContextSpec:
    """How canonical conversation history reaches a route."""

    mode: str
    retention: frozenset[CacheRetention] = frozenset()
    min_prefix_tokens: int | None = None
    max_breakpoints: int | None = None

    @classmethod
    def replay(cls) -> "ContextSpec":
        """Resend canonical history on every request."""
        return cls("replay")

    @classmethod
    def prefix_cache(
        cls,
        *,
        retention: frozenset[CacheRetention],
        min_prefix_tokens: int | None = None,
        max_breakpoints: int | None = None,
    ) -> "ContextSpec":
        """Declare deterministic prefix-cache behavior."""
        return cls("prefix_cache", retention, min_prefix_tokens, max_breakpoints)


@dataclass(frozen=True, slots=True)
class ModelSpec:
    """One normalized selectable model and its route/capability facts."""

    id: str
    display_name: str
    routes: tuple[str, ...]
    wire_ids: Mapping[str, str] = field(default_factory=lambda: _EMPTY_MAP)
    operations: frozenset[Operation] = frozenset({Operation.CHAT})
    family: str | None = None
    context_window: int | None = None
    max_input_tokens: int | None = None
    max_output_tokens: int | None = None
    max_batch: int | None = None
    input_modalities: frozenset[Modality] = frozenset({Modality.TEXT})
    thinking: ThinkingSpec | None = None
    thinking_routing: object | None = None
    cost: Cost = Cost()
    premium_multiplier: object | None = None
    compat: CompatFlags = CompatFlags()
    context: ContextSpec = ContextSpec("replay")
    availability: object | None = None
    context_promotion_target: str | None = None
    remote_compaction: object | None = None
    chat: ChatCaps = ChatCaps()
    embeddings: object | None = None
    image: object | None = None
    video: object | None = None
    speech: object | None = None
    transcription: object | None = None
    realtime: object | None = None
    search: object | None = None
    tokenization: object | None = None

@dataclass(frozen=True, slots=True)
class ProviderSpec:
    """Complete pure-data provider declaration compiled by the Rust catalog."""

    id: str
    name: str
    routes: tuple[RouteSpec, ...]
    models: tuple[ModelSpec, ...] = ()
    management: ManagementSpec = ManagementSpec()
    discovery_defaults: object | None = None
    mapping: object = "concrete"
    aliases: tuple[str, ...] = ()

def provider(spec: ProviderSpec) -> Callable[[_T], _T]:
    """Record one provider declaration during IMPORT without performing I/O."""
    if not isinstance(spec, ProviderSpec):
        raise TypeError("@omp.provider requires a ProviderSpec")

    def decorate(implementation: _T) -> _T:
        registry.register_provider(spec.id, spec, implementation)
        implementation.__omp_provider_spec__ = spec
        return implementation

    return decorate


__all__ = (
    "AccountScope", "Api", "AuthMode", "AuthSpec", "CacheRetention", "Cap", "ChatCaps",
    "CodecProfile", "CompatFlags", "Completion", "ContextSpec", "Cost", "CostTier",
    "CredentialSource", "Effort", "LogprobCaps", "ManagementSpec", "Modality", "ModelSpec",
    "OAuthFlow", "OAuthFlowKind", "OAuthSpec", "Operation", "PrincipalResolution",
    "PromptCacheCaps", "ProviderSpec", "ReasoningCaps", "RefreshBehavior", "RouteSpec",
    "ServerStateCaps", "ServiceTier", "ThinkingMode", "ThinkingSpec", "TokenPlacement",
    "ToolCaps", "ToolFeature", "ToolSchemaFlavor", "Transport", "provider",
)
