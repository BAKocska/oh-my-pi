use super::command;

command!(review, 680, "review", [], "Review a base diff, commit, PR, or uncommitted work", [Workspace, Execution, Owner], false, optional("[base|commit|pr://N|PR-URL]") => |host, target| host.review(target));
