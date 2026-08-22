//! Immutable active-skill inventories, invocation parsing, and prompt
//! rendering.

pub mod managed;

use std::{
	collections::BTreeMap,
	fs,
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_agent::PromptNamedInput;
use omp_core::{CowBytes, Str};
use omp_tools::read::{
	Fault,
	resolver::{
		LineOffsetCache, Resolve, ResourceCompletion, ResourceEntry, ResourceList, fuzzy_score,
	},
	selector::ParsedSelector,
};

use crate::discovery::manifest::{
	CapabilityPayload, CapabilityRecord, DiscoveredCapability, SkillPayload,
};

/// Frozen skill entry consumed by prompts, `/skill:`, and `skill://`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveSkill {
	/// Stable invocation name.
	pub name:                     Str,
	/// Inventory description.
	pub description:              Str,
	/// Canonical skill file.
	pub path:                     PathBuf,
	/// Canonical base directory.
	pub base_dir:                 PathBuf,
	/// Frozen Markdown body.
	pub body:                     Str,
	/// Source provenance label.
	pub source:                   Str,
	/// Whether the skill is omitted from the visible inventory.
	pub hidden:                   bool,
	/// Whether model-driven invocation is forbidden.
	pub disable_model_invocation: bool,
	/// Whether this skill is autoloaded into every prompt.
	pub autoload:                 bool,
	/// Optional package resource containment root.
	pub contain_root:             Option<PathBuf>,
}

/// Immutable per-session skill registry. It never rediscovers beneath a chat.
#[derive(Clone, Debug, Default)]
pub struct SkillSnapshot {
	ordered: Arc<[ActiveSkill]>,
	by_name: Arc<BTreeMap<Str, usize>>,
}

impl SkillSnapshot {
	/// Freezes winning skill records in case-insensitive name/path order.
	#[must_use]
	pub fn from_records(records: &[Arc<CapabilityRecord>]) -> Self {
		let mut ordered = records
			.iter()
			.filter_map(|record| {
				let CapabilityPayload::Skills(payload) = &record.payload else {
					return None;
				};
				Some(from_payload(payload, record.provenance.source.source_id.clone()))
			})
			.collect::<Vec<_>>();
		ordered.sort_by(|left, right| {
			left
				.name
				.as_str()
				.to_ascii_lowercase()
				.cmp(&right.name.as_str().to_ascii_lowercase())
				.then_with(|| left.name.cmp(&right.name))
				.then_with(|| left.path.cmp(&right.path))
		});
		let by_name = ordered
			.iter()
			.enumerate()
			.map(|(index, skill)| (skill.name.clone(), index))
			.collect();
		Self { ordered: ordered.into(), by_name: Arc::new(by_name) }
	}

	/// Freezes provider declarations before registry provenance attachment.
	#[must_use]
	pub fn from_declarations(declarations: &[DiscoveredCapability]) -> Self {
		Self::from_skills(
			declarations
				.iter()
				.filter_map(|declaration| {
					let CapabilityPayload::Skills(payload) = &declaration.payload else {
						return None;
					};
					Some(from_payload(payload, declaration.source.source_id.clone()))
				})
				.collect(),
		)
	}

	/// Freezes already parsed declarations, useful for custom/managed sources.
	#[must_use]
	pub fn from_skills(mut skills: Vec<ActiveSkill>) -> Self {
		skills.sort_by(|left, right| {
			left
				.name
				.as_str()
				.to_ascii_lowercase()
				.cmp(&right.name.as_str().to_ascii_lowercase())
				.then_with(|| left.name.cmp(&right.name))
				.then_with(|| left.path.cmp(&right.path))
		});
		let by_name = skills
			.iter()
			.enumerate()
			.map(|(index, skill)| (skill.name.clone(), index))
			.collect();
		Self { ordered: skills.into(), by_name: Arc::new(by_name) }
	}

	/// All active skills, including hidden/model-disabled entries reachable by
	/// an explicit user invocation or internal URL.
	#[must_use]
	pub fn all(&self) -> &[ActiveSkill] {
		&self.ordered
	}

	/// Looks up a skill without allocating.
	#[must_use]
	pub fn get(&self, name: &str) -> Option<&ActiveSkill> {
		self.by_name.get(name).map(|index| &self.ordered[*index])
	}

	/// Deterministic visible inventory for model selection.
	pub fn visible(&self) -> impl Iterator<Item = &ActiveSkill> {
		self.ordered.iter().filter(|skill| {
			!skill.hidden
				&& !skill.disable_model_invocation
				&& !skill.autoload
				&& !skill.description.is_empty()
		})
	}

	/// Deterministic autoload inventory.
	pub fn autoload(&self) -> impl Iterator<Item = &ActiveSkill> {
		self.ordered.iter().filter(|skill| skill.autoload)
	}

	/// Resolves the frozen whole skill body. Filesystem changes after snapshot
	/// creation are intentionally unobservable through this route.
	#[must_use]
	pub fn resolve_body(&self, name: &str) -> Option<&str> {
		self.get(name).map(|skill| skill.body.as_str())
	}
}

/// Projects visible and autoload skill inventories into immutable named prompt
/// inputs. Autoload entries carry their frozen body; visible entries carry the
/// model-selectable description and `skill://` origin.
#[must_use]
pub fn prompt_inputs(snapshot: &SkillSnapshot) -> Arc<[PromptNamedInput]> {
	snapshot
		.all()
		.iter()
		.filter_map(|skill| {
			if skill.hidden || skill.disable_model_invocation {
				return None;
			}
			Some(PromptNamedInput {
				id:      skill.name.clone(),
				origin:  Str::from(format!("skill://{}", skill.name)),
				content: if skill.autoload {
					skill.body.clone()
				} else {
					skill.description.clone()
				},
			})
		})
		.collect::<Vec<_>>()
		.into()
}

fn from_payload(payload: &SkillPayload, source: Str) -> ActiveSkill {
	ActiveSkill {
		name: payload.name.clone(),
		description: payload.frontmatter.description.clone().unwrap_or_default(),
		path: payload.path.clone(),
		base_dir: payload
			.path
			.parent()
			.unwrap_or(Path::new("."))
			.to_path_buf(),
		body: payload.content.clone(),
		source,
		hidden: payload.frontmatter.hidden,
		disable_model_invocation: payload.frontmatter.disable_model_invocation,
		autoload: payload.frontmatter.always_apply,
		contain_root: payload.contain_root.clone(),
	}
}

/// Parsed `/skill:<name>` occurrence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSkillInvocation {
	/// Skill name.
	pub name: Str,
	/// User text outside the invocation token.
	pub args: Str,
}

/// Detects leading and mid-prompt skill invocations while preserving local
/// `!`/`!!` and `$`/`$$` execution branches and other leading slash commands.
#[must_use]
pub fn parse_invocation(text: &str) -> Option<ParsedSkillInvocation> {
	let trimmed = text.trim_start();
	if let Some(rest) = trimmed.strip_prefix("/skill:") {
		let split = rest.find(char::is_whitespace).unwrap_or(rest.len());
		let name = &rest[..split];
		if name.is_empty() {
			return None;
		}
		return Some(ParsedSkillInvocation {
			name: Str::from(name),
			args: Str::from(rest[split..].trim()),
		});
	}
	if trimmed.starts_with('/') || starts_with_local_execution_prefix(trimmed) {
		return None;
	}
	for (start, _) in text.match_indices("/skill:") {
		if start > 0 && !text.as_bytes()[start - 1].is_ascii_whitespace() {
			continue;
		}
		let name_start = start + "/skill:".len();
		let name_end = text[name_start..]
			.find(char::is_whitespace)
			.map_or(text.len(), |offset| name_start + offset);
		let name = &text[name_start..name_end];
		if name.is_empty() || name.contains('/') {
			continue;
		}
		let before = text[..start].trim_end();
		let after = text[name_end..].trim_start();
		let args = match (before.is_empty(), after.is_empty()) {
			(true, true) => Str::default(),
			(false, true) => Str::from(before),
			(true, false) => Str::from(after),
			(false, false) => Str::from(format!("{before} {after}")),
		};
		return Some(ParsedSkillInvocation { name: Str::from(name), args });
	}
	None
}

fn starts_with_local_execution_prefix(text: &str) -> bool {
	if text.starts_with('!') {
		return true;
	}
	let bytes = text.as_bytes();
	if bytes.first() != Some(&b'$') || bytes.get(1) == Some(&b'{') {
		return false;
	}
	let length = if bytes.get(1) == Some(&b'$') { 2 } else { 1 };
	bytes.get(length).is_none_or(u8::is_ascii_whitespace)
}

/// Read-only `skill://` resolver over one immutable session snapshot.
pub struct SkillResolver {
	snapshot: Arc<SkillSnapshot>,
	lines:    LineOffsetCache,
}

impl SkillResolver {
	/// Creates a resolver which cannot observe skill winner changes after this
	/// call.
	#[must_use]
	pub fn new(snapshot: Arc<SkillSnapshot>) -> Self {
		Self { snapshot, lines: LineOffsetCache::default() }
	}
}

impl Resolve for SkillResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let resource = resource.trim_matches('/');
		let (name, nested) = resource
			.split_once('/')
			.map_or((resource, None), |(name, nested)| (name, Some(nested)));
		let skill = self.snapshot.get(name).ok_or_else(|| Fault::Source {
			message: Str::from(format!("skill resource not found: {resource}")),
		})?;
		let bytes = if let Some(nested) = nested {
			let boundary = skill.contain_root.as_deref().unwrap_or(&skill.base_dir);
			let root = fs::canonicalize(boundary).map_err(|_| Fault::Source {
				message: Str::from(format!("skill resource not found: {resource}")),
			})?;
			let path = fs::canonicalize(root.join(nested)).map_err(|_| Fault::Source {
				message: Str::from(format!("skill resource not found: {resource}")),
			})?;
			if !path.starts_with(&root) || !path.is_file() {
				return Err(Fault::Invalid {
					message: Str::from("skill resource escapes its containRoot"),
				});
			}
			let metadata = fs::metadata(&path).map_err(|_| Fault::Source {
				message: Str::from(format!("skill resource not found: {resource}")),
			})?;
			if metadata.len() > 1024 * 1024 {
				return Err(Fault::Invalid {
					message: Str::from("skill resource exceeds the 1 MiB bound"),
				});
			}
			CowBytes::from(fs::read(path).map_err(|_| Fault::Source {
				message: Str::from(format!("skill resource not found: {resource}")),
			})?)
		} else {
			CowBytes::from(skill.body.as_bytes().to_vec())
		};
		select_snapshot_bytes(&self.lines, resource, bytes, selector)
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		max_bytes: usize,
	) -> Result<ResourceList, Fault> {
		if !resource.trim_matches('/').is_empty() {
			return Err(Fault::Invalid {
				message: Str::from("skill resources can only be listed at the scheme root"),
			});
		}
		let mut entries = Vec::new();
		let mut bytes: usize = 0;
		for skill in self.snapshot.all() {
			let uri = format!("skill://{}", skill.name);
			if entries.len() == max_entries || bytes.saturating_add(uri.len()) > max_bytes {
				return Ok(ResourceList { entries, truncated: true });
			}
			bytes += uri.len();
			entries.push(ResourceEntry {
				uri:       Str::from(uri),
				name:      skill.name.clone(),
				directory: false,
				size:      skill.body.len() as u64,
			});
		}
		Ok(ResourceList { entries, truncated: false })
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let mut matches = self
			.snapshot
			.all()
			.iter()
			.filter_map(|skill| {
				Some(ResourceCompletion {
					value:       Str::from(format!("skill://{}", skill.name)),
					description: skill.description.clone(),
					score:       fuzzy_score(query, &skill.name)?,
				})
			})
			.collect::<Vec<_>>();
		matches.sort_unstable_by(|left, right| {
			right
				.score
				.cmp(&left.score)
				.then_with(|| left.value.cmp(&right.value))
		});
		matches.truncate(max_results);
		Ok(matches)
	}
}

fn select_snapshot_bytes(
	lines: &LineOffsetCache,
	resource: &str,
	bytes: CowBytes<'static>,
	selector: &ParsedSelector,
) -> Result<CowBytes<'static>, Fault> {
	let ParsedSelector::Lines { ranges, .. } = selector else {
		return Ok(bytes);
	};
	if ranges.len() == 1 {
		return lines
			.slice(resource, &bytes, ranges[0])
			.map(CowBytes::into_owned)
			.map_err(|error| Fault::Invalid { message: Str::from(error.to_string()) });
	}
	let mut output = Vec::new();
	for range in ranges {
		output.extend_from_slice(
			&lines
				.slice(resource, &bytes, *range)
				.map_err(|error| Fault::Invalid { message: Str::from(error.to_string()) })?,
		);
	}
	Ok(CowBytes::from(output))
}

/// Skill prompt provenance mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillInvocationKind {
	User,
	Autoload,
}

/// Renders an invocation from frozen content, with distinct user/autoload
/// provenance and explicit base/contain roots.
#[must_use]
pub fn render_invocation(skill: &ActiveSkill, args: &str, kind: SkillInvocationKind) -> Str {
	let args = args.trim();
	let mut output = String::new();
	match kind {
		SkillInvocationKind::User => {
			output.push_str("<skill name=\"");
			output.push_str(skill.name.as_str());
			output.push_str("\" invoked=\"user\">\n");
			output.push_str("<baseDir>");
			output.push_str(&skill.base_dir.to_string_lossy());
			output.push_str("</baseDir>\n");
			if let Some(root) = &skill.contain_root {
				output.push_str("<containRoot>");
				output.push_str(&root.to_string_lossy());
				output.push_str("</containRoot>\n");
			}
		},
		SkillInvocationKind::Autoload => {
			output.push_str("<skill autoload=\"true\" source=\"");
			output.push_str(&skill.path.to_string_lossy());
			output.push_str("\">\n");
		},
	}
	output.push_str(skill.body.as_str());
	if !args.is_empty() {
		output.push_str("\n<arguments>");
		output.push_str(args);
		output.push_str("</arguments>");
	}
	output.push_str("\n</skill>");
	Str::from(output)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_leading_and_mid_prompt_but_not_execution_guards() {
		assert_eq!(
			parse_invocation(" /skill:review auth"),
			Some(ParsedSkillInvocation { name: Str::from("review"), args: Str::from("auth") })
		);
		assert_eq!(
			parse_invocation("fix it /skill:review auth"),
			Some(ParsedSkillInvocation { name: Str::from("review"), args: Str::from("fix it auth") })
		);
		assert!(parse_invocation("!! echo /skill:review").is_none());
		assert!(parse_invocation("$$ print('/skill:review')").is_none());
		assert!(parse_invocation("/compact /skill:review").is_none());
	}

	#[test]
	fn frozen_body_does_not_observe_mutation() {
		let snapshot = SkillSnapshot::from_skills(vec![ActiveSkill {
			name:                     Str::from("x"),
			description:              Str::from("x"),
			path:                     PathBuf::from("SKILL.md"),
			base_dir:                 PathBuf::from("."),
			body:                     Str::from("frozen"),
			source:                   Str::from("test"),
			hidden:                   false,
			disable_model_invocation: false,
			autoload:                 false,
			contain_root:             None,
		}]);
		assert_eq!(snapshot.resolve_body("x"), Some("frozen"));
	}
}
