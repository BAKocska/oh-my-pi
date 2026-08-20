## What the pi original did

`pi-model-fallback` switched models after provider failures such as rate limits. It recovered status codes from response metadata, matched message text with regular expressions, changed the active model, and persisted cooldown timestamps in `~/.pi/agent/model-fallback-state.json`.

## The omp shape

This port follows docs/py/13-inference.md §§“ModelFallback” and “provider_error”. The domain hook consumes typed `ErrorKind` and `Retryability` evidence, respects committed output, retries only bounded transient failures, refreshes credentials only on the typed credential lane, and switches only to the next `provider/model` in the configured `ModelFallback.CHAIN`. There is no status-code sniffing, message-text matching, implicit parent fallback, or extension state file. `Failover(..., cooldown=...)` delegates persistence of the current `(provider, route, identity)` cooldown to Core.

## Gaps

- `ErrorKind`, `ProviderError`, `Failover`, and `ModelFallback` are documented by docs/py/13-inference.md §§“ModelFallback” and “provider_error”, but are absent from `crates/py/python/omp/provider.py` and are not re-exported by `crates/py/python/omp/__init__.py`. The hook therefore cannot import on the current frozen layer.
- `Retryability` and `ProviderError.retryability` are required by the typed-classification contract, but are absent from `crates/py/python/omp/provider.py`; docs/py/13-inference.md §“provider_error” also shows `ProviderError` without the field, so the frozen surface and that reference section both need the typed retry lane added.
