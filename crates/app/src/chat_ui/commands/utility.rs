//! Structural utility, capability-inspection, and device-mode routes.

use super::{ChangelogRequest, ComputerRequest, UtilityRequest, VisionRequest, command};

command!(changelog, 130, "changelog", [], "Show recent version history", [], true, typed("[recent|full]", ["recent", "full"], parse_changelog) => |host, request| host.utility(UtilityRequest::Changelog(request)));
command!(tools, 131, "tools", [], "List active and disabled tools", [Execution], false, none => |host| host.utility(UtilityRequest::Tools));
command!(extensions, 132, "extensions", [], "Open the extensions dashboard", [Workspace, Owner], false, none => |host| host.utility(UtilityRequest::Extensions));
command!(computer, 580, "computer", [], "Control desktop automation for this session", [Execution, Owner], false, typed("[on|off|auto|status|diagnose]", ["on", "off", "auto", "status", "diagnose"], parse_computer) => |host, request| host.utility(UtilityRequest::Computer(request)));
command!(vision, 590, "vision", [], "Control image-tool delegation", [Execution], false, typed("[on|off|auto|status]", ["on", "off", "auto", "status"], parse_vision) => |host, request| host.utility(UtilityRequest::Vision(request)));

fn parse_changelog(raw: &str) -> miette::Result<ChangelogRequest> {
	match raw.trim() {
		"" | "recent" => Ok(ChangelogRequest::Recent),
		"full" => Ok(ChangelogRequest::Full),
		_ => Err(miette::miette!("usage: /changelog [recent|full]")),
	}
}

fn parse_computer(raw: &str) -> miette::Result<ComputerRequest> {
	match raw.trim() {
		"" | "status" => Ok(ComputerRequest::Status),
		"on" => Ok(ComputerRequest::On),
		"off" => Ok(ComputerRequest::Off),
		"auto" => Ok(ComputerRequest::Auto),
		"diagnose" => Ok(ComputerRequest::Diagnose),
		_ => Err(miette::miette!("usage: /computer [on|off|auto|status|diagnose]")),
	}
}

fn parse_vision(raw: &str) -> miette::Result<VisionRequest> {
	match raw.trim() {
		"" | "status" => Ok(VisionRequest::Status),
		"on" => Ok(VisionRequest::On),
		"off" => Ok(VisionRequest::Off),
		"auto" => Ok(VisionRequest::Auto),
		_ => Err(miette::miette!("usage: /vision [on|off|auto|status]")),
	}
}
