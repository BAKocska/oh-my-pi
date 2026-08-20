from __future__ import annotations

import asyncio
import base64
import hashlib
import json
from collections.abc import AsyncIterator, Mapping
from dataclasses import dataclass
from typing import Annotated, Protocol

import omp


_PROVIDER_ID = "openai-realtime"
_ROUTE_ID = "realtime"
_MODEL_ID = "gpt-realtime"
_BRIDGE = "omp-realtime-bridge --stdio --provider openai-realtime --route realtime"

_SPEC = omp.ProviderSpec(
    id=_PROVIDER_ID,
    name="OpenAI-compatible Realtime",
    routes=(
        omp.RouteSpec(
            id=_ROUTE_ID,
            base_url="https://api.openai.com/v1/realtime",
            api=omp.Api.OPENAI_REALTIME,
            transport=omp.Transport.WEBRTC,
            auth=omp.AuthSpec(
                mode=omp.AuthMode.BEARER,
                sources=(
                    omp.CredentialSource.stored(),
                    omp.CredentialSource.env("OPENAI_API_KEY"),
                ),
            ),
            limits=omp.RouteLimits(
                operations=frozenset({omp.Operation.REALTIME})
            ),
        ),
    ),
    models=(
        omp.ModelSpec(
            id=_MODEL_ID,
            display_name="GPT Realtime",
            routes=(_ROUTE_ID,),
            operations=frozenset({omp.Operation.REALTIME}),
            input_modalities=frozenset({omp.Modality.TEXT, omp.Modality.AUDIO}),
        ),
    ),
)
_PROVIDER = omp.provider(_SPEC)


@_PROVIDER
class RealtimeProvider:
    """Declare the realtime codec, transport, model, and brokered credentials."""


@dataclass(frozen=True, slots=True)
class RealtimeArgs:
    """Configure one invocation-scoped realtime voice session."""

    instructions: Annotated[
        str,
        omp.Field(
            "Instructions for the realtime session.",
            coerce=(omp.Coerce.STRIP,),
            expected="non-empty session instructions",
            example="Transcribe the conversation and answer briefly.",
        ),
    ]
    voice: Annotated[str, omp.Field("Provider voice identifier.")] = "alloy"
    language: Annotated[
        str | None, omp.Field("Optional BCP-47 transcript language hint.")
    ] = None


@dataclass(frozen=True, slots=True)
class TranscriptUpdate:
    """Carry one ephemeral partial transcript from the realtime transport."""

    kind: str
    text: str


@dataclass(frozen=True, slots=True)
class AudioUpdate:
    """Report one ephemeral audio delta without placing audio bytes in prose."""

    kind: str
    track: str
    sequence: int
    chunk_bytes: int
    total_bytes: int
    media_type: str


@dataclass(frozen=True, slots=True)
class Transcript:
    """Represent the settled typed transcript."""

    text: str
    language: str | None


@dataclass(frozen=True, slots=True)
class RealtimeResult(omp.Payload):
    """Settle a realtime session with typed text and blob-backed audio tracks."""

    transcript: Transcript
    audio: tuple[omp.BlobPart, ...]
    media_types: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class RealtimeFault(omp.Fault):
    """Describe a malformed or prematurely ended realtime transport session."""

    detail: str


class _Process(Protocol):
    """Narrow Environment process contract used by the realtime session."""

    def output(self, *, after: int = 0) -> AsyncIterator[omp.env.ProcessOutput]: ...

    async def send(self, data: bytes) -> None: ...

    async def stop(self, **options: object) -> object: ...


class _BlobWriter(Protocol):
    """Narrow staged-blob contract used by audio tracks."""

    async def write(self, chunk: bytes) -> None: ...

    async def commit(self) -> omp.BlobRef: ...

    def abort(self) -> None: ...


async def _open_transport(args: RealtimeArgs, ctx: omp.Context) -> _Process:
    """Start one Environment-owned bridge and send its non-secret session configuration."""

    digest = hashlib.blake2s(ctx.invocation.encode("utf-8"), digest_size=8).hexdigest()
    process = await omp.env.proc.start(
        f"examples.realtime-session.{digest}",
        _BRIDGE,
        ready=omp.env.ReadyLog(r"^realtime bridge ready$"),
    )
    request = json.dumps(
        {
            "type": "session.open",
            "model": _MODEL_ID,
            "instructions": args.instructions,
            "voice": args.voice,
            "language": args.language,
        },
        separators=(",", ":"),
    ).encode("utf-8")
    try:
        await process.send(request + b"\n")
    except BaseException:
        try:
            await process.stop(grace=omp.Duration("1s"))
        except (omp.env.NotFound, omp.env.Disconnected):
            pass
        raise
    return process


def _open_audio_writer() -> _BlobWriter:
    """Open an invocation-guarded Environment blob writer for one audio track."""

    return omp.env.blobs.writer()


async def _records(process: _Process) -> AsyncIterator[Mapping[str, object]]:
    """Decode newline-delimited stdout records without retaining the process stream handle."""

    frames = process.output()
    buffered = bytearray()
    try:
        async for frame in frames:
            if frame.channel is not omp.env.Channel.STDOUT:
                continue
            buffered.extend(frame.data)
            while True:
                newline = buffered.find(b"\n")
                if newline < 0:
                    break
                raw = bytes(buffered[:newline])
                del buffered[: newline + 1]
                if not raw:
                    continue
                value = json.loads(raw)
                if not isinstance(value, Mapping):
                    raise ValueError("realtime bridge emitted a non-object frame")
                yield value
        if buffered.strip():
            raise ValueError("realtime bridge ended with a truncated frame")
    finally:
        close = getattr(frames, "aclose", None)
        if close is not None:
            await close()


def _text(value: object, field: str) -> str:
    if not isinstance(value, str):
        raise ValueError(f"realtime frame {field} must be a string")
    return value


class RealtimeDevice:
    """Stream realtime progress while all transport resources remain Environment-owned."""

    Payload = RealtimeResult

    async def __call__(
        self, args: RealtimeArgs, ctx: omp.Context
    ) -> AsyncIterator[omp.Update[TranscriptUpdate | AudioUpdate] | omp.Done[RealtimeResult | RealtimeFault]]:
        """Open, stream, settle, and unconditionally stop one realtime session."""

        instructions = args.instructions.strip()
        if not instructions:
            yield omp.Done(RealtimeFault("instructions must not be empty"))
            return

        process: _Process | None = None
        writers: dict[str, _BlobWriter] = {}
        media_types: dict[str, str] = {}
        totals: dict[str, int] = {}
        committed: set[str] = set()
        transcript_parts: list[str] = []
        try:
            process = await _open_transport(
                RealtimeArgs(instructions, args.voice, args.language), ctx
            )
            async for frame in _records(process):
                ctx.checkpoint()
                kind = frame.get("type")
                if kind == "transcript.delta":
                    delta = _text(frame.get("text"), "text")
                    transcript_parts.append(delta)
                    yield omp.Update(TranscriptUpdate("transcript", "".join(transcript_parts)))
                    continue
                if kind == "audio.delta":
                    track = _text(frame.get("track", "assistant"), "track")
                    media_type = _text(frame.get("media_type", "audio/pcm"), "media_type")
                    if not media_type.startswith("audio/"):
                        raise ValueError("realtime audio media_type must be audio/*")
                    previous_type = media_types.setdefault(track, media_type)
                    if previous_type != media_type:
                        raise ValueError("realtime audio track changed media type")
                    encoded = _text(frame.get("data"), "data")
                    try:
                        chunk = base64.b64decode(encoded, validate=True)
                    except ValueError as error:
                        raise ValueError("realtime audio data must be canonical base64") from error
                    writer = writers.get(track)
                    if writer is None:
                        writer = _open_audio_writer()
                        writers[track] = writer
                    await writer.write(chunk)
                    totals[track] = totals.get(track, 0) + len(chunk)
                    yield omp.Update(
                        AudioUpdate(
                            "audio",
                            track,
                            int(frame.get("sequence", 0)),
                            len(chunk),
                            totals[track],
                            media_type,
                        )
                    )
                    continue
                if kind == "error":
                    yield omp.Done(RealtimeFault(_text(frame.get("detail"), "detail")))
                    return
                if kind != "session.done":
                    raise ValueError(f"unknown realtime frame type {kind!r}")

                final_text = frame.get("transcript")
                if final_text is None:
                    final_text = "".join(transcript_parts)
                final_text = _text(final_text, "transcript")
                if not final_text:
                    raise ValueError("realtime session settled without a transcript")
                if not writers:
                    raise ValueError("realtime session settled without audio")

                parts: list[omp.BlobPart] = []
                settled_types: list[str] = []
                for track, writer in writers.items():
                    ref = await writer.commit()
                    committed.add(track)
                    parts.append(omp.Part.blob(ref, f"Realtime audio track: {track}"))
                    settled_types.append(media_types[track])
                yield omp.Done(
                    RealtimeResult(
                        Transcript(final_text, args.language),
                        tuple(parts),
                        tuple(settled_types),
                    )
                )
                return
            yield omp.Done(RealtimeFault("realtime transport ended without session.done"))
        except asyncio.CancelledError:
            raise
        except (TypeError, ValueError) as error:
            yield omp.Done(RealtimeFault(str(error)))
        finally:
            for track, writer in writers.items():
                if track not in committed:
                    writer.abort()
            if process is not None:
                try:
                    await process.stop(grace=omp.Duration("1s"))
                except (omp.env.NotFound, omp.env.Disconnected):
                    pass

    def prompt(self, view: object, caps: omp.PromptCaps) -> list[object]:
        """Project the transcript first, then audio under the selected model's media budget."""

        if not isinstance(view, omp.Ok) or not isinstance(view.payload, RealtimeResult):
            raise TypeError("realtime prompt received an unsupported call outcome")
        output = omp.Budget(caps)
        output.push(view.payload.transcript.text)
        for part in view.payload.audio:
            if not output.push_blob(part.blob, part.alt or "Realtime audio"):
                break
        return output.finish()


realtime = omp.device(
    "realtime",
    family="voice",
    rev=1,
    place="env",
    schema=RealtimeArgs,
    summary="Open one invocation-scoped realtime voice session.",
    effects=omp.Effects(
        exec=omp.ExecEffects(commands=("omp-realtime-bridge",), network=True),
        inference=omp.InferenceEffects(max_requests=1, max_usd=5.0),
    ),
)(RealtimeDevice())
