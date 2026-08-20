"""Guard shared Git worktrees with Core-owned approval tickets."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from enum import StrEnum
from typing import Any

import omp


class GitDisposition(StrEnum):
    """Classify one Git command for parallel-work safety."""

    PASS = "pass"
    DISRUPTIVE = "disruptive"


@dataclass(frozen=True, slots=True)
class GitClassification:
    """Describe the safety classification of one Git argv."""

    disposition: GitDisposition
    operation: str
    reason: str


_DISRUPTIVE = frozenset({"checkout", "rebase", "reset", "clean"})
_GLOBAL_VALUE_OPTIONS = frozenset(
    {"-C", "-c", "--config-env", "--exec-path", "--git-dir", "--namespace", "--work-tree"}
)
_TARGET_OPTIONS = frozenset({"-C", "--git-dir", "--work-tree"})


def _program_name(word: str) -> str:
    return word.rsplit("/", 1)[-1]


def _git_subcommand(argv: tuple[str, ...]) -> tuple[str | None, int, bool]:
    """Return the subcommand, its index, and whether argv overrides the repository target."""
    index = 1
    target_override = False
    while index < len(argv):
        word = argv[index]
        if word == "--":
            index += 1
            break
        if not word.startswith("-") or word == "-":
            break
        option = word.split("=", 1)[0]
        if option in _GLOBAL_VALUE_OPTIONS:
            target_override |= option in _TARGET_OPTIONS
            index += 1 if "=" in word else 2
            continue
        if word.startswith("-C") and word != "-C":
            target_override = True
        index += 1
    if index >= len(argv):
        return None, index, target_override
    return argv[index], index, target_override


def classify_git_argv(
    argv: tuple[str, ...],
    dynamic_args: tuple[bool, ...],
    *,
    shared_root: bool,
) -> GitClassification | None:
    """Classify a normalized BashIR argv without reparsing shell source."""
    if not argv or _program_name(argv[0]) != "git":
        return None

    operation, operation_index, target_override = _git_subcommand(argv)
    if operation is None:
        return GitClassification(GitDisposition.PASS, "", "no Git operation")

    # A linked worktree isolates ordinary operations. Explicit target overrides are not
    # worktree-local: they may redirect the command back to the shared checkout.
    local = not shared_root and not target_override
    if local:
        return GitClassification(GitDisposition.PASS, operation, "linked worktree is isolated")

    if any(dynamic_args[1:]):
        return GitClassification(
            GitDisposition.DISRUPTIVE,
            operation,
            "dynamic Git arguments can conceal a disruptive operation",
        )

    operands = argv[operation_index + 1 :]
    if operation == "clean" and any(word in {"-n", "--dry-run"} for word in operands):
        return GitClassification(GitDisposition.PASS, operation, "git clean is a dry run")
    if operation in _DISRUPTIVE:
        return GitClassification(
            GitDisposition.DISRUPTIVE,
            operation,
            f"git {operation} mutates the shared checkout",
        )
    if operation == "branch" and (
        "-D" in operands or ("--delete" in operands and "--force" in operands)
    ):
        return GitClassification(
            GitDisposition.DISRUPTIVE,
            operation,
            "forced branch deletion mutates shared repository state",
        )
    return GitClassification(GitDisposition.PASS, operation, "operation is not disruptive")


def _kind_name(meta: Any) -> str:
    """Normalize the documented PathMeta.kind shape and frozen mapping receipts."""
    kind = meta.get("kind") if isinstance(meta, Mapping) else getattr(meta, "kind", None)
    value = getattr(kind, "value", kind)
    return str(value).lower()


async def _shared_root() -> bool:
    """Identify the primary checkout from its Environment-visible .git entry."""
    omp.env.require(omp.env.Capability.FS_READ)
    marker = omp.env.info().root.join(".git")
    kind = _kind_name(await omp.env.fs.lstat(marker))
    if kind in {"directory", "dir"}:
        return True
    if kind in {"regular_file", "file"}:
        return False
    raise RuntimeError(f"cannot classify worktree marker kind {kind!r}")


async def _classify(payload: omp.ToolCallEvent) -> tuple[tuple[int, GitClassification], ...]:
    """Classify every Git command in a tool call against current worktree topology."""
    ir: omp.BashIR | None = payload.bash
    if ir is None or not ir.parse_ok:
        return ()
    git_commands = tuple(
        command
        for command in ir.commands
        if command.argv and _program_name(command.argv[0].text) == "git"
    )
    if not git_commands:
        return ()
    shared = await _shared_root()
    findings: list[tuple[int, GitClassification]] = []
    for command in git_commands:
        argv = tuple(argument.text for argument in command.argv)
        finding = classify_git_argv(argv, command.dynamic_args, shared_root=shared)
        if finding is not None:
            findings.append((command.index, finding))
    return tuple(findings)


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.PRECHECK,
    on_failure=omp.OnFailure.DENY,
)
async def classify_parallel_git(
    payload: omp.ToolCallEvent,
    ctx: omp.Context,
) -> omp.HookDecision:
    """Classify Git operations in PRECHECK while respecting its deny-only contract."""
    del ctx
    findings = await _classify(payload)
    disruptive = tuple(item for item in findings if item[1].disposition is GitDisposition.DISRUPTIVE)
    if not disruptive:
        return omp.Defer()
    operations = ", ".join(finding.operation for _, finding in disruptive)
    return omp.Defer(note=f"shared-worktree approval required for: {operations}")


@omp.hook(
    "tool_call",
    phase=omp.HookPhase.APPROVAL,
    on_failure=omp.OnFailure.DENY,
)
async def approve_parallel_git(
    payload: omp.ToolCallEvent,
    ctx: omp.Context,
) -> omp.HookDecision:
    """Attach timeout and default-on-timeout policy to a durable approval ticket."""
    findings = await _classify(payload)
    disruptive = tuple(item for item in findings if item[1].disposition is GitDisposition.DISRUPTIVE)
    if not disruptive:
        return omp.Defer()

    timeout = omp.Duration(str(ctx.settings.get("timeout", "30s")))
    default = ctx.settings.get("default_on_timeout", False)
    if type(default) is not bool:
        raise TypeError("default_on_timeout must be a boolean")
    ir = payload.bash
    assert ir is not None
    evidence = tuple(
        f"bashir[{index}]:{finding.operation}:{finding.reason}"
        for index, finding in disruptive
    )
    return omp.RequireApproval(
        omp.ApprovalSpec(
            title="Disruptive Git operation in shared worktree",
            body="\n".join(f"- {item}" for item in evidence),
            subject=ir.source,
            kind=omp.ApprovalKind.WRITE,
            timeout=timeout,
            default=default,
            pattern="git:" + ",".join(sorted({finding.operation for _, finding in disruptive})),
            evidence=evidence,
        )
    )
