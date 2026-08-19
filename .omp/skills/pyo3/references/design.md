# PyO3 0.29 design & maintainability

Verified against pyo3 0.29.2 (`guide/src/module.md`, `guide/src/class.md`,
`guide/src/exception.md`, `guide/src/free-threading.md`,
`guide/src/migration.md`, pyo3 source). Target: statically embedded CPython
3.14t, modules registered pre-init via `append_to_inittab!`.

## Contents

1. [Module organization](#1-module-organization)
2. [Newtype boundary architecture](#2-newtype-boundary-architecture)
3. [Error taxonomy](#3-error-taxonomy)
4. [`#[pymethods]` conventions](#4-pymethods-conventions)
5. [GC integration](#5-gc-integration)
6. [Embedding hygiene & statics](#6-embedding-hygiene--statics)
7. [Free-threaded class design](#7-free-threaded-class-design)
8. [Testing bindings](#8-testing-bindings)
9. [Stubs & abi3](#9-stubs--abi3)

## 1. Module organization

- Function-style `#[pymodule] fn _omp(py, module) -> PyResult<()>` (house
  shape, `bindings.rs`) or declarative `#[pymodule] mod name { … }` with
  `#[pymodule_export]` items and `#[pymodule_init]` for imperative setup.
  Either way the module carries `gil_used = false` explicitly —
  `gil_used = true` re-enables the GIL process-wide at import with only a
  `RuntimeWarning`.
- Embedding order invariant: `append_to_inittab!(_omp)` MUST run before the
  interpreter boots (`Builder::init` / `Python::initialize`). In this repo
  that is `bindings::register()` called from `Builder::init`.
- Submodule gotcha: exporting a child `#[pymodule]` mounts it as an
  attribute (`parent.child` works) but does NOT register `parent.child` in
  `sys.modules` — `import parent.child` / `from parent import child` fail.
  Fixes: `#[pymodule(submodule)]` for qualname correctness, plus manual
  `sys.modules[full_name] = module` insertion when dotted imports must work.
- Prefer one flat native module (`_omp`) wrapped by pure-Python packaging
  (`crates/py/python/omp/`) — the Python layer owns import ergonomics,
  docstrings, and API surface; the native layer stays minimal.

## 2. Newtype boundary architecture

- `#[pyclass]` types cannot have lifetimes or generic parameters, and in
  0.29 must be `Send + Sync` (or opt out with `unsendable`, which poisons
  cross-thread use — avoid in this repo).
- Keep pyo3 out of domain crates. Bindings live in the binding crate
  (`crates/py`), wrapping domain types as newtypes:

```rust
#[pyclass(name = "EnvPath", frozen, module = "_omp")]
struct PyEnvPath(EnvPath);
```

  Benefits: core stays FFI-free and unit-testable; the orphan rule is
  satisfied for foreign types; Python-facing naming/docs evolve
  independently. Cost: explicit `.0` plumbing — acceptable.
- `#[pyo3(transparent)]` on single-field wrappers delegates
  `FromPyObject`/`IntoPyObject` to the inner type.
- Enum vocabularies exposed to Python: the house `string_enum!` macro
  (classattr constants over a frozen newtype). Rust-side strings still come
  from strum derives on the inner enum, per workspace lint policy.

## 3. Error taxonomy

- Define a Python exception hierarchy once with `create_exception!`,
  mirroring the domain error structure (see the `create_exception!` block in
  `bindings.rs`: `OmpError` root, `ManifestError`/`CapabilityError`/…
  children). Python
  callers catch by taxonomy, not string matching.
- Libraries keep thiserror enums; the binding crate owns exactly one
  `impl From<CoreError> for PyErr` mapping variants onto the hierarchy.
  `?` then works across every `#[pyfunction]`.
- Preserve causes: `py_err.set_cause(py, Some(inner_pyerr))` = Python's
  `raise … from …`. `err.add_note(py, "…")` for PEP 678 context notes.
- `features = ["anyhow"]` / `["eyre"]` exist (map to `PyRuntimeError`,
  unwrapping an inner `PyErr` if the chain contains one) — acceptable only
  at application-orchestration boundaries, never as a substitute for the
  typed taxonomy.
- Message text goes through the exception constructor lazily
  (`ExecutionError::new_err(msg)`); don't pre-format into `PyErr` on paths
  that usually succeed.

## 4. `#[pymethods]` conventions

- `#[new]` returns `Self` / `PyResult<Self>` / `Py<Self>` (the latter for
  caching/singletons). `#[staticmethod]`; `#[classmethod]` takes
  `&Bound<'_, PyType>` first.
- Properties: `#[pyo3(get)]` on fields for plain reads;
  `#[getter]`/`#[setter]` methods only when computing or validating.
- Signatures: `#[pyo3(signature = (query, limit = 10, *, timeout = 30.0, **extra))]`
  gives keyword-only args, defaults, varargs (`*args: &Bound<PyTuple>`),
  kwargs (`**kw: Option<&Bound<PyDict>>`). `Python<'_>` parameters are
  excluded from the signature tuple. Add `text_signature` where the derived
  one is unclear.
- Comparisons/hash: prefer `#[pyclass(frozen, eq, hash)]` +
  `#[derive(PartialEq, Hash)]` — C-slot implementations, and `hash`
  requires `frozen`. Hand-written `__richcmp__` (as in `string_enum!`) is
  for order-aware or cross-type semantics the derive can't express. Magic
  methods have fixed C-API signatures; `signature = …` is rejected on them.
- Naming: `#[pyclass(name = "PyVisibleName", module = "_omp")]` always —
  without `module`, `__module__` is `builtins` and pickling breaks
  (`__module__ + "." + __qualname__` must be importable).

## 5. GC integration

Any `#[pyclass]` holding `Py<T>`/`Py<PyAny>` can form cycles that refcounting
alone never frees. Implement both:

```rust
fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
   if let Some(ref cb) = self.callback { visit.call(cb)?; }
   Ok(())
}
fn __clear__(&mut self) { self.callback = None; }
```

- `__traverse__` may ONLY visit; `Python::attach` (or any interpreter
  access) inside it panics by design.
- Frozen classes holding `Py<T>` in `Option`+lock fields clear through the
  lock in `__clear__`.
- If a class only holds leaf objects that can never reference it back
  (`PyBytes`, `PyString`, numbers), traversal is unnecessary — document why.

## 6. Embedding hygiene & statics

- Renames (0.29 canon): `Python::attach` ← `with_gil`; `Python::detach` ←
  `allow_threads`; `Python::initialize` ← `prepare_freethreaded_python`;
  `PyOnceLock` ← `GILOnceCell`; `GILProtected` removed.
- Never enable `auto-initialize` in this repo: boot is owned by
  `Engine::builder().init()` (frozen-module table + isolated config must
  precede interpreter init).
- Statics holding Python objects → `PyOnceLock<Py<T>>`
  (`get_or_init(py, || …)` detaches while waiting on another thread's init;
  plain `OnceLock`/lock waits can deadlock against stop-the-world).
  `OnceLockExt::get_or_init_py_attached` upgrades an existing
  `std::sync::OnceLock`.
- Pure-Rust runtime state pushed from the host lives in
  `LazyLock<…parking_lot…>` statics (see `RUNTIME` in `bindings.rs`); the
  invariant is that no Python API is called while such a lock is held.
- One interpreter per process (`INITIALIZED` guard in `lib.rs`); don't
  design for re-initialization. Sub-interpreters inherit the frozen table
  but share process statics — prefer module attributes or pyclass instances
  for state that must be per-interpreter.

## 7. Free-threaded class design

- Attachment (`Python<'py>`) no longer implies exclusivity: any number of
  threads run Python and your methods concurrently.
- Make classes `frozen` + interior mutability: atomics for scalars,
  short-held locks for composites. Unfrozen `&mut self` methods are a
  concurrency landmine (`Already borrowed` at runtime).
- Multi-step mutation of shared *Python* containers →
  `with_critical_section(obj, || …)`.
- Holding a lock while calling into Python risks deadlock with GC's
  stop-the-world: restructure, or use `MutexExt::lock_py_attached`.

## 8. Testing bindings

- `Python::initialize()` then `Python::attach(|py| …)` in tests. In this
  repo, boot through `Engine::builder().init()` instead when the frozen
  stdlib or `_omp` module is under test — plain `initialize` has no stdlib.
- Panics in `#[pyfunction]`/`#[pymethods]` are caught by pyo3's trampoline
  and surface as `pyo3_runtime.PanicException` (a `BaseException`
  subclass). Test with
  `err.is_instance_of::<pyo3::panic::PanicException>(py)`; never rely on
  unwinding across the FFI boundary.
- Behavior over plumbing: test exception taxonomy mapping, signature
  defaults/kwarg edges, and thread-safety of frozen classes — not field
  wiring.

## 9. Stubs & abi3

- Ship `.pyi` stubs for the native module (PEP 561) next to the pure-Python
  package; `experimental-inspect`/`pyo3-introspection` can generate drafts.
  Stubs are static-analysis contracts only.
- `abi3` is irrelevant here: free-threaded builds don't support the limited
  API and pyo3 disables it when compiling against 3.14t. Never enable it.
