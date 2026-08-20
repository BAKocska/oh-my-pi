use std::fmt::{self, Write as _};

use omp_core::Str;
use omp_tool::{
	CallOutcome, Part, PromptCaps, ToolIdentity,
	render::{Render, RenderRegistry, RenderRegistryError},
};

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
	pub edit:  Option<ToolIdentity>,
	/// Identity of the native regex search tool, when enabled.
	pub grep:  Option<ToolIdentity>,
	/// Identity of the native path matching tool, when enabled.
	pub glob:  Option<ToolIdentity>,
	/// Identity of the native persistent shell, when enabled.
	pub shell: Option<ToolIdentity>,
	/// Identity of the native whole-file writer, when enabled.
	pub write: Option<ToolIdentity>,
	/// Identity of the native resource reader, when enabled.
	pub read:  Option<ToolIdentity>,
	/// Identity of the native persistent evaluator, when enabled.
	pub eval:  Option<ToolIdentity>,
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
	if let Some(identity) = identities.glob {
		registry.register(identity, GlobRenderer)?;
	}
	if let Some(identity) = identities.shell {
		registry.register(identity, ShellRenderer)?;
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

impl Render for EditRenderer {
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

impl Render for GrepRenderer {
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

struct GlobRenderer;

impl Render for GlobRenderer {
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
}

struct ShellRenderer;

impl Render for ShellRenderer {
	type Outcome = CallOutcome<crate::shell::Payload, crate::shell::Fault>;
	type State = StreamState;
	type Update = crate::shell::Update;

	fn fold(&self, state: &mut Self::State, update: Self::Update) {
		state.bytes = state
			.bytes
			.saturating_add(u64::try_from(update.data.len()).unwrap_or(u64::MAX));
		state.last_sequence = Some(update.sequence);
	}

	fn view(&self, state: &Self::State, outcome: Option<&Self::Outcome>) -> Option<Str> {
		match outcome {
			None => Some(stream_live_view("shell", state)),
			Some(CallOutcome::Ok(payload)) => Some(render_shell_payload(payload)),
			Some(CallOutcome::Faulted(fault)) => Some(fault_view("shell", &shell_fault(fault))),
			Some(CallOutcome::ArgsRejected(_) | CallOutcome::Aborted { .. }) => None,
		}
	}
}

struct WriteRenderer;

impl Render for WriteRenderer {
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

impl Render for ReadRenderer {
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

impl Render for EvalRenderer {
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
	Str::from(output)
}

fn stream_live_view(name: &str, state: &StreamState) -> Str {
	let status = if state.last_sequence.is_some() {
		format!("running · {} bytes", state.bytes)
	} else {
		String::from("running")
	};
	live_view(name, &status)
}

fn fault_view(name: &str, message: &str) -> Str {
	let mut output = String::from("<row gap=1><text bold fg=error>");
	push_text(&mut output, name);
	output.push_str("</text><text fg=error>");
	push_text(&mut output, message);
	output.push_str("</text></row>");
	Str::from(output)
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
	Str::from(output)
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
	Str::from(output)
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
	Str::from(output)
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
	Str::from(output)
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

fn render_shell_payload(payload: &crate::shell::Payload) -> Str {
	let retained = payload
		.transcript
		.iter()
		.map(|frame| frame.data.len())
		.sum::<usize>();
	let mut status = debug_label(payload.status.outcome);
	if let Some(code) = payload.status.exit_code {
		write!(status, " · exit {code}").expect("writing to String cannot fail");
	}
	if let Some(signal) = &payload.status.signal {
		write!(status, " · signal {signal}").expect("writing to String cannot fail");
	}
	let mut output = String::from("<col gap=0><row gap=1><text bold>shell</text><text>");
	push_text(&mut output, &status);
	output.push_str("</text><text fg=muted>");
	write!(output, "{retained} bytes · {} ms", payload.status.wall_clock_ms)
		.expect("writing to String cannot fail");
	if payload.status.spilled_output.is_some() {
		output.push_str(" · full verdict stored as blob");
	}
	output.push_str("</text></row><text truncate>");
	push_text(&mut output, &payload.command);
	output.push_str("</text>");
	if let Some(frame) = payload.transcript.last() {
		output.push_str("<text fg=muted truncate>");
		push_text(&mut output, &String::from_utf8_lossy(&frame.data));
		output.push_str("</text><text fg=muted>ctrl+o to expand</text>");
	}
	output.push_str("</col>");
	Str::from(output)
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
	Str::from(output)
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
	Str::from(output)
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
	Str::from(output)
}

fn debug_label(value: impl fmt::Debug) -> String {
	format!("{value:?}").to_ascii_lowercase()
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
			vec![Part::Text { text: Str::from(self.text) }]
		}
	}
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use omp_core::Str;
	use omp_tool::{
		Abort, ArgIssue, ArgIssueKind, CallOutcome, Rev, ToolIdentity,
		render::{RenderRegistry, ViewState},
	};

	use super::{BuiltinRendererIdentities, register_builtin_renderers};

	fn identity(name: &str, revision: u16) -> ToolIdentity {
		ToolIdentity { name: Str::from(name), rev: Rev { family: Str::from("test"), n: revision } }
	}

	fn identities() -> BuiltinRendererIdentities {
		BuiltinRendererIdentities {
			edit:  Some(identity("edit", 41)),
			grep:  Some(identity("grep", 42)),
			glob:  Some(identity("glob", 43)),
			shell: Some(identity("shell", 44)),
			write: Some(identity("write", 45)),
			read:  Some(identity("read", 46)),
			eval:  Some(identity("eval", 47)),
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
			identities.glob.as_ref().unwrap(),
			identities.shell.as_ref().unwrap(),
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
			preview:       Str::from("+&lt;already-markup"),
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
	fn typed_fault_renders_while_args_and_abort_use_generic_facts() {
		let (registry, identities) = registry(identities());
		let state = ViewState::new();
		let fault = CallOutcome::<crate::read::Payload, crate::read::Fault>::Faulted(
			crate::read::Fault::Source { message: Str::from("missing <file> & owner") },
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
			expected: Str::from("path"),
			kind:     ArgIssueKind::Missing,
			example:  Some(Str::from(r#"{"path":"src/lib.rs"}"#)),
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
				reason: Str::from("cancelled"),
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
				resolved_path:    Str::from("/tmp/a<&.txt"),
				display_path:     Str::from("a<&.txt"),
				byte_len:         9,
				reported_len:     9,
				disposition:      crate::write::WriteDisposition::Created,
				stripped_wrapper: false,
				made_executable:  true,
				snapshot_tag:     Some(Str::from("ABCD")),
				operation:        crate::write::WriteOperation::Plain,
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
