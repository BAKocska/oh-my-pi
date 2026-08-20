from __future__ import annotations

from collections.abc import Mapping, Sequence

import omp
from omp import Api, CompatFlags, Context, Cost, ManagementSpec, Modality
from omp import ModelSpec, Operation, ProviderSpec, RouteSpec, ToolSchemaFlavor
from omp.provider import (
    DiscoveryKind,
    DiscoveryPage,
    DiscoveryQuery,
    DiscoverySpec,
    TrustDomain,
)

_SERVERS = {"local": "http://127.0.0.1:1234"}

_LMSTUDIO_SPEC = ProviderSpec(
    id="lmstudio",
    name="LM Studio",
    management=ManagementSpec(
        operations=frozenset({Operation.DISCOVER_MODELS}),
    ),
    routes=tuple(
        RouteSpec(
            id=name,
            base_url=f"{base_url}/v1",
            api=Api.OPENAI_CHAT,
            trust=TrustDomain.loopback(),
            compat=CompatFlags(schema_flavor=ToolSchemaFlavor.GRAMMAR),
            discovery=DiscoverySpec(
                kind=DiscoveryKind.SPECIALIZED,
                path="/api/v1/models",
                label=f"LM Studio ({name})",
                authoritative=True,
                interval=omp.Duration("30s"),
            ),
        )
        for name, base_url in _SERVERS.items()
    ),
)


def _parse_models(payload: object, route: str) -> tuple[ModelSpec, ...]:
    if not isinstance(payload, Mapping):
        return ()
    raw_models = payload.get("models")
    if not isinstance(raw_models, Sequence) or isinstance(
        raw_models, (str, bytes, bytearray)
    ):
        return ()

    models: list[ModelSpec] = []
    seen: set[str] = set()
    for row in raw_models:
        if not isinstance(row, Mapping) or row.get("type") != "llm":
            continue
        model_id = row.get("key")
        if not isinstance(model_id, str) or not model_id or model_id in seen:
            continue
        seen.add(model_id)

        display_name = row.get("display_name")
        if not isinstance(display_name, str) or not display_name:
            display_name = model_id
        context_window = row.get("max_context_length")
        if (
            not isinstance(context_window, int)
            or isinstance(context_window, bool)
            or context_window <= 0
        ):
            context_window = None
        capabilities = row.get("capabilities")
        vision = isinstance(capabilities, Mapping) and capabilities.get("vision") is True
        modalities = (
            frozenset({Modality.TEXT, Modality.IMAGE})
            if vision
            else frozenset({Modality.TEXT})
        )
        models.append(
            ModelSpec(
                id=model_id,
                display_name=display_name,
                routes=(route,),
                operations=frozenset({Operation.CHAT}),
                context_window=context_window,
                input_modalities=modalities,
                cost=Cost.free(),
            )
        )
    return tuple(models)


@omp.provider(_LMSTUDIO_SPEC)
class LmStudio:
    """Declare the local OpenAI-compatible LM Studio provider and discovery job."""


@omp.hook(
    "models_discover",
    provider="lmstudio",
    phase=omp.HookPhase.TRANSFORM,
    on_failure=omp.OnFailure.DEFER,
)
async def _discover(query: DiscoveryQuery, ctx: Context) -> DiscoveryPage:
    base_url = _SERVERS.get(query.route)
    if base_url is None:
        raise ValueError(f"unknown LM Studio route: {query.route}")
    response = await omp.env.http_get(
        f"{base_url}/api/v1/models",
        timeout=omp.Duration("2s"),
    )
    return DiscoveryPage(
        models=_parse_models(response.json(), query.route),
        authoritative=True,
    )
