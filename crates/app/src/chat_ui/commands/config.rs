//! Structural provider and configuration routes.

use super::command;

command!(settings, 300, "settings", [], "Open settings", [Owner], false, none => |host| host.settings());
command!(setup, 310, "setup", [], "Configure providers", [Credentials, Owner], false, none => |host| host.setup());
command!(providers, 320, "providers", [], "Show provider status", [Credentials], false, none => |host| host.providers());
command!(login, 330, "login", [], "Authenticate a provider", [Credentials, Owner], false, optional("[provider]") => |host, provider| host.login(provider));
command!(logout, 340, "logout", [], "Remove provider authorization", [Credentials, Owner], false, optional("[provider]") => |host, provider| host.logout(provider));
