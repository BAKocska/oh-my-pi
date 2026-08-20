from __future__ import annotations

import json
import shlex
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

import omp


_HOST = "127.0.0.1"
_PORT = 8765
_PROCESS_NAME = "examples-web-dashboard"
_SERVER_SOURCE = r'''
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_lock = threading.Lock()
_snapshot = {"generated_ms": 0, "sessions": [], "usage": {}}


def receive():
    global _snapshot
    for line in sys.stdin:
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            print(f"dashboard rejected update: {error}", flush=True)
            continue
        with _lock:
            _snapshot = value


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/healthz":
            body = b'{"status":"ok"}'
        elif self.path == "/api/dashboard":
            with _lock:
                body = json.dumps(_snapshot, separators=(",", ":")).encode()
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


threading.Thread(target=receive, daemon=True).start()
server = ThreadingHTTPServer((sys.argv[-2], int(sys.argv[-1])), Handler)
print(f"dashboard listening on http://{sys.argv[-2]}:{sys.argv[-1]}", flush=True)
server.serve_forever()
'''


@dataclass(frozen=True, slots=True)
class DashboardArgs:
    """Request an immediate dashboard refresh."""


@dataclass(frozen=True, slots=True)
class DashboardResult:
    """URL and supervised process state after a dashboard refresh."""

    url: str
    status: str
    generation: int
    sessions: int
    updated_ms: int


def _enum_value(value: object) -> str:
    """Project a string enum or ordinary value into JSON text."""

    return str(getattr(value, "value", value))


def _usage_json(usage: Any) -> dict[str, object]:
    """Project one frozen usage receipt without serializing implementation fields."""

    return {
        "input": usage.input,
        "output": usage.output,
        "cache_read": usage.cache_read,
        "cache_write": usage.cache_write,
        "reasoning": usage.reasoning,
        "premium_requests": usage.premium_requests,
        "context": usage.context,
        "total": usage.total,
        "accuracy": _enum_value(usage.accuracy),
    }


def _bucket_json(bucket: Any) -> dict[str, object]:
    """Project one aggregate usage bucket into stable dashboard JSON."""

    return {
        "key": dict(bucket.key),
        "start_ms": bucket.start_ms,
        "usage": _usage_json(bucket.usage),
        "cost": {
            "nanos_usd": bucket.cost.nanos_usd,
            "estimated": bucket.cost.estimated,
        },
        "requests": bucket.requests,
        "errors": bucket.errors,
        "duration": str(bucket.duration),
    }


def _fold_snapshot(
    sessions: Sequence[omp.SessionInfo], report: omp.UsageReport, *, generated_ms: int
) -> dict[str, object]:
    """Fold indexed session rows and usage receipts into the server snapshot."""

    return {
        "generated_ms": generated_ms,
        "sessions": [
            {
                "id": session.id,
                "title": session.title,
                "project": session.project,
                "created_ms": session.created_ms,
                "updated_ms": session.updated_ms,
                "status": _enum_value(session.status),
                "kind": _enum_value(session.kind),
                "parent": session.parent,
                "entries": session.entries,
                "turns": session.turns,
                "usage": _usage_json(session.usage),
                "cost": {
                    "nanos_usd": session.cost.nanos_usd,
                    "estimated": session.cost.estimated,
                },
                "models": list(session.models),
                "remote": session.remote,
            }
            for session in sessions
        ],
        "usage": {
            "sessions": report.sessions,
            "truncated": report.truncated,
            "total": _bucket_json(report.total),
            "groups": [_bucket_json(group) for group in report.groups],
            "series": [_bucket_json(bucket) for bucket in report.series],
        },
    }


async def _snapshot() -> dict[str, object]:
    """Query the durable sessions index once and build a complete replacement snapshot."""

    filter_ = omp.SessionFilter(kind=None, limit=200)
    sessions = await omp.sessions.list(filter_)
    report = await omp.sessions.usage(
        omp.UsageQuery(
            group_by=(omp.GroupBy.MODEL, omp.GroupBy.PROJECT),
            filter=filter_,
            include_subagents=True,
        )
    )
    return _fold_snapshot(sessions, report, generated_ms=int(time.time() * 1_000))


def _server_command() -> str:
    """Build the environment-owned launch script without any host filesystem path."""

    return f"python3 -u -c {shlex.quote(_SERVER_SOURCE)} -- {_HOST} {_PORT}"


async def _ensure_dashboard() -> omp.env.Process:
    """Adopt or start the workspace's single supervised dashboard process."""

    omp.env.require(omp.env.Capability.PROCESS)
    return await omp.env.proc.ensure(
        _PROCESS_NAME,
        _server_command(),
        restart=omp.env.RestartPolicy(
            policy=omp.Restart.ON_FAILURE,
            delay=omp.Duration("500ms"),
            max_restarts=5,
        ),
        ready=omp.env.ReadyAll(
            omp.env.ReadyLog(
                r"dashboard listening on http://127\.0\.0\.1:8765",
                timeout=omp.Duration("15s"),
            ),
            omp.env.ReadyTcp(
                _PORT,
                host=_HOST,
                timeout=omp.Duration("15s"),
            ),
        ),
    )


@omp.device("dashboard", family="web", rev=1, place="host")
async def dashboard(args: DashboardArgs, ctx: omp.Context) -> DashboardResult:
    """Refresh session usage and return the supervised dashboard URL and status."""

    del args, ctx
    process = await _ensure_dashboard()
    snapshot = await _snapshot()
    await process.send(json.dumps(snapshot, separators=(",", ":")).encode() + b"\n")
    info = await process.info()
    state = info.get("state", "unknown") if isinstance(info, Mapping) else info.state
    return DashboardResult(
        url=f"http://{_HOST}:{_PORT}/api/dashboard",
        status=_enum_value(state),
        generation=process.generation,
        sessions=len(snapshot["sessions"]),
        updated_ms=int(snapshot["generated_ms"]),
    )
