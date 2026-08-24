## What the pi original did

`pi-mcp-adapter` connected to MCP servers through one low-context `mcp` proxy with search, list, describe, call, status, and authentication operations. It could also promote selected endpoints into direct tools, hot-swap the active tool list, supervise stdio children, and provide setup and OAuth UI. That avoided exposing every MCP schema by default, but required a second registry and substantial lifecycle machinery inside the extension.

## The omp shape

This port declares a JSON `servers` setting, starts each configured command through the Environment's named-process supervisor, performs only MCP `initialize`, `tools/list`, and `tools/call`, and mounts discovered endpoints at `mcp/<server>/<tool>`. The manifest's `[settings.servers]` schema receives its resolved value from user or project config such as `[extensions.settings."examples.mcp-devices"]` with `servers = '''{"filesystem":{"command":"npx","args":["-y","@modelcontextprotocol/server-filesystem","."]}}'''`. Each endpoint stays behind the `xd` builtin inside the core `shell` tool and is called as a scriptable command such as `xd mcp/filesystem/read_file …`; `xd mcp/filesystem/read_file --help` shows its exact schema-derived CLI.

There is deliberately **no `mcp` proxy tool** with search/list/describe/call operations: `xd` already supplies discovery, documentation, and invocation through the shell without adding endpoint schemas to the model request. Process state changes batch all endpoint enable/disable transitions through `omp.devices`; the code never re-registers devices on an availability flip. This is the native-mounting simplification in `docs/py/01-devices.md` §“pi-mcp-adapter → native mounting”, using the supervised-daemon shape from `docs/py/11-env.md` §3.

### Dynamic mount design

The manifest and IMPORT phase declare one `mcp` parent with a fixed family, revision, placement, and provenance. ACTIVATE may only add relative leaves beneath that parent via one `mount_many` batch, carrying each endpoint's body, input schema, summary, and docs; it cannot claim another top-level name. Reachability is separate from identity: reconnect and failure paths send `AvailabilityDelta` rows through one `omp.devices.set_availability` call, so a server with fifty endpoints produces one availability transaction, one catalog notification, and no re-registration.

## Gaps

- Environment-owned spawn is only partially frozen. `omp.env.proc.ensure` and `omp.env.Process.send` exist, while typed `RestartPolicy`/readiness values and `ProcessOutput` remain unfrozen (`docs/py/11-env.md` §3).
