"""Apply declarative shell admission rules over Core-provided BashIR facts."""

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum

import omp


class RuleAction(StrEnum):
    """Name the admission outcome associated with a permission rule."""

    ALLOW = "allow"
    DENY = "deny"
    ASK = "ask"


class RuleFact(StrEnum):
    """Name the BashIR fact matched by a permission rule."""

    PARSE_FAILED = "parse_failed"
    DYNAMIC_EVAL = "has_dynamic_eval"
    UNRESOLVED_CWD = "cwd_unresolved"
    READ_ONLY = "is_read_only"
    WRITES_OUTSIDE = "writes_outside"


@dataclass(frozen=True, slots=True)
class Rule:
    """Describe one declarative permission match and its outcome."""

    id: str
    action: RuleAction
    fact: RuleFact
    reason: str


_DENY_PARSE_FAILURE = Rule(
    "bash.parse-failed",
    RuleAction.DENY,
    RuleFact.PARSE_FAILED,
    "the shell parser could not establish execution semantics",
)
_DENY_DYNAMIC_EVAL = Rule(
    "bash.dynamic-eval",
    RuleAction.DENY,
    RuleFact.DYNAMIC_EVAL,
    "dynamic evaluation hides the command that would execute",
)
_DENY_UNRESOLVED_CWD = Rule(
    "bash.cwd-unresolved",
    RuleAction.DENY,
    RuleFact.UNRESOLVED_CWD,
    "a command has an unresolved working directory",
)
_ALLOW_READ_ONLY = Rule(
    "bash.read-only",
    RuleAction.ALLOW,
    RuleFact.READ_ONLY,
    "the analyzed command is read-only",
)
_ASK_OUTSIDE_WRITE = Rule(
    "bash.write-outside",
    RuleAction.ASK,
    RuleFact.WRITES_OUTSIDE,
    "the command writes outside the workspace",
)

# The complete permission policy, in deterministic evaluation order.
RULES: tuple[Rule, ...] = (
    _DENY_PARSE_FAILURE,
    _DENY_DYNAMIC_EVAL,
    _DENY_UNRESOLVED_CWD,
    _ALLOW_READ_ONLY,
    _ASK_OUTSIDE_WRITE,
)


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.PRECHECK,
    on_failure=omp.OnFailure.DENY,
)
async def precheck_permission(
    payload: omp.ToolCallEvent,
    ctx: omp.Context,
) -> omp.HookDecision:
    """Deny ambiguous shell calls and recognize the deterministic read-only allow tier."""
    del ctx
    ir: omp.BashIR | None = payload.bash
    if ir is None:
        return omp.Defer()
    if not ir.parse_ok:
        return omp.Deny(_DENY_PARSE_FAILURE.reason, code=_DENY_PARSE_FAILURE.id)
    if ir.has_dynamic_eval:
        return omp.Deny(_DENY_DYNAMIC_EVAL.reason, code=_DENY_DYNAMIC_EVAL.id)
    if any(command.cwd is None for command in ir.commands):
        return omp.Deny(_DENY_UNRESOLVED_CWD.reason, code=_DENY_UNRESOLVED_CWD.id)
    if ir.is_read_only():
        # PRECHECK is deny-only; abstention preserves later vetoes while the terminal policy
        # performs the allow represented by this rule.
        return omp.Defer(note=f"matched allow rule {_ALLOW_READ_ONLY.id}")
    return omp.Defer()


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.APPROVAL,
    on_failure=omp.OnFailure.DENY,
)
async def require_outside_write_approval(
    payload: omp.ToolCallEvent,
    ctx: omp.Context,
) -> omp.HookDecision:
    """Return a durable approval request for writes outside every workspace root."""
    ir: omp.BashIR | None = payload.bash
    if ir is None:
        return omp.Defer()
    external = ir.writes_outside(ctx.roots)
    if not external:
        return omp.Defer()
    targets = tuple(path.resolved or path.lexical for path in external)
    return omp.RequireApproval(
        omp.ApprovalSpec(
            title="Write outside the workspace",
            body="\n".join(
                f"- {path.lexical} -> {path.resolved or 'unresolved'}" for path in external
            ),
            subject=ir.source,
            kind=omp.ApprovalKind.WRITE,
            pattern=" ".join(sorted(set(targets))),
            evidence=(_ASK_OUTSIDE_WRITE.id,),
            require_human=True,
        )
    )
