//! Verifies environment protocol envelopes preserve effect identity and
//! privileged attribution.

use omp_proto::{
	env::v1::{
		DataRequest, PrivilegedMutationIntent, PrivilegedWriteIntent, ResourceCompleteRequest,
		ResourceOp, data_request, privileged_mutation_intent, resource_op,
	},
	identity::v1::{EffectEnvelope, EffectId, GenerationId, SessionId, ToolId},
	prost::Message,
};

#[test]
fn shared_effect_identity_round_trips_epoch_and_declaration_revision() {
	let envelope = EffectEnvelope {
		effect: Some(EffectId { value: b"effect-7".as_ref().into() }),
		session: Some(SessionId { value: b"session-a".as_ref().into() }),
		tool: Some(ToolId { name: "write".into(), revision: "3".into() }),
		generation: Some(GenerationId { epoch: b"host".as_ref().into(), sequence: 9 }),
		wire_revision: 9,
		..EffectEnvelope::default()
	};
	let encoded = envelope.encode_to_vec();
	assert_eq!(EffectEnvelope::decode(encoded.as_slice()).unwrap(), envelope);
}

#[test]
fn data_envelope_preserves_privileged_attribution_and_stream_bounds() {
	let privileged = DataRequest {
		body: Some(data_request::Body::PrivilegedMutation(PrivilegedMutationIntent {
			mutation:        Some(privileged_mutation_intent::Mutation::Write(
				PrivilegedWriteIntent {
					canonical_target_uri: "file:///workspace/src/lib.rs".into(),
					expected_presence: 1,
					content: b"fn main() {}".as_ref().into(),
					..PrivilegedWriteIntent::default()
				},
			)),
			invocation_id:   "invoke-1".into(),
			session:         Some(SessionId { value: b"session-a".as_ref().into() }),
			approval_ticket: b"ticket".as_ref().into(),
			effect:          Some(EffectId { value: b"effect-7".as_ref().into() }),
			wire_revision:   8,
		})),
		..DataRequest::default()
	};
	let decoded = DataRequest::decode(privileged.encode_to_vec().as_slice()).unwrap();
	assert_eq!(decoded, privileged);

	let completion = DataRequest {
		body: Some(data_request::Body::Resource(ResourceOp {
			op: Some(resource_op::Op::Complete(ResourceCompleteRequest {
				input:            "skill://pro".into(),
				max_results:      12,
				catalog_revision: 44,
				wire_revision:    8,
			})),
		})),
		..DataRequest::default()
	};
	let decoded = DataRequest::decode(completion.encode_to_vec().as_slice()).unwrap();
	assert_eq!(decoded, completion);
}
