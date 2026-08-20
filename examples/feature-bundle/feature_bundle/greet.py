"""Optional greeting device for the feature-bundle example."""

from __future__ import annotations

from dataclasses import dataclass

import omp
from omp import Payload


@dataclass(frozen=True, slots=True)
class GreetArgs:
    """Name to greet."""

    name: str


@dataclass(frozen=True, slots=True)
class GreetPayload(Payload):
    """Structured greeting returned to the caller."""

    greeting: str


@omp.device("bundle_greet", family="v", rev=1, place="host")
async def bundle_greet(args: GreetArgs, ctx: omp.Context) -> GreetPayload:
    """Return a compact greeting without touching the environment."""

    del ctx
    name = args.name.strip() or "there"
    return GreetPayload(f"Hello, {name}!")
