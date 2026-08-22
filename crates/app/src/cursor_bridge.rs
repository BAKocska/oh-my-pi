//! Cursor compatibility bridge with environment-enforced filesystem safety.
//!
//! Cursor frame names are translated into calls to the stable `dyn` tool.  The
//! bridge never manufactures URL-shaped device identities and never bypasses
//! environment admission.

use std::{
	collections::BTreeMap,
	fs,
	path::{Component, Path, PathBuf},
};

use omp_core::{Str, sf};
use serde_json::{Map, Value, json};
use thiserror::Error;

/// Filesystem mutation policy applied before a translated write reaches `dyn`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WritePolicy {
	/// Mutations are forbidden.
	ReadOnly,
	/// Existing regular files may be replaced.
	ExistingOnly,
	/// Regular files may be created or replaced beneath the workspace.
	Workspace,
}

/// A validated invocation of the stable dynamic-device transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynDispatch {
	/// Always `dyn`; exposed so adapters cannot accidentally invoke a device as
	/// a core tool.
	pub tool:      Str,
	/// Flattened arguments accepted by the `dyn` tool.
	pub arguments: Value,
}

/// Cursor compatibility failures.
#[derive(Debug, Error)]
pub enum BridgeError {
	/// The incoming frame family or operation is not supported.
	#[error("unsupported Cursor frame `{0}`")]
	Unsupported(Str),
	/// A required argument was absent or malformed.
	#[error("invalid Cursor arguments: {0}")]
	Invalid(Str),
	/// A write violated the configured filesystem policy.
	#[error("Cursor write rejected: {0}")]
	WriteRejected(Str),
	/// A filesystem safety check failed.
	#[error("Cursor filesystem check failed for {path}: {source}")]
	Io {
		/// Path whose metadata or canonical form was requested.
		path:   PathBuf,
		/// Underlying filesystem failure.
		source: std::io::Error,
	},
}

/// Translates legacy and modern Cursor frames into a `dyn` invocation.
pub fn translate(frame: &str, arguments: &Value) -> Result<DynDispatch, BridgeError> {
	let args = arguments
		.as_object()
		.ok_or_else(|| BridgeError::Invalid(sf!("arguments must be an object")))?;
	if let Some(replace) = cursor_replace_arguments(frame, args) {
		return Ok(dyn_dispatch("edit", translate_edit(replace)?));
	}
	let (device, translated) = match frame {
		"read" | "piRead" => ("read", translate_read(args)?),
		"ls" | "piLs" => ("read", translate_list(args)?),
		"grep" | "piGrep" => ("grep", translate_grep(args)?),
		"shell" | "shellStream" | "piBash" => ("bash", translate_shell(args)?),
		"write" | "piWrite" => ("write", translate_write(args)?),
		"delete" => ("write", translate_delete(args)?),
		"piEdit" => ("edit", translate_edit(args)?),
		"piFind" => ("glob", translate_find(args)?),
		"diagnostics" => ("diagnostics", Value::Object(args.clone())),
		"mcp" | "piMcp" | "mcpResource" => ("mcp", Value::Object(args.clone())),
		other => return Err(BridgeError::Unsupported(Str::from(other))),
	};
	Ok(dyn_dispatch(device, translated))
}

fn dyn_dispatch(device: &str, mut translated: Value) -> DynDispatch {
	let object = translated
		.as_object_mut()
		.expect("translations return objects");
	object.insert("do_".into(), Value::String(format!("invoke/{device}")));
	DynDispatch { tool: sf!("dyn"), arguments: translated }
}

/// Translates a frame and enforces mutation policy before returning a write
/// dispatch. Non-mutating frames are identical to [`translate`].
pub fn translate_checked(
	frame: &str,
	arguments: &Value,
	root: &Path,
	policy: WritePolicy,
) -> Result<DynDispatch, BridgeError> {
	let mut call = translate(frame, arguments)?;
	if matches!(
		call.arguments.get("do_").and_then(Value::as_str),
		Some("invoke/write" | "invoke/edit")
	) {
		let raw = call
			.arguments
			.get("path")
			.and_then(Value::as_str)
			.ok_or_else(|| BridgeError::Invalid(sf!("write has no path")))?;
		let safe = validate_write(root, Path::new(raw), policy)?;
		call.arguments["path"] = Value::String(safe.to_string_lossy().into_owned());
	}
	Ok(call)
}

fn translate_read(args: &Map<String, Value>) -> Result<Value, BridgeError> {
	let mut out = Map::new();
	out.insert("path".into(), Value::String(path_arg(args)?.to_owned()));
	if let Some(start) = unsigned(args, &["start", "offset", "line"])? {
		out.insert("offset".into(), json!(start));
	}
	if let Some(limit) = unsigned(args, &["limit", "length", "lines"])? {
		out.insert("limit".into(), json!(limit));
	}
	Ok(Value::Object(out))
}

fn translate_list(args: &Map<String, Value>) -> Result<Value, BridgeError> {
	let mut out = Map::new();
	out.insert("path".into(), Value::String(path_arg(args)?.to_owned()));
	if let Some(depth) = unsigned(args, &["depth"])? {
		out.insert("depth".into(), json!(depth));
	}
	Ok(Value::Object(out))
}

fn translate_grep(args: &Map<String, Value>) -> Result<Value, BridgeError> {
	let pattern = string(args, &["pattern", "query"])?;
	let literal = args
		.get("literal")
		.and_then(Value::as_bool)
		.unwrap_or(false);
	let escaped = if literal {
		regex_escape(pattern)
	} else {
		pattern.to_owned()
	};
	let mut out = Map::from_iter([
		("pattern".into(), Value::String(escaped)),
		("path".into(), Value::String(path_arg(args)?.to_owned())),
	]);
	if let Some(skip) = unsigned(args, &["skip", "offset"])? {
		out.insert("skip".into(), json!(skip));
	}
	if let Some(case) = args
		.get("caseSensitive")
		.or_else(|| args.get("case"))
		.and_then(Value::as_bool)
	{
		out.insert("case".into(), json!(case));
	}
	Ok(Value::Object(out))
}

fn translate_shell(args: &Map<String, Value>) -> Result<Value, BridgeError> {
	let command = string(args, &["command", "script"])?;
	let mut out = Map::from_iter([("command".into(), Value::String(command.to_owned()))]);
	if let Some(cwd) = args.get("cwd").and_then(Value::as_str) {
		out.insert("cwd".into(), Value::String(cwd.to_owned()));
	}
	Ok(Value::Object(out))
}

fn translate_write(args: &Map<String, Value>) -> Result<Value, BridgeError> {
	Ok(json!({"path": path_arg(args)?, "content": string(args, &["content", "text"]) ?}))
}

fn translate_delete(args: &Map<String, Value>) -> Result<Value, BridgeError> {
	Ok(json!({"path": path_arg(args)?, "content": ""}))
}

fn translate_edit(args: &Map<String, Value>) -> Result<Value, BridgeError> {
	if let Some(edits) = args.get("edits") {
		return Ok(json!({"path": path_arg(args)?, "edits": edits}));
	}
	let mut out = Map::from_iter([
		("path".into(), Value::String(path_arg(args)?.to_owned())),
		(
			"old".into(),
			Value::String(
				string(
					args,
					&["old", "search", "old_string", "old_str", "old_text", "oldString", "oldText"],
				)?
				.to_owned(),
			),
		),
		(
			"new".into(),
			Value::String(
				string(
					args,
					&["new", "replace", "new_string", "new_str", "new_text", "newString", "newText"],
				)?
				.to_owned(),
			),
		),
	]);
	if let Some(replace_all) = args
		.get("replace_all")
		.or_else(|| args.get("replaceAll"))
		.and_then(Value::as_bool)
	{
		out.insert("replace_all".into(), Value::Bool(replace_all));
	}
	Ok(Value::Object(out))
}

const CURSOR_REPLACE_NAMES: [&str; 6] =
	["StrReplace", "str_replace", "strReplace", "SearchReplace", "search_replace", "Edit"];

fn cursor_replace_arguments<'a>(
	frame: &str,
	args: &'a Map<String, Value>,
) -> Option<&'a Map<String, Value>> {
	if CURSOR_REPLACE_NAMES.contains(&frame) {
		return Some(args);
	}
	if !matches!(frame, "mcp" | "piMcp") {
		return None;
	}
	let name = ["toolName", "tool_name", "name"]
		.into_iter()
		.find_map(|key| args.get(key).and_then(Value::as_str))?;
	let payload = args
		.get("args")
		.or_else(|| args.get("arguments"))
		.and_then(Value::as_object)
		.unwrap_or(args);
	if CURSOR_REPLACE_NAMES.contains(&name) {
		return Some(payload);
	}
	if name != "edit"
		|| payload
			.get("input")
			.or_else(|| payload.get("_input"))
			.and_then(Value::as_str)
			.is_some()
	{
		return None;
	}
	let has_old = ["old_string", "old_str", "old_text", "oldString", "oldText"]
		.into_iter()
		.any(|key| payload.get(key).and_then(Value::as_str).is_some());
	let has_new = ["new_string", "new_str", "new_text", "newString", "newText"]
		.into_iter()
		.any(|key| payload.get(key).and_then(Value::as_str).is_some());
	(has_old && has_new).then_some(payload)
}

fn translate_find(args: &Map<String, Value>) -> Result<Value, BridgeError> {
	Ok(json!({"path": string(args, &["path", "pattern", "glob"])?}))
}

fn path_arg(args: &Map<String, Value>) -> Result<&str, BridgeError> {
	string(args, &["path", "file", "directory"])
}
fn string<'a>(args: &'a Map<String, Value>, names: &[&str]) -> Result<&'a str, BridgeError> {
	names
		.iter()
		.find_map(|name| args.get(*name).and_then(Value::as_str))
		.ok_or_else(|| BridgeError::Invalid(Str::from(format!("missing string `{}`", names[0]))))
}
fn unsigned(args: &Map<String, Value>, names: &[&str]) -> Result<Option<u64>, BridgeError> {
	for name in names {
		if let Some(value) = args.get(*name) {
			return value
				.as_u64()
				.map(Some)
				.ok_or_else(|| BridgeError::Invalid(Str::from(format!("`{name}` must be unsigned"))));
		}
	}
	Ok(None)
}
fn regex_escape(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	for ch in input.chars() {
		if matches!(
			ch,
			'.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
		) {
			out.push('\\');
		}
		out.push(ch);
	}
	out
}

/// Validates a mutation target without following a final symlink.
pub fn validate_write(
	root: &Path,
	requested: &Path,
	policy: WritePolicy,
) -> Result<PathBuf, BridgeError> {
	if policy == WritePolicy::ReadOnly {
		return Err(BridgeError::WriteRejected(sf!("workspace is read-only")));
	}
	if requested
		.components()
		.any(|part| matches!(part, Component::ParentDir))
	{
		return Err(BridgeError::WriteRejected(sf!("parent traversal is forbidden")));
	}
	let root =
		fs::canonicalize(root).map_err(|source| BridgeError::Io { path: root.to_owned(), source })?;
	let joined = if requested.is_absolute() {
		requested.to_owned()
	} else {
		root.join(requested)
	};
	let relative = joined
		.strip_prefix(&root)
		.map_err(|_| BridgeError::WriteRejected(sf!("target escapes workspace")))?;
	let mut cursor = root.clone();
	let component_count = relative.components().count();
	for (index, component) in relative.components().enumerate() {
		if index + 1 == component_count {
			break;
		}
		cursor.push(component);
		let metadata = fs::symlink_metadata(&cursor)
			.map_err(|source| BridgeError::Io { path: cursor.clone(), source })?;
		if metadata.file_type().is_symlink() {
			return Err(BridgeError::WriteRejected(sf!("symlink path components are forbidden",)));
		}
		if !metadata.is_dir() {
			return Err(BridgeError::WriteRejected(sf!("write parent is not a directory",)));
		}
	}
	let parent = joined
		.parent()
		.ok_or_else(|| BridgeError::WriteRejected(sf!("target has no parent")))?;
	let parent = fs::canonicalize(parent)
		.map_err(|source| BridgeError::Io { path: parent.to_owned(), source })?;
	if !parent.starts_with(&root) {
		return Err(BridgeError::WriteRejected(sf!("target escapes workspace")));
	}
	let target = parent.join(
		joined
			.file_name()
			.ok_or_else(|| BridgeError::WriteRejected(sf!("target has no filename")))?,
	);
	match fs::symlink_metadata(&target) {
		Ok(metadata) => {
			if metadata.file_type().is_symlink() {
				return Err(BridgeError::WriteRejected(sf!("symlink targets are forbidden",)));
			}
			if !metadata.file_type().is_file() {
				return Err(BridgeError::WriteRejected(sf!("target is not a regular file",)));
			}
			#[cfg(unix)]
			{
				use std::os::unix::fs::MetadataExt as _;
				if metadata.nlink() != 1 {
					return Err(BridgeError::WriteRejected(sf!("hard-linked targets are forbidden",)));
				}
			}
		},
		Err(error)
			if error.kind() == std::io::ErrorKind::NotFound && policy == WritePolicy::Workspace => {},
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			return Err(BridgeError::WriteRejected(sf!("policy permits existing files only",)));
		},
		Err(source) => return Err(BridgeError::Io { path: target, source }),
	}
	Ok(target)
}

/// Stateful sanitizer for prefix-repeated shell output streams.
#[derive(Default)]
pub struct ShellDelta {
	previous:       String,
	pending_escape: String,
}
impl ShellDelta {
	/// Returns only newly appended, complete, terminal-safe text.
	pub fn push(&mut self, snapshot: &str) -> Str {
		let delta = snapshot.strip_prefix(&self.previous).unwrap_or(snapshot);
		self.previous.clear();
		self.previous.push_str(snapshot);
		let mut input = std::mem::take(&mut self.pending_escape);
		input.push_str(delta);
		let safe = complete_escape_prefix(&input);
		self.pending_escape.push_str(&input[safe..]);
		Str::from(input[..safe].replace('\0', ""))
	}
}
fn complete_escape_prefix(input: &str) -> usize {
	let bytes = input.as_bytes();
	let mut at = 0;
	let mut safe = 0;
	while at < bytes.len() {
		if bytes[at] != 0x1b {
			at += 1;
			safe = at;
			continue;
		}
		if at + 1 == bytes.len() {
			break;
		}
		if bytes[at + 1] != b'[' {
			at += 2;
			safe = at;
			continue;
		}
		let Some(end) = bytes[at + 2..]
			.iter()
			.position(|byte| (0x40..=0x7e).contains(byte))
		else {
			break;
		};
		at += end + 3;
		safe = at;
	}
	safe
}

/// Cursor todo projection with deterministic synthetic settlement on failure.
#[derive(Default)]
pub struct TodoSync {
	open: BTreeMap<Str, Value>,
}
impl TodoSync {
	/// Records a todo update associated with a tool call.
	pub fn update(&mut self, call_id: Str, todos: Value) -> Value {
		self.open.insert(call_id.clone(), todos.clone());
		json!({"type":"plan_update","callId":call_id,"todos":todos})
	}

	/// Settles a todo stream, synthesizing a failed terminal update when needed.
	pub fn settle(&mut self, call_id: &str, error: Option<&str>) -> Option<Value> {
		self.open.remove(call_id).map(|todos| json!({"type":"plan_update","callId":call_id,"todos":todos,"settled":true,"error":error}))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn modern_read_uses_dyn() {
		let call = translate("piRead", &json!({"path":"a","start":2,"lines":3})).unwrap();
		assert_eq!(call.tool.as_str(), "dyn");
		assert_eq!(call.arguments["do_"], "invoke/read");
	}
	#[test]
	fn cursor_str_replace_variants_project_onto_replace_edit() {
		for name in CURSOR_REPLACE_NAMES {
			let call = translate(
				name,
				&json!({
					"path": "src/lib.rs",
					"old_string": "before",
					"new_string": "after",
					"replace_all": true
				}),
			)
			.unwrap();
			assert_eq!(call.arguments["do_"], "invoke/edit", "{name}");
			assert_eq!(call.arguments["path"], "src/lib.rs", "{name}");
			assert_eq!(call.arguments["old"], "before", "{name}");
			assert_eq!(call.arguments["new"], "after", "{name}");
			assert_eq!(call.arguments["replace_all"], true, "{name}");
		}
	}
	#[test]
	fn cursor_mcp_replace_shape_uses_replace_but_hashline_stays_mcp() {
		let call = translate(
			"mcp",
			&json!({
				"tool_name": "edit",
				"args": {"path":"src/lib.rs","old_text":"a","newText":"b"}
			}),
		)
		.unwrap();
		assert_eq!(call.arguments["do_"], "invoke/edit");
		assert_eq!(call.arguments["old"], "a");
		assert_eq!(call.arguments["new"], "b");

		let call = translate(
			"mcp",
			&json!({"tool_name":"edit","args":{"path":"src/lib.rs","input":"hashline"}}),
		)
		.unwrap();
		assert_eq!(call.arguments["do_"], "invoke/mcp");
	}
	#[test]
	fn streaming_retains_partial_escape() {
		let mut stream = ShellDelta::default();
		assert_eq!(stream.push("ok\u{1b}[").as_str(), "ok");
		assert_eq!(stream.push("ok\u{1b}[31mred").as_str(), "\u{1b}[31mred");
	}
	#[test]
	fn checked_write_rejects_escape_and_read_only() {
		let root = tempfile::tempdir().unwrap();
		let args = json!({"path":"../outside","content":"x"});
		assert!(translate_checked("piWrite", &args, root.path(), WritePolicy::Workspace).is_err());
		let args = json!({"path":"inside","content":"x"});
		assert!(translate_checked("piWrite", &args, root.path(), WritePolicy::ReadOnly).is_err());
	}

	#[test]
	fn todo_errors_receive_synthetic_settlement() {
		let mut sync = TodoSync::default();
		sync.update(sf!("call"), json!([{"text":"work"}]));
		let settled = sync.settle("call", Some("failed")).unwrap();
		assert_eq!(settled["settled"], true);
		assert_eq!(settled["error"], "failed");
	}
}
