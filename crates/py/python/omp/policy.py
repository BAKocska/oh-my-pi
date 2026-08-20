"""Frozen bash IR views, sandbox profiles, and policy operations."""

from __future__ import annotations

import fnmatch
import inspect
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass
from enum import IntFlag, StrEnum
from typing import Final, TypeAlias

from _omp import Duration, EnvPath, OmpError, WorkspaceUri

from ._errors import NotWiredError
from ._registry import registry as _declarations
from .hooks import ApprovalKind, ApprovalSpec, PolicyScope, Unreachable


class ParseFailure(StrEnum):
    """Classify a Bash IR parse failure."""
    SYNTAX = "syntax"
    UNTERMINATED = "unterminated"
    NODE_LIMIT = "node_limit"
    SOURCE_LIMIT = "source_limit"
    DEPTH_LIMIT = "depth_limit"
    TIMEOUT = "timeout"

class AndOrOp(StrEnum):
    """Join adjacent pipelines in an and-or list."""
    AND = "and"
    OR = "or"

class Separator(StrEnum):
    """Describe how an and-or list terminates."""
    SEQUENCE = "sequence"
    ASYNC = "async"

class Dynamism(IntFlag):
    """Record dynamic shell-expansion features."""
    NONE = 0
    PARAMETER = 1
    COMMAND_SUB = 2
    ARITHMETIC = 4
    TILDE = 8
    GLOB = 16
    BRACE = 32
    ESCAPE = 64

class Quoting(StrEnum):
    """Describe shell argument quoting."""
    BARE = "bare"
    SINGLE = "single"
    DOUBLE = "double"
    ANSI_C = "ansi_c"
    MIXED = "mixed"

class RedirectOp(StrEnum):
    """Classify a shell redirection operator."""
    READ = "read"
    WRITE = "write"
    APPEND = "append"
    READ_WRITE = "read_write"
    CLOBBER = "clobber"
    DUP_IN = "dup_in"
    DUP_OUT = "dup_out"
    HERE_DOC = "here_doc"
    HERE_STRING = "here_string"
    OUT_AND_ERR = "out_and_err"

class RedirectTarget(StrEnum):
    """Classify a redirection target."""
    FILE = "file"
    FD = "fd"
    PROCESS_SUB = "process_sub"
    DUPLICATE = "duplicate"

class ProcessSubDirection(StrEnum):
    """Describe process-substitution data flow."""
    READ = "read"
    WRITE = "write"

class CompoundKind(StrEnum):
    """Classify a compound shell command."""
    ARITHMETIC = "arithmetic"
    ARITHMETIC_FOR = "arithmetic_for"
    BRACE_GROUP = "brace_group"
    SUBSHELL = "subshell"
    FOR = "for"
    CASE = "case"
    IF = "if"
    WHILE = "while"
    UNTIL = "until"
    COPROCESS = "coprocess"

class Access(IntFlag):
    """Describe filesystem access inferred from shell syntax."""
    READ = 1
    WRITE = 2
    APPEND = 4
    EXEC = 8
    DELETE = 16
    METADATA = 32
    CREATE = 64

class PathOrigin(StrEnum):
    """Identify the syntax that produced a path reference."""
    ARGV = "argv"
    REDIRECT = "redirect"
    ASSIGNMENT = "assignment"
    CWD = "cwd"
    HEREDOC = "heredoc"
    INTERPRETER = "interpreter"
    PROCESS_SUB = "process_sub"
    TEST = "test"

class NetKind(StrEnum):
    """Classify inferred network activity."""
    HTTP = "http"
    GIT_REMOTE = "git_remote"
    SSH = "ssh"
    SCP = "scp"
    RSYNC = "rsync"
    DNS = "dns"
    RAW_SOCKET = "raw_socket"
    PACKAGE_MANAGER = "package_manager"
    UNKNOWN = "unknown"

class NetDirection(StrEnum):
    """Describe inferred network data flow."""
    EGRESS = "egress"
    INGRESS = "ingress"
    BIDIRECTIONAL = "bidirectional"

class OpaqueReason(StrEnum):
    """Explain why shell behavior could not be analyzed."""
    EVAL = "eval"
    SOURCE = "source"
    EXEC_REPLACE = "exec_replace"
    DYNAMIC_NAME = "dynamic_name"
    STDIN_DRIVEN = "stdin_driven"
    INTERPRETER_DYNAMIC = "interpreter_dynamic"
    JQ_SYSTEM = "jq_system"
    TEST_SUBSCRIPT = "test_subscript"

class Tier(StrEnum):
    """Select a device's default approval tier."""
    READ = "read"
    WRITE = "write"
    EXEC = "exec"
    PRIVILEGED = "privileged"

@dataclass(frozen=True, slots=True)
class Span:
    """Locate syntax within the original script."""
    start: int
    end: int
    line: int
    column: int

@dataclass(frozen=True, slots=True)
class ParseError:
    """Describe a failed Bash IR parse."""
    kind: ParseFailure
    message: str
    span: Span | None

@dataclass(frozen=True, slots=True)
class BashArg:
    """Describe one shell argument."""
    text: str
    dynamic: bool
    dynamism: Dynamism
    quoting: Quoting
    span: Span

@dataclass(frozen=True, slots=True)
class BashAssignment:
    """Describe one shell assignment."""
    name: str
    index: str | None
    value: str | None
    elements: tuple[tuple[str | None, str], ...]
    array: bool
    append: bool
    exported: bool
    dynamism: Dynamism
    span: Span

@dataclass(frozen=True, slots=True)
class HereDoc:
    """Describe a here-document payload."""
    delimiter: str
    body: str
    strip_tabs: bool
    expands: bool

@dataclass(frozen=True, slots=True)
class PathRef:
    """Describe one inferred filesystem reference."""
    lexical: str
    resolved: str | None
    absolute: str | None
    access: Access
    origin: PathOrigin
    command_index: int
    outside_workspace: bool
    exists: bool
    dynamic: bool
    span: Span

@dataclass(frozen=True, slots=True)
class NetRef:
    """Describe one inferred network reference."""
    kind: NetKind
    direction: NetDirection
    host: str | None
    port: int | None
    scheme: str | None
    url: str | None
    command_index: int
    dynamic: bool
    span: Span

@dataclass(frozen=True, slots=True)
class OpaqueEvaluator:
    """Describe one unanalyzable evaluator."""
    command_index: int
    name: str
    reason: OpaqueReason
    span: Span

@dataclass(frozen=True, slots=True)
class ProcessSubIR:
    """Describe one process substitution."""
    direction: ProcessSubDirection
    body: tuple[BashAndOrList, ...]
    span: Span

@dataclass(frozen=True, slots=True)
class BashRedirect:
    """Describe one shell redirection."""
    fd: int | None
    op: RedirectOp
    target_kind: RedirectTarget
    target: str | None
    target_fd: int | None
    process_sub: ProcessSubIR | None
    heredoc: HereDoc | None
    dynamism: Dynamism
    path: PathRef | None
    span: Span

@dataclass(frozen=True, slots=True)
class BashCommandIR:
    """Describe one simple shell command."""
    index: int
    name: str | None
    argv: tuple[BashArg, ...]
    dynamic_args: tuple[bool, ...]
    env: tuple[BashAssignment, ...]
    redirects: tuple[BashRedirect, ...]
    process_subs: tuple[ProcessSubIR, ...]
    reads: tuple[PathRef, ...]
    writes: tuple[PathRef, ...]
    net: tuple[NetRef, ...]
    cwd: str | None
    depth: int
    container: CompoundKind | None
    subshell: bool
    builtin: bool
    coreutil: bool
    external: bool
    read_only: bool
    interpreter_code: str | None
    span: Span

@dataclass(frozen=True, slots=True)
class BashCompound:
    """Describe one compound shell command."""
    kind: CompoundKind
    body: tuple[BashAndOrList, ...]
    subject: tuple[BashArg, ...]
    redirects: tuple[BashRedirect, ...]
    span: Span

@dataclass(frozen=True, slots=True)
class BashFunctionDef:
    """Describe one shell function definition."""
    name: str
    body: tuple[BashAndOrList, ...]
    redirects: tuple[BashRedirect, ...]
    span: Span

@dataclass(frozen=True, slots=True)
class BashTestExpr:
    """Describe one shell test expression."""
    source: str
    paths: tuple[PathRef, ...]
    dynamism: Dynamism
    span: Span

BashNode: TypeAlias = BashCommandIR | BashCompound | BashFunctionDef | BashTestExpr

@dataclass(frozen=True, slots=True)
class BashPipeline:
    """Describe a shell pipeline."""
    commands: tuple[BashNode, ...]
    negated: bool
    timed: bool
    span: Span

@dataclass(frozen=True, slots=True)
class BashAndOrList:
    """Describe pipelines connected by boolean operators."""
    pipelines: tuple[BashPipeline, ...]
    operators: tuple[AndOrOp, ...]
    separator: Separator
    span: Span


def _walk_process_subs(redirects: tuple[BashRedirect, ...]) -> Iterator[BashNode]:
    for redirect in redirects:
        if redirect.process_sub is not None:
            yield from _walk_lists(redirect.process_sub.body)

def _command_process_subs(command: BashCommandIR) -> tuple[ProcessSubIR, ...]:
    by_span: dict[tuple[int, int], ProcessSubIR] = {
        (process_sub.span.start, process_sub.span.end): process_sub
        for process_sub in command.process_subs
    }
    for redirect in command.redirects:
        if redirect.process_sub is not None:
            process_sub = redirect.process_sub
            by_span.setdefault((process_sub.span.start, process_sub.span.end), process_sub)
    return tuple(
        process_sub
        for _, process_sub in sorted(by_span.items(), key=lambda item: item[0])
    )


def _walk_lists(lists: tuple[BashAndOrList, ...]) -> Iterator[BashNode]:
    for item in lists:
        for pipeline in item.pipelines:
            for node in pipeline.commands:
                yield node
                if isinstance(node, (BashCompound, BashFunctionDef)):
                    yield from _walk_lists(node.body)
                    yield from _walk_process_subs(node.redirects)
                elif isinstance(node, BashCommandIR):
                    for process_sub in _command_process_subs(node):
                        yield from _walk_lists(process_sub.body)

@dataclass(frozen=True, slots=True)
class BashIR:
    """Expose immutable, host-analyzed Bash syntax and effects."""
    source: str
    rev: str
    parser_rev: str
    parse_ok: bool
    parse_error: ParseError | None
    truncated: bool
    node_count: int
    is_compound: bool
    has_dynamic_eval: bool
    lists: tuple[BashAndOrList, ...]
    commands: tuple[BashCommandIR, ...]
    functions: tuple[BashFunctionDef, ...]
    reads: tuple[PathRef, ...]
    writes: tuple[PathRef, ...]
    net: tuple[NetRef, ...]
    opaque: tuple[OpaqueEvaluator, ...]

    def walk(self) -> Iterator[BashNode]:
        """Yield every syntax node depth-first in source order."""
        return _walk_lists(self.lists)

    def simple_commands(self) -> Iterator[BashCommandIR]:
        """Iterate flattened simple commands in execution order."""
        return iter(self.commands)

    def segment(self, index: int) -> str:
        """Return the exact UTF-8 source segment for a flattened command."""
        span = self.commands[index].span
        return self.source.encode("utf-8")[span.start:span.end].decode("utf-8")

    def is_read_only(self) -> bool:
        """Return whether analysis found no write, network, or dynamic effects."""
        return not self.writes and not self.net and not self.has_dynamic_eval and all(command.read_only for command in self.commands)

    @staticmethod
    def _roots(roots: WorkspaceUri | str | Iterable[WorkspaceUri | str]) -> tuple[str, ...]:
        if isinstance(roots, (str, WorkspaceUri)):
            return (str(roots),)
        return tuple(str(root) for root in roots)

    @classmethod
    def _outside(cls, refs: tuple[PathRef, ...], roots: WorkspaceUri | str | Iterable[WorkspaceUri | str]) -> tuple[PathRef, ...]:
        prefixes = cls._roots(roots)
        return tuple(ref for ref in refs if ref.resolved is None or not any(ref.resolved == root or ref.resolved.startswith(root.rstrip("/") + "/") for root in prefixes))

    def writes_outside(self, roots: WorkspaceUri | str | Iterable[WorkspaceUri | str]) -> tuple[PathRef, ...]:
        """Return writes outside all roots, treating unresolved paths as outside."""
        return self._outside(self.writes, roots)

    def reads_outside(self, roots: WorkspaceUri | str | Iterable[WorkspaceUri | str]) -> tuple[PathRef, ...]:
        """Return reads outside all roots, treating unresolved paths as outside."""
        return self._outside(self.reads, roots)

    def net_sinks(self) -> tuple[NetRef, ...]:
        """Return inferred egress and bidirectional network references."""
        return tuple(ref for ref in self.net if ref.direction in (NetDirection.EGRESS, NetDirection.BIDIRECTIONAL))

    def touches(self, *patterns: str) -> tuple[PathRef, ...]:
        """Return path references matching any lexical or resolved glob."""
        return tuple(ref for ref in self.reads + self.writes if any(fnmatch.fnmatch(ref.lexical, pattern) or (ref.resolved is not None and fnmatch.fnmatch(ref.resolved, pattern)) for pattern in patterns))

BASH_IR_REV: Final[str] = "bashir@3"
BASH_IR_MAX_SOURCE: Final[int] = 262144
BASH_IR_MAX_NODES: Final[int] = 50000
BASH_IR_MAX_DEPTH: Final[int] = 128
POLICY_DEADLINE: Final[Duration] = Duration("30s")
APPROVAL_DEADLINE: Final[Duration] = Duration("5m")
VIOLATION_COALESCE: Final[Duration] = Duration("1s")

class SandboxMode(StrEnum):
    """Select sandbox enforcement behavior."""
    OFF = "off"
    OBSERVE = "observe"
    ENFORCE = "enforce"

class SandboxBackend(StrEnum):
    """Identify an available sandbox backend."""
    LANDLOCK = "landlock"
    BWRAP = "bwrap"
    SEATBELT = "seatbelt"
    JOB_OBJECT = "job_object"
    NONE = "none"

class RuleEffect(StrEnum):
    """Allow or deny an operation."""
    ALLOW = "allow"
    DENY = "deny"

class NetworkMode(StrEnum):
    """Select network confinement behavior."""
    OPEN = "open"
    PROXY = "proxy"
    DENY = "deny"

class DnsPolicy(StrEnum):
    """Select DNS resolution policy."""
    PROXY_ONLY = "proxy_only"
    ALLOW = "allow"
    DENY = "deny"

class SandboxSessionKind(StrEnum):
    """Classify the confined execution session."""
    TOOL = "tool"
    USER = "user"
    PROCESS = "process"
    WORKER = "worker"

class FilesystemGrade(StrEnum):
    """Grade installed filesystem confinement."""
    HARD = "hard"
    BROKERED = "brokered"
    BEST_EFFORT = "best_effort"
    NONE = "none"

class NetworkGrade(StrEnum):
    """Grade installed network confinement."""
    HARD = "hard"
    PROXY_ONLY = "proxy_only"
    NONE = "none"

class ProcessGrade(StrEnum):
    """Grade installed process confinement."""
    HARD = "hard"
    PARTIAL = "partial"
    NONE = "none"

class ViolationKind(StrEnum):
    """Classify a sandbox violation."""
    FS_READ = "fs_read"
    FS_WRITE = "fs_write"
    FS_EXEC = "fs_exec"
    FS_CREATE = "fs_create"
    FS_DELETE = "fs_delete"
    NET_CONNECT = "net_connect"
    NET_BIND = "net_bind"
    NET_DNS = "net_dns"
    NET_DOMAIN = "net_domain"
    RESOURCE = "resource"
    PRIVILEGE = "privilege"
    UNKNOWN = "unknown"

class TicketState(StrEnum):
    """Describe the lifecycle of an approval ticket."""
    PENDING = "pending"
    DECIDED = "decided"
    WITHDRAWN = "withdrawn"

class ApprovalSource(StrEnum):
    """Identify the authority that resolved an approval."""
    USER = "user"
    EXTERNAL = "external"
    FORWARDED = "forwarded"
    CONFIG = "config"
    EXTENSION = "extension"
    TIMEOUT = "timeout"
    UNAVAILABLE = "unavailable"

@dataclass(frozen=True, slots=True)
class PathRule:
    """Describe a filesystem path rule."""
    path: str
    recursive: bool = True
    create: bool = False
    delete: bool = False

@dataclass(frozen=True, slots=True)
class FilesystemPolicy:
    """Describe filesystem confinement rules."""
    allow_read: tuple[PathRule, ...] = ()
    deny_read: tuple[PathRule, ...] = ()
    allow_write: tuple[PathRule, ...] = ()
    deny_write: tuple[PathRule, ...] = ()
    allow_exec: tuple[PathRule, ...] = ()
    deny_exec: tuple[PathRule, ...] = ()
    follow_symlinks: bool = False
    tmpdir: str | None = None
    read_default: RuleEffect = RuleEffect.DENY
    write_default: RuleEffect = RuleEffect.DENY
    exec_default: RuleEffect = RuleEffect.ALLOW

@dataclass(frozen=True, slots=True)
class DomainRule:
    """Describe an allowed or denied network domain."""
    domain: str
    ports: tuple[int, ...] = ()

@dataclass(frozen=True, slots=True)
class NetworkPolicy:
    """Describe network confinement rules."""
    mode: NetworkMode = NetworkMode.PROXY
    allow_domains: tuple[DomainRule, ...] = ()
    deny_domains: tuple[DomainRule, ...] = ()
    allow_ports: tuple[int, ...] = (80, 443)
    allow_localhost: bool = False
    allow_unix_sockets: tuple[str, ...] = ()
    allow_mach_lookup: tuple[str, ...] = ()
    dns: DnsPolicy = DnsPolicy.PROXY_ONLY
    inject_proxy_env: bool = True

@dataclass(frozen=True, slots=True)
class ExecPolicy:
    """Describe executable and process confinement rules."""
    allow: tuple[str, ...] = ()
    deny: tuple[str, ...] = ()
    default: RuleEffect = RuleEffect.ALLOW
    allow_interpreters: bool = True
    allow_setuid: bool = False
    allow_ptrace: bool = False
    allow_new_session: bool = False
    max_children: int | None = None

@dataclass(frozen=True, slots=True)
class ResourceBudget:
    """Describe process resource ceilings."""
    wall: Duration | None = None
    cpu: Duration | None = None
    memory_bytes: int | None = None
    file_size_bytes: int | None = None
    open_files: int | None = None
    processes: int | None = None
    disk_write_bytes: int | None = None
    stdout_bytes: int | None = None

@dataclass(frozen=True, slots=True)
class SandboxProfile:
    """Describe a composable sandbox profile."""
    mode: SandboxMode = SandboxMode.ENFORCE
    filesystem: FilesystemPolicy = FilesystemPolicy()
    network: NetworkPolicy = NetworkPolicy()
    exec: ExecPolicy = ExecPolicy()
    resources: ResourceBudget = ResourceBudget()
    label: str = ""
    ignore_violations: tuple[str, ...] = ()
    require: tuple[SandboxBackend, ...] = ()

@dataclass(frozen=True, slots=True)
class SandboxRequest:
    """Describe a request to establish a sandboxed session."""
    session_kind: SandboxSessionKind
    cwd: EnvPath
    roots: tuple[WorkspaceUri, ...]
    backends: tuple[SandboxBackend, ...]
    invocation_id: str | None
    process_name: str | None

@dataclass(frozen=True, slots=True)
class SandboxCapabilities:
    """Describe sandbox facilities available on the host."""
    backends: tuple[SandboxBackend, ...]
    landlock_abi: int | None
    filesystem: bool
    network: bool
    domain_filtering: bool
    resource_limits: bool
    degraded: tuple[str, ...]

@dataclass(frozen=True, slots=True)
class SandboxEnforcement:
    """Describe the confinement actually installed for a session."""
    filesystem: FilesystemGrade
    network: NetworkGrade
    process: ProcessGrade
    backend: str
    degraded_reasons: tuple[str, ...]

@dataclass(frozen=True, slots=True)
class Violation:
    """Describe an observed or enforced sandbox violation."""
    kind: ViolationKind
    subject: str
    access: Access | None
    profile: str
    rule: str | None
    backend: SandboxBackend
    session_kind: SandboxSessionKind
    invocation_id: str | None
    command_index: int | None
    pid: int | None
    argv0: str | None
    enforced: bool
    count: int

@dataclass(frozen=True, slots=True)
class Amend:
    """Request a scoped sandbox profile amendment."""
    patch: SandboxProfile
    scope: PolicyScope = PolicyScope.SESSION
    reason: str = ""
    retry: bool = False
    approval: ApprovalSpec | None = None

@dataclass(frozen=True, slots=True)
class ApprovalDecision:
    """Record the durable resolution of an approval ticket."""
    approved: bool
    scope: PolicyScope
    source: ApprovalSource
    decided_by: str | None
    reason: str | None
    audited: bool

@dataclass(frozen=True, slots=True)
class ApprovalTicket:
    """Expose a durable aggregate approval request."""
    ticket_id: str
    invocation_id: str | None
    reasons: tuple[ApprovalSpec, ...]
    state: TicketState
    decision: ApprovalDecision | None
    created_at: float

@dataclass(frozen=True, slots=True)
class RuleRef:
    """Identify a policy rule contributing to a denial."""
    id: str

@dataclass(frozen=True, slots=True)
class PolicyDenied(OmpError):
    """Describe a structured policy denial."""
    reason: str
    code: str
    decision_id: str
    rules: tuple[RuleRef, ...]

    def __post_init__(self) -> None:
        OmpError.__init__(self, self.reason)

@dataclass(frozen=True, slots=True)
class ProfileHandle:
    """Represent an installed scoped sandbox profile."""
    profile: SandboxProfile

    async def revoke(self) -> None:
        """Revoke the installed profile contribution."""
        raise NotWiredError("omp.policy.ProfileHandle.revoke")

class PolicyError(OmpError):
    """Base error for policy transport and revision failures."""

class ProfileRejected(PolicyError):
    """A sandbox profile is malformed or names a secret placeholder."""

class ProfileWidened(PolicyError):
    """A profile contribution would loosen running confinement."""

class EnforcementUnavailable(PolicyError):
    """No available backend can satisfy required confinement."""

async def parse(script: str, *, cwd: EnvPath | None = None) -> BashIR:
    """Parse shell source into host-analyzed Bash IR."""
    del script, cwd
    raise NotWiredError("omp.policy.parse")

async def match_paths(path: str, *patterns: str, cwd: EnvPath | None = None, access: Access | None = None) -> tuple[PathRef, ...]:
    """Match a path using the host's policy path semantics."""
    del path, patterns, cwd, access
    raise NotWiredError("omp.policy.match_paths")

async def capabilities() -> SandboxCapabilities:
    """Return sandbox capabilities available on the host."""
    raise NotWiredError("omp.policy.capabilities")

async def effective_profile(*, session: str | None = None) -> SandboxProfile:
    """Return the composed profile installed for a session."""
    del session
    raise NotWiredError("omp.policy.effective_profile")

async def enforcement(*, session: str | None = None) -> SandboxEnforcement:
    """Return the confinement receipt for a session."""
    del session
    raise NotWiredError("omp.policy.enforcement")

async def install(profile: SandboxProfile, *, scope: PolicyScope = PolicyScope.SESSION) -> ProfileHandle:
    """Install a scoped profile that can only narrow confinement."""
    del profile, scope
    raise NotWiredError("omp.policy.install")

async def amend(patch: SandboxProfile, *, scope: PolicyScope, reason: str, approval: ApprovalSpec | None = None) -> None:
    """Apply a scoped profile amendment under policy authority."""
    del patch, scope, reason, approval
    raise NotWiredError("omp.policy.amend")

def approver(
    name: str,
    *,
    kinds: Iterable[ApprovalKind] = (),
    timeout: Duration = APPROVAL_DEADLINE,
    unreachable: Unreachable = Unreachable.FAIL_CLOSED,
) -> Callable[[Callable[..., object]], Callable[..., object]]:
    """Declare an idempotent external approver without performing host I/O."""

    if not isinstance(name, str) or not name:
        raise ValueError("approver name must be a non-empty string")
    try:
        frozen_kinds = tuple(kinds)
    except TypeError as error:
        raise TypeError("approver kinds must be an iterable of ApprovalKind") from error
    if any(not isinstance(kind, ApprovalKind) for kind in frozen_kinds):
        raise TypeError("approver kinds must contain only ApprovalKind values")
    if not isinstance(timeout, Duration):
        raise TypeError("approver timeout must be Duration")
    if not isinstance(unreachable, Unreachable):
        raise TypeError("approver unreachable must be Unreachable")

    def decorate(handler: Callable[..., object]) -> Callable[..., object]:
        if not callable(handler) or not inspect.iscoroutinefunction(handler):
            raise TypeError("@omp.approver may decorate only an async callable")
        _declarations.register_approver(
            name, frozen_kinds, timeout, unreachable, handler
        )
        return handler

    return decorate


async def pending() -> tuple[ApprovalTicket, ...]:
    """Return pending approval tickets in filing order."""
    raise NotWiredError("omp.policy.pending")


async def decide(ticket_id: str, decision: ApprovalDecision) -> None:
    """Resolve a ticket; an identical decision after an idempotent re-offer is a no-op."""

    del ticket_id, decision
    raise NotWiredError("omp.policy.decide")


__all__ = (
    "APPROVAL_DEADLINE", "Access", "Amend", "AndOrOp", "ApprovalDecision", "ApprovalSource", "ApprovalTicket",
    "BASH_IR_MAX_DEPTH", "BASH_IR_MAX_NODES", "BASH_IR_MAX_SOURCE", "BASH_IR_REV", "BashAndOrList", "BashArg",
    "BashAssignment", "BashCommandIR", "BashCompound", "BashFunctionDef", "BashIR", "BashNode", "BashPipeline",
    "BashRedirect", "BashTestExpr", "CompoundKind", "DnsPolicy", "DomainRule", "Dynamism", "EnforcementUnavailable",
    "ExecPolicy", "FilesystemGrade", "FilesystemPolicy", "HereDoc", "NetDirection", "NetKind", "NetRef", "NetworkGrade",
    "NetworkMode", "NetworkPolicy", "OpaqueEvaluator", "OpaqueReason", "POLICY_DEADLINE", "ParseError", "ParseFailure",
    "PathOrigin", "PathRef", "PathRule", "PolicyDenied", "PolicyError", "ProcessGrade", "ProcessSubDirection", "ProcessSubIR",
    "ProfileHandle", "ProfileRejected", "ProfileWidened", "Quoting", "RedirectOp", "RedirectTarget", "ResourceBudget", "RuleEffect",
    "RuleRef", "SandboxBackend", "SandboxCapabilities", "SandboxEnforcement", "SandboxMode", "SandboxProfile", "SandboxRequest",
    "Separator", "SandboxSessionKind", "Span", "TicketState", "Tier", "VIOLATION_COALESCE", "Violation", "ViolationKind",
    "amend", "approver", "capabilities", "decide", "effective_profile", "enforcement", "install", "match_paths", "parse", "pending",
)
