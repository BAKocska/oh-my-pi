//! Deterministic, data-only skill discovery and admission.

use std::{
	collections::{BTreeMap, BTreeSet},
	fs,
	path::{Path, PathBuf},
};

use omp_core::Str;
use omp_settings::{
	DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};
use omp_walker::{FollowLinks, WalkRequest};
use serde::{Deserialize, Serialize};

use super::{
	containment::contained_existing,
	manifest::{
		CapabilityPayload, DiscoveredCapability, SkillFrontmatter, SkillPayload, SourceProvenance,
		SourceScope,
	},
};

/// One skill source scanned in caller-defined precedence order.
#[derive(Clone, Debug)]
pub struct SkillSource {
	/// Stable source/provider identity used by settings.
	pub id:                  Str,
	/// Direct `SKILL.md` directory or parent of named skill directories.
	pub root:                PathBuf,
	/// Source scope.
	pub scope:               SourceScope,
	/// Whether the root itself may be a skill.
	pub include_root:        bool,
	/// Whether a description is mandatory for this source.
	pub require_description: bool,
	/// Optional package containment root.
	pub contain_root:        Option<PathBuf>,
	/// Read-only foreign/package content marker.
	pub read_only:           bool,
}

/// Settings projection applied before skill names claim precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SkillDiscoverySettings {
	/// Master enablement.
	pub enabled:             bool,
	/// Explicitly disabled source IDs.
	pub disabled_sources:    BTreeSet<Str>,
	/// Inclusion globs over skill names; empty includes every name.
	pub include:             Vec<Str>,
	/// Exclusion globs over skill names.
	pub ignore:              Vec<Str>,
	/// Explicit disabled skill names.
	pub disabled_skills:     BTreeSet<Str>,
	/// Fallback gate for repo-surface third-party families without a dedicated
	/// source toggle.
	pub third_party_enabled: bool,
	/// Additional authored skill directories.
	pub custom_directories:  Vec<PathBuf>,
}

impl Default for SkillDiscoverySettings {
	fn default() -> Self {
		Self {
			enabled:             true,
			disabled_sources:    BTreeSet::new(),
			include:             Vec::new(),
			ignore:              Vec::new(),
			disabled_skills:     BTreeSet::new(),
			third_party_enabled: true,
			custom_directories:  Vec::new(),
		}
	}
}

const SKILL_SCOPES: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

impl SettingsDomain for SkillDiscoverySettings {
	const DOMAIN: &'static str = "skills";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "skills.enabled",
			label:       "Skills",
			description: "Enable skill discovery and invocation.",
			kind:        SettingKind::Boolean,
			scopes:      SKILL_SCOPES,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.disabled_sources",
			label:       "Disabled skill sources",
			description: "Source IDs excluded before skill names claim precedence.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.include",
			label:       "Included skills",
			description: "Optional skill-name inclusion globs.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       30,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.ignore",
			label:       "Ignored skills",
			description: "Skill-name exclusion globs.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       40,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.disabled_skills",
			label:       "Disabled skills",
			description: "Explicit skill names disabled before collision handling.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       50,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.third_party_enabled",
			label:       "Third-party content skills",
			description: "Enable repo-surface third-party skill families without a dedicated source \
			              toggle.",
			kind:        SettingKind::Boolean,
			scopes:      SKILL_SCOPES,
			order:       60,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "skills.custom_directories",
			label:       "Custom skill directories",
			description: "Additional native authored skill roots.",
			kind:        SettingKind::Array,
			scopes:      SKILL_SCOPES,
			order:       70,
			options:     None,
			condition:   None,
			secret:      false,
		},
	];

	fn validate(&self) -> Result<(), ValidationError> {
		let valid = self.disabled_sources.iter().all(|value| !value.is_empty())
			&& self.disabled_skills.iter().all(|value| !value.is_empty())
			&& self
				.custom_directories
				.iter()
				.all(|path| !path.as_os_str().is_empty());
		if valid {
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

omp_settings::inventory::submit! {
	DomainRegistration::of::<SkillDiscoverySettings>()
}

/// Non-fatal skill discovery diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillWarning {
	/// Source which was skipped or suppressed.
	pub path:    PathBuf,
	/// Stable diagnostic text.
	pub message: Str,
}

/// Stable skill provider output.
#[derive(Clone, Debug, Default)]
pub struct SkillDiscovery {
	/// Winning declarations in case-insensitive name/path order.
	pub declarations: Vec<DiscoveredCapability>,
	/// Non-fatal malformed, duplicate, and collision diagnostics.
	pub warnings:     Vec<SkillWarning>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillHeader {
	name:                     Option<String>,
	description:              Option<String>,
	#[serde(default)]
	globs:                    StringList,
	#[serde(default)]
	always_apply:             bool,
	#[serde(default)]
	enabled:                  Option<bool>,
	#[serde(default, alias = "hide")]
	hidden:                   bool,
	#[serde(default)]
	disable_model_invocation: bool,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum StringList {
	One(String),
	Many(Vec<String>),
	#[default]
	None,
}

impl StringList {
	fn values(self) -> Vec<Str> {
		match self {
			Self::One(value) => value
				.split(',')
				.map(str::trim)
				.filter(|s| !s.is_empty())
				.map(Str::from)
				.collect(),
			Self::Many(values) => values
				.into_iter()
				.map(|s| s.trim().to_owned())
				.filter(|s| !s.is_empty())
				.map(Str::from)
				.collect(),
			Self::None => Vec::new(),
		}
	}
}

/// Scans direct and nested `SKILL.md` declarations from ordered sources,
/// follows only contained symlinks, applies source/name gates before claiming
/// names, and realpath-deduplicates declarations.
#[must_use]
pub fn discover(sources: &[SkillSource], settings: &SkillDiscoverySettings) -> SkillDiscovery {
	if !settings.enabled {
		return SkillDiscovery::default();
	}
	let mut output = SkillDiscovery::default();
	let mut names = BTreeMap::<Str, PathBuf>::new();
	let mut realpaths = BTreeSet::new();
	let mut configured_sources = sources.to_vec();
	configured_sources.extend(
		settings
			.custom_directories
			.iter()
			.cloned()
			.map(|root| SkillSource {
				id: Str::from("custom"),
				root,
				scope: SourceScope::User,
				include_root: true,
				require_description: true,
				contain_root: None,
				read_only: false,
			}),
	);
	for source in &configured_sources {
		if settings.disabled_sources.contains(&source.id)
			|| (!settings.third_party_enabled && source.id.starts_with("foreign-"))
		{
			continue;
		}
		for path in skill_files(source, &mut output.warnings) {
			let canonical =
				match contained_existing(source.contain_root.as_deref().unwrap_or(&source.root), &path)
				{
					Ok(path) => path,
					Err(_) => {
						output.warnings.push(SkillWarning {
							path,
							message: Str::from("skill path escapes its source root"),
						});
						continue;
					},
				};
			if !realpaths.insert(canonical.clone()) {
				continue;
			}
			let (header, content) = match parse_skill(&canonical) {
				Ok(value) => value,
				Err(_) => {
					output.warnings.push(SkillWarning {
						path:    canonical,
						message: Str::from("failed to parse SKILL.md frontmatter"),
					});
					continue;
				},
			};
			if header.enabled == Some(false) {
				continue;
			}
			let fallback = canonical
				.parent()
				.and_then(Path::file_name)
				.and_then(|name| name.to_str())
				.unwrap_or("skill");
			let name = header
				.name
				.as_deref()
				.map(str::trim)
				.filter(|name| !name.is_empty())
				.unwrap_or(fallback);
			if !safe_skill_name(name) {
				output.warnings.push(SkillWarning {
					path:    canonical,
					message: Str::from("skill name is not a safe directory-style identifier"),
				});
				continue;
			}
			if source.require_description
				&& header
					.description
					.as_deref()
					.map(str::trim)
					.filter(|v| !v.is_empty())
					.is_none()
			{
				continue;
			}
			if settings.disabled_skills.contains(name)
				|| settings
					.ignore
					.iter()
					.any(|pattern| glob_matches(pattern.as_str(), name))
				|| (!settings.include.is_empty()
					&& !settings
						.include
						.iter()
						.any(|pattern| glob_matches(pattern.as_str(), name)))
			{
				continue;
			}
			let key = Str::from(name);
			if let Some(winner) = names.get(&key) {
				output.warnings.push(SkillWarning {
					path:    canonical,
					message: Str::from(format!("skill name is already claimed by {}", winner.display())),
				});
				continue;
			}
			names.insert(key.clone(), canonical.clone());
			let payload = SkillPayload {
				name:         key.clone(),
				path:         canonical.clone(),
				content:      Str::from(content),
				frontmatter:  SkillFrontmatter {
					description:              header
						.description
						.map(|value| Str::from(value.trim().to_owned())),
					globs:                    header.globs.values(),
					always_apply:             header.always_apply,
					hidden:                   header.hidden,
					disable_model_invocation: header.disable_model_invocation,
				},
				contain_root: source.contain_root.clone(),
			};
			let mut provenance = SourceProvenance::native(source.id.clone(), canonical, source.scope);
			provenance.read_only = source.read_only;
			output.declarations.push(DiscoveredCapability::keyed(
				key,
				CapabilityPayload::Skills(payload),
				provenance,
			));
		}
	}
	output.declarations.sort_by(|left, right| {
		let left = match &left.payload {
			CapabilityPayload::Skills(skill) => skill,
			_ => unreachable!(),
		};
		let right = match &right.payload {
			CapabilityPayload::Skills(skill) => skill,
			_ => unreachable!(),
		};
		left
			.name
			.as_str()
			.to_ascii_lowercase()
			.cmp(&right.name.as_str().to_ascii_lowercase())
			.then_with(|| left.name.cmp(&right.name))
			.then_with(|| left.path.cmp(&right.path))
	});
	output
}

fn skill_files(source: &SkillSource, warnings: &mut Vec<SkillWarning>) -> Vec<PathBuf> {
	let mut files = Vec::new();
	if source.include_root && source.root.join("SKILL.md").is_file() {
		files.push(source.root.join("SKILL.md"));
	}
	let outcome = WalkRequest::new(&source.root)
		.hidden(false)
		.gitignore(true)
		.skip_git(true)
		.follow_links(FollowLinks::Always)
		.depth(2, 2)
		.limit(1024)
		.collect_files();
	match outcome {
		Ok(entries) => files.extend(
			entries
				.into_iter()
				.map(|entry| entry.absolute_path(&source.root))
				.filter(|path| path.file_name().is_some_and(|name| name == "SKILL.md")),
		),
		Err(_) if source.root.exists() => warnings.push(SkillWarning {
			path:    source.root.clone(),
			message: Str::from("failed to read skills directory"),
		}),
		Err(_) => {},
	}
	files.sort();
	files
}

fn parse_skill(path: &Path) -> Result<(SkillHeader, String), serde_yaml::Error> {
	let source = fs::read_to_string(path).unwrap_or_default();
	let Some(rest) = source.strip_prefix("---\n") else {
		return Ok((SkillHeader::default(), source));
	};
	let Some((header, body)) = rest.split_once("\n---\n") else {
		return Ok((SkillHeader::default(), source));
	};
	Ok((serde_yaml::from_str(header)?, body.trim().to_owned()))
}

/// Returns whether a skill name is a safe, URL-addressable identifier.
#[must_use]
pub fn safe_skill_name(name: &str) -> bool {
	!name.is_empty()
		&& name != "."
		&& name != ".."
		&& name
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Small allocation-free wildcard matcher used for configuration globs.
/// `*` spans any bytes and `?` spans one byte; repeated stars naturally cover
/// `**` without introducing a second pattern dialect.
#[must_use]
pub fn glob_matches(pattern: &str, candidate: &str) -> bool {
	let pattern = pattern.as_bytes();
	let candidate = candidate.as_bytes();
	let (mut p, mut c, mut star, mut retry) = (0, 0, None, 0);
	while c < candidate.len() {
		if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == candidate[c]) {
			p += 1;
			c += 1;
		} else if p < pattern.len() && pattern[p] == b'*' {
			star = Some(p);
			p += 1;
			retry = c;
		} else if let Some(index) = star {
			p = index + 1;
			retry += 1;
			c = retry;
		} else {
			return false;
		}
	}
	while p < pattern.len() && pattern[p] == b'*' {
		p += 1;
	}
	p == pattern.len()
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	#[test]
	fn scans_nested_skills_and_applies_gates_before_collision() {
		let tree = tempfile::tempdir().unwrap();
		let high = tree.path().join("high");
		let low = tree.path().join("low");
		fs::create_dir_all(high.join("alpha")).unwrap();
		fs::create_dir_all(low.join("alpha")).unwrap();
		fs::write(
			high.join("alpha/SKILL.md"),
			"---\nname: alpha\ndescription: hidden\nenabled: false\n---\nhigh",
		)
		.unwrap();
		fs::write(low.join("alpha/SKILL.md"), "---\ndescription: usable\n---\nlow").unwrap();
		let sources = [
			SkillSource {
				id:                  Str::from("high"),
				root:                high,
				scope:               SourceScope::Project,
				include_root:        false,
				require_description: true,
				contain_root:        None,
				read_only:           false,
			},
			SkillSource {
				id:                  Str::from("low"),
				root:                low,
				scope:               SourceScope::User,
				include_root:        false,
				require_description: true,
				contain_root:        None,
				read_only:           false,
			},
		];
		let result = discover(&sources, &SkillDiscoverySettings::default());
		assert_eq!(result.declarations.len(), 1);
		let CapabilityPayload::Skills(skill) = &result.declarations[0].payload else {
			panic!()
		};
		assert_eq!(skill.content, "low");
	}

	#[test]
	fn wildcard_matching_is_deterministic() {
		assert!(glob_matches("rust-*", "rust-review"));
		assert!(glob_matches("*-review", "rust-review"));
		assert!(!glob_matches("go-*", "rust-review"));
	}
}
