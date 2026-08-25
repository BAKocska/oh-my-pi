use super::command;

command!(share, 760, "share", icon: Share, [], "Create an encrypted redacted share link", [Session, Owner], false, raw("[--no-redact] [--store auto|http|gist]", ["--no-redact", "--store", "auto", "http", "gist"]) => |host, args| host.share(args));
