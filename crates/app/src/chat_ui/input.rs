use std::collections::HashSet;

use omp_agent::Budget;
use omp_core::Str;
use omp_proto::{
	inference::v1::TaskBudget,
	thread::v1::{Item, Message, Part, Role, item, part},
};
use omp_tui::Command;
use smallvec::SmallVec;

use super::now_ms;

/// One declarative subcommand offered by completion and help.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubcommandSpec {
	/// Subcommand spelling.
	pub name:        &'static str,
	/// One-line explanation.
	pub description: &'static str,
	/// Positional usage shown after the spelling.
	pub usage:       &'static str,
}

/// Metadata shared by slash-command parsing, completion, help, and dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandSpec {
	/// Command token without the leading slash.
	pub name:        &'static str,
	/// Alternate spellings, also without `/`.
	pub aliases:     &'static [&'static str],
	/// Human-readable completion and help text.
	pub description: &'static str,
	/// Optional argument hint appended by help and completion.
	pub usage:       &'static str,
	/// Declarative first-argument choices.
	pub subcommands: &'static [SubcommandSpec],
}

const TODO_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec {
		name:        "show",
		description: "Show the current task list",
		usage:       "",
	},
	SubcommandSpec {
		name:        "append",
		description: "Append a task",
		usage:       "[phase] <task>",
	},
	SubcommandSpec { name: "start", description: "Start a task", usage: "<task>" },
	SubcommandSpec {
		name:        "done",
		description: "Complete a task or phase",
		usage:       "[task|phase]",
	},
	SubcommandSpec {
		name:        "drop",
		description: "Abandon a task or phase",
		usage:       "[task|phase]",
	},
];
const COMPACT_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec {
		name:        "summary",
		description: "Summarize old context",
		usage:       "[focus]",
	},
	SubcommandSpec {
		name:        "prune",
		description: "Prune replaceable context",
		usage:       "[focus]",
	},
];
const PLAN_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec { name: "on", description: "Enter read-only planning", usage: "" },
	SubcommandSpec {
		name:        "yolo",
		description: "Plan until the first env-authorized mutation",
		usage:       "",
	},
	SubcommandSpec { name: "off", description: "Exit planning", usage: "" },
	SubcommandSpec { name: "status", description: "Show active mode", usage: "" },
];
const GOAL_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec {
		name:        "set",
		description: "Set an autonomous objective",
		usage:       "<objective> [token-budget]",
	},
	SubcommandSpec { name: "pause", description: "Pause goal continuation", usage: "" },
	SubcommandSpec {
		name:        "resume",
		description: "Resume goal continuation",
		usage:       "",
	},
	SubcommandSpec {
		name:        "complete",
		description: "Mark the objective complete",
		usage:       "",
	},
	SubcommandSpec { name: "drop", description: "Abandon the objective", usage: "" },
	SubcommandSpec {
		name:        "budget",
		description: "Replace the hard budget",
		usage:       "<tokens>",
	},
	SubcommandSpec {
		name:        "status",
		description: "Show goal status and spend",
		usage:       "",
	},
];
const VIBE_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec { name: "on", description: "Enter director/worker mode", usage: "" },
	SubcommandSpec { name: "off", description: "Exit director/worker mode", usage: "" },
	SubcommandSpec { name: "status", description: "Show active mode", usage: "" },
];
const PREWALK_SUBCOMMANDS: &[SubcommandSpec] = &[
	SubcommandSpec {
		name:        "on",
		description: "Reason cheaply until first mutation",
		usage:       "",
	},
	SubcommandSpec { name: "off", description: "Disarm prewalk", usage: "" },
	SubcommandSpec { name: "status", description: "Show active mode", usage: "" },
];

/// Canonical builtin slash-command vocabulary.
///
/// This table is deliberately the sole builtin name authority: autocomplete,
/// help, reserved-name filtering, and parsing all consume it.
pub const COMMANDS: &[CommandSpec] = &[
	CommandSpec {
		name:        "help",
		aliases:     &["hotkeys"],
		description: "Show commands and keyboard controls",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "login",
		aliases:     &[],
		description: "Authenticate a provider",
		usage:       "[provider]",
		subcommands: &[],
	},
	CommandSpec {
		name:        "model",
		aliases:     &["models"],
		description: "Change the selected model",
		usage:       "[model]",
		subcommands: &[],
	},
	CommandSpec {
		name:        "resume",
		aliases:     &[],
		description: "Open another project session",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "new",
		aliases:     &[],
		description: "Start a new session",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "clear",
		aliases:     &[],
		description: "Clear conversation context in place",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "compact",
		aliases:     &[],
		description: "Compact conversation context",
		usage:       "[summary|prune] [focus]",
		subcommands: COMPACT_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "todo",
		aliases:     &[],
		description: "Inspect or update session tasks",
		usage:       "[subcommand]",
		subcommands: TODO_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "plan",
		aliases:     &[],
		description: "Control read-only plan mode",
		usage:       "[on|yolo|off|status]",
		subcommands: PLAN_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "goal",
		aliases:     &[],
		description: "Control an autonomous objective",
		usage:       "[set|pause|resume|complete|drop|budget|status]",
		subcommands: GOAL_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "vibe",
		aliases:     &[],
		description: "Control director/worker vibe mode",
		usage:       "[on|off|status]",
		subcommands: VIBE_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "prewalk",
		aliases:     &[],
		description: "Control reason-first prewalk",
		usage:       "[on|off|status]",
		subcommands: PREWALK_SUBCOMMANDS,
	},
	CommandSpec {
		name:        "jobs",
		aliases:     &[],
		description: "List active background jobs",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "settings",
		aliases:     &[],
		description: "Open settings",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "agents",
		aliases:     &["tree"],
		description: "Open the live agent hierarchy",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "pause",
		aliases:     &[],
		description: "Pause the interactive session",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "live",
		aliases:     &[],
		description: "Toggle the firehose activity waveform",
		usage:       "",
		subcommands: &[],
	},
	CommandSpec {
		name:        "quit",
		aliases:     &["exit", "q"],
		description: "Exit the application",
		usage:       "",
		subcommands: &[],
	},
];

/// One command contributed by a live discovery provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandContribution {
	/// Primary spelling without `/`.
	pub name:        Str,
	/// Alternate spellings.
	pub aliases:     SmallVec<Str, 2>,
	/// One-line description.
	pub description: Str,
	/// Inline argument hint.
	pub hint:        Option<Str>,
	/// Human-readable discovery source label.
	pub origin:      Str,
	/// Optional prompt template dispatched when this command is submitted.
	pub template:    Option<Str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AvailableCommand {
	name:        Str,
	aliases:     SmallVec<Str, 2>,
	description: Str,
	hint:        Option<Str>,
	origin:      Str,
	template:    Option<Str>,
	builtin:     bool,
}

impl From<CommandContribution> for AvailableCommand {
	fn from(command: CommandContribution) -> Self {
		Self {
			name:        command.name,
			aliases:     command.aliases,
			description: command.description,
			hint:        command.hint,
			origin:      command.origin,
			template:    command.template,
			builtin:     false,
		}
	}
}

/// Live first-source-wins command roster shared by completion and dispatch.
pub struct CommandRoster {
	available: Vec<AvailableCommand>,
}

impl CommandRoster {
	/// Aggregates builtins followed by provider feeds in precedence order.
	#[must_use]
	pub fn new(sources: Vec<Vec<CommandContribution>>) -> Self {
		let mut ordered = Vec::with_capacity(sources.len().saturating_add(1));
		ordered.push(builtin_available());
		ordered.extend(sources.into_iter().map(|source| {
			source
				.into_iter()
				.map(AvailableCommand::from)
				.collect::<Vec<_>>()
		}));
		Self { available: aggregate_commands(ordered) }
	}

	/// Slash commands offered by the chat composer's completion palette.
	#[must_use]
	pub fn completions(&self) -> Vec<Command> {
		self.available.iter().map(to_completion).collect()
	}

	/// Parses builtin and provider-contributed slash commands.
	pub fn parse_input(&self, text: &str) -> Result<ChatCommand, InputError> {
		parse_input(text, &self.available)
	}

	/// Renders help from the same winning roster used by completion and
	/// dispatch.
	#[must_use]
	pub fn help_text(&self) -> String {
		render_help(&self.available)
	}
}

/// Aggregates command sources in caller order. The first spelling wins;
/// builtin names, aliases, and colon namespaces are always reserved.
fn aggregate_commands(
	sources: impl IntoIterator<Item = impl IntoIterator<Item = AvailableCommand>>,
) -> Vec<AvailableCommand> {
	let reserved = reserved_names();
	let mut claimed = HashSet::<Str>::new();
	let mut available = Vec::new();
	for source in sources {
		for mut command in source {
			let shadows = |candidate: &Str| {
				reserved.iter().any(|name| {
					candidate == name
						|| candidate
							.strip_prefix(name.as_str())
							.is_some_and(|rest| rest.starts_with(':'))
				})
			};
			let shadowed_builtin =
				!command.builtin && (shadows(&command.name) || command.aliases.iter().any(shadows));
			if shadowed_builtin || claimed.contains(&command.name) {
				continue;
			}
			command.aliases.retain(|alias| !claimed.contains(alias));
			claimed.insert(command.name.clone());
			for alias in &command.aliases {
				claimed.insert(alias.clone());
			}
			available.push(command);
		}
	}
	available
}

fn builtin_available() -> Vec<AvailableCommand> {
	COMMANDS
		.iter()
		.map(|spec| AvailableCommand {
			name:        Str::new_static(spec.name),
			aliases:     spec.aliases.iter().copied().map(Str::new_static).collect(),
			description: Str::new_static(spec.description),
			hint:        (!spec.usage.is_empty()).then(|| Str::new_static(spec.usage)),
			origin:      Str::new_static("builtin"),
			template:    None,
			builtin:     true,
		})
		.collect()
}

fn reserved_names() -> HashSet<Str> {
	COMMANDS
		.iter()
		.flat_map(|spec| std::iter::once(spec.name).chain(spec.aliases.iter().copied()))
		.map(Str::new)
		.collect()
}

fn to_completion(available: &AvailableCommand) -> Command {
	let aliases: SmallVec<&str, 2> = available.aliases.iter().map(Str::as_str).collect();
	let mut command =
		Command::new(available.name.as_str(), available.description.as_str(), &aliases);
	if let Some(spec) = COMMANDS
		.iter()
		.find(|spec| spec.name == available.name.as_str())
		&& !spec.subcommands.is_empty()
	{
		let args: Vec<_> = spec
			.subcommands
			.iter()
			.map(|sub| (sub.name, sub.description, sub.usage))
			.collect();
		command = command.with_args(&args);
	}
	if let Some(hint) = &available.hint {
		command = command.with_hint(hint);
	}
	command
}

fn render_help(available: &[AvailableCommand]) -> String {
	let mut help = String::from("**Commands**\n");
	for command in available {
		help.push_str("- `/");
		help.push_str(command.name.as_str());
		if let Some(hint) = &command.hint {
			help.push(' ');
			help.push_str(hint.as_str());
		}
		help.push_str("` — ");
		help.push_str(command.description.as_str());
		if !command.aliases.is_empty() {
			help.push_str(" (aliases: ");
			for (index, alias) in command.aliases.iter().enumerate() {
				if index != 0 {
					help.push_str(", ");
				}
				help.push('/');
				help.push_str(alias.as_str());
			}
			help.push(')');
		}
		if !command.builtin {
			help.push_str(" via ");
			help.push_str(command.origin.as_str());
		}
		help.push('\n');
	}
	help.push_str("\n**Keys**\nesc interrupt · esc esc rewind · enter steer · alt+enter follow-up");
	help
}

/// Actions parsed from user input in the chat shell.
#[derive(Debug, PartialEq)]
pub enum ChatCommand {
	/// Ignore an empty composer submission.
	Nothing,
	/// Show the command and key reference.
	Help,
	/// Start provider authentication.
	Login(Option<Str>),
	/// Update the session model.
	Model(Str),
	/// Open the catalog model picker.
	ModelPicker,
	/// Open the durable-session picker.
	Resume,
	/// Start a new durable session.
	NewSession,
	/// List active background jobs.
	Jobs,
	/// Open settings.
	Settings,
	/// Open the agent hierarchy.
	Agents,
	/// Pause the interactive host.
	Pause,
	/// Toggle the firehose-backed activity waveform.
	Live,
	/// Control plan mode with raw subcommand arguments.
	Plan(Str),
	/// Control goal mode with raw subcommand arguments.
	Goal(Str),
	/// Control vibe mode with raw subcommand arguments.
	Vibe(Str),
	/// Control prewalk mode with raw subcommand arguments.
	Prewalk(Str),
	/// A recognized command whose backend is not available yet.
	Unavailable { command: Str, reason: Str },
	/// Exit cleanly.
	Quit,
	/// A normal prompt, including unknown slash input which must pass through.
	Submit {
		/// Canonical user item with the budget prefix removed.
		item:   Box<Item>,
		/// Visible prompt text with the budget prefix removed.
		text:   Str,
		/// Optional per-turn advisory or hard token budget.
		budget: Option<ParsedTurnBudget>,
	},
}

/// Parsed `+Nk` or `+Nk!` turn-budget directive.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedTurnBudget {
	/// Agent-side output-token budget represented in the landed budget type.
	pub agent: Budget,
	/// Provider task-budget representation used by the turn parameters.
	pub task:  TaskBudget,
	/// `true` for the hard `!` form; advisory budgets remain visible but do not
	/// reject work.
	pub hard:  bool,
}

/// Parsed `/name args` token. The delimiter is the earliest whitespace or `:`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParsedSlash<'a> {
	/// Command name without `/`.
	pub name: &'a str,
	/// Trimmed raw arguments after the delimiter.
	pub args: &'a str,
}

/// Parses a syntactically command-shaped line. Paths containing another `/`
/// and ordinary prompt text return `None` for model passthrough.
#[must_use]
pub fn parse_slash(text: &str) -> Option<ParsedSlash<'_>> {
	let text = text.trim();
	let body = text.strip_prefix('/')?;
	let delimiter = body
		.char_indices()
		.find(|(_, ch)| ch.is_whitespace() || *ch == ':')
		.map_or(body.len(), |(at, _)| at);
	let name = &body[..delimiter];
	if name.is_empty() || name.contains('/') {
		return None;
	}
	let args = body[delimiter..].trim_start_matches(|ch: char| ch.is_whitespace() || ch == ':');
	Some(ParsedSlash { name, args })
}

/// Structured parsing failure for quote-aware command arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
	/// A quoted argument was not terminated.
	UnterminatedQuote,
	/// A `+Nk` budget prefix used zero, overflowed, or was malformed.
	InvalidBudget,
}

impl std::fmt::Display for InputError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::UnterminatedQuote => f.write_str("unterminated quoted command argument"),
			Self::InvalidBudget => f.write_str("turn budget must use +Nk or +Nk! with N > 0"),
		}
	}
}
impl std::error::Error for InputError {}

/// Parses composer text against the same aggregated roster used for completion.
/// Unknown slash names intentionally pass through as normal prompt text.
fn parse_input(text: &str, available: &[AvailableCommand]) -> Result<ChatCommand, InputError> {
	let (text, budget) = parse_budget_prefix(text)?;
	if text.trim().is_empty() {
		return if budget.is_some() {
			Err(InputError::InvalidBudget)
		} else {
			Ok(ChatCommand::Nothing)
		};
	}
	let submit = |text: &str| ChatCommand::Submit {
		item:   Box::new(user_message(text)),
		text:   Str::from(text),
		budget: budget.clone(),
	};
	let Some(parsed) = parse_slash(text) else {
		return Ok(submit(text));
	};
	let Some(available) = available.iter().find(|command| {
		command.name == parsed.name || command.aliases.iter().any(|alias| alias == parsed.name)
	}) else {
		return Ok(submit(text));
	};
	if !available.builtin {
		let Some(template) = &available.template else {
			return Ok(submit(text));
		};
		let args = tokenize_args(parsed.args)?;
		let expanded = expand_arguments(template, &args);
		return Ok(ChatCommand::Submit {
			item: Box::new(user_message(&expanded)),
			text: Str::from(expanded),
			budget,
		});
	}
	let spec = COMMANDS
		.iter()
		.find(|spec| spec.name == available.name.as_str())
		.expect("aggregated builtin must retain its declaration");
	let command = match spec.name {
		"help" => ChatCommand::Help,
		"login" => ChatCommand::Login((!parsed.args.is_empty()).then(|| Str::from(parsed.args))),
		"model" if parsed.args.is_empty() => ChatCommand::ModelPicker,
		"model" => ChatCommand::Model(Str::from(parsed.args)),
		"resume" => ChatCommand::Resume,
		"new" => ChatCommand::NewSession,
		"jobs" => ChatCommand::Jobs,
		"settings" => ChatCommand::Settings,
		"agents" => ChatCommand::Agents,
		"pause" => ChatCommand::Pause,
		"live" => ChatCommand::Live,
		"plan" => ChatCommand::Plan(Str::from(parsed.args)),
		"goal" => ChatCommand::Goal(Str::from(parsed.args)),
		"vibe" => ChatCommand::Vibe(Str::from(parsed.args)),
		"prewalk" => ChatCommand::Prewalk(Str::from(parsed.args)),
		"quit" => ChatCommand::Quit,
		"clear" => unavailable("clear", "the agent backend does not expose in-place context reset"),
		"compact" => unavailable("compact", "manual compaction is not exposed by the agent backend"),
		"todo" => unavailable("todo", "interactive todo storage is not attached to this session"),
		_ => unreachable!("every builtin has a dispatch arm"),
	};
	Ok(command)
}

/// Splits an optional leading `+Nk` / `+Nk!` directive from prompt text.
pub fn parse_budget_prefix(text: &str) -> Result<(&str, Option<ParsedTurnBudget>), InputError> {
	let trimmed = text.trim_start();
	let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
	let token = &trimmed[..end];
	if !token
		.as_bytes()
		.get(1)
		.is_some_and(|byte| byte.is_ascii_digit())
	{
		return Ok((text, None));
	}
	let (body, hard) = token
		.strip_suffix("k!")
		.map(|body| (body, true))
		.or_else(|| token.strip_suffix('k').map(|body| (body, false)))
		.ok_or(InputError::InvalidBudget)?;
	let thousands = body
		.strip_prefix('+')
		.filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
		.ok_or(InputError::InvalidBudget)?
		.parse::<u64>()
		.map_err(|_| InputError::InvalidBudget)?;
	let tokens = thousands
		.checked_mul(1_000)
		.filter(|tokens| *tokens > 0)
		.ok_or(InputError::InvalidBudget)?;
	let remainder = trimmed[end..].trim_start();
	Ok((
		remainder,
		Some(ParsedTurnBudget {
			agent: Budget { max_output_tokens: Some(tokens), ..Budget::default() },
			task: TaskBudget { total_tokens: tokens, remaining_tokens: hard.then_some(tokens) },
			hard,
		}),
	))
}

fn unavailable(command: &'static str, reason: &'static str) -> ChatCommand {
	ChatCommand::Unavailable { command: Str::new_static(command), reason: Str::new_static(reason) }
}

/// Splits arguments on unquoted whitespace. Single/double quotes group values;
/// backslash quotes the following scalar. Quote characters are not retained.
pub fn tokenize_args(raw: &str) -> Result<Vec<Str>, InputError> {
	let mut args = Vec::new();
	let mut current = String::new();
	let mut quote = None;
	let mut escaped = false;
	for ch in raw.chars() {
		if escaped {
			current.push(ch);
			escaped = false;
			continue;
		}
		if ch == '\\' {
			escaped = true;
			continue;
		}
		if let Some(open) = quote {
			if ch == open {
				quote = None;
			} else {
				current.push(ch);
			}
			continue;
		}
		if ch == '\'' || ch == '"' {
			quote = Some(ch);
		} else if ch.is_whitespace() {
			if !current.is_empty() {
				args.push(Str::from(std::mem::take(&mut current)));
			}
		} else {
			current.push(ch);
		}
	}
	if escaped {
		current.push('\\');
	}
	if quote.is_some() {
		return Err(InputError::UnterminatedQuote);
	}
	if !current.is_empty() {
		args.push(Str::from(current));
	}
	Ok(args)
}

/// Expands `$1`, `$2`, `$@`, and `$ARGUMENTS` once. Values are never scanned
/// again, preventing recursive substitution.
pub fn expand_arguments(template: &str, args: &[Str]) -> String {
	let joined = args.iter().map(Str::as_str).collect::<Vec<_>>().join(" ");
	let mut expanded = String::with_capacity(template.len().saturating_add(joined.len()));
	let bytes = template.as_bytes();
	let mut at = 0;
	while at < bytes.len() {
		if bytes[at] != b'$' {
			let ch = template[at..]
				.chars()
				.next()
				.expect("valid scalar boundary");
			expanded.push(ch);
			at += ch.len_utf8();
			continue;
		}
		if template[at..].starts_with("$ARGUMENTS") {
			expanded.push_str(&joined);
			at += "$ARGUMENTS".len();
			continue;
		}
		if template[at..].starts_with("$@") {
			expanded.push_str(&joined);
			at += 2;
			continue;
		}
		let mut end = at + 1;
		while end < bytes.len() && bytes[end].is_ascii_digit() {
			end += 1;
		}
		if end > at + 1 {
			let index = template[at + 1..end].parse::<usize>().unwrap_or(0);
			if index > 0
				&& let Some(value) = args.get(index - 1)
			{
				expanded.push_str(value);
			}
			at = end;
			continue;
		}
		expanded.push('$');
		at += 1;
	}
	expanded
}

/// Builds the canonical user-message item used by submissions and steering.
pub(super) fn user_message(text: impl Into<String>) -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(item::Kind::Message(Message {
			role:  i32::from(Role::User),
			parts: vec![Part { kind: Some(part::Kind::Text(text.into())) }],
		})),
		props:         None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn submit_text(command: ChatCommand) -> String {
		let ChatCommand::Submit { item, .. } = command else {
			panic!("expected passthrough submit")
		};
		let Some(item::Kind::Message(message)) = item.kind else {
			panic!("missing message")
		};
		let Some(part::Kind::Text(text)) = message.parts[0].kind.clone() else {
			panic!("missing text")
		};
		text
	}

	fn builtins() -> CommandRoster {
		CommandRoster::new(Vec::new())
	}

	fn contribution(
		name: &'static str,
		description: &'static str,
		origin: &'static str,
		template: &'static str,
	) -> CommandContribution {
		CommandContribution {
			name:        Str::new_static(name),
			aliases:     SmallVec::new(),
			description: Str::new_static(description),
			hint:        None,
			origin:      Str::new_static(origin),
			template:    Some(Str::new_static(template)),
		}
	}

	#[test]
	fn parses_whitespace_colon_aliases_and_passthrough() {
		let commands = builtins();
		assert_eq!(commands.parse_input("/live"), Ok(ChatCommand::Live));
		assert_eq!(parse_slash("/model: smol"), Some(ParsedSlash { name: "model", args: "smol" }));
		assert_eq!(commands.parse_input("/model:smol"), Ok(ChatCommand::Model(Str::from("smol"))));
		assert_eq!(commands.parse_input("/q"), Ok(ChatCommand::Quit));
		assert_eq!(submit_text(commands.parse_input("/unknown arg").unwrap()), "/unknown arg");
		assert_eq!(
			submit_text(commands.parse_input("/tmp/pic.png describe").unwrap()),
			"/tmp/pic.png describe"
		);
	}

	#[test]
	fn parses_execution_mode_commands_without_submitting_text() {
		let commands = builtins();
		assert_eq!(commands.parse_input("/plan yolo"), Ok(ChatCommand::Plan(Str::from("yolo"))));
		assert_eq!(
			commands.parse_input("/goal set finish migration 12000"),
			Ok(ChatCommand::Goal(Str::from("set finish migration 12000")))
		);
		assert_eq!(commands.parse_input("/vibe on"), Ok(ChatCommand::Vibe(Str::from("on"))));
		assert_eq!(
			commands.parse_input("/prewalk status"),
			Ok(ChatCommand::Prewalk(Str::from("status")))
		);
	}

	#[test]
	fn completion_help_and_reserved_names_share_one_live_roster() {
		let commands = CommandRoster::new(vec![
			vec![
				contribution("model", "shadow", "extension", "Shadow $1"),
				contribution("model:secret", "shadow namespace", "extension", "Shadow $1"),
				contribution("review", "first", "extension", "Review $1"),
			],
			vec![contribution("review", "second", "file", "Second $1")],
		]);
		let completed = commands.completions();
		let help = commands.help_text();
		for spec in COMMANDS {
			assert!(completed.iter().any(|command| command.name() == spec.name));
			assert!(help.contains(&format!("/{}", spec.name)));
		}
		assert_eq!(
			commands
				.available
				.iter()
				.filter(|entry| entry.name == "review")
				.count(),
			1
		);
		assert!(
			!commands
				.available
				.iter()
				.any(|entry| entry.description == "shadow")
		);
		assert!(
			!commands
				.available
				.iter()
				.any(|entry| entry.name == "model:secret")
		);
		assert_eq!(
			submit_text(commands.parse_input("/review 'two words'").unwrap()),
			"Review two words"
		);
	}

	#[test]
	fn turn_budget_prefixes_map_to_agent_and_provider_budgets() {
		let commands = builtins();
		let ChatCommand::Submit { item, text, budget: Some(advisory) } =
			commands.parse_input("+12k investigate").unwrap()
		else {
			panic!("expected budgeted submit")
		};
		assert_eq!(text, "investigate");
		assert_eq!(advisory.agent.max_output_tokens, Some(12_000));
		assert_eq!(advisory.task.total_tokens, 12_000);
		assert_eq!(advisory.task.remaining_tokens, None);
		assert!(!advisory.hard);
		let Some(item::Kind::Message(message)) = item.kind else {
			panic!("missing user message")
		};
		assert!(matches!(
			message.parts[0].kind.as_ref(),
			Some(part::Kind::Text(text)) if text == "investigate"
		));

		let (_, hard) = parse_budget_prefix("+2k! build").unwrap();
		let hard = hard.unwrap();
		assert_eq!(hard.task.remaining_tokens, Some(2_000));
		assert!(hard.hard);
		for invalid in ["+0k no", "+2x no", "+2k!"] {
			assert_eq!(commands.parse_input(invalid), Err(InputError::InvalidBudget));
		}
		assert_eq!(submit_text(commands.parse_input("+context please").unwrap()), "+context please");
	}

	#[test]
	fn tokenizer_and_substitution_are_quote_aware_and_non_recursive() {
		let args = tokenize_args("one 'two words' \"three words\" four\\ five $1").unwrap();
		assert_eq!(args, ["one", "two words", "three words", "four five", "$1"]);
		assert_eq!(
			expand_arguments("a=$1 all=$@ raw=$ARGUMENTS fifth=$5", &args),
			"a=one all=one two words three words four five $1 raw=one two words three words four \
			 five $1 fifth=$1"
		);
		assert_eq!(tokenize_args("'open"), Err(InputError::UnterminatedQuote));
	}

	#[test]
	fn unavailable_commands_have_named_errors() {
		assert_eq!(
			builtins().parse_input("/compact summary"),
			Ok(ChatCommand::Unavailable {
				command: Str::from("compact"),
				reason:  Str::from("manual compaction is not exposed by the agent backend"),
			})
		);
	}
}
