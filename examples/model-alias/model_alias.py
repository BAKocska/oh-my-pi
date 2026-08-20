from __future__ import annotations

import json
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

import omp
from omp.provider import ProviderSpec


class AliasConflict(ValueError):
    """Reject one selector alias assigned to two model targets."""


@dataclass(frozen=True, slots=True)
class ModelAliasPatch:
    """Describe one provider-scoped selector alias and display-name patch."""

    alias: str
    target: str
    display_name: str | None = None


@dataclass(frozen=True, slots=True)
class ProviderAliasPatch:
    """Collect the catalog data contributed for one existing provider."""

    provider: str
    display_name: str | None
    models: tuple[ModelAliasPatch, ...]


def lower_aliases(
    aliases: object,
    provider_aliases: object = (),
) -> tuple[ProviderAliasPatch, ...]:
    """Lower settings records into deterministic provider-scoped catalog patches."""

    model_rows = _records(aliases, "aliases")
    provider_rows = _records(provider_aliases, "provider_aliases")
    model_patches: dict[str, dict[str, ModelAliasPatch]] = {}
    provider_names: dict[str, str] = {}

    for index, row in enumerate(model_rows):
        provider = _text(row, "provider", f"aliases[{index}]")
        model = _text(row, "model", f"aliases[{index}]")
        alias = _text(row, "alias", f"aliases[{index}]")
        name = _optional_text(row, "name", f"aliases[{index}]")
        selector = f"{provider}/{alias}"
        target = f"{provider}/{model}"
        patch = ModelAliasPatch(alias=alias, target=target, display_name=name)
        existing = model_patches.setdefault(provider, {}).get(alias)
        if existing is not None and existing.target != target:
            raise AliasConflict(
                f"alias {selector!r} targets both {existing.target!r} and {target!r}"
            )
        if existing is not None and existing.display_name != name:
            raise AliasConflict(
                f"alias {selector!r} has conflicting display names "
                f"{existing.display_name!r} and {name!r}"
            )
        model_patches[provider][alias] = patch

    for index, row in enumerate(provider_rows):
        provider = _text(row, "provider", f"provider_aliases[{index}]")
        name = _text(row, "name", f"provider_aliases[{index}]")
        existing = provider_names.get(provider)
        if existing is not None and existing != name:
            raise AliasConflict(
                f"provider {provider!r} has conflicting display names "
                f"{existing!r} and {name!r}"
            )
        provider_names[provider] = name

    providers = sorted(model_patches.keys() | provider_names.keys())
    return tuple(
        ProviderAliasPatch(
            provider=provider,
            display_name=provider_names.get(provider),
            models=tuple(
                model_patches[provider][alias]
                for alias in sorted(model_patches.get(provider, {}))
            ),
        )
        for provider in providers
    )


def _records(value: object, setting: str) -> tuple[Mapping[str, object], ...]:
    if isinstance(value, str):
        try:
            value = json.loads(value)
        except json.JSONDecodeError as error:
            raise ValueError(f"settings.{setting} must be valid JSON: {error.msg}") from error
    if not isinstance(value, (list, tuple)):
        raise TypeError(f"settings.{setting} must be an array")
    rows: list[Mapping[str, object]] = []
    for index, row in enumerate(value):
        if not isinstance(row, Mapping):
            raise TypeError(f"settings.{setting}[{index}] must be an object")
        rows.append(row)
    return tuple(rows)


def _text(row: Mapping[str, object], key: str, location: str) -> str:
    value = row.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{location}.{key} must be a non-empty string")
    return value.strip()


def _optional_text(
    row: Mapping[str, object], key: str, location: str
) -> str | None:
    if key not in row:
        return None
    return _text(row, key, location)


def _manifest_defaults() -> tuple[object, object]:
    with Path(__file__).with_name("omp.toml").open("rb") as manifest_file:
        settings = tomllib.load(manifest_file)["settings"]
    return settings["aliases"]["default"], settings["provider_aliases"]["default"]


def _declare_provider_display(provider: str, display_name: str) -> type:
    spec = ProviderSpec(
        id=provider,
        name=display_name,
        routes=(),
        models=(),
    )
    implementation = type(f"_{provider.title()}DisplayAlias", (), {})
    return omp.provider(spec, extends=provider)(implementation)


_MODEL_SETTINGS, _PROVIDER_SETTINGS = _manifest_defaults()
OVERLAYS = lower_aliases(_MODEL_SETTINGS, _PROVIDER_SETTINGS)
_PROVIDER_DECLARATIONS = tuple(
    _declare_provider_display(patch.provider, patch.display_name)
    for patch in OVERLAYS
    if patch.display_name is not None
)
