"""Reader for the static first-party extension index protocol.

Parsing is pure Python and consumes caller-provided bytes.  Live reads are explicit
async calls routed through ``omp.env``; importing this module never opens a socket,
reads a cache, or initializes a client.
"""
from __future__ import annotations

import tomllib
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass
import json
from typing import Any
from urllib.parse import quote, urljoin

from _omp import OmpError


class IndexError(OmpError, ValueError):
    """A static index document did not satisfy the omp index schema."""


class IndexTransportError(OmpError, RuntimeError):
    """A live index request could not be routed through the Environment."""


class IndexVerificationError(IndexError):
    """A caller-provided signature verifier rejected an index document."""


@dataclass(frozen=True, slots=True)
class IdentityClaim:
    """The stable publisher-qualified identity served by an index entry."""

    publisher: str
    extension_id: str
    fingerprint: str


@dataclass(frozen=True, slots=True)
class CapabilityAttestation:
    """Advisory index review of an extension capability set."""

    capability_digest: str | None
    outcome: str
    build_provenance: str | None
    signature: str | None


@dataclass(frozen=True, slots=True)
class CatalogEntry:
    """One discoverable extension record from ``catalog/v1/index.json``."""

    identity: IdentityClaim
    distribution: str
    versions: tuple[str, ...]
    summary: str
    capabilities: tuple[str, ...]
    attestation: CapabilityAttestation | None
    deprecated: str | None
    revocation: str | None
    downloads: int | None


@dataclass(frozen=True, slots=True)
class Catalog:
    """The cacheable extension catalog published by one static index."""

    entries: tuple[CatalogEntry, ...]

    def get(self, extension_id: str) -> CatalogEntry | None:
        """Return an entry by stable extension identifier."""
        return next((entry for entry in self.entries if entry.identity.extension_id == extension_id), None)


@dataclass(frozen=True, slots=True)
class SimpleFile:
    """One PEP 691 JSON simple-index artifact link."""

    filename: str
    url: str
    hashes: tuple[tuple[str, str], ...]
    requires_python: str | None
    yanked: str | bool


@dataclass(frozen=True, slots=True)
class SimpleProject:
    """A static PEP 691 project response compatible with PEP 503 clients."""

    name: str
    files: tuple[SimpleFile, ...]


@dataclass(frozen=True, slots=True)
class ResolvedClosure:
    """A signed, platform-specific lock fragment adopted without resolution."""

    extension_id: str
    version: str
    target: str
    lock: str
    signature: str | None


def _document(payload: bytes | str | Mapping[str, Any]) -> Mapping[str, Any]:
    """Decode one static JSON document without performing any I/O."""
    if isinstance(payload, Mapping):
        document = payload
    else:
        try:
            document = json.loads(payload)
        except (TypeError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise IndexError("index document is not valid JSON") from error
    if not isinstance(document, Mapping):
        raise IndexError("index document must be a JSON object")
    return document


def _string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise IndexError(f"{field} must be a non-empty string")
    return value


def _sequence(value: Any, field: str) -> Sequence[Any]:
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes, bytearray)):
        raise IndexError(f"{field} must be an array")
    return value


def _identity(value: Mapping[str, Any]) -> IdentityClaim:
    identity = value.get("identity", value)
    if not isinstance(identity, Mapping):
        raise IndexError("identity must be an object")
    return IdentityClaim(
        publisher=_string(identity.get("publisher", identity.get("publisher_key")), "identity.publisher"),
        extension_id=_string(identity.get("id", identity.get("extension_id")), "identity.id"),
        fingerprint=_string(identity.get("fingerprint", identity.get("publisher_fingerprint")), "identity.fingerprint"),
    )


def _attestation(value: Any) -> CapabilityAttestation | None:
    if value is None or value is False:
        return None
    if isinstance(value, str):
        return CapabilityAttestation(None, _string(value, "attestation.status"), None, None)
    if not isinstance(value, Mapping):
        raise IndexError("attestation must be an object or status string")
    digest = value.get("capability_digest")
    return CapabilityAttestation(
        capability_digest=None if digest is None else _string(digest, "attestation.capability_digest"),
        outcome=_string(value.get("outcome", value.get("status")), "attestation.outcome"),
        build_provenance=value.get("build_provenance"),
        signature=value.get("signature"),
    )


def parse_catalog(payload: bytes | str | Mapping[str, Any]) -> Catalog:
    """Parse the static first-party catalog without fetching or verifying it."""
    document = _document(payload)
    raw_entries = _sequence(document.get("entries", document.get("extensions", ())), "catalog.entries")
    entries: list[CatalogEntry] = []
    for raw in raw_entries:
        if not isinstance(raw, Mapping):
            raise IndexError("catalog entry must be an object")
        capabilities = _sequence(raw.get("capabilities", raw.get("capability_summary", ())), "capabilities")
        versions = _sequence(raw.get("versions", ()), "versions")
        downloads = raw.get("downloads", raw.get("download_count"))
        if downloads is not None and (not isinstance(downloads, int) or isinstance(downloads, bool) or downloads < 0):
            raise IndexError("downloads must be a non-negative integer")
        entries.append(CatalogEntry(
            identity=_identity(raw), distribution=_string(raw.get("distribution", raw.get("distribution_name")), "distribution"),
            versions=tuple(_string(item, "versions[]") for item in versions), summary=str(raw.get("summary", "")),
            capabilities=tuple(_string(item, "capabilities[]") for item in capabilities),
            attestation=_attestation(raw.get("attestation", raw.get("attestation_status"))),
            deprecated=raw.get("deprecated"), revocation=raw.get("revocation", raw.get("revocation_pointer")), downloads=downloads,
        ))
    return Catalog(tuple(entries))


def parse_simple_project(payload: bytes | str | Mapping[str, Any]) -> SimpleProject:
    """Parse a PEP 691 simple-index JSON response from static bytes."""
    document = _document(payload)
    meta = document.get("meta", {})
    if not isinstance(meta, Mapping) or meta.get("api-version") not in (None, "1.0"):
        raise IndexError("unsupported PEP 691 API version")
    files: list[SimpleFile] = []
    for raw in _sequence(document.get("files"), "files"):
        if not isinstance(raw, Mapping):
            raise IndexError("simple-index file must be an object")
        hashes = raw.get("hashes", {})
        if not isinstance(hashes, Mapping) or not all(isinstance(key, str) and isinstance(value, str) for key, value in hashes.items()):
            raise IndexError("simple-index file hashes must be string pairs")
        yanked = raw.get("yanked", False)
        if not isinstance(yanked, (str, bool)):
            raise IndexError("simple-index yanked must be a boolean or reason")
        files.append(SimpleFile(
            filename=_string(raw.get("filename"), "filename"), url=_string(raw.get("url"), "url"),
            hashes=tuple(sorted(hashes.items())), requires_python=raw.get("requires-python"), yanked=yanked,
        ))
    return SimpleProject(name=_string(document.get("name"), "name"), files=tuple(files))


def parse_closure(
    payload: bytes | str | Mapping[str, Any],
    *,
    extension_id: str,
    version: str,
    target: str,
    signature: str | None = None,
) -> ResolvedClosure:
    """Wrap a signature-verified closure lock fragment for client adoption.

    The lock is intentionally left as TOML text: the resolver, rather than this
    reader, owns lock interpretation and signature policy.
    """
    if isinstance(payload, Mapping):
        lock = payload.get("lock")
    else:
        try:
            lock = payload.decode("utf-8") if isinstance(payload, bytes) else payload
        except UnicodeDecodeError as error:
            raise IndexError("resolved closure is not UTF-8") from error
    if not isinstance(lock, str) or not lock.strip():
        raise IndexError("resolved closure is empty")
    return ResolvedClosure(extension_id, version, target, lock, signature)

_Fetcher = Callable[[str, str], Awaitable[bytes | str | Mapping[str, Any]]]
_Verifier = Callable[[bytes | str, str | None], bool]
_Fallback = Callable[[str, str, str], Awaitable[ResolvedClosure]]


class IndexClient:
    """Explicit async reader for a CDN-hosted first-party index.

    A supplied fetcher makes the client usable in standalone tests.  ``live()``
    routes requests through the current ``omp.env`` bridge, never urllib or a
    process-global HTTP client.
    """
    def __init__(self, base_url: str, fetcher: _Fetcher | None = None, verifier: _Verifier | None = None) -> None:
        if not isinstance(base_url, str) or not base_url:
            raise TypeError("base_url must be a non-empty str")
        self._base_url = base_url.rstrip("/") + "/"
        self._fetcher = fetcher
        self._verifier = verifier

    @classmethod
    def live(cls, base_url: str, verifier: _Verifier | None = None) -> "IndexClient":
        """Create a client whose future requests use the active Environment bridge."""
        async def fetch(url: str, accept: str) -> bytes | str | Mapping[str, Any]:
            from . import env
            try:
                return await env._request("omp.env.http.get", url=url, accept=accept)
            except Exception as error:
                raise IndexTransportError(f"Environment could not fetch {url}") from error
        return cls(base_url, fetch, verifier)

    async def _read(self, path: str, accept: str) -> bytes | str | Mapping[str, Any]:
        if self._fetcher is None:
            raise IndexTransportError("index client has no fetcher; use IndexClient.live() or pass one")
        return await self._fetcher(urljoin(self._base_url, path), accept)

    def _verify(self, payload: bytes | str | Mapping[str, Any], signature: str | None) -> None:
        """Apply an optional caller-owned signature verifier to static content."""
        if self._verifier is None:
            return
        signed = (
            json.dumps(payload, separators=(",", ":"), sort_keys=True)
            if isinstance(payload, Mapping)
            else payload
        )
        if signature is None or not self._verifier(signed, signature):
            raise IndexVerificationError("index signature verification failed")

    @staticmethod
    def _closure_signature(payload: bytes | str | Mapping[str, Any]) -> str | None:
        """Extract an optional lock signature without interpreting lock dependencies."""
        if isinstance(payload, Mapping):
            return payload.get("signature") if isinstance(payload.get("signature"), str) else None
        try:
            text = payload.decode("utf-8") if isinstance(payload, bytes) else payload
            value = tomllib.loads(text).get("signature")
        except (UnicodeDecodeError, tomllib.TOMLDecodeError):
            return None
        return value if isinstance(value, str) else None

    async def catalog(self) -> Catalog:
        """Fetch and parse ``catalog/v1/index.json`` through the configured transport."""
        payload = await self._read("catalog/v1/index.json", "application/json")
        signature = _document(payload).get("signature")
        self._verify(payload, signature if isinstance(signature, str) else None)
        return parse_catalog(payload)

    async def simple(self, distribution: str) -> SimpleProject:
        """Fetch one PEP 691 JSON simple-index project response."""
        payload = await self._read(f"simple/{quote(distribution, safe='')}/", "application/vnd.pypi.simple.v1+json")
        return parse_simple_project(payload)

    async def closure(self, extension_id: str, version: str, target: str) -> ResolvedClosure:
        """Fetch a pre-resolved lock fragment, with resolver fallback left available."""
        path = f"resolved/v1/{quote(extension_id, safe='.')}/{quote(version, safe='')}/{quote(target, safe='')}.omp.lock"
        payload = await self._read(path, "text/plain")
        signature = self._closure_signature(payload)
        self._verify(payload, signature)
        return parse_closure(payload, extension_id=extension_id, version=version, target=target, signature=signature)

    async def closure_or_resolve(
        self,
        extension_id: str,
        version: str,
        target: str,
        fallback: _Fallback,
    ) -> ResolvedClosure:
        """Use a verified closure when reachable, otherwise invoke client resolution.

        Malformed or unverified index content remains an error; only transport
        unavailability selects the documented client-side resolution fallback.
        """
        try:
            return await self.closure(extension_id, version, target)
        except IndexTransportError:
            return await fallback(extension_id, version, target)


__all__ = (
    "CapabilityAttestation", "Catalog", "CatalogEntry", "IdentityClaim", "IndexClient", "IndexError",
    "IndexTransportError", "IndexVerificationError", "ResolvedClosure", "SimpleFile", "SimpleProject",
    "parse_catalog", "parse_closure", "parse_simple_project",
)
