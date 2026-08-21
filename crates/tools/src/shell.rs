use std::{
	collections::BTreeMap,
	future::Future,
	sync::atomic::{AtomicBool, AtomicU64, Ordering},
	time::Duration,
};

use async_stream::stream;
use bytes::Bytes;
use futures::{FutureExt, Stream, future::Either, pin_mut};
use omp_core::{CowBytes, Str, sf};
use omp_proto::inference::v1::{InvokeInput, invoke_input};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, BlobRef, CommitError, Constraint, Effects, Ev, ExecEffects,
	IncomingParams, Interrupt, InterruptWaitError, ParamError, Part, PromptCaps, Rev, Tool,
	ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
	auto_background::{
		DEFAULT_AUTO_BACKGROUND_THRESHOLD, DetachedJob, ForegroundWait, JobWait,
		managed_job_terminal, next_background_name,
	},
	render::TextProjection,
};

fn omit_schema_format(schema: &mut schemars::Schema) {
	schema.remove("format");
}

/// Complete arguments for `shell@1`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[schemars(description = "")]
#[serde(deny_unknown_fields)]
#[schemars(extend(
	"allOf" = [{
		"if": {
			"properties": { "async": { "const": true } },
			"required": ["async"]
		},
		"then": { "required": ["name"] }
	}]
))]
pub struct Params {
	/// Shell script to execute.
	#[schemars(with = "String", length(min = 1), description = "Shell script to execute.")]
	pub command:      Str,
	/// Host-enforced execution timeout in milliseconds; zero disables the
	/// deadline.
	#[schemars(
		default,
		skip_serializing_if = "Option::is_none",
		with = "u64",
		range(min = 0),
		transform = omit_schema_format,
		description = "Host-enforced execution timeout in milliseconds; zero disables the deadline."
	)]
	pub timeout_ms:   Option<u64>,
	/// Environment additions scoped to this command.
	#[serde(default)]
	#[schemars(
		with = "BTreeMap<String, String>",
		description = "Environment additions scoped to this command."
	)]
	pub env:          BTreeMap<Str, Str>,
	/// Command working directory, relative to the workspace when not absolute.
	#[schemars(
		default,
		skip_serializing_if = "Option::is_none",
		with = "String",
		length(min = 1),
		description = "Command working directory, relative to the workspace when not absolute."
	)]
	pub cwd:          Option<Str>,
	/// Allocate a pseudo-terminal for this command.
	#[serde(default)]
	#[schemars(description = "Allocate a pseudo-terminal for this command.")]
	pub pty:          bool,
	/// Run as a named asynchronous job.
	#[serde(default, rename = "async")]
	#[schemars(description = "Run as a named asynchronous job.")]
	pub asynchronous: bool,
	/// Required stable job name when async is true.
	#[schemars(
		default,
		skip_serializing_if = "Option::is_none",
		with = "String",
		length(min = 1),
		description = "Required stable job name when async is true."
	)]
	pub name:         Option<Str>,
}
/// Ordered output channel from a shell command.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputChannel {
	/// Standard output.
	Stdout,
	/// Standard error.
	Stderr,
	/// Combined pseudo-terminal output.
	Pty,
}

/// One ordered live output update.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Update {
	/// Output stream carrying the bytes.
	pub channel:  OutputChannel,
	/// Exact output bytes.
	#[serde(with = "cow_bytes")]
	pub data:     CowBytes<'static>,
	/// Host-assigned ordering sequence.
	pub sequence: u64,
}

/// One retained output frame in the durable transcript.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TranscriptFrame {
	/// Output stream carrying the bytes.
	pub channel:  OutputChannel,
	/// Exact retained output bytes.
	#[serde(with = "cow_bytes")]
	pub data:     CowBytes<'static>,
	/// Host-assigned ordering sequence.
	pub sequence: u64,
}

/// Terminal process disposition reported by the environment owner.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutcome {
	/// The script exited normally.
	Exited,
	/// The script failed to launch or execute.
	Failed,
	/// The host-enforced deadline expired.
	Timeout,
	/// The request owner cancelled the command.
	Cancelled,
	/// Execution was denied by policy.
	Denied,
}

/// Complete terminal execution truth from the host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecStatus {
	/// Stable terminal disposition.
	pub outcome:         ExecOutcome,
	/// Process exit code when one exists.
	pub exit_code:       Option<i32>,
	/// Terminating signal when one exists.
	pub signal:          Option<Str>,
	/// Host-measured elapsed wall time.
	pub wall_clock_ms:   u64,
	/// Host-provided reference to output omitted from the live transcript.
	pub spilled_output:  Option<BlobRef>,
	/// Whether cancellation happened after launch.
	pub aborted:         bool,
	/// Whether the host cannot establish the final effect state.
	pub effects_unknown: bool,
}

/// An execution adjustment recorded in the durable call outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdjustmentReceipt {
	/// The requested finite deadline was clamped to the execution placement's
	/// bounds.
	TimeoutClamped {
		/// Model-requested deadline.
		requested_ms: u64,
		/// Deadline actually sent to the execution placement.
		effective_ms: u64,
	},
}

/// Durable foreground shell result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Payload {
	/// Stable identity of the environment session used for this run.
	pub session_id:  Bytes,
	/// Host identity of this command execution.
	pub exec_id:     Bytes,
	/// Exact submitted script after a leading `cd &&` was extracted.
	pub command:     Str,
	/// Ordered output retained whole in the durable call outcome.
	///
	/// The central call-outcome spill gate moves a large serialized outcome to a
	/// [`BlobRef`]; this executor never clips durable output.
	pub transcript:  Vec<TranscriptFrame>,
	/// Execution adjustments retained as journal receipts.
	pub adjustments: Vec<AdjustmentReceipt>,
	/// Terminal host status, preserved without reinterpretation.
	pub status:      ExecStatus,
}

/// Typed shell resource failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// The environment resource rejected or lost an operation.
	Resource {
		/// Operation that failed.
		operation: Str,
		/// Resource-owned diagnostic.
		message:   Str,
	},
	/// An environment key was not a portable shell identifier.
	InvalidEnvironmentKey {
		/// Rejected key.
		key: Str,
	},
	/// Asynchronous execution did not provide its required stable name.
	AsyncNameRequired,
}

/// Module-owned handle for one persistent environment session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
	/// Opaque environment session identifier, preserved byte-for-byte.
	pub id: Bytes,
}

/// Command-scoped session settings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionOptions {
	/// Requested working directory.
	pub cwd: Option<Str>,
	/// Scoped environment additions.
	pub env: BTreeMap<Str, Str>,
	/// Whether a pseudo-terminal is requested.
	pub pty: bool,
}

impl SessionOptions {
	fn is_default(&self) -> bool {
		self.cwd.is_none() && self.env.is_empty() && !self.pty
	}
}

/// Request to run one command in an existing session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
	/// Exact script text.
	pub command:    Str,
	/// Optional server-enforced timeout in milliseconds.
	pub timeout_ms: Option<u64>,
}

/// Request to create one persistent named process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DetachRequest {
	/// Stable process name.
	pub name:       Str,
	/// Exact script text.
	pub command:    Str,
	/// Optional server-enforced timeout in milliseconds.
	pub timeout_ms: Option<u64>,
	/// Session settings applied to the detached command.
	pub options:    SessionOptions,
}

/// One event consumed from a foreground environment run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunEvent {
	/// The host assigned an execution identity.
	Started {
		/// Stable host execution identity.
		exec_id: Bytes,
	},
	/// Ordered process output.
	Output(Update),
	/// Terminal process status.
	Exit(ExecStatus),
}

enum PendingRun {
	Event(Result<Option<RunEvent>, Fault>),
	Interrupt(Result<Interrupt, InterruptWaitError>),
	Background,
}

/// Request-scoped foreground run whose cancellation leaves its session open.
pub trait ShellRun: Send {
	/// Waits for the next ordered run event.
	fn next_event(&mut self) -> impl Future<Output = Result<Option<RunEvent>, Fault>> + Send + '_;

	/// Requests process-tree cancellation without closing the containing
	/// session.
	fn cancel(&self) -> impl Future<Output = Result<(), Fault>> + Send + '_;

	/// Transfers this in-flight execution to a named process.
	fn detach(&self, name: Str) -> impl Future<Output = Result<DetachedJob, Fault>> + Send + '_;
}

/// Zero-box environment resource boundary used by the native shell executor.
pub trait ShellExec: Clone + Send + Sync + 'static {
	/// Request-scoped run handle retaining the host cancellation guard.
	type Run: ShellRun;

	/// Opens an independent shell session with the given command-scoped
	/// settings.
	fn open_session(
		&self,
		options: SessionOptions,
	) -> impl Future<Output = Result<Session, Fault>> + Send + '_;

	/// Closes an isolated or quarantined session.
	fn close_session(
		&self,
		session: &Session,
	) -> impl Future<Output = Result<(), Fault>> + Send + '_;

	/// Starts a foreground script in the existing session.
	fn run<'a>(
		&'a self,
		session: &'a Session,
		request: RunRequest,
	) -> impl Future<Output = Result<Self::Run, Fault>> + Send + 'a;

	/// Transfers a script to the environment named-process owner.
	fn detach(
		&self,
		request: DetachRequest,
	) -> impl Future<Output = Result<DetachedJob, Fault>> + Send + '_;
}

/// Bounds enforced by the execution placement for finite shell deadlines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeoutBounds {
	/// Default deadline when the request omits `timeout_ms`.
	pub default_ms: u64,
	/// Minimum finite deadline accepted by the placement.
	pub floor_ms:   u64,
	/// Maximum finite deadline accepted by the placement.
	pub ceiling_ms: u64,
}

impl Default for TimeoutBounds {
	fn default() -> Self {
		Self { default_ms: 300_000, floor_ms: 1_000, ceiling_ms: 1_800_000 }
	}
}

/// Generic `shell@1` implementation retaining one lazy persistent session.
pub struct ShellTool<E: ShellExec> {
	exec: E,
	session: Mutex<Option<Session>>,
	persistent_run_active: AtomicBool,
	next_background_name: AtomicU64,
	timeout_bounds: TimeoutBounds,
	auto_background_threshold: Duration,
	spec: ToolSpec,
}

/// Constructs the native `shell@1` executor over an environment resource.
pub fn shell<E: ShellExec>(exec: E) -> ShellTool<E> {
	ShellTool {
		exec,
		session: Mutex::new(None),
		persistent_run_active: AtomicBool::new(false),
		next_background_name: AtomicU64::new(1),
		timeout_bounds: TimeoutBounds::default(),
		auto_background_threshold: DEFAULT_AUTO_BACKGROUND_THRESHOLD,
		spec: ToolSpec {
			name:            sf!("shell"),
			rev:             Rev { family: Str::default(), n: 1 },
			description:     sf!(
				"Execute a shell script in a persistent session, or start a named asynchronous job.",
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects {
				documents: None,
				exec:      Some(ExecEffects {
					commands: [sf!("*")].into_iter().collect(),
					network:  true,
				}),
				inference: None,
				subagents: 0,
			},
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("shell.rs"),
			)
			.into(),
		},
	}
}
/// Constructs the shell executor with execution-placement timeout bounds.
pub fn shell_with_timeout_bounds<E: ShellExec>(
	exec: E,
	timeout_bounds: TimeoutBounds,
) -> ShellTool<E> {
	shell(exec).with_timeout_bounds(timeout_bounds)
}

impl<E: ShellExec> ShellTool<E> {
	/// Overrides the finite timeout bounds supplied by the execution placement.
	#[must_use]
	pub const fn with_timeout_bounds(mut self, timeout_bounds: TimeoutBounds) -> Self {
		self.timeout_bounds = timeout_bounds;
		self
	}

	/// Overrides how long foreground commands wait before managed detachment.
	#[must_use]
	pub const fn with_auto_background_threshold(mut self, threshold: Duration) -> Self {
		self.auto_background_threshold = threshold;
		self
	}

	async fn persistent_session(&self) -> Result<Session, Fault> {
		let mut session = self.session.lock().await;
		if let Some(session) = session.as_ref() {
			return Ok(session.clone());
		}
		let opened = self.exec.open_session(SessionOptions::default()).await?;
		*session = Some(opened.clone());
		Ok(opened)
	}

	async fn finish_session(&self, session: &Session, persistent: bool, quarantine: bool) {
		if persistent {
			if quarantine {
				let discarded = {
					let mut pooled = self.session.lock().await;
					pooled.take()
				};
				if let Some(discarded) = discarded {
					let _ = self.exec.close_session(&discarded).await;
				}
			}
			self.persistent_run_active.store(false, Ordering::Release);
		} else {
			let _ = self.exec.close_session(session).await;
		}
	}

	fn timeout(&self, requested: Option<u64>) -> (Option<u64>, Vec<AdjustmentReceipt>) {
		let Some(requested) = requested else {
			return (Some(self.timeout_bounds.default_ms), Vec::new());
		};
		if requested == 0 {
			return (None, Vec::new());
		}
		let floor = self.timeout_bounds.floor_ms;
		let ceiling = self.timeout_bounds.ceiling_ms.max(floor);
		let effective = requested.clamp(floor, ceiling);
		let adjustments = (effective != requested)
			.then_some(AdjustmentReceipt::TimeoutClamped {
				requested_ms: requested,
				effective_ms: effective,
			})
			.into_iter()
			.collect();
		(Some(effective), adjustments)
	}
}

impl<E: ShellExec> Tool for ShellTool<E> {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		stream! {
			let args = match params.whole::<Params>().await {
				Ok(args) => args,
				Err(error) => {
					yield param_event(error);
					return;
				},
			};
			if let Err(error) = params.committed().await {
				yield commit_event(error);
				return;
			}
			if let Some(interrupt) = params.take_interrupt() {
				yield Ev::Aborted(Abort::Skipped { reason: interrupt.reason });
				return;
			}
			if let Some(Ok(interrupt)) = params.next_interrupt().now_or_never() {
				yield Ev::Aborted(Abort::Skipped { reason: interrupt.reason });
				return;
			}
			if let Some(key) = args.env.keys().find(|key| !valid_env_key(key)).cloned() {
				yield Ev::Done(ToolTerminal::Done {
					result: Err(Fault::InvalidEnvironmentKey { key }),
					useless: false,
				});
				return;
			}

			let (command, extracted_cwd) = extract_leading_cd(&args.command);
			let options = SessionOptions {
				cwd: args.cwd.or(extracted_cwd),
				env: args.env,
				pty: args.pty,
			};
			let (timeout_ms, adjustments) = self.timeout(args.timeout_ms);

			if args.asynchronous {
				let Some(name) = args.name else {
					yield Ev::Done(ToolTerminal::Done { result: Err(Fault::AsyncNameRequired), useless: false });
					return;
				};
				let work = self.exec.detach(DetachRequest {
					name,
					command,
					timeout_ms,
					options,
				}).fuse();
				let interrupt = params.next_interrupt().fuse();
				pin_mut!(work, interrupt);
				match futures::future::select(interrupt, work).await {
					Either::Left((interrupt, _)) => {
						let reason = interrupt_reason(interrupt, "invocation owner disappeared during async setup");
						yield Ev::Aborted(Abort::EffectsUnknown { reason });
					},
					Either::Right((Ok(job), _)) => yield Ev::Done(detached_terminal(job)),
					Either::Right((Err(fault), _)) => {
						yield Ev::Done(ToolTerminal::Done { result: Err(fault), useless: false });
					},
				}
				return;
			}

			let persistent = options.is_default()
				&& self
					.persistent_run_active
					.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
					.is_ok();
			let session = if persistent {
				self.persistent_session().await
			} else {
				self.exec.open_session(options).await
			};
			let session = match session {
				Ok(session) => session,
				Err(fault) => {
					if persistent {
						self.persistent_run_active.store(false, Ordering::Release);
					}
					yield Ev::Done(ToolTerminal::Done { result: Err(fault), useless: false });
					return;
				},
			};
			let session_id = session.id.clone();
			let mut run = match self.exec.run(&session, RunRequest {
				command: command.clone(),
				timeout_ms,
			}).await {
				Ok(run) => run,
				Err(fault) => {
					self.finish_session(&session, persistent, true).await;
					yield Ev::Done(ToolTerminal::Done { result: Err(fault), useless: false });
					return;
				},
			};
			let foreground_wait = ForegroundWait::new(
				self.auto_background_threshold,
				timeout_ms.map(Duration::from_millis),
			);
			let mut auto_background = true;

			let mut exec_id = Bytes::new();
			let mut started = false;
			let mut transcript = Vec::new();
			let mut cancellation_reason: Option<Str> = None;
			loop {
				let event = if cancellation_reason.is_some() {
					run.next_event().await
				} else {
					let pending = if auto_background {
						match foreground_wait
							.race(run.next_event(), params.next_interrupt())
							.await
						{
							JobWait::Settled(event) => PendingRun::Event(event),
							JobWait::Interrupted(interrupt) => PendingRun::Interrupt(interrupt),
							JobWait::Background => PendingRun::Background,
						}
					} else {
						let next = run.next_event().fuse();
						let interrupt = params.next_interrupt().fuse();
						pin_mut!(next, interrupt);
						match futures::future::select(interrupt, next).await {
							Either::Right((event, _)) => PendingRun::Event(event),
							Either::Left((interrupt, _)) => PendingRun::Interrupt(interrupt),
						}
					};
					match pending {
						PendingRun::Background => {
							let name =
								next_background_name("shell", &self.next_background_name);
							if let Ok(job) = run.detach(name).await {
											 self.finish_session(&session, persistent, true).await;
											 yield Ev::Done(detached_terminal(job));
											 return;
										 }
											 auto_background = false;
											 continue;
						},
						PendingRun::Event(event) => event,
						PendingRun::Interrupt(interrupt) => {
							let interrupt = match interrupt {
								Ok(interrupt) => interrupt,
								Err(InterruptWaitError::Closed) => Interrupt {
									class: sf!("closed"),
									reason: sf!("invocation owner disappeared"),
								},
								Err(InterruptWaitError::Protocol(reason)) => Interrupt {
									class: sf!("protocol"),
									reason,
								},
							};
							if interrupt.class == Interrupt::STEERING {
								let name =
									next_background_name("shell", &self.next_background_name);
								if let Ok(job) = run.detach(name).await {
									self.finish_session(&session, persistent, true).await;
									yield Ev::Done(detached_terminal(job));
									return;
								}
							}
							let reason = interrupt.reason;
							if run.cancel().await.is_err() {
								self.finish_session(&session, persistent, true).await;
								yield Ev::Aborted(Abort::EffectsUnknown { reason });
								return;
							}
							cancellation_reason = Some(reason);
							continue;
						},
					}
				};

				match event {
					Ok(Some(RunEvent::Started { exec_id: id })) => {
						exec_id = id;
						started = true;
					},
					Ok(Some(RunEvent::Output(update))) => {
						transcript.push(TranscriptFrame {
							channel: update.channel,
							data: update.data.clone(),
							sequence: update.sequence,
						});
						yield Ev::Update(update);
					},
					Ok(Some(RunEvent::Exit(status)))
						if !started
							&& status.outcome == ExecOutcome::Cancelled
							&& !status.effects_unknown
							&& cancellation_reason.is_some() =>
					{
						self.finish_session(&session, persistent, true).await;
						yield Ev::Aborted(Abort::Skipped {
							reason: cancellation_reason.take().expect("guarded by is_some"),
						});
						return;
					},
					Ok(Some(RunEvent::Exit(status))) => {
						let quarantine = status.aborted
							|| matches!(status.outcome, ExecOutcome::Timeout | ExecOutcome::Cancelled)
							|| status.effects_unknown;
						self.finish_session(&session, persistent, quarantine).await;
						yield Ev::Done(ToolTerminal::Done {
							result: Ok(Payload {
								session_id,
								exec_id,
								command,
								transcript,
								adjustments,
								status,
							}),
							useless: false,
						});
						return;
					},
					Ok(None) => {
						self.finish_session(&session, persistent, true).await;
						yield Ev::Aborted(Abort::EffectsUnknown {
							reason: cancellation_reason.unwrap_or_else(|| sf!("exec event stream ended before terminal status")),
						});
						return;
					},
					Err(fault) => {
						self.finish_session(&session, persistent, true).await;
						yield Ev::Aborted(Abort::EffectsUnknown { reason: Str::new(fault_reason(&fault)) });
						return;
					},
				}
			}
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, caps: &PromptCaps) -> Vec<Part> {
		let Some(mut projection) = TextProjection::new(*caps) else {
			return Vec::new();
		};
		match view {
			Ok(payload) => {
				let status = format!(
					"[status={:?}; exit={:?}; signal={:?}; {}ms{}]\n",
					payload.status.outcome,
					payload.status.exit_code,
					payload.status.signal,
					payload.status.wall_clock_ms,
					if payload.status.spilled_output.is_some() {
						"; output blob attached"
					} else {
						""
					},
				);
				if projection.push(&status) {
					for adjustment in &payload.adjustments {
						let AdjustmentReceipt::TimeoutClamped { requested_ms, effective_ms } = adjustment;
						if !projection.push(&format!(
							"[timeout adjusted from {requested_ms}ms to {effective_ms}ms]\n"
						)) {
							break;
						}
					}
					push_transcript_head_tail(&mut projection, &payload.transcript);
				}
			},
			Err(fault) => {
				projection.push(&fault_reason(fault));
			},
		}
		projection.finish()
	}

	fn invoke_input(&self, update: &Update, invocation_id: &str) -> Option<InvokeInput> {
		let channel = match update.channel {
			OutputChannel::Stdout | OutputChannel::Pty => invoke_input::chunk::Channel::Stdout,
			OutputChannel::Stderr => invoke_input::chunk::Channel::Stderr,
		};
		Some(InvokeInput {
			invocation_id: invocation_id.to_owned(),
			payload:       Some(invoke_input::Payload::Chunk(invoke_input::Chunk {
				channel: channel as i32,
				data:    update.data.clone().into_bytes(),
			})),
		})
	}
}

fn detached_terminal(job: DetachedJob) -> ToolTerminal<Payload, Fault> {
	managed_job_terminal(job, sf!("named process settlement"))
}

fn interrupt_reason(
	interrupt: Result<Interrupt, InterruptWaitError>,
	closed_reason: &'static str,
) -> Str {
	match interrupt {
		Ok(interrupt) => interrupt.reason,
		Err(InterruptWaitError::Closed) => Str::new(closed_reason),
		Err(InterruptWaitError::Protocol(reason)) => reason,
	}
}

fn fault_reason(fault: &Fault) -> String {
	match fault {
		Fault::Resource { operation, message } => format!("shell {operation} failed: {message}"),
		Fault::InvalidEnvironmentKey { key } => format!("invalid shell environment key {key:?}"),
		Fault::AsyncNameRequired => String::from("shell async execution requires a non-empty name"),
	}
}

fn valid_env_key(key: &str) -> bool {
	let mut bytes = key.bytes();
	matches!(bytes.next(), Some(b'_' | b'a'..=b'z' | b'A'..=b'Z'))
		&& bytes.all(|byte| matches!(byte, b'_' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'))
}

fn extract_leading_cd(command: &Str) -> (Str, Option<Str>) {
	let bytes = command.as_bytes();
	if !bytes.starts_with(b"cd") || bytes.get(2).is_none_or(|byte| !byte.is_ascii_whitespace()) {
		return (command.clone(), None);
	}
	let mut cursor = skip_space(bytes, 2);
	let Some((mut cwd, after_cwd)) = shell_word(bytes, cursor) else {
		return (command.clone(), None);
	};
	cursor = after_cwd;
	if cwd == "--" {
		cursor = skip_space(bytes, cursor);
		let Some((path, after_path)) = shell_word(bytes, cursor) else {
			return (command.clone(), None);
		};
		cwd = path;
		cursor = after_path;
	}
	cursor = skip_space(bytes, cursor);
	if bytes.get(cursor..cursor.saturating_add(2)) != Some(b"&&") {
		return (command.clone(), None);
	}
	cursor = skip_space(bytes, cursor + 2);
	if cursor == bytes.len() {
		return (command.clone(), None);
	}
	(Str::new(String::from_utf8_lossy(&bytes[cursor..])), Some(Str::new(cwd)))
}

fn skip_space(bytes: &[u8], mut cursor: usize) -> usize {
	while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
		cursor += 1;
	}
	cursor
}

fn shell_word(bytes: &[u8], start: usize) -> Option<(String, usize)> {
	let quote = *bytes.get(start)?;
	if quote == b'\'' || quote == b'"' {
		let mut cursor = start + 1;
		let mut word = Vec::new();
		while let Some(&byte) = bytes.get(cursor) {
			cursor += 1;
			if byte == quote {
				return Some((String::from_utf8_lossy(&word).into_owned(), cursor));
			}
			if byte == b'\\' && quote == b'"' {
				if let Some(&escaped) = bytes.get(cursor) {
					word.push(escaped);
					cursor += 1;
				}
			} else {
				word.push(byte);
			}
		}
		return None;
	}
	let mut cursor = start;
	while let Some(&byte) = bytes.get(cursor) {
		if byte.is_ascii_whitespace() || byte == b'&' {
			break;
		}
		cursor += 1;
	}
	(cursor != start).then(|| (String::from_utf8_lossy(&bytes[start..cursor]).into_owned(), cursor))
}

fn push_transcript_head_tail(projection: &mut TextProjection, transcript: &[TranscriptFrame]) {
	const FRAMES: usize = 8;
	let split = transcript.len().min(FRAMES);
	for frame in &transcript[..split] {
		if !projection.push(&String::from_utf8_lossy(&frame.data)) {
			return;
		}
	}
	if transcript.len() > FRAMES * 2
		&& !projection.push("\n[output middle omitted from projection]\n")
	{
		return;
	}
	let tail_start = transcript.len().saturating_sub(FRAMES).max(split);
	for frame in &transcript[tail_start..] {
		if !projection.push(&String::from_utf8_lossy(&frame.data)) {
			return;
		}
	}
}

fn param_event<U, P>(error: ParamError) -> Ev<U, P, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Skipped { reason: interrupt.reason })
		},
		ParamError::Protocol(reason) => Ev::Args(protocol_issue(reason)),
	}
}

fn commit_event<U, P>(error: CommitError) -> Ev<U, P, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Skipped { reason: interrupt.reason })
		},
		CommitError::Protocol(reason) => Ev::Args(protocol_issue(reason)),
	}
}

fn protocol_issue(reason: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one complete shell@1 argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"command":"printf hello"}}"#)),
		found:    Some(reason),
	}
}

mod cow_bytes {
	use omp_core::CowBytes;
	use serde::{Deserialize, Deserializer, Serialize, Serializer};

	pub(super) fn serialize<S: Serializer>(
		value: &CowBytes<'static>,
		serializer: S,
	) -> Result<S::Ok, S::Error> {
		value.serialize(serializer)
	}

	pub(super) fn deserialize<'de, D: Deserializer<'de>>(
		deserializer: D,
	) -> Result<CowBytes<'static>, D::Error> {
		Vec::<u8>::deserialize(deserializer).map(CowBytes::from)
	}
}
