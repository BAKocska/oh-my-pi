//! Minimal `/security review` declaration over the ordinary child-agent
//! runtime.

use std::path::{Component, Path};

use omp_core::Str;

use super::{SecurityRequest, command};

fn parse(args: &str) -> miette::Result<SecurityRequest> {
	let args = args.trim();
	let (verb, path) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
	if verb != "review" {
		return Err(miette::miette!("usage: /security review [relative-path]"));
	}
	let path = path.trim();
	if path.len() > 4_096
		|| (!path.is_empty()
			&& (Path::new(path).is_absolute()
				|| Path::new(path)
					.components()
					.any(|component| !matches!(component, Component::Normal(_)))))
	{
		return Err(miette::miette!("security review path must be a bounded relative path"));
	}
	Ok(SecurityRequest::Review((!path.is_empty()).then(|| Str::new(path))))
}

command!(security, 760, "security", icon: Shield, [], "Run a findings-first local security review", [
	Workspace, Execution, Owner
], false, typed("review [relative-path]", ["review"], parse) => |host, request| host.security(request));
