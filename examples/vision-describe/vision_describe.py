from __future__ import annotations

import json
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Annotated

import omp


_VISION_ROLE = "vision"
_DEFAULT_PROMPT = "Describe this image accurately and concisely."
_CACHE_SCHEMA = 1


@dataclass(frozen=True, slots=True)
class DescribeImageArgs:
    """Select an Environment image and the question to ask about it."""

    path: Annotated[
        omp.EnvPath,
        omp.Field(
            "Environment path of a PNG, JPEG, GIF, or WebP image.",
            expected="an Environment image path",
            example="artifacts/screenshot.png",
        ),
    ]
    prompt: Annotated[
        str,
        omp.Field(
            "Question or description instruction for the vision role.",
            coerce=(omp.Coerce.STRIP,),
            expected="a non-empty image question",
            example="Describe the error shown in this screenshot.",
        ),
    ] = _DEFAULT_PROMPT


@dataclass(frozen=True, slots=True)
class DescriptionDetails:
    """Retain the source blob identity and cache provenance outside the projection."""

    blob: omp.BlobRef
    digest: str
    media_type: str
    cache_hit: bool


@dataclass(frozen=True, slots=True)
class ImageDescription(omp.Payload):
    """Carry the projected description and durable source-image details."""

    description: str
    details: DescriptionDetails


def _image_media_type(data: bytes) -> str:
    """Return a concrete image media type from its byte signature."""

    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "image/png"
    if data.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if data.startswith((b"GIF87a", b"GIF89a")):
        return "image/gif"
    if len(data) >= 12 and data.startswith(b"RIFF") and data[8:12] == b"WEBP":
        return "image/webp"
    raise ValueError("path must contain a PNG, JPEG, GIF, or WebP image")


def _cache_path(root: Path, digest: str) -> Path:
    """Return the content-addressed description-cache path."""

    return root / "vision-describe" / f"{digest}.json"


def _read_cache(path: Path, digest: str) -> str | None:
    """Read one validated rebuildable cache entry, ignoring corruption."""

    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        return None
    if not isinstance(value, dict):
        return None
    description = value.get("description")
    if (
        value.get("schema") != _CACHE_SCHEMA
        or value.get("truth") is not False
        or value.get("digest") != digest
        or not isinstance(description, str)
        or not description.strip()
    ):
        return None
    return description.strip()


def _write_cache(path: Path, digest: str, description: str) -> None:
    """Atomically persist one rebuildable content-addressed cache entry."""

    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
    try:
        temporary.write_text(
            json.dumps(
                {
                    "schema": _CACHE_SCHEMA,
                    "truth": False,
                    "digest": digest,
                    "description": description,
                },
                ensure_ascii=False,
                separators=(",", ":"),
            ),
            encoding="utf-8",
        )
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


class DescribeImageDevice:
    """Soft device that asks the deployment-mapped vision role about an image."""

    Payload = ImageDescription

    async def __call__(
        self, args: DescribeImageArgs, ctx: omp.Context
    ) -> ImageDescription:
        """Describe an Environment image or reuse its content-addressed description."""

        del ctx
        question = args.prompt.strip()
        if not question:
            raise ValueError("prompt must not be empty")

        image = await args.path.read_bytes()
        media_type = _image_media_type(image)
        blob = await omp.env.blobs.put(image)
        digest = blob.hex

        state = await omp.state_dir()
        cache = _cache_path(state.local_path(), digest)
        description = _read_cache(cache, digest)
        cache_hit = description is not None
        if description is None:
            image_part = omp.Part.blob(
                omp.Spill(image, media_type=media_type),
                f"Image from {args.path}",
            )
            completion = await omp.agents.completion(
                (omp.Part.text(question), image_part),
                role=_VISION_ROLE,
                system=(
                    "Answer only from the supplied image. State uncertainty instead of "
                    "inventing details."
                ),
                scope="turn",
                max_output_tokens=800,
                deadline=omp.Duration("45s"),
                labels={"device": "describe_image", "media_type": media_type},
            )
            description = completion.text.strip()
            if not description:
                raise ValueError("vision role returned an empty description")
            _write_cache(cache, digest, description)

        return ImageDescription(
            description=description,
            details=DescriptionDetails(
                blob=blob,
                digest=digest,
                media_type=media_type,
                cache_hit=cache_hit,
            ),
        )

    def prompt(self, view: object, caps: omp.PromptCaps) -> list[object]:
        """Project only the description while retaining blob details in the verdict."""

        budget = omp.Budget(caps)
        match view:
            case omp.Ok(payload):
                budget.push(payload.description)
                return budget.finish()
            case _:
                raise TypeError("describe_image prompt received an unsupported call outcome")


describe_image = omp.device(
    "describe_image",
    family="vd",
    rev=1,
    schema=DescribeImageArgs,
    place="env",
    summary="Describe a local image through the deployment-mapped vision role.",
    effects=omp.Effects(inference=omp.InferenceEffects(max_requests=1, max_usd=1.0)),
)(DescribeImageDevice())
