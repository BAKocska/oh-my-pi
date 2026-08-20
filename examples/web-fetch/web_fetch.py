from __future__ import annotations

import asyncio
import dataclasses
import re
import socket
import ssl
from html.parser import HTMLParser
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

import omp
from omp import Budget, Faulted, Ok, Payload, SpillBudget


@dataclasses.dataclass(frozen=True, slots=True)
class FetchArgs:
    """Arguments for fetching one public HTTP or HTTPS page."""

    url: str


@dataclasses.dataclass(frozen=True, slots=True)
class FetchPayload(Payload):
    """Durable structured truth from a successful web fetch."""

    url: str
    status: int
    title: str | None
    text: str


@dataclasses.dataclass(frozen=True, slots=True)
class FetchFault(omp.Fault):
    """Typed failure returned when a page cannot be fetched or extracted."""

    kind: str
    status: int | None
    detail: str


_BLOCK_TAGS = frozenset(
    {
        "address",
        "article",
        "aside",
        "blockquote",
        "br",
        "dd",
        "div",
        "dl",
        "dt",
        "figcaption",
        "figure",
        "footer",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "header",
        "hr",
        "li",
        "main",
        "nav",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "td",
        "th",
        "tr",
        "ul",
    }
)
_SKIP_TAGS = frozenset({"script", "style", "template", "noscript", "svg"})


class _HtmlTextExtractor(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self._skip_depth = 0
        self._in_title = False
        self._title_parts: list[str] = []
        self._text_parts: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        del attrs
        tag = tag.lower()
        if tag in _SKIP_TAGS:
            self._skip_depth += 1
            return
        if self._skip_depth:
            return
        if tag == "title":
            self._in_title = True
        if tag in _BLOCK_TAGS:
            self._text_parts.append("\n")

    def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        self.handle_starttag(tag, attrs)
        if tag.lower() in _SKIP_TAGS:
            self.handle_endtag(tag)

    def handle_endtag(self, tag: str) -> None:
        tag = tag.lower()
        if tag in _SKIP_TAGS:
            if self._skip_depth:
                self._skip_depth -= 1
            return
        if self._skip_depth:
            return
        if tag == "title":
            self._in_title = False
        if tag in _BLOCK_TAGS:
            self._text_parts.append("\n")

    def handle_data(self, data: str) -> None:
        if self._skip_depth:
            return
        if self._in_title:
            self._title_parts.append(data)
            return
        self._text_parts.append(data)

    def result(self) -> tuple[str | None, str]:
        """Return normalized title and visible text without truncating content."""
        title = _normalize_inline("".join(self._title_parts)) or None
        text = "".join(self._text_parts).replace("\r\n", "\n").replace("\r", "\n")
        lines = (_normalize_inline(line) for line in text.split("\n"))
        return title, "\n".join(line for line in lines if line)


def _normalize_inline(value: str) -> str:
    return re.sub(r"\s+", " ", value).strip()


def _fetch_sync(args: FetchArgs) -> FetchPayload | FetchFault:
    parsed = urlsplit(args.url)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        return FetchFault("url", None, "url must be an absolute HTTP or HTTPS URL")

    request = Request(
        args.url,
        headers={
            "Accept": "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1",
            "User-Agent": "omp-web-fetch/0.1",
        },
    )
    try:
        with urlopen(request) as response:
            status = response.status
            final_url = response.geturl()
            media_type = response.headers.get_content_type()
            if media_type not in {"text/html", "application/xhtml+xml"}:
                return FetchFault("not_html", status, f"unsupported content type: {media_type}")
            charset = response.headers.get_content_charset() or "utf-8"
            body = response.read()
    except HTTPError as error:
        return FetchFault("http", error.code, str(error.reason))
    except URLError as error:
        reason = error.reason
        if isinstance(reason, socket.gaierror):
            kind = "dns"
        elif isinstance(reason, ssl.SSLError):
            kind = "tls"
        else:
            kind = "network"
        return FetchFault(kind, None, str(reason))
    except (OSError, ValueError) as error:
        return FetchFault("network", None, str(error))

    try:
        html = body.decode(charset, errors="replace")
    except LookupError:
        html = body.decode("utf-8", errors="replace")

    extractor = _HtmlTextExtractor()
    try:
        extractor.feed(html)
        extractor.close()
    except Exception as error:
        return FetchFault("extract", status, str(error))
    title, text = extractor.result()
    return FetchPayload(final_url, status, title, text)


class FetchWeb:
    """Soft device that fetches a page and returns its complete extracted text."""

    Payload = FetchPayload
    Fault = FetchFault
    __spill__ = SpillBudget(media_type="text/plain")

    async def __call__(self, args: FetchArgs, ctx: omp.Context) -> FetchPayload | FetchFault:
        """Fetch without blocking the extension host event loop."""
        del ctx
        return await asyncio.to_thread(_fetch_sync, args)

    def prompt(self, view: object, caps: object) -> list[object]:
        """Project a verdict into the model's exact text budget."""
        out = Budget(caps)
        match view:
            case Ok(payload):
                if not out.push(f"HTTP {payload.status}\n"):
                    return out.finish()
                if payload.title is not None and not out.push(f"Title: {payload.title}\n"):
                    return out.finish()
                if not out.push(f"URL: {payload.url}\n\n"):
                    return out.finish()
                for line in payload.text.splitlines(keepends=True):
                    if not out.push(line):
                        break
                return out.finish()
            case Faulted(fault):
                if out.push(f"fetch failed ({fault.kind})"):
                    out.push(f": {fault.detail}")
                return out.finish()
            case _:
                raise TypeError("fetch_web prompt received an unsupported call outcome")


fetch_web = omp.device("fetch_web", family="v", rev=1, place="env")(FetchWeb())