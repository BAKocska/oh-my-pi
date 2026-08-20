# Search provider

## What the pi original did

The API-key search backends in `.plan/feature-map/web.md:45-56` each owned an HTTP client, credential lookup, provider-specific response parsing, pagination, and a model-facing search tool. Brave used its Web Search endpoint and mapped each flat result record into a title, URL, and description.

## The omp shape

The provider half is catalog data: `ProviderSpec` declares `Operation.SEARCH`, the endpoint, and `AuthSpec` declares `x-subscription-token` placement with stored and `BRAVE_SEARCH_API_KEY` sources. The `[settings]` value is only an optional credential identity; the API key itself remains in the declared credential store rather than configuration. The soft `web_search` device owns paging and the small Python parser maps the backend's result list to typed `SearchResult(title, url, snippet, rank)` values.

This is deliberately class (b), not a declarative response codec. The resolved 2026-08-19 ruling in `docs/py/13-inference.md:2429-2435` refuses `SearchResponseShape`: teaching extensions to declare arbitrary result paths and field pointers is the first step toward a schema-driven codec DSL. Endpoint and authentication facts stay declarative; response interpretation stays ordinary, reviewable Python. Direct `urllib` follows the v1 no-sandbox ruling and, like `examples/web-fetch`, does not claim `env.net` is a security boundary.

## Gaps

- `omp.creds.reveal` is documented at `docs/py/13-inference.md:1524-1541`, but the frozen package has no `creds` namespace: `crates/py/python/omp/__init__.py:442-493` imports the provider vocabulary and `:920-932` exports `provider` without `creds`. The device shows the documented call and cannot execute it on the frozen layer.
- No extension-visible `Operation.SEARCH` dispatch or parser-registration seam is documented or frozen. `docs/py/13-inference.md:105-109` says SEARCH uses the shared provider machinery, while `crates/py/python/omp/provider.py:1045-1055` only registers declarations. A symbol and typed request/response contract need a ruling before core can route a response into this Python parser.
- The closed `Api` vocabulary at `crates/py/python/omp/provider.py:25-48` has Exa, Tavily, Kagi, Perplexity, and Parallel search codecs but no Brave or generic Python-parsed search selector; compare `docs/py/13-inference.md:434-455`. The declaration therefore names `Api.SEARCH_EXA` as the nearest SEARCH-family selector, but it is not wire-compatible with Brave and must not be spine-dispatched until the missing selector/seam is resolved.
