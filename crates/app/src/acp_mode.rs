//! Agent Client Protocol (ACP) server over newline-delimited JSON on stdio.

use std::{
	collections::HashMap,
	io::IsTerminal as _,
	path::{Path, PathBuf},
	process::Stdio,
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use bytes::Bytes;
use flume::{Receiver, Sender};
use futures::StreamExt as _;
use miette::{IntoDiagnostic as _, miette};
use omp_agent::{ApprovalBook, ApprovalDecision, ApprovalSource, ApprovalSpec, Journal};
use omp_core::{Str, sf};
use omp_llm_catalog::{ModelKey, ReasoningEffort};
use omp_llm_inference::{
	Client, Registry,
	call::{
		CallMeta, ChatRequest, ContentPart, MediaInput, Message as InferenceMessage,
		NegotiationPolicy, ReasoningRequest, ReasoningVisibility, Role as InferenceRole, Sampling,
		Setting, Target,
	},
	event::{ChatEvent, FinishReason},
	id::RequestId as InferenceRequestId,
	receipt::ExecutionBudget,
};
use omp_proto::thread::v1::{Item, Message, Part, Role, item, part};
use omp_storage::{
	index::{IndexedWriteError, NewSession, SessionFilter, SessionIndex, SessionKind},
	transcript::{Header, Kind, SessionId, TitleSource},
};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio_util::sync::CancellationToken;

use crate::cli::{AcpArgs, data_dir, turn_id};

const CANCEL_CLEANUP: Duration = Duration::from_secs(2);

/// Runs ACP using stdin for NDJSON requests and stdout for NDJSON responses.
pub async fn run(args: AcpArgs) -> miette::Result<()> {
	if std::io::stdin().is_terminal() {
		eprintln!("warning: `omp acp` expects newline-delimited JSON on stdin");
	}
	let root = std::fs::canonicalize(&args.project).into_diagnostic()?;
	let data = data_dir(None)?;
	let state_dir = crate::project_state::directory(&data, &root).into_diagnostic()?;
	let sessions_dir = state_dir.join("sessions");
	std::fs::create_dir_all(&sessions_dir).into_diagnostic()?;
	let index = Arc::new(SessionIndex::open(state_dir.join("sessions.sqlite3")).into_diagnostic()?);
	let store =
		crate::daemon::open_credential_store(data.join("credentials.db")).into_diagnostic()?;
	let registry = crate::daemon::production_registry(&data, store)
		.await
		.into_diagnostic()?;
	let model = args
		.model
		.or_else(|| {
			crate::settings::Settings::load(&data)
				.default_model
				.map(Str::from)
		})
		.ok_or_else(|| miette!("acp mode requires --model or config.default_model"))?;
	let models = registry
		.catalog()
		.models()
		.iter()
		.map(|entry| entry.key.as_str().to_owned())
		.collect();
	let (output_tx, output_rx) = flume::unbounded();
	let writer = tokio::spawn(write_ndjson(tokio::io::stdout(), output_rx));
	let runtime = Arc::new(Runtime {
		registry,
		output: output_tx.clone(),
		state: Mutex::new(State {
			initialized: false,
			root,
			sessions_dir,
			index,
			sessions: HashMap::new(),
			active: HashMap::new(),
			approvals: ApprovalBook::new(),
			model: model.to_string(),
			models,
			mode: "default".into(),
			thinking: "auto".into(),
			terminal_auth: args.acp_terminal_auth,
		}),
	});
	let result = read_ndjson(Arc::clone(&runtime)).await;
	let active = {
		let mut state = runtime.state.lock();
		state
			.active
			.drain()
			.map(|(_, token)| token)
			.collect::<Vec<_>>()
	};
	for token in active {
		token.cancel();
	}
	tokio::time::sleep(CANCEL_CLEANUP.min(Duration::from_millis(20))).await;
	drop(runtime);
	drop(output_tx);
	writer.await.into_diagnostic()??;
	result
}

async fn read_ndjson(runtime: Arc<Runtime>) -> miette::Result<()> {
	let mut lines = BufReader::new(tokio::io::stdin()).lines();
	while let Some(line) = lines.next_line().await.into_diagnostic()? {
		if line.trim().is_empty() {
			continue;
		}
		let value: Value = match serde_json::from_str(&line) {
			Ok(value) => value,
			Err(error) => {
				runtime.error(Value::Null, -32700, error.to_string())?;
				continue;
			},
		};
		runtime.dispatch(value).await?;
	}
	Ok(())
}

async fn write_ndjson<W: tokio::io::AsyncWrite + Unpin>(
	mut output: W,
	receiver: Receiver<Value>,
) -> miette::Result<()> {
	while let Ok(value) = receiver.recv_async().await {
		let mut bytes = serde_json::to_vec(&value).into_diagnostic()?;
		bytes.push(b'\n');
		output.write_all(&bytes).await.into_diagnostic()?;
		output.flush().await.into_diagnostic()?;
	}
	Ok(())
}

struct Runtime {
	registry: Registry,
	output:   Sender<Value>,
	state:    Mutex<State>,
}
struct State {
	initialized:   bool,
	root:          PathBuf,
	sessions_dir:  PathBuf,
	index:         Arc<SessionIndex>,
	sessions:      HashMap<Str, AcpSession>,
	active:        HashMap<Str, CancellationToken>,
	approvals:     ApprovalBook,
	model:         String,
	models:        Vec<String>,
	mode:          String,
	thinking:      String,
	terminal_auth: bool,
}
struct AcpSession {
	title:    Option<Str>,
	messages: Vec<StoredMessage>,
	journal:  Journal,
}
#[derive(Clone)]
struct StoredMessage {
	role:   InferenceRole,
	parts:  Vec<ContentPart>,
	replay: Vec<Value>,
}

impl Runtime {
	async fn dispatch(self: &Arc<Self>, frame: Value) -> miette::Result<()> {
		let id = frame.get("id").cloned();
		let Some(method) = frame.get("method").and_then(Value::as_str) else {
			if let Some(id) = id {
				self.error(id, -32600, "request has no method")?;
			}
			return Ok(());
		};
		let params = frame
			.get("params")
			.and_then(Value::as_object)
			.cloned()
			.unwrap_or_default();
		if method != "initialize" && !self.state.lock().initialized {
			if let Some(id) = id {
				self.error(id, -32002, "initialize must complete before other requests")?;
			}
			return Ok(());
		}
		if method == "session/prompt" {
			let Some(id) = id else {
				return Ok(());
			};
			match self.start_prompt(id.clone(), params) {
				Ok(()) => {},
				Err(error) => self.error(id, -32602, error.to_string())?,
			}
			return Ok(());
		}
		let result = match method {
			"initialize" => self.initialize(&params),
			"authenticate" => self.authenticate(&params).await,
			"session/new" => self.new_session(None, &params),
			"session/load" | "session/resume" => self.load_session(&params),
			"session/list" => self.list_sessions(&params),
			"session/close" => self.close_session(&params),
			"session/fork" => self.fork_session(&params),
			"session/cancel" => self.cancel(&params),
			"session/set_mode" => self.set_mode(&params),
			"session/set_model" => self.set_model(&params),
			"session/set_thinking" => self.set_thinking(&params),
			"session/elicitation" => self.elicit(&params),
			"session/approve" => self.approve(&params),
			_ => Err(miette!("unknown ACP method `{method}`")),
		};
		if let Some(id) = id {
			match result {
				Ok(value) => self.respond(id, value)?,
				Err(error) => self.error(id, -32602, error.to_string())?,
			}
		}
		Ok(())
	}

	fn initialize(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let version = params
			.get("protocolVersion")
			.and_then(Value::as_u64)
			.unwrap_or(1);
		if version != 1 {
			return Err(miette!("unsupported ACP protocol version {version}"));
		}
		let mut state = self.state.lock();
		state.initialized = true;
		let mut auth = vec![json!({"id":"none","name":"Configured credentials"})];
		if state.terminal_auth {
			auth.push(json!({"id":"terminal","name":"Authenticate in terminal"}));
		}
		Ok(
			json!({"protocolVersion":1,"agentInfo":{"name":"omp","version":env!("CARGO_PKG_VERSION")},"authMethods":auth,"agentCapabilities":{"loadSession":true,"resumeSession":true,"sessionCapabilities":{"fork":{},"list":{},"close":{}},"promptCapabilities":{"image":true,"audio":false,"embeddedContext":true},"mcpCapabilities":{"http":false,"sse":false},"modes":[{"id":"default","name":"Default"},{"id":"plan","name":"Plan"}]}}),
		)
	}

	async fn authenticate(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let method = required_text(params, "methodId")?;
		if method == "none" {
			return Ok(json!({}));
		}
		if method != "terminal" || !self.state.lock().terminal_auth {
			return Err(miette!("authentication method `{method}` was not advertised"));
		}
		let provider = required_text(params, "provider")?;
		let status = spawn_terminal_auth(provider).await?;
		if !status.success() {
			return Err(miette!("terminal authentication exited with {status}"));
		}
		Ok(json!({}))
	}

	fn new_session(
		&self,
		parent: Option<Str>,
		params: &Map<String, Value>,
	) -> miette::Result<Value> {
		let mut state = self.state.lock();
		if let Some(mode) = params.get("modeId").and_then(Value::as_str) {
			if !matches!(mode, "default" | "plan") {
				return Err(miette!("unknown mode `{mode}`"));
			}
			state.mode = mode.to_owned();
		}
		if let Some(model) = params.get("modelId").and_then(Value::as_str) {
			if !state.models.iter().any(|candidate| candidate == model) {
				return Err(miette!("unknown model `{model}`"));
			}
			state.model = model.to_owned();
		}
		if let Some(thinking) = params.get("thinking").and_then(Value::as_str) {
			if !matches!(thinking, "auto" | "none" | "low" | "medium" | "high") {
				return Err(miette!("unknown thinking level `{thinking}`"));
			}
			state.thinking = thinking.to_owned();
		}
		let id = Str::from(ulid::Ulid::generate().to_string());
		let journal = create_journal(&state, &id, parent.as_ref())?;
		state
			.sessions
			.insert(id.clone(), AcpSession { title: None, messages: Vec::new(), journal });
		drop(state);
		self.push_initial(&id)?;
		Ok(json!({"sessionId":id}))
	}

	fn load_session(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let id = Str::from(required_text(params, "sessionId")?);
		let mut state = self.state.lock();
		let path = state.sessions_dir.join(format!("{id}.jsonl"));
		let mut journal = Journal::open(&path).into_diagnostic()?;
		journal.attach_session_index(Arc::clone(&state.index), SessionId(id.clone()));
		let messages = replay_messages(&path)?;
		let replay = messages
			.iter()
			.flat_map(|message| message.replay.clone())
			.collect::<Vec<_>>();
		state
			.sessions
			.insert(id.clone(), AcpSession { title: None, messages, journal });
		drop(state);
		self.push_initial(&id)?;
		for update in replay {
			self.update(&id, update)?;
		}
		Ok(json!({"sessionId":id}))
	}

	fn list_sessions(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let state = self.state.lock();
		let limit = params
			.get("limit")
			.and_then(Value::as_u64)
			.unwrap_or(100)
			.min(500) as u32;
		let page = state
			.index
			.list(&SessionFilter {
				project: Some(Str::from(state.root.to_string_lossy().into_owned())),
				limit,
				..SessionFilter::default()
			})
			.into_diagnostic()?;
		Ok(
			json!({"sessions":page.sessions.into_iter().map(|row| json!({"sessionId":row.id.0,"title":row.title,"cwd":row.cwd,"createdAt":row.created_ms,"updatedAt":row.updated_ms,"parentSessionId":row.parent.map(|id|id.0)})).collect::<Vec<_>>() }),
		)
	}

	fn close_session(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let id = Str::from(required_text(params, "sessionId")?);
		let mut state = self.state.lock();
		if let Some(token) = state.active.remove(&id) {
			token.cancel();
		}
		if state.sessions.remove(&id).is_none() {
			return Err(miette!("unknown session `{id}`"));
		}
		Ok(json!({}))
	}

	fn fork_session(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let source = Str::from(required_text(params, "sessionId")?);
		if !self.state.lock().sessions.contains_key(&source) {
			self.load_session(&Map::from_iter([("sessionId".into(), json!(source))]))?;
		}
		let messages = self
			.state
			.lock()
			.sessions
			.get(&source)
			.map(|session| session.messages.clone())
			.ok_or_else(|| miette!("unknown session `{source}`"))?;
		let result = self.new_session(Some(source), &Map::new())?;
		let id = Str::from(result["sessionId"].as_str().expect("new session id"));
		let mut state = self.state.lock();
		let session = state.sessions.get_mut(&id).expect("new session retained");
		for message in messages {
			append_stored(session, message)?;
		}
		drop(state);
		self.update(&id, json!({"sessionUpdate":"history_replay_complete"}))?;
		Ok(result)
	}

	fn cancel(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let id = Str::from(required_text(params, "sessionId")?);
		let state = self.state.lock();
		let cancelled = state.active.get(&id).is_some_and(|token| {
			token.cancel();
			true
		});
		Ok(json!({"cancelled":cancelled}))
	}

	fn set_mode(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let mode = required_text(params, "modeId")?;
		if !matches!(mode, "default" | "plan") {
			return Err(miette!("unknown mode `{mode}`"));
		}
		let session = Str::from(required_text(params, "sessionId")?);
		self.state.lock().mode = mode.to_owned();
		self.update(&session, json!({"sessionUpdate":"current_mode_update","currentModeId":mode}))?;
		Ok(json!({}))
	}

	fn set_model(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let model = required_text(params, "modelId")?;
		let session = Str::from(required_text(params, "sessionId")?);
		let mut state = self.state.lock();
		if !state.models.iter().any(|candidate| candidate == model) {
			return Err(miette!("unknown model `{model}`"));
		}
		state.model = model.to_owned();
		drop(state);
		self.update(&session, json!({"sessionUpdate":"config_option_update","configOptions":[{"id":"model","currentValue":model}]}))?;
		Ok(json!({}))
	}

	fn set_thinking(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let thinking = required_text(params, "thinking")?;
		if !matches!(thinking, "auto" | "none" | "low" | "medium" | "high") {
			return Err(miette!("unknown thinking level `{thinking}`"));
		}
		let session = Str::from(required_text(params, "sessionId")?);
		self.state.lock().thinking = thinking.to_owned();
		self.update(&session, json!({"sessionUpdate":"config_option_update","configOptions":[{"id":"thinking","currentValue":thinking}]}))?;
		Ok(json!({}))
	}

	fn elicit(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let session_id = Str::from(required_text(params, "sessionId")?);
		let title = required_text(params, "title")?;
		let body = required_text(params, "body")?;
		let mut state = self.state.lock();
		let ticket = state.approvals.file(
			params
				.get("invocationId")
				.and_then(Value::as_str)
				.map(Str::from),
			vec![ApprovalSpec {
				title:         Str::from(title),
				body:          Str::from(body),
				subject:       params
					.get("subject")
					.and_then(Value::as_str)
					.map_or_else(|| Str::from(title), Str::from),
				kind:          sf!("acp_elicitation"),
				scopes:        vec![sf!("once")],
				default:       None,
				route:         sf!("acp"),
				approver:      None,
				timeout_ms:    0,
				unreachable:   sf!("deny"),
				require_human: true,
				pattern:       None,
				evidence:      Vec::new(),
			}],
			now_ms(),
		);
		let session = state
			.sessions
			.get_mut(&session_id)
			.ok_or_else(|| miette!("unknown session `{session_id}`"))?;
		session
			.journal
			.record_approval_ticket(now_ms(), ticket.filed_record())
			.into_diagnostic()?;
		drop(state);
		self.update(&session_id, json!({"sessionUpdate":"permission_request","ticketId":ticket.ticket_id,"title":title,"body":body,"options":[{"optionId":"approve_once","name":"Approve once"},{"optionId":"reject_once","name":"Reject"}]}))?;
		Ok(json!({"ticketId":ticket.ticket_id}))
	}

	fn approve(&self, params: &Map<String, Value>) -> miette::Result<Value> {
		let session_id = Str::from(required_text(params, "sessionId")?);
		let ticket_id = required_text(params, "ticketId")?;
		let approved = params
			.get("approved")
			.and_then(Value::as_bool)
			.ok_or_else(|| miette!("missing boolean `approved`"))?;
		let mut state = self.state.lock();
		let ticket = state
			.approvals
			.decide(ticket_id, ApprovalDecision {
				approved,
				scope: sf!("once"),
				source: ApprovalSource::External,
				decided_by: params
					.get("decidedBy")
					.and_then(Value::as_str)
					.map(Str::from),
				reason: params.get("reason").and_then(Value::as_str).map(Str::from),
				audited: true,
			})
			.ok_or_else(|| miette!("unknown approval ticket `{ticket_id}`"))?;
		let record = ticket.decision_record().expect("decided ticket has record");
		state
			.sessions
			.get_mut(&session_id)
			.ok_or_else(|| miette!("unknown session `{session_id}`"))?
			.journal
			.record_approval_decision(now_ms(), record)
			.into_diagnostic()?;
		Ok(json!({"approved":approved}))
	}

	fn start_prompt(
		self: &Arc<Self>,
		request_id: Value,
		params: Map<String, Value>,
	) -> miette::Result<()> {
		let session_id = Str::from(required_text(&params, "sessionId")?);
		let blocks = params
			.get("prompt")
			.or_else(|| params.get("content"))
			.ok_or_else(|| miette!("missing prompt content"))?;
		let (parts, replay) = convert_blocks(blocks)?;
		if parts.is_empty() {
			return Err(miette!("prompt contains no supported content"));
		}
		let proposed_title = parts.iter().find_map(|part| match part {
			ContentPart::Text { text, .. } if !text.trim().is_empty() => {
				Some(Str::from(text.chars().take(80).collect::<String>()))
			},
			_ => None,
		});
		let token = CancellationToken::new();
		let title_changed = {
			let mut state = self.state.lock();
			if state.active.contains_key(&session_id) {
				return Err(miette!("session is busy"));
			}
			let session = state
				.sessions
				.get_mut(&session_id)
				.ok_or_else(|| miette!("unknown session `{session_id}`"))?;
			let changed = session.title.is_none() && proposed_title.is_some();
			if let Some(title) = proposed_title.filter(|_| changed) {
				session
					.journal
					.append_title(now_ms(), title.clone(), TitleSource::System)
					.into_diagnostic()?;
				session.title = Some(title);
			}
			append_stored(session, StoredMessage { role: InferenceRole::User, parts, replay })?;
			state.active.insert(session_id.clone(), token.clone());
			changed
		};
		if title_changed {
			let title = self
				.state
				.lock()
				.sessions
				.get(&session_id)
				.and_then(|session| session.title.clone());
			self.update(&session_id, json!({"sessionUpdate":"session_info_update","title":title}))?;
		}
		let runtime = Arc::clone(self);
		tokio::spawn(async move {
			let result = runtime.run_prompt(&session_id, token).await;
			runtime.state.lock().active.remove(&session_id);
			match result {
				Ok(reason) => {
					let _ = runtime.respond(request_id, json!({"stopReason":reason}));
				},
				Err(error) => {
					let _ = runtime.error(request_id, -32000, error.to_string());
				},
			}
		});
		Ok(())
	}

	async fn run_prompt(
		&self,
		session_id: &Str,
		cancellation: CancellationToken,
	) -> miette::Result<&'static str> {
		let (model, request) = {
			let state = self.state.lock();
			let session = state
				.sessions
				.get(session_id)
				.ok_or_else(|| miette!("unknown session `{session_id}`"))?;
			(state.model.clone(), request_from(session, &state.thinking))
		};
		let planner =
			omp_llm_inference::router::Router::new(self.registry.clone(), Duration::from_secs(30));
		let meta = CallMeta {
			id:       InferenceRequestId::from(turn_id()),
			target:   Target::Model(ModelKey::from(model)),
			deadline: None,
			budget:   ExecutionBudget::default(),
			session:  None,
		};
		let mut events = Client::new(self.registry.service(), planner, meta)
			.execute(request)
			.await
			.into_diagnostic()?;
		let mut text = String::new();
		let mut completed = None;
		loop {
			tokio::select! { () = cancellation.cancelled() => { self.update(session_id, json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":""},"cancelled":true}))?; return Ok("cancelled"); }, event = events.next() => { let Some(event) = event else { break; }; match event.into_diagnostic()? { ChatEvent::TextDelta { text: delta, .. } => { text.push_str(delta.as_str()); self.update(session_id, json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":delta}}))?; }, ChatEvent::ThinkingDelta { text, .. } => self.update(session_id, json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":text}}))?, ChatEvent::Usage(update) => self.update(session_id, json!({"sessionUpdate":"usage_update","usage":update.usage}))?, ChatEvent::Completed(completion) => completed = Some(completion.reason), _ => {} } } }
		}
		if !text.is_empty() {
			let mut state = self.state.lock();
			let session = state
				.sessions
				.get_mut(session_id)
				.ok_or_else(|| miette!("session closed during prompt"))?;
			append_stored(session, StoredMessage {
				role:   InferenceRole::Assistant,
				parts:  vec![ContentPart::Text { text: Str::from(text.clone()), proof: None }],
				replay: vec![
					json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}),
				],
			})?;
		}
		Ok(completed.as_ref().map_or("max_tokens", map_stop_reason))
	}

	fn push_initial(&self, session: &Str) -> miette::Result<()> {
		let state = self.state.lock();
		let title = state
			.sessions
			.get(session)
			.and_then(|value| value.title.clone());
		self.update(session, json!({"sessionUpdate":"session_info_update","title":title}))?;
		self.update(
			session,
			json!({"sessionUpdate":"usage_update","usage":{"input_tokens":0,"output_tokens":0}}),
		)?;
		self.update(
			session,
			json!({"sessionUpdate":"current_mode_update","currentModeId":state.mode}),
		)?;
		self.update(session, json!({"sessionUpdate":"config_option_update","configOptions":[{"id":"model","name":"Model","type":"select","currentValue":state.model,"options":state.models},{"id":"thinking","name":"Thinking","type":"select","currentValue":state.thinking,"options":["auto","none","low","medium","high"]}]}))?;
		self.update(session, json!({"sessionUpdate":"available_commands_update","availableCommands":[{"name":"skill","description":"Load a skill"},{"name":"new","description":"Start a session"}]}))
	}

	fn update(&self, session: &Str, update: Value) -> miette::Result<()> {
		self.output.send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}})).into_diagnostic()
	}

	fn respond(&self, id: Value, result: Value) -> miette::Result<()> {
		self
			.output
			.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
			.into_diagnostic()
	}

	fn error(&self, id: Value, code: i64, message: impl Into<String>) -> miette::Result<()> {
		self
			.output
			.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.into()}}))
			.into_diagnostic()
	}
}

#[cfg(unix)]
async fn spawn_terminal_auth(provider: &str) -> miette::Result<std::process::ExitStatus> {
	let terminal = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.open("/dev/tty")
		.into_diagnostic()?;
	let input = terminal.try_clone().into_diagnostic()?;
	let output = terminal.try_clone().into_diagnostic()?;
	tokio::process::Command::new(std::env::current_exe().into_diagnostic()?)
		.args(["auth", "login", provider])
		.stdin(Stdio::from(input))
		.stdout(Stdio::from(output))
		.stderr(Stdio::from(terminal))
		.status()
		.await
		.into_diagnostic()
}

#[cfg(not(unix))]
async fn spawn_terminal_auth(_provider: &str) -> miette::Result<std::process::ExitStatus> {
	Err(miette!(
		"terminal authentication is unavailable: this platform has no isolated terminal spawning \
		 backend"
	))
}

fn map_stop_reason(reason: &FinishReason) -> &'static str {
	match reason {
		FinishReason::Stop | FinishReason::ToolCalls | FinishReason::Other(_) => "end_turn",
		FinishReason::Length => "max_tokens",
		FinishReason::ContentFilter => "refusal",
		FinishReason::Cancelled => "cancelled",
	}
}

fn create_journal(state: &State, id: &Str, parent: Option<&Str>) -> miette::Result<Journal> {
	let session_id = SessionId(id.clone());
	let parent_id = parent.cloned().map(SessionId);
	let path = state.sessions_dir.join(format!("{id}.jsonl"));
	let root = state.root.to_string_lossy();
	let created = now_ms();
	let request = NewSession {
		id:         &session_id,
		cwd:        root.as_ref(),
		project:    root.as_ref(),
		created_ms: created,
		kind:       SessionKind::Interactive,
		parent:     parent_id.as_ref(),
		remote:     false,
	};
	let result = state.index.create_session(&request, || {
		let journal = Journal::create(&path, &Header {
			v: 4,
			id: session_id.clone(),
			created,
			cwd: state.root.clone(),
		})?;
		let watermark = journal.byte_watermark()?;
		Ok::<_, omp_agent::JournalError>((journal, watermark))
	});
	let mut journal = match result {
		Ok(journal) => journal,
		Err(IndexedWriteError::Journal(error)) => return Err(miette!(error.to_string())),
		Err(IndexedWriteError::IndexBeforeJournal(error)) => return Err(miette!(error.to_string())),
		Err(IndexedWriteError::IndexAfterJournal { source, .. }) => {
			return Err(miette!(source.to_string()));
		},
	};
	journal.attach_session_index(Arc::clone(&state.index), session_id);
	Ok(journal)
}

fn append_stored(session: &mut AcpSession, message: StoredMessage) -> miette::Result<()> {
	let text = message
		.parts
		.iter()
		.filter_map(|part| match part {
			ContentPart::Text { text, .. } => Some(text.as_str()),
			_ => None,
		})
		.collect::<Vec<_>>()
		.join("\n");
	let role = match message.role {
		InferenceRole::Assistant => Role::Assistant,
		InferenceRole::System => Role::System,
		_ => Role::User,
	};
	let item = Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(item::Kind::Message(Message {
			role:  i32::from(role),
			parts: vec![Part { kind: Some(part::Kind::Text(text)) }],
		})),
		props:         None,
	};
	session
		.journal
		.append_optimistic(now_ms(), item, None)
		.into_diagnostic()?;
	session.messages.push(message);
	Ok(())
}

fn replay_messages(path: &Path) -> miette::Result<Vec<StoredMessage>> {
	let log = omp_storage::transcript::load(path).into_diagnostic()?;
	let live = log.live();
	let mut messages = Vec::new();
	for index in live {
		let Some(omp_storage::transcript::Entry::Ok(event)) = log.get(index) else {
			continue;
		};
		let Kind::Item(record) = &event.kind else {
			continue;
		};
		let Some(item::Kind::Message(message)) = &record.item.kind else {
			continue;
		};
		let role = match Role::try_from(message.role).unwrap_or(Role::Unspecified) {
			Role::Assistant => InferenceRole::Assistant,
			Role::System => InferenceRole::System,
			_ => InferenceRole::User,
		};
		let text = message
			.parts
			.iter()
			.filter_map(|part| match &part.kind {
				Some(part::Kind::Text(text)) => Some(text.as_str()),
				_ => None,
			})
			.collect::<Vec<_>>()
			.join("\n");
		let update = if role == InferenceRole::Assistant {
			"agent_message_chunk"
		} else {
			"user_message_chunk"
		};
		messages.push(StoredMessage {
			role,
			parts: vec![ContentPart::Text { text: Str::from(text.clone()), proof: None }],
			replay: vec![json!({"sessionUpdate":update,"content":{"type":"text","text":text}})],
		});
	}
	Ok(messages)
}

fn convert_blocks(value: &Value) -> miette::Result<(Vec<ContentPart>, Vec<Value>)> {
	let blocks = value
		.as_array()
		.map_or_else(|| vec![value], |values| values.iter().collect());
	let mut parts = Vec::new();
	let mut replay = Vec::new();
	for block in blocks {
		let kind = block.get("type").and_then(Value::as_str).unwrap_or("text");
		match kind {
			"text" => {
				let text = block
					.get("text")
					.and_then(Value::as_str)
					.unwrap_or_default();
				parts.push(ContentPart::Text { text: Str::from(text), proof: None });
				replay.push(
					json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":text}}),
				);
			},
			"image" => {
				let media_type = block
					.get("mimeType")
					.or_else(|| block.get("mediaType"))
					.and_then(Value::as_str)
					.map(Str::from);
				let image = if let Some(data) = block.get("data").and_then(Value::as_str) {
					let data = base64::engine::general_purpose::STANDARD
						.decode(data)
						.map_err(|error| miette!("invalid image base64: {error}"))?;
					MediaInput::Bytes {
						media_type: media_type.unwrap_or_else(|| sf!("image/png")),
						data:       Bytes::from(data),
					}
				} else {
					let uri = block
						.get("uri")
						.or_else(|| block.get("url"))
						.and_then(Value::as_str)
						.ok_or_else(|| miette!("image block requires `data` or `uri`"))?;
					MediaInput::Remote {
						uri: Str::from(uri),
						media_type,
						name: block.get("name").and_then(Value::as_str).map(Str::from),
					}
				};
				parts.push(ContentPart::Image(image));
				replay.push(json!({"sessionUpdate":"user_message_chunk","content":block}));
			},
			"resource" | "resource_link" => {
				let uri = block
					.get("uri")
					.and_then(Value::as_str)
					.unwrap_or("resource");
				let text = block
					.get("text")
					.and_then(Value::as_str)
					.map_or_else(|| format!("[Resource: {uri}]"), str::to_owned);
				parts.push(ContentPart::Text { text: Str::from(text), proof: None });
				replay.push(json!({"sessionUpdate":"user_message_chunk","content":block}));
			},
			"audio" => {
				parts.push(ContentPart::Text {
					text:  sf!("[Audio attachment unavailable in ACP]"),
					proof: None,
				});
				replay.push(json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"[Audio attachment]"}}));
			},
			other => return Err(miette!("unsupported content block `{other}`")),
		}
	}
	Ok((parts, replay))
}
fn request_from(session: &AcpSession, thinking: &str) -> ChatRequest {
	let reasoning = match thinking {
		"none" => Setting::Prefer(ReasoningRequest {
			visibility:          ReasoningVisibility::Hidden,
			effort:              Some(ReasoningEffort::Off),
			max_tokens:          None,
			preserve_signatures: false,
		}),
		"low" => Setting::Prefer(ReasoningRequest {
			visibility:          ReasoningVisibility::Summary,
			effort:              Some(ReasoningEffort::Low),
			max_tokens:          None,
			preserve_signatures: true,
		}),
		"medium" => Setting::Prefer(ReasoningRequest {
			visibility:          ReasoningVisibility::Summary,
			effort:              Some(ReasoningEffort::Medium),
			max_tokens:          None,
			preserve_signatures: true,
		}),
		"high" => Setting::Prefer(ReasoningRequest {
			visibility:          ReasoningVisibility::Summary,
			effort:              Some(ReasoningEffort::High),
			max_tokens:          None,
			preserve_signatures: true,
		}),
		_ => Setting::Unset,
	};
	ChatRequest {
		messages: Arc::from(
			session
				.messages
				.iter()
				.map(|message| InferenceMessage {
					role:    message.role,
					content: Arc::from(message.parts.clone()),
					name:    None,
				})
				.collect::<Vec<_>>(),
		),
		tools: Arc::from([]),
		hosted_tools: Arc::from([]),
		tool_choice: Setting::Unset,
		output: Setting::Unset,
		reasoning,
		verbosity: Setting::Unset,
		cache_retention: Setting::Unset,
		service_tier: Setting::Unset,
		sampling: Sampling::default(),
		max_output_tokens: None,
		top_logprobs: None,
		safety: Arc::from([]),
		negotiation: NegotiationPolicy::default(),
	}
}
fn required_text<'a>(params: &'a Map<String, Value>, name: &str) -> miette::Result<&'a str> {
	params
		.get(name)
		.and_then(Value::as_str)
		.ok_or_else(|| miette!("missing string `{name}`"))
}
fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

/// Operation delegated to an ACP client that owns the remote workspace.
///
/// These constructors are the sole remote filesystem/terminal seam: they
/// produce ordinary ACP requests and never access the local workspace.
#[derive(Clone, Debug, PartialEq)]
pub enum RemoteOperation {
	/// Read a remote UTF-8 file.
	ReadText {
		/// Absolute remote path interpreted by the client.
		path:  Str,
		/// Optional zero-based starting line.
		line:  Option<u64>,
		/// Optional maximum line count.
		limit: Option<u64>,
	},
	/// Write a remote UTF-8 file.
	WriteText {
		/// Absolute remote path interpreted by the client.
		path:    Str,
		/// Complete replacement content.
		content: Str,
	},
	/// Spawn a remote terminal command.
	StartTerminal {
		/// Shell command supplied verbatim to the client.
		command: Str,
		/// Optional remote working directory.
		cwd:     Option<Str>,
	},
	/// Kill a previously spawned remote terminal.
	KillTerminal {
		/// Client-issued terminal identity.
		terminal_id: Str,
	},
}

impl RemoteOperation {
	/// Encodes this operation as a JSON-RPC request for the ACP client.
	#[must_use]
	pub fn request(&self, id: Value, session_id: &str) -> Value {
		let (method, arguments) = match self {
			Self::ReadText { path, line, limit } => (
				"fs/read_text_file",
				json!({"sessionId":session_id,"path":path,"line":line,"limit":limit}),
			),
			Self::WriteText { path, content } => {
				("fs/write_text_file", json!({"sessionId":session_id,"path":path,"content":content}))
			},
			Self::StartTerminal { command, cwd } => (
				"terminal/create",
				json!({"sessionId":session_id,"command":command,"cwd":cwd,"stream":true}),
			),
			Self::KillTerminal { terminal_id } => {
				("terminal/kill", json!({"sessionId":session_id,"terminalId":terminal_id}))
			},
		};
		json!({"jsonrpc":"2.0","id":id,"method":method,"params":arguments})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn converts_all_acp_content_families() {
		let (parts, updates) = convert_blocks(&json!([
			{"type":"text","text":"a"},
			{"type":"image","uri":"x"},
			{"type":"resource_link","uri":"y"},
			{"type":"audio"}
		]))
		.unwrap();
		assert_eq!(parts.len(), 4);
		assert_eq!(updates.len(), 4);
	}

	#[test]
	fn maps_finish_reasons_to_acp_vocabulary() {
		assert_eq!(map_stop_reason(&FinishReason::Stop), "end_turn");
		assert_eq!(map_stop_reason(&FinishReason::Length), "max_tokens");
		assert_eq!(map_stop_reason(&FinishReason::ContentFilter), "refusal");
		assert_eq!(map_stop_reason(&FinishReason::Cancelled), "cancelled");
	}

	#[test]
	fn remote_terminal_requests_stream_without_url_dispatch() {
		let request = RemoteOperation::StartTerminal { command: sf!("pwd"), cwd: None }
			.request(json!(7), "session");
		assert_eq!(request["method"], "terminal/create");
		assert_eq!(request["params"]["stream"], true);
	}
}
