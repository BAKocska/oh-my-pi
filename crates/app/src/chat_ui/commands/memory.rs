use super::command;

command!(memory, 630, "memory", [], "Inspect or maintain Mnemopi memory", [Session, Owner], false, raw("view|stats|diagnose|clear|enqueue", ["view", "stats", "diagnose", "clear", "enqueue"]) => |host, args| host.memory(args));
