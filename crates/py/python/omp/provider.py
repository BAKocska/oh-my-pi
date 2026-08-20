"""Pure provider-catalog declarations.

This module mirrors the public source vocabulary compiled into
``crates/llm-catalog``.  Importing it only constructs immutable Python values;
provider registration is recorded in the local declaration table and performs
no credential, network, filesystem, CONTROL, or DATA access.
"""
from __future__ import annotations

from ipaddress import ip_address
from urllib.parse import urlsplit
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from decimal import Decimal
from enum import StrEnum
from types import MappingProxyType
from typing import Any, Protocol, TypeVar

from _omp import BlobRef, Duration, Secret

from ._registry import registry
from ._errors import NotWiredError

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
    SEARCH_HTTP = "search_http"
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


class Role(StrEnum):
    """Identify a canonical chat-message role."""

    SYSTEM = "system"
    DEVELOPER = "developer"
    USER = "user"
    ASSISTANT = "assistant"
    TOOL = "tool"


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
    def application_default(
        cls,
        *,
        api_key_env: str = "GOOGLE_API_KEY",
        project_env: str = "GOOGLE_CLOUD_PROJECT",
        location_env: str = "GOOGLE_CLOUD_LOCATION",
    ) -> "CredentialSource":
        """Resolve Google application-default credentials through the host ADC chain."""
        names = {
            "api_key_env": api_key_env,
            "project_env": project_env,
            "location_env": location_env,
        }
        if any(not isinstance(value, str) or not value for value in names.values()):
            raise ValueError("ADC environment names must be non-empty strings")
        return cls("application_default", options=MappingProxyType(names))

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

class DiscoveryKind(StrEnum):
    """Select the response family used for remote model discovery."""

    OPENAI_MODELS = "openai_models"
    GOOGLE_MODELS = "google_models"
    OLLAMA_TAGS = "ollama_tags"
    ACCOUNT_MODELS = "account_models"
    SPECIALIZED = "specialized"


@dataclass(frozen=True, slots=True)
class Pagination:
    """Describe how a remote model listing advances between pages."""

    kind: str
    query_parameter: str | None = None
    first_page: int | None = None

    @classmethod
    def single_page(cls) -> "Pagination":
        """Return a pagination policy whose first response is complete."""
        return cls("single_page")

    @classmethod
    def cursor(cls, query_parameter: str) -> "Pagination":
        """Pass a response cursor through the named query parameter."""
        return cls("cursor", query_parameter=query_parameter)

    @classmethod
    def page_number(
        cls, query_parameter: str, *, first_page: int = 1
    ) -> "Pagination":
        """Pass an increasing page number through the named query parameter."""
        return cls("page_number", query_parameter=query_parameter, first_page=first_page)


@dataclass(frozen=True, slots=True)
class DiscoverySpec:
    """Configure one route's remote model-list operation."""

    kind: DiscoveryKind
    path: str
    label: str
    pagination: Pagination = Pagination.single_page()
    authoritative: bool = False
    interval: Duration | None = None

    def __post_init__(self) -> None:
        """Reject periodic discovery faster than the daemon scheduling floor."""
        if self.interval is not None:
            if not isinstance(self.interval, Duration):
                raise TypeError("DiscoverySpec.interval must be Duration or None")
            if self.interval < Duration("5s"):
                raise ValueError("DiscoverySpec.interval must be at least 5s")


class RedirectTrust(StrEnum):
    """Constrain redirects relative to a route's trusted origin."""

    DENY = "deny"
    SAME_ORIGIN = "same_origin"
    PUBLIC_ONLY = "public_only"


def _origin(url: str) -> tuple[str, str | None]:
    parsed = urlsplit(url)
    if parsed.scheme in {"unix", "http+unix"} or (
        not parsed.scheme and url.startswith("/")
    ):
        return url, None
    if not parsed.scheme or not parsed.netloc:
        raise ValueError("route base_url must be an absolute URL or Unix socket path")
    return f"{parsed.scheme.lower()}://{parsed.netloc}", parsed.hostname


def _is_loopback(url: str) -> bool:
    parsed = urlsplit(url)
    if parsed.scheme in {"unix", "http+unix"} or (
        not parsed.scheme and url.startswith("/")
    ):
        return True
    host = parsed.hostname
    if host is None:
        return False
    lowered = host.rstrip(".").lower()
    if lowered == "localhost" or lowered.endswith(".localhost"):
        return True
    try:
        return ip_address(lowered).is_loopback
    except ValueError:
        return False


@dataclass(frozen=True, slots=True)
class TrustDomain:
    """Declare the origin and credential-forwarding boundary for a route."""

    origin: str
    redirects: RedirectTrust = RedirectTrust.SAME_ORIGIN
    allow_plaintext: bool = False

    def __post_init__(self) -> None:
        """Reject plaintext trust for anything except loopback and Unix sockets."""
        if self.allow_plaintext and self.origin and not _is_loopback(self.origin):
            raise ValueError("plaintext trust is limited to loopback hosts and Unix sockets")

    @classmethod
    def https(
        cls, *, redirects: RedirectTrust = RedirectTrust.SAME_ORIGIN
    ) -> "TrustDomain":
        """Derive a TLS-required trust origin from the route base URL."""
        return cls("", redirects=redirects)

    @classmethod
    def loopback(
        cls, *, redirects: RedirectTrust = RedirectTrust.SAME_ORIGIN
    ) -> "TrustDomain":
        """Derive a trust origin while permitting loopback plaintext."""
        return cls("", redirects=redirects, allow_plaintext=True)


@dataclass(frozen=True, slots=True)
class RouteLimits:
    """Subtract route-specific operations and token limits from model capabilities."""

    operations: frozenset[Operation] | None = None
    max_context_tokens: int | None = None
    max_output_tokens: int | None = None
    disable_server_state: bool = False
    disable_prompt_caching: bool = False




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
    discovery: DiscoverySpec | None = None
    trust: TrustDomain = TrustDomain.https()
    limits: RouteLimits = RouteLimits()
    compat: CompatFlags = CompatFlags()
    codec_profile: CodecProfile = CodecProfile.STANDARD
    priority: int | None = None

    def __post_init__(self) -> None:
        """Resolve and validate the route's declared trust boundary."""
        route_origin, _ = _origin(self.base_url)
        trust = self.trust
        if not isinstance(trust, TrustDomain):
            raise TypeError("RouteSpec.trust must be TrustDomain")
        if trust.allow_plaintext:
            if not _is_loopback(self.base_url):
                raise ValueError(
                    "TrustDomain.loopback() requires a loopback host or Unix socket path"
                )
        elif urlsplit(self.base_url).scheme.lower() != "https":
            raise ValueError("plaintext routes require TrustDomain.loopback()")
        if not trust.origin:
            object.__setattr__(
                self,
                "trust",
                TrustDomain(route_origin, trust.redirects, trust.allow_plaintext),
            )
        elif route_origin != trust.origin:
            raise ValueError("RouteSpec.base_url is outside its TrustDomain origin")


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

    roles: Cap | frozenset[Role] = Cap.UNKNOWN
    mid_session_roles: Cap | frozenset[Role] = Cap.UNKNOWN
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

class Availability(StrEnum):
    """Describe the selectable state assigned to a discovered model."""

    UNSPECIFIED = "unspecified"
    AVAILABLE = "available"
    LOGIN_REQUIRED = "login_required"
    BLOCKED = "blocked"
    DISABLED = "disabled"


class Confidence(StrEnum):
    """Describe the evidence confidence assigned to discovered model facts."""

    VERIFIED = "verified"
    DECLARED = "declared"
    INFERRED = "inferred"
    UNKNOWN = "unknown"


@dataclass(frozen=True, slots=True)
class DiscoveryDefaults:
    """Provide conservative facts for newly discovered models."""

    routes: tuple[str, ...]
    cost: Cost = Cost.free()
    context_window: int | None = None
    max_output_tokens: int | None = None
    operations: frozenset[Operation] = frozenset({Operation.CHAT})
    availability: Availability = Availability.AVAILABLE
    confidence: Confidence = Confidence.INFERRED






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


class ImageFeature(StrEnum):
    """Closed image-operation capability vocabulary."""

    GENERATE = "generate"
    EDIT = "edit"
    MASK = "mask"
    REFERENCE_IMAGES = "reference_images"
    TRANSPARENCY = "transparency"


class ImageFormat(StrEnum):
    """Closed generated-image encoding vocabulary."""

    PNG = "png"
    JPEG = "jpeg"
    WEBP = "webp"


@dataclass(frozen=True, slots=True)
class Dimensions:
    """Raster width and height in pixels."""

    width: int
    height: int

    def __post_init__(self) -> None:
        """Reject non-positive or non-integral raster dimensions."""
        if (
            isinstance(self.width, bool)
            or not isinstance(self.width, int)
            or self.width <= 0
            or isinstance(self.height, bool)
            or not isinstance(self.height, int)
            or self.height <= 0
        ):
            raise ValueError("image dimensions must be positive integers")


@dataclass(frozen=True, slots=True)
class ImageCaps:
    """Image operations, output dimensions, and encodings supported by a model."""

    features: frozenset[ImageFeature]
    sizes: tuple[Dimensions, ...]
    formats: frozenset[ImageFormat]
    max_references: int | None = None


@dataclass(frozen=True, slots=True)
class ImageRequest:
    """Typed request for host-routed image generation."""

    prompt: str
    dimensions: Dimensions
    format: ImageFormat
    count: int = 1


@dataclass(frozen=True, slots=True)
class ImageResult:
    """Blob-backed generated images and their settled nano-USD cost receipt."""

    images: tuple[BlobRef, ...]
    cost_nanos_usd: int


@dataclass(frozen=True, slots=True)
class CatalogAlias:
    """Canonical model alias with review rationale and provenance."""

    alias: str
    target: str
    rationale: str
    provenance: str


@dataclass(frozen=True, slots=True)
class ScopedAlias:
    """One canonical alias visible only inside a provider namespace."""

    provider: str
    definition: CatalogAlias


@dataclass(frozen=True, slots=True)
class ModelPatch:
    """Field-granular changes to an existing catalog model."""

    class_: str | None = None
    display_name: str | None = None
    wire_ids: Mapping[str, str] | None = None
    routes: tuple[str, ...] | None = None
    capabilities: object | None = None
    limits: object | None = None
    thinking: object | None = None
    thinking_routing: object | None = None
    wire_policy: object | None = None
    context: ContextSpec | None = None
    pricing: Cost | None = None
    availability: Availability | None = None
    context_promotion_target: str | None = None
    remote_compaction: object | None = None
    premium_multiplier_millionths: int | None = None
    updated_at_ms: int | None = None
    blocked_until_ms: int | None = None
    deprecated: bool | None = None


@dataclass(frozen=True, slots=True)
class ModelOverlay:
    """One model addition or field-granular patch in an overlay declaration."""

    selector: ModelRef
    added: ModelSpec | None = None
    patch: ModelPatch = ModelPatch()


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
    image: ImageCaps | None = None
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
    discovery_defaults: DiscoveryDefaults | None = None
    mapping: object = "concrete"
    aliases: tuple[ScopedAlias, ...] = ()
    model_overlays: tuple[ModelOverlay, ...] = ()

    def __post_init__(self) -> None:
        """Reject conflicting model patches and aliases before registration."""
        selectors: set[tuple[str, str]] = set()
        for overlay in self.model_overlays:
            if not isinstance(overlay, ModelOverlay):
                raise TypeError("ProviderSpec.model_overlays must contain ModelOverlay values")
            if not isinstance(overlay.selector, ModelRef):
                raise TypeError("ModelOverlay.selector must be a ModelRef")
            key = (overlay.selector.provider, overlay.selector.model)
            if overlay.selector.provider != self.id:
                raise ValueError("model overlay selector must use the declaring provider")
            if key in selectors:
                raise ValueError(f"duplicate model overlay for {key[0]}/{key[1]}")
            selectors.add(key)

        aliases: dict[tuple[str, str], str] = {}
        for scoped in self.aliases:
            if not isinstance(scoped, ScopedAlias):
                raise TypeError("ProviderSpec.aliases must contain ScopedAlias values")
            if scoped.provider != self.id:
                raise ValueError("scoped alias must use the declaring provider")
            key = (scoped.provider, scoped.definition.alias)
            target = scoped.definition.target
            previous = aliases.get(key)
            if previous is not None and previous != target:
                raise ValueError(
                    f"alias {key[0]}/{key[1]} targets both {previous!r} and {target!r}"
                )
            aliases[key] = target

class ProviderHandle:
    """Refer to one provider declaration and its host-owned CONTROL operations."""

    __slots__ = ("_spec", "_priority", "_extends", "_replaces")

    def __init__(
        self,
        spec: ProviderSpec,
        *,
        priority: int = 0,
        extends: str | None = None,
        replaces: str | None = None,
    ) -> None:
        """Create a handle for a validated import-time provider declaration."""
        self._spec = spec
        self._priority = priority
        self._extends = extends
        self._replaces = replaces

    @property
    def id(self) -> str:
        """Return the stable provider identifier."""
        return self._spec.id

    def __call__(self, implementation: _T) -> _T:
        """Bind a provider-scoped implementation class to this declaration."""
        registry.register_provider(self.id, self._spec, implementation)
        implementation.__omp_provider_spec__ = self._spec
        implementation.__omp_provider_priority__ = self._priority
        implementation.__omp_provider_extends__ = self._extends
        implementation.__omp_provider_replaces__ = self._replaces
        return implementation

    async def retract(self) -> None:
        """Remove this provider declaration through the host CONTROL bridge."""
        await _provider_control_request("omp.provider.retract", provider=self.id)

    async def replace(self, spec: ProviderSpec) -> None:
        """Atomically replace this provider declaration through CONTROL."""
        if not isinstance(spec, ProviderSpec):
            raise TypeError("ProviderHandle.replace requires a ProviderSpec")
        await _provider_control_request("omp.provider.replace", provider=self.id, spec=spec)

    async def models(self) -> tuple[ModelSpec, ...]:
        """Return the provider's resolved model cards through CONTROL."""
        return await _provider_control_request("omp.provider.models", provider=self.id)

    async def is_authenticated(self) -> bool:
        """Return whether an eligible provider principal is available."""
        return await _provider_control_request(
            "omp.provider.is_authenticated", provider=self.id
        )

    async def request(
        self, operation: Operation, request: ImageRequest
    ) -> ImageResult:
        """Route one typed provider operation through the host CONTROL and DATA arm."""
        if operation is not Operation.GENERATE_IMAGE:
            raise ValueError("ProviderHandle.request only freezes GENERATE_IMAGE")
        if not isinstance(request, ImageRequest):
            raise TypeError("GENERATE_IMAGE requires an ImageRequest")
        return await _provider_control_request(
            "omp.provider.request",
            provider=self.id,
            operation=operation,
            request=request,
        )


async def _provider_control_request(operation: str, /, **arguments: object) -> Any:
    from . import _control_backend, _control_request

    if _control_backend.get() is None:
        raise NotWiredError(f"{operation} CONTROL dispatch is not wired")
    return await _control_request(operation, **arguments)

@dataclass(frozen=True, slots=True)
class ModelRef:
    """Identify one provider model and API family."""

    provider: str
    api: str
    model: str


@dataclass(frozen=True, slots=True)
class RouteRef:
    """Identify one selected provider route."""

    provider: str
    route: str


class ErrorKind(StrEnum):
    """Classify a stable provider failure for policy decisions."""

    CANCELLED = "cancelled"
    DEADLINE_EXCEEDED = "deadline_exceeded"
    BUDGET_EXHAUSTED = "budget_exhausted"
    POLICY_BUFFER_EXCEEDED = "policy_buffer_exceeded"
    DNS = "dns"
    TLS = "tls"
    CONNECTIVITY = "connectivity"
    PROTOCOL = "protocol"
    STREAM_CORRUPTION = "stream_corruption"
    AUTHENTICATION = "authentication"
    CREDENTIAL_STORAGE_UNAVAILABLE = "credential_storage_unavailable"
    AUTHORIZATION = "authorization"
    ACCOUNT_DISABLED = "account_disabled"
    RATE_LIMITED = "rate_limited"
    QUOTA_EXHAUSTED = "quota_exhausted"
    PAYMENT_REQUIRED = "payment_required"
    INVALID_REQUEST = "invalid_request"
    TARGET_NOT_FOUND = "target_not_found"
    CAPABILITY_UNKNOWN = "capability_unknown"
    CODEC_MISMATCH = "codec_mismatch"
    ROUTE_UNAVAILABLE = "route_unavailable"
    STALE_PLAN = "stale_plan"
    REPLAY_REQUIRED = "replay_required"
    STAGING_REQUIRED = "staging_required"
    CAPABILITY_MISMATCH = "capability_mismatch"
    PROVIDER_CONTRACT_MISMATCH = "provider_contract_mismatch"
    CONTEXT_OVERFLOW = "context_overflow"
    CONTENT_FILTER = "content_filter"
    SAFETY_REFUSAL = "safety_refusal"
    MALFORMED_MODEL_OUTPUT = "malformed_model_output"
    STRUCTURED_OUTPUT_FAILURE = "structured_output_failure"
    TOOL_NON_COMPLIANCE = "tool_non_compliance"
    REPEATED_REASONING = "repeated_reasoning"
    REPEATED_TOOL_CALL = "repeated_tool_call"
    EMPTY_COMPLETION = "empty_completion"
    EMPTY_OUTPUT = "empty_output"
    SESSION_EXPIRED = "session_expired"
    SESSION_CONFLICT = "session_conflict"
    LOCAL_MODEL_UNAVAILABLE = "local_model_unavailable"
    RESOURCE_EXHAUSTED = "resource_exhausted"
    NATIVE_REQUEST_REJECTED = "native_request_rejected"
    INTERNAL_INVARIANT = "internal_invariant"


class Retryability(StrEnum):
    """Name the safe recovery lane for a classified attempt."""

    NEVER = "never"
    SAME_ROUTE = "same_route"
    AFTER_REPAIR = "after_repair"
    AFTER_CREDENTIAL = "after_credential"
    AFTER_DELAY = "after_delay"
    UNSPECIFIED = "unspecified"


@dataclass(frozen=True, slots=True)
class ProviderError:
    """Describe a structured failure from one provider attempt."""

    provider: str
    route: str
    model: str
    operation: Operation
    kind: ErrorKind
    retryability: Retryability
    status: int | None
    retry_after: Duration | None
    attempt: int
    committed: bool
    message: str
    identity: str | None


class FailoverKind(StrEnum):
    """Select the recovery action represented by a failover verdict."""

    RETRY = "retry"
    REFRESH_CREDENTIAL = "refresh_credential"
    ROTATE_ACCOUNT = "rotate_account"
    RESELECT_ROUTE = "reselect_route"
    SWITCH_MODEL = "switch_model"
    RESEED_SESSION = "reseed_session"
    SEMANTIC_RETRY = "semantic_retry"
    FAIL = "fail"


@dataclass(frozen=True, slots=True)
class Failover:
    """Request one typed recovery action for a provider failure."""

    kind: FailoverKind
    after: Duration | None = None
    cooldown: Duration | None = None
    route: str | None = None
    target: str | None = None
    reason: str | None = None

    @staticmethod
    def retry(*, after: Duration | None = None, cooldown: Duration | None = None) -> Failover:
        """Retry the same attempt, optionally after a delay."""
        return Failover(FailoverKind.RETRY, after=after, cooldown=cooldown)

    @staticmethod
    def refresh_credential() -> Failover:
        """Refresh the current credential before retrying."""
        return Failover(FailoverKind.REFRESH_CREDENTIAL)

    @staticmethod
    def rotate_account(
        successor: str, *, cooldown: Duration | None = None
    ) -> Failover:
        """Rotate to the named successor identity before retrying."""
        if not isinstance(successor, str) or not successor:
            raise ValueError("successor identity must be a non-empty string")
        return Failover(
            FailoverKind.ROTATE_ACCOUNT, cooldown=cooldown, target=successor
        )

    @staticmethod
    def reselect_route(
        *, route: str | None = None, cooldown: Duration | None = None
    ) -> Failover:
        """Reselect a route, optionally preferring one route."""
        return Failover(FailoverKind.RESELECT_ROUTE, cooldown=cooldown, route=route)

    @staticmethod
    def switch_model(target: str, *, cooldown: Duration | None = None) -> Failover:
        """Switch to a fully qualified model target."""
        return Failover(FailoverKind.SWITCH_MODEL, cooldown=cooldown, target=target)

    @staticmethod
    def reseed_session() -> Failover:
        """Reseed provider-side session state before retrying."""
        return Failover(FailoverKind.RESEED_SESSION)

    @staticmethod
    def semantic_retry() -> Failover:
        """Retry through the bounded semantic-repair lane."""
        return Failover(FailoverKind.SEMANTIC_RETRY)

    @staticmethod
    def fail(reason: str | None = None) -> Failover:
        """Fail without further recovery."""
        return Failover(FailoverKind.FAIL, reason=reason)


class ModelFallback(StrEnum):
    """Choose selection-time behavior for an unavailable pinned model."""

    DENY = "deny"
    PARENT = "parent"
    CHAIN = "chain"


class AuthMethod(StrEnum):
    """Identify a provider login method."""

    API_KEY = "api_key"
    OAUTH_PKCE = "oauth_pkce"
    OAUTH_DEVICE = "oauth_device"
    OAUTH_PASTE = "oauth_paste"
    AWS_PROFILE = "aws_profile"
    ADC = "adc"
    SESSION = "session"


class LoginUi(Protocol):
    """Provide reentrant user interaction during provider login."""

    async def prompt(self, text: str) -> str:
        """Prompt for a text value."""
        ...

    async def select(self, text: str, options: Sequence[str]) -> str:
        """Select one value from an ordered option list."""
        ...

    async def open_url(self, url: str) -> None:
        """Open a login URL for the user."""
        ...

    async def notify(self, text: str, level: str) -> None:
        """Show a login notification."""
        ...


@dataclass(frozen=True, slots=True)
class LoginRequest:
    """Request an extension-owned provider login flow."""

    provider: str
    method: AuthMethod
    ui: LoginUi


class RefreshReason(StrEnum):
    """Explain why a credential refresh was requested."""

    EXPIRING = "expiring"
    REJECTED_401 = "rejected_401"
    MANUAL = "manual"
    SCHEDULED = "scheduled"


@dataclass(frozen=True, slots=True)
class RefreshRequest:
    """Provide ephemeral material for one serialized credential refresh."""

    provider: str
    identity: str | None
    refresh_token: Secret | None
    expires_at_ms: int | None
    props: Mapping[str, int | str | bool]
    reason: RefreshReason


class Signer(Protocol):
    """Perform keyed signing operations without exposing key material."""

    async def hmac_sha256(self, message: bytes) -> bytes:
        """Compute an HMAC-SHA256 digest."""
        ...

    async def jwt(self, claims: Mapping[str, object], algorithm: str) -> str:
        """Sign a JSON Web Token."""
        ...

    async def attest(self, challenge: bytes) -> bytes:
        """Produce a platform attestation response."""
        ...


@dataclass(frozen=True, slots=True)
class SignRequest:
    """Describe one provider request requiring extension-owned signing."""

    provider: str
    route: str
    method: str
    url: str
    headers: Mapping[str, str]
    body_sha256: bytes
    signer: Signer


@dataclass(frozen=True, slots=True)
class Signature:
    """Carry signer-produced headers and query parameters."""

    headers: Mapping[str, str]
    query: Mapping[str, str] = _EMPTY_MAP


class Fallback(StrEnum):
    """Choose behavior when a provider cannot honor an intent."""

    UNSPECIFIED = "unspecified"
    ERROR = "error"
    IGNORE = "ignore"
    EMULATE = "emulate"


class IntentKind(StrEnum):
    """Identify one negotiated inference capability intent."""

    STRICT = "strict"
    GRAMMAR = "grammar"
    FORCE_CALL = "force_call"
    SERVICE_TIER = "service_tier"
    VERBOSITY = "verbosity"
    CACHE_RETENTION = "cache_retention"
    REASONING = "reasoning"
    SAFETY = "safety"
    DETERMINISM = "determinism"
    HOSTED_TOOL = "hosted_tool"


@dataclass(frozen=True, slots=True)
class Intent:
    """Declare a negotiated inference capability request."""

    kind: IntentKind
    on_unsupported: Fallback = Fallback.UNSPECIFIED
    priority: int = 0
    payload: object = None


@dataclass(frozen=True, slots=True)
class RequestDraft:
    """Expose bounded request metadata to a pre-encoding hook."""

    provider: str
    route: str
    model: str
    operation: Operation
    scalars: Mapping[str, int | float | str | bool]
    headers: Mapping[str, str]
    intents: tuple[Intent, ...]
    message_count: int
    approx_prompt_tokens: int | None


@dataclass(frozen=True, slots=True)
class RequestMutation:
    """Describe a shallow request-body and header mutation."""

    body: Mapping[str, object] = _EMPTY_MAP
    headers: Mapping[str, str | None] = _EMPTY_MAP
    timeout: Duration | None = None


class DiscoveryTrigger(StrEnum):
    """Identify what initiated provider model discovery."""

    SESSION_START = "session_start"
    INTERVAL = "interval"
    MANUAL = "manual"
    POST_LOGIN = "post_login"


@dataclass(frozen=True, slots=True)
class DiscoveryQuery:
    """Request one page of provider model discovery."""

    provider: str
    route: str
    cursor: str | None
    page_size: int | None
    trigger: DiscoveryTrigger


@dataclass(frozen=True, slots=True)
class DiscoveryPage:
    """Return one page of dynamically discovered models."""

    models: tuple[ModelSpec, ...]
    next_cursor: str | None = None
    authoritative: bool = False


@dataclass(frozen=True, slots=True)
class SearchQuery:
    """Request one page from a provider-backed web search."""

    provider: str
    query: str
    count: int
    offset: int | None = None


@dataclass(frozen=True, slots=True)
class SearchResult:
    """Represent one normalized ranked web search result."""

    title: str
    url: str
    snippet: str
    rank: int


@dataclass(frozen=True, slots=True)
class SearchPage:
    """Return one normalized page of provider-backed search results."""

    results: tuple[SearchResult, ...]
    next_offset: int | None = None


class UsageScope(StrEnum):
    """Select the provider usage scope to query."""

    CURRENT = "current"
    BILLING = "billing"
    RATE_LIMIT = "rate_limit"
    ALL = "all"


class UsageUnit(StrEnum):
    """Identify the unit used by a provider usage window."""

    REQUESTS = "requests"
    TOKENS = "tokens"
    PREMIUM_UNITS = "premium_units"
    NANOS_USD = "nanos_usd"


@dataclass(frozen=True, slots=True)
class UsageQuery:
    """Request provider usage for one credential identity."""

    provider: str
    identity: str | None
    scope: UsageScope
    allow_stale: bool


@dataclass(frozen=True, slots=True)
class UsageWindow:
    """Describe one provider quota or billing window."""

    id: str
    used: int | None = None
    limit: int | None = None
    fraction: Decimal | None = None
    resets_at_ms: int | None = None
    unit: UsageUnit = UsageUnit.REQUESTS


@dataclass(frozen=True, slots=True)
class UsageReport:
    """Aggregate provider usage windows and account balance metadata."""

    windows: tuple[UsageWindow, ...]
    balance_nanos_usd: int | None = None
    plan: str | None = None
    observed_at_ms: int | None = None


class CredentialKind(StrEnum):
    """Identify the material carried by a provider credential."""

    API_KEY = "api_key"
    BEARER = "bearer"
    OAUTH = "oauth"
    AWS = "aws"
    SESSION = "session"


@dataclass(frozen=True, slots=True)
class Credential:
    """Carry provider credential material returned by login or refresh."""

    kind: CredentialKind
    secret: Secret
    refresh_token: Secret | None = None
    expires_at_ms: int | None = None
    identity: str | None = None
    props: Mapping[str, int | str | bool] = _EMPTY_MAP


def provider(
    spec: ProviderSpec,
    /,
    *,
    priority: int = 0,
    extends: str | None = None,
    replaces: str | None = None,
) -> ProviderHandle:
    """Return the declaration handle used directly or as a class decorator."""
    if not isinstance(spec, ProviderSpec):
        raise TypeError("omp.provider requires a ProviderSpec")
    if isinstance(priority, bool) or not isinstance(priority, int):
        raise TypeError("provider priority must be an integer")
    if extends is not None and (not isinstance(extends, str) or not extends):
        raise TypeError("provider extends must be a non-empty provider id")
    if spec.model_overlays and extends is None:
        raise ValueError("model overlays require provider(..., extends=...)")
    return ProviderHandle(
        spec, priority=priority, extends=extends, replaces=replaces
    )


__all__ = (
    "AccountScope", "Api", "AuthMethod", "AuthMode", "AuthSpec", "Availability",
    "CacheRetention", "Cap", "CatalogAlias", "ChatCaps", "CodecProfile", "CompatFlags",
    "Completion", "Confidence", "ContextSpec", "Cost", "CostTier", "Credential", "CredentialKind",
    "CredentialSource", "Dimensions", "DiscoveryDefaults", "DiscoveryKind", "DiscoveryPage",
    "DiscoveryQuery", "DiscoverySpec", "DiscoveryTrigger", "Effort", "ErrorKind", "Failover",
    "FailoverKind", "ImageCaps", "ImageFeature", "ImageFormat", "ImageRequest", "ImageResult",
    "Fallback", "Intent", "IntentKind", "LoginRequest", "LoginUi", "LogprobCaps",
    "ManagementSpec", "Modality", "ModelFallback", "ModelOverlay", "ModelPatch", "ModelRef",
    "ModelSpec", "OAuthFlow",
    "OAuthFlowKind", "OAuthSpec", "Operation", "Pagination", "PrincipalResolution",
    "PromptCacheCaps", "ProviderError", "ProviderHandle", "ProviderSpec", "ReasoningCaps",
    "RedirectTrust", "RefreshBehavior", "RefreshReason", "RefreshRequest", "RequestDraft",
    "RequestMutation", "Retryability", "Role", "RouteLimits", "RouteRef", "RouteSpec",
    "ScopedAlias",
    "SearchPage", "SearchQuery", "SearchResult", "ServerStateCaps", "ServiceTier",
    "SignRequest", "Signature", "Signer", "ThinkingMode", "ThinkingSpec", "TokenPlacement",
    "ToolCaps", "ToolFeature", "ToolSchemaFlavor", "Transport", "TrustDomain", "UsageQuery",
    "UsageReport", "UsageScope", "UsageUnit", "UsageWindow", "provider",
)
