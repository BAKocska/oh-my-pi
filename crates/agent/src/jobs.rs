//! Detached-job registration and authoritative settlement delivery.

use std::{
	collections::{BTreeMap, BTreeSet, btree_map::Entry},
	sync::{Arc, Weak},
};

use bytes::Bytes;
use omp_core::{Duration, Str, fmts};
use omp_env::{EnvClient, ProcessAttachmentEvent};
use omp_proto::{
	blob::v1::Chunk,
	env::v1::{
		AttachOutput, ExecStatusMsg, ListProcesses, ProcessInfo, ProcessOutput, ProcessState,
		StopProcess,
	},
	thread::v1 as thread,
};
use omp_tool::{ArtifactLifetime, JobOwner, JobRef};
use parking_lot::{Mutex, MutexGuard};
use serde::Serialize;
use tokio::task::AbortHandle;

use crate::mailbox::{Interrupt, InterruptClass, InterruptSource, MailboxSender};

const SETTLEMENT_MEDIA_TYPE: &str = "application/vnd.omp.process-settlement+json";
const UPLOAD_CHUNK_BYTES: usize = 16 * 1024;

/// Thread-safe registry and structural supervisor for detached jobs.
///
/// The environment remains the resource owner. Each registration starts one
/// attachment watcher; dropping the last board handle aborts every watcher.
#[derive(Clone)]
pub struct JobBoard {
	inner: Arc<JobBoardInner>,
}

struct JobBoardInner {
	env:        EnvClient,
	mailbox:    MailboxSender,
	pending:    Mutex<BTreeMap<Str, JobEntry>>,
	watchers:   Mutex<BTreeMap<Str, AbortHandle>>,
	settled:    Mutex<BTreeSet<Str>>,
	generation: tokio::sync::watch::Sender<u64>,
}

struct JobEntry {
	job:          JobRef,
	settlement:   Option<thread::Item>,
	suppressions: usize,
	leased:       bool,
}

impl Drop for JobBoardInner {
	fn drop(&mut self) {
		for (_, watcher) in std::mem::take(self.watchers.get_mut()) {
			watcher.abort();
		}
	}
}

impl JobBoard {
	/// Creates an empty board over the authoritative environment client.
	pub fn new(env: EnvClient, mailbox: MailboxSender) -> Self {
		Self {
			inner: Arc::new(JobBoardInner {
				env,
				mailbox,
				pending: Mutex::new(BTreeMap::new()),
				watchers: Mutex::new(BTreeMap::new()),
				settled: Mutex::new(BTreeSet::new()),
				generation: tokio::sync::watch::channel(0).0,
			}),
		}
	}

	/// Registers and starts watching one detached job.
	///
	/// Returns `true` when inserted. An exact or conflicting duplicate stable ID
	/// returns `false` without replacing the first descriptor or watcher. This
	/// method must be called from a Tokio runtime.
	pub fn register(&self, job: JobRef) -> bool {
		let mut pending = self.inner.pending.lock();
		match pending.entry(job.id.clone()) {
			Entry::Vacant(entry) => {
				entry.insert(JobEntry {
					job:          job.clone(),
					settlement:   None,
					suppressions: 0,
					leased:       false,
				});
			},
			Entry::Occupied(_) => return false,
		}

		let id = job.id.clone();
		let registration_id = id.clone();
		let weak = Arc::downgrade(&self.inner);
		let env = self.inner.env.clone();
		let watcher = tokio::spawn(async move {
			let item = match watch_job(&env, &job).await {
				Ok(item) => item,
				Err(reason) => settlement_error_item(&job, &reason),
			};
			if let Some(inner) = weak.upgrade() {
				let _ = inner.complete(&id, item);
				inner.watchers.lock().remove(&id);
			}
		})
		.abort_handle();
		self.inner.watchers.lock().insert(registration_id, watcher);
		drop(pending);
		true
	}

	/// Settles a pending job with a caller-supplied canonical item.
	///
	/// This idempotent seam is used by authoritative settlement recovery and
	/// tests. Normal named-process settlement is produced by the board's
	/// watcher.
	pub fn settle(
		&self,
		job_id: &str,
		item: thread::Item,
	) -> Result<bool, Box<flume::TrySendError<Interrupt>>> {
		let accepted = self.inner.complete(job_id, item)?;
		if accepted && let Some(watcher) = self.inner.watchers.lock().remove(job_id) {
			watcher.abort();
		}
		Ok(accepted)
	}

	/// Copies pending descriptors in stable job-identifier order.
	#[must_use]
	pub fn snapshot(&self) -> Vec<JobRef> {
		self
			.inner
			.pending
			.lock()
			.values()
			.map(|entry| entry.job.clone())
			.collect()
	}

	/// Suppresses automatic delivery for selected jobs until a settlement is
	/// claimed or the returned watch is dropped.
	#[must_use]
	pub fn watch(&self, ids: Option<&[Str]>) -> JobWatch {
		let mut pending = self.inner.pending.lock();
		let selected = match ids {
			Some(ids) => ids
				.iter()
				.filter(|id| pending.contains_key(id.as_str()))
				.cloned()
				.collect::<BTreeSet<_>>(),
			None => pending.keys().cloned().collect(),
		};
		for id in &selected {
			if let Some(entry) = pending.get_mut(id) {
				entry.suppressions = entry.suppressions.saturating_add(1);
			}
		}
		drop(pending);
		JobWatch {
			inner:      Arc::clone(&self.inner),
			ids:        selected,
			generation: self.inner.generation.subscribe(),
		}
	}

	/// Stops the verified named process that owns a pending job.
	pub async fn cancel(&self, id: &str, grace: Duration) -> Result<CancelOutcome, JobError> {
		let job = {
			let pending = self.inner.pending.lock();
			let Some(entry) = pending.get(id) else {
				return Ok(if self.inner.settled.lock().contains(id) {
					CancelOutcome::AlreadySettled
				} else {
					CancelOutcome::Missing
				});
			};
			if entry.settlement.is_some() {
				return Ok(CancelOutcome::AlreadySettled);
			}
			entry.job.clone()
		};
		let JobOwner::NamedProcess { name, generation } = &job.owner;
		let processes = self
			.inner
			.env
			.list_processes(ListProcesses { props: None })
			.await
			.map_err(|error| JobError::Environment(Str::from(error.to_string())))?;
		let Some(process) = processes
			.processes
			.iter()
			.find(|process| process.name == name.as_str() && process.generation == *generation)
		else {
			return Ok(CancelOutcome::AlreadySettled);
		};
		if matches!(
			ProcessState::try_from(process.state),
			Ok(ProcessState::Exited | ProcessState::Stopped | ProcessState::Failed)
		) {
			return Ok(CancelOutcome::AlreadySettled);
		}
		let grace_ms = grace
			.to_std()
			.map_err(|error| JobError::InvalidGrace(Str::from(error.to_string())))?
			.as_millis()
			.try_into()
			.unwrap_or(u64::MAX);
		self
			.inner
			.env
			.stop_process(StopProcess { name: name.to_string(), grace_ms, props: None })
			.await
			.map_err(|error| JobError::Environment(Str::from(error.to_string())))?;
		Ok(CancelOutcome::Accepted)
	}

	/// Borrows pending jobs in stable identifier order without allocating.
	pub fn pending(&self) -> PendingJobs<'_> {
		PendingJobs { guard: self.inner.pending.lock() }
	}

	/// Returns the number of jobs awaiting settlement.
	pub fn len(&self) -> usize {
		self.inner.pending.lock().len()
	}

	/// Returns whether no jobs await settlement.
	pub fn is_empty(&self) -> bool {
		self.inner.pending.lock().is_empty()
	}
}

impl JobBoardInner {
	fn complete(
		&self,
		job_id: &str,
		item: thread::Item,
	) -> Result<bool, Box<flume::TrySendError<Interrupt>>> {
		let mut pending = self.pending.lock();
		let Some(entry) = pending.get_mut(job_id) else {
			return Ok(false);
		};
		if entry.settlement.is_some() {
			return Ok(false);
		}
		entry.settlement = Some(item);
		self.flush_locked(job_id, &mut pending)?;
		self.bump();
		Ok(true)
	}

	fn flush_locked(
		&self,
		job_id: &str,
		pending: &mut BTreeMap<Str, JobEntry>,
	) -> Result<(), Box<flume::TrySendError<Interrupt>>> {
		let Some(entry) = pending.get(job_id) else {
			return Ok(());
		};
		if entry.suppressions != 0 || entry.leased {
			return Ok(());
		}
		let Some(item) = entry.settlement.clone() else {
			return Ok(());
		};
		let id = entry.job.id.clone();
		self.mailbox.try_enqueue(Interrupt {
			class: InterruptClass::TurnBoundary,
			item,
			source: InterruptSource::Job { id: id.clone() },
		})?;
		pending.remove(job_id);
		self.settled.lock().insert(id);
		Ok(())
	}

	fn claim(&self, job_id: &str) -> Result<(), JobClaimError> {
		let mut pending = self.pending.lock();
		let Some(entry) = pending.get(job_id) else {
			return Err(JobClaimError::AlreadyConsumed);
		};
		if !entry.leased || entry.settlement.is_none() {
			return Err(JobClaimError::AlreadyConsumed);
		}
		let id = entry.job.id.clone();
		pending.remove(job_id);
		self.settled.lock().insert(id);
		drop(pending);
		self.bump();
		Ok(())
	}

	fn release_lease(&self, job_id: &str) {
		let mut pending = self.pending.lock();
		if let Some(entry) = pending.get_mut(job_id) {
			entry.leased = false;
		}
		let _ = self.flush_locked(job_id, &mut pending);
		drop(pending);
		self.bump();
	}

	fn release_watch(&self, ids: &BTreeSet<Str>) {
		let mut pending = self.pending.lock();
		for id in ids {
			if let Some(entry) = pending.get_mut(id) {
				entry.suppressions = entry.suppressions.saturating_sub(1);
			}
			let _ = self.flush_locked(id, &mut pending);
		}
		drop(pending);
		self.bump();
	}

	fn bump(&self) {
		let next = (*self.generation.borrow()).wrapping_add(1);
		self.generation.send_replace(next);
	}
}

/// Locked, allocation-free view of jobs awaiting settlement.
pub struct PendingJobs<'a> {
	guard: MutexGuard<'a, BTreeMap<Str, JobEntry>>,
}

impl PendingJobs<'_> {
	/// Iterates descriptors in stable job-identifier order.
	pub fn iter(&self) -> impl DoubleEndedIterator<Item = &JobRef> + ExactSizeIterator + Clone + '_ {
		self.guard.values().map(|entry| &entry.job)
	}

	/// Returns the number of jobs in this view.
	pub fn len(&self) -> usize {
		self.guard.len()
	}

	/// Returns whether this view contains no jobs.
	pub fn is_empty(&self) -> bool {
		self.guard.is_empty()
	}
}

/// Result of requesting cancellation for a detached job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelOutcome {
	/// No pending or settled job has this identifier.
	Missing,
	/// The job has already produced a terminal settlement.
	AlreadySettled,
	/// The authoritative environment accepted the stop request.
	Accepted,
}

/// Failure to inspect or stop a detached job.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum JobError {
	/// The configured courtesy grace cannot be represented by the runtime.
	#[error("invalid job cancellation grace: {0}")]
	InvalidGrace(Str),
	/// The environment rejected a process operation.
	#[error("job process operation failed: {0}")]
	Environment(Str),
}

/// Failure to atomically consume a watched settlement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum JobClaimError {
	/// Another consumer already delivered or claimed the settlement.
	#[error("job settlement was already consumed")]
	AlreadyConsumed,
}

/// One watched terminal settlement and its exclusive delivery lease.
pub struct JobSettlement {
	/// Stable detached-job descriptor.
	pub job:   JobRef,
	/// Canonical thread item produced by the settlement watcher.
	pub item:  thread::Item,
	/// Lease controlling whether normal mailbox delivery resumes.
	pub lease: SettlementLease,
}

/// Exclusive claim on one settlement held outside the board lock.
pub struct SettlementLease {
	inner:   Weak<JobBoardInner>,
	job_id:  Str,
	claimed: bool,
}

impl SettlementLease {
	/// Atomically consumes the settlement without mailbox auto-delivery.
	pub fn claim(mut self) -> Result<(), JobClaimError> {
		let inner = self.inner.upgrade().ok_or(JobClaimError::AlreadyConsumed)?;
		inner.claim(self.job_id.as_str())?;
		self.claimed = true;
		Ok(())
	}
}

impl Drop for SettlementLease {
	fn drop(&mut self) {
		if !self.claimed
			&& let Some(inner) = self.inner.upgrade()
		{
			inner.release_lease(self.job_id.as_str());
		}
	}
}

/// Settlement subscription which temporarily suppresses normal delivery.
pub struct JobWatch {
	inner:      Arc<JobBoardInner>,
	ids:        BTreeSet<Str>,
	generation: tokio::sync::watch::Receiver<u64>,
}

impl JobWatch {
	/// Returns whether no selected pending job remains.
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.ids.is_empty()
	}

	/// Waits for the next selected settlement, retaining unrelated jobs.
	pub async fn next(&mut self) -> Option<JobSettlement> {
		loop {
			let selected = {
				let mut pending = self.inner.pending.lock();
				let id = self.ids.iter().find_map(|id| {
					pending
						.get(id)
						.filter(|entry| entry.settlement.is_some() && !entry.leased)
						.map(|_| id.clone())
				});
				id.and_then(|id| {
					let entry = pending.get_mut(&id)?;
					entry.leased = true;
					entry.suppressions = entry.suppressions.saturating_sub(1);
					Some((id, entry.job.clone(), entry.settlement.clone()?))
				})
			};
			if let Some((id, job, item)) = selected {
				self.ids.remove(&id);
				return Some(JobSettlement {
					job,
					item,
					lease: SettlementLease {
						inner:   Arc::downgrade(&self.inner),
						job_id:  id,
						claimed: false,
					},
				});
			}
			self
				.ids
				.retain(|id| self.inner.pending.lock().contains_key(id));
			if self.ids.is_empty() || self.generation.changed().await.is_err() {
				return None;
			}
		}
	}
}

impl Drop for JobWatch {
	fn drop(&mut self) {
		self.inner.release_watch(&self.ids);
	}
}

async fn watch_job(env: &EnvClient, job: &JobRef) -> Result<thread::Item, Str> {
	let JobOwner::NamedProcess { name, generation } = &job.owner;
	let mut attachment = env
		.attach_output(AttachOutput {
			name:           name.to_string(),
			after_sequence: 0,
			props:          None,
		})
		.await
		.map_err(|error| fmts!("could not attach to named process: {error}"))?;
	let attached = match attachment
		.next_event()
		.await
		.map_err(|error| fmts!("named-process attachment failed: {error}"))?
	{
		Some(ProcessAttachmentEvent::Attached(attached)) => attached,
		Some(_) => return Err(Str::from("named-process attachment omitted acknowledgement")),
		None => return Err(Str::from("named-process attachment closed before acknowledgement")),
	};
	if attached.name != name.as_str() || attached.generation != *generation {
		return Err(fmts!(
			"named-process attachment generation mismatch: expected {name}@{generation}, got {}@{}",
			attached.name,
			attached.generation
		));
	}

	let upload = env
		.blob_put()
		.map_err(|error| fmts!("could not open settlement artifact upload: {error}"))?;
	let mut header = serde_json::to_vec(&ArtifactHeader {
		job_id:            job.id.as_str(),
		owner:             OwnerRecord { name: name.as_str(), generation: *generation },
		expected_artifact: ExpectedArtifactRecord {
			description: job.artifact.description.as_str(),
			media_type:  job.artifact.media_type.as_deref(),
			lifetime:    job.artifact.lifetime,
		},
	})
	.map_err(|error| fmts!("could not encode settlement header: {error}"))?;
	if header.pop() != Some(b'}') {
		return Err(Str::from("settlement header was not a JSON object"));
	}
	header.extend_from_slice(b",\"output\":[");
	upload_bytes(&upload, &header).await?;
	let mut first_output = true;

	loop {
		let event = attachment
			.next_event()
			.await
			.map_err(|error| fmts!("named-process attachment failed: {error}"))?
			.ok_or_else(|| Str::from("named-process attachment closed before terminal state"))?;
		match event {
			ProcessAttachmentEvent::Attached(_) => {
				return Err(Str::from("named-process attachment repeated acknowledgement"));
			},
			ProcessAttachmentEvent::Output(output) => {
				validate_output(&output, name, *generation)?;
				let mut encoded = serde_json::to_vec(&OutputRecord {
					sequence: output.sequence,
					channel:  output.channel,
					data:     &output.data,
				})
				.map_err(|error| fmts!("could not encode process output: {error}"))?;
				if !first_output {
					encoded.insert(0, b',');
				}
				first_output = false;
				upload_bytes(&upload, &encoded).await?;
			},
			ProcessAttachmentEvent::State(state) => {
				let info = state
					.process
					.ok_or_else(|| Str::from("named-process state omitted process info"))?;
				validate_state(&info, name, *generation)?;
				if terminal_state(&info) {
					return finish_settlement(upload, job, info).await;
				}
			},
		}
	}
}

async fn finish_settlement(
	upload: omp_env::BlobUpload,
	job: &JobRef,
	info: ProcessInfo,
) -> Result<thread::Item, Str> {
	let mut suffix = Vec::from(&b"],\"state\":"[..]);
	serde_json::to_writer(&mut suffix, &StateRecord::from(&info))
		.map_err(|error| fmts!("could not encode terminal process state: {error}"))?;
	suffix.push(b'}');
	upload_bytes(&upload, &suffix).await?;
	let stored = upload
		.commit()
		.await
		.map_err(|error| fmts!("could not commit settlement artifact: {error}"))?;
	let state = ProcessState::try_from(info.state)
		.map_or_else(|_| format!("state {}", info.state), |state| format!("{state:?}"));
	let text = format!("Detached job {} settled: {}.", job.id, state.to_lowercase());
	let mime = SETTLEMENT_MEDIA_TYPE.to_owned();
	Ok(system_item(vec![
		thread::Part { kind: Some(thread::part::Kind::Text(text)) },
		thread::Part {
			kind: Some(thread::part::Kind::Blob(thread::Blob {
				hash: stored.hash,
				mime,
				size: stored.size,
				inline: Bytes::new(),
				detail: thread::blob::Detail::Auto as i32,
			})),
		},
	]))
}

async fn upload_bytes(upload: &omp_env::BlobUpload, bytes: &[u8]) -> Result<(), Str> {
	for data in bytes.chunks(UPLOAD_CHUNK_BYTES) {
		upload
			.send_chunk(Chunk { data: Bytes::copy_from_slice(data), hash: Bytes::new(), size: None })
			.await
			.map_err(|error| fmts!("could not stream settlement artifact: {error}"))?;
	}
	Ok(())
}

fn validate_output(output: &ProcessOutput, name: &str, generation: u64) -> Result<(), Str> {
	if output.name == name && output.generation == generation {
		Ok(())
	} else {
		Err(fmts!(
			"named-process output generation mismatch: expected {name}@{generation}, got {}@{}",
			output.name,
			output.generation
		))
	}
}

fn validate_state(info: &ProcessInfo, name: &str, generation: u64) -> Result<(), Str> {
	if info.name == name && info.generation == generation {
		Ok(())
	} else {
		Err(fmts!(
			"named-process state generation mismatch: expected {name}@{generation}, got {}@{}",
			info.name,
			info.generation
		))
	}
}

fn terminal_state(info: &ProcessInfo) -> bool {
	matches!(
		ProcessState::try_from(info.state).ok(),
		Some(ProcessState::Exited | ProcessState::Stopped | ProcessState::Failed)
	)
}

fn settlement_error_item(job: &JobRef, reason: &str) -> thread::Item {
	system_item(vec![thread::Part {
		kind: Some(thread::part::Kind::Text(format!(
			"Detached job {} could not be observed to settlement: {reason}",
			job.id
		))),
	}])
}

const fn system_item(parts: Vec<thread::Part>) -> thread::Item {
	thread::Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(thread::item::Kind::Message(thread::Message {
			role: thread::Role::System as i32,
			parts,
		})),
		props:         None,
	}
}

#[derive(Serialize)]
struct ArtifactHeader<'a> {
	job_id:            &'a str,
	owner:             OwnerRecord<'a>,
	expected_artifact: ExpectedArtifactRecord<'a>,
}

#[derive(Serialize)]
struct OwnerRecord<'a> {
	name:       &'a str,
	generation: u64,
}

#[derive(Serialize)]
struct ExpectedArtifactRecord<'a> {
	description: &'a str,
	media_type:  Option<&'a str>,
	lifetime:    ArtifactLifetime,
}

#[derive(Serialize)]
struct OutputRecord<'a> {
	sequence: u64,
	channel:  i32,
	data:     &'a [u8],
}

#[derive(Serialize)]
struct StateRecord<'a> {
	state:  i32,
	status: Option<StatusRecord<'a>>,
}

impl<'a> From<&'a ProcessInfo> for StateRecord<'a> {
	fn from(info: &'a ProcessInfo) -> Self {
		Self { state: info.state, status: info.status.as_ref().map(StatusRecord::from) }
	}
}

#[derive(Serialize)]
struct StatusRecord<'a> {
	outcome:       i32,
	exit_code:     Option<i32>,
	signal:        &'a str,
	wall_clock_ms: u64,
	aborted:       bool,
}

impl<'a> From<&'a ExecStatusMsg> for StatusRecord<'a> {
	fn from(status: &'a ExecStatusMsg) -> Self {
		Self {
			outcome:       status.outcome,
			exit_code:     status.exit_code,
			signal:        status.signal.as_str(),
			wall_clock_ms: status.wall_clock_ms,
			aborted:       status.aborted,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{
		sync::atomic::{AtomicUsize, Ordering},
		thread as std_thread,
	};

	use omp_tool::{ArtifactLifetime, ExpectedArtifact};

	use super::*;
	use crate::mailbox::{DrainPoint, Mailbox};

	fn job(id: &str, lifetime: ArtifactLifetime) -> JobRef {
		JobRef {
			id:       Str::from(id),
			owner:    JobOwner::NamedProcess { name: Str::from(id), generation: 1 },
			artifact: ExpectedArtifact {
				description: Str::from("detached output"),
				media_type: None,
				lifetime,
			},
		}
	}

	#[tokio::test]
	async fn pending_view_is_stable_and_duplicates_preserve_the_first_descriptor() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(job("job-b", ArtifactLifetime::Durable)));
		assert!(board.register(job("job-a", ArtifactLifetime::Session)));
		assert!(!board.register(job("job-a", ArtifactLifetime::Ephemeral)));

		let pending = board.pending();
		assert_eq!(pending.len(), 2);
		let mut jobs = pending.iter();
		assert_eq!(jobs.next().unwrap().id, "job-a");
		assert_eq!(jobs.next().unwrap().id, "job-b");
		assert_eq!(jobs.next(), None);
		assert_eq!(pending.iter().next().unwrap().artifact.lifetime, ArtifactLifetime::Session);
	}

	#[tokio::test]
	async fn concurrent_settlement_enqueues_once_and_removes_pending_state() {
		let mut mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(job("job-1", ArtifactLifetime::Session)));
		assert!(!board.settle("unknown", thread::Item::default()).unwrap());
		let settled = AtomicUsize::new(0);
		std_thread::scope(|scope| {
			for seq in 0..8 {
				let board = &board;
				let settled = &settled;
				scope.spawn(move || {
					if board
						.settle("job-1", thread::Item { seq, ..thread::Item::default() })
						.unwrap()
					{
						settled.fetch_add(1, Ordering::Relaxed);
					}
				});
			}
		});

		assert_eq!(settled.load(Ordering::Relaxed), 1);
		assert!(board.is_empty());
		assert_eq!(mailbox.len(), 1);
		let interrupts = mailbox.drain(DrainPoint::TurnBoundary, false);
		assert_eq!(interrupts.len(), 1);
		assert_eq!(interrupts[0].class, InterruptClass::TurnBoundary);
		assert_eq!(interrupts[0].source, InterruptSource::Job { id: Str::from("job-1") });
		assert!(!board.settle("job-1", thread::Item::default()).unwrap());
		assert!(mailbox.is_empty());
	}
}

#[cfg(test)]
mod watch_tests {
	use omp_tool::{ArtifactLifetime, ExpectedArtifact};

	use super::*;
	use crate::mailbox::Mailbox;

	fn watched_job(id: &str) -> JobRef {
		JobRef {
			id:       Str::from(id),
			owner:    JobOwner::NamedProcess { name: Str::from(id), generation: 1 },
			artifact: ExpectedArtifact {
				description: Str::from("detached output"),
				media_type:  None,
				lifetime:    ArtifactLifetime::Session,
			},
		}
	}

	#[tokio::test]
	async fn claimed_watch_settlement_suppresses_mailbox_delivery() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(watched_job("claimed")));
		let mut watch = board.watch(None);
		assert!(board.settle("claimed", thread::Item::default()).unwrap());
		assert!(mailbox.is_empty());
		let settlement = watch.next().await.expect("watched settlement");
		settlement.lease.claim().expect("exclusive claim");
		assert!(board.is_empty());
		assert!(mailbox.is_empty());
	}

	#[tokio::test]
	async fn dropping_watch_resumes_normal_delivery() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = JobBoard::new(env, mailbox.sender());
		assert!(board.register(watched_job("released")));
		let watch = board.watch(None);
		assert!(board.settle("released", thread::Item::default()).unwrap());
		assert!(mailbox.is_empty());
		drop(watch);
		assert_eq!(mailbox.len(), 1);
		assert!(board.is_empty());
	}
}
