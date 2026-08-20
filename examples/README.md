# Extension examples

Forty-six ports of real pi-ecosystem extensions onto the omp Python extension
layer (`crates/py/python/omp`). Each directory is one extension: an `omp.toml`
manifest, the Python module(s), and a README stating what the pi original did,
how the omp shape differs, and — load-bearing — a **Gaps** section listing every
`omp.*` symbol the port needs that the frozen layer does not export yet.

Symbols the frozen layer exports are used as-is. Symbols specified in
`docs/py/00`–`14` but not yet frozen are imported anyway and marked with a
`# GAP:` comment at the import site; the exercise exists to measure that
distance.

| Example | pi origin | Surface exercised | Reference |
|---|---|---|---|
| `bash-timeout/` | `@mrclrchtr/supi-bash-timeout` | TRANSFORM hook, args mutation | `docs/py/03 §4` |
| `permission-rules/` | `@gotgenes/pi-permission-system` | BashIR rulebook, PRECHECK/APPROVAL, durable tickets | `docs/py/05 §4.1`, `docs/py/06` |
| `bash-guard/` | `@shinynito/pi-menshen` | BashIR REVIEW, classifier ladder, circuit breaker | `docs/py/05 §4.4`, `docs/py/06` |
| `goal-loop/` | `@narumitw/pi-goal` | `agent_settled`, `Continue`, continuation ledger | `docs/py/05 §4.2`, `docs/py/12` |
| `mcp-devices/` | `pi-mcp-adapter` | MCP servers mounted as `dyn` devices | `docs/py/01` |
| `edit-dialect/` | `pi-hashline-edit-pro` | second edit dialect `family@rev`, `lift()` | `docs/py/01`, `docs/py/02 §3` |
| `web-fetch/` | `@mrclrchtr/supi-web` | fetch device, spill → `BlobRef` | `docs/py/02 §1` |
| `code-intel/` | `@mrclrchtr/supi-code-intelligence` | LSP over `omp.env` docs, `place="env"` fan-out | `docs/py/02 §2`, `docs/py/11 §1`, `docs/py/04 §3` |
| `remote-grep/` | `@sreetej510/pi-hpc-tools` | `Site.ATTACHED` worker, bytes never transit host | `docs/py/04 §1`, `docs/py/11 §4` |
| `provider-kimi/` | `@zgltyq/pi-provider-kimi-code` | class (b) `@omp.provider`, OAuth, body mutation | `docs/py/13 §1` |
| `statusline/` | `pi-powerline-footer` | UI statusline segments, coalesced effects | `docs/py/07 §5.1` |
| `questionnaire/` | `pi-ask-user` | dialogs with headless degradation | `docs/py/07 §5.2` |
| `memory-fts/` | `pi-hermes-memory` | env-owned FTS process, stability-banded prompt slots, `omp.state` | `docs/py/08 §3`, `docs/py/09 §4` |
| `todo-journal/` | `@xaccefy/pi-xtodo` | typed entry kinds, journal as the only truth | `docs/py/09 §2` |
| `cache-monitor/` | `@mrclrchtr/supi-cache` | telemetry subscription, `PromptFingerprint` | `docs/py/09 §3`, `docs/py/10 §2` |
| `schedules/` | `pi-schedule-prompt` | durable scheduler, owner principal, missed-run policy | `docs/py/12` |
| `plan-mode/` | `@dreki-gg/pi-plan-mode` | mode gating, PRECHECK denials, `turn_start` patch, byte-identical tool array | `docs/py/05 §4.3` |
| `rewind/` | `@ayulab/pi-rewind` | workspace snapshots, `omp.agents.rewind`, undo-snapshot invariant | `docs/py/12` |
| `subagents/` | `pi-subagents` family (17 pkgs) | `spawn_all` waves, hard `Budget`, steer, `AgentGone`, `subtree_usage` | `docs/py/12` |
| `usage-report/` | `@tmustier/pi-usage-extension` | `omp.sessions` usage query, statusline spend segment | `docs/py/09 §1`, `docs/py/10 §3` |
| `trace-export/` | `@braintrust/pi-extension` | telemetry subscription → declarative export targets | `docs/py/10 §1` |
| `fuzzy-index/` | `@ff-labs/pi-fff` | warm `place="env"` index worker, fuzzy find/grep devices | `docs/py/04 §2`, `docs/py/01` |
| `observational-memory/` | `pi-observational-memory` | OBSERVE ledger, `compaction` hook, `CustomSummary` | `docs/py/08 §2` |
| `ghost-suggestions/` | `@mrclrchtr/supi-prompt-suggestions` | declarative completions + ghost hints | `docs/py/07 §5.4` |
| `model-fallback/` | `pi-model-fallback` | `provider_error` → typed `Failover`, core cooldowns, declared chains | `docs/py/13` |
| `intercom/` | `pi-intercom` | `omp.agents` messaging + `@omp.service` RPC, broker deleted | `docs/py/11 §3`, `docs/py/12` |
| `computer-use/` | `@amaster.ai/pi-computer-use` | 49 tools → one device, `dyn` sub-paths, streaming `Update`, driver worker | `docs/py/00 §2`, `docs/py/01` |
| `sidebar/` | `@esso0428/pi-sidebar` | rail slot, min-size/collapse, keyed patches, user-owned layout | `docs/py/07 §5.3` |
| `prompt-manager/` | `@sreetej510/pi-prompt-manager` | commands + arg ghosts, `Prompt(submit=False)` composer | `docs/py/07 §4.15` |
| `discovery-lmstudio/` | `pi-lmstudio` | `DiscoverySpec`, authoritative-absence merge, zero churn | `docs/py/13 §2` |
| `web-dashboard/` | `@jmfederico/pi-web` | env-owned named process, ReadyProbe, sessions-fed JSON | `docs/py/11 §3` |
| `yolo-seatbelt/` | `@robhowley/pi-yolo-seatbelt` | tier matrix over BashIR path facts, ask → tickets | `docs/py/05`, `docs/py/06` |
| `recall/` | `@joshbochu/pi-recall` | rev-partitioned typed-verdict query, index-not-truth FTS | `docs/py/02`, `docs/py/10` |
| `vector-memory/` | `@galvinsan/pi-mentis` | vector store in `place="worker"`, journal-replay rebuild | `docs/py/04 §4` |
| `onnx-worker/` | `pi-onnx` | native model worker, crash isolation, GIL report-and-degrade | `docs/py/04 §4`, `docs/py/00 q6` |
| `cursor-bridge/` | `@rahularya01/pi-cursor` | class (c) proxy provider, zero Python on the token path | `docs/py/13` |
| `quota-widget/` | `@benvargas/pi-synthetic-provider` + `@ogulcancelik/pi-minimal-footer` | `Operation.USAGE` declaration + themed quota segment | `docs/py/13` |
| `renderers/` | `@heyhuynhgiabuu/pi-pretty` | `(name, rev)` message renderers, no name capture, rev survival | `docs/py/01`, `docs/py/02`, `docs/py/07 §4.13` |
| `hover-cards/` | `pi-cc-extensions` | hover/lift props as intent, overlay detail, no raw escapes | `docs/py/07` |
| `output-budgets/` | `pi-rtk-optimizer` + `pi-slim-tools` + `pi-lean-ctx` | the deleted category: Budget/SpillPolicy/`useless`/DropParts | `docs/py/02 §4` |
| `gateway-litellm/` | `pi-provider-litellm` | gateway declaration, credential-helper deleted, discovery merge | `docs/py/13` |
| `arg-repair/` | `@r3b1s/pi-repair-layer` | `Annotated`/`omp.Field` arg metadata, central finalizer repair | `docs/py/03 §3` |
| `context-trim/` | `@ryan_nookpi/pi-extension-headroom` | `thread_projection` bounded Prune/Replace, pinned respected | `docs/py/08` |
| `workflows/` | `pi-extensible-workflows` | DAG waves over `spawn_all`, journal-resumable, subtree failure | `docs/py/12` |
| `search-provider/` | web.md API-key search cohort | `Operation.SEARCH` declaration + class (b) Python parser | `docs/py/13 q1 ruling` |
| `verdict-details/` | `@eleboucher/pi-memini` | D4 three-channel: terse parts, full details, renderer expand | `docs/py/02` |

pi extension descriptions:
`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`.

## Consolidated gap inventory (2026-08-20)

Aggregated from the sixteen ports' Gaps sections, ranked by how many ports
need the symbol. Everything above the line is blocking for the majority of
real extensions; everything below is namespace-specific.

| Missing from the frozen layer | Ports blocked | Owning doc |
|---|---|---|
| `omp.BashIR` | 2 | `docs/py/06` |

The resolved divergence rulings are reflected in the ports: documents expose
`revision` and `uri`; LSP server values stay opaque; named processes use
`send`; `workers.get` is async; `ui.notify` accepts the documented
`"warning"` spelling; and device revisions remain integers. `WorkerHandle.map`
is explicitly serial until Part 3. `Spill` on unmanaged workers intentionally
raises `BoundaryError`. Remaining gaps are `Doc.dry_run`, `Spill.media_type`,
and their host backing.

Manifest spellings are ratified as `[[telemetry]]` with
`kinds`/`scope`/`queue`/`overflow`, and `schedules:project` for project-scoped
schedules.

## Round 2 residual defects (2026-08-20, ten ports)

The second round ran at a higher bar — the surface was expected complete, so
every finding below is defect-grade, with exact symbol/file/doc citations in
the owning example's README. `usage-report/` came back gap-free.

| Defect | Found by | Frozen file |
|---|---|---|
| `omp.agents` is the big remaining hole: no `Budget`, `spawn`/`spawn_all`, handles/roster (`get`/`list`), `AgentGone`, `AgentKind`, `SubagentSpec.budget`; no `send`/`inbox`/`wait_for`/`peers`/`Message`/`AgentRef`; no `rewind`/`snapshot`/`RestoreScope`/`RewindPending`; `Usage` fields diverge from docs | `subagents/`, `intercom/`, `rewind/` | `agents.py` |
| Compaction surface absent: `CompactionEvent`/`Outcome`/`Tier`/`Verdict`/`CustomSummary`, `omp.context.compact`, `CompactionBusy`; `hooks._DOMAIN_EVENTS` omits `compaction`, so the documented `@omp.hook("compaction")` domain-return form raises | `observational-memory/` | `events.py`, `_context.py`, `hooks.py` |
| Provider error surface absent: `ErrorKind`, `ProviderError`, `Failover`, `ModelFallback`, `Retryability` — and the docs' `provider_error` dataclass itself omits `retryability` (doc defect) | `model-fallback/` | `provider.py` |
| Telemetry export absent: `ProcessTarget`, `OtlpTarget`, `export`; `_instrument_name` emits the literal string `omp.ext.<extension>` (placeholder bug); `Counter.add` is NotWired | `trace-export/` | `telemetry.py:188-262` |
| `omp.BashIR` and `omp.ModelRef` not exported top-level (`ModelRef` is an unresolved postponed annotation in `events.py`); `TurnStartEvent` lacks `thinking` while docs restrict patchable `turn_start` fields to model/route/deadline — needs a ruling | `plan-mode/` | `__init__.py`, `events.py` |
| `omp.device` lacks `replaces=`/`precedence=` and `omp.Precedence`; no `WATCH_RESCANNED` event; `_Workers` lacks documented `restart` | `fuzzy-index/` | `__init__.py`, `placement.py` |
| `omp.Context.settings` absent, so callbacks cannot read configured settings; `omp.completion` not top-level (frozen `ui.completion` suffices — docs divergence) | `ghost-suggestions/` | `_context.py` |
| Docs self-divergence: `Snapshot.generation` monotonic int (12 §Time travel) vs blob-manifest hash (12 §workspace generations, 11) | `rewind/` | — |

**Resolved 2026-08-20:** All five residual defect groups closed across the frozen Python layer and authoritative docs: (1) `omp.agents` full surface, lifecycle handles, messaging, and time travel; (2) compaction and `omp.context` surface with domain hooks; (3) provider error taxonomy, retryability, and failover; (4) telemetry export targets and instrument naming/sinks; and (5) top-level exports (`BashIR`, `ModelRef`), `@omp.device` precedence/replaces, `Context.settings`, environment doc events/workers, and the `Snapshot.generation` / `ProviderError.retryability` documentation fixes.

## Round 3 residual defects (2026-08-20, twenty ports)

Six ports came back gap-free (`yolo-seatbelt/`, `output-budgets/`,
`context-trim/`, `workflows/`, `verdict-details/`, `vector-memory/`). The
rest found these, each with exact citations in the owning example's README:

| Defect | Found by | Frozen file |
|---|---|---|
| Streaming device updates: `Update`/`Done` absent, so the documented `AsyncIterator[Update \| Done]` body cannot emit frames | `computer-use/` | `_verdicts.py:17-142`, `__init__.py` |
| Discovery surface: `DiscoverySpec`/`DiscoveryKind`/`DiscoveryDefaults`/`TrustDomain` absent (`RouteSpec.discovery`/`trust` are `object` placeholders); `DiscoveryQuery`/`Page` not top-level; `ProviderHandle.replace` missing so settings-time URL reconciliation is impossible; docs' `models_discover` omits `phase` while the frozen hook requires it | `discovery-lmstudio/`, `gateway-litellm/`, `cursor-bridge/` | `provider.py:395-406,1045-1071` |
| Credentials facade: no `omp.creds` (`mint_scoped`/`reveal`/`ScopedToken`) | `cursor-bridge/`, `search-provider/` | `__init__.py` |
| env named-process values: `RestartPolicy`/`ReadyLog`/`ReadyTcp`/`ReadyAll` absent (`proc.ensure` forwards opaque `**options`); `Process.send_secret`/`endpoint` absent; wire mismatch — docs `ReadyAll(log, tcp)` vs `env.proto` single-oneof `ReadyProbe` (needs ruling); closure should reuse placement's `Restart` enum | `web-dashboard/`, `cursor-bridge/` | `env.py:863-869,945-997` |
| Telemetry query API: `Predicate`/`Eq`/`Step`/`Query`/`query`/`QueryResult`/`Row` absent (closure must honor the SQL-pushdown ruling, never a Python evaluator); public event payload types (`Envelope`, `SessionStart`, `TurnStart`, `TurnEnd`, `SessionEnd`) absent | `recall/`, `sidebar/` | `telemetry.py:451-464` |
| Command surface: top-level `omp.command` absent; `ui.command` discards `args`/`hint`/`arg_completions` metadata, so usage ghosts and dynamic arg completion cannot be discovered | `prompt-manager/`, `quota-widget/` | `ui/__init__.py:653-656` |
| Arg metadata authoring: top-level `omp.Field` + `Coerce` vocabulary absent; no per-Rev arg-metadata registry introspection | `arg-repair/` | `__init__.py`, `_registry.py:114-133` |
| `provider_usage` classified observation-only while docs give it a `UsageReport \| None` domain return — needs a ruling | `quota-widget/` | `hooks.py:280-290` |
| Transcript element activation (click/Enter on id-bearing focusable boxes) has no dispatch/registration surface; `EventKind.PRESSED` is overlay-only | `hover-cards/` | `ui/__init__.py:220-222,585-655` |
| No extension-visible `Operation.SEARCH` parser-registration seam (symbol unspecified in docs — needs a ruling); `Api.SEARCH_EXA` is a non-wire-compatible placeholder | `search-provider/` | `provider.py:1045-1055` |
| `omp.DuplicateRenderer` documented but frozen raises plain `ValueError` | `renderers/` | `ui/__init__.py:604-605` |
| Constant divergence: `RESULT_SPILL_BYTES` 1 MiB frozen vs 256 KiB documented | `onnx-worker/` | `placement.py:166` |
| `omp.env.http_get` documented by 13 but conflicts with 11's recorded no-HTTP v1 posture — needs a ruling (docs self-conflict) | `discovery-lmstudio/`, `gateway-litellm/` | `env.py:936-997` |

**Resolved 2026-08-20:** All Round 3 defect clusters closed across the frozen
Python layer, wire protocol, and authoritative docs: (1) streaming `Update`/
`Done` frames; (2) the discovery/trust surface (`DiscoverySpec`/`DiscoveryKind`/
`DiscoveryDefaults`/`TrustDomain`/`RouteLimits`, typed `RouteSpec` fields,
`ProviderHandle.replace`/`retract`, top-level exports) with `models_discover`
ruled a phase-free domain hook; (3) the `omp.creds` facade; (4) env
named-process values (`RestartPolicy` over the shared top-level `omp.Restart`,
`ReadyLog`/`ReadyTcp`/`ReadyPing`/`ReadyAll`, typed `proc.ensure`,
`Process.send_secret`/`endpoint`) with `StartProcess.ready` ruled a repeated
`ReadyProbe` — all supplied probes must pass, giving `ReadyAll` an honest wire
form; (5) the telemetry query vocabulary (SQL pushdown, never a Python
evaluator) and public lifecycle payloads; (6) command metadata
(`args`/`hint`/`arg_completions` retained, top-level `omp.command`), transcript
activation (`ui.Activation`/`on_activate`), and typed `DuplicateRenderer`;
(7) `omp.Field`/`Coerce` authoring with per-Rev arg-metadata introspection.
Rulings: `provider_usage` follows the docs' `UsageReport | None` domain return;
the class-(b) search seam is named — `Api.SEARCH_HTTP` plus the provider-scoped
`search_parse` hook; `omp.env.http_get` is frozen as a `NotWired` seam
reconciling 13 §q2 (omp.env owns discovery HTTP) with 11 §q6 (no env-side HTTP
client ships in v1); `RESULT_SPILL_BYTES` follows the documented 262 144.
