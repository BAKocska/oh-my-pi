from __future__ import annotations

import asyncio
import re
import shlex
from collections.abc import Mapping, Sequence
from dataclasses import asdict, dataclass
from typing import Literal

import omp
from omp import ui


Severity = Literal["low", "medium", "high", "critical"]

_MAX_AREAS = 8
_MAX_FINDINGS_PER_AREA = 50
_MAX_DIFF_PREVIEW_BYTES = 24_000
_MAX_RATIONALE_CHARS = 1_000
_MAX_COMMENT_BYTES = 32_000
_POLL_SECONDS = 0.25
_AREA_RE = re.compile(r"^[a-z][a-z0-9_-]{0,31}$")
_SEVERITIES = frozenset({"low", "medium", "high", "critical"})
_SEVERITY_ORDER = {"critical": 0, "high": 1, "medium": 2, "low": 3}
_FINDING_KEYS = frozenset({"area", "severity", "path", "line", "rationale"})


@dataclass(frozen=True, slots=True)
class ReviewArgs:
    """Select the two revisions whose merge diff should be reviewed."""

    base: str = "HEAD^"
    head: str = "HEAD"


@dataclass(frozen=True, slots=True)
class Finding:
    """Represent one schema-validated review finding."""

    area: str
    severity: Severity
    path: str
    line: int
    rationale: str


@dataclass(frozen=True, slots=True)
class PublishArgs:
    """Select a pull request and validated findings to publish."""

    pr: str
    findings: tuple[Finding, ...]


@dataclass(frozen=True, slots=True)
class AreaConfig:
    """Declare one review area, model tier, and finite hard budget."""

    area: str
    model: str
    thinking: omp.agents.ThinkingLevel
    budget: omp.agents.Budget


@dataclass(frozen=True, slots=True)
class AreaResult:
    """Summarize one child without retaining its transcript or raw output."""

    area: str
    model: str
    status: str
    accepted: int
    rejected: int


@dataclass(frozen=True, slots=True)
class ReviewReport(omp.Payload):
    """Return only validated findings and bounded review metadata."""

    base: str
    head: str
    findings: tuple[Finding, ...]
    rejected_findings: int
    areas: tuple[AreaResult, ...]
    diff_bytes: int
    diff_blob: omp.BlobRef | None


@dataclass(frozen=True, slots=True)
class PublishReceipt(omp.Payload):
    """Describe one approved GitHub comment publication."""

    pr: str
    findings: int
    exit_code: int
    output_blob: omp.BlobRef | None


@dataclass(frozen=True, slots=True)
class DiffCapture:
    """Keep a bounded diff preview and the full spill identity when needed."""

    preview: str
    byte_length: int
    blob: omp.BlobRef | None


@dataclass(frozen=True, slots=True)
class LiveRow:
    """Hold only the sanitized fields allowed into the live progress rail."""

    area: str
    status: str
    activity: str


def _positive_int(value: object, field: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ValueError(f"{field} must be a positive integer")
    return value


def _positive_number(value: object, field: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value <= 0:
        raise ValueError(f"{field} must be a positive number")
    return float(value)


def _area_configs(ctx: omp.Context) -> tuple[AreaConfig, ...]:
    raw_areas = ctx.settings.get("areas", ())
    if not isinstance(raw_areas, Sequence) or isinstance(raw_areas, (str, bytes)):
        raise ValueError("settings.areas must be an array of tables")
    if not 1 <= len(raw_areas) <= _MAX_AREAS:
        raise ValueError(f"settings.areas must contain 1 to {_MAX_AREAS} entries")

    configs: list[AreaConfig] = []
    names: set[str] = set()
    for raw in raw_areas:
        if not isinstance(raw, Mapping):
            raise ValueError("each review area must be a table")
        area = str(raw.get("area", "")).strip()
        model = str(raw.get("model", "")).strip()
        if _AREA_RE.fullmatch(area) is None:
            raise ValueError(f"invalid review area {area!r}")
        if area in names:
            raise ValueError(f"duplicate review area {area!r}")
        if not model:
            raise ValueError(f"review area {area!r} must declare a model tier")
        try:
            thinking = omp.agents.ThinkingLevel(str(raw.get("thinking", "med")))
            max_wall = omp.Duration(str(raw["max_wall"]))
        except (KeyError, TypeError, ValueError) as error:
            raise ValueError(f"review area {area!r} has invalid thinking or max_wall") from error
        budget = omp.agents.Budget(
            max_requests=_positive_int(raw.get("max_requests"), f"{area}.max_requests"),
            max_input_tokens=_positive_int(
                raw.get("max_input_tokens"), f"{area}.max_input_tokens"
            ),
            max_output_tokens=_positive_int(
                raw.get("max_output_tokens"), f"{area}.max_output_tokens"
            ),
            max_usd=_positive_number(raw.get("max_usd"), f"{area}.max_usd"),
            max_wall=max_wall,
        )
        configs.append(AreaConfig(area, model, thinking, budget))
        names.add(area)
    return tuple(configs)


def _finding_schema(area: str) -> dict[str, object]:
    return {
        "type": "object",
        "additionalProperties": False,
        "required": ["findings"],
        "properties": {
            "findings": {
                "type": "array",
                "maxItems": _MAX_FINDINGS_PER_AREA,
                "items": {
                    "type": "object",
                    "additionalProperties": False,
                    "required": ["area", "severity", "path", "line", "rationale"],
                    "properties": {
                        "area": {"const": area},
                        "severity": {
                            "type": "string",
                            "enum": ["low", "medium", "high", "critical"],
                        },
                        "path": {"type": "string", "minLength": 1, "maxLength": 512},
                        "line": {"type": "integer", "minimum": 1},
                        "rationale": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": _MAX_RATIONALE_CHARS,
                        },
                    },
                },
            }
        },
    }


def _clean_line(value: object, limit: int = 80) -> str:
    clean = " ".join(
        "".join(character for character in str(value) if ord(character) >= 32).split()
    )
    return clean[:limit]


def _progress_rail(rows: Sequence[LiveRow]) -> ui.Tml:
    bounded = rows[:_MAX_AREAS]
    rendered = [
        ui.tml(
            "<row gap=1><text bold>{area}</text><text fg=muted>{status}</text>"
            "<text>{activity}</text></row>",
            area=_clean_line(row.area, 32),
            status=_clean_line(row.status, 16),
            activity=_clean_line(row.activity),
        )
        for row in bounded
    ]
    return ui.tml(
        "<box title='PR review' border=round noselect><col gap=1>{rows}</col></box>",
        rows=rendered,
    )


def _validate_ref(value: str, field: str) -> str:
    if not value or len(value) > 200 or any(ord(character) < 32 for character in value):
        raise ValueError(f"{field} must be a bounded git revision")
    return value


async def _capture_diff(args: ReviewArgs) -> DiffCapture:
    base = _validate_ref(args.base, "base")
    head = _validate_ref(args.head, "head")
    script = (
        "git diff --no-ext-diff --no-color --unified=80 "
        f"{shlex.quote(base)}...{shlex.quote(head)} --"
    )
    completed = await omp.env.sh.run(script, timeout=omp.Duration("60s"))
    if completed.outcome is not omp.env.Outcome.EXITED or completed.exit_code != 0:
        raise RuntimeError("git diff failed")

    output = completed.output
    blob = completed.artifact
    byte_length = blob.size if blob is not None else len(output)
    if len(output) > _MAX_DIFF_PREVIEW_BYTES:
        if blob is None:
            blob = await omp.env.blobs.put(output)
            byte_length = len(output)
        output = output[:_MAX_DIFF_PREVIEW_BYTES]
    preview = output.decode("utf-8", errors="replace")
    return DiffCapture(preview, byte_length, blob)


def _review_task(config: AreaConfig, diff: DiffCapture) -> str:
    spill_note = ""
    if diff.blob is not None:
        spill_note = (
            f"\nThe full diff spilled to Environment blob {diff.blob.hex} "
            f"({diff.blob.size} bytes). Inspect workspace files with core tools when the "
            "bounded preview omits needed context."
        )
    return (
        f"Review the merge diff only for the {config.area!r} area. "
        "Return the structured payload required by the declared output schema. "
        "Each finding must identify an affected workspace-relative path and line, use "
        "one declared severity, and give a concise evidence-based rationale. Return an "
        "empty findings array when there is no actionable issue. Do not publish comments."
        f"{spill_note}\n\nBounded diff preview:\n{diff.preview}"
    )


def _spec(config: AreaConfig, diff: DiffCapture) -> omp.agents.SubagentSpec:
    return omp.agents.SubagentSpec(
        task=_review_task(config, diff),
        name=("Review" + config.area.title().replace("_", "").replace("-", ""))[:32],
        agent="reviewer",
        model=config.model,
        on_model_unavailable="fail",
        thinking=config.thinking,
        allowed_devices=frozenset(),
        isolation=omp.agents.Isolation.CLEAN,
        max_depth=0,
        output_schema=_finding_schema(config.area),
        schema_mode="strict",
        request_budget=config.budget.max_requests,
        budget=config.budget,
        labels={"review_area": config.area, "model_tier": config.model},
    )


async def _live_row(area: str, handle: omp.agents.SubagentHandle) -> LiveRow:
    try:
        progress = await handle.progress()
    except Exception:
        return LiveRow(area, "running", "status unavailable")
    return LiveRow(area, progress.status.value, progress.activity)


async def _wait_wave(
    configs: Sequence[AreaConfig],
    handles: Sequence[omp.agents.SubagentHandle],
    *,
    show_ui: bool,
) -> list[omp.agents.SubagentResult]:
    rail: ui.SlotHandle | None = None
    if show_ui:
        rail = ui.mount(
            ui.Slot.SIDEBAR_RIGHT,
            _progress_rail([LiveRow(config.area, "pending", "queued") for config in configs]),
            ui.SlotOptions(width=34, min_width=100, collapse=ui.Collapse.SHRINK),
            key="pr-review-progress",
        )

    tasks = [asyncio.create_task(handle.wait()) for handle in handles]
    pending: set[asyncio.Task[omp.agents.SubagentResult]] = set(tasks)
    try:
        while pending:
            if rail is not None:
                rows = await asyncio.gather(
                    *(
                        _live_row(config.area, handle)
                        for config, handle in zip(configs, handles, strict=True)
                    )
                )
                rail.set(_progress_rail(rows))
            _, pending = await asyncio.wait(
                pending,
                timeout=_POLL_SECONDS,
                return_when=asyncio.FIRST_COMPLETED,
            )
        return [task.result() for task in tasks]
    finally:
        for task in pending:
            task.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        if rail is not None:
            rail.unmount()


def _parse_finding(area: str, raw: object) -> Finding:
    if not isinstance(raw, Mapping) or frozenset(raw) != _FINDING_KEYS:
        raise ValueError("finding must contain exactly the declared fields")
    raw_area = raw.get("area")
    severity = raw.get("severity")
    path = raw.get("path")
    line = raw.get("line")
    rationale = raw.get("rationale")
    if raw_area != area:
        raise ValueError("finding area does not match its reviewer")
    if severity not in _SEVERITIES:
        raise ValueError("finding severity is not declared")
    if not isinstance(path, str) or not 1 <= len(path) <= 512:
        raise ValueError("finding path is invalid")
    parts = path.replace("\\", "/").split("/")
    if path.startswith(("/", "\\")) or ".." in parts or any(ord(ch) < 32 for ch in path):
        raise ValueError("finding path must be workspace-relative")
    if isinstance(line, bool) or not isinstance(line, int) or line < 1:
        raise ValueError("finding line must be a positive integer")
    if (
        not isinstance(rationale, str)
        or not rationale.strip()
        or len(rationale) > _MAX_RATIONALE_CHARS
        or any(ord(ch) < 32 and ch not in "\t" for ch in rationale)
    ):
        raise ValueError("finding rationale is invalid")
    return Finding(area, severity, path, line, rationale.strip())


def _validated_result(
    config: AreaConfig, result: omp.agents.SubagentResult
) -> tuple[tuple[Finding, ...], AreaResult]:
    status = result.status.value
    raw_findings: object = None
    if result.status is omp.agents.RunStatus.COMPLETED and isinstance(result.data, Mapping):
        if frozenset(result.data) == {"findings"}:
            raw_findings = result.data.get("findings")
    if not isinstance(raw_findings, Sequence) or isinstance(raw_findings, (str, bytes)):
        return (), AreaResult(config.area, result.model, status, 0, 1)

    accepted: list[Finding] = []
    rejected = 0
    if len(raw_findings) > _MAX_FINDINGS_PER_AREA:
        return (), AreaResult(config.area, result.model, status, 0, len(raw_findings))
    for raw in raw_findings:
        try:
            accepted.append(_parse_finding(config.area, raw))
        except ValueError:
            rejected += 1
    return tuple(accepted), AreaResult(
        config.area, result.model, status, len(accepted), rejected
    )


async def _run_review(args: ReviewArgs, ctx: omp.Context) -> ReviewReport:
    configs = _area_configs(ctx)
    diff = await _capture_diff(args)
    handles = await omp.agents.spawn_all([_spec(config, diff) for config in configs])
    if len(handles) != len(configs):
        raise RuntimeError("spawn_all returned a partial review wave")
    results = await _wait_wave(configs, handles, show_ui=ctx.has_ui)

    findings: list[Finding] = []
    area_results: list[AreaResult] = []
    rejected = 0
    for config, result in zip(configs, results, strict=True):
        valid, summary = _validated_result(config, result)
        findings.extend(valid)
        area_results.append(summary)
        rejected += summary.rejected
    findings.sort(
        key=lambda finding: (
            _SEVERITY_ORDER[finding.severity],
            finding.path,
            finding.line,
            finding.area,
        )
    )
    return ReviewReport(
        base=args.base,
        head=args.head,
        findings=tuple(findings),
        rejected_findings=rejected,
        areas=tuple(area_results),
        diff_bytes=diff.byte_length,
        diff_blob=diff.blob,
    )


def _publication_findings(args: PublishArgs, ctx: omp.Context) -> tuple[Finding, ...]:
    allowed = {config.area for config in _area_configs(ctx)}
    if not args.findings:
        raise ValueError("publish requires at least one finding")
    accepted: list[Finding] = []
    for finding in args.findings:
        if not isinstance(finding, Finding) or finding.area not in allowed:
            raise ValueError("publish accepts only findings from declared review areas")
        accepted.append(_parse_finding(finding.area, asdict(finding)))
    return tuple(accepted)


def _comment(findings: Sequence[Finding]) -> str:
    lines = ["## Automated PR review", ""]
    for finding in findings:
        safe_path = finding.path.replace("`", "'")
        rationale = finding.rationale.replace("\r", " ").replace("\n", " ")
        lines.append(
            f"- **{finding.severity.upper()}** `{safe_path}:{finding.line}` "
            f"({finding.area}): {rationale}"
        )
    body = "\n".join(lines)
    if len(body.encode("utf-8")) > _MAX_COMMENT_BYTES:
        raise ValueError("published review comment exceeds the hard byte budget")
    return body


review = omp.device(
    "review",
    family="pr-review",
    rev=1,
    place="host",
    summary="Run parallel model-tiered reviewers over a bounded git diff.",
    effects=omp.Effects(
        exec=omp.ExecEffects(commands=("git", "gh")),
        inference=omp.InferenceEffects(max_requests=32, max_usd=12.0),
        subagents=_MAX_AREAS,
    ),
    tier=omp.Tier.READ,
)(_run_review)


@review.subtool("publish")
async def publish_review(args: PublishArgs, ctx: omp.Context) -> PublishReceipt:
    """Publish validated findings after the Core-owned approval gate resolves."""

    pr = _validate_ref(args.pr, "pr")
    findings = _publication_findings(args, ctx)
    body = _comment(findings)
    script = f"gh pr comment {shlex.quote(pr)} --body {shlex.quote(body)}"
    completed = await omp.env.sh.run(script, timeout=omp.Duration("60s"))
    if completed.outcome is not omp.env.Outcome.EXITED or completed.exit_code != 0:
        raise RuntimeError("GitHub comment publication failed")
    return PublishReceipt(pr, len(findings), completed.exit_code, completed.artifact)


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.APPROVAL,
    on_failure=omp.OnFailure.DENY,
    when=omp.When(
        target=frozenset({omp.TargetKind.DEVICE}),
        name=frozenset({"review/publish"}),
    ),
)
async def approve_publish(
    event: omp.ToolCallEvent, ctx: omp.Context
) -> omp.HookDecision:
    """File one durable human ticket for the comment-publishing subtool."""

    del ctx
    if not isinstance(event.target, omp.DeviceCall) or event.target.name != "review/publish":
        return omp.Defer()
    pr = _clean_line(event.args.get("pr", "pull request"), 120)
    return omp.RequireApproval(
        omp.ApprovalSpec(
            title="Publish PR review comment",
            body="Publish validated findings as one GitHub pull-request comment.",
            subject=pr,
            kind=omp.ApprovalKind.WRITE,
            scopes=(omp.PolicyScope.ONCE,),
            require_human=True,
            evidence=("review/publish",),
        )
    )
