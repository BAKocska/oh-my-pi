## What the pi original did

`@twogiants/pi-anthropic-vertex` registered Anthropic models against Google Cloud Vertex AI, copied pi's Anthropic option mapping, patched model API identities, constructed and cached SDK clients, and relied on Google application-default credentials. Its documented curl-shaped authentication path used `gcloud auth print-access-token`, while its model table was copied dynamically from pi's built-in Anthropic provider.

## The omp shape

This is the class (b) provider shape from `docs/py/13-inference.md` §§“Providers are data; code is the cold path only”, “Codecs are selected, never implemented”, “`AuthSpec`”, “Cold-path hooks”, and “`omp.creds`”. Four `RouteSpec` rows carry `global`, `us-east5`, `europe-west1`, and `asia-southeast1` as routing data, select `Api.ANTHROPIC_MESSAGES`, and describe Vertex's `projects/{project}/locations/{region}/publishers/anthropic/models` endpoint shape. `AccountScope.REGION` keeps regional quotas and credential leases distinct. Static `ModelSpec.routes` rows encode current region availability: Opus 4.6 and Sonnet 4.6 use all four routes, while Haiku 4.5 omits Asia Pacific.

The credential helper is deleted, not wrapped. No Python code shells out to `gcloud auth print-access-token`, reads ADC files, builds an authorization header per request, or refreshes inside a stream. The sole cold-path hook asks `omp.creds.mint_scoped` for a 45-minute `gcp-access-token:<region>` facet and returns its expiry to the credential layer; the host serializes refresh, stores the result, and applies the bearer header. Anthropic request encoding, streaming, prompt caching, tool normalization, and token counting stay in the selected Rust codec rather than the extension.

## Gaps

- `@omp.hook("provider_refresh")` is documented as a no-phase cold-path domain callback returning `Credential` in `docs/py/13-inference.md` §“provider_refresh” (lines 1296-1317), but frozen `crates/py/python/omp/hooks.py:301-311,343-357` does not classify `provider_refresh` as a domain event and therefore requires an admission `HookPhase`; the documented refresh declaration cannot activate against the frozen decorator.
- `CredentialSource.application_default(...)` is documented as the Google ADC acquisition constructor in `docs/py/13-inference.md` §“AuthSpec” (line 505), but frozen `crates/py/python/omp/provider.py:212-245` defines only `env`, `stored`, `oauth`, `aws_chain`, and `session`; this port declares the broker-managed stored source and scoped mint, but cannot express the documented ADC acquisition chain itself.
