## What the pi original did

`pi-moonshot`, `pi-zai-glm`, `pi-provider-alibaba`, and `pi-package-ovh-ai-chat` registered OpenAI- or Anthropic-compatible providers in executable TypeScript. Together they carried endpoint and credential wiring, repeated model objects, Alibaba and OVH `/models` fetch code, and OVH request-body rewriting. The four packages become four `ProviderSpec` rows, ten `RouteSpec` rows, nine reviewed `ModelSpec` rows, and three `ServiceTier` rows here: zero provider hooks and zero Python request bodies.

## The omp shape

This is the class (a) form from `docs/py/13-inference.md` § “Providers are data; code is the cold path only” and § “omp.provider”. Routes select Core-owned codecs and declare bearer or API-key placement with `AuthSpec`; credentials remain in stored or environment sources. Thinking, tool, modality, prompt-cache, pricing, and service-tier facts live on the rich model row for each provider. Alibaba’s regional coding, token-plan, and pay-as-you-go variants are routes on one provider. OVH’s standard OpenAI model listing is `DiscoverySpec(OPENAI_MODELS)`, so Core owns fetching and parsing; the original fetch loop, fallback mutation, environment-selected codec, request normalizer, and image warning hook are deleted. `docs/py/13-inference.md` § “DiscoverySpec and DiscoveryDefaults” and § “ModelSpec” define those replacements.

A small import-time pack check rejects duplicate provider ids and duplicate bare model ids before any decorator runs. This preserves an unambiguous worked-example catalog while the frozen `ProviderSpec` validator gap below remains open.

## Gaps

- `omp.SpecError` is missing: frozen `crates/py/python/omp/_errors.py:1-11` defines only `ExtensionError` and `NotWiredError`, while `docs/py/13-inference.md:353-358` requires provider declaration validation to raise `SpecError`.
- `ProviderSpec.__post_init__` does not reject duplicate `ModelSpec.id` values: frozen `crates/py/python/omp/provider.py:948-976` validates only overlays and aliases, while `docs/py/13-inference.md:353-358,764-769` promises fail-closed spec validation and defines `id` as the model half of the selector. `_validate_pack` supplies the missing declaration-time rejection for this example.
- Direct class (a) declaration is not wired: frozen `omp.provider` at `crates/py/python/omp/provider.py:1514-1533` only returns a handle, and registration occurs only in `ProviderHandle.__call__` at `crates/py/python/omp/provider.py:1002-1009`; `docs/py/13-inference.md:256-269` documents bare `omp.provider(ProviderSpec(...))` as the class (a) registration form. This example uses empty decorated classes so all four declarations reach the registry.
- `PromptCacheCaps` and `CacheRetention` disagree with their docs: frozen `crates/py/python/omp/provider.py:182-187,617-622` exposes `EPHEMERAL/STANDARD/LONG` and `minimum_prefix_tokens`/`maximum_breakpoints`; `docs/py/13-inference.md:839-842` documents `REQUEST/SESSION/SHORT/LONG` and `min_prefix_tokens`/`max_breakpoints`.
