from __future__ import annotations

import asyncio
import dataclasses
import http.client
import ipaddress
import re
import socket
import ssl
from html.parser import HTMLParser
from urllib.parse import urljoin, urlsplit

import omp
from omp import Budget, Faulted, Ok, Payload, SpillBudget


_MAX_BODY_BYTES = 2 * 1024 * 1024
_MAX_REDIRECTS = 5
_TIMEOUT_SECONDS = 15.0
_REDIRECT_STATUSES = frozenset({301, 302, 303, 307, 308})
_METADATA_HOSTS = frozenset(
    {
        "metadata",
        "metadata.google.internal",
        "metadata.goog",
    }
)
_METADATA_NETWORKS = tuple(
    ipaddress.ip_network(value)
    for value in (
        "169.254.169.254/32",
        "169.254.170.2/32",
        "100.100.100.200/32",
        "168.63.129.16/32",
        "fd00:ec2::254/128",
    )
)
_PRIVATE_NETWORKS = tuple(
    ipaddress.ip_network(value)
    for value in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7")
)


@dataclasses.dataclass(frozen=True, slots=True)
class FetchPageArgs:
    """Arguments for fetching one public HTTP or HTTPS page."""

    url: str


@dataclasses.dataclass(frozen=True, slots=True)
class FetchPagePayload(Payload):
    """Durable Markdown extracted from a public page."""

    url: str
    status: int
    title: str | None
    markdown: str
    redirects: int


@dataclasses.dataclass(frozen=True, slots=True)
class FetchPageFault(omp.Fault):
    """Typed refusal or retrieval failure naming the rule that fired."""

    rule: str
    url: str
    address: str | None
    status: int | None
    detail: str


@dataclasses.dataclass(frozen=True, slots=True)
class _Target:
    scheme: str
    host: str
    port: int
    path: str
    addresses: tuple[str, ...]


@dataclasses.dataclass(frozen=True, slots=True)
class _Response:
    status: int
    headers: dict[str, str]
    body: bytes


class _PinnedHttpConnection(http.client.HTTPConnection):
    def __init__(self, host: str, address: str, port: int) -> None:
        super().__init__(host, port=port, timeout=_TIMEOUT_SECONDS)
        self._address = address

    def connect(self) -> None:
        """Connect only to the address that passed validation."""
        self.sock = socket.create_connection(
            (self._address, self.port), self.timeout, self.source_address
        )
        if self._tunnel_host:
            self._tunnel()


class _PinnedHttpsConnection(http.client.HTTPSConnection):
    def __init__(self, host: str, address: str, port: int) -> None:
        super().__init__(
            host,
            port=port,
            timeout=_TIMEOUT_SECONDS,
            context=ssl.create_default_context(),
        )
        self._address = address

    def connect(self) -> None:
        """Connect to the checked address while authenticating the URL hostname."""
        self.sock = socket.create_connection(
            (self._address, self.port), self.timeout, self.source_address
        )
        self.sock = self._context.wrap_socket(self.sock, server_hostname=self.host)


class _MarkdownExtractor(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self._parts: list[str] = []
        self._title_parts: list[str] = []
        self._skip_depth = 0
        self._in_title = False
        self._list_depth = 0
        self._links: list[str | None] = []
        self._pre_depth = 0

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        tag = tag.lower()
        if tag in {"script", "style", "template", "noscript", "svg"}:
            self._skip_depth += 1
            return
        if self._skip_depth:
            return
        attributes = dict(attrs)
        if tag == "title":
            self._in_title = True
        elif tag in {"ul", "ol"}:
            self._list_depth += 1
            self._parts.append("\n")
        elif tag == "li":
            self._parts.append("\n" + "  " * max(0, self._list_depth - 1) + "- ")
        elif tag in {"p", "div", "section", "article", "main", "header", "footer", "blockquote"}:
            self._parts.append("\n\n")
        elif tag in {"h1", "h2", "h3", "h4", "h5", "h6"}:
            self._parts.append("\n\n" + "#" * int(tag[1]) + " ")
        elif tag == "br":
            self._parts.append("\n")
        elif tag == "a":
            self._links.append(attributes.get("href"))
        elif tag == "pre":
            self._pre_depth += 1
            self._parts.append("\n\n```\n")
        elif tag == "code" and not self._pre_depth:
            self._parts.append("`")

    def handle_endtag(self, tag: str) -> None:
        tag = tag.lower()
        if tag in {"script", "style", "template", "noscript", "svg"}:
            if self._skip_depth:
                self._skip_depth -= 1
            return
        if self._skip_depth:
            return
        if tag == "title":
            self._in_title = False
        elif tag in {"ul", "ol"}:
            self._list_depth = max(0, self._list_depth - 1)
            self._parts.append("\n")
        elif tag == "a" and self._links:
            href = self._links.pop()
            if href:
                self._parts.append(f" ({href})")
        elif tag == "pre":
            self._pre_depth = max(0, self._pre_depth - 1)
            self._parts.append("\n```\n")
        elif tag == "code" and not self._pre_depth:
            self._parts.append("`")
        elif tag in {"p", "div", "section", "article", "main", "blockquote"}:
            self._parts.append("\n\n")

    def handle_data(self, data: str) -> None:
        if self._skip_depth:
            return
        if self._in_title:
            self._title_parts.append(data)
            return
        self._parts.append(data if self._pre_depth else re.sub(r"\s+", " ", data))

    def result(self) -> tuple[str | None, str]:
        """Return the normalized title and Markdown body."""
        title = re.sub(r"\s+", " ", "".join(self._title_parts)).strip() or None
        markdown = "".join(self._parts).replace("\r\n", "\n").replace("\r", "\n")
        markdown = re.sub(r"[ \t]+\n", "\n", markdown)
        markdown = re.sub(r"\n{3,}", "\n\n", markdown).strip()
        return title, markdown


def _address_rule(address: str) -> str | None:
    parsed = ipaddress.ip_address(address)
    comparable: ipaddress.IPv4Address | ipaddress.IPv6Address = parsed
    if isinstance(parsed, ipaddress.IPv6Address) and parsed.ipv4_mapped is not None:
        comparable = parsed.ipv4_mapped
    if any(comparable in network for network in _METADATA_NETWORKS if comparable.version == network.version):
        return "metadata"
    if comparable.is_loopback:
        return "loopback"
    if comparable.is_link_local:
        return "link_local"
    if any(comparable in network for network in _PRIVATE_NETWORKS if comparable.version == network.version):
        return "private"
    if comparable.is_multicast:
        return "multicast"
    if comparable.is_unspecified:
        return "unspecified"
    if comparable.is_reserved:
        return "reserved"
    return None


def _resolve_target(url: str) -> _Target | FetchPageFault:
    try:
        parsed = urlsplit(url)
        port = parsed.port
    except ValueError as error:
        return FetchPageFault("url", url, None, None, str(error))
    if parsed.scheme not in {"http", "https"} or parsed.hostname is None:
        return FetchPageFault("url", url, None, None, "URL must be absolute HTTP or HTTPS")
    if parsed.username is not None or parsed.password is not None:
        return FetchPageFault("url", url, None, None, "credentials in URLs are refused")
    host = parsed.hostname.rstrip(".").lower()
    if not host or "%" in host:
        return FetchPageFault("url", url, None, None, "invalid or scoped hostname")
    if host in _METADATA_HOSTS:
        return FetchPageFault("metadata", url, host, None, "cloud metadata hostname refused")
    port = port or (443 if parsed.scheme == "https" else 80)
    try:
        records = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    except socket.gaierror as error:
        return FetchPageFault("dns", url, None, None, str(error))
    addresses = tuple(dict.fromkeys(record[4][0] for record in records))
    if not addresses:
        return FetchPageFault("dns", url, None, None, "hostname resolved to no addresses")
    for address in addresses:
        rule = _address_rule(address)
        if rule is not None:
            return FetchPageFault(rule, url, address, None, f"resolved address refused by {rule} rule")
    path = parsed.path or "/"
    if parsed.query:
        path += "?" + parsed.query
    return _Target(parsed.scheme, host, port, path, addresses)


def _request_once(target: _Target) -> _Response:
    connection_type = _PinnedHttpsConnection if target.scheme == "https" else _PinnedHttpConnection
    default_port = 443 if target.scheme == "https" else 80
    authority_host = f"[{target.host}]" if ":" in target.host else target.host
    authority = (
        authority_host
        if target.port == default_port
        else f"{authority_host}:{target.port}"
    )
    last_error: OSError | None = None
    for address in target.addresses:
        connection = connection_type(target.host, address, target.port)
        try:
            connection.request(
                "GET",
                target.path,
                headers={
                    "Accept": "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1",
                    "Accept-Encoding": "identity",
                    "Host": authority,
                    "User-Agent": "omp-safe-fetch/0.1",
                },
            )
            response = connection.getresponse()
            content_length = response.getheader("Content-Length")
            if content_length is not None and int(content_length) > _MAX_BODY_BYTES:
                raise OverflowError("response Content-Length exceeds the body limit")
            body = response.read(_MAX_BODY_BYTES + 1)
            if len(body) > _MAX_BODY_BYTES:
                raise OverflowError("response body exceeds the body limit")
            return _Response(
                response.status,
                {name.lower(): value for name, value in response.getheaders()},
                body,
            )
        except OSError as error:
            last_error = error
        finally:
            connection.close()
    if last_error is not None:
        raise last_error
    raise OSError("no validated address was available")


def _fetch_sync(url: str) -> FetchPagePayload | FetchPageFault:
    current_url = url
    for redirects in range(_MAX_REDIRECTS + 1):
        target = _resolve_target(current_url)
        if isinstance(target, FetchPageFault):
            return target
        try:
            response = _request_once(target)
        except OverflowError as error:
            return FetchPageFault("body_limit", current_url, None, None, str(error))
        except (OSError, http.client.HTTPException, ValueError) as error:
            return FetchPageFault("network", current_url, None, None, str(error))
        if response.status in _REDIRECT_STATUSES:
            location = response.headers.get("location")
            if not location:
                return FetchPageFault(
                    "redirect", current_url, None, response.status, "redirect omitted Location"
                )
            if redirects == _MAX_REDIRECTS:
                return FetchPageFault(
                    "redirect_limit", current_url, None, response.status, "too many redirects"
                )
            current_url = urljoin(current_url, location)
            continue
        if not 200 <= response.status < 300:
            return FetchPageFault(
                "http_status", current_url, None, response.status, "server returned a non-success status"
            )
        media_type = response.headers.get("content-type", "text/html").split(";", 1)[0].strip().lower()
        if media_type not in {"text/html", "application/xhtml+xml"}:
            return FetchPageFault(
                "content_type", current_url, None, response.status, f"unsupported content type: {media_type}"
            )
        charset_match = re.search(r"charset=([^;\s]+)", response.headers.get("content-type", ""), re.I)
        charset = charset_match.group(1).strip("\"'") if charset_match else "utf-8"
        try:
            html = response.body.decode(charset, errors="replace")
        except LookupError:
            html = response.body.decode("utf-8", errors="replace")
        extractor = _MarkdownExtractor()
        try:
            extractor.feed(html)
            extractor.close()
        except Exception as error:
            return FetchPageFault("extract", current_url, None, response.status, str(error))
        title, markdown = extractor.result()
        return FetchPagePayload(current_url, response.status, title, markdown, redirects)
    raise AssertionError("redirect loop exhausted without a verdict")


class FetchPage:
    """Soft device that defensively fetches public pages as Markdown."""

    Payload = FetchPagePayload
    Fault = FetchPageFault
    __spill__ = SpillBudget(media_type="text/markdown")

    async def __call__(
        self, args: FetchPageArgs, ctx: omp.Context
    ) -> FetchPagePayload | FetchPageFault:
        """Require network scope, then fetch without blocking the host event loop."""
        ctx.require(omp.env.Capability.NET)
        return await asyncio.to_thread(_fetch_sync, args.url)

    def prompt(self, view: object, caps: object) -> list[object]:
        """Project the structured verdict into the model's exact text budget."""
        out = Budget(caps)
        match view:
            case Ok(payload):
                if not out.push(f"HTTP {payload.status} · {payload.url}\n"):
                    return out.finish()
                if payload.title is not None and not out.push(f"# {payload.title}\n\n"):
                    return out.finish()
                for line in payload.markdown.splitlines(keepends=True):
                    if not out.push(line):
                        break
                return out.finish()
            case Faulted(fault):
                if out.push(f"fetch refused ({fault.rule})"):
                    out.push(f": {fault.detail}")
                return out.finish()
            case _:
                raise TypeError("fetch_page prompt received an unsupported call outcome")


fetch_page = omp.device(
    "fetch_page",
    family="safe",
    rev=1,
    place="env",
    summary="Fetch a public page after validating every resolved address and redirect hop.",
    effects=omp.Effects(exec=omp.ExecEffects(network=True)),
    tier=omp.Tier.READ,
)(FetchPage())
