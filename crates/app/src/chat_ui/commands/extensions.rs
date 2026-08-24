use omp_core::Str;

mod inspector_model;

pub(crate) use inspector_model::{
	build_inspector_snapshot_from_declarations, snapshot_live_mcp,
};

use super::{ConfigScope, ExtensionRequest, MarketplaceRequest, PluginRequest, command};

command!(extensions, 630, "extensions", [], "Inspect discovered extensions and live MCP catalogs", [Workspace, Owner], false, typed("", [], parse_extensions) => |host, _parsed| host.extensions(ExtensionRequest::Inspect));
command!(marketplace, 640, "marketplace", [], "Manage signed extension indexes and packages", [Workspace, Owner], false, typed("[add|remove|update|list|discover|install|uninstall|installed|upgrade|help]", ["add", "remove", "update", "list", "discover", "install", "uninstall", "installed", "upgrade", "help", "--force", "--scope"], parse_marketplace) => |host, request| host.extensions(ExtensionRequest::Marketplace(request)));
command!(plugins, 650, "plugins", [], "List, enable, or disable native extensions", [Workspace, Owner], false, typed("[list|enable|disable]", ["list", "enable", "disable"], parse_plugins) => |host, request| host.extensions(ExtensionRequest::Plugins(request)));
command!(reload_plugins, 660, "reload-plugins", [], "Rediscover and reload native extensions", [Workspace, Owner], false, none => |host| host.extensions(ExtensionRequest::Reload));

pub(super) fn parse_extensions(raw: &str) -> miette::Result<()> {
	if raw.trim().is_empty() {
		Ok(())
	} else {
		Err(miette::miette!("usage: /extensions"))
	}
}

fn parse_marketplace(raw: &str) -> miette::Result<MarketplaceRequest> {
	let mut words = raw.split_whitespace();
	let operation = words.next().unwrap_or("list");
	match operation {
		"list" => none(words, MarketplaceRequest::List),
		"installed" => none(words, MarketplaceRequest::Installed),
		"help" => none(words, MarketplaceRequest::Help),
		"add" => one(words, MarketplaceRequest::Add),
		"remove" => one(words, MarketplaceRequest::Remove),
		"update" => optional_one(words, MarketplaceRequest::Update),
		"discover" => optional_one(words, MarketplaceRequest::Discover),
		"install" => parse_package(words, true, |spec, scope, force| MarketplaceRequest::Install {
			spec,
			scope,
			force,
		}),
		"uninstall" => {
			parse_package(words, false, |spec, scope, _| MarketplaceRequest::Uninstall { spec, scope })
		},
		"upgrade" => parse_upgrade(words),
		_ => Err(usage()),
	}
}

fn parse_plugins(raw: &str) -> miette::Result<PluginRequest> {
	let mut words = raw.split_whitespace();
	match words.next().unwrap_or("list") {
		"list" if words.next().is_none() => Ok(PluginRequest::List),
		"enable" => one(words, PluginRequest::Enable),
		"disable" => one(words, PluginRequest::Disable),
		_ => Err(miette::miette!("usage: /plugins [list|enable <name>|disable <name>]")),
	}
}

fn parse_package<'a>(
	words: impl Iterator<Item = &'a str>,
	allow_force: bool,
	build: impl FnOnce(Str, ConfigScope, bool) -> MarketplaceRequest,
) -> miette::Result<MarketplaceRequest> {
	let mut scope = ConfigScope::User;
	let mut force = false;
	let mut spec = None;
	let mut words = words.peekable();
	while let Some(word) = words.next() {
		match word {
			"--force" if allow_force => force = true,
			"--scope" => {
				scope = match words.next() {
					Some("user") => ConfigScope::User,
					Some("project") => ConfigScope::Project,
					_ => return Err(miette::miette!("--scope must be `user` or `project`")),
				};
			},
			value if value.starts_with("--") => {
				return Err(miette::miette!("unknown marketplace option `{value}`"));
			},
			value if spec.is_none() => spec = Some(Str::new(value)),
			_ => return Err(usage()),
		}
	}
	let spec = spec.ok_or_else(usage)?;
	if !spec.contains('@') {
		return Err(miette::miette!("package must use `name@marketplace` syntax"));
	}
	Ok(build(spec, scope, force))
}

fn parse_upgrade<'a>(words: impl Iterator<Item = &'a str>) -> miette::Result<MarketplaceRequest> {
	let mut scope = ConfigScope::User;
	let mut spec = None;
	let mut words = words.peekable();
	while let Some(word) = words.next() {
		match word {
			"--scope" => {
				scope = match words.next() {
					Some("user") => ConfigScope::User,
					Some("project") => ConfigScope::Project,
					_ => return Err(miette::miette!("--scope must be `user` or `project`")),
				};
			},
			value if value.starts_with("--") || spec.is_some() => return Err(usage()),
			value => spec = Some(Str::new(value)),
		}
	}
	if spec.as_ref().is_some_and(|spec| !spec.contains('@')) {
		return Err(miette::miette!("package must use `name@marketplace` syntax"));
	}
	Ok(MarketplaceRequest::Upgrade { spec, scope })
}

fn none<'a>(
	mut words: impl Iterator<Item = &'a str>,
	request: MarketplaceRequest,
) -> miette::Result<MarketplaceRequest> {
	if words.next().is_none() {
		Ok(request)
	} else {
		Err(usage())
	}
}

fn one<'a, T>(
	mut words: impl Iterator<Item = &'a str>,
	build: impl FnOnce(Str) -> T,
) -> miette::Result<T> {
	let value = words.next().ok_or_else(usage)?;
	if words.next().is_some() {
		Err(usage())
	} else {
		Ok(build(Str::new(value)))
	}
}

fn optional_one<'a, T>(
	mut words: impl Iterator<Item = &'a str>,
	build: impl FnOnce(Option<Str>) -> T,
) -> miette::Result<T> {
	let value = words.next().map(Str::new);
	if words.next().is_some() {
		Err(usage())
	} else {
		Ok(build(value))
	}
}

fn usage() -> miette::Report {
	miette::miette!(
		"usage: /marketplace \
		 add|remove|update|list|discover|install|uninstall|installed|upgrade|help"
	)
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn extensions_accepts_only_an_empty_argument_tail() {
		assert!(parse_extensions("").is_ok());
		assert!(parse_extensions("  \t").is_ok());
		assert!(parse_extensions("list").is_err());
		assert!(parse_extensions("--json").is_err());
	}
}