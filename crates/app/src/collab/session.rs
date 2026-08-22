//! Single runtime-owner command and presence authority for live collaboration.

use std::time::Duration;

use bytes::Bytes;
use flume::{Receiver, Sender};
use omp_collab::{
	codec::RelayRoute,
	crypto::{CryptoError, RoomKey, WriteToken},
	link::{CollabLink, HostedRoom, RelayEndpoint, WebEndpoint},
	presence::{ConnectionState, PresenceFacts},
	relay::{Handshake, RelayClient, RelayError, RelayInbound, RelayRole, SendDisposition},
};
use omp_core::Str;
use omp_proto::collab::v1::{Bye, Hello, PromptRequest, collab_frame};
use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle, time::error::Elapsed};

const COMMAND_CAPACITY: usize = 16;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const WELCOME_TIMEOUT: Duration = Duration::from_secs(30);

/// Validated options for starting an authoritative room.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostOptions {
	/// OMP-v1 relay origin.
	pub relay: RelayEndpoint,
	/// Browser UI origin used only to render fragment links.
	pub web:   WebEndpoint,
}

/// One operation serialized through the sole live collaboration owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollabOwnerCommand {
	/// Start hosting a writable room.
	Start(HostOptions),
	/// Render the read-only link for an existing hosted room.
	View,
	/// Return current role, connection, and participant facts.
	Status,
	/// End an authoritative hosted room.
	Stop,
	/// Join a parsed OMP-v1 room link under the resolved local identity.
	Join {
		/// Strictly parsed room link and credentials.
		link:         CollabLink,
		/// Trimmed setting/OS/fallback participant name.
		display_name: Str,
	},
	/// Submit a writable guest prompt through the host authority.
	Prompt {
		/// Prompt text after expanding staged text attachments.
		text:   Str,
		/// Bounded staged image attachments.
		images: Vec<RemoteImage>,
	},
	/// Leave a replica and restore the prior local session.
	Leave,
}

/// One remote prompt image loaded by the guest UI boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteImage {
	/// Exact image bytes.
	pub data:      Bytes,
	/// Detected media type.
	pub mime_type: Str,
}

/// Owner-produced result rendered by slash-command adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollabCommandResult {
	/// Current presence facts, absent after stop/leave.
	pub presence:      Option<PresenceFacts>,
	/// Writable compact room link when hosting.
	pub full_link:     Option<Str>,
	/// Read-only compact room link when hosting.
	pub view_link:     Option<Str>,
	/// Writable browser deep link when hosting.
	pub web_link:      Option<Str>,
	/// Read-only browser deep link when hosting.
	pub web_view_link: Option<Str>,
}

impl CollabCommandResult {
	/// Constructs an inactive result after stop or leave.
	pub const fn inactive() -> Self {
		Self {
			presence:      None,
			full_link:     None,
			view_link:     None,
			web_link:      None,
			web_view_link: None,
		}
	}
}

struct OwnerRequest {
	command: CollabOwnerCommand,
	reply:   Sender<Result<CollabCommandResult, CollabCommandFault>>,
}

/// Clone-cheap command/presence handle installed only when the production
/// collaboration owner is constructed.
#[derive(Clone)]
pub struct CollabCommandHandle {
	commands: Sender<OwnerRequest>,
	presence: watch::Receiver<Option<PresenceFacts>>,
}

impl CollabCommandHandle {
	/// Requests a serialized owner operation and awaits its settled result.
	pub async fn request(
		&self,
		command: CollabOwnerCommand,
	) -> Result<CollabCommandResult, CollabCommandFault> {
		let (reply, result) = flume::bounded(1);
		self
			.commands
			.send_async(OwnerRequest { command, reply })
			.await
			.map_err(|_| CollabCommandFault::OwnerStopped)?;
		result
			.recv_async()
			.await
			.map_err(|_| CollabCommandFault::OwnerStopped)?
	}

	/// Returns the most recently published role/connection/participant facts.
	pub fn presence(&self) -> Option<PresenceFacts> {
		*self.presence.borrow()
	}

	/// Subscribes to role and presence changes for command filtering and status
	/// rendering.
	pub fn subscribe_presence(&self) -> watch::Receiver<Option<PresenceFacts>> {
		self.presence.clone()
	}
}

/// Receiving half retained by the production host/guest lifecycle owner.
pub struct CollabSessionAuthority {
	commands: Receiver<OwnerRequest>,
	presence: watch::Sender<Option<PresenceFacts>>,
}

impl CollabSessionAuthority {
	/// Constructs the sole authority and its clone-cheap UI handle.
	pub fn new() -> (Self, CollabCommandHandle) {
		let (commands, requests) = flume::bounded(COMMAND_CAPACITY);
		let (presence, observed_presence) = watch::channel(None);
		(Self { commands: requests, presence }, CollabCommandHandle {
			commands,
			presence: observed_presence,
		})
	}

	/// Receives the next serialized owner request.
	pub async fn recv(&self) -> Result<CollabOwnerRequest, CollabCommandFault> {
		let request = self
			.commands
			.recv_async()
			.await
			.map_err(|_| CollabCommandFault::OwnerStopped)?;
		Ok(CollabOwnerRequest { command: request.command, reply: Some(request.reply) })
	}

	/// Atomically publishes role/connection/participant changes.
	pub fn publish_presence(&self, facts: Option<PresenceFacts>) {
		self.presence.send_replace(facts);
	}
}
/// Starts the native relay-backed command owner.
///
/// The returned task owns every active relay socket. Dropping all command
/// handles ends the loop and closes the current room.
pub fn spawn_session_owner(authority: CollabSessionAuthority) -> JoinHandle<()> {
	tokio::spawn(authority.run())
}

enum ActiveSession {
	Host { relay: RelayClient, _write_token: WriteToken, result: CollabCommandResult },
	Guest { relay: RelayClient, sequence: u64, result: CollabCommandResult },
}

impl ActiveSession {
	fn result(&self) -> &CollabCommandResult {
		match self {
			Self::Host { result, .. } | Self::Guest { result, .. } => result,
		}
	}

	async fn close(&mut self, reason: &'static str) -> Result<(), CollabCommandFault> {
		let relay = match self {
			Self::Host { relay, .. } | Self::Guest { relay, .. } => relay,
		};
		let frame = omp_proto::collab::v1::CollabFrame {
			protocol_revision: omp_collab::PROTOCOL_REVISION,
			sequence: 1,
			payload: Some(collab_frame::Payload::Bye(Bye { reason: reason.to_owned() })),
			..Default::default()
		};
		let _ = relay.send(RelayRoute { peer_id: 0 }, &frame).await?;
		relay.close().await?;
		Ok(())
	}
}

impl CollabSessionAuthority {
	async fn run(self) {
		let mut active = None;
		while let Ok(request) = self.recv().await {
			let clears_presence = matches!(
				request.command(),
				CollabOwnerCommand::Start(_) | CollabOwnerCommand::Join { .. }
			);
			let result = self.apply(request.command(), &mut active).await;
			if clears_presence && result.is_err() {
				self.publish_presence(None);
			}
			let _ = request.settle(result);
		}
		if let Some(mut session) = active {
			let _ = session.close("runtime stopped").await;
		}
	}

	async fn apply(
		&self,
		command: &CollabOwnerCommand,
		active: &mut Option<ActiveSession>,
	) -> Result<CollabCommandResult, CollabCommandFault> {
		match command {
			CollabOwnerCommand::Start(options) => {
				if active.is_some() {
					return Err(CollabCommandFault::AlreadyActive);
				}
				self.publish_presence(Some(PresenceFacts::host(ConnectionState::Connecting, 0)));
				let room = HostedRoom::generate(options.relay.clone())?;
				let full_link = Str::from(room.full.compact());
				let view_link = Str::from(room.view.compact());
				let web_link = Str::from(room.full.browser(&options.web));
				let web_view_link = Str::from(room.view.browser(&options.web));
				let mut relay = RelayClient::new(room.full.room_url(), RelayRole::Host, room.room_key)?;
				connect(&mut relay).await?;
				let presence = PresenceFacts::host(ConnectionState::Connected, 0);
				let result = CollabCommandResult {
					presence:      Some(presence),
					full_link:     Some(full_link),
					view_link:     Some(view_link),
					web_link:      Some(web_link),
					web_view_link: Some(web_view_link),
				};
				*active = Some(ActiveSession::Host {
					relay,
					_write_token: room.write_token,
					result: result.clone(),
				});
				self.publish_presence(Some(presence));
				Ok(result)
			},
			CollabOwnerCommand::View => match active {
				Some(ActiveSession::Host { result, .. }) => Ok(result.clone()),
				Some(ActiveSession::Guest { .. }) | None => Err(CollabCommandFault::NotHosting),
			},
			CollabOwnerCommand::Status => Ok(active
				.as_ref()
				.map_or_else(CollabCommandResult::inactive, |session| session.result().clone())),
			CollabOwnerCommand::Stop => {
				let Some(ActiveSession::Host { .. }) = active else {
					return Err(CollabCommandFault::NotHosting);
				};
				let mut session = active.take().expect("host matched above");
				session.close("host stopped").await?;
				self.publish_presence(None);
				Ok(CollabCommandResult::inactive())
			},
			CollabOwnerCommand::Join { link, display_name } => {
				if active.is_some() {
					return Err(CollabCommandFault::AlreadyActive);
				}
				self.publish_presence(Some(PresenceFacts::guest(
					ConnectionState::Connecting,
					0,
					link.credentials().is_read_only(),
				)));
				let key = RoomKey::from_bytes(*link.credentials().key())?;
				let write_token = link
					.credentials()
					.write_token()
					.map(|token| Bytes::copy_from_slice(token.as_bytes()));
				let mut relay = RelayClient::new(link.room_url(), RelayRole::Guest, key)?;
				connect(&mut relay).await?;
				let hello = Handshake::hello(1, Hello {
					protocol_revision: omp_collab::PROTOCOL_REVISION,
					display_name: display_name.to_string(),
					write_token,
					client_version: env!("CARGO_PKG_VERSION").to_owned(),
				});
				if relay.send(RelayRoute { peer_id: 0 }, &hello).await? != SendDisposition::Sent {
					return Err(CollabCommandFault::OutboundQueued);
				}
				let inbound = tokio::time::timeout(WELCOME_TIMEOUT, relay.receive())
					.await
					.map_err(|source| CollabCommandFault::WelcomeTimeout { source })??
					.ok_or(CollabCommandFault::UnexpectedWelcome)?;
				let RelayInbound::Frame(frame) = inbound else {
					return Err(CollabCommandFault::UnexpectedWelcome);
				};
				let mut handshake = Handshake::new(RelayRole::Guest);
				handshake.accept(&frame.frame)?;
				let Some(collab_frame::Payload::Welcome(welcome)) = frame.frame.payload else {
					return Err(CollabCommandFault::UnexpectedWelcome);
				};
				if welcome.read_only != link.credentials().is_read_only() {
					return Err(CollabCommandFault::CredentialTierMismatch);
				}
				let participant_count = welcome
					.initial_state
					.as_ref()
					.map_or(1, |state| state.participants.len().max(1));
				let presence = PresenceFacts::guest(
					ConnectionState::Connected,
					participant_count,
					welcome.read_only,
				);
				let result =
					CollabCommandResult { presence: Some(presence), ..CollabCommandResult::inactive() };
				*active = Some(ActiveSession::Guest { relay, sequence: 1, result: result.clone() });
				self.publish_presence(Some(presence));
				Ok(result)
			},
			CollabOwnerCommand::Prompt { text, images } => {
				let Some(ActiveSession::Guest { relay, sequence, result }) = active else {
					return Err(CollabCommandFault::NotGuest);
				};
				if result.presence.is_some_and(PresenceFacts::read_only) {
					return Err(CollabCommandFault::ReadOnly);
				}
				*sequence = sequence.saturating_add(1);
				let frame = omp_proto::collab::v1::CollabFrame {
					protocol_revision: omp_collab::PROTOCOL_REVISION,
					sequence: *sequence,
					payload: Some(collab_frame::Payload::Prompt(PromptRequest {
						text:   text.to_string(),
						images: images
							.iter()
							.map(|image| omp_proto::collab::v1::ImageAttachment {
								data:      image.data.clone(),
								mime_type: image.mime_type.to_string(),
							})
							.collect(),
					})),
					..Default::default()
				};
				if relay.send(RelayRoute { peer_id: 0 }, &frame).await? != SendDisposition::Sent {
					return Err(CollabCommandFault::OutboundQueued);
				}
				Ok(result.clone())
			},
			CollabOwnerCommand::Leave => {
				let Some(ActiveSession::Guest { .. }) = active else {
					return Err(CollabCommandFault::NotGuest);
				};
				let mut session = active.take().expect("guest matched above");
				session.close("guest left").await?;
				self.publish_presence(None);
				Ok(CollabCommandResult::inactive())
			},
		}
	}
}

async fn connect(relay: &mut RelayClient) -> Result<(), CollabCommandFault> {
	tokio::time::timeout(CONNECT_TIMEOUT, relay.connect())
		.await
		.map_err(|source| CollabCommandFault::ConnectTimeout { source })??;
	Ok(())
}

/// One owner request that must settle exactly once.
pub struct CollabOwnerRequest {
	command: CollabOwnerCommand,
	reply:   Option<Sender<Result<CollabCommandResult, CollabCommandFault>>>,
}

impl CollabOwnerRequest {
	/// Returns the requested operation.
	pub const fn command(&self) -> &CollabOwnerCommand {
		&self.command
	}

	/// Settles the waiting slash-command adapter.
	pub fn settle(
		mut self,
		result: Result<CollabCommandResult, CollabCommandFault>,
	) -> Result<(), CollabCommandFault> {
		self
			.reply
			.take()
			.expect("collaboration request reply is present until settlement")
			.send(result)
			.map_err(|_| CollabCommandFault::CallerStopped)
	}
}

/// Collaboration command authority failure.
#[derive(Debug, Error)]
pub enum CollabCommandFault {
	/// Production collaboration owner has stopped.
	#[error("collaboration runtime owner has stopped")]
	OwnerStopped,
	/// The requesting command surface disappeared before settlement.
	#[error("collaboration command caller has stopped")]
	CallerStopped,
	/// A host-only operation was requested while not hosting.
	#[error("no collaboration room is being hosted")]
	NotHosting,
	/// A leave operation was requested while not joined as a guest.
	#[error("not joined to a collaboration room")]
	NotGuest,
	/// A second room cannot replace an active host or guest implicitly.
	#[error("a collaboration room is already active")]
	AlreadyActive,
	/// Room cryptographic material could not be created or imported.
	#[error(transparent)]
	Crypto(#[from] CryptoError),
	/// Native relay transport failed.
	#[error(transparent)]
	Relay(#[from] RelayError),
	/// Initial relay connection exceeded the host/guest deadline.
	#[error("collaboration relay connection timed out")]
	ConnectTimeout {
		/// Timeout source.
		#[source]
		source: Elapsed,
	},
	/// Guest welcome progress exceeded its deadline.
	#[error("collaboration host welcome timed out")]
	WelcomeTimeout {
		/// Timeout source.
		#[source]
		source: Elapsed,
	},
	/// The relay produced a non-welcome item during guest handshake.
	#[error("collaboration host did not send the expected welcome")]
	UnexpectedWelcome,
	/// A connected outbound operation unexpectedly entered reconnect buffering.
	#[error("collaboration operation could not be sent on the connected relay")]
	OutboundQueued,
	/// Host welcome access tier disagreed with the supplied credential width.
	#[error("collaboration host returned a mismatched credential tier")]
	CredentialTierMismatch,
	/// Viewer credentials cannot submit prompts.
	#[error("this collaboration link is read-only")]
	ReadOnly,
}

#[cfg(test)]
mod tests {
	use omp_collab::presence::{ConnectionState, PresenceFacts};

	use super::*;

	#[tokio::test]
	async fn owner_request_settles_one_waiting_caller() {
		let (owner, handle) = CollabSessionAuthority::new();
		let caller = tokio::spawn({
			let handle = handle.clone();
			async move { handle.request(CollabOwnerCommand::Status).await }
		});
		let request = owner.recv().await.expect("request");
		assert!(matches!(request.command(), CollabOwnerCommand::Status));
		request
			.settle(Ok(CollabCommandResult::inactive()))
			.expect("settle");
		assert_eq!(
			caller.await.expect("caller task").expect("command"),
			CollabCommandResult::inactive(),
		);
	}

	#[test]
	fn presence_watch_is_authoritative() {
		let (owner, handle) = CollabSessionAuthority::new();
		let facts = PresenceFacts::host(ConnectionState::Connected, 2);
		owner.publish_presence(Some(facts));
		assert_eq!(handle.presence(), Some(facts));
	}
}
