//! Always-on bounded, redacted provider/debug capture.

use std::{
	collections::VecDeque,
	fmt,
	sync::{Arc, LazyLock},
};

use flume::Receiver;
use omp_core::Str;
use omp_secrets::{builtins::credential_rules, redact::SecretRedactor};
use parking_lot::Mutex;

use crate::debug_wire::trim_wire_text;

/// Default number of sanitized provider frames retained per process.
pub const DEFAULT_CAPTURE_FRAMES: usize = 512;
/// Maximum sanitized bytes retained for one frame.
pub const DEFAULT_CAPTURE_FRAME_BYTES: usize = 64 * 1024;
/// Default bounded subscriber backlog.
pub const DEFAULT_SUBSCRIBER_FRAMES: usize = 64;

static GLOBAL_PROVIDER_CAPTURE: LazyLock<RawProviderCapture> =
	LazyLock::new(RawProviderCapture::default);

/// Returns the process-global always-on provider capture authority.
pub fn global_provider_capture() -> &'static RawProviderCapture {
	&GLOBAL_PROVIDER_CAPTURE
}

/// One already-redacted provider/debug frame.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CapturedFrame {
	/// Monotonic process-local sequence.
	pub sequence: u64,
	/// Durable session binding, when available.
	pub session:  Option<Str>,
	/// Provider event/category label.
	pub event:    Str,
	/// Bounded, irreversibly redacted payload.
	pub payload:  Str,
}

/// Current capture accounting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CaptureSummary {
	/// Frames currently retained.
	pub retained:         usize,
	/// Oldest frames evicted by capacity pressure.
	pub evicted:          u64,
	/// Subscriber deliveries dropped by bounded backpressure.
	pub subscriber_drops: u64,
}

/// Atomic snapshot of retained frames and counters.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CaptureSnapshot {
	/// Retained frames in sequence order.
	pub frames:  Vec<CapturedFrame>,
	/// Ring and fan-out counters.
	pub summary: CaptureSummary,
}

#[derive(Debug)]
struct Subscriber {
	session: Option<Str>,
	sender:  flume::Sender<CapturedFrame>,
}

struct State {
	frames:           VecDeque<CapturedFrame>,
	subscribers:      Vec<Subscriber>,
	redactor:         Option<SecretRedactor>,
	next_sequence:    u64,
	evicted:          u64,
	subscriber_drops: u64,
}

/// Process-global-capable capture authority.
#[derive(Clone)]
pub struct RawProviderCapture {
	inner:               Arc<Mutex<State>>,
	capacity:            usize,
	frame_bytes:         usize,
	subscriber_capacity: usize,
}

impl fmt::Debug for RawProviderCapture {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("RawProviderCapture")
			.field("capacity", &self.capacity)
			.field("frame_bytes", &self.frame_bytes)
			.finish_non_exhaustive()
	}
}

impl Default for RawProviderCapture {
	fn default() -> Self {
		Self::new(DEFAULT_CAPTURE_FRAMES, DEFAULT_CAPTURE_FRAME_BYTES, DEFAULT_SUBSCRIBER_FRAMES)
	}
}

impl RawProviderCapture {
	/// Builds an always-redacting ring with explicit hard bounds.
	pub fn new(capacity: usize, frame_bytes: usize, subscriber_capacity: usize) -> Self {
		let redactor = credential_rules().ok().map(SecretRedactor::new);
		Self {
			inner:               Arc::new(Mutex::new(State {
				frames: VecDeque::with_capacity(capacity.max(1)),
				subscribers: Vec::new(),
				redactor,
				next_sequence: 0,
				evicted: 0,
				subscriber_drops: 0,
			})),
			capacity:            capacity.max(1),
			frame_bytes:         frame_bytes.max(128),
			subscriber_capacity: subscriber_capacity.max(1),
		}
	}

	/// Redacts and retains one bounded frame, then fans it out without blocking
	/// inference. `session = None` binds the frame only to global viewers.
	pub fn capture(&self, session: Option<&str>, event: &str, payload: &str) -> CapturedFrame {
		let bounded = trim_wire_text(payload, self.frame_bytes);
		let mut state = self.inner.lock();
		let payload = Str::new(
			state
				.redactor
				.as_mut()
				.map_or_else(|| "[REDACTED]".to_owned(), |redactor| redactor.redact(&bounded)),
		);
		let frame = CapturedFrame {
			sequence: state.next_sequence,
			session: session.map(Str::new),
			event: Str::new(event),
			payload,
		};
		state.next_sequence = state.next_sequence.saturating_add(1);
		if state.frames.len() == self.capacity {
			state.frames.pop_front();
			state.evicted = state.evicted.saturating_add(1);
		}
		state.frames.push_back(frame.clone());
		let mut drops = 0_u64;
		state.subscribers.retain(|subscriber| {
			if !subscriber_matches(subscriber.session.as_deref(), frame.session.as_deref()) {
				return true;
			}
			match subscriber.sender.try_send(frame.clone()) {
				Ok(()) => true,
				Err(flume::TrySendError::Full(_)) => {
					drops = drops.saturating_add(1);
					true
				},
				Err(flume::TrySendError::Disconnected(_)) => false,
			}
		});
		state.subscriber_drops = state.subscriber_drops.saturating_add(drops);
		frame
	}

	/// Snapshots either the global ring or frames belonging to one session.
	pub fn snapshot(&self, session: Option<&str>) -> CaptureSnapshot {
		let state = self.inner.lock();
		let frames = state
			.frames
			.iter()
			.filter(|frame| session.is_none_or(|session| frame.session.as_deref() == Some(session)))
			.cloned()
			.collect();
		CaptureSnapshot {
			frames,
			summary: CaptureSummary {
				retained:         state.frames.len(),
				evicted:          state.evicted,
				subscriber_drops: state.subscriber_drops,
			},
		}
	}

	/// Subscribes to global fan-out (`None`) or one exact session. The returned
	/// channel is bounded and slow viewers never block inference.
	pub fn subscribe(&self, session: Option<&str>) -> Receiver<CapturedFrame> {
		let (sender, receiver) = flume::bounded(self.subscriber_capacity);
		self
			.inner
			.lock()
			.subscribers
			.push(Subscriber { session: session.map(Str::new), sender });
		receiver
	}
}

fn subscriber_matches(filter: Option<&str>, frame: Option<&str>) -> bool {
	filter.is_none() || filter == frame
}
