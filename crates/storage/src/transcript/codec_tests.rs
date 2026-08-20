//! Focused round-trip coverage for typed hook and approval journal kinds.

#[cfg(test)]
mod tests {
	use omp_core::Str;

	use super::super::{
		ApprovalDecided, ApprovalReason, ApprovalTicketFiled, Event, HookOutcome, Kind,
		PolicyDecision, read_line, write_line,
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
			invocation_id:   Some(Str::new_static("invoke")),
			event_id:        23,
			dispatch_id:     9,
			subscription_id: Some(2),
			phase:           3,
			decision:        Str::new_static("Deny"),
			reason:          Some(Str::new_static("rule")),
		}));
		round_trip(Kind::PolicyDecision(PolicyDecision {
			invocation_id:       Str::new_static("invoke"),
			requested_target:    Str::new_static("bash"),
			requested_args:      Str::new_static("{}"),
			transformations:     vec![Str::new_static("replace")],
			effective_target:    Str::new_static("bash"),
			effective_args:      Str::new_static("{}"),
			derived_ir_revision: 1,
			allowed:             true,
			reason:              None,
		}));
		round_trip(Kind::ApprovalTicketFiled(ApprovalTicketFiled {
			ticket_id:     Str::new_static("ticket"),
			invocation_id: Some(Str::new_static("invoke")),
			reasons:       vec![ApprovalReason {
				title:         Str::new_static("Run"),
				body:          Str::new_static("run"),
				subject:       Str::new_static("ls"),
				kind:          Str::new_static("exec"),
				scopes:        vec![Str::new_static("once")],
				default:       None,
				route:         Str::new_static("local"),
				approver:      None,
				timeout_ms:    1,
				unreachable:   Str::new_static("fail_closed"),
				require_human: false,
				pattern:       None,
				evidence:      vec![Str::new_static("rule")],
			}],
			created_at_ms: 7,
		}));
		round_trip(Kind::ApprovalDecided(ApprovalDecided {
			ticket_id:  Str::new_static("ticket"),
			state:      Str::new_static("decided"),
			approved:   Some(true),
			scope:      Some(Str::new_static("once")),
			source:     Some(Str::new_static("user")),
			decided_by: None,
			reason:     None,
			audited:    false,
		}));
	}
}
