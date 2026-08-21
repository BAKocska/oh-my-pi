//! Named-worker supervision and generation-fenced worker DATA transport.

use std::{
	collections::BTreeMap,
	io::Read,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use flume::{Receiver, Sender};
use omp_core::{CowBytes, Str};
use omp_env::WorkerLease;
use omp_proto::env::v1::WorkerData;
use omp_storage::blob::BlobStore;
use parking_lot::Mutex;
use thiserror::Error;

/// Largest tunnel header accepted before any buffer allocation.
pub const MAX_TUNNEL_HEADER_BYTES: usize = 64 * 1024;
/// Largest number of out-of-band buffers accepted in one tunnel frame.
pub const MAX_TUNNEL_BUFFERS: usize = 64;
/// Largest individual tunnel buffer accepted by the supervisor.
pub const MAX_TUNNEL_BUFFER_BYTES: usize = 256 * 1024;

/// A decoded worker tunnel frame that preserves its received byte ownership.
#[derive(Clone)]
pub struct TunnelFrame {
	/// Encoded protocol header, never rebuilt by the tunnel.
	pub header:  CowBytes<'static>,
	/// Out-of-band buffers referenced by the header.
	pub buffers: Vec<CowBytes<'static>>,
}

/// Worker transport framing failure.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum TunnelError {
	/// The header length exceeded the fixed pre-allocation limit.
	#[error("worker tunnel header exceeds {MAX_TUNNEL_HEADER_BYTES} bytes")]
	HeaderTooLarge,
	/// The out-of-band buffer count exceeded the fixed pre-allocation limit.
	#[error("worker tunnel has more than {MAX_TUNNEL_BUFFERS} buffers")]
	TooManyBuffers,
	/// A buffer length exceeded the fixed pre-allocation limit.
	#[error("worker tunnel buffer exceeds {MAX_TUNNEL_BUFFER_BYTES} bytes")]
	BufferTooLarge,
	/// The transport ended before a complete frame arrived.
	#[error("worker tunnel frame is truncated")]
	Truncated,
}

impl TunnelFrame {
	/// Decodes a bounded tunnel frame without allocating from untrusted lengths.
	///
	/// The frame is `hlen:u32`, `nbufs:u16`, header bytes, then repeated
	/// `len:u32, bytes` buffers. Bounds are checked before reserving or copying.
	pub fn decode(data: CowBytes<'static>) -> Result<Self, TunnelError> {
		let bytes = &*data;
		if bytes.len() < 6 {
			return Err(TunnelError::Truncated);
		}
		let header_len = usize::try_from(u32::from_be_bytes(bytes[..4].try_into().expect("length")))
			.expect("u32 always fits usize on supported targets");
		let buffer_count = usize::from(u16::from_be_bytes(bytes[4..6].try_into().expect("count")));
		if header_len > MAX_TUNNEL_HEADER_BYTES {
			return Err(TunnelError::HeaderTooLarge);
		}
		if buffer_count > MAX_TUNNEL_BUFFERS {
			return Err(TunnelError::TooManyBuffers);
		}
		let mut offset = 6usize
			.checked_add(header_len)
			.ok_or(TunnelError::Truncated)?;
		if offset > bytes.len() {
			return Err(TunnelError::Truncated);
		}
		let header = CowBytes::owned(bytes::Bytes::copy_from_slice(&bytes[6..offset]));
		let mut buffers = Vec::with_capacity(buffer_count);
		for _ in 0..buffer_count {
			let length_end = offset.checked_add(4).ok_or(TunnelError::Truncated)?;
			let length = bytes
				.get(offset..length_end)
				.ok_or(TunnelError::Truncated)?;
			let length = usize::try_from(u32::from_be_bytes(length.try_into().expect("length")))
				.expect("u32 always fits usize on supported targets");
			if length > MAX_TUNNEL_BUFFER_BYTES {
				return Err(TunnelError::BufferTooLarge);
			}
			offset = length_end;
			let end = offset.checked_add(length).ok_or(TunnelError::Truncated)?;
			let buffer = bytes.get(offset..end).ok_or(TunnelError::Truncated)?;
			buffers.push(CowBytes::owned(bytes::Bytes::copy_from_slice(buffer)));
			offset = end;
		}
		if offset != bytes.len() {
			return Err(TunnelError::Truncated);
		}

		Ok(Self { header, buffers })
	}
}
/// The sole environment-side minting authority for spilled worker payloads.
///
/// Both remote-frame diversion and verdict spilling enter through
/// [`Self::put_reader`], which delegates directly to [`BlobStore::put_reader`].
#[derive(Clone, Debug)]
pub struct SpillDiverter {
	store: Arc<BlobStore>,
}

/// A value that can spill a verdict through the environment blob authority.
pub trait VerdictSpill {
	/// Stores an out-of-band verdict payload and returns a hash-only wire blob.
	///
	/// # Errors
	/// Returns the blob-store error if durable placement fails.
	fn spill_verdict(
		&self,
		reader: impl Read,
	) -> Result<omp_proto::thread::v1::Blob, omp_storage::blob::Error>;
}

impl SpillDiverter {
	/// Binds the diverter to the Environment's unique blob store.
	#[must_use]
	pub const fn new(store: Arc<BlobStore>) -> Self {
		Self { store }
	}

	/// Streams one out-of-band buffer into the blob store without rebuilding it.
	///
	/// # Errors
	/// Returns the blob-store error if durable placement fails.
	pub fn put_reader(
		&self,
		reader: impl Read,
	) -> Result<omp_proto::thread::v1::Blob, omp_storage::blob::Error> {
		let reference = self.store.put_reader(reader)?;
		Ok(omp_proto::thread::v1::Blob {
			hash: reference.hash.as_bytes().to_vec().into(),
			size: reference.size,
			..omp_proto::thread::v1::Blob::default()
		})
	}
}

impl VerdictSpill for SpillDiverter {
	fn spill_verdict(
		&self,
		reader: impl Read,
	) -> Result<omp_proto::thread::v1::Blob, omp_storage::blob::Error> {
		self.put_reader(reader)
	}
}

/// A named worker key. Environment placement is deliberately included so a
/// moved device cannot retain the identity of its former worker.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WorkerKey {
	/// Extension owning the worker.
	pub extension: Str,
	/// Declared worker name.
	pub name:      Str,
	/// Resolved placement site identity.
	pub site:      Str,
}

/// Supervisor failures exposed to worker routing.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum WorkerUnavailable {
	/// The per-layer live-worker ceiling refused an immediate spawn.
	#[error("worker unavailable: layer live-worker ceiling reached")]
	LayerCeiling,
	/// The global concurrent-spawn ceiling refused an immediate spawn.
	#[error("worker unavailable: concurrent spawn ceiling reached")]
	SpawnCeiling,
	/// A generation-fenced request targeted a retired worker.
	#[error("worker unavailable: stale generation")]
	StaleGeneration,
}

/// One worker selected for dispatch.
#[derive(Clone, Debug)]
pub struct WorkerRoute {
	/// Stable worker identity including placement site.
	pub key:        WorkerKey,
	/// Current generation, which fences every DATA frame.
	pub generation: u64,
}

/// A non-streaming placed-device dispatch admitted to one worker generation.
#[derive(Clone, Debug)]
pub struct WorkerDispatch {
	/// The generation that owns this call.
	pub route:    WorkerRoute,
	/// Final arguments delivered without extension-host reserialization.
	pub args:     bytes::Bytes,
	/// Supervisor-enforced execution deadline.
	pub deadline: Duration,
}

/// One DATA frame accepted at the sole generation-fencing demultiplex point.
#[derive(Clone)]
pub struct AcceptedWorkerData {
	/// Worker identity.
	pub route:   WorkerRoute,
	/// Protocol/stderr channel selected by the worker.
	pub channel: u32,
	/// Payload ownership without a parse-and-rebuild pass.
	pub data:    CowBytes<'static>,
}

/// Named-worker actor commands. Data uses a bounded lane; lifecycle uses the
/// unbounded lease lane so cancellation cannot be blocked behind payload bytes.
#[derive(Debug)]
pub enum WorkerCommand {
	/// Open or coalesce a worker route.
	Open(WorkerKey),
	/// Send a bounded DATA frame to the named-worker demultiplexer.
	Data(WorkerData),
	/// Stop exactly one generation.
	Terminate {
		/// Supervised worker name.
		name:       Str,
		/// Generation whose processes are terminated.
		generation: u64,
	},
}
/// Exponential restart scheduling with the required healthy-uptime reset.
#[derive(Clone, Debug)]
pub struct RestartBackoff {
	next:      Duration,
	maximum:   Duration,
	healthy:   Duration,
	last_boot: std::time::Instant,
}

impl RestartBackoff {
	/// Starts a one-second to thirty-second restart schedule.
	#[must_use]
	pub fn new() -> Self {
		Self {
			next:      Duration::from_secs(1),
			maximum:   Duration::from_secs(30),
			healthy:   Duration::from_secs(30),
			last_boot: std::time::Instant::now(),
		}
	}

	/// Records a failure and returns the delay before its replacement spawn.
	pub fn failed(&mut self) -> Duration {
		if self.last_boot.elapsed() >= self.healthy {
			self.next = Duration::from_secs(1);
		}
		let delay = self.next;
		self.next = self.next.saturating_mul(2).min(self.maximum);
		delay
	}

	/// Starts the healthy-uptime window for a replacement generation.
	pub fn booted(&mut self) {
		self.last_boot = std::time::Instant::now();
	}
}

impl Default for RestartBackoff {
	fn default() -> Self {
		Self::new()
	}
}

/// In-process named-worker routing state.
#[derive(Debug)]
pub struct WorkerSupervisor {
	workers:       Mutex<BTreeMap<Str, WorkerRoute>>,
	layer_live:    AtomicU64,
	layer_ceiling: u64,
	spawn_live:    AtomicU64,
	spawn_ceiling: u64,
	stale_frames:  AtomicU64,
	terminate_tx:  Sender<(Str, u64)>,
	terminate_rx:  Receiver<(Str, u64)>,
}

impl WorkerSupervisor {
	/// Creates a supervisor with immediate-refusal worker and spawn ceilings.
	#[must_use]
	pub fn new(layer_ceiling: u64, spawn_ceiling: u64) -> Self {
		let (terminate_tx, terminate_rx) = flume::unbounded();
		Self {
			workers: Mutex::new(BTreeMap::new()),
			layer_live: AtomicU64::new(0),
			layer_ceiling,
			spawn_live: AtomicU64::new(0),
			spawn_ceiling,
			stale_frames: AtomicU64::new(0),
			terminate_tx,
			terminate_rx,
		}
	}

	/// Opens a named route or refuses immediately when a ceiling is exhausted.
	pub fn open(&self, key: WorkerKey) -> Result<(WorkerRoute, WorkerLease), WorkerUnavailable> {
		if let Some(route) = self.workers.lock().get(&key.name).cloned() {
			let lease =
				WorkerLease::new(route.key.name.clone(), route.generation, self.terminate_tx.clone());
			return Ok((route, lease));
		}
		reserve(&self.layer_live, self.layer_ceiling).ok_or(WorkerUnavailable::LayerCeiling)?;
		if !reserve(&self.spawn_live, self.spawn_ceiling) {
			self.layer_live.fetch_sub(1, Ordering::AcqRel);
			return Err(WorkerUnavailable::SpawnCeiling);
		}
		let route = WorkerRoute { key: key.clone(), generation: 1 };
		self.workers.lock().insert(key.name.clone(), route.clone());
		self.spawn_live.fetch_sub(1, Ordering::AcqRel);
		let lease = WorkerLease::new(key.name, route.generation, self.terminate_tx.clone());
		Ok((route, lease))
	}

	/// Admits a final non-streaming device call to the current named worker.
	///
	/// The returned generation is fenced at response demultiplexing; callers
	/// must never accept a response from a replacement generation.
	pub async fn dispatch(
		&self,
		key: WorkerKey,
		args: bytes::Bytes,
		deadline: Duration,
	) -> Result<WorkerDispatch, WorkerUnavailable> {
		let (route, lease) = self.open(key)?;
		lease.relinquish();
		Ok(WorkerDispatch { route, args, deadline })
	}

	/// Closes exactly one current generation and releases its layer slot.
	pub fn close(&self, name: &str, generation: u64) -> bool {
		let mut workers = self.workers.lock();
		if workers
			.get(name)
			.is_none_or(|route| route.generation != generation)
		{
			return false;
		}
		workers.remove(name);
		self.layer_live.fetch_sub(1, Ordering::AcqRel);
		true
	}

	/// Retires exactly one generation and makes its replacement generation
	/// current. Late DATA is rejected at [`Self::demux`].
	pub fn replace(&self, name: &str, generation: u64) -> Option<WorkerRoute> {
		let mut workers = self.workers.lock();
		let route = workers.get_mut(name)?;
		if route.generation != generation {
			return None;
		}
		route.generation = route.generation.checked_add(1)?;
		Some(route.clone())
	}

	/// Returns the current route for a named worker.
	#[must_use]
	pub fn route(&self, name: &str) -> Option<WorkerRoute> {
		self.workers.lock().get(name).cloned()
	}

	/// Returns a stable snapshot for a worker-list response.
	pub fn routes(&self) -> Vec<WorkerRoute> {
		self.workers.lock().values().cloned().collect()
	}

	/// Accepts DATA only when its named generation is still current.
	pub fn demux(&self, frame: WorkerData) -> Result<AcceptedWorkerData, WorkerUnavailable> {
		let route = self.workers.lock().get(frame.name.as_str()).cloned();
		let Some(route) = route.filter(|route| route.generation == frame.generation) else {
			self.stale_frames.fetch_add(1, Ordering::Relaxed);
			return Err(WorkerUnavailable::StaleGeneration);
		};
		Ok(AcceptedWorkerData { route, channel: frame.channel, data: CowBytes::owned(frame.data) })
	}

	/// Returns and drains one drop-triggered termination request.
	pub fn try_termination(&self) -> Option<(Str, u64)> {
		self.terminate_rx.try_recv().ok()
	}

	/// Returns the number of DATA frames rejected by the sole generation fence.
	#[must_use]
	pub fn stale_frame_count(&self) -> u64 {
		self.stale_frames.load(Ordering::Relaxed)
	}
}

fn reserve(counter: &AtomicU64, limit: u64) -> bool {
	counter
		.try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
			(current < limit).then_some(current + 1)
		})
		.is_ok()
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;

	#[test]
	fn codec_refuses_header_before_allocation() {
		let mut bytes = Vec::new();
		bytes.extend_from_slice(
			&u32::try_from(MAX_TUNNEL_HEADER_BYTES + 1)
				.unwrap()
				.to_be_bytes(),
		);
		bytes.extend_from_slice(&0u16.to_be_bytes());
		assert!(matches!(
			TunnelFrame::decode(CowBytes::owned(bytes.into())),
			Err(TunnelError::HeaderTooLarge)
		));
	}

	#[test]
	fn codec_refuses_buffer_count_before_allocation() {
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&0u32.to_be_bytes());
		bytes.extend_from_slice(&u16::try_from(MAX_TUNNEL_BUFFERS + 1).unwrap().to_be_bytes());
		assert!(matches!(
			TunnelFrame::decode(CowBytes::owned(bytes.into())),
			Err(TunnelError::TooManyBuffers)
		));
	}

	#[test]
	fn stale_generation_never_delivers() {
		let supervisor = WorkerSupervisor::new(1, 1);
		let (route, _lease) = supervisor
			.open(WorkerKey { extension: sf!("x"), name: sf!("w"), site: sf!("env") })
			.unwrap();
		let frame = WorkerData {
			name: route.key.name.to_string(),
			generation: route.generation + 1,
			channel: 0,
			data: Vec::new().into(),
			..WorkerData::default()
		};
		assert!(matches!(supervisor.demux(frame), Err(WorkerUnavailable::StaleGeneration)));
		assert_eq!(supervisor.stale_frame_count(), 1);
	}

	#[test]
	fn lease_drop_queues_termination() {
		let supervisor = WorkerSupervisor::new(1, 1);
		let (route, lease) = supervisor
			.open(WorkerKey { extension: sf!("x"), name: sf!("w"), site: sf!("env") })
			.unwrap();
		drop(lease);
		assert_eq!(supervisor.try_termination(), Some((route.key.name, route.generation)));
	}

	#[test]
	fn ceiling_refuses_without_queueing() {
		let supervisor = WorkerSupervisor::new(1, 1);
		let _ = supervisor
			.open(WorkerKey { extension: sf!("x"), name: sf!("a"), site: sf!("env") })
			.unwrap();
		assert!(matches!(
			supervisor.open(WorkerKey {
				extension: sf!("x"),
				name:      sf!("b"),
				site:      sf!("env"),
			}),
			Err(WorkerUnavailable::LayerCeiling)
		));
	}
}
