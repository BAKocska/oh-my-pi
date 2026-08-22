//! Durable session exports from one canonical live-journal projection.

use std::{
	fs,
	path::{Path, PathBuf},
};

use omp_storage::{
	index::{SessionIndex, SessionInfo},
	transcript::{
		SessionId, codec,
		reader::{self, Entry},
	},
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
	#[error("failed to query durable session lineage: {0}")]
	Index(#[from] omp_storage::index::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTree {
	pub id:       String,
	pub created:  u64,
	pub entries:  Vec<Value>,
	pub children: Vec<SessionTree>,
}

impl SessionTree {
	/// Loads one durable journal and nested children from authoritative lineage
	/// metadata.
	pub fn load(path: &Path) -> Result<Self, ExportError> {
		let canonical = fs::canonicalize(path)?;
		let mut root = Self::load_one(&canonical)?;
		let sessions_dir = canonical.parent().unwrap_or_else(|| Path::new("."));
		let index_path = sessions_dir
			.parent()
			.unwrap_or(sessions_dir)
			.join("sessions.sqlite3");
		if index_path.is_file() {
			let index = SessionIndex::open_authoritative_reader(index_path)?;
			let root_id = SessionId(omp_core::Str::new(root.id.as_str()));
			let lineage = index.subagent_tree(&root_id)?;
			root.children = load_indexed_children(&root_id, &lineage, sessions_dir)?;
		}
		Ok(root)
	}

	fn load_one(canonical: &Path) -> Result<Self, ExportError> {
		let log = reader::load(canonical)?;
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
		Ok(Self {
			id: log.header().id.as_str().to_string(),
			created: log.header().created,
			entries,
			children: Vec::new(),
		})
	}
}

fn load_indexed_children(
	parent: &SessionId,
	lineage: &[SessionInfo],
	sessions_dir: &Path,
) -> Result<Vec<SessionTree>, ExportError> {
	let mut children = Vec::new();
	for session in lineage
		.iter()
		.filter(|session| session.parent.as_ref() == Some(parent))
	{
		let path = sessions_dir.join(format!("{}.jsonl", session.id.0));
		if !path.is_file() {
			continue;
		}
		let mut child = SessionTree::load_one(&path)?;
		child.children = load_indexed_children(&session.id, lineage, sessions_dir)?;
		children.push(child);
	}
	Ok(children)
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

/// HTML export palette derived from the active semantic TUI theme.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HtmlThemePalette {
	foreground: String,
	background: String,
	card:       String,
	border:     String,
	accent:     String,
	muted:      String,
	error:      String,
}

impl HtmlThemePalette {
	/// Creates a palette from browser CSS color values.
	pub fn new(
		foreground: impl Into<String>,
		background: impl Into<String>,
		card: impl Into<String>,
		border: impl Into<String>,
		accent: impl Into<String>,
		muted: impl Into<String>,
		error: impl Into<String>,
	) -> Self {
		Self {
			foreground: foreground.into(),
			background: background.into(),
			card:       card.into(),
			border:     border.into(),
			accent:     accent.into(),
			muted:      muted.into(),
			error:      error.into(),
		}
	}
}

impl Default for HtmlThemePalette {
	fn default() -> Self {
		Self::new("#c8ccd4", "#0c0f12", "#3a3f4b", "#454b58", "#61afef", "#5c6370", "#e06c75")
	}
}

/// Renders with the default semantic TUI theme.
pub fn render_html(tree: &SessionTree) -> Result<String, ExportError> {
	render_html_with_palette(tree, &HtmlThemePalette::default())
}

/// Renders a self-contained viewer using the supplied CSS palette.
pub fn render_html_with_palette(
	tree: &SessionTree,
	palette: &HtmlThemePalette,
) -> Result<String, ExportError> {
	let model = serde_json::to_string(tree)?.replace('<', "\\u003c");
	let mut html = String::with_capacity(model.len().saturating_add(16 * 1024));
	html.push_str(
		"<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" \
		 content=\"width=device-width,initial-scale=1\"><title>OMP \
		 session</title><style>:root{color-scheme:light dark;",
	);
	for (name, value) in [
		("--fg", palette.foreground.as_str()),
		("--bg", palette.background.as_str()),
		("--card", palette.card.as_str()),
		("--border", palette.border.as_str()),
		("--accent", palette.accent.as_str()),
		("--muted", palette.muted.as_str()),
		("--error", palette.error.as_str()),
	] {
		html.push_str(name);
		html.push(':');
		html.push_str(value);
		html.push(';');
	}
	html.push_str(EXPORT_STYLE);
	html.push_str(
		"</style></head><body><header><strong>OMP session export</strong><input id=\"q\" \
		 type=\"search\" placeholder=\"Search transcript\" aria-label=\"Search transcript\"><select \
		 id=\"theme\" aria-label=\"Theme\"><option value=\"auto\">Auto</option><option \
		 value=\"light\">Light</option><option value=\"dark\">Dark</option></select></header><div \
		 id=\"filters\" role=\"group\" aria-label=\"Entry filters\"></div><main><aside \
		 id=\"sidebar\"><nav id=\"tree\" aria-label=\"Session tree\"></nav></aside><div id=\"grip\" \
		 title=\"Drag to resize sidebar\" role=\"separator\" \
		 aria-orientation=\"vertical\"></div><section id=\"out\" \
		 aria-live=\"polite\"></section></main><dialog id=\"lightbox\"><button \
		 id=\"close-lightbox\" aria-label=\"Close image\">Close</button><img alt=\"Expanded \
		 attachment\"></dialog><script>const model=",
	);
	html.push_str(&model);
	html.push(';');
	html.push_str(EXPORT_SCRIPT);
	html.push_str("</script></body></html>");
	Ok(html)
}

const EXPORT_STYLE: &str = r#"}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:14px ui-sans-serif,system-ui,sans-serif}
body[data-theme=light]{filter:none;background:oklch(98% .01 250);color:oklch(22% .02 250)}
body[data-theme=dark]{filter:none}header{position:sticky;top:0;z-index:3;display:grid;grid-template-columns:auto minmax(12rem,1fr) auto;gap:.8rem;align-items:center;padding:.8rem 1rem;background:var(--card);border-bottom:1px solid var(--border)}
input,select,button{font:inherit;color:inherit;background:var(--bg);border:1px solid var(--border);border-radius:.4rem;padding:.5rem .65rem}button{cursor:pointer}button:hover{border-color:var(--accent)}
#filters{display:flex;gap:.5rem;overflow:auto;padding:.55rem 1rem;border-bottom:1px solid var(--border)}#filters label{white-space:nowrap}
main{display:grid;grid-template-columns:var(--sidebar,18rem) .4rem minmax(0,1fr);height:calc(100vh - 7.4rem)}
#sidebar{overflow:auto;padding:1rem;border-right:1px solid var(--border)}#tree button{display:block;width:100%;text-align:left;margin:.15rem 0;background:transparent}
#grip{cursor:col-resize;background:var(--border)}#grip:hover{background:var(--accent)}#out{overflow:auto;padding:1rem 1.5rem}
article{position:relative;background:var(--card);border:1px solid var(--border);border-radius:.65rem;padding:1rem;margin:0 0 .8rem;overflow-wrap:anywhere}
article.tool{border-left:4px solid var(--accent)}article h1,article h2,article h3{margin:.4rem 0}pre,code{font-family:ui-monospace,SFMono-Regular,monospace}pre{white-space:pre-wrap;background:var(--bg);padding:.75rem;border-radius:.4rem;overflow:auto}
.tok-key{color:var(--accent)}.tok-string{color:oklch(78% .14 145)}.tok-number{color:oklch(75% .14 65)}.muted{color:var(--muted)}
img.attachment{max-width:min(100%,42rem);max-height:22rem;cursor:zoom-in;border-radius:.4rem}dialog{padding:1rem;background:var(--bg);color:var(--fg);border:1px solid var(--border)}dialog img{display:block;max-width:90vw;max-height:85vh;margin-top:.6rem}
mark{background:color-mix(in oklch,var(--accent) 35%,transparent);color:inherit}
@media(max-width:700px){header{grid-template-columns:1fr auto}header strong{display:none}main{display:block;height:auto}#sidebar{max-height:12rem;border-right:0;border-bottom:1px solid var(--border)}#grip{display:none}#out{padding:.8rem}}
"#;

const EXPORT_SCRIPT: &str = r#"
const out=document.querySelector('#out'),nav=document.querySelector('#tree'),q=document.querySelector('#q'),filters=document.querySelector('#filters'),theme=document.querySelector('#theme'),grip=document.querySelector('#grip'),lightbox=document.querySelector('#lightbox');
let selected=model,enabled=new Set(),kinds=[];
const kindOf=e=>String(e.k??e.kind??e.type??'event');
const esc=s=>s.replace(/[&<>\"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','\"':'&quot;',\"'\":'&#39;'}[c]));
function visibleText(v,parts=[]){if(Array.isArray(v))v.forEach(x=>visibleText(x,parts));else if(v&&typeof v==='object')Object.entries(v).forEach(([k,x])=>{if(['thinking','signature','raw','provider_metadata'].includes(k))return;if(['text','summary','short','content'].includes(k)&&typeof x==='string')parts.push(x);else visibleText(x,parts)});return parts.join('\\n\\n')}
function inline(s){return esc(s).replace(/`([^`]+)`/g,'<code>$1</code>').replace(/\\*\\*([^*]+)\\*\\*/g,'<strong>$1</strong>').replace(/\\b(https?:\\/\\/[^\\s<]+)/g,'<a href=\"$1\" rel=\"noreferrer\">$1</a>')}
function markdown(text){const lines=text.split('\\n'),html=[];let fence=false,code=[];for(const line of lines){if(line.startsWith('```')){if(fence){html.push('<pre><code>'+highlight(code.join('\\n'))+'</code></pre>');code=[]}fence=!fence;continue}if(fence){code.push(line);continue}const h=line.match(/^(#{1,3})\\s+(.+)/);if(h)html.push(`<h${h[1].length}>${inline(h[2])}</h${h[1].length}>`);else if(line.trim())html.push('<p>'+inline(line)+'</p>')}if(code.length)html.push('<pre><code>'+highlight(code.join('\\n'))+'</code></pre>');return html.join('')}
function highlight(text){return esc(text).replace(/(&quot;.*?&quot;|'.*?')/g,'<span class=\"tok-string\">$1</span>').replace(/\\b(\\d+(?:\\.\\d+)?)\\b/g,'<span class=\"tok-number\">$1</span>').replace(/\\b(fn|function|const|let|struct|enum|impl|pub|async|await|return|if|else|match)\\b/g,'<span class=\"tok-key\">$1</span>')}
function images(v,into=[]){if(typeof v==='string'&&v.startsWith('data:image/'))into.push(v);else if(Array.isArray(v))v.forEach(x=>images(x,into));else if(v&&typeof v==='object')Object.values(v).forEach(x=>images(x,into));return into}
function collectKinds(n,set=new Set()){n.entries.forEach(e=>set.add(kindOf(e)));n.children.forEach(c=>collectKinds(c,set));return [...set].sort()}
function buildFilters(){kinds=collectKinds(model);enabled=new Set(kinds);filters.replaceChildren();for(const kind of kinds){const label=document.createElement('label'),box=document.createElement('input');box.type='checkbox';box.checked=true;box.onchange=()=>{box.checked?enabled.add(kind):enabled.delete(kind);draw()};label.append(box,document.createTextNode(kind));filters.append(label)}}
function drawNav(n,d=0){const b=document.createElement('button');b.textContent='  '.repeat(d)+n.id;b.onclick=()=>{selected=n;draw()};nav.append(b);n.children.forEach(c=>drawNav(c,d+1))}
function draw(){out.replaceChildren();const needle=q.value.toLocaleLowerCase();for(const e of selected.entries){const kind=kindOf(e),search=JSON.stringify(e).toLocaleLowerCase();if(!enabled.has(kind)||!search.includes(needle))continue;const article=document.createElement('article');if(kind.toLowerCase().includes('tool'))article.classList.add('tool');const title=document.createElement('div');title.className='muted';title.textContent=kind;article.append(title);const text=visibleText(e);if(text)article.insertAdjacentHTML('beforeend',markdown(text));else{const pre=document.createElement('pre');pre.innerHTML=highlight(JSON.stringify(e,null,2));article.append(pre)}for(const src of images(e)){const img=document.createElement('img');img.src=src;img.className='attachment';img.alt='Session attachment';img.onclick=()=>{lightbox.querySelector('img').src=src;lightbox.showModal()};article.append(img)}out.append(article)}}
const savedTheme=localStorage.getItem('omp-export-theme')||'auto';theme.value=savedTheme;document.body.dataset.theme=savedTheme;theme.onchange=()=>{document.body.dataset.theme=theme.value;localStorage.setItem('omp-export-theme',theme.value)};
q.oninput=draw;document.querySelector('#close-lightbox').onclick=()=>lightbox.close();lightbox.onclick=e=>{if(e.target===lightbox)lightbox.close()};
let dragging=false;grip.onpointerdown=e=>{dragging=true;grip.setPointerCapture(e.pointerId)};grip.onpointermove=e=>{if(dragging)document.documentElement.style.setProperty('--sidebar',Math.max(160,Math.min(innerWidth*.6,e.clientX))+'px')};grip.onpointerup=()=>dragging=false;
buildFilters();drawNav(model);draw();
"#;

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
