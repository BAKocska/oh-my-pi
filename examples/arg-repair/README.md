# Argument repair

## What the pi original did

`@r3b1s/pi-repair-layer` installed a validate-then-repair pass ahead of built-in calls. A representative pi-era rule looked like this:

```ts
if (issue.path === "labels" && typeof args.labels === "string") {
  args.labels = args.labels.split(",");
}
```

That global pass had to infer a device's intent after validation failed. The secondary origin, `pi-thinking-only-guard`, similarly recovered tool-shaped text trapped in thinking blocks. The survey descriptions are in `.plan/user-requests/2026-08-10-pi-extension-survey/catalog.md:243,373`.

## The omp shape

The extension does not contain a repair pass. Each argument declares the aliases, ordered coercions, expected shape, and worked retry example that the central finalizer needs. The replacement for the rule above is one argument-spec row:

```python
labels: Annotated[list[str], omp.Field(alias=("label", "tags"), coerce=(omp.Coerce.CSV, omp.Coerce.SINGLETON), expected="a list of label strings", example='["arguments", "repair"]')]
```

The body receives the final, policy-approved canonical object and performs no validation. Alias matches and coercions are journaled against the raw emission; argument failures remain attributable to the device revision for metrics. The `example` value is load-bearing retry guidance: `PLAN.md` L11 records that dropping `ArgSpec.example` loses "the field that trains the retry." See `docs/py/03-params.md` §Charitable decoding, §3 “Deleting `@r3b1s/pi-repair-layer`,” and §`crates/tool` item 1.

Strict finalization does not guess when two meanings are present. A duplicate canonical key, a canonical key plus one of its aliases, or two aliases for one canonical field is rejected as `ArgIssueKind.AMBIGUOUS`; values are never silently merged or selected by order (`docs/py/03-params.md` §Strict at ARGS_FINALIZED).

Nothing scans thinking text for tool calls: only a provider-emitted tool invocation enters argument finalization, so the secondary origin's recovery mechanism is deleted rather than reimplemented.

## Gaps

- **Registered argument metadata cannot be introspected.** `DeviceDefinition` in `crates/py/python/omp/_registry.py:114-133` stores `schema` and device-name `aliases` but no per-revision argument-spec table, and `DeclarationRegistry` exposes no argument-spec accessor. `docs/py/03-params.md` §`crates/tool` item 1 requires immutable `ArgSpec` storage per `Rev`; §`crates/py` requires `Annotated` metadata to be lowered once at import. Consequently the requested frozen-registry introspection smoke cannot be run until that surface exists.
