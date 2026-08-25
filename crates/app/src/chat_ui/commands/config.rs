//! Structural provider and configuration routes.

use super::command;

command!(settings, 300, "settings", icon: Sliders, [], "Open settings", [Owner], false, none => |host| host.settings());
command!(setup, 310, "setup", icon: Gear, [], "Open provider setup", [Credentials, Owner], false, optional("[providers]") => |host, section| host.setup(section));
command!(providers, 320, "providers", icon: Gear, [], "Show live provider authentication status", [Credentials], false, none => |host| host.providers());
command!(login, 330, "login", icon: Input, [], "Authenticate a provider", [Credentials, Owner], false, optional("[provider]") => |host, provider| host.login(provider));
command!(logout, 340, "logout", icon: Output, [], "Logout from OAuth provider", [Credentials, Owner], false, optional("[provider]") => |host, provider| host.logout(provider));
