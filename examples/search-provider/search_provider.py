from __future__ import annotations

import asyncio
import json
from dataclasses import dataclass
from typing import Mapping, Sequence
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

import omp
from omp import (
    Api,
    AuthMode,
    AuthSpec,
    CredentialSource,
    ModelSpec,
    Operation,
    Payload,
    ProviderSpec,
    RouteSpec,
)


_AUTH = AuthSpec(
    mode=AuthMode.API_KEY,
    header="x-subscription-token",
    prefix="",
    sources=(CredentialSource.stored(), CredentialSource.env("BRAVE_SEARCH_API_KEY")),
)
_PROVIDER = ProviderSpec(
    id="brave-search",
    name="Brave Search",
    routes=(
        RouteSpec(
            id="web",
            base_url="https://api.search.brave.com/res/v1/web/search",
            api=Api.SEARCH_EXA,
            auth=_AUTH,
            headers={"accept": "application/json"},
        ),
    ),
    models=(
        ModelSpec(
            id="web",
            display_name="Brave Web Search",
            routes=("web",),
            operations=frozenset({Operation.SEARCH}),
        ),
    ),
)


@omp.provider(_PROVIDER)
class BraveSearch:
    """Declare the Brave SEARCH endpoint and brokered API-key placement."""


@dataclass(frozen=True, slots=True)
class SearchArgs:
    """Describe one page of a web search."""

    query: str
    page: int = 1
    page_size: int = 10


@dataclass(frozen=True, slots=True)
class SearchResult:
    """Represent one normalized ranked web result."""

    title: str
    url: str
    snippet: str
    rank: int


@dataclass(frozen=True, slots=True)
class SearchResults(Payload):
    """Return one requested page of normalized search results."""

    query: str
    page: int
    results: list[SearchResult]


@dataclass(frozen=True, slots=True)
class SearchFault(omp.Fault):
    """Report a typed search transport or response failure."""

    kind: str
    detail: str
    status: int | None = None


def _parse_results(payload: object, *, rank_offset: int = 0) -> list[SearchResult]:
    if not isinstance(payload, Mapping):
        raise ValueError("search response must be an object")
    web = payload.get("web", payload)
    if not isinstance(web, Mapping):
        raise ValueError("search response web field must be an object")
    raw_results = web.get("results")
    if not isinstance(raw_results, Sequence) or isinstance(raw_results, (str, bytes)):
        raise ValueError("search response results field must be a list")
    results: list[SearchResult] = []
    for raw in raw_results:
        if not isinstance(raw, Mapping):
            raise ValueError("each search result must be an object")
        title, url = raw.get("title"), raw.get("url")
        snippet = raw.get("description", raw.get("snippet", ""))
        if not isinstance(title, str) or not isinstance(url, str) or not isinstance(snippet, str):
            raise ValueError("search result title, url, and snippet must be strings")
        results.append(SearchResult(title, url, snippet, rank_offset + len(results) + 1))
    return results


def _search_sync(args: SearchArgs, api_key: bytes) -> SearchResults | SearchFault:
    page = max(1, args.page)
    page_size = min(20, max(1, args.page_size))
    offset = (page - 1) * page_size
    query = urlencode({"q": args.query, "count": page_size, "offset": offset})
    request = Request(
        f"https://api.search.brave.com/res/v1/web/search?{query}",
        headers={
            "accept": "application/json",
            "x-subscription-token": api_key.decode("utf-8"),
            "user-agent": "omp-search-provider/0.1",
        },
    )
    try:
        with urlopen(request, timeout=20) as response:
            payload = json.load(response)
    except HTTPError as error:
        return SearchFault("http", str(error.reason), error.code)
    except (URLError, OSError, ValueError, UnicodeError, json.JSONDecodeError) as error:
        return SearchFault("network", str(error))
    try:
        return SearchResults(args.query, page, _parse_results(payload, rank_offset=offset))
    except ValueError as error:
        return SearchFault("response", str(error))


@omp.device("web_search", family="ws", rev=1, place="env")
async def web_search(args: SearchArgs, ctx: omp.Context) -> SearchResults | SearchFault:
    """Search through the declared backend and return the requested ranked page."""

    credential_id = str(ctx.settings.get("credential_id", "")) or None
    secret = await omp.creds.reveal(id=credential_id, provider="brave-search")
    with secret.use() as api_key:
        return await asyncio.to_thread(_search_sync, args, bytes(api_key))
