//! Structural native SSH-host management routes.

use std::path::PathBuf;

use omp_core::Str;

use super::{ConfigScope, SshRequest, command};

command!(ssh, 665, "ssh", [], "Manage native SSH host declarations", [Workspace, Owner], false, typed("[list|add|remove|help]", ["list", "add", "remove", "help", "--scope", "--host", "--user", "--port", "--host-key", "--key"], parse_ssh) => |host, request| host.ssh(request));

fn parse_ssh(raw: &str) -> miette::Result<SshRequest> {
	let mut words = raw.split_whitespace();
	match words.next().unwrap_or("list") {
		"list" => parse_list(words),
		"add" => parse_add(words),
		"remove" => parse_remove(words),
		"help" if words.next().is_none() => Ok(SshRequest::Help),
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
		.map_err(|_| miette::miette!("SSH port must be between 1 and 65535"))?;
	if port == 0 {
		Err(miette::miette!("SSH port must be between 1 and 65535"))
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
