//! Structural slash-command router and capability-scoped host contracts.

mod config;
pub mod context;
mod flow;
mod model;
pub mod registry;
pub mod result;
mod session;

use std::{future::Future, pin::Pin, sync::Arc};

use omp_agent::ManualCompactionRequest;
use omp_core::{Str, sf};
pub use registry::{
	AdvertisedCommand, ArgumentHint, CommandCapability, CommandDeclaration, CommandGeneration,
	CommandImplementation, CommandProvenance, CommandRole, CommandRoster, CommandSourceKind,
	CommandSurface, ShadowPolicy, ShadowRule,
};
pub use result::{CommandResult, ConsumedResult, DispatchResult, PromptResult};

/// Cold command future allocated only after an explicit user command.
pub type CommandFuture<'a> = Pin<Box<dyn Future<Output = miette::Result<CommandResult>> + 'a>>;

/// Erased structural handler generated beside its command declaration.
pub type CommandHandler =
	for<'a> fn(&'a mut dyn CommandHost, &'a str, &'a CommandProvenance) -> CommandFuture<'a>;

/// Parsed `/session` operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionRequest {
	/// Show durable session information.
	Info,
	/// Delete through the guarded session authority.
	Delete,
	/// Pin opaque provider account affinity.
	Pin(Str),
}

/// Parsed workspace-root operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceRequest {
	/// Replace the future primary root.
	Move(Str),
	/// Add a supplementary root.
	Add(Str),
	/// Remove a supplementary root.
	Remove(Str),
	/// List current roots.
	List,
}

/// Parsed command-line flags with optional values.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedFlags(pub Vec<(Str, Option<Str>)>);

/// Shell-scoped command capabilities.
pub trait ShellCommandHost {
	/// Render help from the live roster.
	fn help(&mut self) -> CommandFuture<'_>;
	/// Start a new durable session.
	fn new_session(&mut self) -> CommandFuture<'_>;
	/// List active background jobs.
	fn jobs(&mut self) -> CommandFuture<'_>;
	/// Open the live agent hierarchy.
	fn agents(&mut self) -> CommandFuture<'_>;
	/// Pause the interactive session.
	fn pause(&mut self) -> CommandFuture<'_>;
	/// Exit the initiating client.
	fn quit(&mut self) -> CommandFuture<'_>;
}

/// Session/workspace-scoped command capabilities.
pub trait SessionCommandHost {
	/// Append an in-journal context reset.
	fn clear(&mut self) -> CommandFuture<'_>;
	/// Append a provider-reset hint.
	fn fresh(&mut self) -> CommandFuture<'_>;
	/// Assign a durable title.
	fn rename(&mut self, title: Str) -> CommandFuture<'_>;
	/// Retry the latest durable user turn.
	fn retry(&mut self) -> CommandFuture<'_>;
	/// Resume a native selector, or open the picker.
	fn resume(&mut self, selector: Option<Str>) -> CommandFuture<'_>;
	/// Execute a structured session operation.
	fn session(&mut self, request: SessionRequest) -> CommandFuture<'_>;
	/// Execute a structured workspace operation.
	fn workspace(&mut self, request: WorkspaceRequest) -> CommandFuture<'_>;
}

/// Model-scoped command capabilities.
pub trait ModelCommandHost {
	/// Set or select the durable model preference.
	fn model(&mut self, selector: Option<Str>) -> CommandFuture<'_>;
	/// Set a resume-stable session override.
	fn switch(&mut self, selector: Str) -> CommandFuture<'_>;
}

/// Configuration/credential-scoped command capabilities.
pub trait ConfigCommandHost {
	/// Open settings.
	fn settings(&mut self) -> CommandFuture<'_>;
	/// Open provider setup.
	fn setup(&mut self) -> CommandFuture<'_>;
	/// Show configured providers.
	fn providers(&mut self) -> CommandFuture<'_>;
	/// Start a guarded provider login.
	fn login(&mut self, provider: Option<Str>) -> CommandFuture<'_>;
	/// Remove provider authorization.
	fn logout(&mut self, provider: Option<Str>) -> CommandFuture<'_>;
}

/// Context and execution-flow command capabilities.
pub trait FlowCommandHost {
	/// Return the latest complete anchored context snapshot.
	fn context(&mut self) -> CommandFuture<'_>;
	/// Run canonical manual compaction.
	fn compact(&mut self, request: ManualCompactionRequest) -> CommandFuture<'_>;
	/// Reclaim replaceable context.
	fn shake(&mut self, args: Str) -> CommandFuture<'_>;
	/// Query or reset durable usage.
	fn usage(&mut self, args: Str) -> CommandFuture<'_>;
	/// Start or inspect the stats service.
	fn stats(&mut self, flags: ParsedFlags) -> CommandFuture<'_>;
	/// Control planning mode.
	fn plan(&mut self, args: Str) -> CommandFuture<'_>;
	/// Control director/worker mode.
	fn vibe(&mut self, args: Str) -> CommandFuture<'_>;
	/// Inspect or mutate session tasks.
	fn todo(&mut self, args: Str) -> CommandFuture<'_>;
	/// Review the current plan.
	fn plan_review(&mut self, args: Str) -> CommandFuture<'_>;
	/// Start the guided-goal interview.
	fn guided_goal(&mut self, args: Str) -> CommandFuture<'_>;
	/// Configure bounded continuation.
	fn loop_command(&mut self, args: Str) -> CommandFuture<'_>;
	/// Queue work at the next boundary.
	fn queue(&mut self, prompt: Str) -> CommandFuture<'_>;
	/// Force a next-turn tool choice.
	fn force(&mut self, tool: Str) -> CommandFuture<'_>;
	/// Control the fast service tier.
	fn fast(&mut self, args: Str) -> CommandFuture<'_>;
	/// Control dynamic cheap-model prewalk.
	fn prewalk(&mut self, args: Str) -> CommandFuture<'_>;
	/// Run an ephemeral aside.
	fn btw(&mut self, prompt: Str) -> CommandFuture<'_>;
	/// Run a background aside.
	fn tan(&mut self, prompt: Str) -> CommandFuture<'_>;
	/// Generate a durable TTSR rule.
	fn omfg(&mut self, instruction: Str) -> CommandFuture<'_>;
	/// Start or stop realtime voice.
	fn live(&mut self, args: Str) -> CommandFuture<'_>;
}

/// Complete command host assembled from capability-scoped interfaces.
pub trait CommandHost:
	ShellCommandHost + SessionCommandHost + ModelCommandHost + ConfigCommandHost + FlowCommandHost
{
}

impl<T> CommandHost for T where
	T: ShellCommandHost
		+ SessionCommandHost
		+ ModelCommandHost
		+ ConfigCommandHost
		+ FlowCommandHost
{
}

pub(super) fn declaration(
	order: u16,
	name: &'static str,
	aliases: &'static [&'static str],
	description: &'static str,
	argument_hint: &'static str,
	candidates: &'static [&'static str],
	capabilities: &'static [CommandCapability],
	guest_visible: bool,
	handler: CommandHandler,
) -> CommandDeclaration {
	CommandDeclaration {
		order,
		name: sf!(name),
		aliases: aliases.iter().copied().map(Str::new).collect(),
		description: sf!(description),
		argument_hint: (!argument_hint.is_empty()).then(|| sf!(argument_hint)),
		hints: candidates
			.iter()
			.copied()
			.map(|value| ArgumentHint {
				value:       Str::new(value),
				description: Str::new_static(""),
			})
			.collect(),
		capabilities: Arc::from(capabilities),
		surfaces: Arc::from([CommandSurface::Tui, CommandSurface::Acp, CommandSurface::Text]),
		guest_visible,
		acp_description: None,
		provenance: CommandProvenance::builtin(),
		implementation: CommandImplementation::Handler(handler),
	}
}

pub(super) fn parse_none(args: &str, usage: &'static str) -> miette::Result<()> {
	if args.is_empty() {
		Ok(())
	} else {
		Err(miette::miette!("usage: {usage}"))
	}
}

pub(super) fn parse_required(args: &str, usage: &'static str) -> miette::Result<Str> {
	if args.is_empty() {
		Err(miette::miette!("usage: {usage}"))
	} else {
		Ok(Str::new(args))
	}
}

pub(super) fn parse_optional(args: &str) -> miette::Result<Option<Str>> {
	Ok((!args.is_empty()).then(|| Str::new(args)))
}

pub(super) fn parse_raw(args: &str) -> miette::Result<Str> {
	Ok(Str::new(args))
}

pub(super) fn parse_selector(args: &str) -> miette::Result<Option<Str>> {
	let selector = parse_optional(args)?;
	if selector
		.as_ref()
		.is_some_and(|selector| selector.starts_with('@'))
	{
		Err(miette::miette!("foreign session selectors are not supported"))
	} else {
		Ok(selector)
	}
}

pub(super) fn parse_flags(args: &str) -> miette::Result<ParsedFlags> {
	let mut parsed = Vec::new();
	let mut words = args.split_whitespace().peekable();
	while let Some(flag) = words.next() {
		if !flag.starts_with("--") {
			return Err(miette::miette!("expected a --flag, found `{flag}`"));
		}
		let value = if words.peek().is_some_and(|next| !next.starts_with("--")) {
			Some(Str::new(words.next().expect("peeked flag value remains available")))
		} else {
			None
		};
		parsed.push((Str::new(flag), value));
	}
	Ok(ParsedFlags(parsed))
}

macro_rules! command_common {
	($module:ident, $order:literal, $name:literal, [$($alias:literal),* $(,)?], $description:literal,
	 $hint:literal, [$($candidate:literal),* $(,)?], [$($capability:ident),* $(,)?], $guest:literal,
		$parse:expr, |$host:ident, $args:ident| $body:expr) => {
		mod $module {
			#[allow(unused_imports, reason = "commands reference file-scope parsers and types")]
			use super::{super::*, *};

			fn handle<'a>(
				$host: &'a mut dyn $crate::chat_ui::commands::CommandHost,
				raw: &'a str,
				_: &'a $crate::chat_ui::commands::CommandProvenance,
			) -> $crate::chat_ui::commands::CommandFuture<'a> {
				Box::pin(async move {
					let $args = ($parse)(raw)?;
					$body.await
				})
			}
			fn build() -> $crate::chat_ui::commands::CommandDeclaration {
				$crate::chat_ui::commands::declaration(
					$order, $name, &[$($alias),*], $description, $hint, &[$($candidate),*],
					&[$($crate::chat_ui::commands::CommandCapability::$capability),*],
					$guest,
					handle,
				)
			}
			inventory::submit! {
				$crate::chat_ui::commands::registry::BuiltinRegistration { declaration: build }
			}
		}
	};
}

macro_rules! command {
	($module:ident, $order:literal, $name:literal, [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, none => |$host:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, [$($alias),*], $description, "", [],
			[$($capability),*], $guest,
			|raw| $crate::chat_ui::commands::parse_none(raw, concat!("/", $name)),
			|$host, _args| $body);
	};
	($module:ident, $order:literal, $name:literal, [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, required($hint:literal) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, [$($alias),*], $description, $hint, [],
			[$($capability),*], $guest,
			|raw| $crate::chat_ui::commands::parse_required(
				raw, concat!("/", $name, " ", $hint)
			),
			|$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, optional($hint:literal) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, [$($alias),*], $description, $hint, [],
			[$($capability),*], $guest,
			$crate::chat_ui::commands::parse_optional, |$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, selector($hint:literal) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, [$($alias),*], $description, $hint, [],
			[$($capability),*], $guest, $crate::chat_ui::commands::parse_selector,
			|$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, raw($hint:literal, [$($candidate:literal),* $(,)?]) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, [$($alias),*], $description, $hint,
			[$($candidate),*], [$($capability),*], $guest,
			$crate::chat_ui::commands::parse_raw, |$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, flags($hint:literal, [$($candidate:literal),* $(,)?]) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, [$($alias),*], $description, $hint,
			[$($candidate),*], [$($capability),*], $guest,
			$crate::chat_ui::commands::parse_flags, |$host, $arg| $body);
	};
	($module:ident, $order:literal, $name:literal, [$($alias:literal),* $(,)?], $description:literal,
	 [$($capability:ident),* $(,)?], $guest:literal, typed($hint:literal, [$($candidate:literal),* $(,)?], $parse:path) => |$host:ident, $arg:ident| $body:expr) => {
		$crate::chat_ui::commands::command_common!($module, $order, $name, [$($alias),*], $description, $hint,
			[$($candidate),*], [$($capability),*], $guest, $parse, |$host, $arg| $body);
	};
}

pub(super) use command;
pub(crate) use command_common;
