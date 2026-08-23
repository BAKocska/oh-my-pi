//! Focused round-trip coverage for typed hook and approval journal kinds.

use omp_core::{Str, sf};

use super::super::{
	ApprovalDecided, ApprovalReason, ApprovalTicketFiled, Event, HookOutcome, Kind, PolicyDecision,
	read_line, write_line,
};

fn round_trip(kind: Kind) {
	let event = Event { ts: 7, kind };
	let mut line = Vec::new();
	write_line(&event, &mut line).unwrap();
	assert_eq!(read_line(&line).unwrap(), event);
}

#[test]
fn hook_policy_and_approval_kinds_round_trip() {
	round_trip(Kind::HookOutcome(HookOutcome {
		invocation_id:   Some(sf!("invoke")),
		event_id:        23,
		dispatch_id:     9,
		subscription_id: Some(2),
		phase:           3,
		decision:        sf!("Deny"),
		reason:          Some(sf!("rule")),
	}));
	round_trip(Kind::PolicyDecision(PolicyDecision {
		invocation_id:       sf!("invoke"),
		requested_target:    sf!("bash"),
		requested_args:      Str::new_static("{}"),
		transformations:     vec![sf!("replace")],
		effective_target:    sf!("bash"),
		effective_args:      Str::new_static("{}"),
		derived_ir_revision: 1,
		allowed:             true,
		reason:              None,
	}));
	round_trip(Kind::ApprovalTicketFiled(ApprovalTicketFiled {
		ticket_id:     sf!("ticket"),
		invocation_id: Some(sf!("invoke")),
		reasons:       vec![ApprovalReason {
			title:         sf!("Run"),
			body:          sf!("run"),
			subject:       sf!("ls"),
			kind:          sf!("exec"),
			scopes:        vec![sf!("once")],
			default:       None,
			route:         sf!("local"),
			approver:      None,
			timeout_ms:    1,
			unreachable:   sf!("fail_closed"),
			require_human: false,
			pattern:       None,
			evidence:      vec![sf!("rule")],
		}],
		created_at_ms: 7,
	}));
	round_trip(Kind::ApprovalDecided(ApprovalDecided {
		ticket_id:  sf!("ticket"),
		state:      sf!("decided"),
		approved:   Some(true),
		scope:      Some(sf!("once")),
		source:     Some(sf!("user")),
		decided_by: None,
		reason:     None,
		audited:    false,
	}));
}
