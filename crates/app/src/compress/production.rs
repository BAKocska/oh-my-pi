//! Production compression-only sessions and document-authority adapter.

use std::{path::{Path, PathBuf}, sync::Arc};

use async_stream::stream;
use futures::Stream;
use miette::IntoDiagnostic as _;
use omp_agent::TurnId;
use omp_core::{Str, sf};
use omp_proto::{
	document::v1 as doc_pb,
	thread::v1::{Item, Message, Part as ThreadPart, Role, item, part},
};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, Claims, CommitError, Constraint, Effects, Ev, IncomingParams,
	ParamError, Part, Precedence, Presentation, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{Action, CompressArgs, CompressExit, CompressHost, IsolationPolicy, Loss, Status};

const SYSTEM_PROMPT: &str = include_str!("system_prompt.txt");

/// Failure from production session or document ownership.
#[derive(Debug, thiserror::Error)]
pub enum ProductionError {
	/// Project Environment construction failed.
	#[error(transparent)]
	Environment(#[from] crate::envd::EnvdError),
	/// Canonical project/session composition failed.
	#[error("compression child session failed")]
	Session,
	/// Document authority rejected or failed an operation.
	#[error(transparent)]
	Document(#[from] crate::envd::docs::DocumentError),
	/// Document bytes were not UTF-8.
	#[error("compression source is not UTF-8: {path:?}")]
	Utf8 {
		/// Source document.
		path: PathBuf,
		/// Decoding failure.
		#[source]
		source: std::str::Utf8Error,
	},
	/// Whole-document read returned no content body.
	#[error("document authority omitted whole content for {path:?}")]
	MissingContent {
		/// Source document.
		path: PathBuf,
	},
	/// The atomic document transaction was rejected.
	#[error("document authority rejected the approved compression write")]
	WriteRejected,
	/// The document authority reported a partial single-operation commit.
	#[error("document authority partially committed the approved compression write")]
	WritePartial,
	/// Restricted tool registration failed.
	#[error(transparent)]
	ToolRegistry(#[from] omp_tool::RegistryError),
	/// No configured default model exists.
	#[error("compress requires --model or config.default_model")]
	MissingModel,
}

/// One compression-only production child.
pub struct CompressionSession {
	session: crate::headless::HeadlessSession,
	actions: Arc<Mutex<Vec<Action>>>,
	first:   bool,
}

/// Production owner for document I/O and isolated child sessions.
pub struct ProductionCompressHost {
	root:        PathBuf,
	data_dir:    PathBuf,
	documents:   crate::envd::docs::DocumentHost,
	_environment: crate::envd::ProjectEnvironment,
}

impl ProductionCompressHost {
	/// Starts the project Environment and binds its document authority.
	pub async fn open(root: PathBuf, data_dir: PathBuf) -> Result<Self, ProductionError> {
		let root = crate::chat::canonical_project(&root).map_err(|_| ProductionError::Session)?;
		let settings =
			crate::settings::current(&data_dir).map_err(|_| ProductionError::Session)?;
		let state_dir = crate::project_state::directory(&data_dir, &root)
			.map_err(|_| ProductionError::Session)?;
		crate::chat::ensure_state_directory(&state_dir).map_err(|_| ProductionError::Session)?;
		let environment = crate::envd::ProjectEnvironment::connect_or_start(
			&root,
			&state_dir,
			&crate::project_state::environment_socket(&state_dir),
			&crate::project_state::document_socket(&state_dir),
			false,
			&[],
			settings.runtime_durations().interrupt_grace,
		)
		.await?;
		let documents = environment.documents().clone();
		Ok(Self { root, data_dir, documents, _environment: environment })
	}

	fn resolve_model(&self, requested: Option<&str>) -> Result<Str, ProductionError> {
		if let Some(model) = requested {
			return Ok(Str::new(model));
		}
		crate::settings::current(&self.data_dir)
			.map_err(|_| ProductionError::Session)?
			.default_model
			.map(Str::from)
			.ok_or(ProductionError::MissingModel)
	}
}

impl CompressHost for ProductionCompressHost {
	type Session = CompressionSession;
	type Error = ProductionError;

	fn read_text(
		&self,
		path: &Path,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Str, Self::Error>> + Send {
		let path = path.to_owned();
		async move {
			let uri = omp_sdk::Url::from_file_path(&path)
				.map_err(|()| ProductionError::MissingContent { path: path.clone() })?;
			let lease = self.documents.open(Str::new(uri.as_str()), None, cancel).await?;
			let response = self.documents.read(
				&lease,
				doc_pb::ReadSelection {
					selection: Some(doc_pb::read_selection::Selection::Whole(doc_pb::WholeDocument {})),
				},
				cancel,
			).await?;
			let content = match response.body {
				Some(doc_pb::read_document_response::Body::Content(content)) => content,
				_ => return Err(ProductionError::MissingContent { path }),
			};
			self.documents.close(lease, cancel).await?;
			let text = std::str::from_utf8(&content)
				.map_err(|source| ProductionError::Utf8 { path, source })?;
			Ok(Str::new(text))
		}
	}

	fn open_session(
		&self,
		name: &str,
		model: Option<&str>,
		policy: IsolationPolicy,
		_cancel: &CancellationToken,
	) -> impl Future<Output = Result<Self::Session, Self::Error>> + Send {
		let name = Str::new(name);
		let model = self.resolve_model(model);
		async move {
			debug_assert_eq!(policy, super::ISOLATION_POLICY);
			let actions = Arc::new(Mutex::new(Vec::new()));
			let registry = compression_registry(Arc::clone(&actions))?;
			let session = crate::headless::HeadlessSession::open_with_registry(
				self.data_dir.clone(),
				crate::headless::HeadlessSessionOptions {
					project: self.root.clone(),
					additional_roots: Box::new([]),
					model: model?,
					initial_campaign: None,
					initial_prompt_slot: None,
					resume: None,
					fork: None,
					py_eval: false,
					pty_denied: false,
					credential_provider: None,
					api_key: None,
					prompt_cache_affinity: None,
					session_generation: 1,
				},
				Arc::new(registry),
			)
			.await
			.map_err(|_| ProductionError::Session)?;
			session.set_title(name).await.map_err(|_| ProductionError::Session)?;
			Ok(CompressionSession { session, actions, first: true })
		}
	}

	fn turn<'a>(
		&'a self,
		session: &'a mut Self::Session,
		prompt: Str,
		cancel: &'a CancellationToken,
	) -> impl Future<Output = Result<Vec<Action>, Self::Error>> + Send + 'a {
		async move {
			session.actions.lock().clear();
			let mut items = Vec::with_capacity(usize::from(session.first) + 1);
			if session.first {
				items.push(message(Role::System, SYSTEM_PROMPT));
				session.first = false;
			}
			items.push(message(Role::User, prompt.as_str()));
			let interrupt = session.session.interrupt_handle();
			let result = tokio::select! {
				result = session.session.submit(items, TurnId::new(format!("compress-{}", omp_core::Ulid::generate()))) => result,
				() = cancel.cancelled() => {
					interrupt.interrupt();
					return Ok(Vec::new());
				},
			};
			result.map_err(|_| ProductionError::Session)?;
			Ok(std::mem::take(&mut *session.actions.lock()))
		}
	}

	fn close_session(
		&self,
		mut session: Self::Session,
	) -> impl Future<Output = Result<(), Self::Error>> + Send {
		async move {
			session.session.dispose().await;
			Ok(())
		}
	}

	fn write_approved(
		&self,
		path: &Path,
		text: &str,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<(), Self::Error>> + Send {
		let path = path.to_owned();
		let text = bytes::Bytes::copy_from_slice(text.as_bytes());
		async move {
			let uri = omp_sdk::Url::from_file_path(&path)
				.map_err(|()| ProductionError::MissingContent { path: path.clone() })?;
			let mut lease = self.documents.open(Str::new(uri.as_str()), None, cancel).await?;
			let result = self.documents.commit(
				&mut lease,
				bytes::Bytes::copy_from_slice(omp_core::Ulid::generate().to_string().as_bytes()),
				doc_pb::TextMutation {
					base_revision: None,
					change: Some(doc_pb::text_mutation::Change::ProposedContent(text)),
					stale_policy: doc_pb::StalePolicy::Fail as i32,
					format_policy: doc_pb::FormatPolicy::Disabled as i32,
				},
				cancel,
			).await?;
			self.documents.close(lease, cancel).await?;
			match result.outcome {
				Some(doc_pb::commit_transaction_response::Outcome::Committed(_)) => Ok(()),
				Some(doc_pb::commit_transaction_response::Outcome::Rejected(_)) => Err(ProductionError::WriteRejected),
				Some(doc_pb::commit_transaction_response::Outcome::PartiallyCommitted(_)) => Err(ProductionError::WritePartial),
				None => Err(ProductionError::WriteRejected),
			}
		}
	}

	fn progress(&self, completed: usize, total: usize, path: &Path, status: Status) {
		eprintln!("[{completed}/{total}] {}: {status:?}", path.display());
	}
}

/// Runs the production standalone compression command.
pub async fn run_command(args: CompressArgs) -> miette::Result<()> {
	let invocation_dir = std::env::current_dir().into_diagnostic()?;
	let host = ProductionCompressHost::open(invocation_dir.clone(), crate::cli::data_dir(None)?)
		.await
		.map_err(|error| miette::miette!(error))?;
	let result: CompressExit = super::run(&args, &invocation_dir, &host, &CancellationToken::new())
		.await
		.map_err(|error| miette::miette!(error))?;
	let stdout = result.files.len() == 1 && !args.in_place && args.out.is_none();
	for file in &result.files {
		if stdout && file.status == Status::Approved {
			if let Some(draft) = &file.draft {
				println!("{}", draft.text);
			}
		} else {
			eprintln!("{}: {:?}, {} draft(s)", file.path.display(), file.status, file.rounds);
		}
	}
	if result.code == 0 { Ok(()) } else { Err(miette::miette!("compress did not approve every target")) }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct RewriteParams {
	#[schemars(with = "String")]
	text: Str,
	losses: Vec<LossParams>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct LossParams {
	#[schemars(with = "String")]
	content: Str,
	#[schemars(with = "String")]
	reason: Str,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ApproveParams {
	#[schemars(with = "String")]
	verdict: Str,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Ack {
	accepted: bool,
}

#[derive(Debug, Deserialize, Serialize, thiserror::Error)]
enum ToolFault {
	#[error("compression protocol action could not be recorded")]
	Unavailable,
}

struct RewriteTool {
	spec: ToolSpec,
	actions: Arc<Mutex<Vec<Action>>>,
}

impl RewriteTool {
	fn new(actions: Arc<Mutex<Vec<Action>>>) -> Self {
		Self { spec: tool_spec::<RewriteParams>("rewrite", "Submit a complete replacement and every deliberate loss."), actions }
	}
}

impl Tool for RewriteTool {
	type Fault = ToolFault;
	type Params = RewriteParams;
	type Payload = Ack;
	type Update = Ack;

	fn spec(&self) -> &ToolSpec { &self.spec }

	fn call<'c>(&'c self, mut incoming: IncomingParams<'c>) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<RewriteParams>().await {
				Ok(params) => params,
				Err(error) => { yield param_event(error); return; },
			};
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			self.actions.lock().push(Action::Rewrite {
				text: params.text,
				losses: params.losses.into_iter().map(|loss| Loss { content: loss.content, reason: loss.reason }).collect(),
			});
			yield Ev::Done(ToolTerminal::Done { result: Ok(Ack { accepted: true }), useless: false });
		}
	}

	fn prompt(&self, view: Result<&Ack, &ToolFault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text { text: match view { Ok(_) => sf!("draft recorded; await review"), Err(_) => sf!("draft rejected") } }]
	}
}

struct ApproveTool {
	spec: ToolSpec,
	actions: Arc<Mutex<Vec<Action>>>,
}

impl ApproveTool {
	fn new(actions: Arc<Mutex<Vec<Action>>>) -> Self {
		Self { spec: tool_spec::<ApproveParams>("approve", "Approve the newest draft after a separate review turn."), actions }
	}
}

impl Tool for ApproveTool {
	type Fault = ToolFault;
	type Params = ApproveParams;
	type Payload = Ack;
	type Update = Ack;

	fn spec(&self) -> &ToolSpec { &self.spec }

	fn call<'c>(&'c self, mut incoming: IncomingParams<'c>) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<ApproveParams>().await {
				Ok(params) => params,
				Err(error) => { yield param_event(error); return; },
			};
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			self.actions.lock().push(Action::Approve { verdict: params.verdict });
			yield Ev::Done(ToolTerminal::Done { result: Ok(Ack { accepted: true }), useless: false });
		}
	}

	fn prompt(&self, view: Result<&Ack, &ToolFault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text { text: match view { Ok(_) => sf!("approval recorded"), Err(_) => sf!("approval rejected") } }]
	}
}

fn tool_spec<P: JsonSchema>(name: &'static str, description: &'static str) -> ToolSpec {
	ToolSpec {
		name: sf!(name),
		rev: Rev { family: sf!("native"), n: 1 },
		description: sf!(description),
		schema: omp_tool::schema::<P>(),
		constraint: Constraint::Schema { priority: 100, on_unsupported: omp_tool::Fallback::Error },
		effects: Effects::empty(),
		projection_code: omp_tool::native_projection_code(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"), include_bytes!("production.rs")).into_bytes(),
	}
}
fn compression_registry(
	actions: Arc<Mutex<Vec<Action>>>,
) -> Result<omp_tool::Registry, omp_tool::RegistryError> {
	let mut registry = omp_tool::Registry::new();
	let claims = Claims {
		precedence: Precedence::CORE,
		claimant: sf!("omp/compress"),
		replaces: None,
	};
	registry.register(
		RewriteTool::new(Arc::clone(&actions)),
		Presentation::Slot,
		claims.clone(),
	)?;
	registry.register(ApproveTool::new(actions), Presentation::Slot, claims)?;
	Ok(registry)
}

fn param_event(error: ParamError) -> Ev<Ack, Ack, ToolFault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn commit_event(error: CommitError) -> Ev<Ack, Ack, ToolFault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path: Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind: ArgIssueKind::Protocol,
		example: None,
		found: Some(message),
	}
}

fn message(role: Role, text: &str) -> Item {
	Item {
		kind: Some(item::Kind::Message(Message {
			role: role as i32,
			parts: vec![ThreadPart { kind: Some(part::Kind::Text(text.to_owned())), ..ThreadPart::default() }],
			..Message::default()
		})),
		..Item::default()
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn production_registry_advertises_only_protocol_tools() {
		fn assert_host<H: CompressHost>() {}
		assert_host::<ProductionCompressHost>();
		let registry =
			compression_registry(Arc::new(Mutex::new(Vec::new()))).expect("restricted registry");
		let projection = registry.prompt_projection(None);
		let names = projection
			.entries()
			.map(|entry| entry.name.as_str())
			.collect::<Vec<_>>();
		assert_eq!(names, ["approve", "rewrite"]);
		assert_eq!(super::super::ISOLATION_POLICY.tools, ["rewrite", "approve"]);
	}
}
