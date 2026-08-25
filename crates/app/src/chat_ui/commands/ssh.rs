//! Structural native SSH-host management routes.

use std::{
	collections::BTreeSet,
	fmt::Write as _,
	path::{Path, PathBuf},
};

use miette::IntoDiagnostic as _;
use omp_core::{Str, sf};
use omp_envd::ssh::{AuthPolicy, HostConfig, HostStore};

use super::{ConfigScope, SshRequest, command};

command!(ssh, 665, "ssh", icon: Host, [], "Manage SSH hosts (add, list, remove)", [Workspace, Owner], false, typed("<list|add|remove|help>", ["list", "add", "remove", "help", "--scope", "--host", "--user", "--port", "--host-key", "--key"], parse_ssh) => |host, request| host.ssh(request));

/// Executes one scoped SSH host declaration operation.
pub(crate) fn execute(
	request: SshRequest,
	workspace_root: &Path,
	data_dir: &Path,
) -> miette::Result<Str> {
	let project = workspace_root.join(".omp/hosts.toml");
	let user = data_dir.join("hosts.toml");
	match request {
		SshRequest::List(scope) => list_hosts(scope, &project, &user),
		SshRequest::Add { alias, host, user: remote_user, port, host_key, key, scope } => {
			let path = scope_path(scope, &project, &user);
			HostStore::load(path)
				.into_diagnostic()?
				.upsert(path, alias.clone(), HostConfig {
					address: host,
					port,
					user: remote_user,
					host_key,
					auth: key.map_or(AuthPolicy::Agent, |path| AuthPolicy::Key { path }),
					timeout_secs: 30,
				})
				.into_diagnostic()?;
			Ok(sf!("Configured SSH host `{alias}` in {}.", path.display()))
		},
		SshRequest::Remove { alias, scope } => {
			let path = scope_path(scope, &project, &user);
			let removed = HostStore::load(path)
				.into_diagnostic()?
				.remove(path, alias.as_str())
				.into_diagnostic()?;
			if !removed {
				return Err(miette::miette!(
					"SSH host `{alias}` is not configured in {}",
					path.display()
				));
			}
			Ok(sf!("Removed SSH host `{alias}` from {}.", path.display()))
		},
		SshRequest::Help => Ok(Str::new_static(
			"**SSH host management**\n\n`/ssh list [--scope user|project]`\n`/ssh add <alias> --host \
			 <host> --user <user> --host-key <SHA256:fingerprint> [--port <1-65535>] [--key <path>] \
			 [--scope user|project]`\n`/ssh remove <alias> [--scope user|project]`\n`/ssh \
			 help`\n\nProject declarations are stored in `.omp/hosts.toml`; user declarations are \
			 stored in the OMP data directory's `hosts.toml`. Project aliases take precedence.",
		)),
	}
}

fn list_hosts(scope: Option<ConfigScope>, project: &Path, user: &Path) -> miette::Result<Str> {
	let mut rendered = String::from("**Configured SSH hosts**\n");
	let mut count = 0_usize;
	match scope {
		Some(scope) => {
			let (label, path) = scoped_path(scope, project, user);
			append_hosts(&mut rendered, label, path, None, &mut count)?;
		},
		None => {
			let project_store = HostStore::load(project).into_diagnostic()?;
			let project_aliases = project_store.aliases().into_iter().collect::<BTreeSet<_>>();
			append_store(&mut rendered, "project", &project_store, None, &mut count)?;
			let user_store = HostStore::load(user).into_diagnostic()?;
			append_store(&mut rendered, "user", &user_store, Some(&project_aliases), &mut count)?;
		},
	}
	if count == 0 {
		Ok(Str::new_static("No SSH hosts configured. Use `/ssh add` to add one."))
	} else {
		Ok(Str::from(rendered))
	}
}

fn append_hosts(
	rendered: &mut String,
	label: &str,
	path: &Path,
	hidden: Option<&BTreeSet<Str>>,
	count: &mut usize,
) -> miette::Result<()> {
	let store = HostStore::load(path).into_diagnostic()?;
	append_store(rendered, label, &store, hidden, count)
}

fn append_store(
	rendered: &mut String,
	label: &str,
	store: &HostStore,
	hidden: Option<&BTreeSet<Str>>,
	count: &mut usize,
) -> miette::Result<()> {
	for alias in store.aliases() {
		if hidden.is_some_and(|aliases| aliases.contains(&alias)) {
			continue;
		}
		let host = store.get(alias.as_str()).into_diagnostic()?;
		let _ = writeln!(
			rendered,
			"- `{alias}` ({label}) — `{}@{}:{}`",
			host.user, host.address, host.port
		);
		*count += 1;
	}
	Ok(())
}

fn scope_path<'a>(scope: ConfigScope, project: &'a Path, user: &'a Path) -> &'a Path {
	scoped_path(scope, project, user).1
}

fn scoped_path<'a>(
	scope: ConfigScope,
	project: &'a Path,
	user: &'a Path,
) -> (&'static str, &'a Path) {
	match scope {
		ConfigScope::Project => ("project", project),
		ConfigScope::User => ("user", user),
	}
}

fn parse_ssh(raw: &str) -> miette::Result<SshRequest> {
	let mut words = raw.split_whitespace();
	match words.next() {
		None => Ok(SshRequest::Help),
		Some("list") => parse_list(words),
		Some("add") => parse_add(words),
		Some("remove" | "rm") => parse_remove(words),
		Some("help") if words.next().is_none() => Ok(SshRequest::Help),
		_ => Err(usage()),
	}
}

fn parse_list<'a>(mut words: impl Iterator<Item = &'a str>) -> miette::Result<SshRequest> {
	let scope = match words.next() {
		None => None,
		Some("--scope") => Some(parse_scope_value(words.next())?),
		Some(flag) => return Err(miette::miette!("unknown /ssh list flag `{flag}`")),
	};
	if words.next().is_some() {
		return Err(usage());
	}
	Ok(SshRequest::List(scope))
}

fn parse_add<'a>(mut words: impl Iterator<Item = &'a str>) -> miette::Result<SshRequest> {
	let alias = Str::new(words.next().ok_or_else(usage)?);
	let mut scope = ConfigScope::Project;
	let mut host = None;
	let mut user = None;
	let mut port = 22_u16;
	let mut host_key = None;
	let mut key = None;
	while let Some(flag) = words.next() {
		match flag {
			"--scope" => scope = parse_scope_value(words.next())?,
			"--host" => host = Some(Str::new(words.next().ok_or_else(usage)?)),
			"--user" => user = Some(Str::new(words.next().ok_or_else(usage)?)),
			"--port" => port = parse_port(words.next())?,
			"--host-key" => host_key = Some(Str::new(words.next().ok_or_else(usage)?)),
			"--key" => key = Some(PathBuf::from(words.next().ok_or_else(usage)?)),
			_ => return Err(miette::miette!("unknown /ssh add flag `{flag}`")),
		}
	}
	let host_key = host_key.ok_or_else(usage)?;
	if !host_key.starts_with("SHA256:") {
		return Err(miette::miette!("--host-key must be a SHA256: fingerprint"));
	}
	Ok(SshRequest::Add {
		alias,
		host: host.ok_or_else(usage)?,
		user: user.ok_or_else(usage)?,
		port,
		host_key,
		key,
		scope,
	})
}

fn parse_remove<'a>(mut words: impl Iterator<Item = &'a str>) -> miette::Result<SshRequest> {
	let alias = Str::new(words.next().ok_or_else(usage)?);
	let scope = match words.next() {
		None => ConfigScope::Project,
		Some("--scope") => parse_scope_value(words.next())?,
		Some(flag) => return Err(miette::miette!("unknown /ssh remove flag `{flag}`")),
	};
	if words.next().is_some() {
		return Err(usage());
	}
	Ok(SshRequest::Remove { alias, scope })
}

fn parse_scope_value(value: Option<&str>) -> miette::Result<ConfigScope> {
	match value {
		Some("user") => Ok(ConfigScope::User),
		Some("project") => Ok(ConfigScope::Project),
		_ => Err(miette::miette!("--scope must be `user` or `project`")),
	}
}

fn parse_port(value: Option<&str>) -> miette::Result<u16> {
	let port = value
		.ok_or_else(usage)?
		.parse::<u16>()
		.map_err(|_| miette::miette!("SSH port must be an integer between 1 and 65535"))?;
	if port == 0 {
		Err(miette::miette!("SSH port must be an integer between 1 and 65535"))
	} else {
		Ok(port)
	}
}

fn usage() -> miette::Report {
	miette::miette!(
		"usage: /ssh list [--scope user|project]|add <alias> --host <host> --user <user> --host-key \
		 <SHA256:fingerprint> [--port <port>] [--key <path>] [--scope user|project]|remove <alias> \
		 [--scope user|project]|help"
	)
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	const HOST_KEY: &str = "SHA256:test-fingerprint";

	fn add(alias: &str, address: &str, scope: ConfigScope) -> SshRequest {
		SshRequest::Add {
			alias: Str::new(alias),
			host: Str::new(address),
			user: Str::new_static("remote"),
			port: 22,
			host_key: Str::new_static(HOST_KEY),
			key: None,
			scope,
		}
	}

	#[test]
	fn scoped_mutations_persist_and_project_aliases_win() {
		let temp = tempfile::tempdir().unwrap();
		let workspace = temp.path().join("workspace");
		let data = temp.path().join("data");
		fs::create_dir_all(&workspace).unwrap();
		fs::create_dir_all(&data).unwrap();

		execute(add("shared", "user.example", ConfigScope::User), &workspace, &data).unwrap();
		execute(add("shared", "project.example", ConfigScope::Project), &workspace, &data).unwrap();
		execute(add("user-only", "only.example", ConfigScope::User), &workspace, &data).unwrap();

		let effective = execute(SshRequest::List(None), &workspace, &data).unwrap();
		assert!(effective.contains("project.example"));
		assert!(!effective.contains("user.example"));
		assert!(effective.contains("only.example"));
		assert!(workspace.join(".omp/hosts.toml").is_file());
		assert!(data.join("hosts.toml").is_file());

		execute(
			SshRequest::Remove { alias: Str::new_static("shared"), scope: ConfigScope::Project },
			&workspace,
			&data,
		)
		.unwrap();
		let reloaded = HostStore::load(&workspace.join(".omp/hosts.toml")).unwrap();
		assert!(reloaded.get("shared").is_err());
	}

	#[test]
	fn parser_rejects_ports_outside_tcp_range() {
		for port in ["0", "65536", "not-a-port"] {
			let error = parse_ssh(&format!(
				"add host --host example --user remote --host-key {HOST_KEY} --port {port}"
			))
			.unwrap_err();
			assert!(error.to_string().contains("between 1 and 65535"));
		}
	}
}
