# Repository Guidelines

`omp`: pre-release Rust impl of Oh My Pi's coding-agent + inference runtime —
durable agent turns, model routing, project-scoped tools/authorities,
terminal/native presentation, telemetry, embedded free-threaded Python. Rust
rewrite of `pi`: port observable behavior, not TS shape.

## Architecture

- `crates/app`: startup, CLI dispatch, production composition. Domain crates
  implement; app wires, never duplicates.
- `crates/core|storage|proto|rpc|telemetry`: allocation-aware primitives,
  append-only transcript/blob persistence, wire contracts, RPC, observability.
- `crates/agent`: durable turn state, interrupts, event projection, tool
  batching. `crates/llm-catalog`: model/provider data (`data/`) + transports.
  `crates/llm-inference`: typed requests → concrete Tower services, routing,
  recovery middleware → `ChatEvent` streams.
- `crates/tool`: revisioned tool contracts; `crates/tools`: implementations;
  `crates/env`: isolated project processes. `crates/docserver|ast|walker|hashline`:
  document authority, syntax, fs discovery, anchored edits.
  `crates/shell|shell-engine|shell-builtins`: facade, parser/runtime, built-ins.
- `crates/tui`+`tui-macros`: retained declarative DOM; `crates/gui`: native.
  Neither owns agent/provider policy.
- `crates/e2e/tests`: authoritative joined-system proofs P1-P8.
- `PLAN.md`: authoritative plan — locked decisions D1-D8, defect ledger, 8
  parts + checklists.
- `fixtures`, `.plan/quirks`: conformance data, recorded incompatibilities.
  Other `.plan` scratch (research, port, feature-map) NEVER outranks production
  code/tests.
- `.omp/tools`, `scripts`, `crates/*/scripts`: agent tooling, release gen,
  subsystem setup.

Turn flow: `app/src/main.rs` (worker entry, telemetry, `OmpCli` →
`omp_app::run`; command tree `cli.rs`) → `app/src/chat.rs` (authorities,
transcript journal, pending state, tool registry + `AgentSnapshot`) →
`agent/src/loop.rs` (mailbox input/interrupts, `TurnClient`, typed tool batches
via env boundary, durable `AgentEvent`s) → `llm-inference` (facade + Tower
spine, `src/lib.rs`; streamed events → storage → `app/src/chat_ui.rs`) → TUI
retained tree → terminal output materialized once at final renderer.

## Commands

`justfile` = source of truth; use `just`, not raw cargo. `just --list` shows
all recipes.
- One-time before anything linking `omp-py`: `just setup-python`.
- Iterate targeted (`just check-pkg <pkg>`, `just test-pkg <pkg>`); broaden
  (`check`, `test`, `lint`) after the changed contract passes.
- E2E separate + expensive: `just e2e` (or `e2e-build|e2e-core|e2e-p7|e2e-p8|e2e-baseline`).
- `just ci` ≈ CI format+rust jobs locally.

CI (`.github/workflows/ci.yml`): authoritative Cargo-only gate. Format on
Linux; lint/tests/P1-P8/baseline on `macos-15` arm64 (CPython bundle
`aarch64-apple-darwin`-only).

## Conventions

Deps: all in root `[workspace.dependencies]`; members `{ workspace = true }`,
NEVER pin versions. Extra features fine:
`serde = { workspace = true, features = ["rc"] }`. `serde_json` always
`preserve_order` + `raw_value`; `crates/slopjson` (broken/partial/streaming
JSON) mirrors that surface.

Env vars `OMP_*`, never `PI_*`; ported code strips upstream (`pi`, `uu`, …)
env vars, context objects, branding — never aliases. Pre-release: rename+move
(don't copy); clean cutovers; compat shims, old names, deprecated aliases
PROHIBITED; update every caller + remove obsolete exports/tests same change.

Unicode/ANSI: `xutf` for ALL Unicode/UTF-8/16/32, grapheme, display-width,
normalization, ANSI/VT ops. `unicode-normalization` banned. NEVER add utility
crates for these (`unicode-*`, `utf8-*`, `unicode-segmentation`,
`unicode-width`, `ansi_*`, `strip-ansi-escapes`); remove redundant deps, don't
wrap.

Crates: members `crates/*` (virtual workspace, resolver 3); dirs unprefixed
(`crates/demo`); package names `omp-` prefixed (`name = "omp-demo"`). Every
member: real `description` + workspace
`license`/`authors`/`homepage`/`repository`; README (what it is + structural
philosophy); inherits

```toml
[package]
name = "<name>"
version.workspace = true
edition.workspace = true

[lints]
workspace = true
```

Taxonomy: domain prefix after `omp-` (`omp-llm-*`, `omp-shell*`).
**transport** = provider wire protocol ≠ **dialect** = thread rendering to the
LLM; NEVER conflate. Providers = catalog data entries; code only for genuinely
distinct wire behavior; routing stays in inference. `omp-tool` defines
contracts, `omp-tools` implements — never inverted. Daemons = app subcommands,
never standalone `*-d` crates.

Style: pinned nightly (`rust-toolchain.toml`), edition 2024. Lints in root
`[workspace.lints.*]`; `#[allow]` requires `reason`. `cargo fmt` (hard tabs,
3-col, width 100 — `rustfmt.toml`); NEVER hand-format.

Enum↔string: hand-written `match self { … => "…" }` tables (any name —
`name()`, `as_str()`, `label()`, `Display`) PROHIBITED incl. private enums →
derived strum: `IntoStaticStr`/`Display` emit; `EnumString` parse;
`#[strum(serialize_all = "...")]` + per-variant `to_string`/`serialize` for
aliases/irregular names (dotted protobuf paths, multi-word labels —
irregularity ≠ excuse to hand-write); `ascii_case_insensitive` lax input;
`const_into_str` keeps `as_str` `pub const fn`. Custom public parse error:
derive + `map_err`. ONLY escape hatch when strum can't express the shape
(per-arm logic, data variants w/ dynamic strings, one labeled error across
many enums): local `macro_rules!` emitting both directions from one
variant→string table (`vocab!`, `crates/telemetry/src/semconv.rs`). New bare
match table = reviewer-reject; migrate on touch.

Composition/errors/state:
- `crates/app` = DI boundary (registries, concrete Tower services,
  `TurnClient`s, authorities). Libraries NEVER build a second production stack.
- Library errors: `thiserror`, every variant `#[error("…")]`. Hand-written
  `impl Display`/`impl Error` on errors = reviewer-reject. App orchestration:
  `miette`; classify/redact untrusted provider diagnostics before stderr.
- Durable state = append-only transcript journal + blob store; turn state =
  `AgentSnapshot` + journal projection; NEVER a parallel mutable source of
  truth.
- Loops: one `flume` mailbox; priority lifecycle: `tokio::watch`.
  Ownership/cancellation explicit.
- Every public symbol documented (`missing_docs` workspace-warned).

Performance sections below load-bearing + intentionally detailed. NEVER
weaken, summarize away, or bypass in refactors.

### Allocation Discipline (CRITICAL)
Prefer `&T`/`&str`/`&[T]` whenever lifetime permits. Think twice before
`String`/`Vec`. `omp-core` replacements MANDATORY in their target situation,
NOT violations to skip outside it. Test: removes allocations/copies/locking on
a real path? no → default type right; don't churn.
- `Vec<T>` by growth:
  - small (≲12), hot, or short-lived → `smallvec::SmallVec` (inline until
    spill). Cold/long-lived/usually-large → plain Vec (spilled SmallVec =
    worse Vec). Pinned 2.0-alpha (root `Cargo.toml`): two const params, NOT
    1.x array-generic (training-data default; won't compile here):
    ```rust
    SmallVec::<[StateEntry; 8]>::new();  // WRONG — 1.x syntax
    SmallVec::<StateEntry, 8>::new();    // correct — 2.0-alpha syntax
    ```
  - compile-time hard bound → `[T; N]` (`[Option<T>; N]` if slots may be empty).
  - concurrent append-only log, read while written → `omp_core::AppendVec`
    (lock-free appends, stable indices); single-threaded / built before read →
    Vec fine.
  - unbounded, built once, moved once (scratch, collect-and-return, channel
    payloads) → Vec correct.
  - cloned repeatedly (snapshots, per-turn/per-event state, values fanned to
    tasks/channels) → `im::Vector` (Arc-backed structural sharing, O(1)
    clone, cheap mutation of shared copies). Requires `T: Clone`; NOT
    contiguous — consumers needing `&[T]`/`as_slice`/FFI stay on Vec. Only
    pays when the O(1) clone survives to the consumer: small (≲12) vectors of
    cheap-clone items stay SmallVec, and if every consumer flattens back into
    a Vec/SmallVec anyway, the persistent tree is pure overhead — don't
    convert.
- Strings: default `omp_core::Str` (`crates/core/src/str.rs`; NOT smol_str).
  Inline ≤23 bytes; heap `Bytes`-backed: O(1) clone, zero-copy
  slice/split/trim. Build `StrMut`+`freeze()` or `fmts!`; convert `IntoStr`
  (`.to_str()`). Pays for stored/cloned/sliced strings (ids, names, tokens,
  messages). `String` fine as transient build buffer consumed immediately +
  APIs requiring it (`fmt::Write`, FFI, serde sinks). Large/edited text →
  rope (`ropey`).
- Bytes: `omp_core::CowBytes` when shared/sliced/cloned — replaces
  `Cow<'_, [u8]>` (borrowed | `Bytes`-owned; O(1) clone, zero-copy slicing).
  Built once, single consumer → `Vec<u8>` fine.
- Maps/sets keyed by enums/small dense ints → `omp_core::SparseMap`/`SparseSet`
  (bitmap presence + packed values). Clone-heavy maps (state snapshots cloned
  per event/turn, shared caches) → `im::HashMap`/`im::OrdMap` (O(1)
  structural-sharing clone). Plain `HashMap` correct for sparse/unbounded
  keys, strings, no small dense index, and no repeated clones.
- Binary↔text: `omp_core::encoding` (`hex`/`base64`/`base32`), stack
  `ArrayStr<N>` outputs. External encoding crates banned outright — no
  exception.

### Type Size Discipline (CRITICAL)
`clippy::result_large_err|large_enum_variant|large_stack_arrays|large_futures`
= measurement (our type is fat), not a request to add a pointer.
- Boxing to silence a size lint PROHIBITED, reviewer-reject:
  `Err(Box<MyError>)`, `Variant(Box<MyPayload>)`, `Box<SmallVec<..>>`,
  `field: Box<MyStruct>`, box-only wrapper structs,
  `#[allow(clippy::result_large_err, reason = "…")]` — same defect: fat type
  survives, every construction pays an allocation, error path = only heap
  path. Ditto `Box::new([0u8; N])` where a right-sized `Vec`/`BytesMut`
  belongs.
- Fix the type: measure (`size_of`) → find fat field → shrink. Recurring:
  - `SmallVec<T, N>` inline capacity in cold/cloned type (4×`Str` = 136 B):
    cold+cloned → `Arc<[T]>` (16 B, O(1) clone); cold+uniquely owned →
    `Box<[T]>`. Inline capacity = hot/short-lived/usually-small only; never
    declarations, identities, diagnostics.
  - always-contiguous run (physical indexes, sequence ranges) → two-field
    run/range struct, not a collection.
  - identity struct of several `Str`s cloned into maps/messages/errors → one
    `Arc`-backed handle w/ accessors (8 B, O(1) clone, forwarded
    `Eq`/`Ord`/`Hash`).
  - fields derivable from a sibling | duplicated error↔source → delete.
  - error variant carrying a whole aggregate for a `{:?}` → carry only the
    identifying facts it names.
- One exception: foreign fat types (prost message, provider payload,
  unshrinkable foreign struct) MAY box — that ONE field, never our own
  error/enum around it; comment why on the field.
- Pin the win — shrunk type gets a compile-time guard; regression = build
  failure, not a later lint:
  ```rust
  const _: () = assert!(size_of::<Effects>() <= 96, "Effects must stay compact");
  ```

### Async, Iterator & Codegen Discipline (CRITICAL)
House rules, proven in sibling codebase (tetra). Not suggestions.
- Nightly features = the point of the pinned toolchain. Crate MUST gate
  exactly what it uses atop `lib.rs` — and again in every integration
  test/example (separate crates). Canonical trait-plumbing set:
  `impl_trait_in_assoc_type` + `type_alias_impl_trait` (impls infer
  future/iterator types in assoc-type position); `min_specialization`
  (`default fn` fallbacks); `const_eval_select`/`core_intrinsics` codegen
  hints (`core_intrinsics` also needs
  `#![allow(internal_features, reason = "…")]`). NEVER redesign an API around
  a missing stable feature when a nightly gate gives the zero-cost shape.
- Async traits unboxed; MUST NOT allocate per call. Two sanctioned shapes:
  1. callers never name the future → RPITIT:
     `fn run(&mut self) -> impl Future<Output = T> + Send + '_;`
  2. nameable (stored, composed, downstream trait like `tower::Service`) →
     (generic) associated type, impl-inferred:
     ```rust
     pub trait Deliverable<A: ?Sized>: Send + 'static {
        type Result: Send + 'static;
        type Future<'c>: Future<Output = Self::Result> + Send + 'c;
        fn deliver<'c>(self, target: &'c mut A) -> Self::Future<'c>;
     }
     // impl side — concrete type inferred from the async block:
     type Future<'c> = impl Future<Output = Self::Result> + Send + 'c;
     fn deliver<'c>(self, target: &'c mut A) -> Self::Future<'c> {
        async move { /* … */ }
     }
     ```
  `tower::Service`/hyper same rule: `type Future = impl Future<Output = …>;` —
  never `BoxFuture`. Sync answer → `future::Ready<T>`/`future::ready(v)`, not
  an async block, not a box.
- `#[async_trait]`, `BoxFuture`, per-call `Box::pin`: quarantined — ONLY cold
  `dyn` boundaries dominated by real I/O (DNS, remote storage, connection
  establishment); one allocation per network round trip is noise. Per
  message/frame/token/byte → PROHIBITED. Hot-ish `dyn` → box ONCE at
  construction behind an alias
  (`type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;`),
  never per poll/request.
- Enums before `dyn`: one slot, several concrete types → variant per common
  type + single `Boxed(Pin<Box<dyn Trait>>)` fallback; constructor fast-paths,
  boxes only in the `else` arm; common cases dispatch by `match`,
  allocation-free.
- `#[inline]` on small cross-crate hot-path fns; `#[inline(always)]`
  lint-sanctioned when measured.
- Specialize > runtime dispatch: blanket impl (e.g. `Display`-based
  conversion) as `default fn`; concrete fast paths (`&str`, integers)
  override via `min_specialization`. Generic path correct, common path
  allocation- and format-machinery-free, zero branching.
- Iterators lazy, borrowed, unboxed. Return `-> impl Iterator<Item = …> + '_`
  declaring every capability the chain has (`+ Clone`,
  `+ DoubleEndedIterator`, `+ FusedIterator`, `+ ExactSizeIterator`). Yield
  `&T` | O(1)-clone items (`Str`, `Bytes` slices), never fresh allocations.
  NEVER `.collect()` an intermediate `Vec` just to re-iterate — chain adaptors
  end to end, collect only at the final owner, if at all. Nameable iterator
  type (`IntoIterator::IntoIter`, stored field) → TAIT alias, not a written
  adaptor tower, not a box:
  ```rust
  pub type Iter<'s, T: 's> = impl DoubleEndedIterator<Item = &'s T> + FusedIterator + 's;
  impl<'a, T> IntoIterator for &'a Container<T> {
     type Item = &'a T;
     type IntoIter = Iter<'a, T>;
     fn into_iter(self) -> Self::IntoIter { /* plain adaptor chain */ }
  }
  ```
  Containers impl `IntoIterator` for `&T`/`&mut T`/`T` w/ concrete or TAIT
  types. `Box<dyn Iterator>`: same quarantine as `BoxFuture`.
- Tower-style stacks allocate at construction, not per call. Layers compose
  ONCE at build; a request path never assembles middleware.
  `poll_ready` → `call` MUST run on the SAME instance — readiness on one clone
  says nothing about the clone you call; skipping hides backpressure.
  Borrowed-service contract = hand-rolled pin-projected state-machine future,
  not a box: `NotReady { svc: &'c mut S, msg } → Pending(#[pin] S::Future) →
  Done`; `poll` = `ready!(svc.poll_ready(cx))?` then `svc.call(msg)` on that
  same `&mut S`. Pure delegation forwards the inner future verbatim
  (`type Future = <S as Service<Req>>::Future;`) — no wrapper.
  Exception 1 (narrow, documented): type-erasure handle whose readiness gate
  lives INSIDE the erased call MAY `self.clone().oneshot(req)` in an inferred
  future + always-`Ready` `poll_ready` — requires cheap-clone (`Arc`-backed)
  handle + doc comment on `poll_ready` naming where readiness is enforced.
  Never generalize.
  Exception 2 (measured; `async_stream` middleware only): stream-transforming
  layers (retry/rotate/repair) returning a wrapped response stream MAY
  heap-pin one generator per call behind a TAIT alias
  (`Box::pin(async_stream::stream! { … })` inside
  `impl Stream + Send + Unpin`). Fully-inline composition embeds every inner
  layer's state + poll frames in the parent's; a 7-layer stack MEASURED to
  overflow the thread stack at construction (debug builds). Property of the
  current generator impl, not a law — a hand-written pin-projected machine
  avoids the box; preferred for hot layers. Never cite outside
  stream-returning middleware; dyn erasure ≤ once, at the stack's outer
  boundary. Thin wrappers (permit holders, taps) + short-circuits (`Either`,
  one-shot `stream::once`) stay unboxed via pin-projection.
- Scratch buffers: owned once, recycled — two modes, never conflated. Hot
  encode/frame path owns one pre-sized `BytesMut` (`with_capacity` at a
  measured watermark):
  1. true scratch reuse — contents consumed in place before next round:
     `clear()` between rounds; capacity survives; steady state
     allocation-free.
  2. zero-copy transfer — result escapes: `split().freeze()` hands the filled
     prefix (+ its share of the backing allocation) as `Bytes`; unfilled tail
     remains; later rounds `reserve` (amortized realloc). Price of not
     copying — accept knowingly; don't claim capacity survived.
  Derived views (headers, sub-ranges): `slice(..)` on the frozen `Bytes`,
  never a copy. Storage `CowBytes`/`Str`; assembly `BytesMut`.
- Locks: `parking_lot::{Mutex, RwLock}`, never `std::sync`.
  `tokio::sync::Mutex` ONLY when the guard is genuinely held across `.await`.
- Channels: `flume`, never `tokio::sync::mpsc`/`std::sync::mpsc`. Actor loops:
  single flume mailbox; priority signals (resize, shutdown) ride
  `tokio::watch` + `select!`, not a second queue.

### TUI Rendering Doctrine (crates/tui, CRITICAL)
Port exists because pi's `string[]`+ANSI+`render()` contract was per-frame
heap-grooming. Non-negotiable:
- Text parsed ONCE at the boundary: ANSI/VT decomposed (via `xutf`) where
  external text enters (process output, pastes, files); downstream components
  assume ZERO escapes, store none. Sinks get `render(style, text)`; ANSI
  re-emitted exactly once, at final materialization into the stdout buffer.
- Caches own memory: one pooled text buffer + `(Style, Range)` spans;
  re-present = re-slice, not re-parse. Per-frame line buffers (`Vec<Line>`
  fresh each paint) = bug, not style.
- TML degrades like HTML: unknown tag → `CustomElement` (registered renderer
  if any, else children render, layers like `div`). Bad tag MUST NOT fail the
  document into raw-text fallback.
- Props inherit like CSS: `<col fg=blue>hi</col>` colors w/o explicit
  `<text>`; any prop applies where meaningful; well-known props typed +
  non-allocating, arbitrary KV beside. Color fields accept
  `#xxx`|`#xxxxxx`|`rgb(a)`|`hsl(a)`|`lab`/`oklch`|full HTML names|gradients
  as plain `bg`/`fg` values (with angle) — gradients are values, not special
  elements.
- `UiContext` (charset: ascii | unicode | nerdfont; theme) reaches every
  component; hardcoded colors + hand-emitted glyphs banned. Icons from
  `icons.tsv` (generic name + optional specific alias, per-charset, degrading
  inline). Border defaults themed + dim, not `#fff`.
- `dom!`/`layout!` = canonical construction (typed props, loops, `if`/`match`,
  `IntoComponent` for `&str`/`String`/`Str`/`()`/Vec).
  `write!`/`format!`→`String`→reparse = discouraged path.
- Effects are props, not one-offs: shimmer, hover gradient + eased lift,
  streaming reveal (`<text reveal>`), truncate-from-start, tree/checklist,
  clickable scrollbars, non-committed sidebars — example needs it → reusable
  prop/component in core FIRST. Never example-local visual features.
- Examples near-zero boilerplate (`App` host, a `start`, done). Example
  touching kitty image ids, raw escape dispatch, terminal probing, focus
  routing, clipboard internals ⇒ engine missing the primitive — fix engine,
  not example. The editor itself is built from components for recomposition.
- Alt buffer only where required (overlays, welcome scene). Chat/transcripts
  inline + mouse-selectable; quit restores the terminal cleanly (no stray
  mouse-tracking spam).
- Input = one mailbox: decoded `TerminalEvent`s (real input, debug injections,
  resize) through a single async flume mailbox; resize wins via watch +
  `select!`. No polling `read()` loops, no per-example key tables. Keyboard
  input instantly clears mouse-hover; only ever one visible cursor/focus.

### Porting (from pi)
1. Read pi's impl in extreme detail first — incl. `crates/natives`, compat
   shims, support detection, tests. Missed behaviors (editor keys,
   paste/drag-drop, resize-settling, truncation) = user-reported bugs within
   hours.
2. Copy pi's tests; drop TS-shaped compensations (throttles, GC workarounds,
   UTF-16 defenses — "ts is slow, rust isn't"). Port behavior, not shape:
   reimplement where the shape is wrong (mermaid, slopjson, brush parser);
   never wrap what should be native.
3. Generalize while porting: themed, charset-aware, prop-driven engine
   primitives, not one-example checkmarks.
4. Match pi where good (editor UX, telemetry, statusline semantics, alt-buffer
   resize handling); exceed where weak (renderer contract, error taxonomy,
   providers-as-data).
5. Close pi's gaps (missing builtins, slash-arg completion, …) while in the
   area.

### Working Style
- Orchestrate in parallel: one agent per crate/util/provider/category; `sonic`
  for mechanical moves/renames (`sd`/bash bulk renames, never hand edits);
  scouts only for genuinely unknown files. Sequential one-agent-at-a-time =
  failure mode.
- Finish the whole ask: no scaffolds, no "rest is trivial", no half-ports.
  Done = compiles, wired, exercised.
- Verify by running: TUI changes get real-PTY proof (Testing & QA) before
  claiming done — every input path, resize, quit-cleanup.
- NEVER revert/`git checkout` user edits; user edits/renames in flight —
  adapt to the tree as is.

## Key Files
`Cargo.toml`: members, shared deps, lints, release profile.
`rust-toolchain.toml`/`rustfmt.toml`/`clippy.toml`/`rust-analyzer.toml`:
compiler + enforced style/concurrency policy. `.cargo/config.toml`: vendored
`PYO3_CONFIG_FILE`, required before Cargo resolves `pyo3`. `justfile`: all
commands (sync w/ CI + crate READMEs). `crates/proto/proto` + `build.rs`:
protobuf sources, pure-Rust codegen. `crates/tui/README.md` + `icons.tsv`: TUI
architecture, debug protocol, charset-aware icons. `crates/e2e/README.md`:
harness contract. `crates/py/README.md` + `build.rs`: embedded Python linkage,
generated inputs.

## Runtime/Tooling
- Rust pinned `nightly-2026-08-08` (+ `rustfmt`, `clippy`, `rust-analyzer`);
  NEVER redesign nightly-dependent APIs around stable.
- Cargo for Rust; Bun for JS/TS (never Node/npm/pnpm/yarn); `uv` for Python
  (never pip).
- Protobuf: `protox`; no system `protoc`.
- Workspace env vars `OMP_*` only: `OMP_TUI_DEBUG`, `OMP_TTY`, `OMP_PY_SITE`.
  `PYO3_CONFIG_FILE` = required upstream pyo3 exception.
- Release profile deliberate (`opt-level = 2`, thin LTO, 1 codegen unit,
  stripped, unwind panics); change only w/ measured evidence.

### Embedded Python (omp-py)
- `crates/py`: statically links CPython 3.14t (free-threaded), boots
  in-process: `Engine::builder().init()` → `engine.attach(|py| ...)`. Native
  modules: `pyo3::append_to_inittab!` before `init`. `omp-demo` bin ships from
  the same crate. Requires `just setup-python`
  (`crates/py/scripts/fetch-python.sh`) once → gitignored `vendor/python`
  (python-build-standalone archive + derived build inputs).
- Frozen pure-Python packages (e.g. cloudpickle): pinned
  `crates/py/requirements.txt`; fetch script resolves via `uv` → gitignored
  `vendor/python/bundled/` (skipped while stamp matches manifest) +
  regenerates tracked `crates/py/THIRD-PARTY-NOTICES.txt`
  (= `omp_py::THIRD_PARTY_LICENSES`) — rerun after manifest edits, commit the
  notices. Build script only validates stamp + packs; native wheels rejected
  at fetch — those go into site-packages.
- pyo3 via `PYO3_CONFIG_FILE` in `.cargo/config.toml` (default
  `vendor/python/pyo3-config.txt`, fast dev links). Release links
  `vendor/python-release` (`just build-release`); its pgo+lto pbs variant =
  LLVM-22 LTO bitcode, auto-routes through Homebrew LLD 22
  (`brew install lld`, via `crates/py/scripts/ld64.lld`; `needs-lld` marker).
  Enforced loudly by omp-py's build script:
  1. `PYO3_CONFIG_FILE` MUST point at `vendor/python/pyo3-config.txt` before
     cargo runs (repo `.cargo/config.toml` covers members; external crates set
     their own `[env]`/environment) — else pyo3 silently links a host Python.
  2. Consumer bin crates replicate final-link flags `--ld-path=<shim>` +
     `-Wl,-export_dynamic` in their own build script; working examples:
     `crates/app/build.rs`, `crates/e2e/build.rs`.
- Stdlib embedded as marshalled bytecode, served from memory; only real search
  path `$OMP_PY_SITE` (default `~/.local/share/omp-py/site-packages`). End
  users install wheels w/ any free-threaded 3.14 interpreter, no checkout:
  ```sh
  uv python install 3.14t
  uv pip install --python "$(uv python find 3.14t)" \
      --target "${OMP_PY_SITE:-$HOME/.local/share/omp-py/site-packages}" numpy
  ```

### TUI Debugging (`tui` tool, `OMP_TUI_DEBUG`, `OMP_TTY`)
Prefer `.omp/tools/tui.ts`: runs an example/bin on a Bun-native PTY (real
controlling terminal — SIGWINCH resizes + immediate-mode hosts behave as
production); screenshots (`text`), component trees (`tree`), widget values,
key/mouse/paste injection, resizes, raw byte-stream stats as one session-based
tool. Structured ops ride hook 1; hook 2 for external harnesses w/o their own
PTY:
- `OMP_TUI_DEBUG=<unix-socket-path>`: `Terminal::enter` starts a server thread
  on the socket, line-delimited JSON ops (`text`, `tree`, `values`, `keys`,
  `event`, `mouse`, `resize`, `quit`, ...) — see "Debug a running app",
  `crates/tui/README.md`. Wire speaks `TerminalEvent`: injected input rides
  the same mailbox as decoded terminal bytes; `text`/`info` answer from the
  last paint on every host; `frame`/`tree`/`values` = mailbox queries only
  `App` hosts answer (server times out elsewhere); `quit` injects `C-c`.
- `OMP_TTY=<pty-slave-path>`: reroutes ALL terminal I/O (input, rendered
  frames, capability probes, terminal identity) to that device; hold the
  master side to script the UI + capture the exact byte stream a terminal
  would see. stdout untouched.

```python
import fcntl, os, pty, struct, subprocess, termios
master, slave = pty.openpty()
fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
proc = subprocess.Popen(
    ["target/debug/examples/gallery"],
    env=dict(os.environ, OMP_TTY=os.ttyname(slave), TERM="xterm-256color"),
)
os.read(master, 65536)          # frames + control sequences
os.write(master, b"\x1b[C")     # keys (write escape sequences)
os.write(master, b"\x03")       # Ctrl-C quits the examples
```

Caveats: set winsize via `TIOCSWINSZ` before spawn (`SIGWINCH` only reaches
the controlling terminal; live resizes don't propagate). Capability probe
waits for replies — answer DA1 (`\x1b[?62c`) or let it time out. Feed the
master stream to a VT emulator (e.g. `pyte`) for screen assertions.

## Testing & QA
- Unit tests colocated in `src` where private behavior matters; public
  contracts + cross-module → `crates/*/tests`.
- `insta` snapshots: shell parser/tokenizer. `proptest`: encoding, zero-copy
  slicing, transcript replay, round-trip invariants. Review snapshots; NEVER
  accept blindly.
- `crates/app/tests`: production registry, RPC, daemon, document, CLI
  composition — prefer these seams over mocks of production authority.
- `crates/e2e/tests/p1_doc_race.rs`…`p8_baselines.rs`: authoritative for
  concurrency, cancellation, detached jobs, schema isolation, prefix
  stability, crash/replay, real-PTY lifecycle, recorded perf. Bounded waits +
  RAII-owned processes; preserve both.
- P8 = non-gating recorder (metric math/schema, p95 frame time, token-loop
  throughput). NEVER turn noisy host measurements into an unreviewed hard
  gate.
- TUI changes MUST be exercised on a real PTY via `.omp/tools/tui.ts` (or the
  hooks above): input, resize, clean quit restoration.
- No numeric coverage target. Coverage = changed observable behavior defended:
  branch edges, precedence, state transitions, malformed input, cancellation,
  recovery. Narrow test → affected crate → relevant E2E proof.
