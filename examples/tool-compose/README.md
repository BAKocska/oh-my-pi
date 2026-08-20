## What the pi original did

`pi-fabric` exposed a `fabric_exec` programmable tool whose TypeScript ran in a QuickJS sandbox and could compose Pi tools, MCP endpoints, subagents, and actors. It intercepted Pi's extension-tool registration chokepoint to discover and expose registered tools inside that sandbox. That interception made the script runtime a second dispatcher: an inner call could reach a captured implementation rather than re-entering the per-invocation policy decision.

## The omp shape

This port declares one soft `compose` device at `place="worker:compose"`. Its small eval-shaped grammar accepts only `call("exact/device/path", ...)`, optional `await`, literal containers, and references to earlier results; it has no imports, attributes, operators, loops, dynamic path expressions, or Python `eval`. The script and step count are bounded before execution. `[settings.allowed_devices]` is a JSON array of exact paths, and every path must also be present in the live `omp.devices` catalog before the first step runs. A configured value might be `allowed_devices = '''["knowledge/search","notes/append"]'''`; a script can then say `found = await call("knowledge/search", q="admission")` followed by `await call("notes/append", value=found)`.

The registration interception is deliberately deleted. The only dispatch seam is `_invoke_dyn`: each script step must become a fresh ordinary `dyn invoke/<path>` operation, so Core resolves the target and runs admission/approval for that invocation. The compose worker never receives a device implementation or an ambient registry handle. A denial is a typed `StepRecord(status="denied", ...)`, stops later steps, and returns the immutable partial trail; successful earlier calls are not rewritten. This is the load-bearing difference from the QuickJS design: composition cannot escape the per-invocation decision procedure described by `docs/py/01-devices.md:201-213` and `docs/py/05-hooks.md:653-660`.

The named worker supplies the disposable, separately placed execution boundary and the catalog supplies identity; neither supplies authority. `docs/py/00-overview.md:105-108` explains why hostile eval cells belong in a replaceable child, while `docs/py/04-placement.md:206-227` requires a fresh per-call scope rather than warm ambient authority. Oversized script output is represented only in typed step results; the normal worker spill rule remains `docs/py/04-placement.md:254-266`.

## Gaps

None — every symbol this port needs is frozen.
