## What the pi original did

`pi-provider-litellm` registered a LiteLLM proxy as a custom provider, discovered deployed models, copied reported token pricing into its model catalog, and adjusted requests. It could also run `LITELLM_API_KEY_HELPER` synchronously while resolving credentials for a request (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`; `docs/py/13-inference.md:508-515`).

## The omp shape

This class (b) port is declaration-dominant: `ProviderSpec` declares one OpenAI-compatible gateway route, `AuthSpec` asks the encrypted provider account store for a bearer key, and a non-authoritative `DiscoverySpec` polls `/model/info`. The cold `models_discover` hook maps deployed model names, token limits, and any per-token prices reported by LiteLLM into `ModelSpec` rows; prices are converted exactly to omp's per-million-token `Cost` units (`docs/py/13-inference.md` §`DiscoverySpec` and `DiscoveryDefaults`, §`ModelSpec`, and §`Cost`). A missing row is not proof that a deployment was removed, so discovery deliberately does not retire earlier catalog evidence.

The synchronous `LITELLM_API_KEY_HELPER` request-path shell execution is deleted. Normal keys now live in the account store and are injected by the core credential service without entering Python. For installations that must mint short-lived keys with a command, the documented replacement is a policy-visible `provider_refresh` hook using `omp.env.exec` ahead of expiry, whose returned credential is stored under the daemon's refresh lease and real TTL (`docs/py/13-inference.md:508-515, 2382-2388`); this example intentionally uses the simpler stored-key path.

`settings.base_url` selects the gateway for the discovery poll and defaults to `http://127.0.0.1:4000/v1`. The OpenAI route currently carries that same default literally because the frozen declaration API cannot replace an import-time route from activation-time settings; see the gap below.

## Gaps

- The documented `omp.provider(...)->ProviderHandle` / `ProviderHandle.replace` setting-time reconciliation surface (`docs/py/13-inference.md:256-321`) is absent: frozen `crates/py/python/omp/provider.py:1045-1055` only returns a class decorator. Consequently the declared inference route cannot be rebuilt from `ctx.settings.base_url` at activation.
