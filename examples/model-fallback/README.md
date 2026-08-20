## What the pi original did

`pi-model-fallback` switched models after provider failures such as rate limits. It recovered status codes from response metadata, matched message text with regular expressions, changed the active model, and persisted cooldown timestamps in `~/.pi/agent/model-fallback-state.json`.

## The omp shape

This port follows docs/py/13-inference.md §§“ModelFallback” and “provider_error”. The domain hook consumes typed `ErrorKind` and `Retryability` evidence, respects committed output, retries only bounded transient failures, refreshes credentials only on the typed credential lane, and switches only to the next `provider/model` in the configured `ModelFallback.CHAIN`. There is no status-code sniffing, message-text matching, implicit parent fallback, or extension state file. `Failover(..., cooldown=...)` delegates persistence of the current `(provider, route, identity)` cooldown to Core.

## Gaps

None — every symbol this port needs is frozen.
