# PyO3 0.29 performance & zero-copy

Verified against pyo3 0.29.2 (`guide/src/performance.md`,
`guide/src/free-threading.md`, `guide/src/conversions/traits.md`, and pyo3
source). Target: embedded CPython 3.14t, free-threaded.

## Contents

1. [Pointer types & cost model](#1-pointer-types--cost-model)
2. [Zero-copy data exchange](#2-zero-copy-data-exchange)
3. [Extraction & branching costs](#3-extraction--branching-costs)
4. [Calls & interning](#4-calls--interning)
5. [Conversion traits](#5-conversion-traits)
6. [Detach, critical sections, locks](#6-detach-critical-sections-locks)
7. [`#[pyclass]` knobs](#7-pyclass-knobs)
8. [Collection iteration](#8-collection-iteration)
9. [Lazy PyErr](#9-lazy-pyerr)

## 1. Pointer types & cost model

- `Bound<'py, T>` — owned reference, statically attached. Default for
  arguments, returns, locals.
- `Py<T>` — attachment-independent handle. Storage (fields, statics,
  cross-thread) only.
- `Borrowed<'a, 'py, T>` — refcount-free borrow; mostly internal.

Costs:

- `py_obj.bind(py)` → `&Bound` — free (no refcount change).
- `py_obj.into_bound(py)` / `bound.unbind()` — free (ownership handover).
- `Py::clone_ref(py)` — one atomic incref. Cheap, not free; prefer passing
  `&Bound`.
- `bound.py()` — free token extraction. Never `Python::attach` inside code
  that already has a `Bound`.
- Dropping `Py<T>` while **detached** queues a deferred decref through a
  global reference pool (synchronization on next attach). Drop while
  attached, or build with `pyo3_disable_reference_pool` cfg in
  latency-critical binaries.

## 2. Zero-copy data exchange

Borrowing Python's memory (all `'py`-scoped):

- `Bound<PyString>::to_str() -> &'py str` — borrows the internal UTF-8 rep
  (`PyUnicode_AsUTF8AndSize`). Same for `extract::<&str>()`.
- `Bound<PyBytes>::as_bytes() -> &'py [u8]` — borrows `PyBytesObject`'s
  buffer. `str`/`bytes` are immutable, so reads are thread-safe on 3.14t.
- `PyBuffer::<T>::get(obj)` — buffer protocol (`bytearray`, `array.array`,
  memoryview, ndarray). `as_slice(py)` / `as_mut_slice(py)` give zero-copy
  `&[ReadOnlyCell<T>]` / `&[Cell<T>]` for C-contiguous data; buffer released
  on Drop. For heavy ndarray work use `rust-numpy`'s
  `readonly()`/`readwrite()` → `ArrayView`.

Escaping `'py` without copying:

- `PyBackedStr` / `PyBackedBytes` own the `Py<PyString>`/`Py<PyBytes>` and
  deref to `&str`/`&[u8]`. Zero-copy construction on non-limited API;
  `Send + Sync` (backing objects immutable) — safe to ship across threads
  detached.

Producing Python bytes:

- `PyBytes::new(py, &slice)` — one allocation + one memcpy.
- `PyBytes::new_with(py, len, |buf| { … })` — allocate the Python buffer
  once, serialize directly into it. Use whenever the length is known;
  removes the intermediate `Vec<u8>` and its memcpy.

## 3. Extraction & branching costs

- `extract::<T>()` runs `FromPyObject`; on mismatch it builds a **full
  `PyErr`** (message allocation). Fine for one-shot conversion.
- `cast::<T>()` / `downcast::<T>()` is a type-slot check returning a
  lightweight `PyDowncastError`. Mandatory for union-type branching:

```rust
if let Ok(list) = val.cast::<PyList>() { … }
else if let Ok(s) = val.cast::<PyString>() { handle(s.to_str()?) }
else { return Err(PyTypeError::new_err("expected list or str")); }
```

- `#[derive(FromPyObject)]` = one `getattr`/`get_item` per field, each
  wrapping intermediate objects. Hot paths take `&Bound<PyDict>` and pull
  fields manually with interned keys.
- `#[pyo3(signature = (arg = expr))]` defaults are lazy closures — `expr`
  only runs when the caller omits the argument. Defaults are free to be
  non-trivial.

## 4. Calls & interning

- Every `getattr("x")` / `set_item("k", …)` / `call_method("m", …)` with a
  bare literal allocates a temporary `PyString`. `intern!(py, "x")` caches a
  `Py<PyString>` in a static `PyOnceLock` and interns it — dict lookups
  become pointer compares.
- Calls dispatch via vectorcall (PEP 590) when args are a **Rust tuple**:
  `obj.call1((a, b))` allocates no `PyTuple`. Passing a prebuilt
  `Bound<PyTuple>` falls back to `tp_call`.
- `call_method0("m")` → `PyObject_CallMethodNoArgs`, fastest method path;
  `call_method1("m", (a,))` — vectorcall, no tuple; `call_method` with
  `Some(kwargs)` does getattr + call.
- `PyTuple::new(py, slice_or_array)` — don't collect into a `Vec` first when
  the arity is known.

## 5. Conversion traits

- Current: `IntoPyObject<'py>` (unifies `T` and `&T`; `Output` may be
  `Borrowed` — e.g. `bool` converts refcount-free) and `IntoPyObjectExt`
  (`into_py_any`, `into_bound_py_any`).
- `ToPyObject` / `IntoPy` are dead — do not write new impls.
- Derives: `#[derive(IntoPyObject)]` → dict; on tuple structs → tuple;
  `#[pyo3(transparent)]` delegates a newtype straight to the inner type
  (no dict/tuple allocation). `#[derive(IntoPyObjectRef)]` for `&T`.
- Wrappers around existing Python objects: implement manually returning
  `Bound`/`Borrowed` with `type Error = Infallible`.

## 6. Detach, critical sections, locks

- `py.detach(|| …)` is **still required with the GIL off**: an attached
  thread running long Rust code blocks stop-the-world sync (GC, fork,
  settrace) for the whole process. Rule of thumb: blocking I/O or >~1ms
  compute → detach.
- Nothing `'py` may cross `detach` — `unbind()` to `Py<T>` or extract plain
  Rust data first.
- `pyo3::sync::with_critical_section(obj, || …)` — per-object lock
  (`PyCriticalSection`) for multi-step mutation of a shared `PyList`/
  `PyDict`/`PyByteArray`; compiles to a plain call on GIL builds.
- In-class state: single scalar → `std::sync::atomic`; composite → short
  `parking_lot::Mutex`/`RwLock` sections (house rule). NEVER call into
  Python while holding a plain lock; if that's unavoidable, use
  `std::sync::Mutex` + `MutexExt::lock_py_attached(py)` (deadlock-safe
  against stop-the-world).

## 7. `#[pyclass]` knobs

- `frozen` — replaces the per-instance atomic borrow flag with an empty
  slot: `&self` methods skip the compare-exchange entirely, and
  `py_obj.get()` yields `&T` without a token (requires `Sync`). Unfrozen +
  concurrent `&mut self` on 3.14t → `RuntimeError: Already borrowed`.
  Default to frozen.
- `immutable_type` — `Py_TPFLAGS_IMMUTABLETYPE`: enables interpreter-level
  method caching, blocks runtime monkey-patching.
- `freelist = N` — slab for short-lived, high-churn instances (vectors,
  tokens).
- Omit `subclass`, `dict`, `weakref` unless needed — each adds per-instance
  pointers and lookup overhead.
- `eq`, `ord`, `hash` (+ derive `PartialEq`/`PartialOrd`/`Hash`) — C-level
  `tp_richcompare`/`tp_hash` slots, no Python dispatch. `hash` requires
  `frozen`.
- Field access: `#[pyo3(get, set)]` emits offset-based `PyGetSetDef`
  descriptors — cheaper than `#[getter]` methods, which pay a full call
  trampoline. Use getter methods only for validation/laziness/derived
  values.

## 8. Collection iteration

- `list.extract::<Vec<T>>()` allocates and converts every element — only
  when an owned `Vec` is the actual product.
- `for item in list.iter()` borrows; on free-threaded builds the iterator
  wraps access in a critical section and specializes `fold`/`all`/`any` to
  hold it once.
- `try_iter()` (`PyObject_GetIter`) is for arbitrary iterables. On a known
  `PyList`/`PyTuple` use `.iter()` — indexed C-slot access, no iterator
  protocol.

## 9. Lazy PyErr

- `PyValueError::new_err(args)` stores a **lazy** state: no Python exception
  object, no traceback until the error crosses into Python or is inspected
  (`value(py)`, `traceback(py)`).
- Therefore: `Err(...)` returns on Rust paths are cheap; keep errors as
  thiserror enums internally and convert via `From<MyError> for PyErr` at
  the boundary; never inspect/normalize a `PyErr` in a loop.
