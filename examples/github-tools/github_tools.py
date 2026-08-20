from __future__ import annotations

import dataclasses
import json
from collections.abc import Mapping, Sequence
from typing import Literal
from urllib.parse import quote, urlencode

import omp
from omp import Budget, Faulted, Ok, Payload, SpillBudget


GitHubPath = Literal[
    "pr/list",
    "pr/view",
    "pr/comment",
    "ci/runs",
    "issue/list",
    "issue/comment",
    "release/list",
]

_API = "https://api.github.com"
_PROVIDER = "github"
_MAX_ROWS = 20
_MUTATING_PATHS = frozenset({"pr/comment", "issue/comment"})


@dataclasses.dataclass(frozen=True, slots=True)
class GitHubArgs:
    """Select one GitHub sub-path and its repository-scoped arguments."""

    path: GitHubPath
    owner: str
    repo: str
    number: int | None = None
    state: str = "open"
    page: int = 1
    limit: int = 20
    branch: str | None = None
    body: str | None = None


@dataclasses.dataclass(frozen=True, slots=True)
class RateLimit:
    """Retain GitHub's typed rate-limit response fields."""

    limit: int | None
    remaining: int | None
    used: int | None
    reset_at_epoch: int | None
    resource: str | None
    retry_after_seconds: int | None


@dataclasses.dataclass(frozen=True, slots=True)
class GitHubRow:
    """Represent one bounded model-facing GitHub result row."""

    id: int | None
    number: int | None
    name: str
    state: str | None
    url: str | None
    updated_at: str | None
    secondary: str | None


@dataclasses.dataclass(frozen=True, slots=True)
class GitHubPayload(Payload):
    """Keep bounded rows beside the complete redacted API response."""

    path: str
    owner: str
    repo: str
    status: int
    rows: list[GitHubRow]
    total_rows: int
    details_json: bytes
    rate_limit: RateLimit


@dataclasses.dataclass(frozen=True, slots=True)
class GitHubFault(omp.Fault):
    """Describe a typed GitHub request or response failure."""

    path: str
    status: int | None
    detail: str
    rate_limit: RateLimit | None = None


def _integer(value: object) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value)
        except ValueError:
            return None
    return None


def _text(value: object) -> str | None:
    return value if isinstance(value, str) and value else None


def _rate_limit(headers: Mapping[str, str], token: str) -> RateLimit:
    lowered = {key.casefold(): value for key, value in headers.items()}
    resource = _text(lowered.get("x-ratelimit-resource"))
    if resource is not None and token:
        resource = resource.replace(token, "[REDACTED]")
    return RateLimit(
        limit=_integer(lowered.get("x-ratelimit-limit")),
        remaining=_integer(lowered.get("x-ratelimit-remaining")),
        used=_integer(lowered.get("x-ratelimit-used")),
        reset_at_epoch=_integer(lowered.get("x-ratelimit-reset")),
        resource=resource,
        retry_after_seconds=_integer(lowered.get("retry-after")),
    )


def _redact(value: object, token: str) -> object:
    if isinstance(value, str):
        return value.replace(token, "[REDACTED]") if token else value
    if isinstance(value, list):
        return [_redact(item, token) for item in value]
    if isinstance(value, Mapping):
        return {
            (str(key).replace(token, "[REDACTED]") if token else str(key)): _redact(
                item, token
            )
            for key, item in value.items()
        }
    return value


def _canonical_json(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode(
        "utf-8"
    )


def _validate(args: GitHubArgs) -> GitHubFault | None:
    if not args.owner.strip() or not args.repo.strip():
        return GitHubFault(args.path, None, "owner and repo must be non-empty")
    if args.page < 1 or not 1 <= args.limit <= 100:
        return GitHubFault(args.path, None, "page must be positive and limit must be 1..100")
    if args.path in {"pr/view", "pr/comment", "issue/comment"} and (
        args.number is None or args.number < 1
    ):
        return GitHubFault(args.path, None, "this sub-path requires a positive number")
    if args.path in _MUTATING_PATHS and not (args.body or "").strip():
        return GitHubFault(args.path, None, "comment body must be non-empty")
    return None


def _endpoint(args: GitHubArgs) -> tuple[str, bytes | None]:
    base = f"{_API}/repos/{quote(args.owner.strip(), safe='')}/{quote(args.repo.strip(), safe='')}"
    paging: dict[str, str | int] = {"per_page": args.limit, "page": args.page}
    if args.path == "pr/list":
        return f"{base}/pulls?{urlencode({**paging, 'state': args.state})}", None
    if args.path == "pr/view":
        return f"{base}/pulls/{args.number}", None
    if args.path == "ci/runs":
        query = dict(paging)
        if args.branch:
            query["branch"] = args.branch
        return f"{base}/actions/runs?{urlencode(query)}", None
    if args.path == "issue/list":
        return f"{base}/issues?{urlencode({**paging, 'state': args.state})}", None
    if args.path == "release/list":
        return f"{base}/releases?{urlencode(paging)}", None
    if args.path in _MUTATING_PATHS:
        return f"{base}/issues/{args.number}/comments", _canonical_json({"body": args.body})
    raise ValueError(f"unknown GitHub sub-path: {args.path}")


def _items(path: str, value: object) -> list[Mapping[str, object]]:
    if path == "ci/runs" and isinstance(value, Mapping):
        value = value.get("workflow_runs")
    elif path == "pr/view" or path in _MUTATING_PATHS:
        value = [value]
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes, bytearray)):
        return []
    rows = [item for item in value if isinstance(item, Mapping)]
    if path == "issue/list":
        rows = [item for item in rows if "pull_request" not in item]
    return rows


def _row(path: str, item: Mapping[str, object]) -> GitHubRow:
    user = item.get("user")
    actor = _text(user.get("login")) if isinstance(user, Mapping) else None
    if path == "ci/runs":
        name = _text(item.get("display_title")) or _text(item.get("name")) or "(unnamed run)"
        state = _text(item.get("conclusion")) or _text(item.get("status"))
        secondary = _text(item.get("head_branch"))
    elif path == "release/list":
        name = _text(item.get("name")) or _text(item.get("tag_name")) or "(unnamed release)"
        flags = []
        if item.get("draft") is True:
            flags.append("draft")
        if item.get("prerelease") is True:
            flags.append("prerelease")
        state = ",".join(flags) or "published"
        secondary = _text(item.get("tag_name"))
    elif path in _MUTATING_PATHS:
        name = _text(item.get("body")) or "comment created"
        state = None
        secondary = actor
    else:
        name = _text(item.get("title")) or "(untitled)"
        state = _text(item.get("state"))
        secondary = actor
    return GitHubRow(
        id=_integer(item.get("id")),
        number=_integer(item.get("number")) or _integer(item.get("run_number")),
        name=name,
        state=state,
        url=_text(item.get("html_url")),
        updated_at=_text(item.get("updated_at")) or _text(item.get("published_at")),
        secondary=secondary,
    )


async def _headers() -> tuple[dict[str, str], str]:
    scoped = await omp.creds.mint_scoped(
        "github-rest", ttl=omp.Duration("2m"), provider=_PROVIDER
    )
    return (
        {
            "accept": "application/vnd.github+json",
            "authorization": f"Bearer {scoped.token}",
            "x-github-api-version": "2022-11-28",
        },
        scoped.token,
    )


async def _dispatch(args: GitHubArgs) -> GitHubPayload | GitHubFault:
    invalid = _validate(args)
    if invalid is not None:
        return invalid

    url, body = _endpoint(args)
    headers, token = await _headers()
    try:
        if body is None:
            response = await omp.env.http_get(
                url, headers=headers, timeout=omp.Duration("15s")
            )
        else:
            response = await omp.env.http_post(
                url, body=body, headers=headers, timeout=omp.Duration("15s")
            )
    except Exception as error:
        if isinstance(error, omp.NotWiredError):
            raise
        return GitHubFault(args.path, None, type(error).__name__)

    rate = _rate_limit(response.headers, token)
    if not 200 <= response.status < 300:
        return GitHubFault(
            args.path,
            response.status,
            f"GitHub returned HTTP {response.status}",
            rate,
        )
    try:
        decoded = response.json()
    except (UnicodeDecodeError, json.JSONDecodeError):
        return GitHubFault(args.path, response.status, "GitHub returned invalid JSON", rate)

    redacted = _redact(decoded, token)
    items = _items(args.path, redacted)
    return GitHubPayload(
        path=args.path,
        owner=args.owner.strip(),
        repo=args.repo.strip(),
        status=response.status,
        rows=[_row(args.path, item) for item in items[:_MAX_ROWS]],
        total_rows=len(items),
        details_json=_canonical_json(redacted),
        rate_limit=rate,
    )


def _cell(value: object) -> str:
    return str(value if value is not None else "-").replace("|", "\\|").replace("\n", " ")


def _rate_text(rate: RateLimit) -> str:
    remaining = "?" if rate.remaining is None else str(rate.remaining)
    limit = "?" if rate.limit is None else str(rate.limit)
    resource = rate.resource or "unknown"
    return f"rate: {remaining}/{limit} remaining ({resource})"


class GitHub:
    """Dispatch typed GitHub operations without creating per-operation tool slots."""

    Payload = GitHubPayload
    Fault = GitHubFault
    __spill__ = SpillBudget(inline_limit=64 * 1024)

    async def __call__(
        self, args: GitHubArgs, ctx: omp.Context
    ) -> GitHubPayload | GitHubFault:
        """Call the selected GitHub REST sub-path with a scoped credential."""

        del ctx
        return await _dispatch(args)

    def prompt(
        self,
        view: Ok[GitHubPayload] | Faulted[GitHubFault],
        caps: omp.PromptCaps,
    ) -> list[omp.TextPart | omp.JsonPart | omp.BlobPart]:
        """Project a byte-budgeted table while retaining complete typed details."""

        out = Budget(caps)
        match view:
            case Ok(payload):
                out.push(
                    f"{payload.path} · {payload.owner}/{payload.repo} · "
                    f"{payload.total_rows} result(s) · {_rate_text(payload.rate_limit)}\n"
                    "| # | name | state | updated | secondary |\n"
                    "|---:|---|---|---|---|\n"
                )
                for row in payload.rows:
                    if not out.push(
                        f"| {_cell(row.number or row.id)} | {_cell(row.name)} | "
                        f"{_cell(row.state)} | {_cell(row.updated_at)} | "
                        f"{_cell(row.secondary)} |\n"
                    ):
                        break
            case Faulted(fault):
                suffix = "" if fault.rate_limit is None else f" · {_rate_text(fault.rate_limit)}"
                out.push(f"{fault.path} failed: {fault.detail}{suffix}")
            case _:
                raise TypeError("github prompt received an unsupported call outcome")
        return out.finish()


github = omp.device(
    "github",
    family="github",
    rev=1,
    place="host",
    summary="List and inspect GitHub repository resources through typed sub-path dispatch.",
    effects=omp.Effects(exec=omp.ExecEffects(network=True)),
    tier=omp.Tier.WRITE,
)(GitHub())


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.APPROVAL,
    on_failure=omp.OnFailure.DENY,
    when=omp.When(target=frozenset({omp.TargetKind.DEVICE}), name=frozenset({"github"})),
)
async def approve_github_comment(
    payload: omp.ToolCallEvent, ctx: omp.Context
) -> omp.HookDecision:
    """Open a durable human approval ticket for either comment sub-path."""

    del ctx
    path = payload.args.get("path")
    if path not in _MUTATING_PATHS:
        return omp.Defer()
    owner = str(payload.args.get("owner", ""))
    repo = str(payload.args.get("repo", ""))
    number = payload.args.get("number")
    return omp.RequireApproval(
        omp.ApprovalSpec(
            title="Post GitHub comment",
            body="Post one comment through the GitHub REST API.",
            subject=f"{owner}/{repo}#{number}",
            kind=omp.ApprovalKind.WRITE,
            scopes=(omp.PolicyScope.ONCE,),
            require_human=True,
            evidence=(f"github/{path}",),
        )
    )
