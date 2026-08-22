//! Canonical journal-backed persistence for autonomous application modes.

use omp_agent::{ControlError, ControlSender, Journal, JournalError};
use omp_core::{ArtifactDigest, Principal, Provenance, Str, sf};
use omp_storage::state::{DurableRequest, Error as StateError, GenerationFence, StateAuthority};
use thiserror::Error;
use tokio::sync::oneshot;

use super::ModeProjection;

const NAMESPACE: &str = "omp.auto-modes";
const KEY: &str = "execution-modes";

/// Journal-backed mode persistence failure.
#[derive(Debug, Error)]
pub enum ModePersistenceError {
	/// Core state authority construction failed.
	#[error(transparent)]
	State(#[from] StateError),
	/// Initial journal projection failed.
	#[error(transparent)]
	Journal(#[from] JournalError),
	/// Journal-owner control failed.
	#[error(transparent)]
	Control(#[from] ControlError),
	/// Projection serialization failed.
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	/// The persistence actor stopped.
	#[error("autonomous mode persistence actor is unavailable")]
	Closed,
	/// The synchronous UI producer outran durable journal acknowledgement.
	#[error("autonomous mode persistence queue is full")]
	Backpressure,
}

enum Command {
	Store {
		projection: ModeProjection,
		ack:        Option<oneshot::Sender<Result<(), ModePersistenceError>>>,
	},
}

/// Cloneable, ordered writer for the session's autonomous mode projection.
#[derive(Clone)]
pub struct ModePersistence {
	sender: flume::Sender<Command>,
}

impl std::fmt::Debug for ModePersistence {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("ModePersistence")
			.finish_non_exhaustive()
	}
}

impl ModePersistence {
	/// Loads the latest projection before the agent takes sole mutable journal
	/// ownership, then prepares the ordered writer actor.
	pub fn open(
		journal: &Journal,
		control: ControlSender,
		session: &str,
		project: &str,
	) -> Result<(Self, Option<ModeProjection>), ModePersistenceError> {
		let generation = GenerationFence { host: 0, session: 0 };
		let authority = authority(session, project, generation)?;
		let current = journal.latest_session_state(&authority, KEY)?;
		let revision = current.as_ref().map(|value| value.revision);
		let projection = current
			.map(|value| serde_json::from_str(value.value.get()))
			.transpose()?;
		let (sender, receiver) = flume::bounded(32);
		drop(tokio::spawn(run(receiver, control, authority, revision)));
		Ok((Self { sender }, projection))
	}

	/// Queues the newest projection without blocking a synchronous UI callback.
	pub fn store(&self, projection: ModeProjection) -> Result<(), ModePersistenceError> {
		self
			.sender
			.try_send(Command::Store { projection, ack: None })
			.map_err(|error| match error {
				flume::TrySendError::Full(_) => ModePersistenceError::Backpressure,
				flume::TrySendError::Disconnected(_) => ModePersistenceError::Closed,
			})
	}

	/// Persists a projection and waits for the sole journal owner to acknowledge
	/// it. Tool lifecycle operations use this before returning success.
	pub async fn flush(&self, projection: ModeProjection) -> Result<(), ModePersistenceError> {
		let (ack, response) = oneshot::channel();
		self
			.sender
			.send_async(Command::Store { projection, ack: Some(ack) })
			.await
			.map_err(|_| ModePersistenceError::Closed)?;
		response.await.map_err(|_| ModePersistenceError::Closed)?
	}
}

async fn run(
	receiver: flume::Receiver<Command>,
	control: ControlSender,
	authority: StateAuthority,
	mut revision: Option<omp_storage::state::StateRevision>,
) {
	while let Ok(command) = receiver.recv_async().await {
		let Command::Store { projection, ack } = command;
		let result = store_one(&control, &authority, revision, projection).await;
		if let Ok(value) = &result {
			revision = Some(value.revision);
		}
		let failed = result.is_err();
		if let Some(ack) = ack {
			let _ = ack.send(result.map(|_| ()));
		}
		if failed {
			break;
		}
	}
}

async fn store_one(
	control: &ControlSender,
	authority: &StateAuthority,
	revision: Option<omp_storage::state::StateRevision>,
	projection: ModeProjection,
) -> Result<omp_agent::SessionStateValue, ModePersistenceError> {
	let request_id = sf!("mode-{}", omp_core::Ulid::generate());
	let request = DurableRequest::new(request_id.clone(), Some(request_id), authority.generation())?;
	let value = serde_json::value::to_raw_value(&projection)?;
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX);
	Ok(control
		.session_state_compare_exchange(now, authority.clone(), sf!(KEY), revision, value, request)
		.await?)
}

fn authority(
	session: &str,
	project: &str,
	generation: GenerationFence,
) -> Result<StateAuthority, StateError> {
	let namespace = sf!(NAMESPACE);
	let provenance = Provenance::new(
		sf!("omp"),
		namespace.clone(),
		sf!(env!("CARGO_PKG_VERSION")),
		ArtifactDigest::new([0; 32]),
		sf!("builtin"),
		sf!("core"),
		generation.host,
	);
	StateAuthority::new_core(
		Principal::new(sf!("local"), sf!("Local user")),
		provenance,
		namespace,
		Str::new(session),
		Str::new(project),
		generation,
	)
}
