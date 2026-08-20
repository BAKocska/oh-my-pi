## What the pi original did

`@zgltyq/pi-provider-kimi-code` registered Kimi's Anthropic-compatible model, ran browser/device OAuth polling and refresh, stored tokens, and wrapped every stream to replace the client's API-key header with bearer authentication. It also maintained a device-id file, probed macOS for fingerprinting, and intercepted images larger than 1 MB for upload to Kimi's Files API.

## The omp shape

This port is the class (b), mostly class (a), worked port from `docs/py/13-inference.md` §1: the extension is one provider declaration and contains no request-path Python or hook. The stream/header wrapper is replaced by `AuthSpec(mode=BEARER, header="authorization", prefix="Bearer ")`; the OAuth polling loop, refresh, and token storage are replaced by `OAuthSpec` and `OAuthFlow.device_code`; and principal identity is declared by `PrincipalResolution`. The device-id file and OS probe are deleted in favor of omp's client identity, large-image upload is deleted in favor of core media staging, prompt caching is `ContextSpec.prefix_cache`, schema normalization is `ToolSchemaFlavor.MOONSHOT_MFJS`, and token counting is an `Operation.COUNT_TOKENS` catalog fact.

## Gaps

- `Duration` is frozen and matches the documented use; no frozen-versus-docs signature divergence was encountered.
