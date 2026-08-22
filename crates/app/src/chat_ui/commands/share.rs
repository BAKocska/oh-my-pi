use super::command;

command!(share, 760, "share", [], "Create an encrypted redacted share link", [Session, Owner], false, raw("[--no-redact] [--store auto|http|gist]", ["--no-redact", "--store", "auto", "http", "gist"]) => |host, args| host.share(args));
