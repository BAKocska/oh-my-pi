# Image generation

## What the pi original did

`@amaster.ai/pi-image-gen` added an image-generation tool and command backed by OpenAI, Gemini, DashScope/Ark, OpenRouter, or a custom compatible provider. Each implementation owned provider selection, authentication, request encoding, response decoding, and image delivery beside the chat-provider registry.

## The omp shape

The provider is catalog data: `ProviderSpec` selects the `OPENAI_MEDIA` codec and bearer credential sources, while `ModelSpec.operations` declares `Operation.GENERATE_IMAGE` and per-image pricing. This is the non-chat provider operation described in `docs/py/13-inference.md` under **The operation surface a provider may serve**, rather than a private HTTP client or credential cascade.

The `image_gen` soft device has typed `size`, `style`, and `count` arguments carrying `Annotated[..., omp.Field(...)]` descriptions, expectations, examples, and the one deliberate integer coercion (`docs/py/03-params.md`, **Charitable decoding**). Its declared `InferenceEffects` bounds the paid call. Provider result bytes become `omp.Spill(..., media_type="image/png")` values inside `omp.Part.blob`; `prompt()` admits those through `Budget.push_blob`, so images follow the frozen media projection path and fall back to alt text when the selected model cannot receive media (`docs/py/02-verdicts.md`, **Projecting for the model**). No base64 enters prose and no temporary file is created.

The response usage receipt is the sole source of `cost_nanos_usd`. The device appends that value as the declared `examples.image-gen.cost` entry after the routed request settles, following `docs/py/09-journal.md`, **Writing entries**.

## Gaps

- `omp.ImageCaps`, `omp.ImageFeature`, `omp.Dimensions`, and `omp.ImageFormat` are documented at `docs/py/13-inference.md:791-805` but absent from the frozen provider vocabulary. `ModelSpec.image` is only `object | None` at `crates/py/python/omp/provider.py:764-796`, and none of those symbols is exported at `crates/py/python/omp/provider.py:1344-1360`. The example can truthfully declare `Operation.GENERATE_IMAGE`, but it cannot encode the documented supported sizes, PNG format, or generation feature as typed catalog facts.
