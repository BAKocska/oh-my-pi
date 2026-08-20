from __future__ import annotations

from dataclasses import dataclass
from typing import Annotated, Awaitable, Callable, Mapping, Sequence

import omp


_PROVIDER_ID = "openai-speech"
_ROUTE_ID = "media"
_TTS_MODEL = "tts-1"
_STT_MODEL = "whisper-1"

_SPEC = omp.ProviderSpec(
    id=_PROVIDER_ID,
    name="OpenAI-compatible Speech",
    routes=(
        omp.RouteSpec(
            id=_ROUTE_ID,
            base_url="https://api.openai.com/v1/audio",
            api=omp.Api.OPENAI_MEDIA,
            auth=omp.AuthSpec(
                mode=omp.AuthMode.BEARER,
                sources=(
                    omp.CredentialSource.stored(),
                    omp.CredentialSource.env("OPENAI_API_KEY"),
                ),
            ),
        ),
    ),
    models=(
        omp.ModelSpec(
            id=_TTS_MODEL,
            display_name="TTS 1",
            routes=(_ROUTE_ID,),
            operations=frozenset({omp.Operation.SPEAK}),
        ),
        omp.ModelSpec(
            id=_STT_MODEL,
            display_name="Whisper 1",
            routes=(_ROUTE_ID,),
            operations=frozenset({omp.Operation.TRANSCRIBE}),
            input_modalities=frozenset({omp.Modality.AUDIO}),
        ),
    ),
)
_PROVIDER = omp.provider(_SPEC)


@_PROVIDER
class SpeechProvider:
    """Declare shared TTS and STT routes while keeping credentials in the provider spine."""


@dataclass(frozen=True, slots=True)
class SpeakArgs:
    """Describe text to synthesize and its requested voice."""

    text: Annotated[str, omp.Field("Text to synthesize.", coerce=(omp.Coerce.STRIP,))]
    voice: Annotated[str, omp.Field("Provider voice identifier.")] = "alloy"


@dataclass(frozen=True, slots=True)
class TranscribeArgs:
    """Describe blob-backed or Environment-resident audio to transcribe."""

    audio: Annotated[omp.EnvPath | omp.BlobRef, omp.Field("Audio EnvPath or BlobRef.")]
    language: Annotated[str | None, omp.Field("Optional BCP-47 language hint.")] = None


@dataclass(frozen=True, slots=True)
class SpeechRequest:
    """Carry one normalized synthesis request to the provider wire."""

    model: str
    text: str
    voice: str


@dataclass(frozen=True, slots=True)
class TranscriptionRequest:
    """Carry one normalized blob or Environment audio reference to the provider wire."""

    model: str
    audio: omp.EnvPath | omp.BlobRef
    language: str | None


@dataclass(frozen=True, slots=True)
class TranscriptSegment:
    """Represent one time-bounded transcript segment."""

    start_ms: int
    end_ms: int
    text: str


@dataclass(frozen=True, slots=True)
class Transcript(omp.Payload):
    """Return normalized transcript text and ordered timestamped segments."""

    text: str
    language: str | None
    segments: tuple[TranscriptSegment, ...]


@dataclass(frozen=True, slots=True)
class SynthesizedSpeech(omp.Payload):
    """Return synthesized speech exclusively as a media-typed blob part."""

    audio: omp.BlobPart
    media_type: str


Wire = Callable[[omp.Operation, object], Awaitable[object]]


async def _frozen_wire(operation: omp.Operation, request: object) -> object:
    """Use the frozen provider request seam; tests replace this function with a stub wire."""

    return await _PROVIDER.request(operation, request)  # type: ignore[arg-type]


_wire: Wire = _frozen_wire


def _parse_speech(payload: object) -> tuple[bytes, str]:
    """Parse one small class-(b) speech wire response without encoding audio as prose."""

    if not isinstance(payload, Mapping):
        raise ValueError("speech response must be an object")
    audio, media_type = payload.get("audio"), payload.get("media_type")
    if not isinstance(audio, bytes):
        raise ValueError("speech response audio must be bytes")
    if not isinstance(media_type, str) or not media_type.startswith("audio/"):
        raise ValueError("speech response media_type must be audio/*")
    return audio, media_type


def _parse_transcript(payload: object) -> Transcript:
    """Parse one small class-(b) transcription response into typed segments."""

    if not isinstance(payload, Mapping):
        raise ValueError("transcription response must be an object")
    text, language, raw_segments = payload.get("text"), payload.get("language"), payload.get("segments")
    if not isinstance(text, str) or (language is not None and not isinstance(language, str)):
        raise ValueError("transcription text and language have invalid types")
    if not isinstance(raw_segments, Sequence) or isinstance(raw_segments, (str, bytes)):
        raise ValueError("transcription segments must be a list")
    segments: list[TranscriptSegment] = []
    for raw in raw_segments:
        if not isinstance(raw, Mapping):
            raise ValueError("transcription segment must be an object")
        start, end, segment_text = raw.get("start_ms"), raw.get("end_ms"), raw.get("text")
        if isinstance(start, bool) or not isinstance(start, int) or isinstance(end, bool) or not isinstance(end, int):
            raise ValueError("transcription segment bounds must be integers")
        if start < 0 or end < start or not isinstance(segment_text, str):
            raise ValueError("transcription segment is invalid")
        segments.append(TranscriptSegment(start, end, segment_text))
    return Transcript(text, language, tuple(segments))


@omp.device("speak", family="speech", rev=1, place="env")
async def speak(args: SpeakArgs, ctx: omp.Context) -> SynthesizedSpeech:
    """Synthesize text and return audio as a media-typed blob part."""

    del ctx
    text = args.text.strip()
    if not text:
        raise ValueError("text must not be empty")
    audio, media_type = _parse_speech(
        await _wire(omp.Operation.SPEAK, SpeechRequest(_TTS_MODEL, text, args.voice))
    )
    part = omp.Part.blob(omp.Spill(audio, media_type=media_type), "Synthesized speech")
    return SynthesizedSpeech(part, media_type)


@omp.device("transcribe", family="speech", rev=1, place="env")
async def transcribe(args: TranscribeArgs, ctx: omp.Context) -> Transcript:
    """Transcribe referenced audio into text with typed timestamped segments."""

    del ctx
    return _parse_transcript(
        await _wire(
            omp.Operation.TRANSCRIBE,
            TranscriptionRequest(_STT_MODEL, args.audio, args.language),
        )
    )
