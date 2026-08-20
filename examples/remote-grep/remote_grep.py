"""Remote filesystem tools whose bodies execute on an attached worker."""

from __future__ import annotations

import dataclasses
import json
import os
from pathlib import Path
import subprocess
import tempfile
from collections.abc import Iterator

import omp
omp.workers.declare(
    omp.WorkerSpec(
        name="hpc",
        site=omp.Site.attached(process="hpc-login"),
        idle_ttl=omp.Duration("30m"),
        max_concurrency=4,
        restart=omp.Restart.ON_FAILURE,
        unmanaged=True,
        warm=False,
    )
)



@dataclasses.dataclass(frozen=True, slots=True)
class ListArgs:
    """Arguments for listing paths on the attached worker."""

    path: str = "."
    recursive: bool = False
    limit: int = 2_000


@dataclasses.dataclass(frozen=True, slots=True)
class ReadArgs:
    """Arguments for reading a byte range on the attached worker."""

    path: str
    offset: int = 0
    length: int | None = 256 * 1024


@dataclasses.dataclass(frozen=True, slots=True)
class GrepArgs:
    """Arguments for searching files on the attached worker."""

    pattern: str
    path: str = "."
    file_pattern: str | None = None
    limit: int = 2_000


def _paths(root: Path, recursive: bool) -> Iterator[Path]:
    if not recursive:
        yield from sorted(root.iterdir(), key=lambda path: path.name)
        return

    for directory, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames.sort()
        filenames.sort()
        base = Path(directory)
        yield from (base / name for name in dirnames)
        yield from (base / name for name in filenames)


def _json_or_spill(payload: dict[str, object]) -> dict[str, object] | omp.Spill:
    encoded = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode()
    if len(encoded) > omp.workers.RESULT_SPILL_BYTES:
        return omp.Spill(encoded)
    return payload


@omp.device("remote_ls", family="hpc", rev=1, place="worker:hpc")
async def remote_ls(args: ListArgs, ctx: omp.Context) -> dict[str, object] | omp.Spill:
    """List remote path metadata without moving file contents to the host."""

    del ctx
    if args.limit < 1:
        raise ValueError("limit must be positive")
    root = Path(args.path)
    if not root.is_dir():
        raise NotADirectoryError(args.path)

    entries: list[dict[str, object]] = []
    truncated = False
    for path in _paths(root, args.recursive):
        if len(entries) == args.limit:
            truncated = True
            break
        try:
            stat = path.stat(follow_symlinks=False)
        except FileNotFoundError:
            continue
        entries.append(
            {
                "path": str(path),
                "kind": "symlink" if path.is_symlink() else "dir" if path.is_dir() else "file",
                "size": stat.st_size,
            }
        )
    return _json_or_spill({"entries": entries, "truncated": truncated, "root": args.path})


@omp.device("remote_read", family="hpc", rev=1, place="worker:hpc")
async def remote_read(args: ReadArgs, ctx: omp.Context) -> bytes | omp.Spill:
    """Read remote bytes on the worker and spill oversized values out of band."""

    del ctx
    if args.offset < 0:
        raise ValueError("offset must not be negative")
    if args.length is not None and args.length < 0:
        raise ValueError("length must not be negative")

    with Path(args.path).open("rb") as source:
        source.seek(args.offset)
        data = source.read() if args.length is None else source.read(args.length)
    if len(data) > omp.workers.RESULT_SPILL_BYTES:
        return omp.Spill(data)
    return data


@omp.device("remote_grep", family="hpc", rev=1, place="worker:hpc")
async def remote_grep(args: GrepArgs, ctx: omp.Context) -> dict[str, object] | omp.Spill:
    """Run ripgrep on the worker so only structured matches leave the data site."""

    del ctx
    if args.limit < 1:
        raise ValueError("limit must be positive")

    argv = ["rg", "--json", "--line-number", "--max-count", str(args.limit + 1)]
    if args.file_pattern is not None:
        argv.extend(("--glob", args.file_pattern))
    argv.extend(("-e", args.pattern, "--", args.path))

    hits: list[dict[str, object]] = []
    truncated = False
    with tempfile.TemporaryFile(mode="w+b") as stderr:
        with subprocess.Popen(
            argv,
            stdout=subprocess.PIPE,
            stderr=stderr,
            text=True,
            encoding="utf-8",
            errors="replace",
        ) as process:
            assert process.stdout is not None
            for line in process.stdout:
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if event.get("type") != "match":
                    continue
                if len(hits) == args.limit:
                    truncated = True
                    if process.poll() is None:
                        process.terminate()
                    break
                data = event["data"]
                hits.append(
                    {
                        "path": data["path"].get("text", "<non-UTF-8 path>"),
                        "line": data["line_number"],
                        "text": data["lines"].get("text", "").rstrip("\r\n"),
                    }
                )
            returncode = process.wait()

        if returncode not in (0, 1) and not truncated:
            stderr.seek(0)
            detail = stderr.read().decode("utf-8", errors="replace").strip()
            raise RuntimeError(detail or f"rg exited with status {returncode}")

    return _json_or_spill({"hits": hits, "truncated": truncated, "root": args.path})
