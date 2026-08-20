//! Bounded Content-Length framed Debug Adapter Protocol engine.

use std::{
	collections::HashMap,
	io,
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicI64, Ordering},
	},
	time::Duration,
};

use omp_core::{Str, sf};
use parking_lot::Mutex;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
	io::{AsyncRead, AsyncWrite},
	process::{Child, Command},
	sync::{Mutex as AsyncMutex, broadcast, oneshot},
};
use tokio_util::sync::CancellationToken;

const MAX_DAP_HEADER_BYTES: usize = 8 * 1024;
const MAX_DAP_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_CAPACITY: usize = 512;

/// An event or reverse request received from a debug adapter.
#[derive(Clone, Debug)]
pub enum DapInbound {
	/// Adapter event.
	Event {
		/// Event name.
		event: Str,
		/// Opaque event body.
		body:  Value,
	},
	/// Adapter-to-client request.
	ReverseRequest {
		/// Adapter request sequence.
		seq:       i64,
		/// Requested client command.
		command:   Str,
		/// Opaque arguments.
		arguments: Value,
	},
}

/// Framing, transport, or adapter response failure.
#[derive(Debug, Error)]
pub enum DapProtocolError {
	/// The transport ended before completion.
	#[error("DAP transport closed")]
	TransportClosed,
	/// A bounded read, write, or request timed out.
	#[error("DAP request timed out")]
	Timeout,
	/// A message violated DAP framing.
	#[error("invalid DAP frame: {0}")]
	InvalidFrame(Str),
	/// The adapter returned `success: false`.
	#[error("DAP adapter rejected {command}: {message}")]
	Adapter {
		/// Failed command.
		command: Str,
		/// Adapter-supplied sanitized message.
		message: Str,
	},
	/// Transport I/O failed.
	#[error("DAP I/O failed: {0}")]
	Io(#[from] io::Error),
	/// Message JSON was malformed.
	#[error("DAP JSON failed: {0}")]
	Json(#[from] serde_json::Error),
}

struct OutgoingRequest {
	seq:       i64,
	command:   Str,
	arguments: Value,
	response:  oneshot::Sender<Result<Value, DapProtocolError>>,
}

enum Outgoing {
	Request(OutgoingRequest),
	Response {
		request_seq: i64,
		command:     Str,
		success:     bool,
		body:        Value,
		message:     Option<Str>,
	},
	Shutdown,
}

struct ProtocolInner {
	next_seq: AtomicI64,
	outgoing: flume::Sender<Outgoing>,
	events:   broadcast::Sender<DapInbound>,
	closed:   CancellationToken,
}

/// Cloneable client handle for one ordered DAP connection.
#[derive(Clone)]
pub struct DapProtocol {
	inner: Arc<ProtocolInner>,
}

/// A spawned stdio adapter and its protocol connection.
pub struct SpawnedDap {
	/// Active framed protocol.
	pub protocol: DapProtocol,
	/// Owned adapter process.
	pub child:    Arc<AsyncMutex<Child>>,
}

impl DapProtocol {
	/// Starts the protocol actor over an already-connected byte transport.
	pub fn from_streams<R, W>(reader: R, writer: W) -> Self
	where
		R: AsyncRead + Unpin + Send + 'static,
		W: AsyncWrite + Unpin + Send + 'static,
	{
		let (outgoing, receiver) = flume::unbounded();
		let (events, _) = broadcast::channel(EVENT_CAPACITY);
		let closed = CancellationToken::new();
		let inner = Arc::new(ProtocolInner { next_seq: AtomicI64::new(1), outgoing, events, closed });
		let actor = Arc::clone(&inner);
		tokio::spawn(async move { run_protocol(reader, writer, receiver, actor).await });
		Self { inner }
	}

	/// Spawns a non-interactive stdio adapter without a controlling terminal.
	pub fn spawn_stdio(
		command: &str,
		args: &[Str],
		cwd: &Path,
	) -> Result<SpawnedDap, DapProtocolError> {
		let mut process = Command::new(command);
		process
			.args(args.iter().map(Str::as_str))
			.current_dir(cwd)
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::null())
			.kill_on_drop(true)
			.env("CI", "1")
			.env("TERM", "dumb")
			.env("GIT_TERMINAL_PROMPT", "0");
		#[cfg(unix)]
		{
			// SAFETY: `setsid` is async-signal-safe and touches no shared Rust state.
			unsafe {
				process.pre_exec(|| {
					if libc::setsid() < 0 {
						Err(io::Error::last_os_error())
					} else {
						Ok(())
					}
				})
			};
		}
		let mut child = process.spawn()?;
		let reader = child
			.stdout
			.take()
			.ok_or_else(|| io::Error::other("adapter stdout unavailable"))?;
		let writer = child
			.stdin
			.take()
			.ok_or_else(|| io::Error::other("adapter stdin unavailable"))?;
		Ok(SpawnedDap {
			protocol: Self::from_streams(reader, writer),
			child:    Arc::new(AsyncMutex::new(child)),
		})
	}

	/// Connects an existing TCP debug adapter.
	pub async fn connect_tcp(address: std::net::SocketAddr) -> Result<Self, DapProtocolError> {
		let stream = tokio::net::TcpStream::connect(address).await?;
		let (reader, writer) = stream.into_split();
		Ok(Self::from_streams(reader, writer))
	}

	/// Connects an existing Unix-domain debug adapter.
	#[cfg(unix)]
	pub async fn connect_unix(path: &Path) -> Result<Self, DapProtocolError> {
		let stream = tokio::net::UnixStream::connect(path).await?;
		let (reader, writer) = stream.into_split();
		Ok(Self::from_streams(reader, writer))
	}

	/// Sends one request and resolves its correlated response body.
	pub async fn request(
		&self,
		command: impl AsRef<str>,
		arguments: Value,
	) -> Result<Value, DapProtocolError> {
		if self.inner.closed.is_cancelled() {
			return Err(DapProtocolError::TransportClosed);
		}
		let seq = self.inner.next_seq.fetch_add(1, Ordering::Relaxed);
		if seq <= 0 {
			return Err(DapProtocolError::InvalidFrame(sf!("sequence space exhausted")));
		}
		let (response, receiver) = oneshot::channel();
		self
			.inner
			.outgoing
			.send_async(Outgoing::Request(OutgoingRequest {
				seq,
				command: Str::new(command.as_ref()),
				arguments,
				response,
			}))
			.await
			.map_err(|_| DapProtocolError::TransportClosed)?;
		match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
			Ok(Ok(result)) => result,
			Ok(Err(_)) => Err(DapProtocolError::TransportClosed),
			Err(_) => Err(DapProtocolError::Timeout),
		}
	}

	/// Answers one adapter reverse request.
	pub async fn respond_reverse(
		&self,
		request_seq: i64,
		command: impl AsRef<str>,
		success: bool,
		body: Value,
		message: Option<Str>,
	) -> Result<(), DapProtocolError> {
		self
			.inner
			.outgoing
			.send_async(Outgoing::Response {
				request_seq,
				command: Str::new(command.as_ref()),
				success,
				body,
				message,
			})
			.await
			.map_err(|_| DapProtocolError::TransportClosed)
	}

	/// Subscribes before a launch request to avoid stop-on-entry and initialized
	/// races.
	pub fn subscribe(&self) -> broadcast::Receiver<DapInbound> {
		self.inner.events.subscribe()
	}

	/// Waits for an event with an exact name.
	pub async fn wait_for_event(
		mut receiver: broadcast::Receiver<DapInbound>,
		event_name: &str,
		timeout: Duration,
	) -> Result<Value, DapProtocolError> {
		let wait = async {
			loop {
				match receiver.recv().await {
					Ok(DapInbound::Event { event, body }) if event == event_name => return Ok(body),
					Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {},
					Err(broadcast::error::RecvError::Closed) => {
						return Err(DapProtocolError::TransportClosed);
					},
				}
			}
		};
		tokio::time::timeout(timeout, wait)
			.await
			.map_err(|_| DapProtocolError::Timeout)?
	}

	/// Resolves when the byte transport closes or the actor shuts down.
	pub async fn closed(&self) {
		self.inner.closed.cancelled().await;
	}

	/// Stops the protocol actor and wakes subscribers.
	pub fn shutdown(&self) {
		let _ = self.inner.outgoing.send(Outgoing::Shutdown);
	}

	/// Reports whether the protocol transport has closed.
	#[must_use]
	pub fn is_closed(&self) -> bool {
		self.inner.closed.is_cancelled()
	}
}

impl Drop for DapProtocol {
	fn drop(&mut self) {
		if Arc::strong_count(&self.inner) == 1 {
			let _ = self.inner.outgoing.send(Outgoing::Shutdown);
		}
	}
}

async fn run_protocol<R, W>(
	reader: R,
	mut writer: W,
	outgoing: flume::Receiver<Outgoing>,
	inner: Arc<ProtocolInner>,
) where
	R: AsyncRead + Unpin,
	W: AsyncWrite + Unpin,
{
	let mut reader = reader;
	let pending =
		Mutex::new(HashMap::<i64, (Str, oneshot::Sender<Result<Value, DapProtocolError>>)>::new());
	loop {
		tokio::select! {
			outbound = outgoing.recv_async() => match outbound {
				Ok(Outgoing::Request(request)) => {
					let message = json!({"seq": request.seq, "type": "request", "command": request.command, "arguments": request.arguments});
					pending.lock().insert(request.seq, (request.command, request.response));
					if write_message(&mut writer, &message).await.is_err() { break; }
				},
				Ok(Outgoing::Response { request_seq, command, success, body, message }) => {
					let seq = inner.next_seq.fetch_add(1, Ordering::Relaxed);
					let value = json!({"seq": seq, "type": "response", "request_seq": request_seq, "command": command, "success": success, "body": body, "message": message});
					if write_message(&mut writer, &value).await.is_err() { break; }
				},
				Ok(Outgoing::Shutdown) | Err(_) => break,
			},
			message = read_message(&mut reader) => match message {
				Ok(message) => dispatch_message(message, &pending, &inner.events),
				Err(_) => break,
			},
		}
	}
	inner.closed.cancel();
	for (_, (_, response)) in pending.into_inner() {
		let _ = response.send(Err(DapProtocolError::TransportClosed));
	}
}

fn dispatch_message(
	message: Value,
	pending: &Mutex<HashMap<i64, (Str, oneshot::Sender<Result<Value, DapProtocolError>>)>>,
	events: &broadcast::Sender<DapInbound>,
) {
	match message.get("type").and_then(Value::as_str) {
		Some("response") => {
			let Some(request_seq) = message.get("request_seq").and_then(Value::as_i64) else {
				return;
			};
			let Some((command, response)) = pending.lock().remove(&request_seq) else {
				return;
			};
			let result = if message
				.get("success")
				.and_then(Value::as_bool)
				.unwrap_or(false)
			{
				Ok(message.get("body").cloned().unwrap_or(Value::Null))
			} else {
				Err(DapProtocolError::Adapter {
					command,
					message: Str::new(
						message
							.get("message")
							.and_then(Value::as_str)
							.unwrap_or("adapter request failed"),
					),
				})
			};
			let _ = response.send(result);
		},
		Some("event") => {
			let Some(event) = message.get("event").and_then(Value::as_str) else {
				return;
			};
			let _ = events.send(DapInbound::Event {
				event: Str::new(event),
				body:  message.get("body").cloned().unwrap_or(Value::Null),
			});
		},
		Some("request") => {
			let (Some(seq), Some(command)) = (
				message.get("seq").and_then(Value::as_i64),
				message.get("command").and_then(Value::as_str),
			) else {
				return;
			};
			let _ = events.send(DapInbound::ReverseRequest {
				seq,
				command: Str::new(command),
				arguments: message.get("arguments").cloned().unwrap_or(Value::Null),
			});
		},
		_ => {},
	}
}

async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Value, DapProtocolError> {
	let body = crate::lsp_process::read_frame(reader, MAX_DAP_HEADER_BYTES, MAX_DAP_MESSAGE_BYTES)
		.await
		.map_err(DapProtocolError::InvalidFrame)?;
	Ok(serde_json::from_slice(&body)?)
}

async fn write_message<W: AsyncWrite + Unpin>(
	writer: &mut W,
	message: &Value,
) -> Result<(), DapProtocolError> {
	let body = serde_json::to_vec(message)?;
	if body.len() > MAX_DAP_MESSAGE_BYTES {
		return Err(DapProtocolError::InvalidFrame(sf!("message exceeds size bound")));
	}
	let write = crate::lsp_process::write_frame(writer, &body);
	tokio::time::timeout(WRITE_TIMEOUT, write)
		.await
		.map_err(|_| DapProtocolError::Timeout)??;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn correlates_response_and_publishes_event() {
		let (client, mut adapter) = tokio::io::duplex(16 * 1024);
		let (reader, writer) = tokio::io::split(client);
		let protocol = DapProtocol::from_streams(reader, writer);
		let mut events = protocol.subscribe();
		tokio::spawn(async move {
			let request = read_message(&mut adapter).await.unwrap();
			let seq = request["seq"].as_i64().unwrap();
			write_message(
				&mut adapter,
				&json!({"seq": 9, "type": "event", "event": "stopped", "body": {"reason": "entry"}}),
			)
			.await
			.unwrap();
			write_message(&mut adapter, &json!({"seq": 10, "type": "response", "request_seq": seq, "command": "threads", "success": true, "body": {"threads": []}})).await.unwrap();
		});
		let response = protocol.request("threads", json!({})).await.unwrap();
		assert_eq!(response["threads"], json!([]));
		assert!(
			matches!(events.recv().await.unwrap(), DapInbound::Event { event, .. } if event == "stopped")
		);
	}
}
