from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import omp
from omp import Duration, QuotaExceeded, ResourceReceipt


_JOURNAL_QUOTA = "journal.appends"


@omp.entry_kind(
    "examples.resource-receipts.probe", rev="v.1", display=False, spill=False
)
@dataclass(frozen=True, slots=True)
class ReceiptProbe:
    """Record one admitted probe without maintaining extension-local usage."""

    quota: str
    consumed_before: int
    limit: int


@dataclass(frozen=True, slots=True)
class ResourceReceiptsArgs:
    """Choose whether to exercise the hard journal quota after reading it."""

    record_probe: bool = False


@dataclass(frozen=True, slots=True)
class QuotaBudget:
    """Normalize one core-owned quota row for a device result."""

    quota: str
    limit: int
    consumed: int
    remaining: int
    window: Duration | None
    dropped: int


@dataclass(frozen=True, slots=True)
class ResourceReceiptsReport:
    """Return the complete receipt and the probe's declarative disposition."""

    quotas: tuple[QuotaBudget, ...]
    disposition: Literal["observed", "recorded", "deferred"]
    reason: str | None = None


def _budget(receipt: ResourceReceipt, quota: str) -> QuotaBudget:
    """Project one immutable receipt row without a shadow counter."""

    status = receipt.quotas.get(quota)
    if status is None:
        raise KeyError(f"resource receipt has no {quota!r} quota")
    return QuotaBudget(
        quota=quota,
        limit=status.limit,
        consumed=status.used,
        remaining=max(status.limit - status.used, 0),
        window=status.window,
        dropped=receipt.dropped.get(quota, 0),
    )


def _report(
    receipt: ResourceReceipt,
    disposition: Literal["observed", "recorded", "deferred"],
    reason: str | None = None,
) -> ResourceReceiptsReport:
    """Render every quota in stable name order from one atomic receipt."""

    return ResourceReceiptsReport(
        quotas=tuple(_budget(receipt, quota) for quota in sorted(receipt.quotas)),
        disposition=disposition,
        reason=reason,
    )


def _receipt_from_exhaustion(error: QuotaExceeded) -> tuple[str, ResourceReceipt]:
    """Recover the refused quota and its receipt, rereading until carriers freeze."""

    return (
        error.quota or _JOURNAL_QUOTA,
        error.receipt if error.receipt is not None else omp.resources(),
    )


@omp.device("resource_receipts", family="resources", rev=1, place="host")
async def resource_receipts(
    args: ResourceReceiptsArgs, ctx: omp.Context
) -> ResourceReceiptsReport:
    """Read live quota standing and optionally exercise one hard-quota arm."""

    del ctx
    receipt = omp.resources()
    if not args.record_probe:
        return _report(receipt, "observed")

    journal = _budget(receipt, _JOURNAL_QUOTA)
    if journal.remaining == 0:
        return _report(
            receipt,
            "deferred",
            f"{_JOURNAL_QUOTA} has no remaining core-owned budget",
        )

    try:
        omp.journal.append(
            ReceiptProbe(
                quota=_JOURNAL_QUOTA,
                consumed_before=journal.consumed,
                limit=journal.limit,
            )
        )
    except QuotaExceeded as error:
        quota, refused_receipt = _receipt_from_exhaustion(error)
        return _report(
            refused_receipt,
            "deferred",
            f"{quota} was refused by the core quota ledger",
        )

    return _report(omp.resources(), "recorded")
