//! Bounded discovery of native static assets.

use std::{
	io::Read,
	path::{Path, PathBuf},
};

use omp_core::Str;
use omp_walker::WalkRequest;
use thiserror::Error;

/// Configuration scope that supplied an asset root.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DiscoveryScope {
	/// Process-level native configuration.
	Global,
	/// Canonical project configuration.
	Project,
	/// Explicit session-only input.
	Session,
}

/// Static native content understood by SDK discovery.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AssetKind {
	/// Native static extension manifest.
	Extension,
	/// Skill document.
	Skill,
	/// Persistent context file.
	Context,
	/// Reusable prompt template.
	Template,
	/// Markdown slash command.
	Command,
	/// Static MCP declaration.
	Mcp,
}

/// One discovery root and the asset families accepted beneath it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryRequest {
	/// Authority scope of this root.
	pub root:         PathBuf,
	/// Scope precedence assigned by the host.
	pub source_scope: DiscoveryScope,
	/// Accepted asset families.
	pub kinds:        Box<[AssetKind]>,
}

/// One immutable bounded native asset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeAsset {
	/// Asset family.
	pub kind:         AssetKind,
	/// Configuration scope.
	pub source_scope: DiscoveryScope,
	/// Exact source path.
	pub path:         PathBuf,
	/// Validated UTF-8 source bytes.
	pub content:      Str,
}

/// Native asset discovery failure.
#[derive(Debug, Error)]
pub enum DiscoveryError {
	/// A source file could not be read.
	#[error("failed to read discovered asset {path:?}")]
	Read {
		/// Exact source path.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: std::io::Error,
	},
	/// A source file is not UTF-8.
	#[error("discovered asset is not UTF-8: {path:?}")]
	Encoding {
		/// Exact source path.
		path:   PathBuf,
		/// UTF-8 validation failure.
		#[source]
		source: std::str::Utf8Error,
	},
	/// One asset exceeds the per-file byte ceiling.
	#[error("discovered asset exceeds {limit} bytes: {path:?}")]
	AssetBudget {
		/// Exact source path.
		path:  PathBuf,
		/// Per-file ceiling.
		limit: usize,
	},
	/// All accepted assets exceed the aggregate byte ceiling.
	#[error("discovered assets exceed aggregate budget of {limit} bytes")]
	TotalBudget {
		/// Aggregate ceiling.
		limit: usize,
	},
	/// Native traversal failed.
	#[error("native asset traversal failed for {root:?}")]
	Walk {
		/// Discovery root.
		root: PathBuf,
	},
}

/// Deterministic bounded native asset loader.
#[derive(Clone, Debug)]
pub struct DiscoveryLoader {
	max_assets:          usize,
	max_asset_bytes:     usize,
	max_aggregate_bytes: usize,
}

impl DiscoveryLoader {
	/// Creates a loader with production-safe discovery bounds.
	#[must_use]
	pub const fn new() -> Self {
		Self {
			max_assets:          2_000,
			max_asset_bytes:     1024 * 1024,
			max_aggregate_bytes: 16 * 1024 * 1024,
		}
	}

	/// Loads all requested roots in precedence order.
	pub fn load(&self, requests: &[DiscoveryRequest]) -> Result<Vec<NativeAsset>, DiscoveryError> {
		let mut assets = Vec::new();
		let mut total_bytes = 0usize;
		for request in requests {
			let outcome = WalkRequest::new(&request.root)
				.hidden(true)
				.gitignore(false)
				.skip_git(true)
				.depth(1, 16)
				.limit(self.max_assets.saturating_sub(assets.len()))
				.collect()
				.map_err(|_| DiscoveryError::Walk { root: request.root.clone() })?;
			let mut candidates = outcome
				.entries
				.into_iter()
				.filter(|entry| entry.is_file())
				.filter_map(|entry| {
					classify(&entry.path, &request.kinds)
						.map(|kind| (kind, entry.absolute_path(&request.root)))
				})
				.collect::<Vec<_>>();
			candidates.sort_by(|left, right| left.1.cmp(&right.1));
			for (kind, path) in candidates {
				if assets.len() >= self.max_assets {
					break;
				}
				let content = read_bounded(&path, self.max_asset_bytes)?;
				total_bytes = total_bytes.saturating_add(content.len());
				if total_bytes > self.max_aggregate_bytes {
					return Err(DiscoveryError::TotalBudget { limit: self.max_aggregate_bytes });
				}
				assets.push(NativeAsset {
					kind,
					source_scope: request.source_scope,
					path,
					content: Str::from(content),
				});
			}
		}
		Ok(assets)
	}
}

impl Default for DiscoveryLoader {
	fn default() -> Self {
		Self::new()
	}
}

fn read_bounded(path: &Path, limit: usize) -> Result<String, DiscoveryError> {
	let file = std::fs::File::open(path)
		.map_err(|source| DiscoveryError::Read { path: path.to_owned(), source })?;
	let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
	file
		.take(limit as u64 + 1)
		.read_to_end(&mut bytes)
		.map_err(|source| DiscoveryError::Read { path: path.to_owned(), source })?;
	if bytes.len() > limit {
		return Err(DiscoveryError::AssetBudget { path: path.to_owned(), limit });
	}
	let content = std::str::from_utf8(&bytes)
		.map_err(|source| DiscoveryError::Encoding { path: path.to_owned(), source })?;
	Ok(content.to_owned())
}

fn classify(path: &str, accepted: &[AssetKind]) -> Option<AssetKind> {
	let file = path.rsplit('/').next().unwrap_or(path);
	let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
	let kind = if file == "extension.json" {
		AssetKind::Extension
	} else if file == "SKILL.md" {
		AssetKind::Skill
	} else if matches!(file, "AGENTS.md" | "RULES.md")
		|| (directory_named(parent, "instructions") && file.ends_with(".md"))
	{
		AssetKind::Context
	} else if matches!(file, "mcp.json" | ".mcp.json") || directory_named(parent, "mcp") {
		AssetKind::Mcp
	} else if (directory_named(parent, "prompts") || directory_named(parent, "templates"))
		&& file.ends_with(".md")
	{
		AssetKind::Template
	} else if directory_named(parent, "commands") && file.ends_with(".md") {
		AssetKind::Command
	} else {
		return None;
	};
	accepted.contains(&kind).then_some(kind)
}

fn directory_named(path: &str, name: &str) -> bool {
	path.rsplit('/').next() == Some(name)
}
