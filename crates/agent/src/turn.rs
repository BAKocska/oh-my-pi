//! Transport-neutral contracts for one inference turn.

use std::{
	error,
	fmt::{self, Display},
	future::Future,
	pin::Pin,
	task::{Context, Poll},
	time::Duration,
};

use flume::Receiver;
use futures::Stream;
use omp_core::Str;
use omp_inference::TurnId;
use omp_proto::{
	inference::v1::{
		self as pb, ChatParams, ContextRef, Executor, InvokeComplete, InvokeInput, ThreadDelta,
		TurnError, TurnEvent, TurnFrame, ValueMap, turn_error, turn_event, turn_frame, turn_request,
		value,
	},
	thread::v1::Thread,
};
use tonic::transport;
/// Internal turn property carrying a one-shot provider-reset hint.
pub const PROVIDER_RESET_PROP: &str = "omp/session-provider-reset";

/// The canonical conversation input for one logical turn.
#[derive(Clone, Debug)]
pub enum TurnInput {
	/// Supplies a complete thread for a stateless turn or context reseed.
	Full(Thread),
	/// Applies an atomic delta against a held context revision.
	Delta(ContextRef, ThreadDelta),
}
/// Typed stream watchdog bounds consumed by the STREAM arbiter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StreamWatchdog {
	/// Maximum wait for the first decoded event.
	pub first_event_ms: Option<u64>,
	/// Maximum wait between decoded events.
	pub idle_ms:        Option<u64>,
}

/// Per-turn inference options passed through to the live protocol.
#[derive(Clone, Debug, Default)]
pub struct TurnOptions {
	/// Context to seed when [`TurnInput::Full`] is used.
	///
	/// Leaving this absent makes the full turn stateless. Incremental turns take
	/// their context identity from [`TurnInput::Delta`].
	pub context_id:      Option<Str>,
	/// Canonical chat parameters, including model, tools, and sampling controls.
	pub params:          ChatParams,
	/// In-turn invocation capability advertised to the arbiter.
	pub executor:        Option<Executor>,
	/// Namespaced turn-level extension properties.
	pub props:           Option<ValueMap>,
	/// Discard provider-native affinity for this one successful turn.
	pub provider_reset:  bool,
	/// Optional loop-owned stream watchdog bounds.
	pub stream_watchdog: StreamWatchdog,
}

/// A client response frame for a live server-initiated invocation.
#[derive(Clone, Debug)]
pub enum InvokeFrame {
	/// Streams one canonical input chunk or opaque vendor frame.
	Input(Box<InvokeInput>),
	/// Completes an invocation with its canonical result and typed status.
	Complete(Box<InvokeComplete>),
}

impl From<InvokeInput> for InvokeFrame {
	#[inline]
	fn from(frame: InvokeInput) -> Self {
		Self::Input(Box::new(frame))
	}
}

impl From<InvokeComplete> for InvokeFrame {
	#[inline]
	fn from(frame: InvokeComplete) -> Self {
		Self::Complete(Box::new(frame))
	}
}

impl From<InvokeFrame> for TurnFrame {
	#[inline]
	fn from(frame: InvokeFrame) -> Self {
		let frame = match frame {
			InvokeFrame::Input(frame) => turn_frame::Frame::Input(*frame),
			InvokeFrame::Complete(frame) => turn_frame::Frame::Complete(*frame),
		};
		Self { frame: Some(frame) }
	}
}

/// Stable diagnostic codes attached to [`TurnErrorKind::EmptyOutput`]
/// terminal errors.
///
/// The arbiter classifies why a completed provider turn carried no actionable
/// output and stamps one [`pb::Diagnostic`] with these codes. The agent loop
/// selects its terminal retry-cap message from the code so a provider-side
/// content filter is never misreported as a model or context problem.
pub mod empty_stop {
	/// The provider itself signaled no final output (thought-only completion).
	pub const NO_FINAL_OUTPUT: &str = "empty_stop.no_final_output";
	/// The stop carried zero content blocks while billing non-reasoning output
	/// tokens; the diagnostic `detail` holds the billed count in decimal.
	pub const BILLED_OUTPUT: &str = "empty_stop.billed_output";
	/// An empty stop without billed non-reasoning output evidence.
	pub const EMPTY: &str = "empty_stop.empty";
}
/// Loop-owned recovery timing for a typed turn-layer failure.
///
/// This classification is deliberately limited to arbiter protocol kinds. It
/// does not expose provider routes, credentials, or internal retry policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recovery {
	/// Retry the same logical turn after the arbiter-mandated delay.
	RetryAfter(Duration),
	/// Retry the same logical turn with the agent's bounded transient backoff.
	Backoff,
}

/// A turn-layer failure.
///
/// Protocol terminal errors retain their generated [`TurnError`] verbatim so
/// higher layers can inspect diagnostics and stable error identities. Conflict
/// and need-full are separate variants because they are recoveries owned by the
/// agent loop rather than transport policy.
pub enum Error {
	/// The submitted revision was stale; the embedded error carries the actual
	/// revision.
	Conflict(Box<TurnError>),
	/// The referenced context is absent and must be reseeded with a full thread.
	NeedFull(Box<TurnError>),
	/// A non-recoverable terminal protocol error.
	Terminal(Box<TurnError>),
	/// The tonic request or response stream failed.
	Rpc(tonic::Status),
	/// An RPC channel could not be established.
	Connect(transport::Error),
	/// The peer violated the turn framing contract.
	Protocol(&'static str),
	/// The local request is invalid before it reaches the service.
	Invalid(&'static str),
	/// The invocation send side is no longer available.
	Closed,
}

impl Error {
	/// Returns the retained terminal protocol error, when this came from one.
	#[inline]
	pub const fn turn_error(&self) -> Option<&TurnError> {
		match self {
			Self::Conflict(error) | Self::NeedFull(error) | Self::Terminal(error) => Some(error),
			Self::Rpc(_) | Self::Connect(_) | Self::Protocol(_) | Self::Invalid(_) | Self::Closed => {
				None
			},
		}
	}

	/// Returns the loop-owned recovery timing for this typed failure.
	pub fn recovery(&self) -> Option<Recovery> {
		match self {
			Self::Conflict(_) | Self::NeedFull(_) => None,
			Self::Terminal(error) => match turn_error::Kind::try_from(error.kind).ok()? {
				turn_error::Kind::RateLimited => {
					Some(Recovery::RetryAfter(Duration::from_millis(error.retry_after_ms)))
				},
				turn_error::Kind::Overloaded | turn_error::Kind::Upstream => Some(Recovery::Backoff),
				_ => None,
			},
			Self::Rpc(_) => Some(Recovery::Backoff),
			Self::Connect(_) | Self::Protocol(_) | Self::Invalid(_) | Self::Closed => None,
		}
	}

	/// Reports whether the agent loop owns recovery for this failure.
	#[inline]
	pub fn is_recovery(&self) -> bool {
		matches!(self, Self::Conflict(_) | Self::NeedFull(_)) || self.recovery().is_some()
	}

	pub(crate) fn from_turn(error: TurnError) -> Self {
		match turn_error::Kind::try_from(error.kind).unwrap_or(turn_error::Kind::Unspecified) {
			turn_error::Kind::Conflict => Self::Conflict(Box::new(error)),
			turn_error::Kind::NeedFull => Self::NeedFull(Box::new(error)),
			_ => Self::Terminal(Box::new(error)),
		}
	}
}

impl fmt::Debug for Error {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		Display::fmt(self, formatter)
	}
}

impl Display for Error {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Conflict(_) => formatter.write_str("turn context conflict"),
			Self::NeedFull(_) => formatter.write_str("turn context requires a full reseed"),
			Self::Terminal(error) => {
				write!(
					formatter,
					"terminal turn error ({:?})",
					omp_proto::inference::v1::turn_error::Kind::try_from(error.kind)
						.unwrap_or(omp_proto::inference::v1::turn_error::Kind::Unspecified)
				)?;
				if !error.detail.is_empty() {
					write!(formatter, ": {}", error.detail)?;
				}
				Ok(())
			},
			Self::Rpc(status) => {
				write!(formatter, "turn RPC failed ({:?})", status.code())?;
				if !status.message().is_empty() {
					write!(formatter, ": {}", status.message())?;
				}
				Ok(())
			},
			Self::Connect(_) => formatter.write_str("turn RPC connection failed"),
			Self::Protocol(message) => write!(formatter, "turn protocol error: {message}"),
			Self::Invalid(message) => write!(formatter, "invalid turn: {message}"),
			Self::Closed => formatter.write_str("turn invocation stream is closed"),
		}
	}
}

impl error::Error for Error {}

impl From<tonic::Status> for Error {
	#[inline]
	fn from(status: tonic::Status) -> Self {
		Self::Rpc(status)
	}
}

impl From<transport::Error> for Error {
	#[inline]
	fn from(error: transport::Error) -> Self {
		Self::Connect(error)
	}
}

/// Starts logical turns against an inference arbiter.
///
/// Implementations provide transport only. They never retry, rebase, reseed,
/// journal, or deduplicate beyond forwarding the caller's stable [`TurnId`].
pub trait TurnClient: Send + Sync {
	/// Duplex session returned for one admitted turn.
	type Session<'client>: TurnSession + 'client
	where
		Self: 'client;

	/// Opens one logical turn, moving its canonical input into the request.
	fn turn<'client>(
		&'client self,
		turn_id: TurnId,
		input: TurnInput,
		options: &'client TurnOptions,
	) -> impl Future<Output = Result<Self::Session<'client>, Error>> + Send + 'client;
}

/// A live duplex turn.
///
/// Poll one event at a time, then release the returned event stream before
/// calling [`TurnSession::submit`] in response to an invocation. Dropping the
/// session closes both halves and structurally cancels unfinished upstream
/// work.
pub trait TurnSession: Send {
	/// Borrows the ordered event stream.
	///
	/// `Accepted { replay: true }` is passed through unchanged. An in-band
	/// terminal error becomes a typed [`Error`]; a successful [`pb::Outcome`]
	/// remains a regular canonical [`TurnEvent`].
	fn events(&mut self) -> impl Stream<Item = Result<TurnEvent, Error>> + Send + Unpin + '_;

	/// Submits invocation input or completion on the still-live turn stream.
	fn submit(&mut self, frame: InvokeFrame) -> impl Future<Output = Result<(), Error>> + Send + '_;
}

pub fn open_frame(
	turn_id: &TurnId<str>,
	input: TurnInput,
	options: &TurnOptions,
) -> Result<TurnFrame, Error> {
	if turn_id.as_str().is_empty() {
		return Err(Error::Invalid("turn_id must not be empty"));
	}
	if options.context_id.as_ref().is_some_and(Str::is_empty) {
		return Err(Error::Invalid("context_id must not be empty when present"));
	}

	let input = match input {
		TurnInput::Full(thread) => turn_request::Input::Seed(pb::Seed {
			context_id: options
				.context_id
				.as_ref()
				.map_or_else(String::new, |id| id.as_str().to_owned()),
			thread:     Some(thread),
		}),
		TurnInput::Delta(context, delta) => turn_request::Input::Incremental(pb::Incremental {
			context: Some(context),
			delta:   Some(delta),
		}),
	};
	let mut props = options.props.clone();
	if options.provider_reset {
		props
			.get_or_insert_default()
			.fields
			.insert(PROVIDER_RESET_PROP.to_owned(), pb::Value { kind: Some(value::Kind::Bool(true)) });
	}
	Ok(TurnFrame {
		frame: Some(turn_frame::Frame::Open(pb::TurnRequest {
			turn_id: turn_id.as_str().to_owned(),
			input: Some(input),
			params: Some(options.params.clone()),
			executor: options.executor.clone(),
			props,
		})),
	})
}

pub fn invocation_stream(
	receiver: Receiver<TurnFrame>,
) -> impl Stream<Item = TurnFrame> + Send + 'static {
	futures::stream::unfold(receiver, |receiver| async move {
		let frame = receiver.recv_async().await.ok()?;
		Some((frame, receiver))
	})
}

pub struct EventStream<'session> {
	stream:   &'session mut tonic::Streaming<TurnEvent>,
	terminal: &'session mut bool,
}

impl<'session> EventStream<'session> {
	pub(crate) const fn new(
		stream: &'session mut tonic::Streaming<TurnEvent>,
		terminal: &'session mut bool,
	) -> Self {
		Self { stream, terminal }
	}
}

impl Stream for EventStream<'_> {
	type Item = Result<TurnEvent, Error>;

	fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		if *self.terminal {
			return Poll::Ready(None);
		}
		match Pin::new(&mut *self.stream).poll_next(context) {
			Poll::Pending => Poll::Pending,
			Poll::Ready(Some(Err(status))) => {
				*self.terminal = true;
				Poll::Ready(Some(Err(Error::Rpc(status))))
			},
			Poll::Ready(Some(Ok(event))) => match event.event {
				Some(turn_event::Event::Error(error)) => {
					*self.terminal = true;
					Poll::Ready(Some(Err(Error::from_turn(error))))
				},
				event @ Some(turn_event::Event::Outcome(_)) => {
					*self.terminal = true;
					Poll::Ready(Some(Ok(TurnEvent { event })))
				},
				event @ Some(_) => Poll::Ready(Some(Ok(TurnEvent { event }))),
				None => {
					*self.terminal = true;
					Poll::Ready(Some(Err(Error::Protocol("TurnEvent.event is required"))))
				},
			},
			Poll::Ready(None) => {
				*self.terminal = true;
				Poll::Ready(Some(Err(Error::Protocol("turn stream ended without a terminal event"))))
			},
		}
	}
}
