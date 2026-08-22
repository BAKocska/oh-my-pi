# Extension examples

One hundred nineteen extensions onto the omp Python extension
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
| `goal-loop/` | `@narumitw/pi-goal` | Session `@omp.campaign` at SETTLE: `Continue`/`Pass`/`Done`, journaled state, tool-driven engage | `docs/py/15 §4`, `docs/py/12` |
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
| `plan-mode/` | `@dreki-gg/pi-plan-mode` | Session regime campaign (`claims`/`binds`, ADMISSION `Deny`) + bounded decision-gate campaign | `docs/py/15 §7`, `docs/py/15 §6` |
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
| `unified-exec/` | `pi-unified-exec` | env-owned PTY sessions, cursor polling, stdin/keys | `docs/py/11` |
| `profiles/` | `@danypops/pi-packed` | atomic profile switch: availability deltas + turn patch, byte-identical tool array | `docs/py/01`, `docs/py/05` |
| `canary/` | `pi-canary` | STABLE-band slot, KV-safe awareness checks, ext counters | `docs/py/08`, `docs/py/10` |
| `session-titler/` | `@agnishc/edb-auto-name-session` | smol-role completion, SetTitle, once-per-session latch | `docs/py/12`, `docs/py/08` |
| `multi-account/` | `pi-multi-account` | account rotation via `Failover`, core cooldowns, `omp.creds` | `docs/py/13` |
| `fireplace/` | `@jpodivin/pi-fireplace` | declarative animation props, charset degradation, zero timers | `docs/py/07` |
| `chat-bridge/` | `pi-lark-notify` | webhook out, schedule-polled replies, `Inject` dedup | `docs/py/11`, `docs/py/12` |
| `project-model-pin/` | `pi-set-model` | PROJECT state pin, `turn_start` patch, override latch | `docs/py/09`, `docs/py/05` |
| `model-alias/` | `@zigai/pi-model-alias` | catalog overlays via `extends=`, declaration-time conflict | `docs/py/13` |
| `secret-guard/` | `@josephyoung/pi-heimdall` | `omp.secrets` rules + BashIR path denials, core-side masking | `docs/py/06` |
| `tool-toggle/` | `pi-tbox` | device catalog overlay, availability deltas, slot-vs-device cost | `docs/py/01`, `docs/py/08` |
| `session-browser/` | `@vanillagreen/pi-session-manager` | sessions overlay, approval-gated deletion | `docs/py/09`, `docs/py/07` |
| `worktree-guard/` | `avtc-pi-parallel-work-guardrail` | git-op classification, ticket-borne timeout/default | `docs/py/06` |
| `fresh-loop/` | `@cjvnjde/pi-fresh-loop` | fresh-session iteration, stop classifier, journal resume | `docs/py/12` |
| `image-gen/` | `@amaster.ai/pi-image-gen` | image operation route, media BlobPart, `omp.Field` args | `docs/py/13`, `docs/py/02` |
| `settings-sync/` | `@narumitw/pi-sync` | USER CAS sync, `AfterIdle` push, conflict journaling | `docs/py/09`, `docs/py/12` |
| `copy-cut/` | `@shelken/copy-cut` | shortcut chord, clipboard, composer edit | `docs/py/07 §4.14` |
| `remote-approve/` | `@agentapprove/pi` | `@omp.approver`, idempotent re-offer, fail-closed unreachable | `docs/py/06 §Approvals` |
| `provider-vertex/` | `@twogiants/pi-anthropic-vertex` | region-scoped routes, scoped-mint GCP auth | `docs/py/13` |
| `side-chat/` | `pi-btw` | background side-thread agents, overlay transcript, real journals | `docs/py/12`, `docs/py/07` |
| `fzf-actions/` | `pi-fzf` | config-driven pickers: env exec candidates, overlay select, quote-safe actions | `docs/py/07`, `docs/py/11` |
| `esc-steer/` | `pi-esc-steer` | shortcut abort + queued-steer injection, no invented interrupts | `docs/py/03` |
| `task-queue/` | `pi-true-queue` | hidden work queue drained by a SETTLE campaign (`Ladder`, journaled hand-out state) | `docs/py/15 §4`, `docs/py/09` |
| `git-changes/` | `@joyanhui/pi-ext-git-changes` | first `<diff>` markup, porcelain -z parsing, zero polling | `docs/py/07` |
| `pulse/` | `pi-pulse` | TPS/TTFT from event fields, EMA footer, DropStats | `docs/py/10` |
| `vision-describe/` | `@smoose/pi-vision` | vision-role completion with image parts, digest cache | `docs/py/12`, `docs/py/02` |
| `legible/` | `@nklisch/pi-legible` | assistant-message renderer, rewrite as presentation not mutation | `docs/py/07 §4.13` |
| `study-commits/` | `@anthnykr/pi-study-commits` | overlay multi-select, single bounded injection, spill fallback | `docs/py/07`, `docs/py/12` |
| `auto-thinking/` | `@narumitw/pi-auto-thinking` | thinking patch (ruling landed), classifier ladder + heuristic fallback | `docs/py/05 §3.3`, `docs/py/12` |
| `llama-switch/` | `pi-llama-switch` | named-process argv switch, generation fencing, availability flips | `docs/py/11`, `docs/py/13` |
| `tool-search/` | `pi-tool-search` | `dyn` subsumes discovery; allowlisted availability promotion | `docs/py/01` |
| `github-tools/` | `@amitkot/pi-safe-github` | one zero-slot device, typed sub-paths, creds + approval-gated writes | `docs/py/01`, `docs/py/11` |
| `shell-hooks/` | `@hsingjui/pi-hooks` | config-driven event→shell hooks, OBSERVE default, opt-in gating | `docs/py/05` |
| `script-tools/` | `@isr4el-silv4/pi-script-tools` | dynamic mount from workspace scan, header-parsed args, rescan refresh | `docs/py/01`, `docs/py/11` |
| `provider-pack/` | `pi-moonshot` + `pi-zai-glm` + `pi-provider-alibaba` + OVH | four class (a) data providers, zero hooks | `docs/py/13` |
| `feature-bundle/` | `pi-toolbox` / `@bdsqqq/pi` umbrellas | first `[features]`: 33 entrypoints → one extension, three features | `docs/py/14 §3.1.3` |
| `dev-inspector/` | `pi-dev-inspector` | prompt-slot breakdown, request telemetry, capture-grant redaction | `docs/py/10 §4`, `docs/py/08` |
| `grep-heatmap/` | `pi-fovea` | tool_result rewrite prohibited → renderer augmentation + query device | `docs/py/02`, `docs/py/05 §3.11` |
| `green-loop/` | `pi-green-loop` | affected-test loop: OBSERVE fold, `AfterIdle`, failure dedup | `docs/py/12`, `docs/py/11` |
| `speech-providers/` | `@p8n.ai/pi-listens` | `Operation.SPEAK`/`TRANSCRIBE`, audio BlobParts, class (b) parsers | `docs/py/13` |
| `task-graph/` | `@danypops/pi-papyrus` | `Device.subtool` children, graph index, STABLE rule slot | `docs/py/09`, `docs/py/08` |
| `prompt-templates/` | `pi-prompt-template-model` | markdown templates as content, non-recursive substitution, chain spawns | `docs/py/07 §4.15` |
| `tool-compose/` | `pi-fabric` | composed steps stay individually gated (no chokepoint bypass) | `docs/py/01`, `docs/py/06` |
| `browser-cdp/` | `@narumitw/pi-chrome-devtools` | one device + subtools, env-owned browser, streaming, availability-follows-process | `docs/py/11`, `docs/py/01` |
| `safe-fetch/` | `@juicesharp/rpiv-web-tools` | resolve-then-validate fetch, per-hop redirect revalidation | `docs/py/11` |
| `knowledge-index/` | `@galvinsan/pi-mentis-knowledge` | detached ingest + TurnBoundary settlement, hybrid index rebuild | `docs/py/03`, `docs/py/12` |
| `context-report/` | `@mrclrchtr/supi-context` | entry-rendered report invisible to the model, `ContextUsage` fields | `docs/py/09`, `docs/py/08` |
| `preemptive-compact/` | `pi-preemptive-compact` | pressure trigger + hysteresis, `context.compact`, defer summary | `docs/py/08` |
| `rules-sync/` | `@7n/rules` | content skills + conformity device + ADR decision capture | `docs/py/14 §3.2.3` |
| `turn-phases/` | `@yusukeshib/pi-working-status` | event-catalog breadth → one coalesced working status | `docs/py/05 §2.3-2.4` |
| `calm-mode/` | `pi-calm` | hiding as renderer concern, `RenderCtx.collapsed`, zero context mutation | `docs/py/07 §6.4` |
| `skill-palette/` | `pi-skill-palette` | `[[skills]]` content rows, palette overlay, `skill://` invocation | `docs/py/14 §3.1.5` |
| `spec-flow/` | `@mrclrchtr/supi-flow` | journal-held phase machine, artifact-gated transitions, archive spill | `docs/py/09`, `docs/py/11` |
| `handoff/` | `@noice-tech/pi-cutover` | bounded brief → clean child session, parent→child link journaled | `docs/py/12`, `docs/py/08` |
| `foreign-commands/` | `pi-unify-cmd` | foreign-root content import, `W-FOREIGN-ROOT`, containment guards | `docs/py/14 §6.6` |
| `realtime-session/` | `pi-realtime` | `Operation.REALTIME`, streamed partials, cancel-mid-stream | `docs/py/13` |
| `native-grounding/` | `@pokutuna/pi-google-genai` | provider-native tools as model caps, not proxy devices | `docs/py/13` |
| `welcome-chrome/` | `@zeerke/ascet-copilot-ui` | welcome scene, recent-session rows, title set/restore, zero timers | `docs/py/07` |
| `segment-bus/` | `@juanibiapina/pi-powerbar` | `@omp.service` producer/owner bus with per-publisher quotas | `docs/py/00 §Extension services` |
| `pr-review/` | `pi-pr-review` | tiered reviewer waves, schema-validated findings, sanitized rail | `docs/py/12` |
| `patch-dialect/` | `mitsupi` (edit-surface cohort) | third edit family `patch@1`, 3-family `lift()` chain, 02 q3 evidence | `docs/py/02 §3` |
| `resource-receipts/` | `@narumitw/pi-usage` + `@sreetej510/pi-usage` (derived) | `omp.resources()` receipt, `QuotaExceeded` degradation, self-accounting deleted | `docs/py/00`, `docs/py/04` |
| `trust-gates/` | `pi-sandbox` | `omp.Trust` tier branching, degrade-never-fail | `docs/py/00`, `docs/py/06` |
| `grant-widening/` | `pi-sandbox` (policy-safety cohort) | `Amend(approval=)` session-scope widening, external approver decision | `docs/py/06 §Approvals` |
| `org-registry/` | `@7n/rules` (derived) | `StateScope.ORGANIZATION` registry, `StateScopeDenied` fallback overlay | `docs/py/09 §omp.state` |
| `part-pruner/` | `pai-acp` (context-management cohort) | `AmendPatch::DropParts` lossless prune under headroom pressure | `docs/py/02 §4`, `docs/py/08` |


pi extension descriptions:
`.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md`.

## Consolidated gap inventory (2026-08-20)

Aggregated from the sixteen ports' Gaps sections, ranked by how many ports
need the symbol. Everything above the line is blocking for the majority of
real extensions; everything below is namespace-specific.

| Missing from the frozen layer | Ports blocked | Owning doc |
|---|---|---|
| *(none remaining as of 2026-08-20)* | — | — |

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

## Round 4 residual defects (2026-08-20, twenty ports)

Three ports gap-free (`session-titler/`, `fireplace/`, `canary/`). A new
finding class appeared this round: **frozen-but-NotWired stubs** — symbols
that exist and typecheck but unconditionally raise, which the import-level
gate cannot catch. Exact citations in each example's README.

| Defect | Found by | Frozen file |
|---|---|---|
| Sessions mutation verbs absent entirely: no resume/switch, no rename/title-update, no delete (approval emits correctly, nothing can execute it); `sessions.get`/`lineage`, `SessionNotFound`/`SessionLink` missing; `Cost` vs unexported `UsageCost`; `SessionKind` in `__all__` but never imported (import bug) — docs also lack a mutation section: needs design + ruling | `session-browser/` | `sessions.py:168-200` |
| HTTP verbs: no `http_post`/`http_put`; `http_get` is a NotWired stub — and docs/py/11's no-HTTP posture vs 13's usage still needs the ruling | `chat-bridge/`, `settings-sync/`, `remote-approve/` | `env.py:887-913` |
| NotWired stubs behind real signatures: `omp.agents.schedule`, `policy.pending()`; `Inject` has no prompt payload, `Every` no callback delivery, `BeforeAgentStartEvent` no `schedule_id` | `chat-bridge/`, `settings-sync/`, `remote-approve/` | `agents.py:818-950`, `policy.py:736-752` |
| Approver surface: `@omp.approver` absent; no decide/answer verb (only `pending()`) | `remote-approve/` | `policy.py` |
| `omp.secrets` namespace entirely absent (`declare`/`mask`/`SecretRule`/`SecretKind`/`SecretMode`) | `secret-guard/` | no `secrets.py` |
| UI: `set_clipboard` absent (from docs too — doc gap); top-level `shortcut` absent; `ui.shortcut` neither validates nor registers its declaration; `OverlayHandle.events` yields only synthetic SUBMIT/CANCEL, not watched interactions | `copy-cut/`, `side-chat/` | `ui/__init__.py:479-481,704-707` |
| Provider/catalog: `ModelPatch`/`ModelOverlay`/`ScopedAlias` absent; `Failover.rotate_account` cannot target a successor identity; `provider_refresh` gateable vs docs' no-phase Credential callback; `CredentialSource.application_default` absent; `ImageCaps`/`ImageFormat`/`Dimensions` absent (`ModelSpec.image: object`); no `ProviderHandle.request` seam for GENERATE_IMAGE (same needs-ruling class as the SEARCH seam) | `model-alias/`, `multi-account/`, `provider-vertex/`, `image-gen/` | `provider.py` |
| Devices catalog: `omp.devices.list` absent; `DeviceInfo` lacks `provenance`/`slotted`/`schema_bytes`/`schema_tokens` | `tool-toggle/` | `devices.py:189-209,356-394` |
| env: `Pty` value type absent (`proc.ensure` takes `pty: object`); `PathMeta`/`FileKind` absent (`lstat` returns `Any`); `Capability.WORKTREE` declared but no topology query | `unified-exec/`, `worktree-guard/` | `env.py:600-602,1037-1047` |
| `omp.journal.latest`/`fold` helpers absent | `fresh-loop/` | `journal.py:94-119` |
| `Context` exposes no current `RouteRef` and no thinking selection — third round in a row the turn_start `thinking` patchability ruling surfaces (`plan-mode`, `profiles`, `project-model-pin`) | `project-model-pin/`, `profiles/` | `_context.py:32-54`, `events.py:399-414` |
| Async `omp.urls.read` absent (typed `HistoryUrl.read` works via bindings) | `side-chat/` | `urls.py:334-348` |
**Resolved 2026-08-20:** All Round 4 clusters closed across the frozen Python
layer, wire protocol, envd, and authoritative docs: (1) the sessions mutation
design landed — `get`/`lineage`/`resume`/`rename`/`delete` (delete
approval-gated), `SessionLink`/`SessionNotFound`, module-local
`omp.sessions.Cost`, the `SessionKind` import bug fixed and the underlying
top-level collision resolved by renaming the sandbox enum to
`SandboxSessionKind`; (2) env-brokered HTTP ships — 11 §q6's "ship nothing"
posture is superseded by a dated ruling: `HttpRequest`/`HttpResponse` egress
frames, an `env.net`-gated envd client (wreq, per-request timeout, 256 KiB
cap), and `http_get`/`http_post`/`http_put` verbs; (3) schedule values
completed (`Inject` prompt payload, `BeforeAgentStartEvent.schedule_id`;
`Every` stays the documented `(interval, jitter, align)`); (4) `@omp.approver`
+ `omp.policy.decide`; (5) the `omp.secrets` namespace with core-side masking;
(6) UI: `ui.set_clipboard` (doc gap fixed), validated + registered shortcuts
with top-level `omp.shortcut`, host-fed `OverlayHandle.events` watched
interactions; (7) provider catalog: `ModelPatch`/`ModelOverlay`/`ScopedAlias`,
`Failover.rotate_account` successor targeting,
`CredentialSource.application_default`, typed image caps and the
`ProviderHandle.request` seam (ruled like Round 3's SEARCH seam),
`provider_refresh` ruled a phase-free domain hook; (8) `omp.devices.list` +
`DeviceInfo` metadata, env `Pty`/`PathMeta`/`FileKind`/worktree topology,
`journal.latest`/`fold`, async `omp.urls.read`; (9) the three-round
`turn_start.thinking` ruling — patchable fields widen to
model/route/deadline/thinking and `Context` exposes the current route and
thinking selection; (10) the examples gate gained the requested stub-scan:
frozen-but-NotWired symbols are reported distinctly from missing ones, and
the gate now runs 66/66 green after stale `# GAP:` markers and resolved Gaps
claims were reconciled (ports whose declarations the docs proved illegal —
non-TRANSFORM `order`, observation `on_failure`, `@omp.tool` `family=`,
missing loopback trust — were corrected to the documented contract).


## Round 5 residual defects (2026-08-20, twenty ports)

Four ports gap-free (`esc-steer/`, `fzf-actions/`, `script-tools/`,
`study-commits/`). The character changed this round: no missing namespaces —
the findings are micro-divergences and, notably, **docs defects**, including
the first inverse divergences (frozen ahead of docs). The
`turn_start.thinking` ruling landed; `auto-thinking/` closes that saga.

| Defect | Found by | Where |
|---|---|---|
| Docs defects: 05 §4.2's worked example is stale (`Settle(reason=)`, `Continue(prompt=Item.user_note)`, awaited sync `journal.append`, absent `journal.state`); 05:1787 phased `compaction` vs domain-only frozen+08; 07 calls `<diff>` unavailable/proposed while the TUI ships it; 00 `ctx.session: str` vs 07 §5.1 `ctx.session.stats`; 10:796 `tokens` vs frozen `usage`; 04:899 `Spill.buf` vs frozen `value` | `task-queue/`, `shell-hooks/`, `git-changes/`, `feature-bundle/`, `pulse/`, `vision-describe/` | docs |
| Inverse divergences (frozen ahead of docs): `Inject.prompt` required but undocumented; `BeforeAgentStartEvent.schedule_id` frozen but absent from 05's payload table | `green-loop/` | docs |
| Needs ruling: paid classifiers from turn-level TRANSFORM — 12's REVIEW-only completion legality vs 05's Modify-is-TRANSFORM-only leaves no legal phase for a single turn_start classifier hook | `auto-thinking/` | `docs/py/05`, `docs/py/12` |
| No extension-visible merged-catalog read (`models()`/`ModelCard`/WatchModels documented, no frozen Python reader; `ProviderHandle.models()` is declaration-scoped) | `auto-thinking/` | `provider.py:980-1060` |
| Telemetry: `ModelRequest` lacks `latency_ms`/`ttft_ms`/`degraded`/content fields; `coalesce_key` validated then discarded (registry has no field); `PromptFingerprint.slots` lack byte sizes/bands | `pulse/`, `dev-inspector/` | `telemetry.py:108-183`, `_registry.py:184-195` |
| Provider: `SpecError` missing; duplicate `ModelSpec.id` not rejected in `__post_init__`; bare class-(a) `provider(spec)` call doesn't register; `PromptCacheCaps`/`CacheRetention` naming divergence; `SpeechCaps`/`AudioFormat`/`TranscriptionCaps` absent (`ModelSpec` fields `object`); `ProviderHandle.request` image-only despite SPEAK/TRANSCRIBE shared-machinery docs; `completion` has no image-part input contract | `provider-pack/`, `speech-providers/`, `vision-describe/` | `provider.py`, `agents.py:155-167` |
| Devices: `Device.subtool` returns `ToolPath` vs documented child-device decorator (blocks publishing `dyn invoke/github/pr/list` addresses); `devices.list` async vs docs sync; `HARD_SLOT_BUDGET` absent | `github-tools/`, `tool-search/` | `devices.py:290-296,368-415` |
| UI: `MessageView` missing (renderer takes `object`); message-renderer purity contradiction for pending→cached rewrites (no sanctioned presentation-cache/invalidation); no renderer decoration/augmentation mode though 01 sanctions it; `<diff>` `context` prop unread in TUI props | `legible/`, `grep-heatmap/`, `git-changes/` | `ui/__init__.py:666-720`, `crates/tui/src/props.rs` |
| env: `Process.restart()` absent (stop+ensure works); `Process` ops dispatch by name without generation fencing despite the contract; `Run.stdin` vs frozen `write`/`eof` | `llama-switch/`, `shell-hooks/` | `env.py:742-748,1033-1077` |
**Resolved 2026-08-20:** All Round 5 clusters closed. Docs moved to frozen
truth at every named defect site (05 §4.2 worked example, domain-only
compaction, shipped `<diff context=N>`, `sessions.current()` recipes,
`ModelRequest.usage`, `Spill.value`, required `Inject.prompt`,
`BeforeAgentStartEvent.schedule_id`). Rulings: `completion()` is legal from
the turn-scoped `turn_start` TRANSFORM (in addition to REVIEW) — per-call
TRANSFORM stays illegal, closing the auto-thinking saga; `devices.list` is
synchronous per docs over the declaration snapshot merged with the
host-installed view; renderer purity is per verdict state with a host-owned
presentation cache keyed `(identity, call_id, state)` plus the sanctioned
`decorates=True` composition mode. Frozen closures: the merged-catalog read
(`ModelCard`/`Price`/`models()`/`watch_models`); telemetry
`latency_ms`/`ttft_ms`/typed degradations/capture-gated content bytes,
registered `coalesce_key`, and typed `PromptSlotFingerprint(digest,
size_bytes, band)`; provider `SpecError`, duplicate-model rejection, bare
class-(a) registration, docs-exact `CacheRetention`
(`REQUEST`/`SESSION`/`SHORT`/`LONG`) and `PromptCacheCaps` field names, typed
speech/transcription caps, the shared `ProviderHandle.request` SPEAK/
TRANSCRIBE arm, and `TextPart`/`BlobPart` completion prompts; devices
child-decorator `subtool`, `HARD_SLOT_BUDGET`; UI `MessageView` and the wired
`<diff>` `context` prop; env `Process.restart()`, generation-fenced Process
frames (`PreconditionFailed` on stale handles), and the documented
`Run.stdin`/`Run.eof` spelling. The examples gate now discovers package- and
multi-module-shaped ports; ports using pre-rename spellings
(`CacheRetention.STANDARD`/`EPHEMERAL`, `maximum_breakpoints`,
`minimum_prefix_tokens`) were migrated to the documented contract.


## Round 6 residual defects (2026-08-20, twenty ports)

Five ports gap-free (`preemptive-compact/`, `welcome-chrome/`,
`prompt-templates/`, `turn-phases/`, `browser-cdp/`). Two catalog rows were
deliberately not ported (offensive-security casefile/audit packages); their
distinctive mechanics are covered by `spec-flow/` and `pr-review/`.

| Defect | Found by | Where |
|---|---|---|
| **Most-hit:** `Device.subtool` accepts only `name` and hard-codes child `schema=None`, so documented per-child schemas and inherited-property overrides are unavailable; 01 also self-contradicts (documents `subtool(name: str)` at :911 then claims overrides at :927) | `task-graph/`, `spec-flow/`, `pr-review/` (+ `github-tools/` in round 5) | `devices.py:362-423`, `docs/py/01:911-929` |
| **Content declarations have no row spelling:** 14 §3.1.5's kind vocabulary covers only lazy-reachable CODE surfaces; §3.2.3's `kind="skills"` is a *package* kind forbidding code — so a code-bearing extension shipping skills/rules/context-files/prompts has no manifest shape, though PLAN Part 8 §Content discovery M1 puts content discovery on that very table. Also `packages.Distribution` has no `declarations`, so an extension cannot enumerate its own declared content | `skill-palette/`, `rules-sync/` | `docs/py/14 §3.1.5,§3.2.3`, `packages.py:108-121` |
| Detached tool outcomes absent: no `omp.Detached`/`JobRef`, no JobBoard registration op for an env-placed device coroutine | `knowledge-index/` | `docs/py/03:153-179` |
| Dynamic COMMAND registration impossible: `command` is import-time only and `RegisterUi` needs predeclared manifest rows, so activate-time discovered commands cannot reach the host (devices got `dynamic_mount`; commands have no equivalent) | `foreign-commands/` | `ui/__init__.py:820-838` |
| **Needs ruling:** no public extension→device invoke API, and 11:242/04:221 prohibit worker re-entrant device invocation while 01:201-213 describes individually gated inner calls — composition has no legal path | `tool-compose/` | `docs/py/01` vs `docs/py/04`/`11` |
| Provider caps holes: `HostedTool` missing (`ChatCaps.hosted_tools` is `frozenset[str]`), no `URL_CONTEXT`/`DEEP_RESEARCH` features; `RealtimeCaps`/`RealtimeFeature` missing and `ProviderHandle.request` rejects `REALTIME` (`Operation.REALTIME`/`Api.OPENAI_REALTIME`/`Transport.WEBRTC` DO exist) | `native-grounding/`, `realtime-session/` | `provider.py:161-169,651-670` |
| `http_get` has no no-follow/one-hop option and `HttpResponse` no final URL, so per-hop redirect revalidation cannot go through the broker | `safe-fetch/` | `env.py:925-983` |
| Public verdict arms incomplete: `CallOutcome`/`ArgsRejected`/`Aborted` unexported (only `Ok`/`Faulted`); `JournalEntry.artifact` is `object \| None` with no `ArtifactRef`/`omp.artifacts` | `handoff/` | `_verdicts.py:54-73`, `journal.py:45-59` |
| `ContextUsage.catalog_notice_tokens` missing though the accounting split was ruled; 08:2091 still calls it unresolved | `context-report/` | `context.py:91-106` |
| Sync folds cannot read async SESSION state — no immutable presentation field on `RenderCtx`/`MessageView`/`View` | `calm-mode/` | `ui/__init__.py` |
| Docs drift: 00:788 `omp.context()` vs frozen `Context.current()`; 09 uses `omp.ExtensionActivate` vs frozen `ExtensionActivateEvent`; 14 §6.6 lacks the normative content-only-never-code rule; `agent_settled` phase classification reads OBSERVE-able in 05 but is domain-only frozen | `segment-bus/`, `task-graph/`, `foreign-commands/`, `rules-sync/` | docs |
**Resolved 2026-08-20:** All Round 6 clusters closed. The most-hit defect
lands as a design ruling — **Device is a router**: `@device.subtool(path,
**overrides)` is a route decorator (multi-segment paths, child schema from
the handler's `Annotated`/`omp.Field` signature, inherited-property
overrides), and `omp.router(prefix)` is a standalone mountable sub-router
composed via `Device.mount`; docs/py/01's :911/:927 self-contradiction
resolves into that contract. Content declarations gained their row spelling —
docs/py/14 §3.1.5 grows `skills`/`rules`/`context-files`/`prompts` content
rows (distinct from the code-forbidding package kind, §6.6 now states the
normative content-only-never-code rule) and `packages.Distribution` gained a
typed `declarations` enumeration. The composition ruling: gated
`omp.devices.invoke(path, args, *, deadline)` is the public host-placement
surface (each inner call independently admission-gated, per 01:201-213); the
04/11 worker re-entrancy prohibition stands and cross-references it. Also
closed: `Detached`/`JobRef` + the `omp.jobs.register` arm; activate-time
`ui.dynamic_mount` for commands with full metadata; typed `HostedTool`
(incl. `URL_CONTEXT`/`DEEP_RESEARCH`) and `RealtimeCaps`/`RealtimeFeature`
with an establishment-only REALTIME request arm (media never transits
Python); bounded `redirects=0..10` + `HttpResponse.final_url` through the
broker with cross-origin credential stripping; the complete public
`CallOutcome` union (`ArgsRejected`/`Aborted` exported) with `ArtifactRef` +
`omp.artifacts` and a typed `JournalEntry.artifact`;
`ContextUsage.catalog_notice_tokens` per the accounting ruling; immutable
host-materialized `presentation` snapshots on the render-input values; and
the four docs drifts moved to frozen truth.


## Round 7 targeted ports (2026-08-20, six ports)

A narrow round aimed at the surfaces every prior selection passed over, plus
an exhaustive docs-drift sweep. Four ports came back gap-free
(`patch-dialect/` — settling `docs/py/02` q3 with three-family evidence:
pairwise destination lift steps suffice, no hub family emerged —
`trust-gates/`, `grant-widening/`, and the docs sweep found and fixed
thirteen residual drift sites the round-5/6 spot fixes missed, notably every
`omp.journal.state(...)` worked example rewritten onto the frozen typed
`omp.state` surface).

| Defect | Found by | Frozen file |
|---|---|---|
| `StateScopeDenied` derives from `OmpError` while docs specify `omp.JournalError` as its base (and no `JournalError` is exported) | `org-registry/` | `__init__.py:171-172` |
| `AmendPatch::DropParts` declared by 02 §4 but the frozen `ContextPatch` vocabulary has no arm — the policy is declarable, the amend op is not | `part-pruner/` | `context.py` |
| `QuotaExceeded` carries neither documented `.quota` nor `.receipt` payload fields (native `create_exception!` supplies no payload) | `resource-receipts/` | `_errors.py` / native |
**Resolved 2026-08-20:** All three Round 7 findings closed. `JournalError`
is frozen per docs/py/09 (with the documented `appended` partial-append
payload) and `StateScopeDenied` rebases on it; the `DropParts(ids, reason)`
amend arm joined the `ContextPatch` vocabulary with projection-only semantics
and top-level export; `QuotaExceeded` became a payload-carrying Python
exception (`quota: str`, `receipt: ResourceReceipt | None`), replacing the
fieldless native `create_exception!` (no native raiser existed). A docs
self-conflict surfaced during closure — 09:512-520 places `SchemaError`
under `JournalError` while 01:1179-1188 places it under `DeviceError` — and
resolved by multiple inheritance: schema failures genuinely arise on both the
device-decode and journal-replay paths, so `SchemaError(DeviceError,
JournalError)` keeps both documented contracts true. The three finding ports
were reconciled to the landed surface and are gap-free.


**Parallel-round reconciliation (2026-08-20).** Round 7 was run twice
concurrently against the same six target surfaces, under two naming schemes.
The duplicate set (`resource-budget/`, `trust-tiers/`, `grant-widen/`,
`org-notes/`, `retro-trim/`) was removed in favour of the ports above, which
did the discovery; `patch-dialect/` is shared and retains the three-family
edge-count evidence. Two results survive from the removed set because they
refine the findings rather than restate them:

- **`ResourceReceipt` replaces self-accounting, not admission policy.** The
  duplicate concluded the receipt could *not* subsume the two earlier
  hand-rolled quota sites; the surviving port claimed it deleted both.
  Reconciled in `resource-receipts/README.md`: `segment-bus/` dropped its
  custom `PublisherQuotaExceeded` for `omp.QuotaExceeded` (the mechanism the
  receipt genuinely deletes) but keeps its per-publisher maxima, which
  apportion the slot it owns among sibling callers — something a
  per-extension receipt cannot describe. `tool-toggle/` needed no change: its
  schema costs already come from frozen `DeviceInfo` rows.
- **Concurrent rounds hide gaps.** Five of the six duplicates came back
  gap-free precisely because the surviving ports' three findings
  (`JournalError` base, `DropParts` arm, `QuotaExceeded` payload) were closed
  while the duplicates were still running. A gap-free result is only evidence
  when the layer held still underneath it.

## Round 8 — adversarial audit (2026-08-20, 4 audits + 7 probes)

Method change: seven rounds of feature-porting had converged, so this round
attacked the surface instead of sampling it — mechanical bidirectional
doc↔code diffs, boundary/limit probes, and multi-extension conflict tests.
Seven conformance probes landed under `examples/` (`limits-probe/`,
`malformed-probe/`, `fencing-probe/`, `cancel-probe/`, `precedence-conflict/`,
`patch-conflict/`, `determinism-probe/` — no pi origin; each drives a contract
to its boundary and asserts the documented refusal) plus one re-runnable gate,
`scripts/check-docs-surface.py`. Highest-yield round of the eight.

**Closed (2026-08-20 remediation).** Everything below is the audit record, not
live state: all 15 code defects are fixed, the 257 drift entries are closed
(`check-docs-surface.py` now reports 0), the 8 docs self-contradictions and
underspecified contracts are ruled and recorded in the owning docs'
Open-questions sections, and the six enum divergences are resolved. Each
probe's README carries the re-observed matrix and per-finding closure records;
the one known remaining gap is agent-side (`crates/agent/src/context.rs`
`apply_patches` still short-circuits per-sequence instead of the ruled per-op
drop), recorded in `patch-conflict/README.md`.

### Live code defects (not drift)

| # | Defect | Found by | Where |
|---|---|---|---|
| 1 | **Two distinct `PlacementError` classes** — `WorkerUnavailable`/`ShipError`/`BoundaryError` derive from a private `placement.PlacementError(RuntimeError)`, so `except omp.PlacementError` (the native `OmpError` subclass) does **not** catch them | error audit | `placement.py:25-32`, `__init__.py:23-58,426-443` |
| 2 | **`except omp.OmpError` does not catch everything** despite 00:952 — hook, UI, url, package, prompt, telemetry, and private-placement errors all sit outside the umbrella | error audit | 7 modules |
| 3 | **Cancellation grace ladder collapsed** — `task.cancel()` and `_interrupt(thread_id)` fire back-to-back, so D5's `CANCEL_GRACE` unwind window never exists; `crates/app/src/exthost/cancel.rs:67-83`'s staged delays are dead code | `cancel-probe/` | `_host.py:111-115` |
| 4 | **`ctx.on_cancel` callbacks never fire** — stored only; `Scope.cancelled` is never set on `CancelDispatch` | `cancel-probe/` | `_context.py:111-134`, `_host.py:108-116` |
| 5 | **TML byte/depth ceilings unenforced** — `_validate` never reads `TML_MAX_BYTES`/`TML_MAX_DEPTH`; depth 65 constructs fine, contra the reject-before-allocate rule | `limits-probe/`, `malformed-probe/` | `ui/__init__.py:102-152` |
| 6 | **`MAX_FRAME_BYTES` 16× divergence** — documented 67 108 864, privately enforced 4 194 304 | `limits-probe/` | `_host.py:20,76-83` |
| 7 | **`MAX_DECLARATIONS` bypassable** — completion/message_renderer/verdict_renderer write UI-local dicts, skipping `_insert`, so 256 can be exceeded without `DeclarationLimit` | declaration audit | `ui/__init__.py:758-834` |
| 8 | **Metric instruments unbounded** — 1 025 counters created without refusal; the quotas are promised but numerically unspecified | `limits-probe/` | `telemetry.py:240-330` |
| 9 | **`WorkerHandle.call` drops its generation** and wraps stale-generation as `WorkerUnavailable` instead of `StaleGeneration` | `fencing-probe/` | `placement.py:134-150,213-218` |
| 10 | **Manifest drift never checked at FREEZE** — `_manifest_tools`/`_hooks`/`_services` are written and never read; `DeclarationDrift` is dead code | declaration audit | `_registry.py:372-375,780-841` |
| 11 | **Executable declaration rows cannot be ingested** — `configure_manifest(declarations=)` decodes every row as `ContentDeclaration` (4 content kinds), so command/renderer/completion rows in real manifests never reach the registry | declaration audit | `_registry.py:337-375` |
| 12 | **Reserved parameter names survive** — `do_` and trailing-underscore params pass schema derivation instead of raising activation-time `SchemaError` | `malformed-probe/` | `_registry.py:1071-1119` |
| 13 | **`omp.ui.__all__` leaks imports** — `Any`, `Callable`, `ContextVar`, `MappingProxyType`, `dataclass`, `field`, `annotations` are public exports | symbol audit | `ui/__init__.py` |
| 14 | **No activation-trigger class is ever set** — no Definition record or snapshot carries `trigger`; static/lazy/eager-prompt/eager-ui are all absent in practice. Three kinds are also recorded but absent from `DeclarationSnapshot` | declaration audit | `_registry.py:83-255` |
| 15 | **`TmlError.at` is a code-point offset, not the documented byte offset** | `malformed-probe/` | `ui/__init__.py:102-130` |

### Surface drift, measured exhaustively

`scripts/check-docs-surface.py` inventories 1 590 documented spellings against
1 372 frozen exports and reports **257 drift entries** (157 documented-but-absent,
100 frozen-but-undocumented), each citing doc line and frozen module. Clusters:
the entire params/finalizer family (`IncomingParams`, `ArgFault`, `ArgIssue`,
`ArgIssueKind`, `Abort`), ~25 exception classes, `omp.dumps`/`omp.loads`,
`journal.decode` plus its six error types, telemetry constants and
`TelemetryError`/`QueryError`, `DOCS_TOTAL_BUDGET`, `MAX_PENDING_EFFECTS`; and
in the other direction `omp.index.*` errors that are raised but documented
nowhere.

**Enum vocabularies diverge badly** (41 compared): `Capability` frozen has 3
members against 22 documented (20 missing, 1 extra `SCHEDULES_PROJECT`);
`SessionStatus` overlaps on only `ABORTED` (4 frozen vs 6 documented);
`SessionKind` missing `EVAL`; `GroupBy` missing `KIND`/`DAY`; `Bucket` extra
`MONTH`; `TitleSource` `USER|MODEL|SYSTEM` vs documented `AUTO|USER`.

**Native exceptions carry no payload** — `create_exception!` declares none, so
`ManifestError.path/key/detail`, `ApiLevelError.requested/supported`,
`DeclarationLimit.count/limit`, `CapabilityError.capability`,
`TrustError.required/actual`, `DuplicateRegistration.name/holder`,
`DeadlineExceeded.deadline`, and `FrameTooLarge.actual/limit` are all prose
strings at their raise sites.

### Docs self-contradictions

`Spill.value` (:901) vs `.buf` (:930) in one section · `services.connect`
documented sync at 00:790 but awaited at 00:389 · `EffectsNotAuthorized(str)`
(03:500) vs `(invocation, spec)` (00:964) · `ContextPatch` "all four lists"
while frozen has five · `View.presentation` used in 07:1487 but absent from
02's field table · patch rejection scope whole-patch (08:551) vs per-op
(08:2018) · `SelectorError` claimed under both `ArtifactError` and `UrlError`
but frozen satisfies only the latter · `PolicyDenied` a frozen value class with
optional `code` in 02 vs an `OmpError` with required `code` frozen.

### Underspecified — needs rulings

`DropParts` is frozen but absent from 08's op vocabulary, leaving its
Replace/pin/unknown-id/conflict semantics undefined; duplicate-id disposition
(reject vs coalesce) unstated; same-patch `Reorder`/`Prune` precedence unstated;
the patch-application algorithm has no epoch fence though staleness is promised
detectable. Precedence arbitration is not representable at all: the registry
holds one extension identity, so cross-claimant live-winner, qualified-shadow,
and hidden-catalog rules cannot be enforced in-process.

### Conformant — recorded so the negatives are evidence

Core-name claims above `Precedence.CORE` correctly raise `DeviceNameError`
before decoration · `Process.send` forwards its generation and refuses stale
handles with `PreconditionFailed` · stale doc leases raise `Conflict` and
`Partial` stays distinct from it · every round-7 closure verified
(`QuotaExceeded` payload, `JournalError.appended`, `StateScopeDenied` base,
`SchemaError`'s dual MRO catching on both documented bases) · `TmlError`'s
payload is complete · and no contracted-stable operation in the existing corpus
varied: `canary`'s slot, `chat-bridge`'s prompt, and `edit-dialect`'s `lift`
and `prompt` were all byte-stable under identical input and all changed under
perturbation, with the deliberately volatile slot correctly rejected.
