use super::command;

command!(memory, 630, "memory", icon: Memory, [], "Inspect or maintain Mnemopi memory", [Session, Owner], false, raw("view|stats|diagnose|clear|reset|enqueue", ["view", "stats", "diagnose", "clear", "reset", "enqueue"]) => |host, args| host.memory(args));
