//! Multi-target structural search with stable pagination and hashline
//! locations.

use std::{fmt, path::PathBuf, sync::Arc};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, DocEffects, Effects, Ev, IncomingParams,
	ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	#[schemars(with = "String")]
	pub pat:    Str,
	#[serde(default)]
	#[schemars(with = "Option<String>")]
	pub path:   Option<Str>,
	#[serde(default)]
	pub cursor: usize,
	#[serde(default)]
	pub limit:  Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Match {
	pub path:       Str,
	pub line:       usize,
	pub column:     usize,
	pub end_line:   usize,
	pub end_column: usize,
	pub text:       Str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Advisory {
	pub path:    Str,
	pub message: Str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Payload {
	pub matches:     Vec<Match>,
	pub advisories:  Vec<Advisory>,
	pub total:       usize,
	pub next_cursor: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fault {
	message: Str,
}
impl fmt::Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}
impl std::error::Error for Fault {}

pub struct AstGrep {
	root: PathBuf,
	spec: ToolSpec,
}

pub fn tool(root: PathBuf) -> AstGrep {
	AstGrep {
		root,
		spec: ToolSpec {
			name:            sf!("ast_grep"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Searches multiple files structurally with ast-grep metavariables. `path` accepts \
				 semicolon-separated files, directories, and globs. Results use stable path/source \
				 ordering; `cursor` resumes pagination."
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects {
				documents: Some(DocEffects { read: true, write_globs: Arc::default() }),
				exec:      None,
				inference: None,
				desktop:   None,
				subagents: 0,
			},
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("ast_grep.rs"),
			)
			.into(),
		},
	}
}

impl Tool for AstGrep {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, Payload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<Params>().await { Ok(v) => v, Err(e) => { yield param_event(e); return; } };
			if params.pat.trim().is_empty() { yield done(Err(Fault { message: sf!("pat must not be empty") })); return; }
			if let Err(error) = incoming.interruptable().committed().await { yield commit_event(error); return; }
			let targets = params.path.as_deref().unwrap_or(".").split(';').map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect::<Vec<_>>();
			let files = match omp_ast::ops::collect_matched_files(&self.root, &targets) { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
			let mut matches = Vec::new();
			let mut advisories = Vec::new();
			for file in files {
				let language = match omp_ast::ops::resolve_language(None, &file.absolute_path) { Ok(v) => v, Err(e) => { advisories.push(Advisory { path: file.relative_path, message: Str::new(e.to_string()) }); continue; } };
				let patterns = match omp_ast::ops::compile_search_patterns(&params.pat, language) { Ok(v) => v, Err(e) => { advisories.push(Advisory { path: file.relative_path, message: Str::new(e.to_string()) }); continue; } };
				let source = match std::fs::read_to_string(&file.absolute_path) { Ok(v) => v, Err(e) => { advisories.push(Advisory { path: file.relative_path, message: Str::new(e.to_string()) }); continue; } };
				for found in omp_ast::ops::collect_matches(&source, language, &patterns) {
					matches.push(Match { path: file.relative_path.clone(), line: found.line, column: found.column, end_line: found.end_line, end_column: found.end_column, text: found.text });
				}
			}
			matches.sort_unstable_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)).then(a.column.cmp(&b.column)));
			let total = matches.len();
			let start = params.cursor.min(total);
			let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
			let end = start.saturating_add(limit).min(total);
			let page = matches.drain(start..end).collect();
			yield done(Ok(Payload { matches: page, advisories, total, next_cursor: (end < total).then_some(end) }));
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		let text = match view {
			Err(e) => Str::new(e.to_string()),
			Ok(payload) => {
				let mut out = String::new();
				for found in &payload.matches {
					use std::fmt::Write as _;
					let _ =
						writeln!(out, "{}:{}:{}\n{}", found.path, found.line, found.column, found.text);
				}
				for advisory in &payload.advisories {
					use std::fmt::Write as _;
					let _ = writeln!(out, "[advisory {}] {}", advisory.path, advisory.message);
				}
				if let Some(cursor) = payload.next_cursor {
					use std::fmt::Write as _;
					let _ = writeln!(out, "[next cursor: {cursor}; total: {}]", payload.total);
				}
				Str::new(out)
			},
		};
		vec![Part::Text { text }]
	}
}
fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
	Ev::Done(ToolTerminal::Done {
		useless: result.as_ref().is_ok_and(|p| p.matches.is_empty()),
		result,
	})
}
fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
		ParamError::Args(v) => Ev::Args(*v),
		ParamError::Interrupted(v) => Ev::Aborted(Abort::Interrupted { reason: v.reason }),
		ParamError::Protocol(v) => Ev::Args(issue(v)),
	}
}
fn commit_event(error: CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(v) => Ev::Aborted(Abort::Interrupted { reason: v.reason }),
		CommitError::Protocol(v) => Ev::Args(issue(v)),
	}
}
fn issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  None,
		found:    Some(message),
	}
}
