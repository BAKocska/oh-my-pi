//! Structural durable-session and workspace routes.

use omp_core::Str;

use super::{BranchRequest, SessionRequest, WorkspaceRequest, command};

command!(help, 10, "help", ["hotkeys"], "Show commands and keyboard controls", [], true, none => |host| host.help());
command!(new_session, 20, "new", [], "Start a new session", [Session], false, none => |host| host.new_session());
command!(clear, 30, "clear", [], "Clear context inside this session", [Session], false, none => |host| host.clear());
command!(fresh, 40, "fresh", [], "Reset provider affinity for the next turn", [Session], false, none => |host| host.fresh());
command!(rename, 50, "rename", [], "Rename this session", [Session], false, required("<title>") => |host, title| host.rename(title));
command!(retry, 60, "retry", [], "Retry the previous user turn", [Session, Execution], false, none => |host| host.retry());
command!(resume, 70, "resume", [], "Resume a native session", [Session], false, selector("[session]") => |host, selector| host.resume(selector));
command!(pin, 79, "pin", [], "Pin or unpin a session at the top of the resume list", [Session, Owner], false, optional("[session id]") => |host, selector| host.session(SessionRequest::Pin(selector)));
command!(session, 80, "session", [], "Inspect or mutate this session", [Session, Owner], false, typed("info|delete|pin [session id]", ["info", "delete", "pin"], parse_session) => |host, request| host.session(request));
command!(jobs, 81, "jobs", [], "List active background jobs", [Execution], true, none => |host| host.jobs());
command!(agents, 82, "agents", [], "Open the live agent hierarchy", [Execution], false, none => |host| host.agents());
command!(pause, 83, "pause", [], "Pause the interactive session", [Execution], false, none => |host| host.pause());
command!(move_root, 90, "move", [], "Move the future primary workspace root", [Workspace, Owner], false, required("<directory>") => |host, root| host.workspace(WorkspaceRequest::Move(root)));
command!(add_dir, 100, "add-dir", [], "Add a workspace root", [Workspace, Owner], false, required("<directory>") => |host, root| host.workspace(WorkspaceRequest::Add(root)));
command!(remove_dir, 110, "remove-dir", [], "Remove a workspace root", [Workspace, Owner], false, required("<directory>") => |host, root| host.workspace(WorkspaceRequest::Remove(root)));
command!(dirs, 120, "dirs", [], "List workspace roots", [Workspace], true, none => |host| host.workspace(WorkspaceRequest::List));

command!(handoff, 121, "handoff", [], "Summarize the session into a handoff document and compact in place", [Session, Execution], false, optional("[instructions]") => |host, instructions| host.handoff(instructions));
command!(branch, 122, "branch", [], "Create a lineage child at a checkpoint", [Session, Execution], false, typed("[checkpoint]", [], parse_branch) => |host, request| host.branch(request));
command!(fork, 123, "fork", [], "Fork the live session projection", [Session, Execution], false, optional("[title]") => |host, title| host.fork(title));
command!(branch_tree, 124, "tree", [], "Show session branch lineage", [Session], false, none => |host| host.branch_tree());
command!(debug, 125, "debug", [], "Open the session inspector", [Session, Owner], false, optional("[inspector]") => |host, inspector| host.debug(inspector));
command!(quit, 900, "quit", ["exit", "q"], "Exit the client", [], true, none => |host| host.quit());
fn parse_branch(args: &str) -> miette::Result<BranchRequest> {
	let checkpoint = args.trim();
	if checkpoint.split_whitespace().count() > 1 {
		return Err(miette::miette!("usage: /branch [checkpoint]"));
	}
	Ok(BranchRequest { checkpoint: (!checkpoint.is_empty()).then(|| Str::new(checkpoint)) })
}

fn parse_session(args: &str) -> miette::Result<SessionRequest> {
	let mut words = args.split_whitespace();
	match (words.next(), words.next(), words.next()) {
		(Some("info"), None, None) => Ok(SessionRequest::Info),
		(Some("delete"), None, None) => Ok(SessionRequest::Delete),
		(Some("pin"), None, None) => Ok(SessionRequest::Pin(None)),
		(Some("pin"), Some(session), None) => Ok(SessionRequest::Pin(Some(Str::new(session)))),
		_ => Err(miette::miette!("usage: /session info|delete|pin [session id]")),
	}
}
