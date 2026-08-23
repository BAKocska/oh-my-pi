//! Static native custom-tool discovery and lowering.
//!
//! Discovery reads declarations only. Python modules and process handlers are
//! activated later by the owning supervised extension worker or Environment.

use std::{
	collections::{BTreeMap, BTreeSet},
	fs, io,
	path::{Component, Path, PathBuf},
};

use omp_core::Str;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use thiserror::Error;

use super::manifest::{ToolHandlerDeclaration, ToolPayload};

/// Native source tier for deterministic tool precedence.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ToolSourceTier {
	/// Project-authored `.omp/tools`.
	Project,
	/// User-authored native config tools.
	User,
	/// Installed signed native package tools.
	Package,
}

impl ToolSourceTier {
	const fn priority(self) -> u8 {
		match self {
			Self::Project => 3,
			Self::User => 2,
			Self::Package => 1,
		}
	}
}

/// One static custom-tool discovery root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRoot {
	/// Canonical native root containing tool files.
	pub path:         PathBuf,
	/// Source precedence tier.
	pub tier:         ToolSourceTier,
	/// Owning extension identity for package roots.
	pub extension_id: Option<Str>,
}

/// Non-fatal malformed declaration evidence.
#[derive(Debug)]
pub struct ToolWarning {
	/// Source path.
	pub path:  PathBuf,
	/// Typed reason.
	pub error: ToolDiscoveryError,
}

/// Winning static custom tools plus skipped-source diagnostics.
#[derive(Debug, Default)]
pub struct CustomToolDiscovery {
	/// First winner for each tool name, sorted by name.
	pub tools:    BTreeMap<Str, ToolPayload>,
	/// Malformed or duplicate declarations.
	pub warnings: Vec<ToolWarning>,
}

/// Fail-closed static custom-tool declaration error.
#[derive(Debug, Error)]
pub enum ToolDiscoveryError {
	/// Source could not be read.
	#[error("custom tool source could not be read")]
	Io(#[source] io::Error),
	/// JSON declaration was malformed.
	#[error("custom tool JSON declaration is malformed")]
	Json(#[source] serde_json::Error),
	/// Markdown frontmatter was malformed.
	#[error("custom tool Markdown frontmatter is malformed")]
	Yaml(#[source] serde_yaml::Error),
	/// A declaration omitted a required field.
	#[error("custom tool declaration is missing {0}")]
	Missing(&'static str),
	/// Tool name is outside the native identifier vocabulary.
	#[error("custom tool name is invalid")]
	InvalidName,
	/// Input schema is not a frozen local JSON Schema object.
	#[error("custom tool input schema is not a frozen local JSON Schema")]
	InvalidSchema,
	/// Handler escaped its declaration root.
	#[error("custom tool handler escapes its native root")]
	EscapedHandler,
	/// A higher-priority source already claimed the tool name.
	#[error("custom tool name is already claimed")]
	Duplicate,
}

#[derive(Debug, Deserialize)]
struct JsonTool {
	name:         Option<Str>,
	description:  Option<Str>,
	#[serde(default, alias = "inputSchema", alias = "parameters")]
	input_schema: Option<Value>,
	handler:      Option<JsonHandler>,
	module:       Option<Str>,
	callable:     Option<Str>,
	program:      Option<PathBuf>,
	#[serde(default)]
	args:         Vec<Str>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum JsonHandler {
	Python {
		module:   Str,
		callable: Str,
	},
	Process {
		program: PathBuf,
		#[serde(default)]
		args:    Vec<Str>,
	},
}

/// Scans native roots without importing or executing any source. Canonical
/// paths are deduplicated before parsing; source priority and lexical path
/// order make name collisions deterministic.
pub fn discover(roots: impl IntoIterator<Item = ToolRoot>) -> CustomToolDiscovery {
	let mut roots = roots.into_iter().collect::<Vec<_>>();
	roots.sort_by(|left, right| {
		right
			.tier
			.priority()
			.cmp(&left.tier.priority())
			.then_with(|| left.path.cmp(&right.path))
	});
	let mut seen_paths = BTreeSet::new();
	let mut output = CustomToolDiscovery::default();
	for root in roots {
		for path in tool_files(&root.path) {
			let canonical = match path.canonicalize() {
				Ok(path) => path,
				Err(error) => {
					output
						.warnings
						.push(ToolWarning { path, error: ToolDiscoveryError::Io(error) });
					continue;
				},
			};
			if !canonical.starts_with(
				root
					.path
					.canonicalize()
					.unwrap_or_else(|_| root.path.clone()),
			) || !seen_paths.insert(canonical.clone())
			{
				continue;
			}
			match load_tool(&root, &canonical) {
				Ok(tool) if output.tools.contains_key(&tool.name) => output
					.warnings
					.push(ToolWarning { path: canonical, error: ToolDiscoveryError::Duplicate }),
				Ok(tool) => {
					output.tools.insert(tool.name.clone(), tool);
				},
				Err(error) => output.warnings.push(ToolWarning { path: canonical, error }),
			}
		}
	}
	output
}

fn tool_files(root: &Path) -> Vec<PathBuf> {
	if !root.is_dir() {
		return Vec::new();
	}
	let mut pending = vec![root.to_path_buf()];
	let mut files = Vec::new();
	while let Some(directory) = pending.pop() {
		let Ok(entries) = fs::read_dir(directory) else {
			continue;
		};
		for entry in entries.filter_map(Result::ok) {
			let path = entry.path();
			if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
				pending.push(path);
				continue;
			}
			if path
				.extension()
				.and_then(|extension| extension.to_str())
				.is_some_and(|extension| {
					matches!(extension.to_ascii_lowercase().as_str(), "json" | "md" | "py" | "sh")
				}) {
				files.push(path);
			}
		}
	}
	files.sort();
	files
}

fn load_tool(root: &ToolRoot, path: &Path) -> Result<ToolPayload, ToolDiscoveryError> {
	let extension = path
		.extension()
		.and_then(|extension| extension.to_str())
		.unwrap_or_default();
	match extension.to_ascii_lowercase().as_str() {
		"json" => load_json(root, path),
		"md" => load_markdown(root, path),
		"py" => load_python(root, path),
		"sh" => load_process(path),
		_ => Err(ToolDiscoveryError::Missing("supported extension")),
	}
}

fn load_json(root: &ToolRoot, path: &Path) -> Result<ToolPayload, ToolDiscoveryError> {
	let bytes = fs::read(path).map_err(ToolDiscoveryError::Io)?;
	let declaration: JsonTool = serde_json::from_slice(&bytes).map_err(ToolDiscoveryError::Json)?;
	lower(root, path, declaration, None)
}

fn load_markdown(root: &ToolRoot, path: &Path) -> Result<ToolPayload, ToolDiscoveryError> {
	let text = fs::read_to_string(path).map_err(ToolDiscoveryError::Io)?;
	let (frontmatter, body) = markdown_parts(&text)?;
	let declaration: JsonTool =
		serde_yaml::from_str(frontmatter).map_err(ToolDiscoveryError::Yaml)?;
	lower(
		root,
		path,
		declaration,
		body
			.lines()
			.find(|line| !line.trim().is_empty())
			.map(str::trim),
	)
}

fn markdown_parts(text: &str) -> Result<(&str, &str), ToolDiscoveryError> {
	let rest = text
		.strip_prefix("---\n")
		.ok_or(ToolDiscoveryError::Missing("frontmatter"))?;
	let (frontmatter, body) = rest
		.split_once("\n---")
		.ok_or(ToolDiscoveryError::Missing("frontmatter fence"))?;
	Ok((frontmatter, body.trim_start_matches(|character| matches!(character, '\r' | '\n'))))
}

fn load_python(root: &ToolRoot, path: &Path) -> Result<ToolPayload, ToolDiscoveryError> {
	let relative = path
		.strip_prefix(
			root
				.path
				.canonicalize()
				.unwrap_or_else(|_| root.path.clone()),
		)
		.map_err(|_| ToolDiscoveryError::EscapedHandler)?;
	let mut components = relative
		.components()
		.filter_map(|component| match component {
			Component::Normal(value) => value.to_str(),
			_ => None,
		})
		.collect::<Vec<_>>();
	let stem = path
		.file_stem()
		.and_then(|stem| stem.to_str())
		.ok_or(ToolDiscoveryError::InvalidName)?;
	if let Some(last) = components.last_mut() {
		*last = stem;
	}
	let module = components.join(".");
	let name = stem.replace('_', "-");
	validate_name(&name)?;
	Ok(ToolPayload {
		name:         Str::new(&name),
		path:         path.to_path_buf(),
		description:  Str::new(format!("Native custom tool {name}")),
		input_schema: empty_schema(),
		handler:      ToolHandlerDeclaration::Python {
			module:   Str::new(module),
			callable: Str::new_static("run"),
		},
	})
}

fn load_process(path: &Path) -> Result<ToolPayload, ToolDiscoveryError> {
	let name = path
		.file_stem()
		.and_then(|stem| stem.to_str())
		.ok_or(ToolDiscoveryError::InvalidName)?;
	validate_name(name)?;
	Ok(ToolPayload {
		name:         Str::new(name),
		path:         path.to_path_buf(),
		description:  Str::new(format!("Native custom tool {name}")),
		input_schema: empty_schema(),
		handler:      ToolHandlerDeclaration::Process {
			program: path.to_path_buf(),
			args:    Vec::new(),
		},
	})
}

fn lower(
	root: &ToolRoot,
	path: &Path,
	declaration: JsonTool,
	body_description: Option<&str>,
) -> Result<ToolPayload, ToolDiscoveryError> {
	let name = declaration.name.unwrap_or_else(|| {
		Str::new(
			path
				.file_stem()
				.and_then(|stem| stem.to_str())
				.unwrap_or_default(),
		)
	});
	validate_name(&name)?;
	let schema = declaration.input_schema.unwrap_or_else(empty_schema);
	validate_schema(&schema)?;
	let handler = match declaration.handler {
		Some(JsonHandler::Python { module, callable }) => {
			ToolHandlerDeclaration::Python { module, callable }
		},
		Some(JsonHandler::Process { program, args }) => {
			ToolHandlerDeclaration::Process { program: contained_program(root, program)?, args }
		},
		None if declaration.module.is_some() => ToolHandlerDeclaration::Python {
			module:   declaration.module.expect("checked"),
			callable: declaration
				.callable
				.unwrap_or_else(|| Str::new_static("run")),
		},
		None if declaration.program.is_some() => ToolHandlerDeclaration::Process {
			program: contained_program(root, declaration.program.expect("checked"))?,
			args:    declaration.args,
		},
		None => return Err(ToolDiscoveryError::Missing("handler")),
	};
	Ok(ToolPayload {
		name,
		path: path.to_path_buf(),
		description: declaration
			.description
			.or_else(|| body_description.map(Str::new))
			.ok_or(ToolDiscoveryError::Missing("description"))?,
		input_schema: schema,
		handler,
	})
}

fn contained_program(root: &ToolRoot, program: PathBuf) -> Result<PathBuf, ToolDiscoveryError> {
	let candidate = if program.is_absolute() {
		program
	} else {
		root.path.join(program)
	};
	let canonical = candidate.canonicalize().map_err(ToolDiscoveryError::Io)?;
	let canonical_root = root.path.canonicalize().map_err(ToolDiscoveryError::Io)?;
	canonical
		.starts_with(canonical_root)
		.then_some(canonical)
		.ok_or(ToolDiscoveryError::EscapedHandler)
}

fn validate_name(name: &str) -> Result<(), ToolDiscoveryError> {
	if name.is_empty()
		|| name.starts_with('-')
		|| !name
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
	{
		return Err(ToolDiscoveryError::InvalidName);
	}
	Ok(())
}

fn validate_schema(schema: &Value) -> Result<(), ToolDiscoveryError> {
	let Some(object) = schema.as_object() else {
		return Err(ToolDiscoveryError::InvalidSchema);
	};
	if object
		.get("type")
		.and_then(Value::as_str)
		.is_some_and(|kind| kind != "object")
		|| contains_remote_ref(schema)
	{
		return Err(ToolDiscoveryError::InvalidSchema);
	}
	Ok(())
}

fn contains_remote_ref(value: &Value) -> bool {
	match value {
		Value::Object(object) => object.iter().any(|(key, value)| {
			(key == "$ref"
				&& value
					.as_str()
					.is_some_and(|reference| !reference.starts_with('#')))
				|| contains_remote_ref(value)
		}),
		Value::Array(values) => values.iter().any(contains_remote_ref),
		_ => false,
	}
}

fn empty_schema() -> Value {
	let mut schema = Map::new();
	schema.insert("type".to_owned(), json!("object"));
	schema.insert("properties".to_owned(), Value::Object(Map::new()));
	schema.insert("additionalProperties".to_owned(), Value::Bool(false));
	Value::Object(schema)
}
