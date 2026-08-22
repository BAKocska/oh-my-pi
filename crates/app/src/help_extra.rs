//! Extended CLI help assembled from native environment and tool metadata.

use std::fmt::Write as _;

/// Native environment variables understood during bootstrap and launch.
pub const ENVIRONMENT_VARIABLES: &[(&str, &str)] = &[
	("OMP_PROFILE", "named profile selected before settings load"),
	("OMP_DATA_DIR", "application data and credential root"),
	("OMP_CONFIG_FILES", "platform-separated read-only TOML overlays"),
	("OMP_DEFAULT_MODEL", "primary model-role override"),
	("OMP_SMOL_MODEL", "fast/low-cost model-role override"),
	("OMP_SLOW_MODEL", "deep-reasoning model-role override"),
	("OMP_PLAN_MODEL", "planning model-role override"),
	("OMP_WORKTREE_DIR", "isolated worktree base directory"),
	("OMP_PY_SITE", "supervised CPython site-packages root"),
];

/// Built-in Environment tool names. Kept as one public metadata table so help,
/// validation, and completion callers consume one ordering.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
	"ask",
	"checkpoint",
	"computer",
	"dyn",
	"edit",
	"eval",
	"fetch",
	"glob",
	"goal",
	"grep",
	"hub",
	"image_gen",
	"read",
	"report_issue",
	"rewind",
	"shell",
	"think",
	"todo",
	"tts",
	"vibe",
	"write",
	"yield",
];

/// Renders the extended reference appended to clap's root help.
#[must_use]
pub fn render() -> String {
	let mut output = String::from("Environment variables:\n");
	for (name, description) in ENVIRONMENT_VARIABLES {
		let _ = writeln!(output, "  {name:<24} {description}");
	}
	output.push_str("\nBuilt-in tools:\n  ");
	output.push_str(&BUILTIN_TOOL_NAMES.join(", "));
	output
}
