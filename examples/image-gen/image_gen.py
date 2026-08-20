from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from typing import Annotated, Mapping

import omp


_PROVIDER_ID = "openai-images"
_ROUTE_ID = "images"
_DEFAULT_MODEL = "gpt-image-1"

_IMAGE_PROVIDER_SPEC = omp.ProviderSpec(
    id=_PROVIDER_ID,
    name="OpenAI Images",
    routes=(
        omp.RouteSpec(
            id=_ROUTE_ID,
            base_url="https://api.openai.com/v1/images/generations",
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
            id=_DEFAULT_MODEL,
            display_name="GPT Image 1",
            routes=(_ROUTE_ID,),
            operations=frozenset({omp.Operation.GENERATE_IMAGE}),
            cost=omp.Cost(image="0.04"),
        ),
    ),
)
_IMAGE_PROVIDER = omp.provider(_IMAGE_PROVIDER_SPEC)


@_IMAGE_PROVIDER
class OpenAIImages:
    """Declare an image-generation route whose auth and codec stay in the provider spine."""


class ImageSize(StrEnum):
    """Image dimensions accepted by the declared model."""

    SQUARE = "1024x1024"
    LANDSCAPE = "1536x1024"
    PORTRAIT = "1024x1536"


class ImageStyle(StrEnum):
    """Provider-neutral image styling preference."""

    NATURAL = "natural"
    VIVID = "vivid"


@dataclass(frozen=True, slots=True)
class ImageGenArgs:
    """Describe one routed image-generation request."""

    prompt: Annotated[
        str,
        omp.Field(
            "Text description of the image to generate.",
            coerce=(omp.Coerce.STRIP,),
            expected="a non-empty image prompt",
            example="A cyanotype moth over a technical grid",
        ),
    ]
    size: Annotated[
        ImageSize,
        omp.Field(
            "Output dimensions.",
            expected="1024x1024, 1536x1024, or 1024x1536",
            example="1024x1024",
        ),
    ] = ImageSize.SQUARE
    style: Annotated[
        ImageStyle,
        omp.Field(
            "Rendering style preference.",
            expected="natural or vivid",
            example="natural",
        ),
    ] = ImageStyle.NATURAL
    count: Annotated[
        int,
        omp.Field(
            "Number of images to generate, from one through four.",
            coerce=(omp.Coerce.INTEGER,),
            expected="an integer from 1 through 4",
            example="1",
        ),
    ] = 1


@dataclass(frozen=True, slots=True)
class ImageUsage:
    """Cost attributed by the provider spine to one image request."""

    cost_nanos_usd: int


@dataclass(frozen=True, slots=True)
class GeneratedImage:
    """One media-typed image returned by the provider spine."""

    data: bytes
    media_type: str


@dataclass(frozen=True, slots=True)
class ImageResponse:
    """Typed terminal response expected from routed image generation."""

    provider: str
    route: str
    model: str
    images: tuple[GeneratedImage, ...]
    usage: ImageUsage


@dataclass(frozen=True, slots=True)
class ImageGeneration(omp.Payload):
    """Durable image result projected only through blob-backed media parts."""

    provider: str
    model: str
    size: ImageSize
    style: ImageStyle
    images: tuple[omp.BlobPart, ...]
    cost_nanos_usd: int


@omp.entry_kind("examples.image-gen.cost", rev="v.1", display=False, spill=False)
@dataclass(frozen=True, slots=True)
class ImageGenerationCost:
    """Record provider-attributed cost for one settled image request."""

    provider: str
    route: str
    model: str
    count: int
    cost_nanos_usd: int


async def _request_image(payload: Mapping[str, object]) -> ImageResponse:
    """Send GENERATE_IMAGE through the provider handle once that frozen seam exists."""

    request = getattr(_IMAGE_PROVIDER, "request", None)
    if request is None:
        raise omp.NotWiredError("omp.ProviderHandle.request is not frozen")
    response = await request(
        omp.Operation.GENERATE_IMAGE,
        route=_ROUTE_ID,
        model=_DEFAULT_MODEL,
        payload=payload,
    )
    if not isinstance(response, ImageResponse):
        raise TypeError("image provider returned an unsupported response")
    return response


class ImageGenDevice:
    """Soft device routing generation through the declared image provider."""

    Payload = ImageGeneration

    async def __call__(self, args: ImageGenArgs, ctx: omp.Context) -> ImageGeneration:
        """Generate spill-backed images and journal the spine-attributed cost."""

        del ctx
        prompt = args.prompt.strip()
        if not prompt:
            raise ValueError("prompt must not be empty")
        if not 1 <= args.count <= 4:
            raise ValueError("count must be between 1 and 4")

        response = await _request_image(
            {
                "prompt": prompt,
                "size": args.size.value,
                "style": args.style.value,
                "count": args.count,
                "response_format": "bytes",
            }
        )
        if len(response.images) != args.count:
            raise ValueError("image provider returned an unexpected image count")
        if isinstance(response.usage.cost_nanos_usd, bool) or response.usage.cost_nanos_usd < 0:
            raise ValueError("image provider returned invalid usage cost")

        parts: list[omp.BlobPart] = []
        for number, image in enumerate(response.images, 1):
            if not image.media_type.startswith("image/"):
                raise ValueError("image provider returned a non-image media type")
            parts.append(
                omp.Part.blob(
                    omp.Spill(image.data, media_type=image.media_type),
                    f"Generated image {number}: {prompt}",
                )
            )

        omp.journal.append(
            ImageGenerationCost(
                provider=response.provider,
                route=response.route,
                model=response.model,
                count=len(parts),
                cost_nanos_usd=response.usage.cost_nanos_usd,
            )
        )
        return ImageGeneration(
            provider=response.provider,
            model=response.model,
            size=args.size,
            style=args.style,
            images=tuple(parts),
            cost_nanos_usd=response.usage.cost_nanos_usd,
        )

    def prompt(self, view: object, caps: omp.PromptCaps) -> list[object]:
        """Project generated images through the frozen blob-part budget path."""

        out = omp.Budget(caps)
        match view:
            case omp.Ok(payload):
                for image in payload.images:
                    if not out.push_blob(image.blob, image.alt or "Generated image"):
                        break
                return out.finish()
            case _:
                raise TypeError("image_gen prompt received an unsupported call outcome")


image_gen = omp.device(
    "image_gen",
    family="img",
    rev=1,
    place="host",
    schema=ImageGenArgs,
    summary="Generate images through a declared provider route.",
    effects=omp.Effects(inference=omp.InferenceEffects(max_requests=1, max_usd=1.0)),
)(ImageGenDevice())
