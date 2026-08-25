use std::path::Path;

use omp_core::{Str, sf};

use super::{ConfigScope, ExtensionRequest, MarketplaceRequest, PluginRequest};
use crate::ext_cli::{
	Scope,
	service::{ExtensionTransactions, InstalledExtensionView},
};

/// Result of one native extension command transaction.
pub(crate) struct ExtensionCommandOutput {
	pub(crate) status: Str,
	pub(crate) reload: bool,
}

/// Executes marketplace and installed-extension operations against the same
/// durable transactions as `omp ext`.
pub(crate) async fn execute(
	request: ExtensionRequest,
	data_dir: &Path,
	project: &Path,
) -> miette::Result<ExtensionCommandOutput> {
	match request {
		ExtensionRequest::Marketplace(request) => marketplace(request, data_dir, project).await,
		ExtensionRequest::Plugins(request) => plugins(request, data_dir, project),
		ExtensionRequest::Inspect | ExtensionRequest::Reload => {
			Err(miette::miette!("extension request belongs to the live host adapter"))
		},
	}
}

async fn marketplace(
	request: MarketplaceRequest,
	data_dir: &Path,
	project: &Path,
) -> miette::Result<ExtensionCommandOutput> {
	let scope = marketplace_scope(&request);
	let transactions = ExtensionTransactions::new(data_dir, project, scope);
	let (status, reload) = match request {
		MarketplaceRequest::List => {
			let indexes = transactions.indexes()?;
			if indexes.is_empty() {
				(
					sf!(
						"No marketplaces configured.\n\nGet started:\n  /marketplace add \
						 <signed-index-url>\n\nThen browse with /marketplace discover"
					),
					false,
				)
			} else {
				let lines = indexes
					.into_iter()
					.map(|entry| format!("  {}  {}", entry.name, entry.url))
					.collect::<Vec<_>>()
					.join("\n");
				(
					sf!(
						"Marketplaces:\n{lines}\n\nUse /marketplace discover to browse plugins, or \
						 /marketplace help for all commands"
					),
					false,
				)
			}
		},
		MarketplaceRequest::Add(source) => {
			let entry = transactions.add_index(source.as_str()).await?;
			(sf!("Added marketplace: {}", entry.name), false)
		},
		MarketplaceRequest::Remove(name) => {
			transactions.remove_index(name.as_str())?;
			(sf!("Removed marketplace: {name}"), false)
		},
		MarketplaceRequest::Update(name) => {
			let updated = transactions.update_index(name.as_deref()).await?;
			if let Some(name) = name {
				(sf!("Updated marketplace: {name}"), false)
			} else {
				(sf!("Updated {} marketplace(s)", updated.len()), false)
			}
		},
		MarketplaceRequest::Discover(marketplace) => {
			if transactions.indexes()?.is_empty() {
				(sf!("No marketplaces configured. Try:\n  /marketplace add <signed-index-url>"), false)
			} else {
				let packages = transactions.discover(marketplace.as_deref())?;
				if packages.is_empty() {
					(sf!("No plugins available in configured marketplaces"), false)
				} else {
					let mut lines = vec!["Available plugins:".to_owned()];
					for package in packages {
						lines.push(format!(
							"  - {}@{} ({})",
							package.id, package.marketplace, package.version
						));
						if !package.description.is_empty() {
							lines.push(format!("      {}", package.description));
						}
					}
					(Str::from(lines.join("\n")), false)
				}
			}
		},
		MarketplaceRequest::Install { spec, force, .. } => {
			let package = transactions.install(spec.as_str(), force).await?;
			(sf!("Installed {} from {}", package.id, package.marketplace), true)
		},
		MarketplaceRequest::Uninstall { spec, .. } => {
			let id = transactions.uninstall(spec.as_str())?;
			(sf!("Uninstalled {id}"), true)
		},
		MarketplaceRequest::Installed => {
			let installed = transactions.installed()?;
			if installed.is_empty() {
				(sf!("No marketplace plugins installed"), false)
			} else {
				let lines = installed
					.iter()
					.map(render_installed)
					.collect::<Vec<_>>()
					.join("\n");
				(sf!("Installed plugins:\n{lines}"), false)
			}
		},
		MarketplaceRequest::Upgrade { spec, .. } => {
			let upgraded = transactions.upgrade(spec.as_deref()).await?;
			if upgraded.is_empty() {
				(sf!("All marketplace plugins are up to date"), false)
			} else if let Some(spec) = spec {
				let version = upgraded
					.first()
					.and_then(|entry| entry.to.as_ref())
					.map_or("?", Str::as_str);
				(sf!("Upgraded {spec} to {version}"), true)
			} else {
				let lines = upgraded
					.iter()
					.map(|entry| {
						format!(
							"  {}: {} -> {}",
							entry.id,
							entry.from.as_ref().map_or("?", Str::as_str),
							entry.to.as_ref().map_or("?", Str::as_str)
						)
					})
					.collect::<Vec<_>>()
					.join("\n");
				(sf!("Upgraded {} plugin(s):\n{lines}", upgraded.len()), true)
			}
		},
		MarketplaceRequest::Help => (
			sf!(
				"Marketplace commands:\n  /marketplace                              List configured \
				 marketplaces\n  /marketplace add <source>                 Add a signed index \
				 source\n  /marketplace remove <name>              Remove a marketplace\n  \
				 /marketplace update [name]              Re-fetch signed catalog(s)\n  /marketplace \
				 list                       List configured marketplaces\n  /marketplace discover \
				 [marketplace]      Browse available plugins\n  /marketplace install [--force] \
				 [--scope user|project] <name@marketplace>\n  /marketplace uninstall [--scope \
				 user|project] <name@marketplace>\n  /marketplace installed                   List \
				 installed plugins\n  /marketplace upgrade [--scope user|project] [name@marketplace]"
			),
			false,
		),
	};
	Ok(ExtensionCommandOutput { status, reload })
}

fn plugins(
	request: PluginRequest,
	data_dir: &Path,
	project: &Path,
) -> miette::Result<ExtensionCommandOutput> {
	let (status, reload) = match request {
		PluginRequest::List => {
			let installed = ExtensionTransactions::new(data_dir, project, Scope::User).installed()?;
			if installed.is_empty() {
				(sf!("No plugins installed"), false)
			} else {
				let lines = installed
					.iter()
					.map(render_installed)
					.collect::<Vec<_>>()
					.join("\n");
				(sf!("native plugins:\n{lines}"), false)
			}
		},
		PluginRequest::Enable { id, scope } => {
			let id = ExtensionTransactions::new(data_dir, project, ext_scope(scope))
				.set_enabled(id.as_str(), true)?;
			(sf!("Enabled {id}"), true)
		},
		PluginRequest::Disable { id, scope } => {
			let id = ExtensionTransactions::new(data_dir, project, ext_scope(scope))
				.set_enabled(id.as_str(), false)?;
			(sf!("Disabled {id}"), true)
		},
	};
	Ok(ExtensionCommandOutput { status, reload })
}

fn marketplace_scope(request: &MarketplaceRequest) -> Scope {
	match request {
		MarketplaceRequest::Install { scope, .. }
		| MarketplaceRequest::Uninstall { scope, .. }
		| MarketplaceRequest::Upgrade { scope, .. } => ext_scope(*scope),
		_ => Scope::User,
	}
}

const fn ext_scope(scope: ConfigScope) -> Scope {
	match scope {
		ConfigScope::User => Scope::User,
		ConfigScope::Project => Scope::Project,
	}
}

fn render_installed(entry: &InstalledExtensionView) -> String {
	let marketplace = entry
		.marketplace
		.as_ref()
		.map_or_else(String::new, |marketplace| format!("@{marketplace}"));
	let version = entry.version.as_ref().map_or("?", Str::as_str);
	let disabled = if entry.enabled { "" } else { " (disabled)" };
	let shadowed = if entry.shadowed { " [shadowed]" } else { "" };
	let scope = match entry.scope {
		Scope::User => "user",
		Scope::Project => "project",
	};
	format!("  {}{} v{}{} [{}]{}", entry.id, marketplace, version, disabled, scope, shadowed)
}
