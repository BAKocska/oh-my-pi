from __future__ import annotations

from collections.abc import Mapping, Sequence
from decimal import Decimal, InvalidOperation

import omp
from omp import (
    Api,
    AuthMode,
    AuthSpec,
    Cost,
    CredentialSource,
    DiscoveryDefaults,
    DiscoveryKind,
    DiscoverySpec,
    Duration,
    ManagementSpec,
    ModelSpec,
    Operation,
    ProviderSpec,
    RouteSpec,
)
from omp.provider import DiscoveryPage, DiscoveryQuery

_DEFAULT_BASE_URL = "http://127.0.0.1:4000/v1"
_ROUTE = "gateway"
_MILLION = Decimal(1_000_000)

_LITELLM_SPEC = ProviderSpec(
    id="litellm",
    name="LiteLLM Gateway",
    management=ManagementSpec(operations=frozenset({Operation.DISCOVER_MODELS})),
    discovery_defaults=DiscoveryDefaults(
        routes=(_ROUTE,),
        operations=frozenset({Operation.CHAT}),
    ),
    routes=(
        RouteSpec(
            id=_ROUTE,
            base_url=_DEFAULT_BASE_URL,
            api=Api.OPENAI_CHAT,
            auth=AuthSpec(
                mode=AuthMode.BEARER,
                header="authorization",
                prefix="Bearer ",
                sources=(CredentialSource.stored(),),
            ),
            discovery=DiscoverySpec(
                kind=DiscoveryKind.SPECIALIZED,
                path="/model/info",
                label="LiteLLM model info",
                authoritative=False,
                interval=Duration("5m"),
            ),
        ),
    ),
)


@omp.provider(_LITELLM_SPEC)
class LiteLLMGateway:
    """Declare an OpenAI-compatible LiteLLM gateway and stored-key authentication."""


def _positive_int(value: object) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ValueError("token limits must be positive integers")
    return value


def _per_million(value: object) -> str:
    if isinstance(value, bool):
        raise ValueError("token prices must be non-negative decimal numbers")
    try:
        price = Decimal(str(value))
    except (InvalidOperation, ValueError) as error:
        raise ValueError("token prices must be non-negative decimal numbers") from error
    if not price.is_finite() or price < 0:
        raise ValueError("token prices must be non-negative decimal numbers")
    return format(price * _MILLION, "f")


def _cost(info: Mapping[str, object]) -> Cost:
    fields = {
        "input": "input_cost_per_token",
        "output": "output_cost_per_token",
        "cache_read": "cache_read_input_token_cost",
        "cache_write": "cache_creation_input_token_cost",
    }
    reported = {
        destination: _per_million(info[source])
        for destination, source in fields.items()
        if info.get(source) is not None
    }
    return Cost(**reported) if reported else Cost.free()


def _model_rows(payload: object, route: str = _ROUTE) -> tuple[ModelSpec, ...]:
    if not isinstance(payload, Mapping):
        raise ValueError("LiteLLM model-info response must be an object")
    raw_rows = payload.get("data")
    if not isinstance(raw_rows, Sequence) or isinstance(raw_rows, (str, bytes, bytearray)):
        raise ValueError("LiteLLM model-info data must be a list")

    models: list[ModelSpec] = []
    seen: set[str] = set()
    for raw in raw_rows:
        if not isinstance(raw, Mapping):
            raise ValueError("each LiteLLM model-info row must be an object")
        model_name = raw.get("model_name")
        if not isinstance(model_name, str) or not model_name.strip():
            raise ValueError("each LiteLLM model-info row needs a model_name")
        model_name = model_name.strip()
        if model_name in seen:
            continue
        seen.add(model_name)

        raw_info = raw.get("model_info")
        if raw_info is None:
            info: Mapping[str, object] = {}
        elif isinstance(raw_info, Mapping):
            info = raw_info
        else:
            raise ValueError("LiteLLM model_info must be an object")

        models.append(
            ModelSpec(
                id=model_name,
                display_name=model_name,
                routes=(route,),
                wire_ids={route: model_name},
                context_window=_positive_int(info.get("max_tokens")),
                max_input_tokens=_positive_int(info.get("max_input_tokens")),
                max_output_tokens=_positive_int(info.get("max_output_tokens")),
                cost=_cost(info),
            )
        )
    return tuple(models)


def _model_info_url(ctx: omp.Context) -> str:
    base_url = str(ctx.settings.get("base_url", _DEFAULT_BASE_URL)).rstrip("/")
    if base_url.endswith("/v1"):
        base_url = base_url[:-3]
    return f"{base_url}/model/info"


@omp.hook(
    "models_discover",
    phase=omp.HookPhase.TRANSFORM,
    provider="litellm",
    on_failure=omp.OnFailure.DEFER,
)
async def discover_models(payload: DiscoveryQuery, ctx: omp.Context) -> DiscoveryPage:
    """Fetch LiteLLM model metadata and return non-authoritative catalog rows."""
    response = await omp.env.http_get(
        _model_info_url(ctx),
        timeout=Duration("5s"),
    )
    return DiscoveryPage(
        models=_model_rows(response.json(), payload.route),
        authoritative=False,
    )
