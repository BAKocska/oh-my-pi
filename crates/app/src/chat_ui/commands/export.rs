use super::{ExportRequest, command};

command!(export, 730, "export", [], "Export a styled HTML transcript", [Session, Owner], false, optional("[path]") => |host, path| host.export(ExportRequest::Html(path)));
command!(dump, 740, "dump", [], "Copy the transcript and optional request JSON", [Session], true, typed("[--requests]", ["--requests"], parse_dump) => |host, requests| host.export(ExportRequest::Dump { requests }));
command!(copy, 750, "copy", [], "Copy a selection, code block, or last command", [Session], true, raw("[selection|code [N]|command]", ["selection", "code", "command"]) => |host, request| host.export(ExportRequest::Copy(request)));

fn parse_dump(raw: &str) -> miette::Result<bool> {
	match raw.trim() {
		"" => Ok(false),
		"--requests" => Ok(true),
		_ => Err(miette::miette!("usage: /dump [--requests]")),
	}
}
