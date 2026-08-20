# Realtime session

## What the pi original did

`pi-realtime` added a live voice/stream session to Pi (`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:395`). It kept a bidirectional realtime transport open while publishing partial speech/transcript progress and returning the completed conversation.

## The omp shape

One catalog declaration names the `OPENAI_REALTIME` codec, `WEBRTC` transport, brokered bearer credential sources, and a model whose sole operation is `Operation.REALTIME`. Those are ordinary provider facts from `docs/py/13-inference.md:96-110` and `:492-515`; the extension does not implement authentication or a second provider registry.

The frozen provider handle has no realtime streaming request/session arm, so the `realtime` device uses the supported fallback: one uniquely named `omp-realtime-bridge` process per invocation. The Environment owns that process and its upstream WebRTC/WebSocket handles. Python sees only `Process.send` and `Process.output` frames, never a socket, file descriptor, PID, or credential. The process output stream is closed and the process tree is stopped in the async generator's `finally` path after settle, transport fault, generator close, or coroutine cancellation. This follows the scoped-handle/drop rule at `docs/py/11-env.md:64-70`, the requirement to consume or drop stream handles at `:169-170`, and the invocation-guard cancellation path at `docs/py/00-overview.md:288-294`. No socket can outlive the invocation.

Partial transcript and audio notifications are typed `omp.Update` payloads. Audio deltas are decoded only from the bridge protocol and streamed immediately into invocation-guarded Environment blob writers; updates report track, sequence, media type, and byte counts rather than putting audio in prose. Exactly one `omp.Done` settles with a typed `Transcript` plus `omp.BlobPart` audio tracks, matching the progressive-output contract in `docs/py/01-devices.md:268-304` and the JSON-safe update/terminal vocabulary in `docs/py/03-params.md:149-175`. Cancellation aborts every uncommitted writer and stops the bridge; the Environment guard remains the structural backstop if Python cannot unwind.

## Gaps

- The documented typed realtime capability vocabulary is not frozen. `docs/py/13-inference.md:851-865` specifies `RealtimeCaps` and `RealtimeFeature`, but `crates/py/python/omp/provider.py:990-1021` leaves `ModelSpec.realtime` as `object | None`, and `crates/py/python/omp/provider.py:1846-1869` exports neither symbol. The route can truthfully declare `Operation.REALTIME`, but cannot publish typed audio-in, audio-out, VAD, interruption, voice, or transport capabilities.
- Realtime dispatch is internally contradictory in the reference and incomplete in the frozen layer. `docs/py/13-inference.md:102-107` says `REALTIME` uses the shared provider machinery and `:511` assigns it a codec, but the typed operation values at `:867-916` define no realtime request, result, update, or session handle. Correspondingly, `ProviderHandle.request` at `crates/py/python/omp/provider.py:1133-1155` accepts only `ImageRequest | SpeechRequest | TranscriptionRequest` and explicitly rejects operations other than `GENERATE_IMAGE`, `SPEAK`, and `TRANSCRIBE`. A typed streaming realtime dispatch/session arm is required before this port can replace the Environment-owned bridge with direct provider routing.

`Operation.REALTIME` itself is present at `crates/py/python/omp/provider.py:121-138`, as are `Api.OPENAI_REALTIME` and `Transport.WEBRTC` at `:28-77`; those symbols are not gaps.
