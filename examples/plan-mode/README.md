# Plan mode

## What the pi original did

`@dreki-gg/pi-plan-mode` assembled a session regime from a planning model and toolset takeover, a standing write guard, and a three-step decision gate before settlement. It also re-registered tools, rewrote stale context, and hosted a loopback plan viewer.

## The omp shape

Invoke this extension's soft `plan` tool with `op="on"`, `op="status"`, or `op="off"`; `on` also requires the selected model and thinking level, while `off` requires the completed plan. The app's built-in `/plan` command routes to Core's campaign id `plan`, so it is not this extension's entry point.

`op="on"` engages two campaigns. The `plan-mode` regime is Session-scoped, has no ladder, declares `claims=("mode", "worktree")` and `binds=("toolset", "model")`, and exits only when `op="off"` explicitly disengages it. Its CONTEXT and PRE_MODEL reactions bind the read-only planning toolset and selected model/thinking configuration. Its ADMISSION reaction retains the fail-closed write guard: core `write` and `edit` calls and any bash call that Core's analyzed IR does not classify as read-only receive `Deny("plan is read-only", code="plan_readonly")`.

The separate Run-scoped `plan-decision-gate` subscribes only to SETTLE. Its `Ladder(3)` bounds decision nudges before `Exhaust.SETTLE` gives up. Gate exhaustion does not disengage the regime. `op="off"` records the typed `Plan`, disengages a still-active gate, and then explicitly disengages the regime. This split follows `docs/py/15-campaigns.md` §7: a regime lingers until explicit exit while its bounded gate is a member-like campaign. Client-side context rewriting and the loopback viewer remain deleted.

## Gaps

- Doc 15 §7's member supervision and the gate's `revive=reset` policy remain doc-only; frozen v1 cannot declare the gate as a supervised regime member or reset its ladder on revival. Its policy surface also cannot express §6's `ResetOn(user prompt)`.
- Frozen v1 `Continue` carries only `inject`, so doc 15 §6's plan-gate `Continue(force="required")` is not expressible.
- Frozen `dispatch_campaign_react` accepts a decision point but currently does not forward it into the handler event. Both handlers read `event.get("point")` defensively and return `Pass` when the point key was not supplied; multi-point routing therefore cannot select these reactions until dispatch preserves the point.
- The agent-side consumers do not yet apply the `toolset` binding or the thinking selection carried by the `model` binding; recorded agent-side gap.
- The agent-side ADMISSION consumer currently collapses `Deny("plan is read-only", code="plan_readonly")` to a terminal protocol error instead of denying only the call; recorded agent-side gap.
- The SETTLE consumer currently substitutes `AutoContinue` and does not yet deliver `Continue.inject`; recorded agent-side gap.
