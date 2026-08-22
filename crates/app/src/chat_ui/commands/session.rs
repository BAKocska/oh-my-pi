//! Structural durable-session and workspace routes.

use omp_core::Str;

use super::{SessionRequest, WorkspaceRequest, command};

command!(help, 10, "help", ["hotkeys"], "Show commands and keyboard controls", [], true, none => |host| host.help());
command!(new_session, 20, "new", [], "Start a new session", [Session], false, none => |host| host.new_session());
command!(clear, 30, "clear", [], "Clear context inside this session", [Session], false, none => |host| host.clear());
command!(fresh, 40, "fresh", [], "Reset provider affinity for the next turn", [Session], false, none => |host| host.fresh());
command!(rename, 50, "rename", [], "Rename this session", [Session], false, required("<title>") => |host, title| host.rename(title));
command!(retry, 60, "retry", [], "Retry the previous user turn", [Session, Execution], false, none => |host| host.retry());
command!(resume, 70, "resume", [], "Resume a native session", [Session], false, selector("[session]") => |host, selector| host.resume(selector));
command!(session, 80, "session", [], "Inspect or mutate this session", [Session, Owner], false, typed("info|delete|pin <account>", ["info", "delete", "pin"], parse_session) => |host, request| host.session(request));
command!(jobs, 81, "jobs", [], "List active background jobs", [Execution], true, none => |host| host.jobs());
command!(agents, 82, "agents", ["tree"], "Open the live agent hierarchy", [Execution], false, none => |host| host.agents());
command!(pause, 83, "pause", [], "Pause the interactive session", [Execution], false, none => |host| host.pause());
command!(move_root, 90, "move", [], "Move the future primary workspace root", [Workspace, Owner], false, required("<directory>") => |host, root| host.workspace(WorkspaceRequest::Move(root)));
command!(add_dir, 100, "add-dir", [], "Add a workspace root", [Workspace, Owner], false, required("<directory>") => |host, root| host.workspace(WorkspaceRequest::Add(root)));
command!(remove_dir, 110, "remove-dir", [], "Remove a workspace root", [Workspace, Owner], false, required("<directory>") => |host, root| host.workspace(WorkspaceRequest::Remove(root)));
command!(dirs, 120, "dirs", [], "List workspace roots", [Workspace], true, none => |host| host.workspace(WorkspaceRequest::List));
command!(quit, 900, "quit", ["exit", "q"], "Exit the client", [], true, none => |host| host.quit());

fn parse_session(args: &str) -> miette::Result<SessionRequest> {
	let mut words = args.split_whitespace();
	match (words.next(), words.next(), words.next()) {
		(Some("info"), None, None) => Ok(SessionRequest::Info),
		(Some("delete"), None, None) => Ok(SessionRequest::Delete),
		(Some("pin"), Some(account), None) => Ok(SessionRequest::Pin(Str::new(account))),
		_ => Err(miette::miette!("usage: /session info|delete|pin <account>")),
	}
}
