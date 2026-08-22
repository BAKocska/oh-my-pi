//! Host-side peer authentication, visibility classification, and mutation
//! admission.

use omp_core::{CredentialTier, Hash32, RemotePrincipal, Str, sf};
use omp_proto::collab::v1::{
	AbortRequest, AgentCommand, Hello, PromptRequest, UiResponse, VisibilityClass as WireVisibility,
	collab_frame,
};
use thiserror::Error;

use crate::{PROTOCOL_REVISION, crypto::WriteToken};

const DISPLAY_NAME_MAX_CHARS: usize = 64;

/// Explicit host projection visibility; host-local facts never serialize to
/// peers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum VisibilityClass {
	/// Canonical public transcript content.
	PublicTranscript,
	/// Credential-free state used only for remote presentation.
	PublicPresentation,
	/// Host credentials, internals, advisors, raw providers, or local resources.
	HostLocal,
}

impl VisibilityClass {
	/// Converts a public class to the protobuf vocabulary.
	#[must_use]
	pub const fn to_wire(self) -> WireVisibility {
		match self {
			Self::PublicTranscript => WireVisibility::PublicTranscript,
			Self::PublicPresentation => WireVisibility::PublicPresentation,
			Self::HostLocal => WireVisibility::HostLocalOmitted,
		}
	}
}

/// Mutation action named in targeted read-only rejection frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum MutationAction {
	/// Submit a user prompt or images.
	Prompt,
	/// Interrupt the active host generation.
	Abort,
	/// Chat with, kill, or revive a visible agent.
	AgentCommand,
	/// Answer a host UI request.
	UiResponse,
}

/// Host-authenticated peer retained after a successful hello.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedPeer {
	principal: RemotePrincipal,
}

impl AuthenticatedPeer {
	/// Returns the immutable principal stamped onto admitted mutations.
	#[must_use]
	pub const fn principal(&self) -> &RemotePrincipal {
		&self.principal
	}

	/// Returns whether this peer is restricted to observation.
	#[must_use]
	pub fn read_only(&self) -> bool {
		!self.principal.may_mutate()
	}
}

/// Host-side credential authority for one encrypted collaboration room.
pub struct HostAdmission {
	room_id:     Str,
	write_token: WriteToken,
}

impl HostAdmission {
	/// Creates a room-scoped host admission authority.
	#[must_use]
	pub const fn new(room_id: Str, write_token: WriteToken) -> Self {
		Self { room_id, write_token }
	}

	/// Validates protocol version, sanitizes the peer name, and classifies
	/// credentials.
	pub fn authenticate(
		&self,
		peer_id: u32,
		hello: &Hello,
	) -> Result<AuthenticatedPeer, AdmissionError> {
		if hello.protocol_revision != PROTOCOL_REVISION {
			return Err(AdmissionError::ProtocolMismatch {
				expected: PROTOCOL_REVISION,
				actual:   hello.protocol_revision,
			});
		}
		let display_name = sanitize_display_name(hello.display_name.as_str(), peer_id);
		let writable = hello
			.write_token
			.as_deref()
			.is_some_and(|candidate| self.write_token.matches(candidate));
		let credential_tier = if writable {
			CredentialTier::FullAccess
		} else {
			CredentialTier::ReadOnly
		};
		let token_digest = writable.then(|| Hash32::sum(self.write_token.as_bytes()));
		Ok(AuthenticatedPeer {
			principal: RemotePrincipal::new(
				peer_id,
				display_name,
				credential_tier,
				self.room_id.clone(),
				token_digest,
			),
		})
	}

	/// Admits only authenticated writable mutation frames and stamps their
	/// principal.
	pub fn admit_mutation(
		&self,
		peer: &AuthenticatedPeer,
		payload: &collab_frame::Payload,
	) -> Result<AuthorizedMutation, AdmissionError> {
		let (action, operation) = match payload {
			collab_frame::Payload::Prompt(request) => {
				(MutationAction::Prompt, RemoteOperation::Prompt(Box::new(request.clone())))
			},
			collab_frame::Payload::Abort(request) => {
				(MutationAction::Abort, RemoteOperation::Abort(Box::new(request.clone())))
			},
			collab_frame::Payload::AgentCommand(request) => {
				(MutationAction::AgentCommand, RemoteOperation::AgentCommand(Box::new(request.clone())))
			},
			collab_frame::Payload::UiResponse(response) => {
				(MutationAction::UiResponse, RemoteOperation::UiResponse(Box::new(response.clone())))
			},
			_ => return Err(AdmissionError::NotMutation),
		};
		if !peer.principal.may_mutate() {
			return Err(AdmissionError::ReadOnly { action });
		}
		Ok(AuthorizedMutation { principal: peer.principal.clone(), operation })
	}
}

/// One foreign protobuf mutation admitted with its immutable remote principal.
#[derive(Clone, Debug)]
pub struct AuthorizedMutation {
	/// Authenticated peer facts carried through Core, Environment, and
	/// approvals.
	pub principal: RemotePrincipal,
	/// Requested operation; protobuf payloads are boxed because generated
	/// foreign messages can grow independently of this compact authorization
	/// envelope.
	pub operation: RemoteOperation,
}

/// Mutation operation accepted by host admission.
#[derive(Clone, Debug)]
pub enum RemoteOperation {
	/// Canonical remote user prompt and optional images.
	Prompt(Box<PromptRequest>),
	/// User interrupt request.
	Abort(Box<AbortRequest>),
	/// Visible-agent chat, kill, or revive request.
	AgentCommand(Box<AgentCommand>),
	/// Response to one active host UI request.
	UiResponse(Box<UiResponse>),
}

/// Host admission failure suitable for a targeted protocol error frame.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AdmissionError {
	/// Peer and host use incompatible OMP collaboration revisions.
	#[error("collaboration protocol mismatch: host speaks v{expected}, guest sent v{actual}")]
	ProtocolMismatch {
		/// Host protocol revision.
		expected: u32,
		/// Guest protocol revision.
		actual:   u32,
	},
	/// A non-mutation frame was presented to mutation admission.
	#[error("collaboration frame is not a guest mutation")]
	NotMutation,
	/// A read-only peer attempted a mutation.
	#[error("{action} is disabled on a read-only collaboration link")]
	ReadOnly {
		/// Rejected action.
		action: MutationAction,
	},
}

/// Classifies an agent registry row without exposing advisor identities.
#[must_use]
pub const fn registry_visibility(is_advisor: bool) -> VisibilityClass {
	if is_advisor {
		VisibilityClass::HostLocal
	} else {
		VisibilityClass::PublicPresentation
	}
}

/// Classifies EventBus channels; only the two task channels are peer-visible.
#[must_use]
pub fn bus_visibility(channel: i32) -> VisibilityClass {
	use omp_proto::collab::v1::bus_event::Channel;
	match Channel::try_from(channel) {
		Ok(Channel::TaskSubagentProgress | Channel::TaskSubagentLifecycle) => {
			VisibilityClass::PublicPresentation
		},
		_ => VisibilityClass::HostLocal,
	}
}

fn sanitize_display_name(name: &str, peer_id: u32) -> Str {
	let name = name.trim();
	if name.is_empty() {
		return sf!("guest-{peer_id}");
	}
	let end = name
		.char_indices()
		.nth(DISPLAY_NAME_MAX_CHARS)
		.map_or(name.len(), |(index, _)| index);
	Str::new(&name[..end])
}

#[cfg(test)]
mod tests {
	use super::*;

	fn admission() -> HostAdmission {
		HostAdmission::new(sf!("room"), WriteToken::from_bytes([9; 16]))
	}

	#[test]
	fn hello_classifies_timing_safe_credentials_and_sanitizes_name() {
		let authority = admission();
		let full = authority
			.authenticate(7, &Hello {
				protocol_revision: PROTOCOL_REVISION,
				display_name:      "  guest  ".to_owned(),
				write_token:       Some(vec![9; 16].into()),
				client_version:    String::new(),
			})
			.expect("authenticate");
		assert_eq!(full.principal().display_name(), "guest");
		assert_eq!(full.principal().credential_tier(), CredentialTier::FullAccess);
		assert!(full.principal().token_digest().is_some());
	}

	#[test]
	fn read_only_mutations_are_rejected_at_host_admission() {
		let authority = admission();
		let peer = authority
			.authenticate(2, &Hello {
				protocol_revision: PROTOCOL_REVISION,
				display_name:      String::new(),
				write_token:       None,
				client_version:    String::new(),
			})
			.expect("authenticate");
		let payload = collab_frame::Payload::Abort(AbortRequest { reason: String::new() });
		assert_eq!(
			authority.admit_mutation(&peer, &payload).unwrap_err(),
			AdmissionError::ReadOnly { action: MutationAction::Abort }
		);
	}
}
