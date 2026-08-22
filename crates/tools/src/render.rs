use std::fmt::{self, Write as _};

use omp_core::Str;
use omp_tool::{
	CallOutcome, Part, PromptCaps, ToolIdentity,
	render::{RenderFold, RenderRegistry, RenderRegistryError},
};
use serde::Deserialize;

/// Bounded JSON-tree previews shared by structured tool views.
pub mod json_tree;
/// Grouped path and directory-tree rendering.
pub mod paths;
/// Shared line, byte, and column truncation.
pub mod truncate;

/// Exact production identities associated with enabled native renderer
/// implementations.
///
/// Composition supplies identities only for tools that were actually
/// registered. Renderers therefore auto-follow tool inclusion independently:
/// disabling one tool cannot suppress every unrelated renderer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BuiltinRendererIdentities {
	/// Identity of the native hashline editor, when enabled.
	pub edit:       Option<ToolIdentity>,
	/// Identity of the native regex search tool, when enabled.
	pub grep:       Option<ToolIdentity>,
	/// Identity of canonical web search, when enabled.
	pub web_search: Option<ToolIdentity>,
	/// Identity of the native path matching tool, when enabled.
	pub glob:       Option<ToolIdentity>,
	/// Identity of the native persistent shell, when enabled.
	pub shell:      Option<ToolIdentity>,
	/// Identity of the native coordination hub, when enabled.
	pub hub:        Option<ToolIdentity>,
	/// Identity of the native whole-file writer, when enabled.
	pub write:      Option<ToolIdentity>,
	/// Identity of the native resource reader, when enabled.
	pub read:       Option<ToolIdentity>,
	/// Identity of the native persistent evaluator, when enabled.
	pub eval:       Option<ToolIdentity>,
}

/// Registers every native renderer under the exact identities supplied by
/// production composition.
///
/// # Errors
///
/// Returns the first duplicate-identity error reported by `registry`.
pub fn register_builtin_renderers(
	registry: &mut RenderRegistry,
	identities: BuiltinRendererIdentities,
) -> Result<(), RenderRegistryError> {
	if let Some(identity) = identities.edit {
		registry.register(identity, EditRenderer)?;
	}
	if let Some(identity) = identities.grep {
		registry.register(identity, GrepRenderer)?;
	}
	if let Some(identity) = identities.web_search {
		registry.register(identity, WebSearchRenderer)?;
	}
	if let Some(identity) = identities.glob {
		registry.register(identity, GlobRenderer)?;
	}
	if let Some(identity) = identities.shell {
		registry.register(identity, ShellRenderer)?;
	}
	if let Some(identity) = identities.hub {
		registry.register(identity, HubRenderer)?;
	}
	if let Some(identity) = identities.write {
		registry.register(identity, WriteRenderer)?;
	}
	if let Some(identity) = identities.read {
		registry.register(identity, ReadRenderer)?;
	}
	if let Some(identity) = identities.eval {
		registry.register(identity, EvalRenderer)?;
	}
	Ok(())
}

#[derive(Default)]
struct EditState {
	latest: Option<crate::edit::EditUpdate>,
}

struct EditRenderer;

impl RenderFold for EditRenderer {
	type Outcome = CallOutcome<crate::edit::Payload, crate::edit::Fault>;
	type State = EditState;
	type Update = crate::edit::EditUpdate;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.latest = Some(update);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(render_edit_live(state.latest.as_ref())),
			Some(CallOutcome::Ok(payload)) => Some(render_edit_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("edit", &edit_fault(fault))),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

struct GrepRenderer;

impl RenderFold for GrepRenderer {
	type Outcome = CallOutcome<crate::grep::Payload, crate::grep::Fault>;
	type State = ();
	type Update = crate::grep::Update;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn view(&self, _state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("grep", "searching")),
			Some(CallOutcome::Ok(payload)) => Some(render_grep_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("grep", &fault.to_string())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

struct WebSearchRenderer;

impl RenderFold for WebSearchRenderer {
	type Outcome = CallOutcome<crate::web_search::Payload, crate::web_search::Fault>;
	type State = ();
	type Update = crate::web_search::Update;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn view(&self, _state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("web_search", "searching providers")),
			Some(CallOutcome::Ok(payload)) => Some(render_web_search_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(render_web_search_fault(&fault.to_string())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

struct GlobRenderer;

impl RenderFold for GlobRenderer {
	type Outcome = CallOutcome<crate::glob::Payload, crate::glob::Fault>;
	type State = ();
	type Update = crate::glob::Update;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn view(&self, _state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("glob", "matching paths")),
			Some(CallOutcome::Ok(payload)) => Some(render_glob_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("glob", &fault.to_string())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

#[derive(Default)]
struct StreamState {
	bytes:         u64,
	last_sequence: Option<u64>,
	tail:          Vec<u8>,
	cached:        Option<Str>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ShellRenderOutcome {
	Call(CallOutcome<crate::shell::Payload, crate::shell::Fault>),
	Terminal(omp_tool::ToolTerminal<crate::shell::Payload, crate::shell::Fault>),
}

struct ShellRenderer;

impl RenderFold for ShellRenderer {
	type Outcome = ShellRenderOutcome;
	type State = StreamState;
	type Update = crate::shell::Update;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.bytes = state
			.bytes
			.saturating_add(u64::try_from(update.data.len()).unwrap_or(u64::MAX));
		state.last_sequence = Some(update.sequence);
		append_bounded_tail(&mut state.tail, update.data.as_ref());
		state.cached = Some(render_shell_live(state));
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(
				state
					.cached
					.clone()
					.unwrap_or_else(|| render_shell_live(state)),
			),
			Some(ShellRenderOutcome::Call(CallOutcome::Ok(payload)))
			| Some(ShellRenderOutcome::Terminal(omp_tool::ToolTerminal::Done {
				result: Ok(payload),
				..
			})) => Some(render_shell_payload(payload)),
			Some(ShellRenderOutcome::Call(CallOutcome::Faulted(fault)))
			| Some(ShellRenderOutcome::Terminal(omp_tool::ToolTerminal::Done {
				result: Err(fault),
				..
			})) => Some(fault_view("shell", &shell_fault(fault))),
			Some(ShellRenderOutcome::Terminal(omp_tool::ToolTerminal::Detached(job))) => {
				Some(render_shell_detached(job))
			},
			Some(ShellRenderOutcome::Call(
				CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. },
			)) => None,
		}
	}
}
#[derive(Default)]
struct HubState {
	latest: Option<crate::hub::Response>,
}

struct HubRenderer;

impl RenderFold for HubRenderer {
	type Outcome = CallOutcome<crate::hub::Response, crate::hub::Fault>;
	type State = HubState;
	type Update = crate::hub::Response;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.latest = Some(update);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => state
				.latest
				.as_ref()
				.and_then(render_hub_response)
				.or_else(|| Some(live_view("hub", "waiting for peer, job, or process activity"))),
			Some(CallOutcome::Ok(response)) => render_hub_response(response),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("hub", &fault.message)),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

struct WriteRenderer;

impl RenderFold for WriteRenderer {
	type Outcome = CallOutcome<crate::write::Payload, crate::write::Fault>;
	type State = ();
	type Update = crate::write::Update;

	fn fold(&self, _state: &mut Self::State, update: Self::Update) {
		match update {}
	}

	fn view(&self, _state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("write", "writing")),
			Some(CallOutcome::Ok(payload)) => Some(render_write_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("write", &fault.to_string())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

#[derive(Default)]
struct ReadState {
	phase: Option<Str>,
}

struct ReadRenderer;

impl RenderFold for ReadRenderer {
	type Outcome = CallOutcome<crate::read::Payload, crate::read::Fault>;
	type State = ReadState;
	type Update = crate::read::Update;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.phase = Some(update.phase);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(live_view("read", state.phase.as_deref().unwrap_or("reading"))),
			Some(CallOutcome::Ok(payload)) => Some(render_read_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("read", fault.message())),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

struct EvalRenderer;

impl RenderFold for EvalRenderer {
	type Outcome = CallOutcome<crate::eval::Payload, crate::eval::Fault>;
	type State = StreamState;
	type Update = crate::eval::Update;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.bytes = state
			.bytes
			.saturating_add(u64::try_from(update.data.len()).unwrap_or(u64::MAX));
		state.last_sequence = Some(update.sequence);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(stream_live_view("eval", state)),
			Some(CallOutcome::Ok(payload)) => Some(render_eval_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("eval", &eval_fault(fault))),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

fn live_view(name: &str, status: &str) -> Str {
	let mut output = String::from("<row gap=1><text bold>");
	push_text(&mut output, name);
	output.push_str("</text><text fg=muted>");
	push_text(&mut output, status);
	output.push_str("</text></row>");
	Str::new(output)
}

fn stream_live_view(name: &str, state: &StreamState) -> Str {
	let status = if state.last_sequence.is_some() {
		format!("running · {} bytes", state.bytes)
	} else {
		String::from("running")
	};
	live_view(name, &status)
}

fn append_bounded_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
	const MAX_LIVE_OUTPUT_BYTES: usize = 16 * 1024;
	if chunk.len() >= MAX_LIVE_OUTPUT_BYTES {
		tail.clear();
		tail.extend_from_slice(&chunk[chunk.len() - MAX_LIVE_OUTPUT_BYTES..]);
		return;
	}
	let overflow = tail
		.len()
		.saturating_add(chunk.len())
		.saturating_sub(MAX_LIVE_OUTPUT_BYTES);
	if overflow > 0 {
		tail.drain(..overflow);
	}
	tail.extend_from_slice(chunk);
}

fn render_shell_live(state: &StreamState) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=accent><col gap=0><row gap=1><text bold \
		 fg=accent>$</text><text bold>shell</text>",
	);
	if state.last_sequence.is_some() {
		output.push_str("<spinner>running</spinner><text fg=muted>");
		write!(output, "{} bytes", state.bytes).expect("writing to String cannot fail");
		output.push_str("</text>");
	} else {
		output.push_str("<spinner>starting</spinner>");
	}
	output.push_str("</row>");
	if !state.tail.is_empty() {
		output.push_str("<pre fg=muted>");
		push_text(&mut output, &String::from_utf8_lossy(&state.tail));
		output.push_str("</pre><text fg=muted>streaming tail · ctrl+o to expand</text>");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_response(response: &crate::hub::Response) -> Option<Str> {
	let value = serde_json::from_str::<serde_json::Value>(&response.text).ok()?;
	let object = value.as_object()?;
	if let Some(peers) = object.get("peers").and_then(serde_json::Value::as_array) {
		return Some(render_hub_roster(peers));
	}
	if let Some(jobs) = object.get("jobs").and_then(serde_json::Value::as_array) {
		return Some(render_hub_jobs(
			jobs,
			object.get("waitingMs").and_then(serde_json::Value::as_u64),
		));
	}
	if let Some(processes) = object
		.get("processes")
		.and_then(serde_json::Value::as_array)
	{
		return Some(render_hub_processes(processes));
	}
	if object.contains_key("lines") {
		return Some(render_hub_logs(object));
	}
	if object.contains_key("deliveries")
		|| object.contains_key("messages")
		|| object.contains_key("message")
	{
		return Some(render_hub_messages(object));
	}
	if object.contains_key("timeout")
		|| object.contains_key("waitedMs")
		|| object.contains_key("waitingMs")
	{
		return Some(render_hub_wait(object));
	}
	if object.contains_key("name") || object.contains_key("event") || object.contains_key("job") {
		return Some(render_hub_process_or_job(object));
	}
	None
}

fn render_hub_roster(peers: &[serde_json::Value]) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>@</text><text bold>Hub roster</text><text fg=muted>",
	);
	write!(output, "{} peers", peers.len()).expect("writing to String cannot fail");
	output.push_str("</text></row>");
	for peer in peers.iter().take(24) {
		let Some(peer) = peer.as_object() else {
			continue;
		};
		let name = json_string(peer, &["name", "callerName", "id"]).unwrap_or("unknown");
		let status = json_string(peer, &["status", "lifecycle"]).unwrap_or("unknown");
		let parent = json_string(peer, &["parent", "parentId"]);
		let unread = json_u64(peer, &["unread", "unreadCount"]).unwrap_or(0);
		let active = matches!(status, "running" | "active" | "reviving" | "queued");
		output.push_str("<row gap=1>");
		if active {
			output.push_str("<spinner></spinner>");
		} else {
			output.push_str("<text fg=muted>○</text>");
		}
		output.push_str("<text bold>");
		push_text(&mut output, name);
		output.push_str("</text><text fg=muted>");
		push_text(&mut output, status);
		if let Some(parent) = parent {
			output.push_str(" · child of ");
			push_text(&mut output, parent);
		}
		if unread > 0 {
			write!(output, " · {unread} unread").expect("writing to String cannot fail");
		}
		if let Some(activity) = json_u64(peer, &["lastActivityMs", "activityMs", "updatedAtMs"]) {
			write!(output, " · {activity} ms").expect("writing to String cannot fail");
		}
		output.push_str("</text></row>");
	}
	if peers.len() > 24 {
		write!(output, "<text fg=muted>+{} more peers</text>", peers.len() - 24)
			.expect("writing to String cannot fail");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_jobs(jobs: &[serde_json::Value], waiting_ms: Option<u64>) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>&amp;</text><text bold>Jobs</text><text fg=muted>",
	);
	write!(output, "{} tracked", jobs.len()).expect("writing to String cannot fail");
	if let Some(waiting_ms) = waiting_ms {
		output.push_str("</text><spinner>");
		write!(output, "waiting {waiting_ms} ms").expect("writing to String cannot fail");
		output.push_str("</spinner><text fg=muted>");
	}
	output.push_str("</text></row>");
	for job in jobs.iter().take(24) {
		let Some(job) = job.as_object() else {
			continue;
		};
		let id = json_string(job, &["id", "job", "name"]).unwrap_or("unknown");
		let status = json_string(job, &["status", "state", "lifecycle"]).unwrap_or("unknown");
		let running = matches!(status, "queued" | "running" | "active" | "waiting");
		output.push_str("<row gap=1>");
		if running {
			output.push_str("<spinner></spinner>");
		} else {
			output.push_str("<text fg=muted>└</text>");
		}
		output.push_str("<text bold>");
		push_text(&mut output, id);
		output.push_str("</text><text fg=muted>");
		push_text(&mut output, status);
		if let Some(kind) = json_string(job, &["kind", "model"]) {
			output.push_str(" · ");
			push_text(&mut output, kind);
		}
		if let Some(duration) = json_u64(job, &["durationMs", "elapsedMs"]) {
			write!(output, " · {duration} ms").expect("writing to String cannot fail");
		}
		output.push_str("</text></row>");
		if let Some(error) = json_string(job, &["error", "reason"]) {
			output.push_str("<text fg=error>  ");
			push_text(&mut output, error);
			output.push_str("</text>");
		}
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_processes(processes: &[serde_json::Value]) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=secondary><col gap=0><row gap=1><text bold \
		 fg=secondary>&gt;_</text><text bold>Processes</text><text fg=muted>",
	);
	write!(output, "{} supervised", processes.len()).expect("writing to String cannot fail");
	output.push_str("</text></row>");
	for process in processes.iter().take(24) {
		let Some(process) = process.as_object() else {
			continue;
		};
		let name = json_string(process, &["name"]).unwrap_or("unknown");
		let state = json_string(process, &["status", "state"]).unwrap_or("unknown");
		output.push_str("<row gap=1><text bold>");
		push_text(&mut output, name);
		output.push_str("</text><text fg=muted>");
		push_text(&mut output, state);
		if let Some(pid) = json_u64(process, &["pid"]) {
			write!(output, " · pid {pid}").expect("writing to String cannot fail");
		}
		if let Some(uptime) = json_u64(process, &["uptimeMs", "elapsedMs"]) {
			write!(output, " · up {uptime} ms").expect("writing to String cannot fail");
		}
		output.push_str("</text></row>");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_logs(object: &serde_json::Map<String, serde_json::Value>) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=secondary><col gap=0><row gap=1><text bold \
		 fg=secondary>&gt;_</text><text bold>Process log</text>",
	);
	if let Some(name) = json_string(object, &["name"]) {
		output.push_str("<text fg=muted>");
		push_text(&mut output, name);
		output.push_str("</text>");
	}
	output.push_str("</row><box border=round bc=muted><pre>");
	if let Some(lines) = object.get("lines").and_then(serde_json::Value::as_array) {
		for (index, line) in lines.iter().take(80).enumerate() {
			if index > 0 {
				output.push('\n');
			}
			push_text(&mut output, line.as_str().unwrap_or_default());
		}
	} else if let Some(lines) = object.get("lines").and_then(serde_json::Value::as_str) {
		push_text(&mut output, lines);
	}
	output.push_str("</pre></box>");
	if let Some(cursor) = json_u64(object, &["cursor"]) {
		write!(output, "<text fg=muted>cursor {cursor}</text>")
			.expect("writing to String cannot fail");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_messages(object: &serde_json::Map<String, serde_json::Value>) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>@</text><text bold>IRC</text></row>",
	);
	let rows = object
		.get("messages")
		.or_else(|| object.get("deliveries"))
		.and_then(serde_json::Value::as_array);
	if let Some(rows) = rows {
		for row in rows.iter().take(24) {
			render_hub_message_row(&mut output, row);
		}
	} else if let Some(message) = object.get("message") {
		if !message.is_null() {
			render_hub_message_row(&mut output, message);
		} else {
			output.push_str("<text fg=muted>no message received</text>");
		}
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_hub_message_row(output: &mut String, value: &serde_json::Value) {
	let Some(message) = value.as_object() else {
		output.push_str("<text fg=muted>");
		push_text(output, value.as_str().unwrap_or_default());
		output.push_str("</text>");
		return;
	};
	let from = json_string(message, &["from", "sender"]).unwrap_or("me");
	let to = json_string(message, &["to", "recipient"]).unwrap_or("hub");
	let text = json_string(message, &["text", "message", "outcome", "status"]).unwrap_or_default();
	output.push_str("<row gap=1><text fg=info>");
	push_text(output, from);
	output.push_str(" → ");
	push_text(output, to);
	output.push_str("</text><text>");
	push_text(output, text);
	output.push_str("</text></row>");
}

fn render_hub_wait(object: &serde_json::Map<String, serde_json::Value>) -> Str {
	let waited = json_u64(object, &["waitingMs", "waitedMs"]).unwrap_or(0);
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><row gap=1><spinner>waiting</spinner><text fg=muted>",
	);
	write!(output, "{waited} ms elapsed").expect("writing to String cannot fail");
	if object.get("timeout").and_then(serde_json::Value::as_bool) == Some(true) {
		output.push_str(" · timeout");
	}
	output.push_str("</text></row></box>");
	Str::new(output)
}

fn render_hub_process_or_job(object: &serde_json::Map<String, serde_json::Value>) -> Str {
	let label = if object.contains_key("job") {
		"Job"
	} else {
		"Process"
	};
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=secondary><col gap=0><row gap=1><text bold fg=secondary>",
	);
	push_text(&mut output, label);
	output.push_str("</text><text bold>");
	if let Some(name) = json_string(object, &["name", "job"]) {
		push_text(&mut output, name);
	}
	output.push_str("</text></row>");
	for (key, value) in object {
		if matches!(key.as_str(), "name" | "job") {
			continue;
		}
		output.push_str("<row gap=1><text fg=muted>");
		push_text(&mut output, key);
		output.push_str("</text><text truncate>");
		push_text(&mut output, &json_compact(value));
		output.push_str("</text></row>");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn json_string<'a>(
	object: &'a serde_json::Map<String, serde_json::Value>,
	keys: &[&str],
) -> Option<&'a str> {
	keys
		.iter()
		.find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
}

fn json_u64(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<u64> {
	keys
		.iter()
		.find_map(|key| object.get(*key).and_then(serde_json::Value::as_u64))
}

fn json_compact(value: &serde_json::Value) -> String {
	match value {
		serde_json::Value::String(value) => value.clone(),
		_ => serde_json::to_string(value).unwrap_or_default(),
	}
}

fn fault_view(name: &str, message: &str) -> Str {
	let mut output = String::from("<row gap=1><text bold fg=error>");
	push_text(&mut output, name);
	output.push_str("</text><text fg=error>");
	push_text(&mut output, message);
	output.push_str("</text></row>");
	Str::new(output)
}

fn render_edit_live(update: Option<&crate::edit::EditUpdate>) -> Str {
	let Some(update) = update else {
		return live_view("edit", "preparing");
	};
	let mut output = String::from("<col gap=0><row gap=1><text bold>edit</text><text fg=muted>");
	write!(
		output,
		"preview · {} ops · +{} -{}",
		update.applied_ops, update.added_lines, update.removed_lines
	)
	.expect("writing to String cannot fail");
	output.push_str("</text></row><diff>");
	push_text(&mut output, &update.preview);
	output.push_str("</diff></col>");
	Str::new(output)
}

fn render_edit_payload(payload: &crate::edit::Payload) -> Str {
	let (added, removed) = payload
		.sections
		.iter()
		.flat_map(|section| section.diff.lines())
		.fold((0usize, 0usize), |(added, removed), line| {
			(
				added + usize::from(line.starts_with('+') && !line.starts_with("+++")),
				removed + usize::from(line.starts_with('-') && !line.starts_with("---")),
			)
		});
	let mut output = String::from("<col gap=0><row gap=1><text bold>edit</text><text>");
	write!(output, "{} files changed · +{added} -{removed}", payload.sections.len())
		.expect("writing to String cannot fail");
	output.push_str("</text></row>");
	for section in &payload.sections {
		output.push_str("<row gap=1><text>");
		push_text(&mut output, &section.path);
		output.push_str("</text><text fg=muted>");
		write!(output, "{} ops", section.applied_ops.len()).expect("writing to String cannot fail");
		if section.rebased {
			output.push_str(" · rebased");
		}
		output.push_str("</text></row><diff>");
		push_text(&mut output, &section.diff);
		output.push_str("</diff>");
	}
	output.push_str("</col>");
	Str::new(output)
}

fn edit_fault(fault: &crate::edit::Fault) -> String {
	use crate::edit::RejectionReason;
	let mut output = match &fault.reason {
		RejectionReason::Conflict => String::from("edit conflict"),
		RejectionReason::StaleUnrecoverable { message }
		| RejectionReason::Format { message }
		| RejectionReason::InvalidPatch { message } => message.to_string(),
	};
	for conflict in &fault.conflicts {
		write!(
			output,
			" · lines {}-{}: {}",
			conflict.start_line, conflict.end_line, conflict.message
		)
		.expect("writing to String cannot fail");
	}
	output
}

fn render_grep_payload(payload: &crate::grep::Payload) -> Str {
	let matches = payload
		.files
		.iter()
		.map(|file| file.matches.len())
		.sum::<usize>();
	let mut output = String::from("<col gap=0><row gap=1><text bold>grep</text><text>");
	write!(output, "{matches} matches in {} files", payload.total_files)
		.expect("writing to String cannot fail");
	if payload.total_files_lower_bound {
		output.push_str(" or more");
	}
	output.push_str("</text></row>");
	for file in &payload.files {
		output.push_str("<row gap=1><text>");
		push_text(&mut output, &file.path);
		output.push_str("</text><text fg=muted>");
		write!(output, "{} matches", file.matches.len()).expect("writing to String cannot fail");
		output.push_str("</text></row>");
	}
	for note in &payload.notes {
		output.push_str("<text fg=muted>");
		push_text(&mut output, note);
		output.push_str("</text>");
	}
	output.push_str("</col>");
	Str::new(output)
}

fn render_glob_payload(payload: &crate::glob::Payload) -> Str {
	let mut output = String::from("<col gap=0><row gap=1><text bold>glob</text><text>");
	write!(output, "{} paths", payload.matches.len()).expect("writing to String cannot fail");
	if payload.truncated {
		write!(output, " · truncated from {} partial matches", payload.partial_match_count)
			.expect("writing to String cannot fail");
	}
	if payload.timed_out {
		write!(output, " · timed out after {} ms", payload.timeout_ms)
			.expect("writing to String cannot fail");
	}
	output.push_str("</text></row>");
	for entry in &payload.matches {
		output.push_str("<text>");
		push_text(&mut output, &entry.path);
		if entry.is_dir {
			output.push('/');
		}
		output.push_str("</text>");
	}
	output.push_str("</col>");
	Str::new(output)
}

fn shell_fault(fault: &crate::shell::Fault) -> String {
	match fault {
		crate::shell::Fault::Resource { operation, message } => format!("{operation}: {message}"),
		crate::shell::Fault::InvalidEnvironmentKey { key } => {
			format!("invalid shell environment key {key:?}")
		},
		crate::shell::Fault::AsyncNameRequired => {
			String::from("async shell execution requires a name")
		},
	}
}

fn render_shell_detached(job: &omp_tool::JobRef) -> Str {
	let mut output = String::from(
		"<box border=round pad=\"0 1\" bc=info><col gap=0><row gap=1><text bold \
		 fg=info>$</text><text bold>shell detached</text><spinner>running</spinner></row><row \
		 gap=1><text fg=muted>job</text><text bold>",
	);
	push_text(&mut output, &job.id);
	output.push_str("</text></row><text>");
	push_text(&mut output, &job.metadata.label);
	output.push_str("</text><text fg=muted>completion will be delivered by the job board</text>");
	output.push_str("</col></box>");
	Str::new(output)
}

fn render_shell_payload(payload: &crate::shell::Payload) -> Str {
	const PREVIEW_LINES: usize = 20;
	let retained = payload
		.transcript
		.iter()
		.map(|frame| frame.data.len())
		.sum::<usize>();
	let outcome = debug_label(payload.status.outcome);
	let color = match payload.status.outcome {
		crate::shell::ExecOutcome::Exited if payload.status.exit_code.unwrap_or_default() == 0 => {
			"success"
		},
		crate::shell::ExecOutcome::Timeout => "warning",
		crate::shell::ExecOutcome::Exited
		| crate::shell::ExecOutcome::Failed
		| crate::shell::ExecOutcome::Cancelled
		| crate::shell::ExecOutcome::Denied => "error",
	};
	let mut output = String::from("<box border=round pad=\"0 1\" bc=");
	output.push_str(color);
	output.push_str(
		"><col gap=0><row gap=1><text bold fg=accent>$</text><text bold>shell</text><text fg=",
	);
	output.push_str(color);
	output.push('>');
	push_text(&mut output, &outcome);
	output.push_str("</text>");
	if let Some(code) = payload.status.exit_code {
		output.push_str("<text fg=");
		output.push_str(color);
		output.push('>');
		write!(output, "exit {code}").expect("writing to String cannot fail");
		output.push_str("</text>");
	}
	if let Some(signal) = &payload.status.signal {
		output.push_str("<text fg=error>");
		push_text(&mut output, signal);
		output.push_str("</text>");
	}
	output.push_str("<text fg=muted>");
	write!(output, "{} ms · {retained} bytes", payload.status.wall_clock_ms)
		.expect("writing to String cannot fail");
	output.push_str("</text></row><pre fg=accent>");
	push_text(&mut output, "$ ");
	push_text(&mut output, &payload.command);
	output.push_str("</pre>");
	if let Some(cwd) = &payload.status.final_cwd_uri {
		output.push_str("<row gap=1><text fg=muted>cwd</text><text truncate>");
		push_text(&mut output, cwd);
		output.push_str("</text></row>");
	}
	let contains_sixel = payload.transcript.iter().any(|frame| {
		frame.data.as_ref().contains(&0x90)
			|| frame
				.data
				.as_ref()
				.windows(2)
				.any(|window| window == b"\x1bP")
	});
	let transcript = bounded_transcript_tail(&payload.transcript, contains_sixel);
	if !transcript.is_empty() {
		let text = String::from_utf8_lossy(&transcript);
		let lines = text.lines().collect::<Vec<_>>();
		let preview_start = lines.len().saturating_sub(PREVIEW_LINES);
		output.push_str("<pre fg=muted>");
		for (index, line) in lines[preview_start..].iter().enumerate() {
			if index > 0 {
				output.push('\n');
			}
			push_text(&mut output, line);
		}
		output.push_str("</pre>");
		if preview_start > 0 {
			write!(
				output,
				"<text fg=muted>{preview_start} earlier lines hidden · ctrl+o to expand</text>"
			)
			.expect("writing to String cannot fail");
		}
	}
	if payload.status.spilled_output.is_some() {
		output.push_str("<text fg=muted>full output stored as blob</text>");
	}
	if payload.status.effects_unknown {
		output.push_str("<text fg=warning>final effect state is unknown</text>");
	}
	output.push_str("</col></box>");
	Str::new(output)
}

fn bounded_transcript_tail(
	transcript: &[crate::shell::TranscriptFrame],
	retain_all: bool,
) -> Vec<u8> {
	const MAX_RENDER_BYTES: usize = 64 * 1024;
	let total = transcript
		.iter()
		.map(|frame| frame.data.len())
		.sum::<usize>();
	let retain = if retain_all {
		total
	} else {
		total.min(MAX_RENDER_BYTES)
	};
	let skip = total.saturating_sub(retain);
	let mut output = Vec::with_capacity(retain);
	let mut offset = 0usize;
	for frame in transcript {
		let bytes = frame.data.as_ref();
		let frame_end = offset.saturating_add(bytes.len());
		if frame_end > skip {
			let start = skip.saturating_sub(offset);
			output.extend_from_slice(&bytes[start..]);
		}
		offset = frame_end;
	}
	output
}

fn render_write_payload(payload: &crate::write::Payload) -> Str {
	let disposition = debug_label(payload.disposition);
	let mut output = String::from("<row gap=1><text bold>write</text><text>");
	push_text(&mut output, &disposition);
	output.push(' ');
	push_text(&mut output, &payload.display_path);
	output.push_str("</text><text fg=muted>");
	write!(output, "{} bytes", payload.byte_len).expect("writing to String cannot fail");
	if payload.made_executable {
		output.push_str(" · executable");
	}
	if payload.stripped_wrapper {
		output.push_str(" · stripped wrapper");
	}
	output.push_str("</text></row>");
	Str::new(output)
}

fn render_read_payload(payload: &crate::read::Payload) -> Str {
	let mut text_bytes = 0usize;
	let mut blobs = 0usize;
	let mut blob_bytes = 0u64;
	for part in &payload.parts {
		match part {
			crate::read::PayloadPart::Text { text } => {
				text_bytes = text_bytes.saturating_add(text.len());
			},
			crate::read::PayloadPart::Blob { blob, .. } => {
				blobs = blobs.saturating_add(1);
				blob_bytes = blob_bytes.saturating_add(blob.byte_len);
			},
		}
	}
	let mut output = String::from("<row gap=1><text bold>read</text><text>");
	write!(output, "{} parts · {text_bytes} text bytes", payload.parts.len())
		.expect("writing to String cannot fail");
	if blobs != 0 {
		write!(output, " · {blobs} blobs · {blob_bytes} blob bytes")
			.expect("writing to String cannot fail");
	}
	output.push_str("</text></row>");
	Str::new(output)
}

fn eval_fault(fault: &crate::eval::Fault) -> String {
	match fault {
		crate::eval::Fault::InvalidTimeout => String::from("timeout must be non-negative and finite"),
		crate::eval::Fault::Resource { operation, message } => {
			format!("{operation}: {message}")
		},
		crate::eval::Fault::SessionLost { message } => message.to_string(),
	}
}

fn render_eval_payload(payload: &crate::eval::Payload) -> Str {
	let mut status = debug_label(payload.status.outcome);
	if let Some(code) = payload.status.exit_code {
		write!(status, " · exit {code}").expect("writing to String cannot fail");
	}
	let retained = payload
		.frames
		.iter()
		.map(|frame| frame.data.len())
		.sum::<usize>();
	let mut output = String::from("<col gap=0><row gap=1><text bold>eval</text><text>");
	push_text(&mut output, &status);
	output.push_str("</text><text fg=muted>");
	write!(
		output,
		"{retained} retained bytes · {} total bytes · {} ms",
		payload.total_bytes, payload.status.duration_ms
	)
	.expect("writing to String cannot fail");
	output.push_str("</text></row>");
	if let Some(title) = &payload.title {
		output.push_str("<text bold>");
		push_text(&mut output, title);
		output.push_str("</text>");
	}
	if let Some(exception) = &payload.status.exception {
		output.push_str("<text fg=error>");
		push_text(&mut output, &exception.name);
		if !exception.message.is_empty() {
			output.push_str(": ");
			push_text(&mut output, &exception.message);
		}
		output.push_str("</text>");
	}
	if payload.truncated {
		output.push_str("<text fg=muted>output truncated</text>");
	}
	output.push_str("</col>");
	Str::new(output)
}

fn render_web_search_payload(payload: &crate::web_search::Payload) -> Str {
	let response = &payload.response;
	let mut output = String::from("<col gap=0><row gap=1><text bold>web_search</text>");
	if !response.engine.is_empty() {
		output.push_str("<text fg=accent bold>");
		push_text(&mut output, &response.engine);
		output.push_str("</text>");
	}
	if !response.auth_mode.is_empty() {
		output.push_str("<text fg=muted>");
		push_text(&mut output, &response.auth_mode);
		output.push_str("</text>");
	}
	output.push_str("</row>");
	if !response.answer.is_empty() {
		output.push_str("<md>");
		push_text(&mut output, &response.answer);
		output.push_str("</md>");
	}
	if !response.sources.is_empty() {
		output.push_str("<text bold>Sources</text><col gap=0>");
		for (index, source) in response.sources.iter().enumerate() {
			output.push_str("<row gap=1><text fg=muted>");
			write!(output, "{}.", index + 1).expect("writing to a String cannot fail");
			output.push_str("</text><text href=\"");
			push_attr(&mut output, &source.url);
			output.push_str("\" fg=accent underline>");
			if source.title.is_empty() {
				push_text(&mut output, &source.url);
			} else {
				push_text(&mut output, &source.title);
			}
			output.push_str("</text>");
			if !source.snippet.is_empty() {
				output.push_str("<text fg=muted truncate>");
				push_text(&mut output, &source.snippet);
				output.push_str("</text>");
			}
			output.push_str("</row>");
		}
		output.push_str("</col>");
	}
	if let Some(usage) = response.usage.as_ref() {
		let total = usage
			.total_tokens
			.unwrap_or_else(|| usage.input_tokens.saturating_add(usage.output_tokens));
		let searches = usage
			.server_tools
			.as_ref()
			.and_then(|tools| tools.web_search_requests)
			.unwrap_or(0);
		if total != 0 || searches != 0 {
			output.push_str("<row gap=1><text fg=muted>");
			if total != 0 {
				write!(output, "{total} tokens").expect("writing to a String cannot fail");
			}
			if total != 0 && searches != 0 {
				output.push_str(" · ");
			}
			if searches != 0 {
				write!(output, "{searches} search requests").expect("writing to a String cannot fail");
			}
			output.push_str("</text></row>");
		}
	}
	for warning in &response.warnings {
		output.push_str("<row gap=1><text fg=warn bold>relaxed</text><text fg=warn>");
		push_text(&mut output, warning);
		output.push_str("</text></row>");
	}
	for failure in &response.failures {
		output.push_str("<row gap=1><text fg=muted>");
		push_text(&mut output, &failure.provider);
		output.push_str("</text><text fg=warn>");
		push_text(&mut output, &failure.code);
		if let Some(status) = failure.status {
			write!(output, " · HTTP {status}").expect("writing to a String cannot fail");
		}
		output.push_str("</text></row>");
	}
	output.push_str("</col>");
	Str::new(output)
}

fn render_web_search_fault(message: &str) -> Str {
	let mut output = String::from("<col gap=0><row gap=1><text bold fg=error>web_search</text>");
	output.push_str("<text fg=error>failed</text></row><text fg=error>");
	push_text(&mut output, message);
	output.push_str("</text></col>");
	Str::new(output)
}

fn debug_label(value: impl fmt::Debug) -> String {
	format!("{value:?}").to_ascii_lowercase()
}

fn push_attr(output: &mut String, text: &str) {
	for character in text.chars() {
		match character {
			'&' => output.push_str("&amp;"),
			'<' => output.push_str("&lt;"),
			'>' => output.push_str("&gt;"),
			'"' => output.push_str("&quot;"),
			'\'' => output.push_str("&#39;"),
			character if character.is_control() => output.push('\u{fffd}'),
			character => output.push(character),
		}
	}
}

fn push_text(output: &mut String, text: &str) {
	for character in text.chars() {
		match character {
			'&' => output.push_str("&amp;"),
			'<' => output.push_str("&lt;"),
			'>' => output.push_str("&gt;"),
			'\t' | '\n' | '\r' => output.push(character),
			character if character.is_control() => output.push('\u{fffd}'),
			character => output.push(character),
		}
	}
}

/// Accumulates whole UTF-8 fragments without splitting a caller-owned unit.
pub struct TextProjection {
	text:      String,
	max_bytes: usize,
	truncated: bool,
}

impl TextProjection {
	pub(crate) fn new(caps: PromptCaps) -> Option<Self> {
		(caps.maximum_parts != 0 && caps.maximum_text_bytes != 0).then(|| Self {
			text:      String::new(),
			max_bytes: usize::try_from(caps.maximum_text_bytes).unwrap_or(usize::MAX),
			truncated: false,
		})
	}

	pub(crate) fn push(&mut self, fragment: &str) -> bool {
		if self.text.len().saturating_add(fragment.len()) > self.max_bytes {
			self.truncated = true;
			return false;
		}
		self.text.push_str(fragment);
		true
	}

	pub(crate) fn finish(mut self) -> Vec<Part> {
		const MARKER: &str = "\n[truncated]";
		if self.truncated && self.text.len().saturating_add(MARKER.len()) <= self.max_bytes {
			self.text.push_str(MARKER);
		}
		if self.text.is_empty() {
			Vec::new()
		} else {
			vec![Part::Text { text: Str::new(self.text) }]
		}
	}
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use omp_core::{Str, sf};
	use omp_tool::{
		Abort, ArgIssue, ArgIssueKind, CallOutcome, Rev, ToolIdentity,
		render::{RenderRegistry, ViewState},
	};

	use super::{BuiltinRendererIdentities, register_builtin_renderers};

	fn identity(name: &str, revision: u16) -> ToolIdentity {
		ToolIdentity { name: Str::new(name), rev: Rev { family: sf!("test"), n: revision } }
	}

	fn identities() -> BuiltinRendererIdentities {
		BuiltinRendererIdentities {
			edit:       Some(identity("edit", 41)),
			grep:       Some(identity("grep", 42)),
			web_search: Some(identity("web_search", 48)),
			glob:       Some(identity("glob", 43)),
			shell:      Some(identity("shell", 44)),
			hub:        Some(identity("hub", 45)),
			write:      Some(identity("write", 45)),
			read:       Some(identity("read", 46)),
			eval:       Some(identity("eval", 47)),
		}
	}

	fn registry(
		identities: BuiltinRendererIdentities,
	) -> (RenderRegistry, BuiltinRendererIdentities) {
		let mut registry = RenderRegistry::new();
		register_builtin_renderers(&mut registry, identities.clone())
			.expect("unique built-in identities register");
		(registry, identities)
	}

	#[test]
	fn registers_every_builtin_at_only_its_exact_revision() {
		let (registry, identities) = registry(identities());
		for identity in [
			identities.edit.as_ref().unwrap(),
			identities.grep.as_ref().unwrap(),
			identities.web_search.as_ref().unwrap(),
			identities.glob.as_ref().unwrap(),
			identities.shell.as_ref().unwrap(),
			identities.hub.as_ref().unwrap(),
			identities.write.as_ref().unwrap(),
			identities.read.as_ref().unwrap(),
			identities.eval.as_ref().unwrap(),
		] {
			assert!(
				registry
					.get(identity)
					.is_some_and(|entry| entry.identity() == identity)
			);
		}

		let wrong_revision = identity("edit", identities.edit.as_ref().unwrap().rev.n + 1);
		assert!(registry.get(&wrong_revision).is_none());
		let raw = br#"{"kind":"ok","value":{"foreign":true}}"#;
		assert_eq!(
			registry
				.view(&wrong_revision, &ViewState::new(), Some(raw))
				.expect("unknown exact revision uses generic facts")
				.as_str(),
			std::str::from_utf8(raw).expect("fixture is UTF-8"),
		);
	}

	#[test]
	fn disabled_tool_does_not_suppress_enabled_renderers() {
		let read = identity("read", 9);
		let mut registry = RenderRegistry::new();
		register_builtin_renderers(&mut registry, BuiltinRendererIdentities {
			read: Some(read.clone()),
			..Default::default()
		})
		.unwrap();
		assert!(registry.get(&read).is_some());
		assert!(registry.get(&identity("edit", 9)).is_none());
	}

	#[test]
	fn edit_update_reduces_to_compact_state_then_settles() {
		let (registry, identities) = registry(identities());
		let update = crate::edit::EditUpdate {
			applied_ops:   2,
			preview:       sf!("+&lt;already-markup"),
			added_lines:   3,
			removed_lines: 1,
		};
		let mut state = ViewState::new();
		registry
			.fold(
				identities.edit.as_ref().unwrap(),
				&mut state,
				Bytes::from(serde_json::to_vec(&update).expect("update serializes")),
			)
			.expect("typed update folds");
		assert_eq!(state.raw_update_count(), 0);
		assert_eq!(
			registry
				.view(identities.edit.as_ref().unwrap(), &state, None)
				.expect("live edit renders")
				.as_str(),
			"<col gap=0><row gap=1><text bold>edit</text><text fg=muted>preview · 2 ops · +3 \
			 -1</text></row><diff>+&amp;lt;already-markup</diff></col>",
		);

		let outcome =
			CallOutcome::<crate::edit::Payload, crate::edit::Fault>::Ok(crate::edit::Payload {
				sections: Vec::new(),
			});
		let encoded = serde_json::to_vec(&outcome).expect("outcome serializes");
		assert_eq!(
			registry
				.view(identities.edit.as_ref().unwrap(), &state, Some(&encoded))
				.expect("settled edit renders")
				.as_str(),
			"<col gap=0><row gap=1><text bold>edit</text><text>0 files changed · +0 \
			 -0</text></row></col>",
		);
	}

	#[test]
	fn hub_renderer_projects_wait_progress_roster_and_isolated_logs() {
		let (registry, identities) = registry(identities());
		let hub = identities.hub.as_ref().expect("hub identity");
		let mut state = ViewState::new();
		let progress = crate::hub::Response {
			text:    Str::from(r#"{"waitingMs":500,"jobs":[]}"#),
			useless: true,
		};
		registry
			.fold(
				hub,
				&mut state,
				Bytes::from(serde_json::to_vec(&progress).expect("progress serializes")),
			)
			.expect("hub progress folds");
		let live = registry
			.view(hub, &state, None)
			.expect("hub progress renders");
		assert!(live.contains("<spinner>"));
		assert!(live.contains("waiting 500 ms"));

		let response = crate::hub::Response {
			text:    Str::from(
				r#"{"peers":[{"name":"Scout","status":"running","unreadCount":2,"parent":"Main"}]}"#,
			),
			useless: false,
		};
		let encoded =
			serde_json::to_vec(&CallOutcome::<crate::hub::Response, crate::hub::Fault>::Ok(response))
				.expect("outcome serializes");
		let roster = registry
			.view(hub, &state, Some(&encoded))
			.expect("roster renders");
		assert!(roster.contains("Hub roster"));
		assert!(roster.contains("Scout"));
		assert!(roster.contains("2 unread"));
	}

	#[test]
	fn typed_fault_renders_while_args_and_abort_use_generic_facts() {
		let (registry, identities) = registry(identities());
		let state = ViewState::new();
		let fault = CallOutcome::<crate::read::Payload, crate::read::Fault>::Faulted(
			crate::read::Fault::Source { message: sf!("missing <file> & owner") },
		);
		let encoded_fault = serde_json::to_vec(&fault).expect("fault serializes");
		assert_eq!(
			registry
				.view(identities.read.as_ref().unwrap(), &state, Some(&encoded_fault))
				.expect("typed fault renders")
				.as_str(),
			"<row gap=1><text bold fg=error>read</text><text fg=error>missing &lt;file&gt; &amp; \
			 owner</text></row>",
		);

		let args = CallOutcome::<crate::read::Payload, crate::read::Fault>::ArgsRejected(ArgIssue {
			path:     Vec::new(),
			expected: sf!("path"),
			kind:     ArgIssueKind::Missing,
			example:  Some(sf!(r#"{{"path":"src/lib.rs"}}"#)),
			found:    None,
		});
		let encoded_args = serde_json::to_vec(&args).expect("argument issue serializes");
		assert_eq!(
			registry
				.view(identities.read.as_ref().unwrap(), &state, Some(&encoded_args))
				.expect("argument fallback renders")
				.as_str(),
			std::str::from_utf8(&encoded_args).expect("JSON is UTF-8"),
		);

		let abort =
			CallOutcome::<crate::read::Payload, crate::read::Fault>::aborted(Abort::Interrupted {
				reason: sf!("cancelled"),
			});
		let encoded_abort = serde_json::to_vec(&abort).expect("abort serializes");
		assert_eq!(
			registry
				.view(identities.read.as_ref().unwrap(), &state, Some(&encoded_abort))
				.expect("abort fallback renders")
				.as_str(),
			std::str::from_utf8(&encoded_abort).expect("JSON is UTF-8"),
		);
	}

	#[test]
	fn settled_output_is_deterministic_and_escapes_payload_text() {
		let (registry, identities) = registry(identities());
		let outcome =
			CallOutcome::<crate::write::Payload, crate::write::Fault>::Ok(crate::write::Payload {
				resolved_path:      sf!("/tmp/a<&.txt"),
				display_path:       sf!("a<&.txt"),
				canonical_recovery: None,
				byte_len:           9,
				reported_len:       9,
				disposition:        crate::write::WriteDisposition::Created,
				stripped_wrapper:   false,
				made_executable:    true,
				snapshot_tag:       Some(sf!("ABCD")),
				operation:          crate::write::WriteOperation::Plain,
			});
		let encoded = serde_json::to_vec(&outcome).expect("outcome serializes");
		let state = ViewState::new();
		let write_identity = identities
			.write
			.as_ref()
			.expect("write identity registered");
		let first = registry
			.view(write_identity, &state, Some(&encoded))
			.expect("write renders");
		let second = registry
			.view(write_identity, &state, Some(&encoded))
			.expect("write rerenders");
		assert_eq!(first, second);
		assert_eq!(
			first.as_str(),
			"<row gap=1><text bold>write</text><text>created a&lt;&amp;.txt</text><text fg=muted>9 \
			 bytes · executable</text></row>",
		);
	}
}
