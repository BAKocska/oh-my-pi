#!/usr/bin/env python3
"""Gate the example ports against the frozen Python declaration surface.

Run from anywhere in the repository:

    python3 scripts/check-python-examples.py

The checker supplies in-memory native and CONTROL stubs; it never connects to a
host.  Each example is checked in an isolated interpreter so declaration-table
state cannot leak between ports.  A ``# GAP:`` import must still be unresolved
(and a module containing gaps must fail to import), every ordinary import must
succeed, and an export claimed missing by a README ``Gaps`` section is an error.
The report also tracks public functions and methods whose frozen implementation
is an unconditional ``NotWiredError`` stub; these are informational and do not
affect the gate result.
"""

from __future__ import annotations

import argparse
import ast
import importlib
import importlib.util
import json
import re
import subprocess
import sys
import types
from pathlib import Path
from typing import Any

_ROOT = Path(__file__).resolve().parents[1]
_PYTHON = _ROOT / "crates" / "py" / "python"
_EXAMPLES = _ROOT / "examples"
_EXPORT_CLAIM = re.compile(
    r"missing from|not exported|not in (?:the )?frozen|does not yet export|"
    r"(?:is|are) absent|(?:is|are) missing|(?:is|are) pending|exports none of",
    re.IGNORECASE,
)
_CODE = re.compile(r"`([^`]+)`")
_SYMBOL = re.compile(r"(?:omp(?:\.[A-Za-z_]\w*)+|[A-Za-z_]\w*)\Z")


class _StubValue:
    """Permissive inert stand-in for values constructed by native bindings."""

    def __call__(self, *_args: Any, **_kwargs: Any) -> "_StubValue":
        return self

    def __getattr__(self, _name: str) -> "_StubValue":
        return self

    def __iter__(self):
        return iter(())

    def __bool__(self) -> bool:
        return False

    # Frozen modules compare and combine native values (Duration floors,
    # budget arithmetic); the inert stand-in must stay permissive there too.
    def __lt__(self, _other: Any) -> bool:
        return False

    def __le__(self, _other: Any) -> bool:
        return False

    def __gt__(self, _other: Any) -> bool:
        return False

    def __ge__(self, _other: Any) -> bool:
        return False

    def __add__(self, _other: Any) -> "_StubValue":
        return self

    __radd__ = __sub__ = __rsub__ = __mul__ = __rmul__ = __add__


_VALUE = _StubValue()


class _NativeMeta(type):
    def __getattr__(cls, _name: str) -> _StubValue:
        return _VALUE


class _NativeError(Exception, metaclass=_NativeMeta):
    """Common inert native type; exception ancestry supports package errors."""    # Native value types (Duration, budgets) are compared and combined by the
    # frozen modules at import time; instances must stay inert there too.
    def __lt__(self, _other: Any) -> bool:
        return False

    __le__ = __gt__ = __ge__ = __lt__

    def __add__(self, _other: Any) -> "_NativeError":
        return self

    __radd__ = __sub__ = __rsub__ = __mul__ = __rmul__ = __add__



class _NativeModule(types.ModuleType):
    def __getattr__(self, name: str) -> Any:
        if name.startswith("__"):
            raise AttributeError(name)
        value = _NativeError
        setattr(self, name, value)
        return value


def _install_native_stubs() -> None:
    native = _NativeModule("_omp")
    native._runtime_metadata = lambda: {}
    native._phase_legality_matrix = lambda: {}
    native._scheme_snapshot = lambda: (b"stub", ())
    native.operation_spec = lambda _symbol: None
    native._read_bytes_blocking = lambda *_args, **_kwargs: b""
    native._interrupt = lambda *_args, **_kwargs: None
    native._thread_id = lambda: 0
    native.resources = _VALUE
    sys.modules["_omp"] = native

    vocab = types.ModuleType("_omp_url_vocab")
    vocab.URL_VOCAB_VERSION = 1
    vocab.SELECTOR_GRAMMAR = "stub"
    vocab.SCHEMES = (("FILE", ("file",), True),)
    sys.modules["_omp_url_vocab"] = vocab


class _ControlStub:
    """No-I/O CONTROL backend: every attempted request is explicitly unwired."""

    async def request(self, _operation: str, _arguments: dict[str, Any]) -> Any:
        import omp

        error = getattr(omp, "NotWiredError", omp.EnvUnavailable)
        raise error("example gate has no live CONTROL host")

    def effect(self, _effect: dict[str, Any]) -> None:
        import omp

        error = getattr(omp, "NotWiredError", omp.EnvUnavailable)
        raise error("example gate has no live CONTROL host")


def _gap_section(readme: Path) -> list[tuple[int, str]]:
    lines = readme.read_text(encoding="utf-8").splitlines()
    start = next(
        (
            index
            for index, line in enumerate(lines)
            if re.fullmatch(r"##\s+.*\bGaps?\b.*", line, re.I)
        ),
        None,
    )
    if start is None:
        return []
    result: list[tuple[int, str]] = []
    for index in range(start + 1, len(lines)):
        if lines[index].startswith("## "):
            break
        result.append((index + 1, lines[index]))
    return result


def _resolve(root: Any, path: str) -> bool:
    value = root
    for part in path.split("."):
        if part == "omp":
            continue
        try:
            value = getattr(value, part)
        except AttributeError:
            return False
    return True


def _stale_readme_exports(omp: Any, readme: Path) -> list[str]:
    stale: list[str] = []
    inventory = readme == _EXAMPLES / "README.md"
    for line_number, line in _gap_section(readme):
        for sentence in re.split(r"(?<=[.;])\s+", line):
            if not _EXPORT_CLAIM.search(sentence) and not (
                inventory and sentence.lstrip().startswith("|")
            ):
                continue
            for spelling in _CODE.findall(sentence):
                token = spelling.strip().removesuffix("()").lstrip("@")
                if token == "omp" or not _SYMBOL.fullmatch(token):
                    continue
                paths = [token if token.startswith("omp.") else f"omp.{token}"]
                if not token.startswith("omp."):
                    for namespace in ("env", "ui", "agents", "journal", "telemetry"):
                        if f"omp.{namespace}" in line:
                            paths.append(f"omp.{namespace}.{token}")
                exported = next((path for path in paths if _resolve(omp, path)), None)
                if exported is not None:
                    stale.append(f"{readme.relative_to(_ROOT)}:{line_number}: {exported}")
    return stale


def _function_statements(
    node: ast.FunctionDef | ast.AsyncFunctionDef,
) -> list[ast.stmt]:
    statements = node.body
    if (
        statements
        and isinstance(statements[0], ast.Expr)
        and isinstance(statements[0].value, ast.Constant)
        and isinstance(statements[0].value.value, str)
    ):
        statements = statements[1:]
    return [statement for statement in statements if not isinstance(statement, ast.Delete)]


def _call_name(call: ast.Call) -> str | None:
    if isinstance(call.func, ast.Name):
        return call.func.id
    if isinstance(call.func, ast.Attribute):
        return call.func.attr
    return None


def _routes_to_host(
    node: ast.FunctionDef | ast.AsyncFunctionDef, armed_names: set[str]
) -> bool:
    body = ast.Module(body=_function_statements(node), type_ignores=[])
    backend_aliases = {
        target.id
        for child in ast.walk(body)
        if isinstance(child, (ast.Assign, ast.AnnAssign))
        for target in (
            child.targets
            if isinstance(child, ast.Assign)
            else (child.target,)
        )
        if isinstance(target, ast.Name)
        and isinstance(child.value, ast.Attribute)
        and isinstance(child.value.value, ast.Name)
        and "backend" in child.value.value.id
    }

    def routed(expression: ast.expr | None) -> bool:
        if isinstance(expression, ast.Await):
            expression = expression.value
        if not isinstance(expression, ast.Call):
            return False
        name = _call_name(expression)
        if name == "_control_request" or name in armed_names or name in backend_aliases:
            return True
        return (
            isinstance(expression.func, ast.Attribute)
            and isinstance(expression.func.value, ast.Name)
            and "backend" in expression.func.value.id
        )

    return any(
        (isinstance(child, ast.Await) and routed(child))
        or (isinstance(child, ast.Return) and routed(child.value))
        for child in ast.walk(body)
    )


def _function_kind(
    node: ast.FunctionDef | ast.AsyncFunctionDef, armed_names: set[str]
) -> str | None:
    statements = _function_statements(node)
    if len(statements) == 1 and isinstance(statements[0], ast.Raise):
        exception = statements[0].exc
        if isinstance(exception, ast.Call) and _call_name(exception) == "NotWiredError":
            return "stub"
    if _routes_to_host(node, armed_names):
        return "host-armed"
    return None


def _module_functions(
    tree: ast.Module,
) -> list[tuple[tuple[str, ...], ast.FunctionDef | ast.AsyncFunctionDef]]:
    result: list[tuple[tuple[str, ...], ast.FunctionDef | ast.AsyncFunctionDef]] = []

    def visit(statements: list[ast.stmt], scope: tuple[str, ...]) -> None:
        for statement in statements:
            if isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef)):
                result.append((scope, statement))
            elif isinstance(statement, ast.ClassDef):
                visit(statement.body, (*scope, statement.name))

    visit(tree.body, ())
    return result


def _not_wired_stubs() -> list[str]:
    stubs: list[str] = []
    package = _PYTHON / "omp"
    for path in sorted(package.rglob("*.py")):
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        functions = _module_functions(tree)
        armed_names: set[str] = set()
        changed = True
        while changed:
            changed = False
            for _scope, node in functions:
                if node.name not in armed_names and _routes_to_host(node, armed_names):
                    armed_names.add(node.name)
                    changed = True

        relative = path.relative_to(package)
        parts = relative.with_suffix("").parts
        module_parts = parts[:-1] if parts[-1] == "__init__" else parts
        module = ".".join(("omp", *module_parts))
        for scope, node in functions:
            if node.name.startswith("_") or any(name.startswith("_") for name in scope):
                continue
            if _function_kind(node, armed_names) == "stub":
                stubs.append(".".join((module, *scope, node.name)))
    return stubs


def _check_one(module_path: Path) -> dict[str, Any]:
    _install_native_stubs()
    sys.path.insert(0, str(_PYTHON))
    import omp

    omp._install_control_backend(_ControlStub())
    source = module_path.read_text(encoding="utf-8")
    tree = ast.parse(source, filename=str(module_path))
    lines = source.splitlines()
    gaps = 0
    errors: list[str] = []

    for node in tree.body:
        if not isinstance(node, (ast.Import, ast.ImportFrom)):
            continue
        text = "\n".join(lines[node.lineno - 1 : node.end_lineno])
        marked = "# GAP:" in text
        statement = ast.Module(body=[node], type_ignores=[])
        ast.fix_missing_locations(statement)
        try:
            exec(compile(statement, str(module_path), "exec"), {})
        except (ImportError, AttributeError) as error:
            if not marked:
                errors.append(f"line {node.lineno}: ordinary import failed: {error}")
        except Exception as error:  # an import must not execute arbitrary failing work
            errors.append(f"line {node.lineno}: import raised {type(error).__name__}: {error}")
        else:
            if marked and isinstance(node, ast.ImportFrom):
                errors.append(f"line {node.lineno}: # GAP: import now succeeds; remove its marker")
        gaps += int(marked)

    is_package = module_path.name == "__init__.py"
    example_dir = module_path.parent.parent if is_package else module_path.parent
    module_error: BaseException | None = None
    name = f"_omp_example_gate_{example_dir.name.replace('-', '_')}"
    try:
        locations = [str(module_path.parent)] if is_package else None
        spec = importlib.util.spec_from_file_location(
            name, module_path, submodule_search_locations=locations
        )
        if spec is None or spec.loader is None:
            raise ImportError(f"cannot load {module_path}")
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
    except BaseException as error:
        module_error = error
    finally:
        sys.modules.pop(name, None)

    if gaps == 0 and module_error is not None:
        errors.append(f"module import failed without a # GAP: marker: {type(module_error).__name__}: {module_error}")
    elif gaps and module_error is None:
        errors.append("module import succeeds; remaining # GAP: markers are stale")
    elif gaps and not isinstance(module_error, (ImportError, AttributeError)):
        errors.append(
            "module with gaps failed for the wrong reason: "
            f"{type(module_error).__name__}: {module_error}"
        )

    if example_dir.name == "bash-guard":
        errors.extend(_stale_readme_exports(omp, _EXAMPLES / "README.md"))
    errors.extend(_stale_readme_exports(omp, example_dir / "README.md"))
    return {
        "example": example_dir.name,
        "gaps": gaps,
        "errors": errors,
        "stubs": _not_wired_stubs(),
    }


def _example_modules() -> list[Path]:
    modules: list[Path] = []
    for directory in sorted(path for path in _EXAMPLES.iterdir() if path.is_dir()):
        candidates = sorted(directory.glob("*.py"))
        if not candidates:
            # A package-shaped port: exactly one package directory whose
            # __init__.py is the import root.
            candidates = sorted(directory.glob("*/__init__.py"))
        if len(candidates) > 1:
            # Multi-module ports name their import root after the directory;
            # sibling modules are companions it imports.
            named = directory / f"{directory.name.replace('-', '_')}.py"
            if named in candidates:
                candidates = [named]
        if len(candidates) != 1:
            raise SystemExit(f"{directory.relative_to(_ROOT)}: expected exactly one Python module, found {len(candidates)}")
        modules.append(candidates[0])
    return modules


def _run_parent() -> int:
    failures = 0
    modules = _example_modules()
    stubs = _not_wired_stubs()
    for module in modules:
        completed = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "--check-one", str(module)],
            cwd=_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        try:
            result = json.loads(completed.stdout)
        except json.JSONDecodeError:
            result = {
                "example": module.parent.name,
                "gaps": "?",
                "errors": [completed.stderr.strip() or completed.stdout.strip() or "worker produced no result"],
            }
        errors = result["errors"]
        status = "ok" if not errors and completed.returncode == 0 else "FAIL"
        print(f"{status:4} {result['example']:<18} gaps={result['gaps']}")
        for error in errors:
            print(f"     {error}", file=sys.stderr)
        failures += bool(errors) or completed.returncode != 0
    print("stubs:")
    for symbol in stubs:
        print(f"  {symbol}")
    print(f"checked {len(modules)} example modules; {failures} failed")
    return int(failures != 0)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check every example module and README against the frozen Python API using no-I/O native and CONTROL stubs."
    )
    parser.add_argument("--check-one", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.check_one is not None:
        result = _check_one(args.check_one.resolve())
        print(json.dumps(result))
        return int(bool(result["errors"]))
    return _run_parent()


if __name__ == "__main__":
    raise SystemExit(main())
