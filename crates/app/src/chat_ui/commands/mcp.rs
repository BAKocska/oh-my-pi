use omp_core::Str;

use super::{McpRequest, command};

command!(mcp, 620, "mcp", [], "Manage Environment MCP servers", [Workspace, Owner], false, typed("list|add|remove|enable|disable|test|reconnect|reauth|unauth|help", ["list", "add", "remove", "enable", "disable", "test", "reconnect", "reauth", "unauth", "help"], parse_mcp) => |host, request| host.mcp(request));

fn parse_mcp(raw: &str) -> miette::Result<McpRequest> {
	let raw = raw.trim();
	if raw.is_empty() || raw == "list" {
		return Ok(McpRequest::List);
	}
	if raw == "help" {
		return Ok(McpRequest::Help);
	}
	let (operation, tail) = raw.split_once(char::is_whitespace).unwrap_or((raw, ""));
	let tail = tail.trim();
	match operation {
		"add" => parse_add(tail),
		"remove" => named(McpRequest::Remove, tail),
		"enable" => named(McpRequest::Enable, tail),
		"disable" => named(McpRequest::Disable, tail),
		"test" => named(McpRequest::Test, tail),
		"reconnect" => named(McpRequest::Reconnect, tail),
		"reauth" => named(McpRequest::Reauth, tail),
		"unauth" => named(McpRequest::Unauth, tail),
		_ => Err(miette::miette!(
			"usage: /mcp list|add|remove|enable|disable|test|reconnect|reauth|unauth|help"
		)),
	}
}

fn parse_add(raw: &str) -> miette::Result<McpRequest> {
	let (scope, raw) = if let Some(raw) = raw.strip_prefix("--scope ") {
		let (scope, raw) = raw.split_once(char::is_whitespace).ok_or_else(|| {
			miette::miette!("usage: /mcp add [--scope user|project] <name> <server-json>")
		})?;
		let scope = match scope {
			"user" => super::ConfigScope::User,
			"project" => super::ConfigScope::Project,
			_ => return Err(miette::miette!("MCP scope must be `user` or `project`")),
		};
		(scope, raw.trim())
	} else {
		(super::ConfigScope::Project, raw)
	};
	let (name, server_json) = raw.split_once(char::is_whitespace).ok_or_else(|| {
		miette::miette!("usage: /mcp add [--scope user|project] <name> <server-json>")
	})?;
	serde_json::from_str::<serde_json::Value>(server_json)
		.map_err(|error| miette::miette!("invalid MCP server JSON: {error}"))?;
	Ok(McpRequest::Add { scope, name: Str::new(name), server_json: Str::new(server_json) })
}

fn named(build: fn(Str) -> McpRequest, raw: &str) -> miette::Result<McpRequest> {
	if raw.is_empty() || raw.split_whitespace().count() != 1 {
		Err(miette::miette!("MCP operation requires exactly one server name"))
	} else {
		Ok(build(Str::new(raw)))
	}
}
