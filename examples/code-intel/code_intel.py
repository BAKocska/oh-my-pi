from __future__ import annotations

import json as _json
from collections.abc import Mapping as _Mapping
from dataclasses import dataclass as _dataclass
from typing import Any as _Any

import omp
omp.workers.declare(
    omp.WorkerSpec(
        name="code-intel",
        site=omp.Site.ENV,
        idle_ttl=omp.Duration("2m"),
        max_concurrency=8,
        restart=omp.Restart.NO,
    )
)



@_dataclass(frozen=True, slots=True)
class DiagnosticsArgs:
    """Select a document whose current language-server diagnostics should be returned."""

    path: omp.EnvPath


@_dataclass(frozen=True, slots=True)
class ReferencesArgs:
    """Select a one-based source position whose references should be returned."""

    path: omp.EnvPath
    line: int
    column: int
    include_declaration: bool = True


@_dataclass(frozen=True, slots=True)
class SymbolsArgs:
    """Select a document whose language-server symbols should be returned."""

    path: omp.EnvPath


@_dataclass(frozen=True, slots=True)
class SurveyArgs:
    """Select documents to inspect concurrently beside the Environment."""

    paths: tuple[omp.EnvPath, ...]
    concurrency: int = 8


def _binding_field(binding: _Any, name: str) -> _Any:
    if isinstance(binding, _Mapping):
        return binding[name]
    return getattr(binding, name)


async def _lsp_query(path: omp.EnvPath, method: str, params: dict[str, _Any]) -> dict[str, _Any]:
    omp.env.require(omp.env.Capability.DOC_READ, omp.env.Capability.LSP)
    async with await omp.env.docs.open(path) as doc:
        bindings = await omp.env.lsp.bindings(path)
        if not bindings:
            return {"path": path.uri, "server": None, "result": None, "error": "no language server binding"}

        binding = bindings[0]
        result = await omp.env.lsp.request(
            _binding_field(binding, "server_id"),
            method,
            {"textDocument": {"uri": path.uri}, **params},
            doc=doc,
            timeout=omp.Duration("30s"),
        )
        return {
            "path": path.uri,
            "server": _binding_field(binding, "name"),
            "result": result,
        }


@omp.device("diagnostics", family="ci", rev=1, place=omp.Place.ENV)
async def diagnostics(args: DiagnosticsArgs, ctx: omp.Context) -> dict[str, _Any]:
    """Return pull diagnostics from the language server bound to a document."""

    return await _lsp_query(args.path, "textDocument/diagnostic", {})


@omp.device("references", family="ci", rev=1, place=omp.Place.ENV)
async def references(args: ReferencesArgs, ctx: omp.Context) -> dict[str, _Any]:
    """Return references for a one-based source position."""

    return await _lsp_query(
        args.path,
        "textDocument/references",
        {
            "position": {"line": args.line - 1, "character": args.column - 1},
            "context": {"includeDeclaration": args.include_declaration},
        },
    )


@omp.device("symbols", family="ci", rev=1, place=omp.Place.ENV)
async def symbols(args: SymbolsArgs, ctx: omp.Context) -> dict[str, _Any]:
    """Return document symbols from the language server bound to a document."""

    return await _lsp_query(args.path, "textDocument/documentSymbol", {})


async def _survey_file(path: omp.EnvPath) -> dict[str, _Any]:
    diagnostics_result = await _lsp_query(path, "textDocument/diagnostic", {})
    symbols_result = await _lsp_query(path, "textDocument/documentSymbol", {})
    return {
        "path": path.uri,
        "diagnostics": diagnostics_result["result"],
        "symbols": symbols_result["result"],
        "error": diagnostics_result.get("error") or symbols_result.get("error"),
    }


def _pack_survey(reports: list[dict[str, _Any]]) -> dict[str, _Any] | omp.Spill:
    result = {"files": len(reports), "reports": reports}
    encoded = _json.dumps(result, ensure_ascii=False, separators=(",", ":")).encode()
    if len(encoded) > 64 * 1024:
        return omp.Spill(encoded)
    return result


@omp.device("survey", family="ci", rev=1, place=omp.Place.ENV)
async def survey(args: SurveyArgs, ctx: omp.Context) -> _Any:
    """Fan out code-intelligence queries and spill an oversized aggregate value."""

    worker: omp.WorkerHandle = await omp.workers.get("code-intel")
    reports = await worker.map(_survey_file, args.paths, concurrency=args.concurrency)
    return await worker.call(_pack_survey, reports)
