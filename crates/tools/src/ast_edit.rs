//! Multi-file structural rewrites with dry-run validation and recovery
//! snapshots.

use std::{
	collections::HashSet,
	fmt,
	path::{Path, PathBuf},
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use futures::Stream;
use omp_core::{Hash32, Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, DocEffects, Effects, Ev, IncomingParams,
	ParamError, Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MAX_FILES: usize = 200;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RewriteOp {
	#[schemars(with = "String")]
	pub pat: Str,
	#[schemars(with = "String")]
	pub out: Str,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	pub ops:   Vec<RewriteOp>,
	#[schemars(with = "Vec<String>")]
	pub paths: Vec<Str>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangedFile {
	pub path:         Str,
	pub replacements: u32,
	pub before_hash:  Str,
	pub after_hash:   Str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Advisory {
	pub path:    Str,
	pub message: Str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Payload {
	pub files:         Vec<ChangedFile>,
	pub advisories:    Vec<Advisory>,
	pub recovery_root: Option<Str>,
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

pub struct AstEdit {
	root: PathBuf,
	spec: ToolSpec,
}

pub fn tool(root: PathBuf) -> AstEdit {
	AstEdit {
		root,
		spec: ToolSpec {
			name:            sf!("ast_edit"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Applies structural ast-grep rewrites across mixed-language targets. Every rewrite is \
				 dry-run first; duplicate patterns and more than 200 files are rejected. Source \
				 hashes are rechecked immediately before an all-file commit, and recovery snapshots \
				 are retained under the project .omp state."
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects {
				documents: Some(DocEffects {
					read:        true,
					write_globs: [sf!("**")].into_iter().collect::<Arc<_>>(),
				}),
				exec:      None,
				inference: None,
				desktop:   None,
				subagents: 0,
			},
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("ast_edit.rs"),
			)
			.into(),
		},
	}
}

struct Prepared {
	absolute:     PathBuf,
	relative:     Str,
	original:     Vec<u8>,
	updated:      String,
	replacements: u32,
	before:       [u8; 32],
	after:        [u8; 32],
}

impl Tool for AstEdit {
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
			if params.ops.is_empty() || params.paths.is_empty() { yield done(Err(fault("ops and paths must not be empty"))); return; }
			let mut unique = HashSet::with_capacity(params.ops.len());
			if params.ops.iter().any(|op| op.pat.trim().is_empty() || !unique.insert(op.pat.clone())) { yield done(Err(fault("rewrite patterns must be non-empty and unique"))); return; }
			if let Err(error) = incoming.interruptable().committed().await { yield commit_event(error); return; }
			let target_patterns = params.paths.iter().map(ToString::to_string).collect::<Vec<_>>();
			let files = match omp_ast::ops::collect_matched_files(&self.root, &target_patterns) { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
			if files.len() > MAX_FILES { yield done(Err(fault("ast_edit target exceeds the 200-file hard cap"))); return; }
			let root = match self.root.canonicalize() { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
			let mut prepared = Vec::new(); let mut advisories = Vec::new();
			for file in files {
				let absolute = match file.absolute_path.canonicalize() { Ok(v) if v.starts_with(&root) => v, Ok(_) => { yield done(Err(fault("ast_edit target escapes the workspace root"))); return; }, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
				let language = match omp_ast::ops::resolve_language(None, &absolute) { Ok(v) => v, Err(e) => { advisories.push(Advisory { path: file.relative_path, message: Str::new(e.to_string()) }); continue; } };
				let rules_input = params.ops.iter().map(|op| (op.pat.to_string(), op.out.to_string())).collect::<Vec<_>>();
				let rules = match omp_ast::ops::compile_rewrite_rules(&rules_input, language) { Ok(v) => v, Err((index, e)) => { advisories.push(Advisory { path: file.relative_path, message: sf!("operation {} does not parse for this language: {}", index + 1, e) }); continue; } };
				let original = match std::fs::read(&absolute) { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
				let source = match std::str::from_utf8(&original) { Ok(v) => v, Err(_) => { advisories.push(Advisory { path: file.relative_path, message: sf!("non-UTF-8 file skipped") }); continue; } };
				let (updated, replacements) = match omp_ast::ops::rewrite_source(source, language, &rules) { Ok(v) => v, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } };
				if replacements != 0 { prepared.push(Prepared { absolute, relative: file.relative_path, before: *Hash32::sum(&original).as_bytes(), after: *Hash32::sum(updated.as_bytes()).as_bytes(), original, updated, replacements }); }
			}
			if prepared.is_empty() { yield done(Ok(Payload { files: Vec::new(), advisories, recovery_root: None })); return; }
			for item in &prepared { match std::fs::read(&item.absolute) { Ok(current) if Hash32::sum(&current).as_bytes() == &item.before => {}, Ok(_) => { yield done(Err(fault("ast_edit aborted because a document revision changed after dry-run"))); return; }, Err(e) => { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; } } }
			let generation = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |v| v.as_nanos());
			let recovery = root.join(".omp/recovery/ast-edit").join(generation.to_string());
			if let Err(e) = snapshot_all(&recovery, &prepared) { yield done(Err(Fault { message: Str::new(e.to_string()) })); return; }
			let mut committed = 0;
			for item in &prepared {
				let temporary = item.absolute.with_extension(format!("omp-ast-edit-{generation}"));
				let result = std::fs::write(&temporary, item.updated.as_bytes()).and_then(|()| std::fs::rename(&temporary, &item.absolute));
				if let Err(error) = result {
					for restore in prepared[..committed].iter().rev() { let _ = std::fs::write(&restore.absolute, &restore.original); }
					yield done(Err(Fault { message: Str::new(error.to_string()) })); return;
				}
				committed += 1;
			}
			let files = prepared.into_iter().map(|p| ChangedFile { path: p.relative, replacements: p.replacements, before_hash: short_hash(&p.before), after_hash: short_hash(&p.after) }).collect();
			yield done(Ok(Payload { files, advisories, recovery_root: Some(Str::from(recovery.to_string_lossy().into_owned())) }));
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Err(e) => Str::new(e.to_string()),
				Ok(p) => {
					let mut out = String::new();
					for file in &p.files {
						use std::fmt::Write as _;
						let _ = writeln!(
							out,
							"{}: {} replacements ({} -> {})",
							file.path, file.replacements, file.before_hash, file.after_hash
						);
					}
					for advisory in &p.advisories {
						use std::fmt::Write as _;
						let _ = writeln!(out, "[advisory {}] {}", advisory.path, advisory.message);
					}
					Str::new(out)
				},
			},
		}]
	}
}

fn snapshot_all(root: &Path, prepared: &[Prepared]) -> std::io::Result<()> {
	for item in prepared {
		let target = root.join(item.relative.as_str());
		if let Some(parent) = target.parent() {
			std::fs::create_dir_all(parent)?;
		}
		std::fs::write(target, &item.original)?;
	}
	Ok(())
}
fn short_hash(hash: &[u8; 32]) -> Str {
	use omp_core::encoding::hex;
	let mut out = [0_u8; 16];
	let count = hex::encode_mut(hash, &mut out);
	Str::new(std::str::from_utf8(&out[..count.min(12)]).expect("hex is UTF-8"))
}
fn fault(message: &'static str) -> Fault {
	Fault { message: Str::new_static(message) }
}
fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
	Ev::Done(ToolTerminal::Done {
		useless: result.as_ref().is_ok_and(|p| p.files.is_empty()),
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
