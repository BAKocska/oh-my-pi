//! Native tool-renderer lifecycle gallery and PNG capture command.

use std::{fs, path::PathBuf};

use bytes::Bytes;
use clap::{Args, ValueEnum};
use miette::{IntoDiagnostic, miette};
use omp_chat_ui::Chat;
use omp_core::Str;
use omp_tool::render::{RenderRegistry, ViewState};
use omp_tools::{gallery::RendererGalleryFixture, register_builtin_renderers};
use omp_tui::{Size, UiContext};
use strum::{Display, EnumIter, IntoEnumIterator};

/// Tool lifecycle states rendered by the gallery, in display order.
#[derive(Clone, Copy, Debug, Display, EnumIter, Eq, PartialEq, ValueEnum)]
#[strum(serialize_all = "snake_case")]
pub enum GalleryState {
	/// Streaming arguments before the call is ready to execute.
	#[value(alias = "streaming-args")]
	Streaming,
	/// A live call with an optional typed progress update.
	#[value(alias = "in-progress")]
	Progress,
	/// A successfully settled call.
	#[value(alias = "done")]
	Success,
	/// A faulted call.
	#[value(alias = "failed")]
	Error,
}

/// Native renderer gallery options.
#[derive(Clone, Debug, Args)]
pub struct GalleryArgs {
	/// Restrict output to one registered renderer name.
	#[arg(short = 't', long)]
	pub tool:       Option<Str>,
	/// Restrict output to lifecycle states; may be repeated or comma-separated.
	#[arg(short = 's', long = "state", value_delimiter = ',')]
	pub states:     Vec<GalleryState>,
	/// Terminal width in columns, clamped to 40..=200.
	#[arg(short = 'w', long, default_value_t = 100)]
	pub width:      u16,
	/// Capture one native PNG per renderer and lifecycle state.
	#[arg(long)]
	pub screenshot: bool,
	/// PNG output directory.
	#[arg(short = 'o', long, default_value = "gallery")]
	pub out:        PathBuf,
}

/// Renders the native tool lifecycle gallery to stdout or PNG files.
pub fn run(args: GalleryArgs) -> miette::Result<()> {
	let gallery = omp_tools::gallery::builtin_renderer_gallery();
	let mut registry = RenderRegistry::new();
	register_builtin_renderers(&mut registry, gallery.identities).into_diagnostic()?;
	let fixtures = gallery
		.fixtures
		.iter()
		.filter(|fixture| {
			args
				.tool
				.as_ref()
				.is_none_or(|tool| tool.as_str() == fixture.identity.name.as_str())
		})
		.collect::<Vec<_>>();
	if fixtures.is_empty() {
		let requested = args.tool.as_ref().map_or("", Str::as_str);
		return Err(miette!("unknown gallery renderer '{requested}'"));
	}
	let states = if args.states.is_empty() {
		GalleryState::iter().collect::<Vec<_>>()
	} else {
		args.states
	};
	let width = args.width.clamp(40, 200);
	if args.screenshot {
		fs::create_dir_all(&args.out).into_diagnostic()?;
	}
	for fixture in fixtures {
		for state in &states {
			if args.screenshot {
				let context = UiContext::default();
				let mut chat = fixture_chat(&context, &registry, fixture, *state)?;
				let rendered = chat.render(Size::new(width, 18));
				let png = omp_tui::frame_png(rendered.frame).into_diagnostic()?;
				let state_name = state.to_string();
				let path = args
					.out
					.join(format!("{}-{state_name}.png", fixture.identity.name));
				fs::write(&path, png).into_diagnostic()?;
				println!("{}", path.display());
			} else {
				let state_name = state.to_string();
				println!("\n── {} · {state_name} {}", fixture.identity.name, "─".repeat(24));
				let text = render_fixture(&registry, fixture, *state, width)?;
				println!("{text}");
			}
		}
	}
	Ok(())
}

fn render_fixture(
	registry: &RenderRegistry,
	fixture: &RendererGalleryFixture,
	state: GalleryState,
	width: u16,
) -> miette::Result<String> {
	let context = UiContext::default();
	let mut chat = fixture_chat(&context, registry, fixture, state)?;
	let rendered = chat.render(Size::new(width, 18));
	Ok(omp_tui::frame_text(rendered.frame))
}

fn fixture_chat<'a>(
	context: &'a UiContext,
	registry: &RenderRegistry,
	fixture: &RendererGalleryFixture,
	state: GalleryState,
) -> miette::Result<Chat> {
	let mut fold = ViewState::new();
	if state != GalleryState::Streaming
		&& let Some(update) = fixture.progress_update
	{
		registry
			.fold(&fixture.identity, &mut fold, Bytes::from_static(update))
			.into_diagnostic()?;
	}
	let outcome = match state {
		GalleryState::Streaming | GalleryState::Progress => None,
		GalleryState::Success => Some(fixture.success_outcome),
		GalleryState::Error => Some(fixture.error_outcome),
	};
	let view = registry
		.view(&fixture.identity, &fold, outcome)
		.into_diagnostic()?;
	let mut chat = Chat::new(context);
	let revision = if fixture.identity.rev.family.is_empty() {
		fixture.identity.rev.n.to_string()
	} else {
		format!("{}.{}", fixture.identity.rev.family, fixture.identity.rev.n)
	};
	chat.tool_started("gallery", fixture.identity.name.as_str(), &revision, fixture.title);
	match state {
		GalleryState::Streaming | GalleryState::Progress => chat.tool_view("gallery", view),
		GalleryState::Success => chat.tool_finished("gallery", true, view),
		GalleryState::Error => chat.tool_finished("gallery", false, view),
	}
	Ok(chat)
}
