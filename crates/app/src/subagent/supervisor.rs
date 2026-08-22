//! Durable session supervisor for owned subagent loops.

use std::{
	collections::HashMap,
	future::Future,
	pin::Pin,
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use omp_agent::{
	AbortHandle, Agent, AgentError, AgentEvent, AgentNode, AgentRunSummary, AgentStatus, AgentTree,
	Interrupt, InterruptClass, InterruptSource, JobBoard, SubagentActivity, SubagentActivityKind,
	SubagentDisposition, SubagentLifecycle, SubagentRunState, SubagentStateError,
	SubagentTerminalKind, SubagentTerminalStatus, TurnClient, TurnId,
};
use omp_core::{Str, sf};
use omp_proto::{
	inference::v1::turn_event,
	thread::v1::{self as thread, Item},
};
use omp_tool::{ArtifactLifetime, ExpectedArtifact, JobKind, JobMetadata, JobOwner, JobRef};
use parking_lot::RwLock;
use thiserror::Error;

use super::settings::TaskSettings;

/// Cold revival future. This allocation occurs only after memory parking, not
/// on a request, token, or tool-call path.
pub type RevivalFuture<C> =
	Pin<Box<dyn Future<Output = Result<SupervisedRuntime<C>, SupervisorError>> + Send + 'static>>;

/// Reconstructs an equivalent child loop from its durable journal and
/// snapshots.
pub trait ChildReviver<C: TurnClient>: Send + Sync + 'static {
	/// Rebuilds the live runtime after memory parking.
	fn revive(&self) -> RevivalFuture<C>;
}

/// Opaque live resources retained for exactly as long as a child loop.
pub trait ChildResource: Send + 'static {}

impl<T: Send + 'static> ChildResource for T {}

/// Live child loop plus application bindings owned by the supervisor actor.
pub struct SupervisedRuntime<C: TurnClient> {
	agent:     Agent<C>,
	resources: Vec<Box<dyn ChildResource>>,
}

impl<C: TurnClient> SupervisedRuntime<C> {
	/// Creates a supervised runtime around a fully configured durable loop.
	#[must_use]
	pub fn new(agent: Agent<C>) -> Self {
		Self { agent, resources: Vec::new() }
	}

	/// Retains an environment, control lease, hub attachment, or other binding.
	pub fn retain(&mut self, resource: impl ChildResource) {
		self.resources.push(Box::new(resource));
	}

	/// Returns the live child loop before it is registered with a supervisor.
	pub const fn agent(&self) -> &Agent<C> {
		&self.agent
	}
}

struct ChildHandle {
	commands: flume::Sender<ChildCommand>,
	abort:    Arc<RwLock<Option<AbortHandle>>>,
	state:    Arc<SubagentRunState>,
}

struct RunCommand {
	items:    Vec<Item>,
	turn_id:  TurnId,
	settings: Arc<TaskSettings>,
	reply:    flume::Sender<Result<AgentRunSummary, SupervisorError>>,
}

enum ChildCommand {
	Run(RunCommand),
	Park(ParkReason, flume::Sender<Result<(), SupervisorError>>),
	Teardown(flume::Sender<()>),
}
#[derive(Clone, Copy, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum ParkReason {
	Parked,
	Stop,
}

/// Session-owned durable child-loop authority.
pub struct SessionSupervisor<C: TurnClient + Send + 'static> {
	tree:        Arc<AgentTree>,
	children:    RwLock<HashMap<Str, ChildHandle>>,
	settings:    RwLock<Arc<TaskSettings>>,
	parent_jobs: RwLock<Option<Arc<JobBoard>>>,
	_marker:     std::marker::PhantomData<fn() -> C>,
}

impl<C: TurnClient + Send + 'static> SessionSupervisor<C> {
	/// Creates one supervisor for a session's complete child roster.
	#[must_use]
	pub fn new(tree: Arc<AgentTree>) -> Self {
		Self {
			tree,
			children: RwLock::new(HashMap::new()),
			settings: RwLock::new(Arc::new(TaskSettings::default())),
			parent_jobs: RwLock::new(None),
			_marker: std::marker::PhantomData,
		}
	}

	/// Replaces the live settings snapshot used by later runs.
	pub fn apply_settings(&self, settings: Arc<TaskSettings>) {
		*self.settings.write() = settings;
	}

	/// Binds the parent agent's authoritative detached-job board.
	pub fn bind_parent_jobs(&self, jobs: Arc<JobBoard>) {
		*self.parent_jobs.write() = Some(jobs);
	}

	/// Returns the parent board used for self-delivering durable child turns.
	#[must_use]
	pub fn parent_jobs(&self) -> Option<Arc<JobBoard>> {
		self.parent_jobs.read().clone()
	}

	/// Registers and starts ownership of one configured child loop.
	pub fn register(
		&self,
		node: Arc<AgentNode>,
		runtime: SupervisedRuntime<C>,
		reviver: Option<Arc<dyn ChildReviver<C>>>,
	) -> Result<Arc<SubagentRunState>, SupervisorError> {
		let id = node.id.clone();
		let mut children = self.children.write();
		if children.contains_key(&id) {
			return Err(SupervisorError::AlreadyRegistered { id });
		}
		let state = Arc::new(SubagentRunState::new(id.clone()));
		let abort = Arc::new(RwLock::new(Some(runtime.agent.abort_handle())));
		let (commands, receiver) = flume::unbounded();
		let tree = Arc::clone(&self.tree);
		let loop_state = Arc::clone(&state);
		tokio::spawn(child_loop(
			node,
			tree,
			Some(runtime),
			reviver,
			Arc::clone(&abort),
			loop_state,
			receiver,
		));
		children.insert(id, ChildHandle { commands, abort, state: Arc::clone(&state) });
		Ok(state)
	}

	/// Registers a journal-recovered identity without constructing live
	/// resources.
	pub fn register_parked(
		&self,
		node: Arc<AgentNode>,
		reviver: Arc<dyn ChildReviver<C>>,
	) -> Result<Arc<SubagentRunState>, SupervisorError> {
		let id = node.id.clone();
		let mut children = self.children.write();
		if children.contains_key(&id) {
			return Err(SupervisorError::AlreadyRegistered { id });
		}
		let state = Arc::new(SubagentRunState::new(id.clone()));
		state.transition(SubagentLifecycle::Settled)?;
		state.transition(SubagentLifecycle::Parked)?;
		let abort = Arc::new(RwLock::new(None));
		let (commands, receiver) = flume::unbounded();
		let tree = Arc::clone(&self.tree);
		let loop_state = Arc::clone(&state);
		tokio::spawn(child_loop(
			node,
			tree,
			None,
			Some(reviver),
			Arc::clone(&abort),
			loop_state,
			receiver,
		));
		children.insert(id, ChildHandle { commands, abort, state: Arc::clone(&state) });
		Ok(state)
	}

	/// Runs a first turn or serialized follow-up on a retained child loop.
	pub async fn run(
		&self,
		id: &str,
		items: Vec<Item>,
		turn_id: TurnId,
	) -> Result<AgentRunSummary, SupervisorError> {
		let commands = self
			.children
			.read()
			.get(id)
			.map(|child| child.commands.clone())
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		let (reply, response) = flume::bounded(1);
		let settings = Arc::clone(&self.settings.read());
		commands
			.send_async(ChildCommand::Run(RunCommand { items, turn_id, settings, reply }))
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?;
		response
			.recv_async()
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?
	}

	/// Starts one background child run registered through the parent JobBoard.
	///
	/// The returned job reference is process-local; the durable agent identity
	/// remains the `AgentLoop` owner and survives job-row retention.
	pub async fn run_detached(
		&self,
		id: &str,
		items: Vec<Item>,
		turn_id: TurnId,
	) -> Result<JobRef, SupervisorError> {
		let commands = self
			.children
			.read()
			.get(id)
			.map(|child| child.commands.clone())
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		let board = self
			.parent_jobs
			.read()
			.clone()
			.ok_or(SupervisorError::JobBoardUnavailable)?;
		let now = now_ms();
		let job = JobRef {
			id:       board.next_id(),
			owner:    JobOwner::AgentLoop { agent_id: Str::new(id) },
			metadata: Arc::new(JobMetadata::running(JobKind::Task, sf!("subagent:{}", id), now)),
			artifact: ExpectedArtifact {
				description: sf!("durable subagent result"),
				media_type:  Some(sf!("application/vnd.omp.subagent-result+json")),
				lifetime:    ArtifactLifetime::Durable,
			},
		};
		if !board
			.try_register(job.clone())
			.map_err(|_| SupervisorError::JobCapacity)?
		{
			return Err(SupervisorError::DuplicateJob { id: job.id.clone() });
		}
		let (reply, response) = flume::bounded(1);
		let settings = Arc::clone(&self.settings.read());
		commands
			.send_async(ChildCommand::Run(RunCommand { items, turn_id, settings, reply }))
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?;
		let settlement_board = board;
		let settlement_id = job.id.clone();
		tokio::spawn(async move {
			let text = match response.recv_async().await {
				Ok(Ok(summary)) => summary
					.final_assistant()
					.map_or_else(|| sf!("subagent completed"), Str::new),
				Ok(Err(_)) => sf!("subagent failed; inspect its durable history for details"),
				Err(_) => sf!("subagent supervisor stopped before settlement"),
			};
			let _ = settlement_board.settle(settlement_id.as_str(), system_item(text));
		});
		Ok(job)
	}

	/// Cancels the active generation without destroying its durable identity.
	pub fn cancel(&self, id: &str) -> Result<(), SupervisorError> {
		let children = self.children.read();
		let child = children
			.get(id)
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		if let Some(abort) = child.abort.read().as_ref() {
			abort.abort();
		}
		Ok(())
	}

	/// Releases one idle loop's live resources while retaining its state and
	/// reviver.
	pub async fn park(&self, id: &str) -> Result<(), SupervisorError> {
		self.park_with_reason(id, ParkReason::Parked).await
	}

	/// Releases a cancelled loop within the stop lifecycle.
	pub async fn park_stopped(&self, id: &str) -> Result<(), SupervisorError> {
		self.park_with_reason(id, ParkReason::Stop).await
	}

	async fn park_with_reason(&self, id: &str, reason: ParkReason) -> Result<(), SupervisorError> {
		let commands = self
			.children
			.read()
			.get(id)
			.map(|child| child.commands.clone())
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		let (reply, response) = flume::bounded(1);
		commands
			.send_async(ChildCommand::Park(reason, reply))
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?;
		response
			.recv_async()
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?
	}

	/// Returns retained state without requiring a live listener or child loop.
	#[must_use]
	pub fn state(&self, id: &str) -> Option<Arc<SubagentRunState>> {
		self
			.children
			.read()
			.get(id)
			.map(|child| Arc::clone(&child.state))
	}

	/// Cancels and tears down one live actor at session shutdown.
	pub async fn teardown(&self, id: &str) -> Result<(), SupervisorError> {
		let child = self
			.children
			.write()
			.remove(id)
			.ok_or_else(|| SupervisorError::UnknownAgent { id: Str::from(id) })?;
		if let Some(abort) = child.abort.read().as_ref() {
			abort.abort();
		}
		let (reply, response) = flume::bounded(1);
		child
			.commands
			.send_async(ChildCommand::Teardown(reply))
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })?;
		response
			.recv_async()
			.await
			.map_err(|_| SupervisorError::Stopped { id: Str::from(id) })
	}
}

impl<C: TurnClient + Send + 'static> Drop for SessionSupervisor<C> {
	fn drop(&mut self) {
		for (_, child) in self.children.get_mut().drain() {
			if let Some(abort) = child.abort.read().as_ref() {
				abort.abort();
			}
			let (reply, _) = flume::bounded(1);
			let _ = child.commands.send(ChildCommand::Teardown(reply));
		}
	}
}

async fn child_loop<C: TurnClient + Send + 'static>(
	node: Arc<AgentNode>,
	tree: Arc<AgentTree>,
	mut runtime: Option<SupervisedRuntime<C>>,
	reviver: Option<Arc<dyn ChildReviver<C>>>,
	abort: Arc<RwLock<Option<AbortHandle>>>,
	state: Arc<SubagentRunState>,
	commands: flume::Receiver<ChildCommand>,
) {
	while let Ok(command) = commands.recv_async().await {
		match command {
			ChildCommand::Run(command) => {
				let result = run_child(
					&node,
					&tree,
					&state,
					&mut runtime,
					reviver.as_ref(),
					&abort,
					command.items,
					command.turn_id,
					&command.settings,
				)
				.await;
				let _ = command.reply.send(result);
			},
			ChildCommand::Park(reason, reply) => {
				let result = if reviver.is_none() {
					Err(SupervisorError::RevivalUnavailable { id: node.id.clone() })
				} else if state.lifecycle() != SubagentLifecycle::Settled {
					Err(SupervisorError::NotIdle { id: node.id.clone() })
				} else {
					let journaled = runtime.as_mut().map_or(Ok(()), |runtime| {
						record_lifecycle(runtime, &state, &node.id, <&'static str>::from(reason), None)
					});
					journaled.and_then(|()| {
						runtime = None;
						node.set_status(AgentStatus::Settled);
						state
							.transition(SubagentLifecycle::Parked)
							.map_err(SupervisorError::State)
					})
				};
				let _ = reply.send(result);
			},
			ChildCommand::Teardown(reply) => {
				drop(runtime.take());
				let _ = reply.send(());
				break;
			},
		}
	}
}

async fn run_child<C: TurnClient + Send + 'static>(
	node: &AgentNode,
	tree: &AgentTree,
	state: &SubagentRunState,
	runtime: &mut Option<SupervisedRuntime<C>>,
	reviver: Option<&Arc<dyn ChildReviver<C>>>,
	abort: &RwLock<Option<AbortHandle>>,
	items: Vec<Item>,
	turn_id: TurnId,
	settings: &TaskSettings,
) -> Result<AgentRunSummary, SupervisorError> {
	let first_turn = state.lifecycle() == SubagentLifecycle::Created;
	let reopening = state.lifecycle() == SubagentLifecycle::Parked;
	match state.lifecycle() {
		SubagentLifecycle::Created => state.transition(SubagentLifecycle::Starting)?,
		SubagentLifecycle::Settled | SubagentLifecycle::Parked => {
			state.begin_generation()?;
		},
		lifecycle => return Err(SupervisorError::NotIdleState { id: node.id.clone(), lifecycle }),
	}
	if runtime.is_none() {
		let factory =
			reviver.ok_or_else(|| SupervisorError::RevivalUnavailable { id: node.id.clone() })?;
		*runtime = Some(factory.revive().await?);
		*abort.write() = Some(
			runtime
				.as_ref()
				.expect("reviver produced a live runtime")
				.agent
				.abort_handle(),
		);
	}
	let runtime = runtime
		.as_mut()
		.expect("runtime was restored before lifecycle publication");
	record_lifecycle(
		runtime,
		state,
		&node.id,
		if first_turn {
			"spawn"
		} else if reopening {
			"reopen"
		} else {
			"turn-started"
		},
		None,
	)?;
	if first_turn || reopening {
		record_lifecycle(runtime, state, &node.id, "turn-started", None)?;
	}
	let permit = tree.admit(1).await?;
	state.transition(SubagentLifecycle::Running)?;
	state.record_activity(SubagentActivity {
		kind: Some(if first_turn {
			SubagentActivityKind::FirstTurn
		} else {
			SubagentActivityKind::FollowUp
		}),
		detail: sf!(if first_turn {
			"first turn"
		} else {
			"follow-up turn"
		}),
		..SubagentActivity::default()
	})?;
	node.set_status(AgentStatus::Running);
	let result = supervised_submit(state, runtime, abort, items, turn_id, settings).await;
	drop(permit);
	match result {
		Ok(summary) => {
			let kind = if summary.interrupted {
				SubagentTerminalKind::Cancelled
			} else {
				SubagentTerminalKind::Succeeded
			};
			settle(state, kind)?;
			record_lifecycle(runtime, state, &node.id, "turn-settled", Some(kind))?;
			node.set_status(if summary.interrupted {
				AgentStatus::Cancelled
			} else {
				AgentStatus::Completed
			});
			Ok(summary)
		},
		Err(SupervisorError::RuntimeLimit { .. }) => {
			settle(state, SubagentTerminalKind::RuntimeLimit)?;
			record_lifecycle(
				runtime,
				state,
				&node.id,
				"turn-settled",
				Some(SubagentTerminalKind::RuntimeLimit),
			)?;
			node.set_status(AgentStatus::Exhausted);
			result
		},
		Err(SupervisorError::RequestBudget { .. }) => {
			settle(state, SubagentTerminalKind::Failed)?;
			record_lifecycle(
				runtime,
				state,
				&node.id,
				"turn-settled",
				Some(SubagentTerminalKind::Failed),
			)?;
			node.set_status(AgentStatus::Exhausted);
			result
		},
		Err(SupervisorError::Agent(AgentError::Interrupted)) => {
			settle(state, SubagentTerminalKind::Cancelled)?;
			record_lifecycle(
				runtime,
				state,
				&node.id,
				"turn-settled",
				Some(SubagentTerminalKind::Cancelled),
			)?;
			node.set_status(AgentStatus::Cancelled);
			Err(SupervisorError::Agent(AgentError::Interrupted))
		},
		Err(error) => {
			settle(state, SubagentTerminalKind::Failed)?;
			record_lifecycle(
				runtime,
				state,
				&node.id,
				"turn-settled",
				Some(SubagentTerminalKind::Failed),
			)?;
			node.set_status(AgentStatus::Failed);
			Err(error)
		},
	}
}

fn record_lifecycle<C: TurnClient>(
	runtime: &mut SupervisedRuntime<C>,
	state: &SubagentRunState,
	child_id: &Str,
	lifecycle: &'static str,
	terminal: Option<SubagentTerminalKind>,
) -> Result<(), SupervisorError> {
	runtime.agent.record_child_lifecycle(
		now_ms(),
		omp_storage::transcript::ChildLifecycleEntry {
			child_id:        child_id.clone(),
			generation:      state.generation().0,
			init_event:      0,
			lifecycle:       Str::new(lifecycle),
			terminal_status: terminal.map(|kind| Str::from(kind.to_string())),
		},
	)?;
	Ok(())
}

async fn supervised_submit<C: TurnClient + Send + 'static>(
	state: &SubagentRunState,
	runtime: &mut SupervisedRuntime<C>,
	abort: &RwLock<Option<AbortHandle>>,
	items: Vec<Item>,
	turn_id: TurnId,
	settings: &TaskSettings,
) -> Result<AgentRunSummary, SupervisorError> {
	let events = runtime.agent.events().subscribe_lossless();
	let mailbox = runtime.agent.mailbox();
	let submission = runtime.agent.submit(items, turn_id);
	tokio::pin!(submission);
	let deadline =
		tokio::time::Instant::now() + Duration::from_millis(settings.max_runtime_ms.max(1));
	loop {
		tokio::select! {
			biased;
			result = &mut submission => return result.map_err(SupervisorError::Agent),
			event = events.recv() => {
				let Ok(event) = event else {
					continue;
				};
				if handle_event(state, event.as_ref(), &mailbox, settings, abort)? {
					return Err(SupervisorError::RequestBudget {
						requests: state.progress().requests,
						budget: settings.soft_request_budget,
					});
				}
			},
			() = tokio::time::sleep_until(deadline), if settings.max_runtime_ms != 0 => {
				if let Some(abort) = abort.read().as_ref() {
					abort.abort();
				}
				let _ = tokio::time::timeout(Duration::from_secs(5), &mut submission).await;
				return Err(SupervisorError::RuntimeLimit {
					max_runtime_ms: settings.max_runtime_ms,
				});
			},
		}
	}
}

fn handle_event(
	state: &SubagentRunState,
	event: &AgentEvent,
	mailbox: &omp_agent::MailboxSender,
	settings: &TaskSettings,
	abort: &RwLock<Option<AbortHandle>>,
) -> Result<bool, SupervisorError> {
	match event {
		AgentEvent::Turn { event, .. } => match event.event.as_ref() {
			Some(turn_event::Event::Accepted(accepted)) if !accepted.replay => {
				state.record_activity(SubagentActivity {
					kind: Some(SubagentActivityKind::Request),
					detail: sf!("assistant request"),
					..SubagentActivity::default()
				})?;
				let requests = state.progress().requests;
				let budget = settings.soft_request_budget;
				if budget == 0 {
					return Ok(false);
				}
				let stop = u64::from(budget).saturating_mul(3).saturating_add(1) / 2;
				if settings.soft_request_budget_notice && requests == budget {
					steer(
						mailbox,
						sf!(
							"[budget notice] {} requests used (soft budget {}). Finish the current step \
							 and yield the final result.",
							requests,
							budget
						),
					);
				}
				if u64::from(requests) == stop {
					steer(
						mailbox,
						sf!(
							"Soft request budget reached its force threshold. Call yield now with all \
							 partial work; do not start another task."
						),
					);
				}
				if u64::from(requests) >= stop.saturating_add(5) {
					if let Some(abort) = abort.read().as_ref() {
						abort.abort();
					}
					return Ok(true);
				}
			},
			Some(turn_event::Event::Outcome(outcome)) => {
				let usage = outcome.usage.as_ref();
				state.record_activity(SubagentActivity {
					kind:           Some(SubagentActivityKind::Usage),
					detail:         sf!("usage receipt"),
					serving_model:  (!outcome.model.is_empty()).then(|| Str::new(&outcome.model)),
					input_tokens:   usage.map_or(0, |usage| usage.input_tokens),
					output_tokens:  usage.map_or(0, |usage| usage.output_tokens),
					cost_micros:    outcome
						.cost
						.as_ref()
						.map_or(0, |cost| cost.nanos_usd / 1_000),
					context_tokens: usage
						.and_then(|usage| usage.context_tokens)
						.unwrap_or_default(),
				})?;
			},
			Some(turn_event::Event::Error(error)) => {
				state.record_activity(SubagentActivity {
					kind: Some(SubagentActivityKind::ProviderWait),
					detail: Str::new(&error.detail),
					..SubagentActivity::default()
				})?;
			},
			_ => {},
		},
		AgentEvent::ToolOpened { name, .. } => {
			state.record_activity(SubagentActivity {
				kind: Some(SubagentActivityKind::Tool),
				detail: name.clone(),
				..SubagentActivity::default()
			})?;
		},
		AgentEvent::PhaseChanged { to: omp_agent::AgentPhase::Turning, .. } => {
			state.record_activity(SubagentActivity {
				kind: Some(SubagentActivityKind::ProviderWait),
				detail: sf!("provider admission"),
				..SubagentActivity::default()
			})?;
		},
		_ => {},
	}
	Ok(false)
}

fn steer(mailbox: &omp_agent::MailboxSender, text: Str) {
	let item = system_item(text);
	let _ = mailbox.try_enqueue(Interrupt {
		class: InterruptClass::TurnBoundary,
		item,
		source: InterruptSource::Producer(sf!("subagent supervisor")),
	});
}

fn system_item(text: Str) -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(thread::item::Kind::Message(thread::Message {
			role:  thread::Role::System as i32,
			parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text.to_string())) }],
		})),
		props:         None,
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

fn settle(state: &SubagentRunState, kind: SubagentTerminalKind) -> Result<(), SupervisorError> {
	let summary = match kind {
		SubagentTerminalKind::Succeeded => sf!("completed"),
		SubagentTerminalKind::Cancelled => sf!("cancelled with bounded salvage"),
		SubagentTerminalKind::SchemaInvalid => sf!("structured output schema validation failed"),
		SubagentTerminalKind::RuntimeLimit => sf!("runtime limit reached with bounded salvage"),
		SubagentTerminalKind::Failed => sf!("subagent run failed"),
	};
	state.settle(SubagentTerminalStatus {
		kind,
		summary,
		disposition: SubagentDisposition::default(),
	})?;
	Ok(())
}

/// Durable supervisor operation failure.
#[derive(Debug, Error)]
pub enum SupervisorError {
	/// Agent loop failure.
	#[error(transparent)]
	Agent(#[from] AgentError),
	/// Durable lifecycle publication failed.
	#[error(transparent)]
	Journal(#[from] omp_agent::JournalError),
	/// Core-owned lifecycle mutation failed.
	#[error(transparent)]
	State(#[from] SubagentStateError),
	/// Admission failed.
	#[error(transparent)]
	Admission(#[from] omp_agent::SpawnRefusal),
	/// Configured wall-clock limit stopped this generation.
	#[error("subagent runtime limit reached after {max_runtime_ms}ms")]
	RuntimeLimit {
		/// Configured limit.
		max_runtime_ms: u64,
	},
	/// The child ignored forced-yield steering beyond its request budget.
	#[error("subagent request budget exhausted ({requests} requests; budget {budget})")]
	RequestBudget {
		/// Requests observed.
		requests: u32,
		/// Configured soft budget.
		budget:   u32,
	},
	/// No root JobBoard has been bound yet.
	#[error("parent detached-job board is unavailable")]
	JobBoardUnavailable,
	/// Parent JobBoard capacity rejected this child.
	#[error("parent detached-job capacity is exhausted")]
	JobCapacity,
	/// A generated process-local JobBoard identifier collided.
	#[error("subagent job {id} is already registered")]
	DuplicateJob {
		/// Conflicting job identifier.
		id: Str,
	},
	/// Stable ID is already owned by this session supervisor.
	#[error("subagent {id} is already registered")]
	AlreadyRegistered {
		/// Stable ID.
		id: Str,
	},
	/// Stable ID is unknown in this session.
	#[error("subagent {id} is not registered")]
	UnknownAgent {
		/// Stable ID.
		id: Str,
	},
	/// Child actor stopped before replying.
	#[error("subagent supervisor for {id} stopped")]
	Stopped {
		/// Stable ID.
		id: Str,
	},
	/// The child is executing or transitioning and cannot accept another turn.
	#[error("subagent {id} is not idle ({lifecycle})")]
	NotIdleState {
		/// Stable ID.
		id:        Str,
		/// Current lifecycle.
		lifecycle: SubagentLifecycle,
	},
	/// The child is not settled and cannot be parked.
	#[error("subagent {id} is not idle")]
	NotIdle {
		/// Stable ID.
		id: Str,
	},
	/// Memory parking is unavailable without an equivalent cold reviver.
	#[error("subagent {id} has no cold-revival factory")]
	RevivalUnavailable {
		/// Stable ID.
		id: Str,
	},
	/// The application could not reconstruct an equivalent parked loop.
	#[error("subagent {id} cold revival failed")]
	RevivalFailed {
		/// Stable ID.
		id: Str,
	},
}
