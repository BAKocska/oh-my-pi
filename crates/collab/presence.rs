//! Presentation-neutral live-room presence and transcript attribution facts.

use omp_core::Str;

/// Local participant role in a live collaboration room.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollabRole {
	/// This process owns the authoritative session.
	Host,
	/// This process renders a remote replica.
	Guest,
}

/// User-visible relay/session connection state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
	/// Initial relay connection or guest snapshot is in progress.
	Connecting,
	/// The room is live and synchronized.
	Connected,
	/// A transient relay drop is reconnecting.
	Reconnecting,
	/// The room has ended or failed terminally.
	Disconnected,
}

/// Stable facts consumed by status bars and `/collab status` renderers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresenceFacts {
	role:              CollabRole,
	connection:        ConnectionState,
	participant_count: usize,
	read_only:         bool,
}

impl PresenceFacts {
	/// Constructs host facts. The host is included in the participant count.
	#[must_use]
	pub const fn host(connection: ConnectionState, connected_peers: usize) -> Self {
		Self {
			role: CollabRole::Host,
			connection,
			participant_count: connected_peers.saturating_add(1),
			read_only: false,
		}
	}

	/// Constructs guest facts from the host-published total participant count.
	#[must_use]
	pub const fn guest(
		connection: ConnectionState,
		participant_count: usize,
		read_only: bool,
	) -> Self {
		Self { role: CollabRole::Guest, connection, participant_count, read_only }
	}

	/// Returns the local role.
	#[must_use]
	pub const fn role(self) -> CollabRole {
		self.role
	}

	/// Returns the current connection state.
	#[must_use]
	pub const fn connection(self) -> ConnectionState {
		self.connection
	}

	/// Returns every participant, including the host.
	#[must_use]
	pub const fn participant_count(self) -> usize {
		self.participant_count
	}

	/// Reports whether local mutation controls must be disabled.
	#[must_use]
	pub const fn read_only(self) -> bool {
		self.read_only
	}
}

/// Authenticated remote prompt attribution retained by transcript projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestAuthorFacts {
	/// Sanitized name authenticated by the host handshake.
	pub display_name: Str,
	/// Relay peer identity for stable same-name disambiguation.
	pub peer_id:      u32,
	/// Whether this peer joined with viewer credentials.
	pub read_only:    bool,
}

/// Participant lifecycle notice rendered by retained TML with a generic text
/// fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresenceNotice {
	/// One authenticated participant joined.
	Joined(GuestAuthorFacts),
	/// One authenticated participant left.
	Left(GuestAuthorFacts),
	/// The relay connection dropped transiently.
	Reconnecting,
	/// The room authority ended the session.
	Ended,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn host_count_always_includes_the_owner() {
		assert_eq!(PresenceFacts::host(ConnectionState::Connected, 2).participant_count(), 3,);
	}

	#[test]
	fn read_only_is_a_guest_fact() {
		let facts = PresenceFacts::guest(ConnectionState::Connected, 4, true);
		assert_eq!(facts.role(), CollabRole::Guest);
		assert!(facts.read_only());
	}
}
