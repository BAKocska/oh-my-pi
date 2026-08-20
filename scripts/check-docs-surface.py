#!/usr/bin/env python3
"""Bidirectionally check the frozen Python exports against docs/py/00-14.

The checker imports the no-I/O native and CONTROL stubs from
``check-python-examples.py`` so the two gates cannot acquire divergent host
models.  It reports documented paths that cannot be resolved and public frozen
exports for which no documented spelling resolves to that export.
"""

from __future__ import annotations

import argparse
import importlib
import importlib.util
import pkgutil
import re
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from types import ModuleType
from typing import Any

_ROOT = Path(__file__).resolve().parents[1]
_PYTHON = _ROOT / "crates" / "py" / "python"
_DOCS = _ROOT / "docs" / "py"
_DOC_NAME = re.compile(r"(?:0[0-9]|1[0-4])-[^/]+\.md\Z")
_SYMBOL = re.compile(r"(?<![A-Za-z0-9_])omp(?:\.[A-Za-z_]\w*)+")
_INLINE_CODE = re.compile(r"(?<!`)`(?!`)([^`\n]+)`(?!`)")
_FENCE = re.compile(r"^\s*(`{3,}|~{3,})")

_SIGNATURE = re.compile(r"^\s*(?:async\s+def|def|class|type)\s+[A-Za-z_]\w*")

# These exact spellings and namespaces occur in code font but are not Python:
# manifest keys/filenames, protobuf packages, CLI/config names, and telemetry
# attribute keys. Prefixes include the trailing dot so the real decorators
# ``omp.tool`` and ``omp.telemetry`` remain audited.
_DOCUMENTED_NON_EXPORTS: dict[str, str] = {
    "omp.toml": "the extension manifest filename, not a Python module",
    "omp.context.aux": "an explicitly rejected draft API in 08-context",
    "omp._params": "private implementation module named only as an internal detail",
    "omp.env.v1": "protobuf package",
    "omp.policy.v1.SandboxProfile": "protobuf message, not the Python policy type",
    "omp.agent": "OpenTelemetry attribute namespace",
    "omp.artifact": "OpenTelemetry attribute namespace",
    "omp.binaries": "manifest table",
    "omp.dev": "service/domain name",
    "omp.llm": "tail of the service domain name dev.omp.llm, not a Python module",
    "omp.ext": "telemetry/CLI namespace",
    "omp.extensions": "manifest table",
    "omp.features": "manifest table",
    "omp.gen_ai": "OpenTelemetry attribute namespace",
    "omp.host": "manifest/supervisor namespace",
    "omp.isolation": "manifest table",
    "omp.lock": "lockfile name",
    "omp.vendored": "manifest table",
}
_DOCUMENTED_NON_EXPORT_PREFIXES: dict[str, str] = {
    "omp.agent.": "OpenTelemetry attribute namespace",
    "omp.artifact.": "OpenTelemetry attribute namespace",
    "omp.blob.v1.": "protobuf package",
    "omp.compaction.": "OpenTelemetry attribute namespace",
    "omp.constraint.": "OpenTelemetry attribute namespace",
    "omp.control.v1.": "protobuf package",
    "omp.document.v1.": "protobuf package",
    "omp.env.v1.": "protobuf package",
    "omp.ext.": "telemetry/CLI namespace",
    "omp.gen_ai.": "OpenTelemetry attribute namespace",
    "omp.inference.v1.": "protobuf package",
    "omp.issue.": "OpenTelemetry attribute namespace",
    "omp.journal.v1.": "protobuf package",
    "omp.telemetry.v1.": "protobuf package",
    "omp.tool.": "OpenTelemetry attribute namespace",
}

# Public modules repeat some package-root exports for discoverability.  Such a
# spelling is not a second API contract: when the root and module attributes are
# the identical object, documentation of either spelling covers the re-export.
# This is identity-checked below rather than being a name-only suppression.


@dataclass(frozen=True)
class DocRef:
    path: str
    line: int


@dataclass(frozen=True)
class FrozenExport:
    path: str
    module: str
    value: Any


def _load_example_gate() -> ModuleType:
    path = _ROOT / "scripts" / "check-python-examples.py"
    spec = importlib.util.spec_from_file_location("_omp_example_gate", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load shared stub provider {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _code_regions(text: str) -> list[tuple[int, str]]:
    """Return inline-code spans and Python signature blocks with line numbers."""

    regions: list[tuple[int, str]] = []
    block: list[tuple[int, str]] | None = None
    fence: str | None = None
    python_fence = False
    for line_number, line in enumerate(text.splitlines(), 1):
        marker = _FENCE.match(line)
        if marker is not None:
            token = marker.group(1)
            if fence is None:
                fence = token[0]
                language = line[marker.end() :].strip().lower()
                python_fence = language in {"py", "python", "python3"}
                block = []
            elif token[0] == fence:
                if python_fence and block is not None and any(
                    _SIGNATURE.match(source) for _, source in block
                ):
                    regions.extend(block)
                fence = None
                block = None
                python_fence = False
            continue
        if fence is not None:
            if block is not None:
                block.append((line_number, line))
            continue
        regions.extend(
            (line_number, match.group(1)) for match in _INLINE_CODE.finditer(line)
        )
    return regions


def _documented_symbols() -> dict[str, list[DocRef]]:
    result: dict[str, list[DocRef]] = defaultdict(list)
    for path in sorted(_DOCS.glob("*.md")):
        if _DOC_NAME.fullmatch(path.name) is None:
            continue
        relative = path.relative_to(_ROOT).as_posix()
        for line_number, region in _code_regions(path.read_text(encoding="utf-8")):
            for match in _SYMBOL.finditer(region):
                symbol = match.group(0)
                ref = DocRef(relative, line_number)
                if ref not in result[symbol]:
                    result[symbol].append(ref)
    return dict(result)


def _public_modules(omp: ModuleType) -> list[ModuleType]:
    modules = [omp]
    for info in pkgutil.walk_packages(omp.__path__, prefix="omp."):
        parts = info.name.split(".")[1:]
        if any(part.startswith("_") for part in parts):
            continue
        module = importlib.import_module(info.name)
        if hasattr(module, "__all__"):
            modules.append(module)
    return modules


def _frozen_exports(omp: ModuleType) -> dict[str, FrozenExport]:
    exports: dict[str, FrozenExport] = {}
    for module in _public_modules(omp):
        names = getattr(module, "__all__", ())
        for name in names:
            if not isinstance(name, str) or name.startswith("_"):
                continue
            path = f"{module.__name__}.{name}"
            exports[path] = FrozenExport(path, module.__name__, getattr(module, name))
    return exports

_DOC_MODULES: dict[str, tuple[str, ...]] = {
    "00-overview.md": ("omp", "omp.limits", "omp.urls"),
    "01-devices.md": ("omp", "omp.devices"),
    "02-verdicts.md": ("omp",),
    "03-params.md": ("omp",),
    "04-placement.md": ("omp", "omp.env"),
    "05-hooks.md": ("omp", "omp.events", "omp.hooks"),
    "06-policy.md": ("omp", "omp.policy"),
    "07-ui.md": ("omp", "omp.ui"),
    "08-context.md": ("omp", "omp.context", "omp.prompts"),
    "09-journal.md": ("omp", "omp.artifacts", "omp.journal", "omp.sessions"),
    "10-telemetry.md": ("omp", "omp.telemetry"),
    "11-env.md": ("omp", "omp.env"),
    "12-agents.md": ("omp", "omp.agents"),
    "13-inference.md": ("omp", "omp.creds", "omp.provider", "omp.secrets"),
    "14-deploy.md": ("omp", "omp.diagnostics", "omp.index", "omp.packages"),
}


def _scoped_mentions(
    frozen: dict[str, FrozenExport],
) -> dict[str, list[DocRef]]:
    """Qualify short API names under the module-owning specification page."""

    result: dict[str, list[DocRef]] = defaultdict(list)
    by_module: dict[str, dict[str, str]] = defaultdict(dict)
    for path, export in frozen.items():
        by_module[export.module][path.rsplit(".", 1)[1]] = path
    for doc_name, modules in _DOC_MODULES.items():
        path = _DOCS / doc_name
        relative = path.relative_to(_ROOT).as_posix()
        for line_number, region in _code_regions(path.read_text(encoding="utf-8")):
            spelling = region.strip().lstrip("@")
            match = re.match(
                r"(?:(?:async\s+)?def\s+|class\s+|type\s+)?([A-Za-z_]\w*)",
                spelling,
            )
            if match is None:
                continue
            name = match.group(1)
            for module in modules:
                public_path = by_module.get(module, {}).get(name)
                if public_path is None:
                    continue
                ref = DocRef(relative, line_number)
                if ref not in result[public_path]:
                    result[public_path].append(ref)
    return dict(result)


def _namespace_citation(
    export: FrozenExport,
    documented: dict[str, list[DocRef]],
) -> str:
    prefix = f"{export.module}."
    refs = [
        ref
        for symbol, symbol_refs in documented.items()
        if symbol == export.module or symbol.startswith(prefix)
        for ref in symbol_refs
    ]
    if not refs:
        return "docs/py/00-overview.md:1"
    ref = min(refs, key=lambda item: (item.path, item.line))
    return f"{ref.path}:{ref.line}"


def _resolve(omp: ModuleType, path: str) -> Any:
    value: Any = omp
    for part in path.split(".")[1:]:
        value = getattr(value, part)
    return value

def _is_non_export(symbol: str) -> bool:
    return symbol in _DOCUMENTED_NON_EXPORTS or any(
        symbol.startswith(prefix) for prefix in _DOCUMENTED_NON_EXPORT_PREFIXES
    )


def _is_python_candidate(omp: ModuleType, symbol: str) -> bool:
    first = symbol.split(".", 2)[1]
    return symbol.count(".") == 1 or hasattr(omp, first)


def _equivalent_reexport(
    export: FrozenExport,
    documented_values: list[tuple[str, Any]],
) -> str | None:
    for spelling, value in documented_values:
        if value is not export.value:
            continue
        # Only collapse an actual package-root/module re-export.  Arbitrary
        # aliases inside classes or facades remain independently auditable.
        if export.path.count(".") > 1 and spelling.count(".") == 1:
            if export.path.rsplit(".", 1)[1] == spelling.rsplit(".", 1)[1]:
                return spelling
        if export.path.count(".") == 1 and spelling.count(".") > 1:
            if export.path.rsplit(".", 1)[1] == spelling.rsplit(".", 1)[1]:
                return spelling
    return None


def _audit() -> tuple[list[str], list[str], int, int]:
    gate = _load_example_gate()
    gate._install_native_stubs()
    sys.path.insert(0, str(_PYTHON))
    omp = importlib.import_module("omp")
    omp._install_control_backend(gate._ControlStub())
    frozen = _frozen_exports(omp)
    documented = _documented_symbols()
    scoped = _scoped_mentions(frozen)

    absent: list[str] = []
    documented_values: list[tuple[str, Any]] = []
    for symbol, refs in sorted(documented.items()):
        if _is_non_export(symbol) or not _is_python_candidate(omp, symbol):
            continue
        try:
            value = _resolve(omp, symbol)
        except AttributeError:
            citations = ", ".join(f"{ref.path}:{ref.line}" for ref in refs)
            module = "omp"
            current: Any = omp
            for part in symbol.split(".")[1:-1]:
                try:
                    current = getattr(current, part)
                except AttributeError:
                    break
                if isinstance(current, ModuleType):
                    module = current.__name__
            absent.append(f"{symbol} — docs {citations}; frozen module {module}")
        else:
            documented_values.append((symbol, value))
    documented_values.extend(
        (path, frozen[path].value) for path in scoped if path in frozen
    )

    all_mentions = dict(documented)
    all_mentions.update(scoped)

    undocumented: list[str] = []
    documented_paths = set(all_mentions)
    for path, export in sorted(frozen.items()):
        if path in documented_paths:
            continue
        alias = _equivalent_reexport(export, documented_values)
        if alias is not None:
            continue
        citation = _namespace_citation(export, all_mentions)
        undocumented.append(
            f"{path} — frozen module {export.module}; docs {citation} (symbol not named)"
        )
    return absent, undocumented, len(all_mentions), len(frozen)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check docs/py/00-14 symbols against the frozen omp export surface."
    )
    parser.parse_args()
    absent, undocumented, documented_count, frozen_count = _audit()
    print(
        f"checked {documented_count} documented spellings and "
        f"{frozen_count} frozen public exports"
    )
    print(f"documented-but-absent ({len(absent)}):")
    for drift in absent:
        print(f"  {drift}")
    print(f"frozen-but-undocumented ({len(undocumented)}):")
    for drift in undocumented:
        print(f"  {drift}")
    if absent or undocumented:
        print(f"docs surface drift: {len(absent) + len(undocumented)}")
        return 1
    print("docs surface drift: 0")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
