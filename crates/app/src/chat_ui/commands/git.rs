//! Interactive Git workbench route.

use super::command;

command!(git, 126, "git", [], "Open the git UI (split diff viewer, staging, commit composer)", [Workspace], false, optional("[revision]") => |host, revision| host.git(revision));
