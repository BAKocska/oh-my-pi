# PHILOSOPHY

What omp is trying to achieve, distilled from a year of building it — first as a growing
pile of diffs against [Pi](https://github.com/earendil-works/pi), now as omp², a
ground-up Rust rewrite. Every claim below has a receipt: a design that shipped, a number
that was measured, or a dated quote from the prompt history that built this repo.

The one-sentence version: **build a coding harness so good that the model is the only
replaceable part.**

## The two bets

**1. The harness is the product.** Models converge; harnesses decide how much of a model
you actually get. The same frontier model lands or flails depending on the edit format,
the tool roster, the transcript it reads back. So the harness earns obsessive engineering:
the renderer, the shell, the document layer, the inference spine are each rebuilt from
the contract up rather than patched at the symptom.

**2. Agents are both the builders and the users.** omp is written largely by the agents
it hosts, and its surfaces are consumed by models more than by humans. That inverts
several defaults:

- Pick implementation languages by *idiom convergence*, not preference. Agents average
  the training corpus; TypeScript hands every codebase its own dialect, Rust rejects the
  unsound version before it runs. "Pick a language your agents can't ruin." Mediocre-but-
  uniform beats brilliant-but-divergent.
- Serve the names models were trained on. The in-process shell answers `grep` and `find`
  with ripgrep-class implementations under the muscle-memory names — nobody
  prompt-engineers "please use rg instead."
- Decode what they meant, not what they said. `file_path` for `path`, `"true"` for a
  boolean, a bare string for a one-element array: repair charitably, save the round-trip,
  and record the raw emission with the repair flagged so model quality stays measurable.
- Deputize the model as QA. A standing `report_issue` instruction turns every session
  into a test run; false positives are training data for the detector.
- Validate surfaces by use, not by review. The Python extension SDK was frozen only
  after waves of subagents ported ~80 real extensions against it and every gap they hit
  was closed ("goal is to make it a full surface, not just bandaid current things we
  miss" — 2026-08-20).

## The eight lessons

One year, one lesson per subsystem (the omp² essay series expands each):

1. **The Stack** — pick a language your agents can't ruin. Rust engine; embedded
   free-threaded Python for extensions (agents write decent Python, it embeds small, and
   it can introspect itself).
2. **The TUI** — an ANSI-escaped `string[]` plus `render()` is not a rendering
   primitive. Text is parsed once at the boundary; styled runs stream through one sink;
   ANSI is emitted exactly once, at materialization. One session's render cost:
   267 s → 90 ms.
3. **The Shell** — the shell tool should not actually shell out. Full bash parser,
   interpreter, and 66 coreutils in-process; one expansion semantics on every platform;
   the session owns every process tree "from the parse to the kill signal" — otherwise
   cancellation is an illusion.
4. **The Architecture** — agents don't read files, they check out documents. Everything
   two agents could fight over (documents, LSP servers, OAuth tokens, messages)
   converges on one daemon; a read pins a revision, an edit is a compare-and-swap,
   conflicts get a fuzzy 3-way rebase where both histories are visible. The same RPC
   boundary makes local, VM, remote, and headless-fleet deployments one topology.
5. **The Inference** — a provider flag is never just a flag. Forced calls and grammar
   constraints are *intents* the harness satisfies by the cheapest reliable path (soft
   prompt first, native flag only when free, bounded escalation). No silent capability
   drops — every degradation is an explicit receipt. Provider differences are catalog
   *data* ("nginx for models"); runtime code never inspects a model name.
6. **The Tools** — you're paying to not use tools. Every registered schema taxes every
   token of every turn, called or not. Core tools earn wire slots; everything else —
   MCP servers, extensions — rides a discoverable device bus behind the `dyn` tool at
   zero schema slots, so the prompt cache survives and TTFT stays flat.
7. **The Loop** — the string is not the truth. A call settles into one durable typed
   verdict; the model prompt, the UI render, and the transcript are projections of it.
   Error messages train the model as hard as confirmations, so ad-hoc strings are banned
   in both directions.
8. **Version everything the model sees.** Every call is stamped `family@rev`
   (`edit@hl.3`); old calls `lift()` into the current dialect so histories stay coherent
   across model switches; replay at the same rev is byte-stable, so prefix caches
   survive. Unversioned transcripts are write-only data.

## Doctrine

**Structured truth, projected views.** The disease behind most harness bugs is treating
model-facing text as the source of truth. Fix that once — typed verdicts, revision-stamped
journal, deterministic rendering — and compaction, re-rendering, per-rev metrics, and
"how often does the fuzzy rebase fire?" all become queries instead of regex archaeology.

**Own the whole mechanism or don't ship it.** No wrapping what should be native: the
JSON repairer is a parser (`slopjson`), not a regex pass in front of serde; the shell is
an interpreter, not a `/bin/sh -c` wrapper; the renderer owns every byte it draws. Safety
rules are AST queries, not regexes fishing for `rm -rf` in a string.

**Speculation, then effect.** Everything before commit is speculation, everything after
is effect. Tools stream arguments and dry-run as values complete; effects are authorized
only once the assistant item is durable. Dropping an uncommitted invocation leaves the
world untouched. Cancellation is owned by resources (doc leases, process groups, worker
supervisors), never by per-tool flags authors will get wrong.

**Honesty over convenience.** Satisfied, rejected, or changed under explicit policy —
never silently dropped. Semantic retries are transactional: consumers never see an event
that may later be retracted. Benchmarks show the losing rows.

**Data over code.** Providers, models, quirks, thinking policies: catalog entries
compiled offline, not `if model_id.contains(...)` scattered through middleware. The
system "does not have thousands of unique provider implementations. It has a small set
of wire codecs, a small set of policy profiles, many routes, and many model rows."

**Delete the weightless machinery.** The parallelism planner, the approval gate, the
cancellation taxonomy, the `loadMode` half-measure — all built, all measured against
reality, all deleted. A batch that rare does not earn an admission machine. The
counterweight to maximalism is the willingness to reverse it.

## How to choose

The decision procedure behind the doctrine, in the order it actually runs:

**1. Write the call site first.** A design is judged where it is used, not where it is
defined. "Why not just `Self::new(...).detail(...).committed(...)`" (2026-08-14) killed
a construct-then-mutate error API; "instead of this ugly emit thing, why not make it an
async stream" (2026-08-13) killed a callback interface. If the usage reads badly, the
abstraction is wrong — redesign, don't document around it.

**2. Name the concepts before writing the code.** Most spaghetti is two concepts sharing
one name: "no dude. broker != gateway" (2026-08-12); provider ≠ route ≠ codec ≠ model;
transport ≠ dialect. Conversely, refuse splits that carry no weight: "llm-anthropic |
llm-google etc should not exist, i dont want to split this into 10 crates. i decided"
(2026-08-13). The boundary follows the concept, never the org chart of the old code.

**3. Ask whether the variance is data.** If behavior differs *per entity* (per model,
per provider, per icon, per prop), it's a table, a KDL file, a catalog row — never a
branch. "i dont quite like the codex change we made. it hardcodes compat stuff for codex
into the code, instead of the KDL layer" (2026-08-19). Code is reserved for genuinely
distinct *kinds* of behavior.

**4. Read the prior art's source, in parallel.** Before the GUI webview: "make sure to
clone and read wry throughout, so we know what we're doing with that baseline." Before
the inference spine: subagents traced every remote-provider call path in Pi. Before the
extension SDK: agents read the actual Pi extension catalog's source. Survey by
delegation, decide personally.

**5. Design by simulation, validate by use.** The extension layer was specified as
docs "as if all these things existed," then real extensions were ported against the
frozen surface in waves until the gap tables emptied. A surface nobody has consumed is a
guess; a ported consumer is evidence. Same instinct at small scale: place a fact where
it belongs and check the placement — "i feel like this doesnt belong here? dont we have
a uicontext charset thing" (2026-08-10).

**6. Generalize the primitive, not the instance.** "lets first implement animation, its
a fundamental part of any renderer afterall. dont think about just our case though"
(2026-08-09). An example needing a capability means the engine is missing a primitive.
The inverse guard is rule 3 of Doctrine's deletions: a generalization no caller needs is
weight, not foresight.

**7. When the fix stays ugly, re-derive the requirement.** "can you think of a more
universal fix to this issue; by rethinking what this identifier is needed for from
scratch??" (2026-08-22). A refactor that comes out as spaghetti again means the plan
treated symptoms; stop, name the disease ("five symptoms, one disease"), restart from
the actual requirement.

**8. Prefer ladders to gates.** Almost no capability is binary: soft prompt → free
native flag → paid escalation (forced calls); enforce → degrade to client-side repair
(grammar budget); permissioned zero-copy path → fallback ("we could just try the
permissioned path -> fallback to worse one" — 2026-08-19). Erroring and silent dropping
are both failures; a graded path with receipts is the design.

**9. Lock it, write it down, delete the losers.** Decisions get numbered (D1–D8),
restated in full, and carried forward; superseded plans are deleted or annotated
`abandoned`, "instead of mentioning its superseded everywhere" (2026-08-19). Precedence
between documents is explicit. An architecture that lives in chat history is not a
decision.

**10. Keep an honest ledger.** Open defects are recorded with owner and classification —
real work, cosmetic, or needs-a-ruling — and "these are open, but idc" (2026-08-20) is a
legitimate, recorded state. What is not legitimate: shipping a feature list while the
product regressed. "we added all this bullshit features and yet neither of these things
are working as good as /work/pi out of the box" (2026-08-21) — the out-of-box experience
is the acceptance test that outranks every checklist.

## Rust, specifically

The code-level taste, enforced at reviewer-reject severity in AGENTS.md. The common
thread: **the default type is a claim about the data's life, so make the claim true.**

**Memory is a decision, not a default.** "think twice before using String | Vec"
(2026-08-08, the first taste rule ever written into AGENTS.md). The ladder: borrow if
the lifetime permits; `SmallVec` if small and hot; `[T; N]` if bounded; `Str` (inline
≤23 bytes, O(1) clone, zero-copy slicing) for stored strings; rope for edited text;
plain `Vec`/`String` only for build-once-move-once buffers. Refs and borrows are not
advanced Rust — "agents shouldnt be afraid of using refs and borrows." Per-frame or
per-token allocation is a bug regardless of profiler evidence: "yes, but its allocated
on each frame dude." And the optimization must reach the consumer to count: `im::Vector`
for clone-heavy state, but "if ur gonna cloned.collect, the im::Vec makes no sense"
(2026-08-20) — a persistent tree every caller flattens is pure overhead.

**Fix the type, never box the lint away.** A `result_large_err` or
`large_enum_variant` warning is a measurement: the type is fat. "some of them box the
error types… this is disgusting: what should have been done is instead changing the
error type so that it does not suck" (2026-08-20). Measure with `size_of`, find the fat
field, shrink it (`Arc<[T]>`, run structs, handle types), then pin the win with a
compile-time size assert. `Box::new(Box::new(...))` appearing anywhere means someone
was silencing the compiler instead of listening to it. Judgment still applies: a
64 KiB I/O scratch buffer on the heap "does make sense so lets keep that."

**Errors are types, never strings.** `Result<T, String>` "is disgusting"; every library
error is `thiserror` with typed fields; hand-written `impl Display` on an error means
"it should just use thiserror" (2026-08-20). The deeper rule: "NEVER USE FORMATTERS WITH
ERROR TYPES — PROPAGATE INNER TYPE" — carry the source error and the identifying facts
as fields ("why not just make the format type == incoming format's type, and message ==
other"), render once at the app/miette boundary. An error that stringifies its cause has
destroyed information to save a type parameter.

**Derive both directions from one table.** Hand-written enum↔string matches are "these
stupid match statements" (2026-08-11): strum derives emit and parse from the variant
list; `vocab!`-style macros cover the shapes strum can't. Same instinct everywhere a
static vocabulary exists: `Str`-typed wire fields become real enums, `KNOWN_PROPS`
tables become a macro-generated typed prop struct, scattered escape literals become one
`esc!`. If two artifacts must agree, one source generates both.

**Macros are vocabulary, and vocabulary is short.** `sf!`, `esc!`, `dom!`, `layout!`,
`semver!` — each names one concern, is used consistently everywhere, and does its
dispatch at compile time: `sf!("literal")` compiles to a static, `sf!("{x}")` to a
format, because "you can just match at the macro level no?" (2026-08-20). Names are cut
to the bone — `fmts!` → `str!` → "we cant call it str!, its reserved" → `sf!`, and "we
dont need an alias, just call it sf!". One name per concept; aliases are drag.

**Standard traits over bespoke ones.** The old provider "facets" — a struct of optional
trait objects per capability — was "almost like a manual vtable… they only had to
implement `tower::Service` for the said request type" (2026-08-13). If an ecosystem
trait (`Service`, `Stream`, `FromStr`, `IntoIterator`) expresses the contract, implement
it and inherit the ecosystem; conversion ergonomics ride `Into*` traits (`IntoComponent`
for `&str`, `String`, `Str`, `()`, `Vec<_>`) rather than overload forests.

**Nightly is the point of pinning.** TAIT and `impl_trait_in_assoc_type` so async traits
and iterators are unboxed and impl-inferred; `min_specialization` so the generic path is
correct and the common path is free. "enabling nightly stuff so that traits can use
inferred types, heavily avoiding async boxing… note them down with a STRICT wording"
(2026-08-10, importing the tetra house rules). Never redesign an API around a missing
stable feature.

**Concurrency has one shape.** `flume` mailboxes and `parking_lot` locks (std/tokio
equivalents banned wholesale); an actor is one mailbox of decoded, typed events — "You
must have a SINGLE mailbox for Terminal, that gets DECODED PROPER EVENTS" (2026-08-10) —
with priority signals on `tokio::watch` + `select!`, never a second queue. Cancellation
is structural: drop-cancellable futures, `CancellationToken` over ad-hoc
`watch::Sender<()>`, process groups for anything that escaped the process.

**Every dependency defends its seat.** Both failure modes are real. Wrapping or
reimplementing a real library is rejected — "use the fucking library" (2026-08-08,
bytemuck for byte casting; ropey; the grep-* stack). But redundant and trivial deps get
evicted on sight: "do we rlly need fs2 AND fs_extra? could we really not implement
normalize-path/os-path/secrecy ourselves? whys smol_str here, its not even used??"
(2026-08-22). One crate owns each concern — `xutf` owns all Unicode/ANSI, and importing
a second opinion about graphemes is a defect. The test: does the dep carry weight the
codebase can't carry better?

**Lints are advice, not law — in both directions.** A warning about a real defect gets
the real fix (see boxing, above). A recommendation that produces spam gets deleted:
"can u remove the thing that asks them to use coldpath? they spam it everywhere so id
rather remove this recommendation" (2026-08-10). The reviewer optimizes the codebase,
not the linter's happiness.

## Standards

**Performance is correctness.** "ts is slow, rust isn't" — the rewrite exists to delete
the throttles, GC workarounds, and UTF-16 defenses that compensated for the runtime, not
to port them. 50 ms is a lot. ("im very very anal about performance" — 2026-08-20.)

**Clean cutover, always.** Pre-release means rename+move, migrate every caller, delete
the old path in the same change. No shims, no deprecated aliases, no dual runtimes, no
`PI_*` residue. When replacing a subsystem: "port knowledge and fixtures, not structure"
— the legacy code is a behavioral oracle to extract tests from, then delete.

**Finished means running.** No scaffolds, no "rest is trivial", no speculative design
reports in place of working code. Done = compiles, wired, exercised on a real PTY.
("no dude i want you to fucking finish it all" — 2026-08-08.)

**Craft is visible.** The TUI is the argument that a terminal app can amaze: themed
semantic color, charset-aware icons degrading ASCII→Unicode→Nerdfont, eased animation, a
raytraced welcome scene — and every effect a reusable engine prop, never an example-local
hack. "i really want to impress people with the amount of love that went into this tui"
(2026-08-09). The blog posts hold the same bar: receipts over posture, losing rows in
the table, zero generated-prose tells.

## Method

**Orchestrate, don't type.** Work fans out to 10–20 parallel subagents in phased waves —
one per crate, per extension port, per dependency audit — with the parent owning
verification and the mutation boundaries decided up front. Batching follows real
boundaries: "why not group all slopjson changes into one agent's work… reorganize based
on crate boundaries && dependency of those tasks" (2026-08-20). Sequential
one-agent-at-a-time is a named failure mode.

**Measure, then argue.** The 267 s render, the 86.2 s → 36.6 s tool-roster sweep, the
1.3× forced-call cache bill: findings come from profiles and A/B sweeps of the actual
system, and the accumulated run store — structured, versioned — is the instrument. The
harness that records its own history best is the one that improves fastest.

**Independence as strategy.** Server-side tools, vendor discovery patterns, and
provider-specific escape hatches are lock-in vectors; omp reimplements the capability
behind a neutral interface instead (own web search, own fetch, backends swappable under
an unchanged schema). Multiple concurrent agents, remote execution, and Windows are
design inputs, not ports. The endgame was stated early: "lets assume that we want to
build such a great harness that no one will ever need any plugins at all" (2026-08-10)
— batteries included, and every battery held to the core's standard.

## Lineage

Pi deserves the credit the rewrite implies: it was the first TUI harness comfortable
enough to convert a Claude Code user, and its editor UX, statusline semantics, and
telemetry are matched deliberately. The fork's months of diffs are the evidence base —
every field added to Pi's `ToolDefinition` patched a real symptom, and omp² exists
because the symptoms shared one cause. Port behavior, not shape; match Pi where it is
good, exceed it where the contract was the disease.
