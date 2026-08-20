"""Demonstrate declarative, centrally finalized device arguments."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Annotated

import omp


@dataclass(frozen=True, slots=True)
class CreateIssueArgs:
    """Describe one issue after central argument finalization."""

    project_key: Annotated[
        str,
        omp.Field(
            alias=("projectKey", "project"),
            coerce=(omp.Coerce.STRING, omp.Coerce.STRIP),
            expected="a non-empty project key string",
            example="OPS",
        ),
    ]
    summary: Annotated[
        str,
        omp.Field(
            alias=("title", "subject"),
            coerce=(omp.Coerce.STRING, omp.Coerce.STRIP),
            expected="a concise issue summary string",
            example="Repair duplicate aliases centrally",
        ),
    ]
    labels: Annotated[
        list[str],
        omp.Field(
            alias=("label", "tags"),
            coerce=(omp.Coerce.CSV, omp.Coerce.SINGLETON),
            expected="a list of label strings",
            example='["arguments", "repair"]',
        ),
    ]


@dataclass(frozen=True, slots=True)
class CreatedIssue(omp.Payload):
    """Return the canonical arguments received by the device."""

    project_key: str
    summary: str
    labels: tuple[str, ...]


@omp.device(
    "arg_repair",
    family="repair-demo",
    rev=1,
    place="host",
    schema=CreateIssueArgs,
    summary="Echo centrally finalized issue arguments.",
)
async def arg_repair(args: CreateIssueArgs, ctx: omp.Context) -> CreatedIssue:
    """Return already-finalized arguments without validating or repairing them."""
    return CreatedIssue(args.project_key, args.summary, tuple(args.labels))
