//! Single-shot, stdout-safe inference mode.

use std::{
	collections::BTreeMap,
	env, fs, io,
	io::IsTerminal as _,
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use miette::{IntoDiagnostic as _, miette};
use omp_agent::{
	AgentEvent, AgentRunSummary, EventSubscription, InProcTurnClient, PlanState, RunSettlement,
};
use omp_catalog::snapshot;
use omp_core::{Hash32, Str};
use omp_driver::{
	discovery::roles,
	headless::{
		HeadlessSession, HeadlessSessionOptions,
		finalize::{FinalizerBudget, FinalizerReport},
	},
	plan::ModelSelection,
};
use omp_envd::exthost::lifecycle::{HeadlessLifecycleKind, HeadlessLifecycleSubscription};
use omp_inference::call::{ContentPart, MediaInput};
use omp_proto::{
	inference::v1::{part_start, turn_event},
	thread::v1::{Blob, Item, Message, Part, Role, blob, item, part},
};
use omp_tools::read::dirtree;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, Stderr, Stdout, stderr, stdin, stdout};

use crate::{
	chat_cmd::AppAdvisorRuntime,
	cli::{PrintArgs, turn_id},
	image_attachment,
	image_attachment::ImageAttachmentError,
	spec,
	usage_error::CliUsageError,
};

const MAX_TOTAL_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
const MAX_AUTO_READ_TEXT_BYTES: usize = 5 * 1024 * 1024;
const MAX_AUTO_READ_IMAGE_BYTES: usize = 25 * 1024 * 1024;
const DIRECTORY_MENTION_LIMIT: usize = 500;

#[derive(Debug, thiserror::Error)]
enum PrintTurnError {
	#[error(transparent)]
	Session(#[from] omp_sdk::SessionHandleError),
	#[error(transparent)]
	Stdout(#[from] io::Error),
	#[error(transparent)]
	Json(#[from] serde_json::Error),
}

/// Runs prompts through the durable headless agent loop.
pub async fn run(args: PrintArgs) -> miette::Result<()> {
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let settings =
		omp_driver::settings::current_with_overlays(&data_dir, &args.config).into_diagnostic()?;
	let catalog = snapshot::Catalog::try_embedded().map_err(|error| miette!(error))?;
	let roles = roles::resolve_launch_roles(
		catalog,
		args.model.as_deref(),
		args.smol.as_deref(),
		args.slow.as_deref(),
		args.plan.as_deref(),
	)
	.map_err(|error| miette!(error))?;
	for selector in args
		.models
		.as_ref()
		.into_iter()
		.flat_map(|selectors| selectors.0.iter())
	{
		omp_catalog::select_model(
			catalog.models(),
			catalog.routes(),
			catalog.aliases(),
			&[],
			&Default::default(),
			selector,
		)
		.map_err(|error| miette!(error))?;
	}
	for root in &args.add_dir {
		fs::canonicalize(root).into_diagnostic()?;
	}
	let model = roles
		.primary
		.map(|model| Str::from(model.as_str()))
		.or_else(|| args.model.clone())
		.or_else(|| settings.default_model.clone().map(Str::from))
		.ok_or_else(|| miette!("print mode requires --model or config.default_model"))?;
	if args.api_key.is_some() && args.model.is_none() && args.models.is_none() {
		return Err(miette!("--api-key requires a model to be specified via --model or --models"));
	}
	let model = omp_driver::chat::resolve_model_selector(catalog, model.as_str())
		.map_err(|error| miette!(error))?;
	let plan_handoff = if args.plan_yolo {
		match args.plan_yolo_into.as_deref() {
			Some(selector) => {
				let selected =
					roles::resolve_role_selector(catalog, selector).map_err(|error| miette!(error))?;
				Some(
					ModelSelection::resolved(selected.model.as_str(), selected.thinking.as_deref())
						.map_err(|error| miette!(error))?,
				)
			},
			None => roles
				.smol
				.as_ref()
				.map(|model| ModelSelection::resolved(model.as_str(), None))
				.transpose()
				.map_err(|error| miette!(error))?,
		}
	} else {
		None
	};
	let credential_provider = args
		.api_key
		.as_ref()
		.map(|_| omp_driver::chat::resolve_model_provider(catalog, model.as_str(), None))
		.transpose()
		.map_err(|error| miette!(error))?;
	let initial = initial_parts(&args.prompt, settings.images.auto_resize).await?;
	if initial.is_empty() {
		return Err(
			CliUsageError::new("print mode requires a prompt or piped standard input").into(),
		);
	}
	let cwd = env::current_dir().into_diagnostic()?;
	let home = env::var_os("HOME").map_or_else(|| cwd.clone(), PathBuf::from);
	let system = spec::resolve_prompt_slots(
		&cwd,
		&home,
		args.prompt_settings.custom_prompt.as_deref(),
		args.prompt_settings.append_prompt.as_deref(),
	)?
	.combined();
	let mut session = HeadlessSession::open(data_dir, HeadlessSessionOptions {
		project: env::current_dir().into_diagnostic()?,
		additional_roots: args.add_dir.clone().into_boxed_slice(),
		model,
		initial_campaign: args.plan_yolo.then_some("plan"),
		initial_prompt_slot: args.plan_yolo.then_some("plan-yolo"),
		plan_handoff,
		resume: None,
		fork: None,
		py_eval: false,
		approval_mode: args.effective_approval().map(Into::into),
		pty_denied: args.no_pty,
		credential_provider,
		api_key: args.api_key.clone(),
		prompt_cache_affinity: args.prompt_cache_key.clone(),
		session_generation: 1,
	})
	.await
	.into_diagnostic()?;
	let advisor_runtime = if args.advisor {
		let (runtime, _notices) = AppAdvisorRuntime::compose(
			session.advisor_parent(),
			None,
			cwd.clone(),
			Str::new(session.session_id()),
			true,
			session.available_tool_names(),
			session.advise_queue(),
			catalog,
			true,
		);
		Some(Arc::new(runtime))
	} else {
		None
	};
	let fresh = session.initial_items().is_empty();
	let startup_plan_ignored = startup_plan_ignored(&settings, fresh, args.plan_yolo);
	let mut stderr = stderr();
	if startup_plan_ignored {
		stderr
			.write_all(
				b"Note: plan.defaultOnStartup is ignored in print mode (no interactive surface to \
				 review the plan). Use --plan-yolo for a headless plan flow.\n",
			)
			.await
			.into_diagnostic()?;
	}
	if args.plan_yolo {
		session.publish(AgentEvent::PlanStateChanged {
			from:               PlanState::Inactive,
			to:                 PlanState::Yolo,
			session_generation: 1,
		});
	}
	session
		.finalizer_mut()
		.set_telemetry(|| async { omp_telemetry::export::shutdown() });
	if let Some(runtime) = advisor_runtime.as_ref() {
		let runtime = Arc::clone(runtime);
		session
			.finalizer_mut()
			.set_advisor(move || async move { runtime.drain().await });
	}
	let events = session
		.take_events()
		.expect("headless print owns the lossless event subscription");
	let lifecycle_events = session
		.take_lifecycle_events()
		.expect("headless print owns the extension event subscription");
	let json = args.mode == "json";
	let mut stdout = stdout();
	if json {
		write_json(
			&mut stdout,
			&format!(
				"{{\"type\":\"session_start\",\"session_id\":{}}}\n",
				json_string(session.session_id())
			),
		)
		.await?;
	} else {
		stderr.write_all(b"Working...\n").await.into_diagnostic()?;
	}

	let mut part_kinds = BTreeMap::new();
	let mut summary = submit_print_turn(
		&mut session,
		&events,
		&lifecycle_events,
		advisor_runtime.as_deref(),
		initial_message(initial, system),
		&mut part_kinds,
		json,
		args.shape_transcript,
		&mut stdout,
		&mut stderr,
	)
	.await;
	if let Ok(current) = &summary {
		emit_warning(current, &mut stderr).await?;
	}
	for follow_up in &args.follow_ups {
		if summary.is_err() {
			break;
		}
		summary = submit_print_turn(
			&mut session,
			&events,
			&lifecycle_events,
			advisor_runtime.as_deref(),
			vec![message(Role::User, vec![Part {
				kind: Some(part::Kind::Text(follow_up.to_string())),
			}])],
			&mut part_kinds,
			json,
			args.shape_transcript,
			&mut stdout,
			&mut stderr,
		)
		.await;
		if let Ok(current) = &summary {
			emit_warning(current, &mut stderr).await?;
		}
	}

	let summary = match summary {
		Ok(summary) => summary,
		Err(error) => {
			let report = session
				.finalize(&mut stdout, FinalizerBudget::terminal_error())
				.await;
			emit_finalizer_report(report, &mut stderr).await?;
			return Err(error).into_diagnostic();
		},
	};
	match summary.settlement {
		RunSettlement::Success | RunSettlement::Warning => {},
		RunSettlement::SilentCompactionTransition => {
			let report = session
				.finalize(&mut stdout, FinalizerBudget::success(Duration::from_secs(30)))
				.await;
			emit_finalizer_report(report, &mut stderr).await?;
			return Ok(());
		},
		RunSettlement::CallerAbort | RunSettlement::MaxTokens | RunSettlement::TerminalFault => {
			let report = session
				.finalize(&mut stdout, FinalizerBudget::terminal_error())
				.await;
			emit_finalizer_report(report, &mut stderr).await?;
			return Err(miette!("headless turn settled as {}", <&str>::from(summary.settlement)));
		},
	}

	if !json {
		if args.print_thoughts {
			if let Some(thinking) = final_thinking(&summary) {
				stdout
					.write_all(sanitize(thinking.as_str()).as_bytes())
					.await
					.into_diagnostic()?;
				stdout.write_all(b"\n").await.into_diagnostic()?;
			}
		}
		if let Some(text) = summary.final_assistant() {
			stdout
				.write_all(sanitize(text).as_bytes())
				.await
				.into_diagnostic()?;
			stdout.write_all(b"\n").await.into_diagnostic()?;
		}
	}
	let report = session
		.finalize(&mut stdout, FinalizerBudget::success(Duration::from_secs(30)))
		.await;
	emit_finalizer_report(report, &mut stderr).await
}

fn startup_plan_ignored(
	settings: &omp_driver::settings::Settings,
	fresh: bool,
	plan_yolo: bool,
) -> bool {
	settings.plan.enabled && settings.plan.default_on_startup && fresh && !plan_yolo
}

async fn submit_print_turn(
	session: &mut HeadlessSession,
	events: &EventSubscription,
	lifecycle_events: &HeadlessLifecycleSubscription,
	advisor: Option<&AppAdvisorRuntime<InProcTurnClient>>,
	items: Vec<Item>,
	part_kinds: &mut BTreeMap<u32, part_start::Kind>,
	json: bool,
	shape_transcript: bool,
	stdout: &mut Stdout,
	stderr: &mut Stderr,
) -> Result<AgentRunSummary, PrintTurnError> {
	let submit = session.submit(items, omp_agent::TurnId::new(turn_id()));
	tokio::pin!(submit);
	let result = loop {
		tokio::select! {
			result = &mut submit => break result,
			event = events.recv() => {
				let Ok(event) = event else { continue; };
				if let Some(advisor) = advisor {
					advisor.observe(event.as_ref()).await;
				}
				emit_event(&event, part_kinds, json, shape_transcript, stdout, stderr).await?;
			},
			event = lifecycle_events.recv() => {
				let Ok(event) = event else { continue; };
				emit_lifecycle(&event.kind, stderr).await;
			},
		}
	};
	while let Ok(event) = events.try_recv() {
		if let Some(advisor) = advisor {
			advisor.observe(event.as_ref()).await;
		}
		emit_event(&event, part_kinds, json, shape_transcript, stdout, stderr).await?;
	}
	Ok(result?)
}

async fn emit_lifecycle(kind: &HeadlessLifecycleKind, stderr: &mut Stderr) {
	if let HeadlessLifecycleKind::ExtensionError { extension, error } = kind {
		let _ = stderr
			.write_all(
				format!("Extension error ({}): {error}\n", sanitize(extension.as_str())).as_bytes(),
			)
			.await;
	}
}

async fn emit_event(
	event: &AgentEvent,
	part_kinds: &mut BTreeMap<u32, part_start::Kind>,
	json: bool,
	shape_transcript: bool,
	stdout: &mut Stdout,
	stderr: &mut Stderr,
) -> Result<(), PrintTurnError> {
	if let AgentEvent::Failed { message, .. } = event {
		let _ = stderr
			.write_all(format!("{}\n", sanitize(message.as_str())).as_bytes())
			.await;
	}
	if !json {
		return Ok(());
	}
	let line = match event {
		AgentEvent::Turn { turn_id, event } => match event.event.as_ref() {
			Some(turn_event::Event::PartStart(start)) => {
				if let Ok(kind) = part_start::Kind::try_from(start.kind) {
					part_kinds.insert(start.index, kind);
				}
				serde_json::json!({"type":"part_start","turn_id":turn_id.as_str(),"index":start.index,"kind":start.kind})
			},
			Some(turn_event::Event::PartDelta(delta)) => {
				let kind = part_kinds.get(&delta.index).copied();
				let text = String::from_utf8_lossy(&delta.chunk);
				serde_json::json!({
					"type": match kind {
						Some(part_start::Kind::Text) => "text_delta",
						Some(part_start::Kind::Thinking) => "thinking_delta",
						Some(part_start::Kind::ToolCall) => "tool_args_delta",
						_ => "part_delta",
					},
					"turn_id":turn_id.as_str(),
					"index":delta.index,
					"text":sanitize(&text),
				})
			},
			Some(turn_event::Event::PartEnd(end)) => {
				part_kinds.remove(&end.index);
				serde_json::json!({"type":"part_end","turn_id":turn_id.as_str(),"index":end.index})
			},
			Some(turn_event::Event::Outcome(outcome)) if shape_transcript => {
				serde_json::json!({"type":"outcome","turn_id":turn_id.as_str(),"stop":outcome.stop})
			},
			Some(turn_event::Event::Outcome(outcome)) => {
				serde_json::json!({"type":"outcome","turn_id":turn_id.as_str(),"stop":outcome.stop,"model":outcome.model,"provider":outcome.provider})
			},
			Some(_) => serde_json::json!({"type":"turn_event","turn_id":turn_id.as_str()}),
			None => serde_json::json!({"type":"turn_event","turn_id":turn_id.as_str(),"empty":true}),
		},
		AgentEvent::ToolObserved {
			call_id,
			identity,
			path,
			visibility,
			provenance,
			session_generation,
		} => serde_json::json!({
			"type":"tool_observed",
			"call_id":call_id.as_str(),
			"name":identity.name.as_str(),
			"rev":identity.rev.to_string(),
			"path":path.as_ref().map(|path| path.as_str()),
			"visibility":<&str>::from(*visibility),
			"provenance":<&str>::from(*provenance),
			"session_generation":session_generation,
		}),
		AgentEvent::PlanStateChanged { from, to, session_generation } => serde_json::json!({
			"type":"plan_state_changed",
			"from":<&str>::from(*from),
			"to":<&str>::from(*to),
			"session_generation":session_generation,
		}),
		AgentEvent::ToolOpened { call_id, name, rev } => {
			serde_json::json!({"type":"tool_opened","call_id":call_id.as_str(),"name":name.as_str(),"rev":rev.to_string()})
		},
		AgentEvent::ToolArgs { call_id, fragment, .. } => {
			serde_json::json!({"type":"tool_args","call_id":call_id.as_str(),"fragment":String::from_utf8_lossy(fragment)})
		},
		AgentEvent::ToolUpdate { call_id, json } => {
			serde_json::json!({"type":"tool_update","call_id":call_id.as_str(),"json":String::from_utf8_lossy(json)})
		},
		AgentEvent::ToolFinished { call_id, .. } => {
			serde_json::json!({"type":"tool_finished","call_id":call_id.as_str()})
		},
		AgentEvent::PhaseChanged { from, to } => {
			serde_json::json!({"type":"phase_changed","from":format!("{from:?}"),"to":format!("{to:?}")})
		},
		AgentEvent::RosterChanged { generation } => {
			serde_json::json!({"type":"roster_changed","generation":generation})
		},
		AgentEvent::JobRegistered { job_id } => {
			serde_json::json!({"type":"job_registered","job_id":job_id.as_str()})
		},
		AgentEvent::JobSettled { job_id } => {
			serde_json::json!({"type":"job_settled","job_id":job_id.as_str()})
		},
		AgentEvent::Failed { turn_id, message } => {
			serde_json::json!({"type":"failed","turn_id":turn_id.as_ref().map(|id| id.as_str()),"message":sanitize(message.as_str())})
		},
		AgentEvent::TitleChanged { title, source } => {
			serde_json::json!({"type":"title_changed","title":title.as_str(),"source":format!("{source:?}")})
		},
		AgentEvent::RunStateChanged { from, to } => {
			serde_json::json!({"type":"run_state_changed","from":<&str>::from(*from),"to":<&str>::from(*to)})
		},
		AgentEvent::Snapshot(_) => serde_json::json!({"type":"snapshot"}),
		AgentEvent::PeerRelay(_) => return Ok(()),
	};
	let mut encoded = serde_json::to_string(&line)?;
	encoded.push('\n');
	stdout.write_all(encoded.as_bytes()).await?;
	Ok(())
}

async fn emit_warning(summary: &AgentRunSummary, stderr: &mut Stderr) -> miette::Result<()> {
	if summary.settlement != RunSettlement::Warning {
		return Ok(());
	}
	if let Some(outcome) = &summary.outcome {
		for diagnostic in &outcome.diagnostics {
			stderr
				.write_all(format!("Warning: {}\n", sanitize(&diagnostic.detail)).as_bytes())
				.await
				.into_diagnostic()?;
		}
		for unsupported in &outcome.unsupported {
			stderr
				.write_all(
					format!(
						"Warning: {}: {}\n",
						sanitize(&unsupported.what),
						sanitize(&unsupported.detail)
					)
					.as_bytes(),
				)
				.await
				.into_diagnostic()?;
		}
	}
	Ok(())
}

async fn emit_finalizer_report(report: FinalizerReport, stderr: &mut Stderr) -> miette::Result<()> {
	for phase in &report.timed_out {
		stderr
			.write_all(format!("Finalizer timed out: {}\n", <&str>::from(*phase)).as_bytes())
			.await
			.into_diagnostic()?;
	}
	if let Some(error) = report.stdout_error {
		return Err(error).into_diagnostic();
	}
	Ok(())
}

fn initial_message(parts: Vec<ContentPart>, system: Option<Str>) -> Vec<Item> {
	let mut items = Vec::with_capacity(usize::from(system.is_some()) + 1);
	if let Some(system) = system {
		items.push(message(Role::System, vec![Part {
			kind: Some(part::Kind::Text(system.to_string())),
		}]));
	}
	let mut canonical = Vec::with_capacity(parts.len());
	for part in parts {
		match part {
			ContentPart::Text { text, .. } => {
				canonical.push(Part { kind: Some(part::Kind::Text(text.to_string())) })
			},
			ContentPart::Image(media) | ContentPart::Document(media) => {
				if let MediaInput::Bytes { media_type, data } = media {
					canonical.push(Part {
						kind: Some(part::Kind::Blob(Blob {
							hash:   Bytes::copy_from_slice(Hash32::sum(&data).as_bytes()),
							mime:   media_type.to_string(),
							size:   data.len() as u64,
							inline: data,
							detail: blob::Detail::Auto as i32,
						})),
					});
				}
			},
			_ => {},
		}
	}
	items.push(message(Role::User, canonical));
	items
}

fn message(role: Role, parts: Vec<Part>) -> Item {
	Item { kind: Some(item::Kind::Message(Message { role: role as i32, parts })), ..Item::default() }
}

fn final_thinking(summary: &AgentRunSummary) -> Option<Str> {
	let outcome = summary.outcome.as_ref()?;
	let message = outcome.output.iter().rev().find_map(|item| {
		let Some(item::Kind::Message(message)) = item.kind.as_ref() else {
			return None;
		};
		(message.role() == Role::Assistant).then_some(message)
	})?;
	let mut text = String::new();
	for part in &message.parts {
		if let Some(part::Kind::Thinking(thinking)) = part.kind.as_ref()
			&& !thinking.text.trim().is_empty()
		{
			text.push_str(&thinking.text);
		}
	}
	(!text.is_empty()).then(|| Str::from(text))
}

async fn initial_parts(
	words: &[Str],
	auto_resize_images: bool,
) -> miette::Result<Vec<ContentPart>> {
	let mut parts = Vec::new();
	let mut text = String::new();
	let mut consumed = 0usize;
	for word in words {
		if let Some(path) = word.strip_prefix("@") {
			let attachment =
				read_reference(Path::new(path.as_str()), &mut consumed, auto_resize_images)?;
			match attachment {
				Attachment::Text(contents) => append_text(&mut text, &contents),
				Attachment::Image { media_type, data } => {
					parts.push(ContentPart::Image(MediaInput::Bytes { media_type, data }));
				},
				Attachment::Document { media_type, data } => {
					parts.push(ContentPart::Document(MediaInput::Bytes { media_type, data }));
				},
			}
		} else {
			append_text(&mut text, word);
		}
	}
	if text.is_empty() && !io::stdin().is_terminal() {
		let mut stdin = stdin();
		stdin.read_to_string(&mut text).await.into_diagnostic()?;
	}
	if !text.is_empty() {
		parts.insert(0, ContentPart::Text { text: text.into(), proof: None });
	}
	Ok(parts)
}

fn append_text(target: &mut String, value: &str) {
	if !target.is_empty() {
		target.push(' ');
	}
	target.push_str(value);
}

enum Attachment {
	Text(String),
	Image { media_type: Str, data: Bytes },
	Document { media_type: Str, data: Bytes },
}

fn read_reference(
	path: &Path,
	consumed: &mut usize,
	auto_resize_images: bool,
) -> miette::Result<Attachment> {
	let metadata = fs::metadata(path).into_diagnostic()?;
	if metadata.is_dir() {
		return read_directory_reference(path);
	}
	let bytes = fs::read(path).into_diagnostic()?;
	*consumed = consumed
		.checked_add(bytes.len())
		.ok_or_else(|| miette!("attachment budget overflow"))?;
	if *consumed > MAX_TOTAL_ATTACHMENT_BYTES {
		return Ok(skip_notice(path, "total attachment budget exceeded", bytes.len()));
	}
	if let Some(media_type) = image_media_type(&bytes) {
		if bytes.len() > MAX_AUTO_READ_IMAGE_BYTES {
			return Ok(skip_notice(path, "too large", bytes.len()));
		}
		if !auto_resize_images {
			return Ok(Attachment::Image {
				media_type: Str::new_static(media_type),
				data:       Bytes::from(bytes),
			});
		}
		return match image_attachment::prepare(Bytes::from(bytes), true) {
			Ok(image) => Ok(Attachment::Image {
				media_type: Str::new_static(image.media_type),
				data:       image.bytes,
			}),
			Err(ImageAttachmentError::Unsupported) => {
				Ok(skip_notice(path, "unrecognized image encoding", metadata.len() as usize))
			},
			Err(_) => Ok(skip_notice(path, "too large", metadata.len() as usize)),
		};
	}
	if let Some(media_type) = document_media_type(path, &bytes) {
		return Ok(Attachment::Document {
			media_type: media_type.into(),
			data:       Bytes::from(bytes),
		});
	}
	if bytes.len() > MAX_AUTO_READ_TEXT_BYTES {
		return Ok(skip_notice(path, "too large", bytes.len()));
	}
	if omp_tools::read::is_probably_binary_header(
		&bytes[..bytes.len().min(omp_tools::read::BINARY_SNIFF_BYTES)],
	) {
		return Ok(skip_notice(path, "binary file", bytes.len()));
	}
	let content = match String::from_utf8(bytes) {
		Ok(content) => content,
		Err(error) => return Ok(skip_notice(path, "binary file", error.as_bytes().len())),
	};
	let tag = omp_hashline::compute_file_hash(&content);
	let header = omp_hashline::format_hashline_header(&path.to_string_lossy(), tag.as_str());
	let numbered = omp_hashline::format_numbered_lines(&content, 1);
	Ok(Attachment::Text(format!(
		"<file name=\"{}\">\n{header}\n{numbered}\n</file>",
		path.display()
	)))
}

fn read_directory_reference(path: &Path) -> miette::Result<Attachment> {
	let mut entries = Vec::new();
	for entry in fs::read_dir(path).into_diagnostic()? {
		let entry = entry.into_diagnostic()?;
		let metadata = match entry.metadata() {
			Ok(metadata) => metadata,
			Err(_) => continue,
		};
		let modified_ms = metadata
			.modified()
			.ok()
			.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
			.and_then(|duration| u64::try_from(duration.as_millis()).ok())
			.unwrap_or(0);
		entries.push(dirtree::DirEntry {
			relative_path: entry.file_name().to_string_lossy().into_owned().into(),
			is_dir: metadata.is_dir(),
			size: metadata.len(),
			modified_ms,
		});
	}
	let now_ms = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.ok()
		.and_then(|duration| u64::try_from(duration.as_millis()).ok())
		.unwrap_or(0);
	let listing = dirtree::render_directory_mention(&entries, now_ms, DIRECTORY_MENTION_LIMIT);
	Ok(Attachment::Text(format!("<directory name=\"{}\">\n{listing}\n</directory>", path.display())))
}

fn skip_notice(path: &Path, reason: &str, bytes: usize) -> Attachment {
	Attachment::Text(format!(
		"<file name=\"{}\">(skipped auto-read: {reason}, {} bytes)</file>",
		path.display(),
		bytes
	))
}

fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
	if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
		Some("image/png")
	} else if bytes.starts_with(b"\xff\xd8\xff") {
		Some("image/jpeg")
	} else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
		Some("image/gif")
	} else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
		Some("image/webp")
	} else {
		None
	}
}

fn document_media_type(path: &Path, bytes: &[u8]) -> Option<&'static str> {
	if bytes.starts_with(b"%PDF-") {
		return Some("application/pdf");
	}
	if bytes.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
		return Some("application/vnd.ms-office");
	}
	match path
		.extension()
		.and_then(|extension| extension.to_str())
		.map(str::to_ascii_lowercase)
		.as_deref()
	{
		Some("docx") => {
			Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
		},
		Some("pptx") => {
			Some("application/vnd.openxmlformats-officedocument.presentationml.presentation")
		},
		Some("xlsx") => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
		Some("ipynb") => Some("application/x-ipynb+json"),
		Some("html" | "htm") => Some("text/html"),
		_ => None,
	}
}

async fn write_json(stdout: &mut Stdout, line: &str) -> miette::Result<()> {
	stdout.write_all(line.as_bytes()).await.into_diagnostic()?;
	stdout.flush().await.into_diagnostic()
}

fn sanitize(text: &str) -> String {
	text.replace('\0', "")
}
fn json_string(text: &str) -> String {
	format!("{:?}", sanitize(text))
}

#[cfg(test)]
mod tests {

	use omp_driver::settings::Settings;

	use super::*;
	#[test]
	fn print_suppresses_only_fresh_startup_plan_without_yolo() {
		let mut settings = Settings::default();
		settings.plan.enabled = true;
		settings.plan.default_on_startup = true;
		assert!(startup_plan_ignored(&settings, true, false));
		assert!(!startup_plan_ignored(&settings, false, false));
		assert!(!startup_plan_ignored(&settings, true, true));
	}

	#[test]
	fn classifies_text_documents_and_images_by_content() {
		assert_eq!(image_media_type(b"\x89PNG\r\n\x1a\nmore"), Some("image/png"));
		assert_eq!(
			document_media_type(Path::new("report.pdf"), b"%PDF-1.7"),
			Some("application/pdf")
		);
		assert!(document_media_type(Path::new("sheet.xlsx"), b"PK\x03\x04").is_some());
	}
	#[test]
	fn attachment_budget_returns_an_explicit_skip_notice() {
		let file = env::temp_dir().join("omp-print-large-reference.txt");
		fs::write(&file, vec![b'x'; MAX_AUTO_READ_TEXT_BYTES + 1]).expect("write");
		let Attachment::Text(notice) = read_reference(&file, &mut 0, true).expect("notice") else {
			panic!("text notice");
		};
		assert!(notice.contains("skipped auto-read: too large"));
		let _ = fs::remove_file(file);
	}

	#[test]
	fn text_binary_and_directory_mentions_are_classified_explicitly() {
		let tree = tempfile::tempdir().unwrap();
		let text = tree.path().join("main.rs");
		fs::write(&text, "fn main() {}\n").unwrap();
		let Attachment::Text(rendered) = read_reference(&text, &mut 0, true).unwrap() else {
			panic!("text");
		};
		assert!(rendered.contains("["));
		assert!(rendered.contains("#"));
		assert!(rendered.contains("1:fn main() {}"));

		let binary = tree.path().join("blob.bin");
		fs::write(&binary, b"a\0b").unwrap();
		let Attachment::Text(notice) = read_reference(&binary, &mut 0, true).unwrap() else {
			panic!("binary notice");
		};
		assert!(notice.contains("binary file"));

		let Attachment::Text(listing) = read_reference(tree.path(), &mut 0, true).unwrap() else {
			panic!("directory");
		};
		assert!(listing.contains("blob.bin"));
		assert!(listing.contains("main.rs"));
	}
}
