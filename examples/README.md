# Extension examples

Sixteen ports of real pi-ecosystem extensions onto the omp Python extension
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
