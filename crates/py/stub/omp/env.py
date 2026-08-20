"""Deterministic in-process Environment fake for standalone extension tests."""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import PurePosixPath
from typing import AsyncIterator


class EnvError(RuntimeError):
    """Base error raised by the standalone Environment fake."""


class NotFound(EnvError):
    """A requested in-memory path or blob was absent."""


@dataclass(frozen=True, slots=True)
class BlobRef:
    """Opaque identifier of bytes retained by the fake Environment."""

    digest: str


@dataclass(frozen=True, slots=True)
class DirEntry:
    """One deterministic directory listing entry."""

    path: PurePosixPath
    is_dir: bool


class _Files:
    def __init__(self, owner: "Fake") -> None:
        self._owner = owner

    async def read(self, path: str | PurePosixPath) -> bytes:
        """Return bytes from the in-memory filesystem."""
        key = PurePosixPath(path)
        try:
            return self._owner._files[key]
        except KeyError as error:
            raise NotFound(str(key)) from error

    async def write(self, path: str | PurePosixPath, data: bytes | str) -> None:
        """Replace an in-memory file with bytes or UTF-8 text."""
        key = PurePosixPath(path)
        self._owner._files[key] = data.encode() if isinstance(data, str) else bytes(data)

    async def list_dir(self, path: str | PurePosixPath) -> tuple[DirEntry, ...]:
        """List immediate children in lexical order."""
        root = PurePosixPath(path)
        children: dict[PurePosixPath, bool] = {}
        for item in self._owner._files:
            try:
                relative = item.relative_to(root)
            except ValueError:
                continue
            if not relative.parts:
                continue
            child = root / relative.parts[0]
            children[child] = len(relative.parts) > 1
        return tuple(DirEntry(item, is_dir) for item, is_dir in sorted(children.items()))


class _Blobs:
    def __init__(self, owner: "Fake") -> None:
        self._owner = owner

    async def put(self, data: bytes | str) -> BlobRef:
        """Store bytes and return a deterministic synthetic reference."""
        payload = data.encode() if isinstance(data, str) else bytes(data)
        ref = BlobRef(f"stub:{len(self._owner._blobs):016x}")
        self._owner._blobs[ref] = payload
        return ref

    async def get(self, ref: BlobRef) -> bytes:
        """Return a previously stored blob."""
        try:
            return self._owner._blobs[ref]
        except KeyError as error:
            raise NotFound(ref.digest) from error


class Fake:
    """A fresh, isolated in-memory implementation of the supported env surface."""

    def __init__(self) -> None:
        self._files: dict[PurePosixPath, bytes] = {}
        self._blobs: dict[BlobRef, bytes] = {}
        self.fs = _Files(self)
        self.blobs = _Blobs(self)

    async def iter_files(self) -> AsyncIterator[tuple[PurePosixPath, bytes]]:
        """Yield the fake filesystem in deterministic path order."""
        for path in sorted(self._files):
            yield path, self._files[path]


_current = Fake()


def install(fake: Fake) -> None:
    """Install a fake for module-level convenience calls in one test scope."""
    if not isinstance(fake, Fake):
        raise TypeError("fake must be an omp.env.Fake")
    global _current, fs, blobs
    _current = fake
    fs, blobs = fake.fs, fake.blobs


def reset() -> Fake:
    """Replace the module-level fake with a clean isolated instance."""
    fake = Fake()
    install(fake)
    return fake


fs = _current.fs
blobs = _current.blobs

__all__ = ("BlobRef", "DirEntry", "EnvError", "Fake", "NotFound", "blobs", "fs", "install", "reset")
