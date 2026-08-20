"""A trust-aware ambient file probe with a fail-soft sandboxed arm."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import omp


_MAX_CHARS = 16_384


@dataclass(frozen=True, slots=True)
class TrustReadArgs:
    """Name an ambient file and bound the returned preview."""

    path: str
    max_chars: int = 4_096


@dataclass(frozen=True, slots=True)
class TrustReadResult:
    """Describe whether this tier permitted an ambient read attempt."""

    trust: omp.Trust
    access: str
    attempted: bool
    path: str
    content: str | None
    truncated: bool
    detail: str


def _sandboxed_result(args: TrustReadArgs) -> TrustReadResult:
    """Degrade without touching the ambient filesystem."""

    return TrustReadResult(
        trust=omp.Trust.SANDBOXED,
        access="broker-only",
        attempted=False,
        path=args.path,
        content=None,
        truncated=False,
        detail=(
            "Ambient files are unavailable at the sandboxed tier; use an "
            "omp.env document capability with a declared scope instead."
        ),
    )


def _trusted_result(args: TrustReadArgs) -> TrustReadResult:
    """Read one bounded ambient preview, reporting ordinary failures as data."""

    limit = min(max(args.max_chars, 0), _MAX_CHARS)
    path = Path(args.path).expanduser()
    try:
        with path.open("r", encoding="utf-8", errors="replace") as stream:
            content = stream.read(limit + 1)
    except OSError as error:
        return TrustReadResult(
            trust=omp.Trust.TRUSTED,
            access="ambient",
            attempted=True,
            path=str(path),
            content=None,
            truncated=False,
            detail=f"Ambient read unavailable: {error.strerror or type(error).__name__}",
        )

    truncated = len(content) > limit
    return TrustReadResult(
        trust=omp.Trust.TRUSTED,
        access="ambient",
        attempted=True,
        path=str(path),
        content=content[:limit],
        truncated=truncated,
        detail="Ambient preview read." if not truncated else "Ambient preview truncated.",
    )


@omp.device("trust_read", family="trust", rev=1, place="host")
async def trust_read(args: TrustReadArgs, ctx: omp.Context) -> TrustReadResult:
    """Read ambient content only when this child was installed as trusted."""

    if ctx.trust is omp.Trust.SANDBOXED:
        return _sandboxed_result(args)
    if ctx.trust is omp.Trust.TRUSTED:
        return _trusted_result(args)

    # A newer host tier must degrade until this extension understands its contract.
    return TrustReadResult(
        trust=ctx.trust,
        access="unknown-tier",
        attempted=False,
        path=args.path,
        content=None,
        truncated=False,
        detail="This trust tier is not recognized; no ambient access was attempted.",
    )
