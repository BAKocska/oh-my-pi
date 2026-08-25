//! Structural advisor watchdog routes.

use omp_core::Str;

use super::{AdvisorRequest, command};

command!(advisor, 570, "advisor", icon: Advisor, [], "Control the advisor watchdog", [Execution, Session], false, typed("[toggle|on|off|status|dump|configure <settings>]", ["toggle", "on", "off", "status", "dump", "configure"], parse_advisor) => |host, request| host.advisor(request));

fn parse_advisor(raw: &str) -> miette::Result<AdvisorRequest> {
	let (operation, rest) = raw
		.trim()
		.split_once(char::is_whitespace)
		.unwrap_or((raw.trim(), ""));
	match operation {
		"" | "toggle" if rest.is_empty() => Ok(AdvisorRequest::Toggle),
		"on" if rest.is_empty() => Ok(AdvisorRequest::SetEnabled(true)),
		"off" if rest.is_empty() => Ok(AdvisorRequest::SetEnabled(false)),
		"status" if rest.is_empty() => Ok(AdvisorRequest::Status),
		"dump" if rest.is_empty() => Ok(AdvisorRequest::DumpRaw),
		"configure" if !rest.trim().is_empty() => {
			Ok(AdvisorRequest::Configure(Str::new(rest.trim())))
		},
		_ => Err(miette::miette!("usage: /advisor [toggle|on|off|status|dump|configure <settings>]")),
	}
}
