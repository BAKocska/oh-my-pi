//! Structural context-control and execution-flow routes.

use super::command;

command!(compact, 410, "compact", [], "Compact conversation context", [Context, Execution], false, typed("[soft|remote|snapcompact] [focus]", ["soft", "remote", "snapcompact"], parse_compact) => |host, request| host.compact(request));
command!(shake, 420, "shake", [], "Reclaim replaceable context", [Context, Execution], false, raw("[elide|drop-media]", ["elide", "drop-media"]) => |host, args| host.shake(args));
command!(usage, 430, "usage", [], "Show or reset durable usage", [Context], false, raw("[show|reset]", ["show", "reset"]) => |host, args| host.usage(args));
command!(stats, 440, "stats", [], "Open the local usage dashboard", [Context], false, flags("[--host HOST] [--port PORT]", ["--host", "--port"]) => |host, flags| host.stats(flags));
command!(plan, 450, "plan", [], "Control planning mode", [Execution], false, raw("[args]", []) => |host, args| host.plan(args));
command!(vibe, 451, "vibe", [], "Control director/worker mode", [Execution], false, raw("[on|off|status]", ["on", "off", "status"]) => |host, args| host.vibe(args));
command!(todo, 452, "todo", [], "Inspect or update session tasks", [Session], false, raw("[subcommand]", ["show", "edit", "copy", "export", "import",  "append", "start", "done", "drop", "rm", "help"]) => |host, args| host.todo(args));
command!(plan_review, 460, "plan-review", [], "Review the current plan", [Execution], false, raw("[args]", []) => |host, args| host.plan_review(args));
command!(goal, 470, "goal", ["guided-goal"], "Start or control a guided goal", [Execution], false, raw("[goal]", []) => |host, args| host.guided_goal(args));
command!(loop_command, 480, "loop", [], "Configure bounded continuation", [Execution], false, raw("[args]", []) => |host, args| host.loop_command(args));
command!(queue, 490, "queue", [], "Queue work at the next turn boundary", [Execution], false, required("<prompt>") => |host, prompt| host.queue(prompt));
command!(force, 500, "force", [], "Force the next turn's tool choice", [Execution], false, required("<tool>") => |host, tool| host.force(tool));
command!(fast, 510, "fast", [], "Control the fast service tier", [Model, Execution], false, raw("[on|off|status]", ["on", "off", "status"]) => |host, args| host.fast(args));
command!(prewalk, 520, "prewalk", [], "Control cheap-model prewalk", [Model, Execution], false, raw("[on|off|status]", ["on", "off", "status"]) => |host, args| host.prewalk(args));
command!(btw, 530, "btw", [], "Run an ephemeral aside", [Execution], false, required("<prompt>") => |host, prompt| host.btw(prompt));
command!(tan, 540, "tan", [], "Run a background aside", [Execution], false, required("<prompt>") => |host, prompt| host.tan(prompt));
command!(omfg, 550, "omfg", [], "Generate a durable TTSR rule", [Execution, Session], false, required("<instruction>") => |host, instruction| host.omfg(instruction));
command!(live, 560, "live", [], "Start or stop realtime voice", [Execution], false, raw("[start|stop|status]", ["start", "stop", "status"]) => |host, args| host.live(args));

fn parse_compact(args: &str) -> miette::Result<omp_agent::ManualCompactionRequest> {
	omp_agent::ManualCompactionRequest::parse(args).map_err(|error| miette::miette!("{error}"))
}
