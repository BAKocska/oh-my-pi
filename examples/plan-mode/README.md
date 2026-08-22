# Plan mode

## What the pi original did

`@dreki-gg/pi-plan-mode` assembled a session regime from a planning model and toolset takeover, a standing write guard, and a three-step decision gate before settlement. It also re-registered tools, rewrote stale context, and hosted a loopback plan viewer.

## The omp shape

`/plan on` engages one Session-scoped `plan-mode` campaign; `/plan off` records the completed typed `Plan` and disengages it. The selected model and thinking level live in the campaign's journaled `PlanModeState`, not module state or a separately folded transition log.

The campaign declares `binds=("toolset", "model")` and `claims=("mode",)`. Its CONTEXT and PRE_MODEL reactions bind the read-only planning toolset and the selected planning inference configuration. This uses Core's scoped binding stack rather than re-registering tools or patching `enabled_tools`, preserving the byte-identical tool-array concern across transitions. ADMISSION retains the former fail-closed PRECHECK behavior: core `write` and `edit` calls and any bash call that Core's analyzed IR does not classify as read-only receive `Deny("plan is read-only", code="plan_readonly")`. At SETTLE, `Ladder(3)` bounds the decision reminder before `Exhaust.SETTLE` gives up.

This is the regime shape from `docs/py/15-campaigns.md` §7: one Session campaign owns the mode claim, bindings, guard, and bounded decision gate. Client-side context rewriting and the loopback viewer remain deleted.

## Gaps

- The §6 plan decision gate specifies a ×3 gate with `revive=reset` and `ResetOn(user prompt)`. Frozen v1 has `Ladder(3)`, but not `revive` or `ResetOn`, so this port has the bound without reset-on-prompt/revival behavior.
- The §7 regime fields `members` and `dwell` are doc-only. Frozen v1 cannot supervise separate permanent write-guard and transient decision-gate members, so this port approximates the regime with one campaign; it also cannot declare dwell hysteresis.
