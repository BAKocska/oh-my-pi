use super::command;

command!(green, 670, "green", [], "Gather CI evidence and propose bounded remediation", [Workspace, Execution, Owner], false, optional("[branch|run]") => |host, target| host.green(target));
