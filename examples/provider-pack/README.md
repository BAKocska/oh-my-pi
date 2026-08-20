## What the pi original did

`pi-moonshot`, `pi-zai-glm`, `pi-provider-alibaba`, and `pi-package-ovh-ai-chat` registered OpenAI- or Anthropic-compatible providers in executable TypeScript. Together they carried endpoint and credential wiring, repeated model objects, Alibaba and OVH `/models` fetch code, and OVH request-body rewriting. The four packages become four `ProviderSpec` rows, ten `RouteSpec` rows, nine reviewed `ModelSpec` rows, and three `ServiceTier` rows here: zero provider hooks and zero Python request bodies.

## The omp shape

This is the class (a) form from `docs/py/13-inference.md` § “Providers are data; code is the cold path only” and § “omp.provider”. Routes select Core-owned codecs and declare bearer or API-key placement with `AuthSpec`; credentials remain in stored or environment sources. Thinking, tool, modality, prompt-cache, pricing, and service-tier facts live on the rich model row for each provider. Alibaba’s regional coding, token-plan, and pay-as-you-go variants are routes on one provider. OVH’s standard OpenAI model listing is `DiscoverySpec(OPENAI_MODELS)`, so Core owns fetching and parsing; the original fetch loop, fallback mutation, environment-selected codec, request normalizer, and image warning hook are deleted. `docs/py/13-inference.md` § “DiscoverySpec and DiscoveryDefaults” and § “ModelSpec” define those replacements.

A small import-time pack check rejects duplicate provider ids and duplicate bare model ids before any decorator runs. This preserves an unambiguous worked-example catalog while the frozen `ProviderSpec` validator gap below remains open.

## Gaps

None — every symbol this port needs is frozen.
