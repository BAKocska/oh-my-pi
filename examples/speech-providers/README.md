# Speech providers

## What the pi original did

`@p8n.ai/pi-listens` provided speech-first interaction with pluggable speech-to-text and text-to-speech providers, defaulting to Sarvam AI (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:364`).

## The omp shape

The provider wire is class (a): one `ProviderSpec` declares an `OPENAI_MEDIA` route and separate model catalog entries for `Operation.SPEAK` and `Operation.TRANSCRIBE`, sharing brokered credentials as specified by `docs/py/13-inference.md:96-109` and the media codec at `:492-513`. The two soft devices remain behind `dyn`: `speak` maps text to an `omp.BlobPart` whose `omp.Spill` carries an `audio/*` media type, while `transcribe` accepts only an `omp.EnvPath` or `omp.BlobRef` and returns typed timestamped segments. Audio is never rendered as base64 prose; this follows the blob-backed media rule in `docs/py/02-verdicts.md:632-653`.

The response parsers are deliberately small class (b) Python. This applies the resolved SEARCH-seam precedent at `docs/py/13-inference.md:2540-2547`: refusing a declarative response-shape DSL keeps endpoint, operation, authentication, and codec selection declarative while response interpretation stays ordinary reviewable code.

## Gaps

- Typed speech dispatch is documented but absent from the frozen provider request contract. `docs/py/13-inference.md:102-107` says `SPEAK` and `TRANSCRIBE` use the shared provider machinery, but `crates/py/python/omp/provider.py:1031-1044` types `ProviderHandle.request` only as `(Operation, ImageRequest) -> ImageResult` and rejects every operation except `GENERATE_IMAGE`. Frozen `SpeechRequest`, `SpeechResult`, `TranscriptionRequest`, and `TranscriptionResult` symbols (or an equivalent typed contract) are required before these devices can use the real dispatch arm; the smoke replaces `_wire` at that boundary.
- Speech capability records documented at `docs/py/13-inference.md:849-865` are not frozen: `crates/py/python/omp/provider.py:783-848` defines only image capability/request/result types, and `ModelSpec.speech` / `.transcription` at `:928-929` are untyped `object | None`. Exact missing symbols are `SpeechCaps`, `SpeechFeature`, `AudioFormat`, `TranscriptionCaps`, and `TranscriptionFeature`.

`Operation.SPEAK` and `Operation.TRANSCRIBE` are present at `crates/py/python/omp/provider.py:121-135`. Generic media parts are also present: `omp.Spill.media_type` is frozen at `crates/py/python/omp/_verdicts.py:345-351`, and `omp.BlobPart` / `Part.blob` at `:137-167` preserve the blob rather than prose, so neither is a gap.
