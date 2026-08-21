pub mod input;

use std::{
	collections::{HashMap, HashSet},
	future::pending,
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use miette::IntoDiagnostic as _;
use omp_agent::{
	Agent, AgentEvent, AgentPhase, AgentState, AgentTree, Interrupt, InterruptClass,
	InterruptSource, RewindTarget, TurnClient,
};
use omp_chat_ui::{
	ActivityWaveform, AgentRow, Attachment, BackendEvent, Chat, Intent, ModelRow, RewindTargetRow,
	SessionRow, StatusFacts, SubmitMode, TranscriptFrame, TranscriptFrameKind,
	host::{HostExit, HostOptions},
};
use omp_core::{Hash32, SecretString, Str, encoding::hex, sf};
use omp_llm_catalog::{
	ModelKey, ModelSpec, PriceUnit, ProviderDef, ProviderId, provider::AuthSpecKind,
	snapshot::Catalog,
};
use omp_llm_inference::{call::AuthInput, id::TurnId};
use omp_proto::{
	inference::v1::{part_start, turn_event::Event, value},
	thread::v1::{Blob, Item, Message, Part, Role, blob, item, part},
};
use omp_telemetry::firehose::{
	Event as FirehoseEvent, Kind as FirehoseKind, SubscriptionHandle, SubscriptionOptions,
};
use omp_tool::{Registry, Rev, TOOL_REV_PROP, ToolIdentity, render::ViewState};
use omp_tui::{UiContext, components::AttachmentContent, detect};

use crate::{
	chat_ui::input::{ChatCommand, CommandContribution, CommandRoster, ParsedTurnBudget},
	modes::{ActiveMode, ExecutionModes, Goal, GoalStatus, GoalUsage},
	settings::Settings,
};

pub const CREDENTIAL_STORAGE_LOCKED_MESSAGE: &str =
	"Credential storage is locked. Run interactively for owner-only local storage, or set \
	 OMP_LLM_KEYCHAIN=1 to use the OS keychain.";
const GATEWAY_LOGIN_MESSAGE: &str = "Provider login is unavailable through a remote gateway; run \
                                     `omp auth login <provider>` on the gateway host.";
const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;

/// Kind of caller response requested by an authentication provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthPromptKind {
	/// Static API key.
	ApiKey,
	/// OAuth authorization code.
	AuthorizationCode,
	/// Provider session token.
	SessionToken,
	/// Visible plain text, including an empty default selection.
	PlainText,
	/// Optional secret text for which an empty response means skip.
	OptionalSecret,
	/// Confirmation that an external device step is complete.
	Confirmation,
}

/// User-visible progress from the asynchronous provider login worker.
#[derive(Debug, Eq, PartialEq)]
pub enum ChatAuthEvent {
	/// Public browser authorization URL.
	Url(Str),
	/// Short-lived device code and public verification URL.
	DeviceCode { code: Str, url: Str },
	/// Private input requested by the provider.
	Prompt { message: Str, kind: AuthPromptKind },
	/// Public login instructions or waiting state.
	Notice(Str),
	/// Login completed and credentials are available to later turns.
	Complete(Str),
	/// Login could not persist credentials because no OS keychain is available.
	CredentialStorageLocked,
	/// Login stopped with a secret-free diagnostic.
	Failed(Str),
}

/// Commands serialized into the authentication worker's single mailbox.
pub enum ChatAuthCommand {
	/// Starts a new provider flow.
	Start(Str),
	/// Answers the current private-input prompt.
	Answer(AuthInput),
	/// Cancels the active flow regardless of its current provider event.
	Cancel,
}

/// Non-blocking command and event channels for provider authentication.
pub struct ChatAuth {
	commands: flume::Sender<ChatAuthCommand>,
	events:   flume::Receiver<ChatAuthEvent>,
	active:   Arc<AtomicBool>,
}

impl ChatAuth {
	/// Creates a UI handle over an application-owned authentication worker.
	pub(crate) const fn new(
		commands: flume::Sender<ChatAuthCommand>,
		events: flume::Receiver<ChatAuthEvent>,
		active: Arc<AtomicBool>,
	) -> Self {
		Self { commands, events, active }
	}

	/// Starts one provider login unless another flow is already active.
	pub(crate) fn start(&self, provider: Str) -> Result<(), &'static str> {
		if self
			.active
			.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
			.is_err()
		{
			return Err("authentication is already in progress");
		}
		if self
			.commands
			.try_send(ChatAuthCommand::Start(provider))
			.is_err()
		{
			self.active.store(false, Ordering::Release);
			return Err("authentication worker is unavailable");
		}
		Ok(())
	}

	/// Answers the active provider prompt without exposing its secret to UI
	/// events.
	pub(crate) fn answer(&self, input: AuthInput) -> Result<(), &'static str> {
		match input {
			AuthInput::Cancel => self.cancel(),
			input => self
				.commands
				.try_send(ChatAuthCommand::Answer(input))
				.map_err(|_| "authentication worker is not waiting for input"),
		}
	}

	/// Cancels the active flow even while it is waiting on an external provider.
	pub(crate) fn cancel(&self) -> Result<(), &'static str> {
		self
			.commands
			.try_send(ChatAuthCommand::Cancel)
			.map_err(|_| "authentication worker is unavailable")
	}

	/// Reports whether the worker currently owns a login flow.
	pub(crate) fn is_active(&self) -> bool {
		self.active.load(Ordering::Acquire)
	}

	/// Receives the next secret-free worker event.
	pub(crate) async fn next_event(&self) -> Option<ChatAuthEvent> {
		self.events.recv_async().await.ok()
	}
}

/// One project-local durable session shown by the welcome and resume pickers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeChoice {
	/// Stable session identity submitted by the picker.
	pub id:     Str,
	/// Human-readable session name.
	pub label:  Str,
	/// Recency and identity details shown beneath the name.
	pub detail: Str,
}

/// Durable session facts required to initialize the designed chat scene.
pub struct ChatUiSession {
	/// Stable session identifier displayed by the status line.
	pub session_id:     Str,
	/// Canonical history replayed before live events.
	pub initial_items:  Vec<Item>,
	/// Selected model's total token window, when known by the catalog.
	pub context_window: Option<u64>,
}

enum UiCmd {
	/// Boxes the foreign generated protobuf item; one allocation is paid per
	/// user submit.
	Submit {
		item:   Box<Item>,
		budget: Option<ParsedTurnBudget>,
	},
	ListRewind {
		reply: flume::Sender<Result<Vec<RewindTarget>, String>>,
	},
	Rewind {
		to:    Option<u64>,
		reply: flume::Sender<Result<Vec<Item>, String>>,
	},
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SubmitAck {
	interrupted:     bool,
	committed_turns: u32,
}

struct PendingPrompt {
	text:        Str,
	attachments: Vec<Attachment>,
}

struct ToolDisplay {
	identity: ToolIdentity,
	args:     omp_slopjson::Value,
	started:  bool,
	fold:     ViewState,
}

struct BridgeState {
	model:             String,
	context_window:    Option<u64>,
	context_tokens:    u64,
	cost_nanos:        u64,
	queued:            usize,
	jobs:              HashSet<Str>,
	attempt:           u32,
	turn_started:      Option<Instant>,
	submit_pending:    bool,
	pending_prompt:    Option<PendingPrompt>,
	part_serial:       u64,
	active_parts:      HashMap<u32, Str>,
	streaming_tools:   HashMap<u32, (Str, Vec<u8>)>,
	tools:             HashMap<Str, ToolDisplay>,
	rewind_targets:    Vec<RewindTarget>,
	pending_auth_kind: Option<AuthPromptKind>,
	live_enabled:      bool,
	live_activity:     ActivityWaveform,
	replaying_turn:    bool,
	settings:          Settings,
	commands:          CommandRoster,
}

fn subscribe_chat_events(bus: &omp_agent::EventBus) -> omp_agent::EventSubscription {
	bus.subscribe_lossless()
}

/// Runs the designed terminal chat scene bridged to a real durable agent.
#[expect(
	clippy::future_not_send,
	reason = "the terminal scene and its bridge stay on one event-loop thread"
)]
pub async fn run<'a, C, R>(
	mut agent: Agent<C>,
	session: ChatUiSession,
	registry: Arc<Registry>,
	tree: Arc<AgentTree>,
	modes: Arc<ExecutionModes>,
	auth: Option<&'a ChatAuth>,
	data_dir: PathBuf,
	command_sources: Vec<Vec<CommandContribution>>,
	mut list_sessions: R,
	welcome: bool,
) -> miette::Result<HostExit>
where
	C: TurnClient + 'static,
	R: FnMut() -> miette::Result<Vec<ResumeChoice>> + 'a,
{
	let bus = agent.events().clone();
	let mailbox = agent.mailbox();
	// Turn deltas and their authoritative outcome share this stream. Dropping
	// either can leave a blank or permanently partial transcript.
	let agent_events = subscribe_chat_events(&bus);
	let live_events = agent
		.firehose()
		.subscribe(
			SubscriptionOptions::new(
				[
					FirehoseKind::TurnStart,
					FirehoseKind::TurnEnd,
					FirehoseKind::ModelRequest,
					FirehoseKind::ModelAttempt,
					FirehoseKind::ProviderError,
					FirehoseKind::ToolCall,
				],
				128,
			)
			.into_diagnostic()?,
		)
		.into_diagnostic()?;
	let session_id = session.session_id.clone();
	let mut roster_events = tree.watch_roster();
	let mut roster_tick = tokio::time::interval(Duration::from_millis(100));
	roster_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	let agent_state = agent.state().clone();
	let abort = agent.abort_handle();
	let startup_pending = startup_recovery_needed(
		agent.journal().pending_turn().is_some(),
		agent.journal().pending_input_submission().is_some(),
	);

	let submission_state = agent.state().clone();
	let (ui_tx, ui_rx) = flume::bounded::<UiCmd>(1);
	let (error_tx, error_rx) = flume::unbounded::<String>();
	let (ack_tx, ack_rx) = flume::bounded::<SubmitAck>(1);
	let mut agent_task = tokio::spawn(async move {
		if startup_pending {
			let turn_id = TurnId::new(omp_core::Ulid::generate().to_string());
			let ack = match agent.submit(Vec::new(), turn_id).await {
				Ok(summary) => SubmitAck {
					interrupted:     summary.interrupted,
					committed_turns: summary.committed_turns,
				},
				Err(error) => {
					let _ = error_tx.send(format!("Startup resume error: {error}"));
					SubmitAck { interrupted: false, committed_turns: 0 }
				},
			};
			let _ = ack_tx.send(ack);
		}
		while let Ok(command) = ui_rx.recv_async().await {
			match command {
				UiCmd::Submit { item, budget } => {
					apply_turn_budget(&submission_state, budget.as_ref());
					let turn_id = TurnId::new(omp_core::Ulid::generate().to_string());
					let ack = match agent.submit([*item], turn_id).await {
						Ok(summary) => SubmitAck {
							interrupted:     summary.interrupted,
							committed_turns: summary.committed_turns,
						},
						Err(error) => {
							let _ = error_tx.send(format!("Submit error: {error}"));
							SubmitAck { interrupted: false, committed_turns: 0 }
						},
					};
					apply_turn_budget(&submission_state, None);
					let _ = ack_tx.send(ack);
				},
				UiCmd::ListRewind { reply } => {
					let result = agent.rewind_targets().map_err(|error| error.to_string());
					let _ = reply.send(result);
				},
				UiCmd::Rewind { to, reply } => {
					let result = agent.rewind(to).map_err(|error| error.to_string());
					let _ = reply.send(result);
				},
			}
		}
	});

	let caps = detect();
	let ctx = UiContext::default().with_terminal_caps(&caps);
	let mut chat = Chat::new(&ctx);
	let commands = CommandRoster::new(command_sources);
	chat.set_slash_commands(commands.completions());
	let (backend_tx, backend_rx) = flume::unbounded();
	let (intent_tx, intent_rx) = flume::unbounded();
	let snapshot = agent_state.snapshot();
	let model = snapshot.turn.params.model.clone();
	drop(snapshot);
	let mut state = BridgeState {
		model,
		context_window: session.context_window,
		context_tokens: 0,
		cost_nanos: 0,
		queued: 0,
		jobs: HashSet::new(),
		attempt: 0,
		turn_started: startup_pending.then(Instant::now),
		submit_pending: startup_pending,
		pending_prompt: None,
		part_serial: 0,
		active_parts: HashMap::new(),
		streaming_tools: HashMap::new(),
		tools: HashMap::new(),
		rewind_targets: Vec::new(),
		live_enabled: false,
		live_activity: ActivityWaveform::new(),
		pending_auth_kind: None,
		replaying_turn: false,
		settings: Settings::load(&data_dir),
		commands,
	};
	chat.set_composer_style(state.settings.composer.shape);

	send_backend(&backend_tx, BackendEvent::ModelsUpdated {
		rows:    model_rows(Catalog::embedded()),
		current: current_model_index(Catalog::embedded(), &state.model),
	});
	if welcome {
		match list_sessions() {
			Ok(choices) => send_backend(&backend_tx, BackendEvent::Sessions(session_rows(choices))),
			Err(error) => {
				send_backend(&backend_tx, BackendEvent::Error(sf!("Could not list sessions: {error}")));
			},
		}
	}
	replay_items(
		&backend_tx,
		&session.initial_items,
		&mut state.tools,
		&mut state.part_serial,
		registry.as_ref(),
	);
	send_status(&backend_tx, &state, &bus, 0);
	let mut last_roster = project_agent_roster(&tree, &session_id);
	send_backend(&backend_tx, BackendEvent::AgentRoster(last_roster.clone()));

	let bridge = async move {
		loop {
			tokio::select! {
				intent = intent_rx.recv_async() => {
					let Ok(intent) = intent else { break };
					if handle_intent(
						intent,
						&backend_tx,
						&ui_tx,
						&mailbox,
						&abort,
						&agent_state,
						&modes,
						auth,
						&data_dir,
						&mut list_sessions,
						&bus,
						registry.as_ref(),
						0,
						&mut state,
					).await? {
						break;
					}
				},
				Ok(message) = error_rx.recv_async() => {
					send_backend(&backend_tx, BackendEvent::Error(Str::from(message)));
				},
				Ok(ack) = ack_rx.recv_async() => {
					state.submit_pending = false;
					state.turn_started = None;
					state.queued = 0;
					if ack.interrupted && ack.committed_turns == 0
						&& let Some(prompt) = state.pending_prompt.take()
					{
						send_backend(&backend_tx, BackendEvent::PromptDropped {
							text: prompt.text,
							attachments: prompt.attachments,
						});
					} else {
						state.pending_prompt = None;
					}
					send_backend(&backend_tx, BackendEvent::Ack {
						interrupted: ack.interrupted,
					});
					send_status(&backend_tx, &state, &bus, 0);
				},
				Some(event) = next_auth_event(auth) => {
					handle_auth_event(&backend_tx, &mut state, event);
				},
				Ok(event) = agent_events.recv() => {
					if matches!(&*event, AgentEvent::RosterChanged { .. }) {
						publish_agent_roster(&backend_tx, &tree, &session_id, &mut last_roster);
					} else {
						handle_agent_event(
							&backend_tx,
							&mut state,
							&event,
							modes.as_ref(),
							registry.as_ref(),
							&bus,
							0,
						);
					}
				},
				Ok(()) = roster_events.changed() => {
					let generation = *roster_events.borrow_and_update();
					bus.publish(AgentEvent::RosterChanged { generation });
				},
				_ = roster_tick.tick() => {
					if project_agent_roster(&tree, &session_id) != last_roster {
						bus.publish(AgentEvent::RosterChanged {
							generation: tree.roster_generation(),
						});
					}
					if drain_live_activity(&live_events, &mut state) {
						send_status(&backend_tx, &state, &bus, 0);
					}
				},
			}
		}
		Ok::<(), miette::Report>(())
	};

	let host = omp_chat_ui::host::run_with_options(chat, ctx, backend_rx, intent_tx, HostOptions {
		welcome,
		exit_on_session_change: true,
	});
	let (host_result, bridge_result) = tokio::join!(host, bridge);
	if tokio::time::timeout(Duration::from_secs(3), &mut agent_task)
		.await
		.is_err()
	{
		agent_task.abort();
		let _ = agent_task.await;
	}
	bridge_result?;
	host_result.into_diagnostic()
}
const fn mode_name(mode: ActiveMode) -> &'static str {
	match mode {
		ActiveMode::Standard => "standard",
		ActiveMode::Plan => "plan",
		ActiveMode::Prewalk => "prewalk",
		ActiveMode::Goal => "goal",
		ActiveMode::Vibe => "vibe",
	}
}

fn handle_plan_command(backend: &flume::Sender<BackendEvent>, modes: &ExecutionModes, args: &str) {
	let result = match args.trim() {
		"" | "status" => {
			send_backend(
				backend,
				BackendEvent::Notice(sf!("Execution mode: **{}**", mode_name(modes.active()))),
			);
			return;
		},
		"on" => modes.enter_plan(false),
		"yolo" => modes.enter_plan(true),
		"off" => {
			modes.exit_plan();
			Ok(())
		},
		_ => {
			send_backend(backend, BackendEvent::Error(sf!("Usage: /plan [on|yolo|off|status]")));
			return;
		},
	};
	report_mode_result(backend, result, modes);
}

fn handle_prewalk_command(
	backend: &flume::Sender<BackendEvent>,
	modes: &ExecutionModes,
	args: &str,
) {
	let result = match args.trim() {
		"" | "status" => {
			send_backend(
				backend,
				BackendEvent::Notice(sf!("Execution mode: **{}**", mode_name(modes.active()))),
			);
			return;
		},
		"on" => modes.arm_prewalk(),
		"off" => {
			modes.disarm_prewalk();
			Ok(())
		},
		_ => {
			send_backend(backend, BackendEvent::Error(sf!("Usage: /prewalk [on|off|status]")));
			return;
		},
	};
	report_mode_result(backend, result, modes);
}

fn handle_vibe_command(backend: &flume::Sender<BackendEvent>, modes: &ExecutionModes, args: &str) {
	let result = match args.trim() {
		"" | "status" => {
			send_backend(
				backend,
				BackendEvent::Notice(sf!("Execution mode: **{}**", mode_name(modes.active()))),
			);
			return;
		},
		"on" => modes.enter_vibe(),
		"off" => {
			modes.exit_vibe();
			Ok(())
		},
		_ => {
			send_backend(backend, BackendEvent::Error(sf!("Usage: /vibe [on|off|status]")));
			return;
		},
	};
	report_mode_result(backend, result, modes);
}

fn handle_goal_command(backend: &flume::Sender<BackendEvent>, modes: &ExecutionModes, args: &str) {
	let args = args.trim();
	let (op, rest) = args
		.split_once(char::is_whitespace)
		.map_or((args, ""), |(op, rest)| (op, rest.trim()));
	let result = match op {
		"" => {
			send_backend(
				backend,
				BackendEvent::Notice(sf!(
					"Use `/goal set <objective> [token-budget]` to start an autonomous goal.",
				)),
			);
			return;
		},
		"status" => {
			send_backend(backend, BackendEvent::Notice(goal_status(modes.goal())));
			return;
		},
		"set" => {
			let (objective, budget) =
				rest
					.rsplit_once(char::is_whitespace)
					.map_or((rest, None), |(objective, tail)| {
						tail
							.trim()
							.parse::<u64>()
							.ok()
							.map_or((rest, None), |budget| (objective.trim(), Some(budget)))
					});
			modes.set_goal(objective, budget, now_ms())
		},
		"pause" => modes.pause_goal(now_ms()),
		"resume" => modes.resume_goal(now_ms()),
		"complete" => modes.complete_goal(now_ms()),
		"drop" => modes.drop_goal(now_ms()),
		"budget" => {
			if let Ok(budget) = rest.parse::<u64>() {
				modes.set_goal_budget(budget)
			} else {
				send_backend(
					backend,
					BackendEvent::Error(sf!("Usage: /goal budget <positive-tokens>")),
				);
				return;
			}
		},
		_ => {
			send_backend(
				backend,
				BackendEvent::Error(
					sf!("Usage: /goal [set|pause|resume|complete|drop|budget|status]",),
				),
			);
			return;
		},
	};
	match result {
		Ok(goal) => send_backend(backend, BackendEvent::Notice(goal_status(Some(goal)))),
		Err(error) => send_backend(backend, BackendEvent::Error(Str::from(error.to_string()))),
	}
}

fn report_mode_result(
	backend: &flume::Sender<BackendEvent>,
	result: Result<(), crate::modes::ModeError>,
	modes: &ExecutionModes,
) {
	match result {
		Ok(()) => send_backend(
			backend,
			BackendEvent::Notice(sf!("Execution mode: **{}**", mode_name(modes.active()))),
		),
		Err(error) => send_backend(backend, BackendEvent::Error(Str::from(error.to_string()))),
	}
}

fn goal_status(goal: Option<Goal>) -> Str {
	let Some(goal) = goal else {
		return sf!("No goal is configured.");
	};
	let status = match goal.status {
		GoalStatus::Active => "active",
		GoalStatus::Paused => "paused",
		GoalStatus::BudgetLimited => "budget-limited",
		GoalStatus::Complete => "complete",
		GoalStatus::Dropped => "dropped",
	};
	let budget = goal.token_budget.map_or_else(
		|| "unbounded".to_owned(),
		|budget| format!("{}/{budget} tokens", goal.tokens_used),
	);
	Str::from(format!(
		"**Goal {status}** · {budget} · {}s\n{}",
		goal.time_used_seconds, goal.objective
	))
}

#[allow(clippy::too_many_arguments, reason = "the bridge owns one explicit production seam")]
async fn handle_intent<R>(
	intent: Intent,
	backend: &flume::Sender<BackendEvent>,
	commands_tx: &flume::Sender<UiCmd>,
	mailbox: &omp_agent::MailboxSender,
	abort: &omp_agent::AbortHandle,
	agent_state: &AgentState,
	modes: &ExecutionModes,
	auth: Option<&ChatAuth>,
	data_dir: &std::path::Path,
	list_sessions: &mut R,
	bus: &omp_agent::EventBus,
	registry: &Registry,
	dropped: u64,
	state: &mut BridgeState,
) -> miette::Result<bool>
where
	R: FnMut() -> miette::Result<Vec<ResumeChoice>>,
{
	match intent {
		Intent::Submit { text, attachments, mode } => match state.commands.parse_input(&text) {
			Ok(ChatCommand::Nothing) => {
				if should_abort_empty(chat_active(state.submit_pending, bus.phase()), state.queued) {
					abort.abort();
				}
			},
			Ok(ChatCommand::Help) => {
				send_backend(backend, BackendEvent::Notice(Str::from(state.commands.help_text())));
			},
			Ok(ChatCommand::Login(provider)) => {
				if chat_active(state.submit_pending, bus.phase()) {
					send_backend(
						backend,
						BackendEvent::Error(
							sf!("Wait for the active turn to finish before logging in.",),
						),
					);
				} else {
					handle_login(backend, auth, provider, state);
				}
			},
			Ok(ChatCommand::Model(selector)) => {
				switch_model(backend, agent_state, data_dir, selector.as_str(), state);
			},
			Ok(ChatCommand::ModelPicker) => send_open_models(backend, state),
			Ok(ChatCommand::Resume) => {
				if chat_active(state.submit_pending, bus.phase()) {
					send_backend(
						backend,
						BackendEvent::Error(sf!(
							"Wait for the active turn to finish before resuming another session.",
						)),
					);
				} else {
					match list_sessions() {
						Ok(choices) => {
							send_backend(backend, BackendEvent::Sessions(session_rows(choices)));
						},
						Err(error) => send_backend(
							backend,
							BackendEvent::Error(sf!("Could not list sessions: {error}")),
						),
					}
				}
			},
			Ok(ChatCommand::NewSession) => {
				if chat_active(state.submit_pending, bus.phase()) {
					send_backend(
						backend,
						BackendEvent::Error(sf!(
							"Wait for the active turn to finish before starting a new session.",
						)),
					);
				} else {
					send_backend(backend, BackendEvent::NewSessionRequested);
				}
			},
			Ok(ChatCommand::Jobs) => {
				let mut jobs: Vec<_> = state.jobs.iter().map(Str::as_str).collect();
				jobs.sort_unstable();
				let message = if jobs.is_empty() {
					sf!("No active background jobs.")
				} else {
					Str::from(format!(
						"**Active jobs ({})**\n{}",
						jobs.len(),
						jobs
							.into_iter()
							.map(|job| format!("- `{job}`"))
							.collect::<Vec<_>>()
							.join("\n"),
					))
				};
				send_backend(backend, BackendEvent::Notice(message));
			},
			Ok(ChatCommand::Settings) => send_backend(backend, BackendEvent::OpenSettings),
			Ok(ChatCommand::Live) => {
				state.live_enabled = !state.live_enabled;
				if state.live_enabled {
					state.live_activity = ActivityWaveform::new();
				}
				send_backend(
					backend,
					BackendEvent::Notice(sf!(if state.live_enabled {
						"Live activity waveform enabled."
					} else {
						"Live activity waveform disabled."
					})),
				);
				send_status(backend, state, bus, dropped);
			},
			Ok(ChatCommand::Plan(args)) => handle_plan_command(backend, modes, args.as_str()),
			Ok(ChatCommand::Goal(args)) => handle_goal_command(backend, modes, args.as_str()),
			Ok(ChatCommand::Vibe(args)) => handle_vibe_command(backend, modes, args.as_str()),
			Ok(ChatCommand::Prewalk(args)) => handle_prewalk_command(backend, modes, args.as_str()),
			Ok(ChatCommand::Agents) => send_backend(backend, BackendEvent::OpenAgentTree),
			Ok(ChatCommand::Pause) => send_backend(backend, BackendEvent::Pause),
			Ok(ChatCommand::Unavailable { command, reason }) => {
				send_backend(backend, BackendEvent::Error(sf!("/{command} unavailable: {reason}")));
			},
			Ok(ChatCommand::Quit) => {
				if chat_active(state.submit_pending, bus.phase()) {
					abort.abort();
				}
				return Ok(true);
			},
			Ok(ChatCommand::Submit { item, text: prompt_text, budget }) => {
				if auth.is_some_and(ChatAuth::is_active) {
					send_backend(
						backend,
						BackendEvent::Error(sf!(
							"Wait for provider authentication to finish before submitting.",
						)),
					);
				} else {
					let active = chat_active(state.submit_pending, bus.phase());
					let pending_prompt = (!active).then(|| PendingPrompt {
						text:        prompt_text.clone(),
						attachments: attachments.clone(),
					});
					let mut item = *item;
					let chips = lower_attachments(&mut item, attachments, |message| {
						send_backend(backend, BackendEvent::Error(message));
					});
					let delivered = if active {
						apply_turn_budget(agent_state, budget.as_ref());
						let delivered = mailbox
							.try_enqueue(Interrupt {
								class: active_submit_class(mode),
								item,
								source: InterruptSource::Producer(sf!("user")),
							})
							.is_ok();
						if !delivered {
							apply_turn_budget(agent_state, None);
						}
						delivered
					} else {
						state.submit_pending = true;
						commands_tx
							.send_async(UiCmd::Submit { item: Box::new(item), budget })
							.await
							.is_ok()
					};
					if delivered {
						send_backend(backend, BackendEvent::UserReplayed { text: prompt_text, chips });
						if active {
							state.queued = state.queued.saturating_add(1);
						} else {
							state.turn_started.get_or_insert_with(Instant::now);
							state.pending_prompt = pending_prompt;
						}
					} else {
						state.submit_pending = false;
						state.pending_prompt = None;
						send_backend(backend, BackendEvent::Error(sf!("Agent input channel is closed.")));
					}
				}
			},
			Err(error) => send_backend(backend, BackendEvent::Error(Str::from(error.to_string()))),
		},
		Intent::Abort => {
			if chat_active(state.submit_pending, bus.phase()) {
				abort.abort();
			}
		},
		Intent::RewindRequest => {
			if chat_active(state.submit_pending, bus.phase()) {
				send_backend(
					backend,
					BackendEvent::Error(sf!("Wait for the active turn to finish before rewinding.",)),
				);
			} else {
				let (reply_tx, reply_rx) = flume::bounded(1);
				if commands_tx
					.send_async(UiCmd::ListRewind { reply: reply_tx })
					.await
					.is_err()
				{
					send_backend(backend, BackendEvent::Error(sf!("Agent input channel is closed.")));
				} else {
					match reply_rx.recv_async().await {
						Ok(Ok(targets)) => {
							state.rewind_targets = targets;
							send_backend(
								backend,
								BackendEvent::RewindTargets(
									state
										.rewind_targets
										.iter()
										.map(|target| RewindTargetRow {
											event: target.event,
											text:  target.text.clone(),
										})
										.collect(),
								),
							);
						},
						Ok(Err(error)) => send_backend(backend, BackendEvent::Error(Str::from(error))),
						Err(_) => send_backend(
							backend,
							BackendEvent::Error(sf!("Agent rewind reply channel is closed.")),
						),
					}
				}
			}
		},
		Intent::Rewind { event } => {
			let target = state
				.rewind_targets
				.iter()
				.find(|target| target.event == event)
				.cloned();
			if let Some(target) = target {
				let (reply_tx, reply_rx) = flume::bounded(1);
				if commands_tx
					.send_async(UiCmd::Rewind { to: target.keep, reply: reply_tx })
					.await
					.is_err()
				{
					send_backend(backend, BackendEvent::Error(sf!("Agent input channel is closed.")));
				} else {
					match reply_rx.recv_async().await {
						Ok(Ok(items)) => {
							state.tools.clear();
							send_backend(backend, BackendEvent::HistoryCleared);
							replay_items(
								backend,
								&items,
								&mut state.tools,
								&mut state.part_serial,
								registry,
							);
							state.rewind_targets.clear();
						},
						Ok(Err(error)) => send_backend(backend, BackendEvent::Error(Str::from(error))),
						Err(_) => send_backend(
							backend,
							BackendEvent::Error(sf!("Agent rewind reply channel is closed.")),
						),
					}
				}
			} else {
				send_backend(
					backend,
					BackendEvent::Error(sf!("The selected rewind target is no longer available.",)),
				);
			}
		},
		Intent::SwitchModel(model) => {
			switch_model(backend, agent_state, data_dir, model.as_str(), state);
		},
		Intent::Login(provider) => {
			if chat_active(state.submit_pending, bus.phase()) {
				send_backend(
					backend,
					BackendEvent::Error(sf!("Wait for the active turn to finish before logging in.",)),
				);
			} else {
				handle_login(backend, auth, provider, state);
			}
		},
		Intent::Resume(None) => {
			if chat_active(state.submit_pending, bus.phase()) {
				send_backend(
					backend,
					BackendEvent::Error(sf!(
						"Wait for the active turn to finish before resuming another session.",
					)),
				);
			} else {
				match list_sessions() {
					Ok(choices) => {
						send_backend(backend, BackendEvent::Sessions(session_rows(choices)));
					},
					Err(error) => send_backend(
						backend,
						BackendEvent::Error(sf!("Could not list sessions: {error}")),
					),
				}
			}
		},
		Intent::Resume(Some(_)) | Intent::NewSession => {},
		Intent::AuthAnswer { value } => {
			if let (Some(auth), Some(kind)) = (auth, state.pending_auth_kind.take()) {
				if let Err(error) = auth.answer(auth_input(kind, value)) {
					send_backend(backend, BackendEvent::Error(Str::from(error)));
				}
			} else {
				send_backend(backend, BackendEvent::Error(sf!("No authentication prompt is active.")));
			}
		},
		Intent::AuthCancel => {
			state.pending_auth_kind = None;
			if let Some(auth) = auth
				&& let Err(error) = auth.cancel()
			{
				send_backend(backend, BackendEvent::Error(Str::from(error)));
			}
		},
		Intent::Help => {
			send_backend(backend, BackendEvent::Notice(Str::from(state.commands.help_text())));
		},
		Intent::Quit => {
			if chat_active(state.submit_pending, bus.phase()) {
				abort.abort();
			}
			return Ok(true);
		},
	}
	send_status(backend, state, bus, dropped);
	Ok(false)
}

fn handle_login(
	backend: &flume::Sender<BackendEvent>,
	auth: Option<&ChatAuth>,
	requested: Option<Str>,
	state: &BridgeState,
) {
	let Some(auth) = auth else {
		send_backend(backend, BackendEvent::Error(sf!(GATEWAY_LOGIN_MESSAGE)));
		return;
	};
	if let Some(requested) = requested {
		match resolve_login_provider(Catalog::embedded(), &requested) {
			Ok(provider) => match auth.start(provider.clone()) {
				Ok(()) => send_backend(
					backend,
					BackendEvent::Notice(sf!("Starting authentication for `{provider}`…")),
				),
				Err(error) => send_backend(backend, BackendEvent::Error(Str::from(error))),
			},
			Err(error) => send_backend(backend, BackendEvent::Error(error)),
		}
	} else {
		let current = model_provider(Catalog::embedded(), &state.model);
		send_backend(
			backend,
			BackendEvent::LoginProviders(provider_rows(Catalog::embedded(), current.as_deref())),
		);
	}
}

fn handle_auth_event(
	backend: &flume::Sender<BackendEvent>,
	state: &mut BridgeState,
	event: ChatAuthEvent,
) {
	match event {
		ChatAuthEvent::Url(url) => {
			send_backend(backend, BackendEvent::Notice(sf!("[open to authorize]({url})")));
		},
		ChatAuthEvent::DeviceCode { code, url } => {
			send_backend(backend, BackendEvent::Notice(sf!("Enter code `{code}` at [{url}]({url})")));
		},
		ChatAuthEvent::Prompt { message, kind } => {
			state.pending_auth_kind = Some(kind);
			send_backend(backend, BackendEvent::AuthPrompt {
				message,
				masked: prompt_masks_input(kind),
			});
		},
		ChatAuthEvent::Notice(message) => send_backend(backend, BackendEvent::Notice(message)),
		ChatAuthEvent::Complete(message) => {
			state.pending_auth_kind = None;
			send_backend(backend, BackendEvent::AuthPromptClose);
			send_backend(backend, BackendEvent::Notice(message));
		},
		ChatAuthEvent::CredentialStorageLocked => {
			state.pending_auth_kind = None;
			send_backend(backend, BackendEvent::AuthPromptClose);
			send_backend(backend, BackendEvent::Error(sf!(CREDENTIAL_STORAGE_LOCKED_MESSAGE)));
		},
		ChatAuthEvent::Failed(message) => {
			state.pending_auth_kind = None;
			send_backend(backend, BackendEvent::AuthPromptClose);
			send_backend(backend, BackendEvent::Error(message));
		},
	}
}

fn handle_agent_event(
	backend: &flume::Sender<BackendEvent>,
	state: &mut BridgeState,
	event: &AgentEvent,
	modes: &ExecutionModes,
	registry: &Registry,
	bus: &omp_agent::EventBus,
	dropped: u64,
) {
	match event {
		AgentEvent::Turn { event, .. } => match &event.event {
			Some(Event::Accepted(accepted)) => state.replaying_turn = accepted.replay,
			Some(Event::Outcome(outcome)) => {
				if state.replaying_turn {
					replay_items(
						backend,
						&outcome.output,
						&mut state.tools,
						&mut state.part_serial,
						registry,
					);
					state.replaying_turn = false;
				}
				state.queued = 0;
				state.model.clone_from(&outcome.model);
				state.context_window = resolve_model(Catalog::embedded(), &outcome.model)
					.and_then(|spec| spec.limits.context_window);
				if let Some(cost) = &outcome.cost {
					state.cost_nanos = state.cost_nanos.saturating_add(cost.nanos_usd);
				}
				if let Some(snapshot) = &outcome.context_snapshot {
					state.context_tokens = snapshot.prompt_tokens;
				}
				if let Some(usage) = &outcome.usage {
					let _ = modes.record_goal_usage(
						GoalUsage {
							input_tokens:        usage.input_tokens,
							cache_write_tokens:  usage.cache_write_tokens,
							cached_input_tokens: usage.cache_read_tokens,
							output_tokens:       usage.output_tokens,
						},
						now_ms(),
					);
				}
				for (_, id) in state.active_parts.drain() {
					send_backend(backend, BackendEvent::AssistantEnd { id });
				}
				if state.attempt > 1 {
					send_backend(
						backend,
						BackendEvent::TranscriptFrame(TranscriptFrame {
							kind:   TranscriptFrameKind::Recovery,
							title:  sf!("Recovered on attempt {}", state.attempt),
							detail: None,
						}),
					);
				}
				state.attempt = 0;
			},
			Some(Event::Attempt(attempt)) => {
				state.attempt = attempt.number;
				if attempt.number > 1 {
					send_backend(
						backend,
						BackendEvent::TranscriptFrame(TranscriptFrame {
							kind:   TranscriptFrameKind::Recovery,
							title:  sf!("Retry attempt {}", attempt.number),
							detail: None,
						}),
					);
				}
			},
			Some(Event::PartStart(start)) => match part_start::Kind::try_from(start.kind) {
				Ok(part_start::Kind::Text | part_start::Kind::Thinking) => {
					state.part_serial = state.part_serial.saturating_add(1);
					let id = Str::from(format!("assistant-{}", state.part_serial));
					send_backend(backend, BackendEvent::AssistantBegin { id: id.clone() });
					if start.kind == part_start::Kind::Thinking as i32 {
						send_backend(backend, BackendEvent::AssistantDelta {
							id:   id.clone(),
							text: sf!("*Thinking:* "),
						});
					}
					state.active_parts.insert(start.index, id);
				},
				Ok(part_start::Kind::ToolCall) => {
					let id = Str::from(start.tool_call_id.as_str());
					let identity = missing_identity(&start.tool_name);
					state.tools.insert(id.clone(), ToolDisplay {
						identity,
						args: omp_slopjson::Value::Object(omp_slopjson::Object::new()),
						started: false,
						fold: ViewState::new(),
					});
					state.streaming_tools.insert(start.index, (id, Vec::new()));
				},
				_ => {},
			},
			Some(Event::PartDelta(delta)) => {
				if let Some(id) = state.active_parts.get(&delta.index)
					&& let Ok(fragment) = std::str::from_utf8(&delta.chunk)
				{
					send_backend(backend, BackendEvent::AssistantDelta {
						id:   id.clone(),
						text: Str::from(fragment),
					});
				} else if let Some((id, bytes)) = state.streaming_tools.get_mut(&delta.index) {
					bytes.extend_from_slice(&delta.chunk);
					if let Ok(fragment) = std::str::from_utf8(bytes)
						&& let Some(tool) = state.tools.get_mut(id.as_str())
					{
						tool.args = omp_slopjson::parse_streaming(fragment);
						ensure_tool_started(backend, id, tool, false);
						if tool.started
							&& let Some(input) = tool.args.get("input").and_then(|value| value.as_str())
						{
							send_backend(backend, BackendEvent::ToolView {
								id:   id.clone(),
								view: Str::from(input),
							});
						}
					}
				}
			},
			Some(Event::PartEnd(end)) => {
				if let Some(id) = state.active_parts.remove(&end.index) {
					send_backend(backend, BackendEvent::AssistantEnd { id });
				}
				state.streaming_tools.remove(&end.index);
			},
			_ => {},
		},
		AgentEvent::ToolOpened { call_id, name, rev } => {
			let identity = ToolIdentity { name: name.clone(), rev: rev.clone() };
			if let Some(tool) = state.tools.get_mut(call_id.as_str()) {
				tool.identity = identity;
			} else {
				state.tools.insert(call_id.clone(), ToolDisplay {
					identity,
					args: omp_slopjson::Value::Object(omp_slopjson::Object::new()),
					started: false,
					fold: ViewState::new(),
				});
			}
		},
		AgentEvent::ToolArgs { call_id, view, .. } => {
			if let Some(tool) = state.tools.get_mut(call_id.as_str()) {
				tool.args = view.clone();
				ensure_tool_started(backend, call_id, tool, false);
			}
		},
		AgentEvent::ToolUpdate { call_id, json } => {
			if let Some(tool) = state.tools.get_mut(call_id.as_str()) {
				ensure_tool_started(backend, call_id, tool, true);
				let view = fold_tool_update(registry, tool, json.clone());
				send_backend(backend, BackendEvent::ToolView { id: call_id.clone(), view });
			}
		},
		AgentEvent::ToolFinished { call_id, item } => {
			let mut tool = state.tools.remove(call_id.as_str());
			let (identity, ok, view) = render_tool_result_view(registry, item, tool.as_ref());
			if let Some(tool) = tool.as_mut() {
				ensure_tool_started(backend, call_id, tool, true);
			} else {
				send_backend(backend, BackendEvent::ToolStarted {
					id:    call_id.clone(),
					name:  identity.name.clone(),
					title: identity.name,
				});
			}
			send_tool_result_images(backend, call_id, item);
			send_backend(backend, BackendEvent::ToolFinished { id: call_id.clone(), ok, view });
		},
		AgentEvent::JobRegistered { job_id } => {
			state.jobs.insert(job_id.clone());
		},
		AgentEvent::JobSettled { job_id } => {
			state.jobs.remove(job_id);
		},
		AgentEvent::Failed { message, .. } => {
			send_backend(
				backend,
				BackendEvent::TranscriptFrame(TranscriptFrame {
					kind:   TranscriptFrameKind::Error,
					title:  sf!("Agent error"),
					detail: Some(message.clone()),
				}),
			);
		},
		AgentEvent::Snapshot(_)
		| AgentEvent::PhaseChanged { .. }
		| AgentEvent::RosterChanged { .. } => {},
	}
	send_status(backend, state, bus, dropped);
}

fn replay_items(
	backend: &flume::Sender<BackendEvent>,
	items: &[Item],
	tools: &mut HashMap<Str, ToolDisplay>,
	serial: &mut u64,
	registry: &Registry,
) {
	for item in items {
		match &item.kind {
			Some(item::Kind::Message(message)) => replay_message(backend, message, serial),
			Some(item::Kind::ToolCall(call)) => {
				let id = Str::from(call.id.as_str());
				let args = std::str::from_utf8(&call.args_json).map_or_else(
					|_| omp_slopjson::Value::Object(omp_slopjson::Object::new()),
					omp_slopjson::parse_streaming,
				);
				let identity =
					item_tool_identity(item, &call.name).unwrap_or_else(|| missing_identity(&call.name));
				let title = call
					.intent
					.as_deref()
					.map_or_else(|| tool_title(&identity.name, &args), Str::from);
				send_backend(backend, BackendEvent::ToolStarted {
					id: id.clone(),
					name: identity.name.clone(),
					title,
				});
				tools.insert(id, ToolDisplay { identity, args, started: true, fold: ViewState::new() });
			},
			Some(item::Kind::ToolResult(result)) => {
				let id = Str::from(result.call_id.as_str());
				let tool = tools.remove(id.as_str());
				let (identity, ok, view) = render_tool_result_view(registry, item, tool.as_ref());
				if tool.is_none() {
					send_backend(backend, BackendEvent::ToolStarted {
						id:    id.clone(),
						name:  identity.name.clone(),
						title: identity.name.clone(),
					});
				}
				send_tool_result_images(backend, &id, item);
				send_backend(backend, BackendEvent::ToolFinished { id, ok, view });
			},
			_ => {},
		}
	}
}

fn ensure_tool_started(
	backend: &flume::Sender<BackendEvent>,
	call_id: &Str,
	tool: &mut ToolDisplay,
	force: bool,
) {
	if tool.started {
		return;
	}
	let title = tool_title(&tool.identity.name, &tool.args);
	if !force && title == tool.identity.name {
		return;
	}
	send_backend(backend, BackendEvent::ToolStarted {
		id: call_id.clone(),
		name: tool.identity.name.clone(),
		title,
	});
	tool.started = true;
}

fn replay_message(backend: &flume::Sender<BackendEvent>, message: &Message, serial: &mut u64) {
	let mut text_parts = Vec::new();
	let mut chips = Vec::new();
	for part in &message.parts {
		match &part.kind {
			Some(part::Kind::Text(text)) => {
				if let Some(attachment) = text
					.strip_prefix("<attachment>")
					.and_then(|text| text.strip_suffix("</attachment>"))
				{
					let lines = attachment.bytes().filter(|byte| *byte == b'\n').count() + 1;
					chips.push(sf!("paste · {lines} lines"));
				} else {
					text_parts.push(text.as_str());
				}
			},
			Some(part::Kind::Blob(blob)) => chips.push(blob_label(blob)),
			_ => {},
		}
	}
	let text = text_parts.join("\n");
	match Role::try_from(message.role) {
		Ok(Role::User) => {
			send_backend(backend, BackendEvent::UserReplayed { text: Str::from(text), chips });
		},
		Ok(Role::System) => {
			if !text.is_empty() {
				send_backend(backend, BackendEvent::Notice(Str::from(text)));
			}
		},
		_ if !text.is_empty() => {
			*serial = serial.saturating_add(1);
			let id = Str::from(format!("history-assistant-{serial}"));
			send_backend(backend, BackendEvent::AssistantBegin { id: id.clone() });
			send_backend(backend, BackendEvent::AssistantDelta {
				id:   id.clone(),
				text: Str::from(text),
			});
			send_backend(backend, BackendEvent::AssistantEnd { id });
		},
		_ => {},
	}
}

fn lower_attachments(
	item: &mut Item,
	attachments: Vec<Attachment>,
	mut report: impl FnMut(Str),
) -> Vec<Str> {
	let mut parts = Vec::with_capacity(attachments.len());
	let mut chips = Vec::with_capacity(attachments.len());
	for attachment in attachments {
		match attachment.content {
			AttachmentContent::Image { source, .. } => {
				let bytes = match std::fs::read(source.as_str()) {
					Ok(bytes) => bytes,
					Err(error) => {
						report(sf!("Could not attach image `{source}`: {error}"));
						continue;
					},
				};
				if bytes.len() > MAX_ATTACHMENT_BYTES {
					report(sf!(
						"Image `{source}` is larger than the 8 MiB attachment limit and was skipped."
					));
					continue;
				}
				let Some(mime) = image_mime(source.as_str()) else {
					report(sf!("Image `{source}` has an unsupported file type and was skipped."));
					continue;
				};
				let size = bytes.len() as u64;
				let hash = Bytes::copy_from_slice(Hash32::sum(&bytes).as_bytes());
				let blob = Blob {
					hash,
					mime: mime.to_owned(),
					size,
					inline: Bytes::from(bytes),
					detail: blob::Detail::Auto as i32,
				};
				chips.push(blob_label(&blob));
				parts.push(Part { kind: Some(part::Kind::Blob(blob)) });
			},
			AttachmentContent::Text { text, lines, .. } => {
				chips.push(sf!("paste · {lines} lines"));
				parts.push(Part {
					kind: Some(part::Kind::Text(format!("<attachment>{text}</attachment>"))),
				});
			},
		}
	}
	if let Some(item::Kind::Message(message)) = item.kind.as_mut() {
		message.parts.extend(parts);
	}
	chips
}

fn image_mime(path: &str) -> Option<&'static str> {
	let extension = std::path::Path::new(path).extension()?.to_str()?;
	if extension.eq_ignore_ascii_case("png") {
		Some("image/png")
	} else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
		Some("image/jpeg")
	} else if extension.eq_ignore_ascii_case("gif") {
		Some("image/gif")
	} else if extension.eq_ignore_ascii_case("webp") {
		Some("image/webp")
	} else {
		None
	}
}

fn blob_label(blob: &Blob) -> Str {
	sf!("image {} · {} KB", blob.mime, blob.size.div_ceil(1024))
}

fn item_tool_identity(item: &Item, name: &str) -> Option<ToolIdentity> {
	let rev = item
		.props
		.as_ref()?
		.fields
		.get(TOOL_REV_PROP)?
		.kind
		.as_ref()
		.and_then(|kind| match kind {
			value::Kind::String(rev) => rev.parse::<Rev>().ok(),
			_ => None,
		})?;
	Some(ToolIdentity { name: Str::from(name), rev })
}

fn durable_tool_identity(item: &Item) -> Option<ToolIdentity> {
	let Some(item::Kind::ToolResult(result)) = &item.kind else {
		return None;
	};
	item_tool_identity(item, &result.name)
}

fn missing_identity(name: &str) -> ToolIdentity {
	ToolIdentity { name: Str::from(name), rev: Rev { family: Default::default(), n: 0 } }
}

fn missing_tool_identity(item: &Item) -> ToolIdentity {
	let name = match &item.kind {
		Some(item::Kind::ToolResult(result)) => result.name.as_str(),
		_ => "tool",
	};
	missing_identity(name)
}

fn durable_tool_outcome(item: &Item) -> Option<Bytes> {
	let Some(item::Kind::ToolResult(result)) = &item.kind else {
		return None;
	};
	let details = proto_to_json(result.details.as_ref()?)?;
	serde_json::to_vec(&details).ok().map(Bytes::from)
}

fn durable_tool_ok(item: &Item) -> bool {
	let Some(item::Kind::ToolResult(result)) = &item.kind else {
		return false;
	};
	let branch = result
		.details
		.as_ref()
		.and_then(|details| match details.kind.as_ref()? {
			value::Kind::Map(map) => map.fields.get("kind"),
			_ => None,
		})
		.and_then(|kind| match kind.kind.as_ref()? {
			value::Kind::String(kind) => Some(kind.as_str()),
			_ => None,
		});
	match branch {
		Some("ok") => true,
		Some("faulted" | "fault" | "args_rejected" | "args" | "aborted") => false,
		_ => !result.is_error,
	}
}

fn structured_bytes_fallback(bytes: &Bytes) -> Str {
	std::str::from_utf8(bytes).map_or_else(|_| Str::new_static("{}"), Str::from)
}

fn fold_tool_update(registry: &Registry, tool: &mut ToolDisplay, update: Bytes) -> Str {
	match registry.fold_render(&tool.identity, &mut tool.fold, update.clone()) {
		Ok(()) => registry
			.render_view(&tool.identity, &tool.fold, None)
			.unwrap_or_else(|_| structured_bytes_fallback(&update)),
		Err(_) => structured_bytes_fallback(&update),
	}
}

fn render_tool_result_view(
	registry: &Registry,
	item: &Item,
	tool: Option<&ToolDisplay>,
) -> (ToolIdentity, bool, Str) {
	let outcome = durable_tool_outcome(item);
	let Some(identity) = durable_tool_identity(item) else {
		let view = outcome
			.as_ref()
			.map_or_else(|| Str::new_static("{}"), structured_bytes_fallback);
		return (missing_tool_identity(item), durable_tool_ok(item), view);
	};
	let empty_fold = ViewState::new();
	let fold = tool
		.filter(|tool| tool.identity == identity)
		.map_or(&empty_fold, |tool| &tool.fold);
	let view = registry
		.render_view(&identity, fold, outcome.as_deref())
		.unwrap_or_else(|_| {
			outcome
				.as_ref()
				.map_or_else(|| Str::new_static("{}"), structured_bytes_fallback)
		});
	(identity, durable_tool_ok(item), view)
}

fn tool_title(name: &Str, args: &omp_slopjson::Value) -> Str {
	let detail = ["title", "path", "command", "pattern", "query"]
		.into_iter()
		.find_map(|key| args.get(key).and_then(|value| value.as_str()))
		.and_then(|text| text.lines().next())
		.or_else(|| {
			args
				.get("input")
				.and_then(|value| value.as_str())
				.and_then(|input| input.lines().next())
				.and_then(|header| header.strip_prefix('['))
				.and_then(|header| header.split_once('#').map(|(path, _)| path))
		});
	detail.map_or_else(|| name.clone(), |detail| sf!("{name} · {detail}"))
}

fn send_tool_result_images(backend: &flume::Sender<BackendEvent>, call_id: &Str, item: &Item) {
	let Some(item::Kind::ToolResult(result)) = &item.kind else {
		return;
	};
	for part in &result.parts {
		let Some(part::Kind::Blob(blob)) = &part.kind else {
			continue;
		};
		if let Some(source) = persist_tool_image(blob) {
			send_backend(backend, BackendEvent::ToolImage { id: call_id.clone(), source });
		}
	}
}

/// Persists an inline PNG tool-result payload to a content-addressed temp
/// file for inline terminal rendering, returning its path. Non-PNG payloads
/// and by-reference blobs are represented by the structured renderer view.
fn persist_tool_image(blob: &Blob) -> Option<Str> {
	if blob.mime != "image/png" || blob.inline.is_empty() {
		return None;
	}
	let name = if blob.hash.is_empty() {
		format!("omp-tool-image-{}.png", omp_core::Ulid::generate())
	} else {
		let hex = hex::encode(&blob.hash[..blob.hash.len().min(16)]).into_string();
		format!("omp-tool-image-{hex}.png")
	};
	let path = std::env::temp_dir().join(name);
	if !path.exists() {
		std::fs::write(&path, &blob.inline).ok()?;
	}
	Some(Str::from(path.to_string_lossy().as_ref()))
}

fn proto_to_json(value: &omp_proto::inference::v1::Value) -> Option<serde_json::Value> {
	match value.kind.as_ref()? {
		value::Kind::Null(_) => Some(serde_json::Value::Null),
		value::Kind::Int(number) => Some((*number).into()),
		value::Kind::Uint(number) => Some((*number).into()),
		value::Kind::Double(number) => serde_json::Number::from_f64(*number).map(Into::into),
		value::Kind::Bool(boolean) => Some((*boolean).into()),
		value::Kind::String(string) => Some(string.clone().into()),
		value::Kind::List(list) => list
			.values
			.iter()
			.map(proto_to_json)
			.collect::<Option<Vec<_>>>()
			.map(Into::into),
		value::Kind::Map(map) => {
			let mut object = serde_json::Map::with_capacity(map.fields.len());
			for (key, value) in &map.fields {
				object.insert(key.clone(), proto_to_json(value)?);
			}
			Some(serde_json::Value::Object(object))
		},
	}
}

fn model_rows(catalog: &Catalog) -> Vec<ModelRow> {
	catalog
		.models()
		.iter()
		.map(|model| {
			let (provider_id, provider) = model
				.routes
				.first()
				.and_then(|route| catalog.route(route))
				.map(|route| {
					let name = catalog
						.provider(&route.provider)
						.map_or_else(|| route.provider.to_string(), |provider| provider.name.to_string());
					(Str::from(route.provider.as_str()), Str::from(name))
				})
				.unwrap_or_default();
			let price = |unit| {
				model
					.pricing
					.components
					.iter()
					.find(|price| price.unit == unit)
					.map(|price| price.nanos_usd as f64 / 1_000_000_000.0)
			};
			ModelRow {
				key: Str::from(model.key.to_string()),
				name: model.display_name.clone(),
				provider_id,
				provider,
				context: model.limits.context_window,
				input_mtok: price(PriceUnit::MtokInput),
				output_mtok: price(PriceUnit::MtokOutput),
			}
		})
		.collect()
}

fn current_model_index(catalog: &Catalog, current: &str) -> usize {
	catalog
		.models()
		.iter()
		.position(|model| model.key.as_str() == current)
		.unwrap_or_default()
}

fn send_open_models(backend: &flume::Sender<BackendEvent>, state: &BridgeState) {
	send_backend(backend, BackendEvent::OpenModelPicker {
		rows:    model_rows(Catalog::embedded()),
		current: current_model_index(Catalog::embedded(), &state.model),
	});
}

fn send_models_updated(backend: &flume::Sender<BackendEvent>, state: &BridgeState) {
	send_backend(backend, BackendEvent::ModelsUpdated {
		rows:    model_rows(Catalog::embedded()),
		current: current_model_index(Catalog::embedded(), &state.model),
	});
}

fn provider_rows(catalog: &Catalog, current: Option<&str>) -> Vec<SessionRow> {
	let mut providers = catalog
		.providers()
		.iter()
		.filter(|provider| provider_supports_login(catalog, provider))
		.map(|provider| {
			let oauth = provider_uses_oauth(catalog, provider);
			(provider, oauth, current == Some(provider.id.as_str()))
		})
		.collect::<Vec<_>>();
	providers.sort_by_key(|(_, oauth, current)| (!*current, !*oauth));
	providers
		.into_iter()
		.map(|(provider, oauth, _)| SessionRow {
			id:     Str::from(provider.id.as_str()),
			label:  provider.name.clone(),
			detail: sf!(if oauth { "OAuth" } else { "API key" }),
		})
		.collect()
}

fn provider_supports_login(catalog: &Catalog, provider: &ProviderDef) -> bool {
	provider
		.auth
		.iter()
		.filter_map(|auth_id| catalog.auth_spec(auth_id))
		.any(|auth| auth.kind != AuthSpecKind::None)
}

fn provider_uses_oauth(catalog: &Catalog, provider: &ProviderDef) -> bool {
	provider.auth.iter().any(|auth_id| {
		catalog
			.auth_spec(auth_id)
			.and_then(|auth| auth.oauth.as_ref())
			.is_some_and(|oauth_id| catalog.oauth_spec(oauth_id).is_some())
	})
}

fn session_rows(choices: Vec<ResumeChoice>) -> Vec<SessionRow> {
	choices
		.into_iter()
		.map(|choice| SessionRow { id: choice.id, label: choice.label, detail: choice.detail })
		.collect()
}

fn switch_model(
	backend: &flume::Sender<BackendEvent>,
	state_handle: &AgentState,
	data_dir: &std::path::Path,
	selector: &str,
	state: &mut BridgeState,
) {
	match select_model(state_handle, Catalog::embedded(), selector) {
		Some(spec) => {
			state.model = spec.key.to_string();
			state.context_window = spec.limits.context_window;
			state.settings.default_model = Some(state.model.clone());
			if let Err(error) = state.settings.save(data_dir) {
				send_backend(
					backend,
					BackendEvent::Error(sf!("Could not save the default model: {error}")),
				);
			}
			send_models_updated(backend, state);
		},
		None => send_backend(backend, BackendEvent::Error(sf!("Unknown model: {selector}"))),
	}
}

fn select_model<'a>(
	state: &AgentState,
	catalog: &'a Catalog,
	selector: &str,
) -> Option<&'a ModelSpec> {
	let spec = resolve_model(catalog, selector)?;
	let key = spec.key.to_string();
	state.update(|snapshot| snapshot.turn.params.model.clone_from(&key));
	Some(spec)
}

fn resolve_model<'a>(catalog: &'a Catalog, selector: &str) -> Option<&'a ModelSpec> {
	catalog
		.model(&ModelKey::from(selector))
		.or_else(|| catalog.resolve_alias(selector))
}

fn model_provider(catalog: &Catalog, selector: &str) -> Option<Str> {
	let model = resolve_model(catalog, selector)?;
	let route = catalog.route(model.routes.first()?)?;
	Some(Str::from(route.provider.as_str()))
}

fn model_uses_subscription(catalog: &Catalog, selector: &str) -> bool {
	resolve_model(catalog, selector)
		.and_then(|model| model.routes.first())
		.and_then(|route| catalog.route(route))
		.and_then(|route| catalog.provider(&route.provider))
		.is_some_and(|provider| provider_uses_oauth(catalog, provider))
}

fn resolve_login_provider(catalog: &Catalog, requested: &Str) -> Result<Str, Str> {
	let provider_id = ProviderId::from(requested.as_str());
	let Some(provider) = catalog.provider(&provider_id) else {
		return Err(sf!(
			"Unknown provider `{requested}`. Use `/login` to choose an available provider."
		));
	};
	if !provider_supports_login(catalog, provider) {
		return Err(sf!(
			"Provider `{}` does not support interactive authentication. Use `/login` to choose \
			 another provider.",
			provider.id
		));
	}
	Ok(Str::from(provider.id.as_str()))
}

/// Projects the live roster for the HUD. The roster exists to surface
/// subagent activity: a session whose only node is the root agent projects
/// empty, so the HUD stays out of the way until subagents actually run.
fn project_agent_roster(tree: &AgentTree, session: &str) -> Vec<AgentRow> {
	let rows: Vec<AgentRow> = tree
		.roster()
		.filter(|node| {
			node.session == session
				&& tree
					.node(&node.id)
					.is_some_and(|latest| Arc::ptr_eq(&latest, node))
		})
		.map(|node| {
			let usage = node.usage();
			let activity = node.activity();
			AgentRow {
				id:     node.id.clone(),
				name:   node.name.clone(),
				parent: node.parent.clone(),
				depth:  node.depth,
				status: Str::from(node.status().to_string()),
				tool:   (!activity.is_empty()).then_some(activity),
				tokens: Some(usage.input_tokens.saturating_add(usage.output_tokens)),
			}
		})
		.collect();
	if rows.iter().all(|row| row.parent.is_none()) && rows.len() <= 1 {
		return Vec::new();
	}
	rows
}

fn publish_agent_roster(
	backend: &flume::Sender<BackendEvent>,
	tree: &AgentTree,
	session: &str,
	last: &mut Vec<AgentRow>,
) {
	let current = project_agent_roster(tree, session);
	if current != *last {
		*last = current.clone();
		send_backend(backend, BackendEvent::AgentRoster(current));
	}
}

fn send_status(
	backend: &flume::Sender<BackendEvent>,
	state: &BridgeState,
	bus: &omp_agent::EventBus,
	dropped: u64,
) {
	send_backend(
		backend,
		BackendEvent::Status(StatusFacts {
			model: Str::from(state.model.as_str()),
			model_subscription: model_uses_subscription(Catalog::embedded(), &state.model),
			advisor_model: None,
			advisor_subscription: false,
			working: chat_active(state.submit_pending, bus.phase()),
			turn_started: state.turn_started,
			context_tokens: state.context_tokens,
			context_window: state.context_window,
			compaction_speculation: omp_chat_ui::CompactionSpeculationStatus::Idle,
			cost_nanos: state.cost_nanos,
			advisor_cost_nanos: 0,
			queued: state.queued,
			jobs: state.jobs.len(),
			attempt: state.attempt,
			dropped,
			git: None,
			live_activity: state.live_enabled.then_some(state.live_activity),
		}),
	);
}

fn send_backend(sender: &flume::Sender<BackendEvent>, event: BackendEvent) {
	let _ = sender.send(event);
}

fn drain_live_activity(events: &SubscriptionHandle, state: &mut BridgeState) -> bool {
	let mut changed = false;
	while let Ok(event) = events.try_recv() {
		let band = match &*event {
			FirehoseEvent::TurnStart(_) => 1,
			FirehoseEvent::TurnEnd(_) => 0,
			FirehoseEvent::ModelRequest(_) => 3,
			FirehoseEvent::ModelAttempt(_) | FirehoseEvent::ProviderError(_) => 4,
			FirehoseEvent::ToolCall(_) => 2,
			_ => continue,
		};
		if state.live_enabled {
			state.live_activity.push(band);
			changed = true;
		}
	}
	changed
}

fn apply_turn_budget(state: &AgentState, budget: Option<&ParsedTurnBudget>) {
	state.update(|snapshot| {
		snapshot.turn.params.task_budget = budget.map(|budget| budget.task);
	});
}

fn chat_active(submit_pending: bool, phase: AgentPhase) -> bool {
	submit_pending || phase != AgentPhase::Idle
}
const fn should_abort_empty(active: bool, queued: usize) -> bool {
	active && queued > 0
}

/// Interrupt class delivering a submission into an active turn: Enter
/// steers immediately, Alt+Enter queues an idle follow-up.
const fn active_submit_class(mode: SubmitMode) -> InterruptClass {
	match mode {
		SubmitMode::Steer => InterruptClass::Immediate,
		SubmitMode::FollowUp => InterruptClass::Idle,
	}
}

const fn startup_recovery_needed(pending_turn: bool, pending_input_submission: bool) -> bool {
	pending_turn || pending_input_submission
}

/// Returns whether an authentication prompt must suppress terminal echo.
pub const fn prompt_masks_input(kind: AuthPromptKind) -> bool {
	!matches!(kind, AuthPromptKind::Confirmation | AuthPromptKind::PlainText)
}

/// Converts the scene's prompt answer to the inference authentication input.
pub fn auth_input(kind: AuthPromptKind, value: String) -> AuthInput {
	match kind {
		AuthPromptKind::ApiKey => AuthInput::ApiKey(SecretString::from(value)),
		AuthPromptKind::AuthorizationCode if url_shaped(&value) => {
			AuthInput::CallbackUrl(SecretString::from(value))
		},
		AuthPromptKind::AuthorizationCode => AuthInput::AuthorizationCode(SecretString::from(value)),
		AuthPromptKind::SessionToken => AuthInput::SessionToken(SecretString::from(value)),
		AuthPromptKind::PlainText => AuthInput::PlainText(Str::from(value)),
		AuthPromptKind::OptionalSecret => AuthInput::OptionalSecret(SecretString::from(value)),
		AuthPromptKind::Confirmation => AuthInput::DeviceConfirmed,
	}
}

fn url_shaped(value: &str) -> bool {
	let Some((scheme, _)) = value.split_once("://") else {
		return false;
	};
	let mut chars = scheme.chars();
	chars
		.next()
		.is_some_and(|first| first.is_ascii_alphabetic())
		&& chars.all(|char| char.is_ascii_alphanumeric() || matches!(char, '+' | '-' | '.'))
}

async fn next_auth_event(auth: Option<&ChatAuth>) -> Option<ChatAuthEvent> {
	match auth {
		Some(auth) => auth.next_event().await,
		None => pending().await,
	}
}

/// Current Unix time in milliseconds for canonical user items.
pub fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

#[cfg(test)]
mod tests {
	use omp_agent::{AgentKind, AgentStatus, Budget, ExecutionModeHandle};
	use omp_core::ExposeSecret as _;
	use omp_tui::{
		Color, Size, UiContext, components::AttachmentContent, test_support::frame_row_text,
	};

	use super::*;

	#[test]
	fn roster_projection_keeps_only_the_current_canonical_session_nodes() {
		let tree = AgentTree::new(4, 4, 4);
		let old = tree
			.register(
				sf!("main"),
				sf!("Main"),
				AgentKind::Main,
				None,
				sf!("session-a"),
				Budget::default(),
			)
			.expect("old root");
		old.set_status(AgentStatus::Completed);
		let latest = tree
			.register(
				sf!("main"),
				sf!("Main"),
				AgentKind::Main,
				None,
				sf!("session-a"),
				Budget::default(),
			)
			.expect("replacement root");
		latest.set_status(AgentStatus::Running);
		tree
			.register(
				sf!("other"),
				sf!("Main"),
				AgentKind::Main,
				None,
				sf!("session-b"),
				Budget::default(),
			)
			.expect("other session");

		// A lone root is not worth a HUD: the roster projects empty until
		// a subagent joins the session.
		assert!(project_agent_roster(&tree, "session-a").is_empty());

		tree
			.register(
				sf!("worker"),
				sf!("Worker"),
				AgentKind::Subagent,
				Some(sf!("main")),
				sf!("session-a"),
				Budget::default(),
			)
			.expect("subagent");
		let rows = project_agent_roster(&tree, "session-a");
		assert_eq!(rows.len(), 2);
		let main = rows
			.iter()
			.find(|row| row.id == "main")
			.expect("canonical main");
		assert_eq!(main.status, "running", "the replacement root is the canonical node");
		assert!(rows.iter().any(|row| row.id == "worker"));
		assert!(project_agent_roster(&tree, "session-b").is_empty(), "other sessions stay solo");
	}

	fn test_bridge_state() -> BridgeState {
		BridgeState {
			model:             "test/model".to_owned(),
			context_window:    None,
			context_tokens:    0,
			cost_nanos:        0,
			queued:            0,
			jobs:              HashSet::new(),
			attempt:           0,
			turn_started:      None,
			submit_pending:    false,
			pending_prompt:    None,
			part_serial:       0,
			active_parts:      HashMap::new(),
			streaming_tools:   HashMap::new(),
			tools:             HashMap::new(),
			rewind_targets:    Vec::new(),
			pending_auth_kind: None,
			live_enabled:      false,
			live_activity:     ActivityWaveform::new(),
			replaying_turn:    false,
			settings:          Settings::default(),
			commands:          CommandRoster::new(Vec::new()),
		}
	}

	#[test]
	fn chat_event_subscription_keeps_bursts_beyond_the_old_ui_capacity() {
		let bus = omp_agent::EventBus::new();
		let events = subscribe_chat_events(&bus);
		for generation in 0..300 {
			bus.publish(AgentEvent::RosterChanged { generation });
		}
		assert_eq!(events.len(), 300);
	}

	#[test]
	fn active_turn_text_and_submit_errors_project_into_visible_transcript_rows() {
		let (tx, rx) = flume::unbounded();
		let mut state = test_bridge_state();
		let modes = ExecutionModes::new(ExecutionModeHandle::default());
		let registry = Registry::new();
		let bus = omp_agent::EventBus::new();
		for event in [
			Event::PartStart(omp_proto::inference::v1::PartStart {
				index:        0,
				kind:         part_start::Kind::Text as i32,
				tool_call_id: String::new(),
				tool_name:    String::new(),
			}),
			Event::PartDelta(omp_proto::inference::v1::PartDelta {
				index: 0,
				chunk: Bytes::from_static(b"banana"),
			}),
			Event::PartEnd(omp_proto::inference::v1::PartEnd {
				index:     0,
				signature: Bytes::new(),
			}),
		] {
			handle_agent_event(
				&tx,
				&mut state,
				&AgentEvent::Turn {
					turn_id: TurnId::new("active-turn"),
					event:   Box::new(omp_proto::inference::v1::TurnEvent { event: Some(event) }),
				},
				&modes,
				&registry,
				&bus,
				0,
			);
		}
		send_backend(&tx, BackendEvent::Error(sf!("Submit error: unauthorized")));

		let mut chat = Chat::new(&UiContext::default());
		let viewport = Size::new(80, 30);
		let _ = chat.render(viewport);
		let _ = chat.apply_backend_event(BackendEvent::UserReplayed {
			text:  sf!("say banana"),
			chips: Vec::new(),
		});
		for event in rx.drain() {
			let _ = chat.apply_backend_event(event);
		}
		let rendered = chat.render(viewport);
		let transcript = (0..rendered.stable_rows)
			.map(|row| frame_row_text(rendered.frame, row))
			.collect::<Vec<_>>()
			.join("\n");
		assert!(transcript.contains("say banana"), "{transcript}");
		assert!(transcript.contains("banana"), "{transcript}");
		assert!(transcript.contains("Submit error: unauthorized"), "{transcript}");
	}

	#[test]
	fn blank_submission_interrupts_only_with_queued_work() {
		assert!(!should_abort_empty(false, 0));
		assert!(!should_abort_empty(true, 0));
		assert!(!should_abort_empty(false, 1));
		assert!(should_abort_empty(true, 1));
	}

	#[test]
	fn active_submissions_map_enter_to_steer_and_follow_up_to_idle() {
		assert_eq!(active_submit_class(SubmitMode::Steer), InterruptClass::Immediate);
		assert_eq!(active_submit_class(SubmitMode::FollowUp), InterruptClass::Idle);
	}

	#[test]
	fn authentication_prompt_masking_matches_input_kind() {
		assert!(prompt_masks_input(AuthPromptKind::ApiKey));
		assert!(prompt_masks_input(AuthPromptKind::OptionalSecret));
		assert!(!prompt_masks_input(AuthPromptKind::PlainText));
		assert!(!prompt_masks_input(AuthPromptKind::Confirmation));
	}

	#[test]
	fn auth_input_preserves_other_prompt_kinds() {
		assert!(matches!(
			auth_input(AuthPromptKind::ApiKey, "secret".to_owned()),
			AuthInput::ApiKey(_)
		));
		assert!(matches!(
			auth_input(AuthPromptKind::PlainText, "visible".to_owned()),
			AuthInput::PlainText(value) if value.as_str() == "visible"
		));
		assert!(matches!(
			auth_input(AuthPromptKind::Confirmation, String::new()),
			AuthInput::DeviceConfirmed
		));
	}

	#[test]
	fn auth_input_maps_redirect_urls_to_callback_urls() {
		let AuthInput::CallbackUrl(value) = auth_input(
			AuthPromptKind::AuthorizationCode,
			"http://localhost:54545/callback?code=abc&state=xyz".to_owned(),
		) else {
			panic!("redirect URL must be submitted as a callback URL");
		};
		assert_eq!(value.expose_secret(), "http://localhost:54545/callback?code=abc&state=xyz");
	}

	#[test]
	fn auth_input_keeps_bare_authorization_codes() {
		assert!(matches!(
			auth_input(AuthPromptKind::AuthorizationCode, "abc-123".to_owned()),
			AuthInput::AuthorizationCode(value) if value.expose_secret() == "abc-123"
		));
	}

	#[test]
	fn text_attachment_lowers_after_typed_text() {
		let mut item = input::user_message("typed");
		let attachment = Attachment::new(
			AttachmentContent::Text {
				text:    sf!("pasted"),
				snippet: sf!("pasted"),
				lines:   1,
				chars:   6,
			},
			1,
			Color::Default,
		);
		let chips = lower_attachments(&mut item, vec![attachment], |_| {});
		let Some(item::Kind::Message(message)) = item.kind else {
			panic!("message")
		};
		assert_eq!(message.parts.len(), 2);
		assert!(matches!(
			&message.parts[1].kind,
			Some(part::Kind::Text(text)) if text == "<attachment>pasted</attachment>"
		));
		assert_eq!(chips[0].as_str(), "paste · 1 lines");
	}

	#[test]
	fn image_attachment_lowers_to_inline_hashed_blob() {
		let path = std::env::temp_dir()
			.join(format!("omp-chat-attachment-{}.png", omp_core::Ulid::generate()));
		let bytes = b"not-a-decoded-image";
		std::fs::write(&path, bytes).expect("write attachment fixture");
		let mut item = input::user_message("inspect");
		let attachment = Attachment::new(
			AttachmentContent::Image {
				source:     Str::from(path.to_string_lossy().as_ref()),
				dimensions: None,
			},
			1,
			Color::Default,
		);
		let mut errors = Vec::new();
		let chips = lower_attachments(&mut item, vec![attachment], |error| errors.push(error));
		std::fs::remove_file(path).expect("remove attachment fixture");
		assert!(errors.is_empty());
		let Some(item::Kind::Message(message)) = item.kind else {
			panic!("message")
		};
		let Some(part::Kind::Blob(blob)) = &message.parts[1].kind else {
			panic!("blob")
		};
		assert_eq!(blob.mime, "image/png");
		assert_eq!(blob.inline.as_ref(), bytes);
		assert_eq!(blob.hash.as_ref(), Hash32::sum(bytes).as_bytes());
		assert_eq!(chips.len(), 1);
	}

	#[test]
	fn png_tool_result_blobs_surface_as_inline_image_events() {
		let (tx, rx) = flume::unbounded();
		let png: &[u8] = b"\x89PNG\r\n\x1a\nfake";
		let item = Item {
			kind: Some(item::Kind::ToolResult(omp_proto::thread::v1::ToolResult {
				call_id: "call-1".to_owned(),
				name: "read".to_owned(),
				parts: vec![
					Part { kind: Some(part::Kind::Text("rendered page 1".to_owned())) },
					Part {
						kind: Some(part::Kind::Blob(Blob {
							hash:   Bytes::from_static(b"0123456789abcdef0123456789abcdef"),
							mime:   "image/png".to_owned(),
							size:   png.len() as u64,
							inline: Bytes::from_static(png),
							detail: blob::Detail::Original as i32,
						})),
					},
				],
				..Default::default()
			})),
			..Default::default()
		};
		send_tool_result_images(&tx, &sf!("call-1"), &item);
		let events: Vec<_> = rx.drain().collect();
		let Some(BackendEvent::ToolImage { id, source }) = events.first() else {
			panic!("PNG blob produces a ToolImage event");
		};
		assert_eq!(id.as_str(), "call-1");
		let persisted = std::fs::read(source.as_str()).expect("persisted image payload");
		assert_eq!(persisted, png);
		assert_eq!(events.len(), 1, "model-facing text is not mined into the UI view");
		std::fs::remove_file(source.as_str()).ok();
	}

	#[test]
	fn non_png_tool_result_blobs_defer_to_the_structured_view() {
		let (tx, rx) = flume::unbounded();
		let item = Item {
			kind: Some(item::Kind::ToolResult(omp_proto::thread::v1::ToolResult {
				call_id: "call-2".to_owned(),
				name: "read".to_owned(),
				parts: vec![Part {
					kind: Some(part::Kind::Blob(Blob {
						hash:   Bytes::new(),
						mime:   "image/jpeg".to_owned(),
						size:   4,
						inline: Bytes::from_static(b"jpeg"),
						detail: blob::Detail::Original as i32,
					})),
				}],
				..Default::default()
			})),
			..Default::default()
		};
		send_tool_result_images(&tx, &sf!("call-2"), &item);
		assert!(rx.is_empty());
	}

	#[test]
	fn startup_recovery_covers_both_durable_crash_windows() {
		assert!(!startup_recovery_needed(false, false));
		assert!(startup_recovery_needed(true, false));
		assert!(startup_recovery_needed(false, true));
		assert!(startup_recovery_needed(true, true));
	}

	#[derive(Default)]
	struct TestFold {
		updates: String,
	}

	#[derive(serde::Deserialize)]
	struct TestUpdate {
		text: String,
	}

	struct TestRenderer(&'static str);

	impl omp_tool::render::Render for TestRenderer {
		type Outcome = serde_json::Value;
		type State = TestFold;
		type Update = TestUpdate;

		fn fold(&self, state: &mut Self::State, update: Self::Update) {
			state.updates.push_str(&update.text);
		}

		fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
			let branch = outcome
				.and_then(|outcome| outcome.get("kind"))
				.and_then(serde_json::Value::as_str)
				.unwrap_or("live");
			Some(sf!("<row>{}:{}:{branch}</row>", self.0, state.updates))
		}
	}

	fn test_identity(rev: &str) -> ToolIdentity {
		ToolIdentity { name: sf!("same"), rev: rev.parse().expect("valid test revision") }
	}

	fn json_proto(value: serde_json::Value) -> omp_proto::inference::v1::Value {
		let kind = match value {
			serde_json::Value::Null => value::Kind::Null(true),
			serde_json::Value::Bool(value) => value::Kind::Bool(value),
			serde_json::Value::String(value) => value::Kind::String(value),
			serde_json::Value::Number(value) => value
				.as_i64()
				.map_or_else(|| value::Kind::Uint(value.as_u64().expect("integer")), value::Kind::Int),
			serde_json::Value::Array(values) => {
				value::Kind::List(omp_proto::inference::v1::ValueList {
					values: values.into_iter().map(json_proto).collect(),
				})
			},
			serde_json::Value::Object(values) => {
				value::Kind::Map(omp_proto::inference::v1::ValueMap {
					fields: values
						.into_iter()
						.map(|(key, value)| (key, json_proto(value)))
						.collect(),
				})
			},
		};
		omp_proto::inference::v1::Value { kind: Some(kind) }
	}

	fn revision_props(rev: &str) -> omp_proto::inference::v1::ValueMap {
		omp_proto::inference::v1::ValueMap {
			fields: [(TOOL_REV_PROP.to_owned(), omp_proto::inference::v1::Value {
				kind: Some(value::Kind::String(rev.to_owned())),
			})]
			.into(),
		}
	}

	fn result_item(call_id: &str, rev: Option<&str>, branch: &str) -> Item {
		Item {
			kind: Some(item::Kind::ToolResult(omp_proto::thread::v1::ToolResult {
				call_id: call_id.to_owned(),
				name: "same".to_owned(),
				details: Some(json_proto(serde_json::json!({
					"kind": branch,
					"value": { "fact": format!("{branch}-fact") }
				}))),
				is_error: branch == "faulted",
				..Default::default()
			})),
			props: rev.map(revision_props),
			..Default::default()
		}
	}

	#[test]
	fn exact_revision_renderers_fold_streamed_updates_independently() {
		let mut registry = Registry::new();
		registry
			.register_renderer(test_identity("test.1"), TestRenderer("one"))
			.expect("register revision one");
		registry
			.register_renderer(test_identity("test.2"), TestRenderer("two"))
			.expect("register revision two");
		let mut first = ToolDisplay {
			identity: test_identity("test.1"),
			args:     omp_slopjson::Value::Object(omp_slopjson::Object::new()),
			started:  true,
			fold:     ViewState::new(),
		};
		let mut second = ToolDisplay {
			identity: test_identity("test.2"),
			args:     omp_slopjson::Value::Object(omp_slopjson::Object::new()),
			started:  true,
			fold:     ViewState::new(),
		};
		assert_eq!(
			fold_tool_update(&registry, &mut first, Bytes::from_static(br#"{"text":"a"}"#)).as_str(),
			"<row>one:a:live</row>"
		);
		assert_eq!(
			fold_tool_update(&registry, &mut first, Bytes::from_static(br#"{"text":"b"}"#)).as_str(),
			"<row>one:ab:live</row>"
		);
		assert_eq!(
			fold_tool_update(&registry, &mut second, Bytes::from_static(br#"{"text":"z"}"#)).as_str(),
			"<row>two:z:live</row>"
		);
	}

	#[test]
	fn durable_branches_and_missing_revisions_preserve_structured_facts() {
		let mut registry = Registry::new();
		registry
			.register_renderer(test_identity("test.1"), TestRenderer("exact"))
			.expect("register exact renderer");
		for branch in ["ok", "faulted", "args_rejected", "aborted"] {
			let (_, ok, view) =
				render_tool_result_view(&registry, &result_item("call", Some("test.1"), branch), None);
			assert_eq!(ok, branch == "ok");
			assert!(view.contains(branch), "{view}");
		}

		let unknown = result_item("unknown", Some("unknown.9"), "faulted");
		let (identity, ok, view) = render_tool_result_view(&registry, &unknown, None);
		assert_eq!(identity.rev.to_string(), "unknown.9");
		assert!(!ok);
		assert!(view.contains(r#""kind":"faulted""#));
		assert!(view.contains("faulted-fact"));

		let missing = result_item("missing", None, "aborted");
		let (identity, ok, view) = render_tool_result_view(&registry, &missing, None);
		assert_eq!(identity.rev.n, 0);
		assert!(!ok);
		assert!(view.contains(r#""kind":"aborted""#));
		assert!(view.contains("aborted-fact"));
	}

	#[test]
	fn replay_uses_durable_revision_and_is_deterministic() {
		let mut registry = Registry::new();
		registry
			.register_renderer(test_identity("test.1"), TestRenderer("one"))
			.expect("register revision one");
		registry
			.register_renderer(test_identity("test.2"), TestRenderer("two"))
			.expect("register revision two");
		let items = [
			Item {
				kind: Some(item::Kind::ToolCall(omp_proto::thread::v1::ToolCall {
					id: "call".to_owned(),
					name: "same".to_owned(),
					args_json: Bytes::from_static(b"{}"),
					..Default::default()
				})),
				props: Some(revision_props("test.1")),
				..Default::default()
			},
			result_item("call", Some("test.2"), "ok"),
		];
		let replay = || {
			let (tx, rx) = flume::unbounded();
			let mut tools = HashMap::new();
			let mut serial = 0;
			replay_items(&tx, &items, &mut tools, &mut serial, &registry);
			rx.drain()
				.find_map(|event| match event {
					BackendEvent::ToolFinished { view, .. } => Some(view),
					_ => None,
				})
				.expect("replayed tool result")
		};
		let first = replay();
		let second = replay();
		assert_eq!(first, second);
		assert_eq!(first.as_str(), "<row>two::ok</row>");
	}
}
