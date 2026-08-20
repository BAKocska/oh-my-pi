# Native grounding

## What the pi original did

`@pokutuna/pi-google-genai` added Google GenAI server-side grounding surfaces for Google Search, Maps, URL context, and deep research (`catalog.md:324`). It exposed provider features as extension tools.

## The omp shape

Provider-native tools are model capabilities, not Python devices. `_NATIVE_GROUNDING_SPEC` declares a Gemini `Api.GEMINI` route and places the representable grounding facts (`search` and `web`) on `ModelSpec.chat.hosted_tools`. The adjacent `ToolCaps` describes only ordinary function-call behavior. This follows `docs/py/13-inference.md` §“ChatCaps”, especially lines 827–838: hosted tools are advertised by the inference spine without occupying a registered schema slot. Registering `google_search`, `url_context`, or `deep_research` proxy devices would spend schema slots for operations the provider already performs and would replace the provider's native response path, losing its own grounding metadata and citations.

The sole `[[tools]]` row is therefore `grounding_citations`. It performs no search and calls no provider. It only validates already-returned citations into a typed `CitationPayload`, gives the model a byte-budgeted projection, and renders the same durable verdict as a citation view (`docs/py/02-verdicts.md` §“One call, one truth, three projections” and §“Rendering: the update fold”). The module stays declaration-dominant: provider, route, model, and capability values are import-time data; only citation presentation has an async body.

## Gaps

None — every symbol this port needs is frozen.
