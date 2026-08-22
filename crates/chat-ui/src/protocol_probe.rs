//! Capability-aware terminal protocol smoke-test overlay.

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, OverlayAnchor, OverlayOptions, Size, TerminalCaps, Ui, UiContext, dom,
};

use crate::{OverlayPanel, panel_divider};

/// Host-side effect requested by the protocol probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtocolProbeEvent {
	/// Probe remains open.
	Consumed,
	/// Close the overlay.
	Close,
	/// Send one capability-aware desktop notification through `omp-tui`.
	Notify {
		/// Notification title.
		title: Str,
		/// Notification body.
		body:  Str,
	},
}

/// Visual and active protocol test using the renderer's typed primitives.
pub struct ProtocolProbe {
	ui:      Ui,
	options: OverlayOptions,
	caps:    TerminalCaps,
	ctx:     UiContext,
	width:   u16,
}

impl ProtocolProbe {
	/// Opens the probe using capabilities already negotiated by the live
	/// terminal.
	#[must_use]
	pub fn open(caps: TerminalCaps, ctx: &UiContext) -> Self {
		let mut probe = Self {
			ui: Ui::from_root(dom! { <text/> }, 1, ctx.clone()),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(72))
				.z(20),
			caps,
			ctx: ctx.clone(),
			width: 72,
		};
		probe.rebuild();
		probe
	}

	/// Routes close and active notification-test keys.
	pub fn handle_key(&mut self, key: Key) -> ProtocolProbeEvent {
		match key {
			Key::Esc => ProtocolProbeEvent::Close,
			Key::Char('n') | Key::Enter => ProtocolProbeEvent::Notify {
				title: Str::new_static("OMP terminal protocol probe"),
				body:  Str::new_static("Notification delivery succeeded."),
			},
			_ => ProtocolProbeEvent::Consumed,
		}
	}

	/// Returns a responsive composited layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(4).clamp(36, 84);
		if width != self.width {
			self.width = width;
			self.rebuild();
		}
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn rebuild(&mut self) {
		let sgr = "bold · italic · underline · strike";
		let graphics = sf!(
			"{:?}{}",
			self.caps.graphics,
			if self.caps.kitty_placeholders {
				" + placeholders"
			} else {
				""
			}
		);
		let link_status = supported(self.caps.hyperlinks);
		let sizing_status = supported(self.caps.text_sizing);
		let notify = sf!("{:?}", self.caps.notify);
		let sync = supported(self.caps.sync_output);
		self.ui = Ui::from_root(OverlayPanel::new("Terminal protocol probe").child(dom! {
			<col gap=1>
				<text bold>{"SGR styles"}</text>
				<row gap=1>
					<text bold>{"bold"}</text><text italic>{"italic"}</text>
					<text underline>{"underline"}</text><text strike>{"strike"}</text>
				</row>
				<row gap=0><text fg="#ff3b30">{"true"}</text><text fg="#ffcc00">{"color"}</text><text fg="#34c759">{" ramp"}</text></row>
				<row gap=1><text>{"OSC 8"}</text><text href="https://omp.dev">{"clickable test link"}</text><text dim>{link_status}</text></row>
				<row gap=1><text>{"OSC 66 sizing"}</text><text dim>{sizing_status}</text></row>
				<row gap=1><text>{"Graphics"}</text><text dim>{graphics}</text></row>
								<row gap=1>
					<img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=" w=4 h=2/>
					<text dim>{"inline image / cell fallback"}</text>
				</row>
<row gap=1><text>{"Notifications"}</text><text dim>{notify}</text></row>
				<row gap=1><text>{"Synchronized output"}</text><text dim>{sync}</text></row>
				{panel_divider()}
				<text dim truncate>{sf!("{sgr} · Enter/N notify · Esc close")}</text>
			</col>
		}), self.width, self.ctx.clone());
	}
}

const fn supported(value: bool) -> &'static str {
	if value {
		"supported"
	} else {
		"unsupported / fallback"
	}
}
