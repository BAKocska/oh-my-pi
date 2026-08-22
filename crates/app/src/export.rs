//! Durable session exports from one canonical live-journal projection.

use std::{
	fs,
	path::{Path, PathBuf},
};

use omp_storage::transcript::{
	codec,
	reader::{self, Entry},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const MAX_EMBEDDED_ARTIFACT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ExportError {
	#[error("failed to read session journal: {0}")]
	Journal(#[from] codec::Error),
	#[error("failed to read export input: {0}")]
	Io(#[from] std::io::Error),
	#[error("failed to serialize export: {0}")]
	Json(#[from] serde_json::Error),
	#[error("failed to serialize YAML export: {0}")]
	Yaml(#[from] serde_yaml::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTree {
	pub id:       String,
	pub created:  u64,
	pub entries:  Vec<Value>,
	pub children: Vec<SessionTree>,
}

impl SessionTree {
	/// Loads one durable journal and all nested journals below its sibling
	/// session directory.
	pub fn load(path: &Path) -> Result<Self, ExportError> {
		let canonical = fs::canonicalize(path)?;
		let log = reader::load(&canonical)?;
		let live = log.live();
		let mut entries = Vec::with_capacity(live.len());
		for index in live {
			if let Some(Entry::Ok(event)) = log.get(index) {
				let mut encoded = Vec::new();
				codec::write_line(event, &mut encoded)?;
				let mut value: Value = serde_json::from_slice(&encoded)?;
				sanitize_value(&mut value);
				if peer_visible(&value) {
					entries.push(value);
				}
			}
		}
		let mut children = Vec::new();
		let child_dir = canonical.with_extension("");
		if child_dir.is_dir() {
			let mut paths = fs::read_dir(&child_dir)?
				.filter_map(Result::ok)
				.map(|entry| entry.path())
				.filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
				.collect::<Vec<_>>();
			paths.sort_unstable();
			for path in paths {
				children.push(Self::load(&path)?);
			}
		}
		Ok(Self {
			id: log.header().id.as_str().to_string(),
			created: log.header().created,
			entries,
			children,
		})
	}
}

/// Peer messages are presentation data only when the durable event explicitly
/// marks them public.
fn peer_visible(value: &Value) -> bool {
	let Some(object) = value.as_object() else {
		return true;
	};
	let kind = object
		.get("kind")
		.and_then(Value::as_str)
		.unwrap_or_default();
	if !kind.contains("peer") {
		return true;
	}
	object.get("visibility").and_then(Value::as_str) == Some("public_presentation")
		|| object.get("peer_visible").and_then(Value::as_bool) == Some(true)
}

fn sanitize_value(value: &mut Value) {
	match value {
		Value::String(text) => {
			if Path::new(text.as_str()).is_absolute() {
				*text = Path::new(text.as_str())
					.file_name()
					.and_then(|v| v.to_str())
					.unwrap_or("[path]")
					.to_owned();
			}
		},
		Value::Array(values) => values.iter_mut().for_each(sanitize_value),
		Value::Object(object) => {
			for (key, value) in object {
				if matches!(
					key.as_str(),
					"cwd" | "sessionPath" | "session_path" | "previousSession" | "previous_session"
				) {
					*value = Value::Null;
				} else {
					sanitize_value(value);
				}
			}
		},
		_ => {},
	}
}

/// Resolves an artifact only when it remains under `root`, is a regular file,
/// and is bounded.
pub fn safe_artifact(root: &Path, relative: &Path) -> Result<Option<Vec<u8>>, std::io::Error> {
	if relative.is_absolute() {
		return Ok(None);
	}
	let root = fs::canonicalize(root)?;
	let path = fs::canonicalize(root.join(relative))?;
	if !path.starts_with(&root) {
		return Ok(None);
	}
	let metadata = fs::metadata(&path)?;
	if !metadata.is_file() || metadata.len() > MAX_EMBEDDED_ARTIFACT {
		return Ok(None);
	}
	fs::read(path).map(Some)
}

pub fn render_html(tree: &SessionTree) -> Result<String, ExportError> {
	let model = serde_json::to_string(tree)?.replace('<', "\\u003c");
	Ok(format!(
		r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>OMP session</title><style>:root{{color-scheme:light dark;--bg:oklch(98% .01 250);--fg:oklch(22% .02 250);--card:oklch(94% .015 250)}}@media(prefers-color-scheme:dark){{:root{{--bg:oklch(18% .02 250);--fg:oklch(92% .01 250);--card:oklch(25% .02 250)}}}}body{{margin:0;background:var(--bg);color:var(--fg);font:14px system-ui}}main{{max-width:1100px;margin:auto;padding:2rem}}input{{width:100%;padding:.7rem}}article{{white-space:pre-wrap;background:var(--card);padding:1rem;margin:.6rem 0;border-radius:.5rem}}button{{margin:.25rem}}</style></head><body><main><input id="q" placeholder="Search"><nav id="tree"></nav><section id="out"></section></main><script>const model={model};const out=document.querySelector('#out'),nav=document.querySelector('#tree'),q=document.querySelector('#q');let selected=model;function drawNav(n,d=0){{const b=document.createElement('button');b.textContent=' '.repeat(d)+n.id;b.onclick=()=>{{selected=n;draw()}};nav.append(b);n.children.forEach(c=>drawNav(c,d+1))}}function draw(){{out.replaceChildren();const needle=q.value.toLowerCase();selected.entries.forEach(e=>{{const s=JSON.stringify(e,null,2);if(s.toLowerCase().includes(needle)){{const a=document.createElement('article');a.textContent=s;out.append(a)}}}})}}q.oninput=draw;drawNav(model);draw();</script></body></html>"#
	))
}

pub fn render_yaml(tree: &SessionTree) -> Result<String, ExportError> {
	Ok(serde_yaml::to_string(tree)?)
}

pub fn render_markdown(tree: &SessionTree) -> String {
	fn append_tree(output: &mut String, tree: &SessionTree, depth: usize) {
		let heading = "#".repeat(depth.saturating_add(1).min(6));
		output.push_str(&format!("{heading} Session {}\n\n", tree.id));
		for entry in &tree.entries {
			let kind = entry.get("k").and_then(Value::as_str).unwrap_or("event");
			let mut text = Vec::new();
			collect_visible_text(entry, &mut text);
			if text.is_empty() {
				continue;
			}
			output.push_str(&format!("{} {}\n\n", "#".repeat((depth + 2).min(6)), kind));
			for value in text {
				output.push_str(value);
				output.push_str("\n\n");
			}
		}
		for child in &tree.children {
			append_tree(output, child, depth.saturating_add(1));
		}
	}

	let mut output = String::new();
	append_tree(&mut output, tree, 0);
	output
}

fn collect_visible_text<'a>(value: &'a Value, output: &mut Vec<&'a str>) {
	match value {
		Value::Array(values) => {
			for value in values {
				collect_visible_text(value, output);
			}
		},
		Value::Object(object) => {
			for (key, value) in object {
				if matches!(key.as_str(), "text" | "summary" | "short")
					&& let Some(text) = value.as_str()
					&& !text.trim().is_empty()
				{
					output.push(text);
				} else if !matches!(
					key.as_str(),
					"thinking" | "signature" | "raw" | "provider_metadata"
				) {
					collect_visible_text(value, output);
				}
			}
		},
		_ => {},
	}
}

/// Output format for a durable session export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportFormat {
	/// Live-projection YAML.
	Yaml,
	/// Concise Markdown.
	Markdown,
	/// Themed self-contained HTML.
	Html,
}

pub fn export_session(path: &Path, output: &Path) -> Result<PathBuf, ExportError> {
	export_session_as(path, output, ExportFormat::Html)
}

pub fn export_session_as(
	path: &Path,
	output: &Path,
	format: ExportFormat,
) -> Result<PathBuf, ExportError> {
	let tree = SessionTree::load(path)?;
	let rendered = match format {
		ExportFormat::Yaml => render_yaml(&tree)?,
		ExportFormat::Markdown => render_markdown(&tree),
		ExportFormat::Html => render_html(&tree)?,
	};
	fs::write(output, rendered)?;
	Ok(output.to_owned())
}
