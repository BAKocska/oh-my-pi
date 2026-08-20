"""Pure URL and read-selector parsing for the frozen :mod:`omp` package."""

from __future__ import annotations

from collections.abc import Callable, Iterable, Mapping
from dataclasses import dataclass
from enum import StrEnum
from types import MappingProxyType
from typing import Final

from _omp import AgentUrl, ArtifactUrl, EnvPath, HistoryUrl, OmpError
from _omp_url_vocab import (
    SCHEMES as _SCHEMES,
    SELECTOR_GRAMMAR,
    URL_VOCAB_VERSION,
)


class UrlError(OmpError, ValueError):
    """Base error for URL namespace operations."""


class SelectorError(UrlError):
    """A selector has invalid syntax or bounds."""


class SchemeNotReadable(UrlError):
    """A known scheme has no reader in the current deployment."""


if URL_VOCAB_VERSION != 1:
    raise RuntimeError(f"unsupported URL vocabulary version {URL_VOCAB_VERSION}")

Scheme = StrEnum(
    "Scheme",
    {
        member: wire[0] if wire else member.lower()
        for member, wire, _selectors in _SCHEMES
    },
    module=__name__,
)
Scheme.__doc__ = "Dense built-in URL-scheme vocabulary supplied by ``omp-tools``."


@dataclass(frozen=True, slots=True)
class Selector:
    """Sorted, merged, one-based inclusive line selection."""

    ranges: tuple[tuple[int, int | None], ...] = ()
    raw: bool = False
    conflicts: bool = False



TypedUrl = ArtifactUrl | HistoryUrl | AgentUrl | EnvPath


@dataclass(frozen=True, slots=True)
class Url:
    """Pure parse of a URL or bare file path."""

    scheme: Scheme
    raw_scheme: str
    resource: str
    selector: Selector | None
    text: str
    value: TypedUrl | None


@dataclass(frozen=True, slots=True)
class SchemeInfo:
    """Live capabilities for one built-in scheme."""

    readable: bool
    mintable: bool
    selectors: bool
    description: str


_SCHEME_ALIASES: Final[Mapping[str, Scheme]] = MappingProxyType(
    {
        spelling: Scheme[member]
        for member, wire, _selectors in _SCHEMES
        for spelling in wire
    }
)
_SELECTOR_SCHEMES: Final = frozenset(
    Scheme[member] for member, _wire, selectors in _SCHEMES if selectors
)
_SCHEME_RE = __import__("re").compile(r"^[A-Za-z][A-Za-z0-9+.-]*$")
_scheme_source: Callable[[], tuple[bytes, Iterable[tuple[Scheme, SchemeInfo]]]] | None = None
_scheme_hash: bytes | None = None
_scheme_cache: Mapping[Scheme, SchemeInfo] = MappingProxyType({})
_U64_MAX: Final = (1 << 64) - 1


def _bind_scheme_source(
    source: Callable[[], tuple[bytes, Iterable[tuple[Scheme, SchemeInfo]]]],
) -> None:
    """Bind the Rust scheme snapshot source used by the frozen package."""
    global _scheme_source, _scheme_hash, _scheme_cache
    _scheme_source = source
    _scheme_hash = None
    _scheme_cache = MappingProxyType({})


def schemes() -> Mapping[Scheme, SchemeInfo]:
    """Return the live scheme table, invalidating on the device-side digest."""
    global _scheme_hash, _scheme_cache
    if _scheme_source is None:
        return _scheme_cache
    device_hash, entries = _scheme_source()
    if device_hash != _scheme_hash:
        _scheme_cache = MappingProxyType(dict(entries))
        _scheme_hash = bytes(device_hash)
    return _scheme_cache


def parse(url: str | TypedUrl) -> Url:
    """Purely parse a URL or bare file path without performing I/O."""
    text = str(url)
    separator = text.find("://")
    if separator < 0:
        split = _split_selector(text)
        selector = _parse_selector_or_none(split[1])
        return Url(Scheme.FILE, "", split[0], selector, text, None)

    raw_scheme = text[:separator]
    if not _SCHEME_RE.fullmatch(raw_scheme):
        raise UrlError(f"invalid URL scheme {raw_scheme!r}")
    scheme = _SCHEME_ALIASES.get(raw_scheme.lower(), Scheme.UNKNOWN)
    tail = text[separator + 3 :]
    if not tail:
        raise UrlError("URL resource must not be empty")
    if any(character.isspace() or ord(character) < 32 for character in tail):
        raise UrlError("URL resource must not contain whitespace or control characters")

    resource, selector_text = _split_uri_selector(tail, scheme)
    selector = _parse_selector_or_none(selector_text)
    value_type = {
        Scheme.ARTIFACT: ArtifactUrl,
        Scheme.HISTORY: HistoryUrl,
        Scheme.AGENT: AgentUrl,
    }.get(scheme)
    value = (
        url
        if value_type is not None and isinstance(url, value_type)
        else value_type(text) if value_type is not None else None
    )
    return Url(scheme, raw_scheme, resource, selector, text, value)


def parse_selector(text: str) -> Selector:
    """Purely parse one read-selector fragment."""
    parsed = _parse_selector_or_none(text)
    if parsed is None:
        raise SelectorError(_invalid_selector(text))
    return parsed


def _parse_selector_or_none(text: str | None) -> Selector | None:
    if not text:
        return None
    if ":" in text:
        chunks = text.split(":")
        if len(chunks) == 2:
            first, second = chunks
            range_text = second if first.lower() == "raw" else first if second.lower() == "raw" else None
            ranges = _parse_ranges(range_text) if range_text is not None else None
            if ranges is not None:
                return Selector(ranges=ranges, raw=True)
        if all(_selector_chunk_looks_read_like(chunk) for chunk in chunks):
            raise SelectorError(_invalid_selector(text))
        return None
    lowered = text.lower()
    if lowered == "raw":
        return Selector(raw=True)
    if lowered == "conflicts":
        return Selector(conflicts=True)
    ranges = _parse_ranges(text)
    return Selector(ranges=ranges) if ranges is not None else None


def _parse_ranges(text: str | None) -> tuple[tuple[int, int | None], ...] | None:
    if not text:
        return None
    ranges: list[tuple[int, int | None]] = []
    for chunk in text.split(","):
        parsed = _parse_range(chunk)
        if parsed is None:
            return None
        ranges.append(parsed)
    ranges.sort(key=lambda item: item[0])
    merged: list[tuple[int, int | None]] = []
    for start, end in ranges:
        if not merged:
            merged.append((start, end))
            continue
        previous_start, previous_end = merged[-1]
        if previous_end is None:
            continue
        if start <= previous_end + 1:
            merged[-1] = (previous_start, None if end is None else max(previous_end, end))
        else:
            merged.append((start, end))
    return tuple(merged)


def _parse_range(text: str) -> tuple[int, int | None] | None:
    value = text[1:] if text[:1].lower() == "l" else text
    digit_end = 0
    while digit_end < len(value) and value[digit_end].isdigit() and value[digit_end].isascii():
        digit_end += 1
    if digit_end == 0:
        return None
    start = int(value[:digit_end])
    if start > _U64_MAX:
        raise SelectorError(f"Line selector '{value[:digit_end]}' is too large.")
    if start == 0:
        raise SelectorError("Line selector 0 is invalid; lines are 1-indexed. Use :1.")
    rest = value[digit_end:]
    if not rest:
        return (start, None)
    if rest.startswith(".."):
        separator, right = "-", rest[2:]
    elif rest.startswith("-"):
        separator, right = "-", rest[1:]
    elif rest.startswith("+"):
        separator, right = "+", rest[1:]
    else:
        return None
    right = right[1:] if right[:1].lower() == "l" else right
    if separator == "-" and not right:
        return (start, None)
    if not right or not right.isascii() or not right.isdigit():
        return None
    amount = int(right)
    if amount > _U64_MAX:
        raise SelectorError(f"Line selector '{right}' is too large.")
    if separator == "+":
        if amount == 0:
            raise SelectorError(f"Invalid range {start}+0: count must be >= 1.")
        end = start + amount - 1
        if end > _U64_MAX:
            raise SelectorError(f"Invalid range {start}+{amount}: count is too large.")
        return (start, end)
    if amount < start:
        raise SelectorError(f"Invalid range {start}-{amount}: end must be >= start.")
    return (start, amount)


def _selector_chunk_looks_read_like(text: str) -> bool:
    if text.lower() in {"raw", "conflicts"}:
        return True
    if _parse_ranges(text) is not None:
        return True
    if not text.startswith("-"):
        return False
    tail = text[1:]
    digit_end = 0
    while digit_end < len(tail) and tail[digit_end].isascii() and tail[digit_end].isdigit():
        digit_end += 1
    if digit_end == 0:
        return False
    rest = tail[digit_end:]
    if not rest:
        return True
    if rest[:1] not in {"-", "+"}:
        return False
    right = rest[1:]
    return bool(right) and right.isascii() and right.isdigit()


def _split_selector(text: str) -> tuple[str, str | None]:
    colon = text.rfind(":")
    if colon <= 0 or not _is_simple_selector(text[colon + 1 :]):
        return text, None
    path, selector = text[:colon], text[colon + 1 :]
    inner = path.rfind(":")
    if inner > 0:
        first = path[inner + 1 :]
        if (first.lower() == "raw" and _is_range_list(selector)) or (
            _is_range_list(first) and selector.lower() == "raw"
        ):
            return path[:inner], text[inner + 1 :]
    return path, selector


def _split_uri_selector(resource: str, scheme: Scheme) -> tuple[str, str | None]:
    if scheme not in _SELECTOR_SCHEMES:
        return resource, None
    if scheme is Scheme.SSH and "/" not in resource:
        return resource, None
    path = resource
    selector_start: int | None = None
    while (colon := path.rfind(":")) >= 0:
        if not _internal_selector_chunk(path[colon + 1 :]):
            break
        selector_start = colon + 1
        path = resource[:colon]
    return (path, resource[selector_start:] if selector_start is not None else None)


def _internal_selector_chunk(text: str) -> bool:
    try:
        return _is_simple_selector(text) or _selector_chunk_looks_read_like(text)
    except SelectorError:
        return True


def _is_simple_selector(text: str) -> bool:
    return text.lower() in {"raw", "conflicts"} or _is_range_list(text)


def _is_range_list(text: str) -> bool:
    if not text:
        return False
    try:
        return all(_parse_range(chunk) is not None for chunk in text.split(","))
    except SelectorError:
        return True




async def read(
    url: str | Url | TypedUrl,
    selector: str | None = None,
) -> str:
    """Read a URL through the package's existing host resolver."""

    from . import _read_url

    target = url.text if isinstance(url, Url) else str(url)
    if selector is not None:
        parsed_selector = parse_selector(selector)
        if parsed_selector is None:
            raise SelectorError("selector must not be empty")
        target = f"{target}:{selector}"
    return await _read_url(target)


def _invalid_selector(text: str) -> str:
    return (
        f"Invalid selector ':{text}'. Use :N, :N-M, :N+K, :N- (open-ended), "
        "a comma-separated list of ranges, :raw, or a range combined with raw "
        "(e.g. :raw:50-100)."
    )


__all__ = (
    "AgentUrl",
    "ArtifactUrl",
    "HistoryUrl",
    "Scheme",
    "SchemeInfo",
    "SchemeNotReadable",
    "Selector",
    "SelectorError",
    "Url",
    "UrlError",
    "parse",
    "parse_selector",
    "read",
    "schemes",
)
