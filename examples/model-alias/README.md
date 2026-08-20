## What the pi original did

[`@zigai/pi-model-alias`](https://github.com/zigai/pi-tweaks/tree/master/packages/pi-model-alias) monkey-patched Pi's model registry and selector UI, then rewrote the model field in every provider request so configured short IDs reached the original provider model. It also replaced provider and model labels in the UI.

## The omp shape

This port follows `docs/py/13-inference.md` §§“omp.provider” and “ProviderSpec”. `[settings.aliases]` and `[settings.provider_aliases]` are lowered once into deterministic, provider-scoped `ModelAliasPatch` and `ProviderAliasPatch` data. Duplicate aliases for different targets fail before activation and name both targets, matching the equal-priority conflict rule. Provider display names are declaration-time `@omp.provider(..., extends=...)` overlays. There is no registry monkey-patch, model-select hook, UI patch, or per-request model rewrite: selector aliases and display names belong in the catalog, with catalog provenance.

## Gaps

- Model selector aliases cannot be submitted to the frozen Python layer. The catalog has `ModelOverlay` / `ModelPatch` and `ScopedAlias` shapes cited by `docs/py/13-inference.md:291-296,1985-1987`, but `crates/py/python/omp/provider.py:764-809,1344-1360` exposes only complete `ModelSpec` records and provider-level `ProviderSpec.aliases`; it exports no `ModelPatch`, `ModelOverlay`, or `ScopedAlias`. The example therefore lowers and validates both model aliases as catalog data, but only its provider display-name overlays can currently be registered.
