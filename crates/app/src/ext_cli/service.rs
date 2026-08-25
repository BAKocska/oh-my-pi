use std::{collections::BTreeMap, fs, path::Path};

use futures::StreamExt as _;
use miette::{IntoDiagnostic as _, miette};
use omp_core::Str;
use omp_ext::{
	Layer as BackendLayer,
	index::SignedIndex,
	lock::{InstalledRecord, LockFile},
};

use super::{
	ExtInstallArgs, ExtUninstallArgs, ExtUpgradeArgs, Scope, StatePaths, Tier, install, uninstall,
	upgrade,
};

const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;

/// One configured signed extension index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MarketplaceIndex {
	pub(crate) name: Str,
	pub(crate) url:  String,
}

/// One extension release shown by marketplace discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MarketplacePackage {
	pub(crate) id:          Str,
	pub(crate) version:     Str,
	pub(crate) description: Str,
	pub(crate) marketplace: Str,
}

/// One installed native extension projected across user and project scopes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstalledExtensionView {
	pub(crate) id:          Str,
	pub(crate) version:     Option<Str>,
	pub(crate) enabled:     bool,
	pub(crate) scope:       Scope,
	pub(crate) marketplace: Option<Str>,
	pub(crate) shadowed:    bool,
}

/// One committed extension upgrade.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpgradeView {
	pub(crate) id:   Str,
	pub(crate) from: Option<Str>,
	pub(crate) to:   Option<Str>,
}

/// Shared signed-index and installation transactions used by both `omp ext`
/// and interactive slash commands.
pub(crate) struct ExtensionTransactions {
	state: StatePaths,
	scope: Scope,
}

impl ExtensionTransactions {
	pub(crate) fn new(data_dir: &Path, project: &Path, scope: Scope) -> Self {
		Self { state: StatePaths::new(data_dir, project), scope }
	}

	pub(crate) fn indexes(&self) -> miette::Result<Vec<MarketplaceIndex>> {
		Ok(super::read_index_config(&self.state)?
			.entries
			.into_iter()
			.map(|entry| MarketplaceIndex { name: entry.name, url: entry.url })
			.collect())
	}

	pub(crate) async fn add_index(&self, source: &str) -> miette::Result<MarketplaceIndex> {
		let source = source.trim().trim_end_matches('/');
		if source.is_empty() {
			return Err(miette!("marketplace source cannot be empty"));
		}
		let mut components = source.rsplit('/');
		let leaf = components.next().unwrap_or(source);
		let leaf = leaf.strip_suffix(".git").unwrap_or(leaf);
		let name = if matches!(leaf, "index.json" | "index") {
			components.next().unwrap_or(leaf)
		} else {
			leaf
		};
		if name.is_empty()
			|| matches!(name, "." | "..")
			|| !name
				.bytes()
				.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
		{
			return Err(miette!("marketplace source has no usable name"));
		}
		if source.contains("://") && !source.starts_with("https://") && !source.starts_with("file://")
		{
			return Err(miette!("marketplace source must use HTTPS or file://"));
		}
		let url = if source.contains("://") || source.starts_with("file:") {
			source.to_owned()
		} else {
			format!("https://raw.githubusercontent.com/{source}/HEAD/index.json")
		};
		let entry = super::IndexConfigEntry { name: Str::new(name), url };
		let previous = self
			.indexes()?
			.into_iter()
			.find(|previous| previous.name == entry.name);
		super::upsert_index(&self.state, entry.clone(), false)?;
		if let Err(error) = self.update_index(Some(entry.name.as_str())).await {
			let _ = super::remove_index(&self.state, entry.name.as_str());
			if let Some(previous) = previous {
				let _ = super::upsert_index(
					&self.state,
					super::IndexConfigEntry { name: previous.name, url: previous.url },
					false,
				);
			}
			return Err(error);
		}
		Ok(MarketplaceIndex { name: entry.name, url: entry.url })
	}

	pub(crate) fn remove_index(&self, name: &str) -> miette::Result<()> {
		super::remove_index(&self.state, name)
	}

	pub(crate) async fn update_index(&self, name: Option<&str>) -> miette::Result<Vec<Str>> {
		let indexes = self.indexes()?;
		let selected = indexes
			.into_iter()
			.filter(|entry| name.is_none_or(|name| entry.name == name))
			.collect::<Vec<_>>();
		if selected.is_empty() {
			return match name {
				Some(name) => Err(miette!("marketplace {name} is unknown")),
				None => Ok(Vec::new()),
			};
		}
		let mut updated = Vec::with_capacity(selected.len());
		for entry in selected {
			let bytes = fetch_index(&entry.url).await?;
			let parent = self
				.state
				.index_snapshot
				.parent()
				.ok_or_else(|| miette!("signed index path has no parent"))?;
			fs::create_dir_all(parent).into_diagnostic()?;
			let staged = self.state.index_snapshot.with_extension("json.tmp");
			fs::write(&staged, bytes).into_diagnostic()?;
			let key = fs::read_to_string(&self.state.index_key).into_diagnostic()?;
			let catalog = match SignedIndex::read(&staged, key.trim()) {
				Ok(catalog) => catalog,
				Err(error) => {
					let _ = fs::remove_file(&staged);
					return Err(miette!("{error}"));
				},
			};
			if catalog.name != entry.name {
				let _ = fs::remove_file(&staged);
				return Err(miette!(
					"marketplace {} served signed catalog {}",
					entry.name,
					catalog.name
				));
			}
			fs::rename(staged, &self.state.index_snapshot).into_diagnostic()?;
			updated.push(entry.name);
		}
		Ok(updated)
	}

	pub(crate) fn discover(
		&self,
		marketplace: Option<&str>,
	) -> miette::Result<Vec<MarketplacePackage>> {
		let catalog = self.catalog()?;
		if let Some(marketplace) = marketplace
			&& catalog.name != marketplace
		{
			return Err(miette!(
				"marketplace {marketplace} is not the active signed catalog; run `/marketplace update \
				 {marketplace}` first"
			));
		}
		Ok(project_catalog(&catalog, "", None, false, usize::MAX))
	}

	pub(crate) async fn install(
		&self,
		spec: &str,
		force: bool,
	) -> miette::Result<MarketplacePackage> {
		let (id, marketplace) = package_spec(spec)?;
		let catalog = self.catalog()?;
		if catalog.name != marketplace {
			return Err(miette!(
				"marketplace {marketplace} is not the active signed catalog; run `/marketplace update \
				 {marketplace}` first"
			));
		}
		let extension = catalog
			.extensions
			.iter()
			.find(|extension| extension.id == id)
			.ok_or_else(|| miette!("extension {id} is absent from marketplace {marketplace}"))?;
		let release = extension
			.releases
			.iter()
			.rev()
			.find(|release| !release.yanked)
			.ok_or_else(|| miette!("extension {id} has no eligible release"))?;
		let selected = MarketplacePackage {
			id:          extension.id.clone(),
			version:     release.version.clone(),
			description: extension.description.clone(),
			marketplace: catalog.name.clone(),
		};
		let source =
			Str::from(format!("index:{}/{}@{}", catalog.name, extension.id, release.version));
		let state = self.state.scoped(self.scope);
		install(&state, ExtInstallArgs {
			specs: vec![source],
			tier: Tier::Sandboxed,
			pool: None,
			features: None,
			capabilities: None,
			yes: false,
			dry_run: false,
			no_preresolved: false,
			target: Vec::new(),
			no_lock: false,
			force,
		})
		.await?;
		Ok(selected)
	}

	pub(crate) fn uninstall(&self, spec: &str) -> miette::Result<Str> {
		let (id, _) = package_spec(spec)?;
		let id = Str::new(id);
		uninstall(&self.state.scoped(self.scope), ExtUninstallArgs {
			ids:        vec![id.clone()],
			keep_grant: false,
			keep_lock:  false,
			purge:      false,
			dry_run:    false,
		})?;
		Ok(id)
	}

	pub(crate) fn installed(&self) -> miette::Result<Vec<InstalledExtensionView>> {
		installed_views(&self.state)
	}

	pub(crate) fn set_enabled(&self, spec: &str, enabled: bool) -> miette::Result<Str> {
		let id = spec.rsplit_once('@').map_or(spec, |(id, _)| id);
		let state = self.state.scoped(self.scope);
		super::enable(&state, id, enabled)?;
		Ok(Str::new(id))
	}

	pub(crate) async fn upgrade(&self, spec: Option<&str>) -> miette::Result<Vec<UpgradeView>> {
		let state = self.state.scoped(self.scope);
		let before = versions(&state.client_lock, state.layer)?;
		let ids = match spec {
			Some(spec) => {
				let (id, marketplace) = package_spec(spec)?;
				let catalog = self.catalog()?;
				if catalog.name != marketplace {
					return Err(miette!("marketplace {marketplace} is not the active signed catalog"));
				}
				vec![Str::new(id)]
			},
			None => Vec::new(),
		};
		upgrade(&state, ExtUpgradeArgs {
			ids: ids.clone(),
			to: None,
			dry_run: false,
			allow_capability_widening: false,
			rollback: None,
		})
		.await?;
		let after = versions(&state.client_lock, state.layer)?;
		let selected = if ids.is_empty() {
			after.keys().cloned().collect::<Vec<_>>()
		} else {
			ids
		};
		Ok(selected
			.into_iter()
			.filter_map(|id| {
				let from = before.get(&id).cloned();
				let to = after.get(&id).cloned();
				(from != to).then_some(UpgradeView { id, from, to })
			})
			.collect())
	}

	fn catalog(&self) -> miette::Result<SignedIndex> {
		read_catalog(&self.state)
	}
}

pub(super) fn catalog_packages(
	state: &StatePaths,
	query: &str,
	capability: Option<&str>,
	attested: bool,
	limit: usize,
) -> miette::Result<Vec<MarketplacePackage>> {
	let catalog = read_catalog(state)?;
	Ok(project_catalog(&catalog, query, capability, attested, limit))
}

fn project_catalog(
	catalog: &SignedIndex,
	query: &str,
	capability: Option<&str>,
	attested: bool,
	limit: usize,
) -> Vec<MarketplacePackage> {
	catalog
		.search(query, capability, attested)
		.take(limit)
		.map(|(extension, release)| MarketplacePackage {
			id:          extension.id.clone(),
			version:     release.version.clone(),
			description: extension.description.clone(),
			marketplace: catalog.name.clone(),
		})
		.collect()
}

fn read_catalog(state: &StatePaths) -> miette::Result<SignedIndex> {
	let key = fs::read_to_string(&state.index_key).into_diagnostic()?;
	SignedIndex::read(&state.index_snapshot, key.trim()).map_err(|error| miette!("{error}"))
}

pub(super) fn installed_views(state: &StatePaths) -> miette::Result<Vec<InstalledExtensionView>> {
	let client =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	let workspace =
		InstalledRecord::read(&state.workspace_installed).map_err(|error| miette!("{error}"))?;
	let client_versions = versions(&state.client_lock, BackendLayer::Client)?;
	let workspace_versions = versions(&state.workspace_lock, BackendLayer::Workspace)?;
	let project_ids = workspace
		.extensions
		.iter()
		.filter(|entry| entry.enabled)
		.map(|entry| entry.id.clone())
		.collect::<std::collections::BTreeSet<_>>();
	let mut entries = Vec::with_capacity(client.extensions.len() + workspace.extensions.len());
	entries.extend(
		client
			.extensions
			.into_iter()
			.map(|entry| InstalledExtensionView {
				version:     client_versions.get(&entry.id).cloned(),
				marketplace: source_index(&entry.source),
				shadowed:    project_ids.contains(&entry.id),
				id:          entry.id,
				enabled:     entry.enabled,
				scope:       Scope::User,
			}),
	);
	entries.extend(
		workspace
			.extensions
			.into_iter()
			.map(|entry| InstalledExtensionView {
				version:     workspace_versions.get(&entry.id).cloned(),
				marketplace: source_index(&entry.source),
				shadowed:    false,
				id:          entry.id,
				enabled:     entry.enabled,
				scope:       Scope::Project,
			}),
	);
	entries.sort_by(|left, right| {
		left
			.id
			.cmp(&right.id)
			.then(scope_order(left.scope).cmp(&scope_order(right.scope)))
	});
	Ok(entries)
}

fn package_spec(spec: &str) -> miette::Result<(&str, &str)> {
	let (id, marketplace) = spec
		.rsplit_once('@')
		.ok_or_else(|| miette!("package must use `name@marketplace` syntax"))?;
	if id.is_empty() || marketplace.is_empty() {
		return Err(miette!("package must use `name@marketplace` syntax"));
	}
	Ok((id, marketplace))
}
const fn scope_order(scope: Scope) -> u8 {
	match scope {
		Scope::User => 1,
		Scope::Project => 0,
	}
}

fn versions(path: &Path, layer: BackendLayer) -> miette::Result<BTreeMap<Str, Str>> {
	if !path.exists() {
		return Ok(BTreeMap::new());
	}
	let lock = LockFile::read(path, layer).map_err(|error| miette!("{error}"))?;
	Ok(lock
		.extensions
		.into_iter()
		.map(|entry| (entry.id, entry.version))
		.collect())
}

fn source_index(source: &toml::Value) -> Option<Str> {
	source
		.get("index")
		.and_then(toml::Value::as_str)
		.filter(|index| !index.is_empty())
		.map(Str::new)
}

async fn fetch_index(url: &str) -> miette::Result<Vec<u8>> {
	if let Some(path) = url.strip_prefix("file://") {
		return fs::read(path).into_diagnostic();
	}
	if !url.starts_with("https://") {
		return Err(miette!("marketplace index URL must use HTTPS or file://"));
	}
	let response = wreq::Client::new()
		.get(url)
		.send()
		.await
		.into_diagnostic()?;
	if !response.status().is_success() {
		return Err(miette!("marketplace update returned HTTP {}", response.status()));
	}
	let mut bytes = Vec::new();
	let mut stream = response.bytes_stream();
	while let Some(chunk) = stream.next().await {
		let chunk = chunk.into_diagnostic()?;
		if bytes.len().saturating_add(chunk.len()) > MAX_INDEX_BYTES {
			return Err(miette!("marketplace index exceeds the 16 MiB safety limit"));
		}
		bytes.extend_from_slice(&chunk);
	}
	Ok(bytes)
}
#[cfg(test)]
mod tests {
	use omp_ext::{TrustTier, lock::InstalledExtension};

	use super::*;

	fn installed(id: &'static str, enabled: bool) -> InstalledExtension {
		InstalledExtension {
			id: Str::new_static(id),
			source: toml::Value::Table(toml::map::Map::new()),
			tier: TrustTier::Sandboxed,
			enabled,
		}
	}

	#[test]
	fn scoped_enable_mutates_the_cli_project_record() {
		let temp = tempfile::tempdir().unwrap();
		let data_dir = temp.path().join("data");
		let project = temp.path().join("project");
		let state = StatePaths::new(&data_dir, &project);
		let user = InstalledRecord { version: 1, extensions: vec![installed("sample", false)] };
		let project_record =
			InstalledRecord { version: 1, extensions: vec![installed("sample", false)] };
		user.write(&state.client_installed).unwrap();
		project_record.write(&state.workspace_installed).unwrap();

		ExtensionTransactions::new(&data_dir, &project, Scope::Project)
			.set_enabled("sample@index", true)
			.unwrap();

		let user = InstalledRecord::read(&state.client_installed).unwrap();
		let project_record = InstalledRecord::read(&state.workspace_installed).unwrap();
		assert!(!user.extensions[0].enabled);
		assert!(project_record.extensions[0].enabled);
	}

	#[test]
	fn installed_projection_marks_user_entries_shadowed_by_project() {
		let temp = tempfile::tempdir().unwrap();
		let data_dir = temp.path().join("data");
		let project = temp.path().join("project");
		let state = StatePaths::new(&data_dir, &project);
		InstalledRecord { version: 1, extensions: vec![installed("sample", true)] }
			.write(&state.client_installed)
			.unwrap();
		InstalledRecord { version: 1, extensions: vec![installed("sample", true)] }
			.write(&state.workspace_installed)
			.unwrap();

		let entries = ExtensionTransactions::new(&data_dir, &project, Scope::User)
			.installed()
			.unwrap();

		assert_eq!(entries.len(), 2);
		assert!(
			entries
				.iter()
				.any(|entry| entry.scope == Scope::User && entry.shadowed)
		);
		assert!(
			entries
				.iter()
				.any(|entry| entry.scope == Scope::Project && !entry.shadowed)
		);
	}
}
