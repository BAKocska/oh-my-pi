//! Layered WATCHDOG discovery and prompt context composition.

use std::{
	collections::BTreeMap,
	fs,
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_agent::advisor::{
	AdvisorRoster, AdvisorRule, AdvisorRuleWarning, WatchdogRuleSet, evaluate_advisor_tools,
	merge_watchdog_rules, parse_watchdog_yaml,
};
use omp_core::{Str, StrMut};
use parking_lot::Mutex;
use rand::RngExt as _;

use crate::discovery::{
	active_repo::resolve_active_repo_context,
	at_path::{expand_at_paths, expand_at_text},
	context::{self, ContextDiscoveryOptions, GrantedContextRoot},
	native::user_config_root,
};

const WATCHDOG_FILENAMES: [&str; 3] = ["WATCHDOG.md", "WATCHDOG.yml", "WATCHDOG.yaml"];
const MAX_DISCOVERY_DEPTH: usize = 64;

/// Source precedence for one discovered WATCHDOG file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchdogLevel {
	/// Active user agent directory.
	User,
	/// Repository or cwd ancestor.
	Project,
}

/// One readable WATCHDOG candidate, ordered from least to most specific.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchdogSource {
	/// Source path.
	pub path:  PathBuf,
	/// Discovery level.
	pub level: WatchdogLevel,
	/// Ancestor distance from the session cwd.
	pub depth: u16,
}

/// Non-fatal WATCHDOG discovery failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigDiagnostic {
	/// A candidate existed but could not be read.
	Unreadable(PathBuf),
	/// A YAML candidate was rejected by the closed advisor schema.
	InvalidYaml(PathBuf),
	/// An instruction import could not be expanded; original text was retained.
	ImportFailed(PathBuf),
	/// The upward walk hit its hard I/O bound.
	WalkTruncated(PathBuf),
}

/// Complete immutable advisor configuration projection for one session.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdvisorConfigSnapshot {
	/// Specificity-merged advisor roster.
	pub roster:              AdvisorRoster,
	/// WATCHDOG.md attention blocks in prompt order.
	pub attention:           Arc<[Str]>,
	/// Standing repository instructions for the advisor system prompt.
	pub project_context:     Option<Str>,
	/// Single-direct-child repository hint when the cwd itself is outside Git.
	pub active_repo_context: Option<Str>,
	/// Sources that contributed valid content.
	pub sources:             Arc<[WatchdogSource]>,
	/// Bounded non-fatal discovery diagnostics.
	pub diagnostics:         Arc<[ConfigDiagnostic]>,
}

/// One enabled advisor ready for app-owned runtime construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledAdvisor {
	/// Specificity-winning advisor declaration.
	pub rule:                AdvisorRule,
	/// Tool subset evaluated against tools actually built for this session.
	pub tools:               Box<[Str]>,
	/// Stable provider-facing UUIDv7 affinity identity.
	pub provider_session_id: Str,
}

/// Advisor roster and shared prompt projected for one primary session.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdvisorSchedule {
	/// Enabled advisors in stable roster order.
	pub advisors:      Arc<[ScheduledAdvisor]>,
	/// Unknown tool grants dropped during session-specific evaluation.
	pub warnings:      Arc<[AdvisorRuleWarning]>,
	/// Shared WATCHDOG and standing-project context.
	pub shared_prompt: Option<Str>,
}

impl AdvisorConfigSnapshot {
	/// Resolves enabled advisors against the session's actual built-in tools.
	#[must_use]
	pub fn schedule(
		&self,
		primary_session: &str,
		available_tools: &[Str],
		provider_sessions: &AdvisorProviderSessions,
	) -> AdvisorSchedule {
		let mut advisors = Vec::new();
		let mut warnings = Vec::new();
		for rule in self.roster.advisors.iter().filter(|rule| rule.enabled) {
			let evaluated = evaluate_advisor_tools(rule, available_tools);
			for warning in &evaluated.warnings {
				tracing::warn!(
					advisor = %warning.advisor,
					source = %warning.source,
					tool = %warning.tool,
					"unknown advisor tool grant was dropped"
				);
			}
			warnings.extend(evaluated.warnings);
			advisors.push(ScheduledAdvisor {
				rule:                rule.clone(),
				tools:               evaluated.tools,
				provider_session_id: provider_sessions
					.get_or_create(primary_session, rule.slug.as_str()),
			});
		}
		AdvisorSchedule {
			advisors:      advisors.into(),
			warnings:      warnings.into(),
			shared_prompt: self.shared_prompt(),
		}
	}

	fn shared_prompt(&self) -> Option<Str> {
		let mut prompt = StrMut::new("");
		if let Some(instructions) = self.roster.instructions.as_ref() {
			prompt.push_str(instructions.as_str());
		}
		for block in self.attention.iter() {
			if !prompt.is_empty() {
				prompt.push_str("\n\n");
			}
			prompt.push_str(block.as_str());
		}
		for block in [self.project_context.as_ref(), self.active_repo_context.as_ref()]
			.into_iter()
			.flatten()
		{
			if !prompt.is_empty() {
				prompt.push_str("\n\n");
			}
			prompt.push_str(block.as_str());
		}
		(!prompt.is_empty()).then(|| prompt.freeze())
	}
}

/// Discovers user and project WATCHDOG configuration for `cwd`.
///
/// Files are applied user-first and then project ancestor-to-leaf. Duplicate
/// advisor slugs therefore resolve to the closest project declaration.
#[must_use]
pub fn discover(cwd: &Path, agent_dir: Option<&Path>) -> AdvisorConfigSnapshot {
	let home = std::env::var_os("HOME").map_or_else(|| cwd.to_path_buf(), PathBuf::from);
	let agent_dir = agent_dir.map_or_else(|| user_config_root(&home), Path::to_path_buf);
	let (candidates, mut diagnostics) = collect_candidates(cwd, &agent_dir, &home);
	let mut rules = Vec::<WatchdogRuleSet>::new();
	let mut attention = Vec::new();
	let mut sources = Vec::new();

	for candidate in candidates {
		let raw = match fs::read_to_string(&candidate.path) {
			Ok(raw) => raw,
			Err(_) => {
				diagnostics.push(ConfigDiagnostic::Unreadable(candidate.path));
				continue;
			},
		};
		match candidate.path.extension().and_then(std::ffi::OsStr::to_str) {
			Some("md") => match expand_at_paths(&candidate.path) {
				Ok(expanded) => {
					attention.push(Str::from(format!(
						"Especially pay attention to:\n<attention>\n{expanded}\n</attention>"
					)));
					sources.push(candidate);
				},
				Err(_) => diagnostics.push(ConfigDiagnostic::ImportFailed(candidate.path)),
			},
			Some("yml" | "yaml") => {
				let source = candidate.path.to_string_lossy();
				let mut parsed = match parse_watchdog_yaml(&source, &raw) {
					Ok(parsed) => parsed,
					Err(error) => {
						tracing::warn!(path = %candidate.path.display(), %error, "advisor config was rejected");
						diagnostics.push(ConfigDiagnostic::InvalidYaml(candidate.path));
						continue;
					},
				};
				expand_rule_instructions(&mut parsed, &candidate.path, &mut diagnostics);
				rules.push(parsed);
				sources.push(candidate);
			},
			_ => {},
		}
	}

	let boundary = repository_root(cwd).unwrap_or_else(|| fallback_boundary(cwd, &home));
	let project_context = format_project_context(cwd, &boundary);
	let active_repo_context = resolve_active_repo_context(cwd)
		.ok()
		.flatten()
		.map(|active| {
			Str::from(format!(
				"<attention>\nSession cwd is outside git; the active project is the single \
				 direct-child repository `{}`. Check paths beneath it before claiming work is \
				 absent.\n</attention>",
				active.relative_repo_root.display()
			))
		});

	AdvisorConfigSnapshot {
		roster: merge_watchdog_rules(rules),
		attention: attention.into(),
		project_context,
		active_repo_context,
		sources: sources.into(),
		diagnostics: diagnostics.into(),
	}
}

fn expand_rule_instructions(
	rules: &mut WatchdogRuleSet,
	source: &Path,
	diagnostics: &mut Vec<ConfigDiagnostic>,
) {
	if let Some(instructions) = rules.instructions.as_mut() {
		match expand_at_text(instructions.as_str(), source) {
			Ok(expanded) => *instructions = Str::from(expanded.trim()),
			Err(_) => diagnostics.push(ConfigDiagnostic::ImportFailed(source.to_path_buf())),
		}
	}
	for advisor in &mut rules.advisors {
		let Some(instructions) = advisor.instructions.as_mut() else {
			continue;
		};
		match expand_at_text(instructions.as_str(), source) {
			Ok(expanded) => *instructions = Str::from(expanded.trim()),
			Err(_) => diagnostics.push(ConfigDiagnostic::ImportFailed(source.to_path_buf())),
		}
	}
}

fn collect_candidates(
	cwd: &Path,
	agent_dir: &Path,
	home: &Path,
) -> (Vec<WatchdogSource>, Vec<ConfigDiagnostic>) {
	let mut output = Vec::new();
	for filename in WATCHDOG_FILENAMES {
		let path = agent_dir.join(filename);
		if path.is_file() {
			output.push(WatchdogSource { path, level: WatchdogLevel::User, depth: u16::MAX });
		}
	}

	let stop = repository_root(cwd).unwrap_or_else(|| fallback_boundary(cwd, home));
	let mut levels = Vec::<(PathBuf, u16)>::new();
	let mut current = cwd.to_path_buf();
	let mut truncated = true;
	for depth in 0..=MAX_DISCOVERY_DEPTH {
		levels.push((current.clone(), u16::try_from(depth).unwrap_or(u16::MAX)));
		if current == stop {
			truncated = false;
			break;
		}
		let Some(parent) = current.parent() else {
			truncated = false;
			break;
		};
		if parent == current {
			truncated = false;
			break;
		}
		current = parent.to_path_buf();
	}
	let diagnostics = truncated
		.then(|| ConfigDiagnostic::WalkTruncated(current))
		.into_iter()
		.collect();

	for (directory, depth) in levels.into_iter().rev() {
		let hidden_owner = directory
			.file_name()
			.and_then(std::ffi::OsStr::to_str)
			.is_some_and(|name| name.starts_with('.'));
		for filename in WATCHDOG_FILENAMES {
			let native = directory.join(".omp").join(filename);
			if native.is_file() {
				output.push(WatchdogSource { path: native, level: WatchdogLevel::Project, depth });
			}
			let standalone = directory.join(filename);
			if !hidden_owner && standalone.is_file() {
				output.push(WatchdogSource { path: standalone, level: WatchdogLevel::Project, depth });
			}
		}
	}
	(output, diagnostics)
}

fn repository_root(cwd: &Path) -> Option<PathBuf> {
	cwd.ancestors()
		.find(|path| path.join(".git").is_dir() || path.join(".git").is_file())
		.map(Path::to_path_buf)
}

fn fallback_boundary(cwd: &Path, home: &Path) -> PathBuf {
	if cwd.starts_with(home) {
		home.to_path_buf()
	} else {
		cwd.ancestors().last().unwrap_or(cwd).to_path_buf()
	}
}

fn format_project_context(cwd: &Path, boundary: &Path) -> Option<Str> {
	let snapshot = context::discover(
		&[GrantedContextRoot { root: boundary.to_path_buf(), start: cwd.to_path_buf() }],
		&ContextDiscoveryOptions::default(),
	);
	if snapshot.items.is_empty() {
		return None;
	}
	let mut prompt = StrMut::new("");
	prompt.push_str(
		"<project-context>\nContext files are binding standing project instructions for the driving \
		 agent. Enforce them and flag drift; never advise against their mandates.\n",
	);
	for item in snapshot.items.iter() {
		let path = item.path.strip_prefix(cwd).unwrap_or(&item.path);
		prompt.push_str("<file path=\"");
		push_xml_attribute(&mut prompt, path.to_string_lossy().as_ref());
		prompt.push_str("\">\n");
		prompt.push_str(item.content.as_str());
		if !item.content.ends_with('\n') {
			prompt.push('\n');
		}
		prompt.push_str("</file>\n");
	}
	prompt.push_str("</project-context>");
	Some(prompt.freeze())
}

fn push_xml_attribute(output: &mut StrMut, text: &str) {
	for character in text.chars() {
		match character {
			'&' => output.push_str("&amp;"),
			'<' => output.push_str("&lt;"),
			'>' => output.push_str("&gt;"),
			'\"' => output.push_str("&quot;"),
			'\'' => output.push_str("&apos;"),
			other => output.push(other),
		}
	}
}

/// Stable provider-facing UUIDv7 allocation per primary-session/advisor pair.
#[derive(Default)]
pub struct AdvisorProviderSessions {
	ids: Mutex<BTreeMap<(Str, Str), Str>>,
}

impl AdvisorProviderSessions {
	/// Returns the existing provider affinity id or allocates it once.
	#[must_use]
	pub fn get_or_create(&self, primary_session: &str, advisor_slug: &str) -> Str {
		let key = (Str::new(primary_session), Str::new(advisor_slug));
		let mut ids = self.ids.lock();
		ids.entry(key).or_insert_with(uuid_v7).clone()
	}

	/// Clears affinity ids at the explicit session lifecycle boundary.
	pub fn clear_session(&self, primary_session: &str) {
		self
			.ids
			.lock()
			.retain(|(session, _), _| session.as_str() != primary_session);
	}
}

fn uuid_v7() -> Str {
	let timestamp = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |duration| duration.as_millis() as u64);
	let mut bytes = rand::rng().random::<[u8; 16]>();
	let stamp = timestamp.to_be_bytes();
	bytes[..6].copy_from_slice(&stamp[2..]);
	bytes[6] = (bytes[6] & 0x0f) | 0x70;
	bytes[8] = (bytes[8] & 0x3f) | 0x80;
	Str::from(format!(
		"{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:\
		 02x}{:02x}",
		bytes[0],
		bytes[1],
		bytes[2],
		bytes[3],
		bytes[4],
		bytes[5],
		bytes[6],
		bytes[7],
		bytes[8],
		bytes[9],
		bytes[10],
		bytes[11],
		bytes[12],
		bytes[13],
		bytes[14],
		bytes[15]
	))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn provider_affinity_is_stable_and_session_scoped() {
		let ids = AdvisorProviderSessions::default();
		let first = ids.get_or_create("primary", "architecture");
		assert_eq!(first, ids.get_or_create("primary", "architecture"));
		assert_ne!(first, ids.get_or_create("other", "architecture"));
		assert_eq!(first.as_bytes()[14], b'7');
		assert!(matches!(first.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
	}
}
