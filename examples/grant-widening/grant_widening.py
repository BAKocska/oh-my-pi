"""Turn one exact denied note write into an externally approved session grant."""

from __future__ import annotations

import omp


_APPROVER_NAME = "grant-widening"
_NOTE_FILE = "assistant-notes.jsonl"
_EVIDENCE_MARKER = "grant-widening:workspace-session-note"
_GRANT_SCOPES = (omp.PolicyScope.ONCE, omp.PolicyScope.SESSION)


def _is_session_note(subject: str) -> bool:
    """Accept only the dedicated note file below an `.omp/session-notes` directory."""

    parts = tuple(part for part in subject.replace("\\", "/").split("/") if part)
    return (
        ".." not in parts
        and len(parts) >= 3
        and parts[-3:] == (".omp", "session-notes", _NOTE_FILE)
    )


def _approval_for(violation: omp.Violation) -> omp.ApprovalSpec:
    """Bind the widening request to the exact path that enforcement denied."""

    evidence = (_EVIDENCE_MARKER,)
    if violation.rule is not None:
        evidence += (f"sandbox-rule:{violation.rule}",)
    return omp.ApprovalSpec(
        title="Allow session note writes",
        body=(
            "The notes writer requested one exact denied file. Approval grants "
            "write/create access to that file for this session."
        ),
        subject=violation.subject,
        kind=omp.ApprovalKind.WRITE,
        scopes=_GRANT_SCOPES,
        route=omp.ApprovalRoute.EXTERNAL,
        approver=_APPROVER_NAME,
        timeout=omp.Duration("2m"),
        unreachable=omp.Unreachable.FAIL_CLOSED,
        pattern=violation.subject,
        evidence=evidence,
    )


def _ticket_requests_session_note(ticket: omp.ApprovalTicket) -> bool:
    """Recognize only tickets minted by this extension for one exact note path."""

    if ticket.state is not omp.TicketState.PENDING or len(ticket.reasons) != 1:
        return False
    reason = ticket.reasons[0]
    return (
        reason.approver == _APPROVER_NAME
        and reason.route is omp.ApprovalRoute.EXTERNAL
        and reason.kind is omp.ApprovalKind.WRITE
        and reason.scopes == _GRANT_SCOPES
        and reason.pattern == reason.subject
        and _EVIDENCE_MARKER in reason.evidence
        and _is_session_note(reason.subject)
    )


@omp.approver(
    _APPROVER_NAME,
    kinds=(omp.ApprovalKind.WRITE,),
    timeout=omp.Duration("2m"),
    unreachable=omp.Unreachable.FAIL_CLOSED,
)
async def approve_session_note(
    ticket: omp.ApprovalTicket, ctx: omp.Context
) -> omp.ApprovalDecision:
    """Approve the dedicated note path at session scope; deny every other ticket."""

    del ctx
    approved = _ticket_requests_session_note(ticket)
    return omp.ApprovalDecision(
        approved=approved,
        scope=omp.PolicyScope.SESSION if approved else omp.PolicyScope.ONCE,
        source=omp.ApprovalSource.EXTERNAL,
        decided_by=_APPROVER_NAME,
        reason=(
            "exact workspace session-note grant approved"
            if approved
            else "ticket is not the dedicated session-note request"
        ),
        audited=False,
    )


@omp.hook("sandbox_violation")
async def widen_session_note_grant(
    violation: omp.Violation, ctx: omp.Context
) -> omp.Amend | None:
    """Ask Core to widen one denied note path, then retry under the approved grant."""

    del ctx
    if (
        not violation.enforced
        or violation.kind not in (omp.ViolationKind.FS_WRITE, omp.ViolationKind.FS_CREATE)
        or not _is_session_note(violation.subject)
    ):
        return None

    return omp.Amend(
        patch=omp.SandboxProfile(
            filesystem=omp.FilesystemPolicy(
                allow_write=(
                    omp.PathRule(
                        path=violation.subject,
                        recursive=False,
                        create=True,
                        delete=False,
                    ),
                ),
            ),
            label="grant-widening/session-note",
        ),
        scope=omp.PolicyScope.SESSION,
        reason="permit the exact session note path after external approval",
        retry=True,
        approval=_approval_for(violation),
    )
